//! The Strong layer itself (cargo feature `dfn3`): see the parent module.

use super::{StepDownReason, StrongState};
// The thresholds below are explained in the module docs of src/strong.rs.
use std::sync::Arc;

use crate::FRAME;
use crate::dfn3::{Dfn3Model, Dfn3Stream, tail_weight};

/// One block at or over this steps down at once (ns of the thread's CPU time).
pub(crate) const SLOW_BLOCK: u64 = 6_000_000;
/// The rolling window, in the stage's blocks (1 s): the length of the audio pipeline's own
/// window over the stage's calls.
pub(crate) const WINDOW: usize = 100;
/// A block over this counts towards [`WINDOW_SLOW_COUNT`] (ns).
pub(crate) const WINDOW_SLOW: u64 = 4_000_000;
/// This many slow blocks in the window step down.
pub(crate) const WINDOW_SLOW_COUNT: u32 = 2;
/// A block over this counts towards [`WINDOW_HEAVY_COUNT`] (ns).
pub(crate) const WINDOW_HEAVY: u64 = 3_000_000;
/// This many heavy blocks in the window step down.
pub(crate) const WINDOW_HEAVY_COUNT: u32 = 50;
/// A retry's warm-up, in blocks.
pub(crate) const PROBE_BLOCKS: u32 = 50;
/// The first retry waits this many blocks (30 s); each later one twice as long.
pub(crate) const RETRY_BLOCKS: u64 = 3_000;
/// The counted step-down that is final.
pub(crate) const MAX_STEP_DOWNS: u32 = 3;
/// In a grace window a block over this counts towards [`GRACE_SLOW_COUNT`] (ns): the line of
/// the audio pipeline's own percentile rule.
pub(crate) const GRACE_SLOW: u64 = 6_000_000;
/// This many blocks over [`GRACE_SLOW`] among the last [`WINDOW`] blocks step down in a
/// grace window: the pipeline's percentile rule strikes at the same count.
pub(crate) const GRACE_SLOW_COUNT: u32 = 2;
/// A step-down in a grace window, or within this many blocks (5 s) after one closed, is
/// retried after [`SHORT_RETRY_BLOCKS`].
pub(crate) const GRACE_TAIL_BLOCKS: u64 = 500;
/// The retry of a step-down tied to a grace window waits this many blocks (5 s).
pub(crate) const SHORT_RETRY_BLOCKS: u64 = 500;
/// The audio pipeline's own percentile line for one call of the stage, on the clock the rules
/// judge (ns): a retry does not start while a block of the last [`WINDOW`] went over it.
pub(crate) const PIPELINE_LINE: u64 = 6_000_000;
/// The audio pipeline's starvation line, by the wall clock (ns): it counts a call this long as
/// slow whatever its CPU time, so a retry does not start while a block of the last [`WINDOW`]
/// reached it either.
pub(crate) const PIPELINE_STARVED_WALL: u64 = 20_000_000;
/// A model block over this by the wall clock counts towards [`STARVED_BLOCKS`] (ns).
///
/// The starvation cutoff is the one rule on wall time: the thread is not getting the time it
/// needs, whatever the cause, and running the model makes that worse. 10 ms is one block's
/// own period: a block over it means the audio thread falls behind real time, which a
/// pre-emption that only delays a block (as a camera starting does) does not.
pub(crate) const STARVED_WALL: u64 = 10_000_000;
/// This many model blocks in a row over [`STARVED_WALL`] step down, in a grace window too.
/// Three in a row is a run of falling behind, not one late block; fewer would trip on the
/// single long pre-emptions this cutoff is meant to let through.
pub(crate) const STARVED_BLOCKS: u32 = 3;
/// A step-down within this many blocks (1 s) after a reset of the stage counts as one in a
/// grace window: the audio pipeline restarts its own window at a reset, and its first second
/// of blocks is judged on very few of them.
pub(crate) const RESET_TIE_BLOCKS: u64 = WINDOW as u64;
/// Step-downs tied to a grace window or a reset that are final: such a step-down does not
/// count towards [`MAX_STEP_DOWNS`] (it says the device was busy, not that the model is too
/// slow for it), so this separate bound keeps a device that is busy all the time from trying
/// the model again for the whole call. Each is retried after [`SHORT_RETRY_BLOCKS`], so this
/// many give the model a few dozen seconds of tries before it is set aside.
pub(crate) const MAX_GRACE_STEP_DOWNS: u32 = 8;
/// Blocks of test signal a new layer's stream runs before it is cleared and used.
const WARM_UP_BLOCKS: u32 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    Idle,
    /// The model runs, its output unused; `left` more blocks, then the fade in.
    Warm {
        left: u32,
    },
    /// The model runs and its output fades in over this block.
    FadeIn,
    /// The model's output goes on.
    Active,
    /// The model's tail fades out over this block (no model run), then Idle.
    Leaving,
}

/// What a block did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Outcome {
    /// The model ran this block.
    pub(crate) ran: bool,
    /// Some of the block's output is the model's.
    pub(crate) active: bool,
}

/// The Strong layer of a stage.
pub(crate) struct Strong {
    stream: Dfn3Stream,
    out: [f32; FRAME],
    run: Run,
    rules: Rules,
    /// Every step-down so far.
    pub(crate) step_downs: u32,
    /// Step-downs that count towards [`MAX_STEP_DOWNS`] (those not tied to a grace window or
    /// a reset); the retry schedule doubles with these.
    counted: u32,
    /// Step-downs tied to a grace window or a reset ([`MAX_GRACE_STEP_DOWNS`]).
    pub(crate) grace_step_downs: u32,
    pub(crate) last: Option<StepDownReason>,
    pub(crate) last_block: u64,
    retry_at: u64,
    retired: bool,
    /// A retry is due but held back: a recent block went over the pipeline's own line.
    pub(crate) held: bool,
    /// The rules applied at the latest block (off for measurement: then no retry is held).
    rules_on: bool,
    /// The block count at the latest reset ([`Self::reset`]).
    last_reset: Option<u64>,
    /// A retry's warm-up is being judged: its blocks' times so far.
    probing: bool,
    probe_times: [u64; PROBE_BLOCKS as usize],
    probe_n: usize,
    /// Warm-up blocks still to run before the layer is used ([`Self::warm_up_step`]).
    pub(crate) warm_left: u32,
    /// Warm-up blocks run so far (the test signal's position).
    warm_done: u32,
    warm_seed: u32,
    /// A warm-up block has begun in the stream.
    warm_in_block: bool,
    /// What the block begun by [`Self::begin`] does at [`Self::end`].
    plan: Plan,
    /// Tests: the model's next output is not finite (a fault).
    #[cfg(test)]
    poison: bool,
}

/// What a begun block does when it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// Nothing of the model's (Standard's output stays).
    Idle,
    /// The model's tail fades out over the block (no model run).
    Leaving,
    /// The model runs; its output is used (`Run::FadeIn` or `Run::Active` at begin) or not
    /// (`Run::Warm`).
    Model,
}

impl std::fmt::Debug for Strong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Strong")
            .field("run", &self.run)
            .field("step_downs", &self.step_downs)
            .finish_non_exhaustive()
    }
}

impl Strong {
    /// The layer over `model`, warmed up: every code path of the model has run on this
    /// stream (on a few blocks of a test signal), then the stream is cleared.
    pub(crate) fn new(model: Arc<Dfn3Model>) -> Self {
        let mut s = Self::cold(model);
        s.finish_warm_up();
        s
    }

    /// The layer over `model`, not yet warmed up: [`Self::warm_up_step`] until it answers
    /// true (or [`Self::finish_warm_up`]) before its first block.
    pub(crate) fn cold(model: Arc<Dfn3Model>) -> Self {
        Self {
            stream: Dfn3Stream::new(model),
            out: [0.0; FRAME],
            run: Run::Idle,
            rules: Rules::new(),
            step_downs: 0,
            counted: 0,
            grace_step_downs: 0,
            last: None,
            last_block: 0,
            retry_at: 0,
            retired: false,
            held: false,
            rules_on: true,
            last_reset: None,
            probing: false,
            probe_times: [0; PROBE_BLOCKS as usize],
            probe_n: 0,
            warm_left: WARM_UP_BLOCKS,
            warm_done: 0,
            warm_seed: 0x2545_f491,
            warm_in_block: false,
            plan: Plan::Idle,
            #[cfg(test)]
            poison: false,
        }
    }

    /// One step of the warm-up (a block's start, or one part of the model's work, or a block's
    /// end): bounded work. True once the warm-up is complete (the stream then cleared).
    pub(crate) fn warm_up_step(&mut self) -> bool {
        if self.warm_left == 0 {
            return true;
        }
        if !self.warm_in_block {
            let b = self.warm_done as usize;
            let mut input = [0.0f32; FRAME];
            // Noise, louder then quieter, so the model's SNR estimate crosses its thresholds
            // and every decoder runs at least once.
            let level = if b % 8 < 4 { 0.3 } else { 0.003 };
            let mut seed = self.warm_seed;
            for (i, s) in input.iter_mut().enumerate() {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let noise = (seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
                let tone = ((b * FRAME + i) as f32 * 0.0523).sin();
                *s = level * (0.5 * noise + 0.5 * tone);
            }
            self.warm_seed = seed;
            self.stream.begin(&input);
            self.warm_in_block = true;
            return false;
        }
        if self.stream.steps_left() > 0 {
            self.stream.step();
            return false;
        }
        self.stream.end(&mut self.out);
        self.warm_in_block = false;
        self.warm_left -= 1;
        self.warm_done = self.warm_done.wrapping_add(1);
        if self.warm_left > 0 {
            return false;
        }
        self.stream.clear();
        self.out = [0.0; FRAME];
        true
    }

    /// Runs what is left of the warm-up.
    pub(crate) fn finish_warm_up(&mut self) {
        while !self.warm_up_step() {}
    }

    /// Forget held audio (a reset, at the stage's block count `block_no`): the model starts
    /// again from a clear state, with a one-block warm-up, unless it is stepped down. The
    /// step-down history stays. The rules' window starts afresh, as the audio pipeline's own
    /// does at a reset.
    pub(crate) fn reset(&mut self, block_no: u64) {
        self.plan = Plan::Idle;
        self.stream.clear();
        self.rules.clear();
        self.run = Run::Idle;
        self.probing = false;
        self.last_reset = Some(block_no);
    }

    pub(crate) fn state(&self, block: u64) -> StrongState {
        if self.retired {
            return StrongState::Retired;
        }
        match self.run {
            Run::Warm { .. } => StrongState::Warming,
            Run::FadeIn | Run::Active | Run::Leaving => StrongState::Running,
            Run::Idle if self.step_downs > 0 && (block < self.retry_at || self.held) => {
                StrongState::SteppedDown
            }
            Run::Idle => StrongState::Idle,
        }
    }

    /// The block at which the next retry may start.
    pub(crate) fn retry_at(&self) -> u64 {
        self.retry_at
    }

    /// Starts a block: decides what it does and, if the model runs, starts its hop. Cheap.
    /// `input` is the block that came in (sanitised); `want`: Strong is asked for; `block_no`:
    /// the stage's block count. Then [`Self::step`] (any number of times) and [`Self::end`]:
    /// together one block, the same however the steps are spread.
    pub(crate) fn begin(&mut self, input: &[f32; FRAME], want: bool, block_no: u64) {
        self.plan = Plan::Idle;
        if self.run == Run::Idle {
            if !want || self.retired || block_no < self.retry_at {
                self.held = false;
                return;
            }
            let retry = self.step_downs > 0;
            // The audio pipeline still holds a block over its line among its recent ones: one
            // more during the warm-up would make it bypass the whole stage. Wait until it has
            // aged out of the window.
            self.held = retry && self.rules_on && self.rules.over_pipeline_line();
            if self.held {
                return;
            }
            self.stream.clear();
            // The rules' window is kept: it covers the stage's recent blocks, as the
            // pipeline's own window does across a retry.
            self.probing = retry;
            self.probe_n = 0;
            let left = if retry { PROBE_BLOCKS } else { 1 };
            self.run = Run::Warm { left };
        }
        if !want && matches!(self.run, Run::Active) {
            self.run = Run::Leaving;
        }
        match self.run {
            Run::Idle => {}
            Run::Leaving => self.plan = Plan::Leaving,
            Run::Warm { .. } | Run::FadeIn | Run::Active => {
                if !want {
                    // Warm or FadeIn without Strong: Standard's output never left.
                    self.run = Run::Idle;
                    return;
                }
                self.stream.begin(input);
                self.plan = Plan::Model;
            }
        }
    }

    /// Parts of the model's work left in the begun block.
    pub(crate) fn steps_left(&self) -> u32 {
        if self.plan == Plan::Model {
            self.stream.steps_left()
        } else {
            0
        }
    }

    /// Runs one part of the model's work in the begun block; true once none is left.
    pub(crate) fn step(&mut self) -> bool {
        if self.plan == Plan::Model {
            self.stream.step()
        } else {
            true
        }
    }

    /// What the model's last hop ran: the ERB decoder, and the deep-filter decoder.
    #[cfg(feature = "probe")]
    pub(crate) fn decoders_ran(&self) -> (bool, bool) {
        self.stream.decoders_ran()
    }

    /// Ends the begun block: the rest of the model's work, then this layer's output into
    /// `block` (which holds Standard's output for the block going out, or the dry signal).
    pub(crate) fn end(&mut self, block: &mut [f32; FRAME], block_no: u64) -> Outcome {
        let idle = Outcome {
            ran: false,
            active: false,
        };
        let plan = std::mem::replace(&mut self.plan, Plan::Idle);
        match plan {
            Plan::Idle => idle,
            Plan::Leaving => {
                self.fade_out(block);
                self.run = Run::Idle;
                Outcome {
                    ran: false,
                    active: true,
                }
            }
            Plan::Model => {
                self.stream.end(&mut self.out);
                #[cfg(test)]
                if self.poison {
                    self.out[0] = f32::NAN;
                }
                if !self.out.iter().all(|v| v.is_finite()) {
                    self.fault(block_no);
                    return Outcome {
                        ran: true,
                        active: false,
                    };
                }
                match self.run {
                    Run::Warm { left } => {
                        self.run = if left <= 1 {
                            Run::FadeIn
                        } else {
                            Run::Warm { left: left - 1 }
                        };
                        Outcome {
                            ran: true,
                            active: false,
                        }
                    }
                    Run::FadeIn => {
                        for (i, (b, &d)) in block.iter_mut().zip(&self.out).enumerate() {
                            let t = (i + 1) as f32 / FRAME as f32;
                            *b = (*b * (1.0 - t) + d * t).clamp(-1.0, 1.0);
                        }
                        self.run = Run::Active;
                        Outcome {
                            ran: true,
                            active: true,
                        }
                    }
                    _ => {
                        for (b, &d) in block.iter_mut().zip(&self.out) {
                            *b = d.clamp(-1.0, 1.0);
                        }
                        Outcome {
                            ran: true,
                            active: true,
                        }
                    }
                }
            }
        }
    }

    /// The block after the model's last: its overlap-add tail (already computed, for these
    /// very samples) plus Standard's output weighted by the window's complement.
    fn fade_out(&mut self, block: &mut [f32; FRAME]) {
        if self.stream.skipping() {
            // The model was muting silence: its tail is stale, and Standard's output is the
            // same near-silence.
            return;
        }
        let window = self.stream.window();
        let tail = self.stream.tail();
        for (i, b) in block.iter_mut().enumerate() {
            let tw = tail_weight(window, i);
            *b = (tail[i] + *b * (1.0 - tw)).clamp(-1.0, 1.0);
        }
    }

    fn fault(&mut self, block_no: u64) {
        self.step_down(StepDownReason::Fault, block_no, 0);
        // The tail may be just as bad: no fade, Standard's output at once.
        self.run = Run::Idle;
        self.retired = true;
    }

    /// After every block of the stage, with its times: `judged` is what the rules judge (the
    /// thread's CPU time where there is a clock for it), `wall` the wall time (the
    /// starvation cutoff's, and what the audio pipeline sees). `ran`: the model ran this
    /// block. Steps down if a rule says so (`judge`: the rules apply; off for measurement).
    /// `force`: step down now. `grace_until`: the stage's grace window runs until this block
    /// count (0: none was opened); inside it only the grace rule and the starvation cutoff
    /// apply.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn judge(
        &mut self,
        judged: u64,
        wall: u64,
        ran: bool,
        block_no: u64,
        judge: bool,
        force: bool,
        grace_until: u64,
    ) {
        self.rules_on = judge;
        let grace = block_no < grace_until;
        let model = ran && matches!(self.run, Run::Warm { .. } | Run::FadeIn | Run::Active);
        // Every block goes into the window, the model's or not, so it ages as the pipeline's
        // own window over the stage's calls does.
        let verdict = self.rules.push(judged, wall, model, grace);
        if !model {
            return;
        }
        if force {
            self.step_down(StepDownReason::Forced, block_no, grace_until);
            return;
        }
        if !judge {
            return;
        }
        if self.probing {
            // A retry's warm-up is judged as a whole at its end, on its 99th percentile (and
            // its median outside a grace window); only the starvation cutoff acts at once.
            if verdict == Some(StepDownReason::Starved) {
                self.step_down(StepDownReason::Starved, block_no, grace_until);
                return;
            }
            if let Some(t) = self.probe_times.get_mut(self.probe_n) {
                *t = judged;
                self.probe_n += 1;
            }
            if matches!(self.run, Run::Warm { .. }) {
                return;
            }
            self.probing = false;
            if let Some(r) = probe_verdict(&self.probe_times[..self.probe_n], grace) {
                self.step_down(r, block_no, grace_until);
            }
            return;
        }
        if let Some(r) = verdict {
            self.step_down(r, block_no, grace_until);
        }
    }

    fn step_down(&mut self, reason: StepDownReason, block_no: u64, grace_until: u64) {
        // `run` is the state for the next block: Active means the model's output went out
        // this block, so its tail fades out next block; Warm and FadeIn mean none of it has.
        self.run = match self.run {
            Run::Active | Run::Leaving => Run::Leaving,
            Run::Warm { .. } | Run::FadeIn | Run::Idle => Run::Idle,
        };
        self.step_downs += 1;
        self.last = Some(reason);
        self.last_block = block_no;
        self.probing = false;
        // The window is kept: its slow blocks age out within a second, before any retry.
        let timing = matches!(
            reason,
            StepDownReason::SlowBlock
                | StepDownReason::SlowWindow
                | StepDownReason::Heavy
                | StepDownReason::Starved
        );
        let after_reset = self
            .last_reset
            .is_some_and(|r| block_no < r.saturating_add(RESET_TIE_BLOCKS));
        if timing && (block_no < grace_until || after_reset) {
            // The device was busy: not counted towards retiring the model, but bounded.
            self.grace_step_downs += 1;
            if self.grace_step_downs >= MAX_GRACE_STEP_DOWNS {
                self.retired = true;
            } else {
                self.retry_at = block_no + SHORT_RETRY_BLOCKS;
            }
            return;
        }
        self.counted += 1;
        if self.counted >= MAX_STEP_DOWNS {
            self.retired = true;
        } else {
            self.retry_at = block_no + retry_wait(self.counted, block_no, grace_until);
        }
    }
}

/// A retry's warm-up, judged on its blocks' `times` (ns): its 99th percentile over
/// [`WINDOW_SLOW`] ([`GRACE_SLOW`] in a grace window) steps down as
/// [`StepDownReason::SlowWindow`], and outside a grace window its median over
/// [`WINDOW_HEAVY`] as [`StepDownReason::Heavy`]. The percentile is the sorted times' value
/// at `(n - 1) * 99 / 100`: of 50 blocks, the second slowest, so one slow block alone passes.
fn probe_verdict(times: &[u64], grace: bool) -> Option<StepDownReason> {
    let n = times.len();
    if n == 0 {
        return None;
    }
    let mut sorted = [0u64; PROBE_BLOCKS as usize];
    let sorted = &mut sorted[..n];
    sorted.copy_from_slice(times);
    sorted.sort_unstable();
    let p99 = sorted[(n - 1) * 99 / 100];
    let p50 = sorted[(n - 1) / 2];
    let line = if grace { GRACE_SLOW } else { WINDOW_SLOW };
    if p99 > line {
        Some(StepDownReason::SlowWindow)
    } else if !grace && p50 > WINDOW_HEAVY {
        Some(StepDownReason::Heavy)
    } else {
        None
    }
}

/// How long the `step_downs`-th step-down (1 for the first) at `block_no` waits for its retry:
/// [`SHORT_RETRY_BLOCKS`] when it is tied to a grace window (inside one, or within
/// [`GRACE_TAIL_BLOCKS`] after it closed; `grace_until` 0: none was opened), otherwise
/// [`RETRY_BLOCKS`], doubled for each step-down before it.
pub(crate) const fn retry_wait(step_downs: u32, block_no: u64, grace_until: u64) -> u64 {
    if grace_until > 0 && block_no < grace_until.saturating_add(GRACE_TAIL_BLOCKS) {
        SHORT_RETRY_BLOCKS
    } else {
        RETRY_BLOCKS << (step_downs - 1)
    }
}

/// Per block in the window: over [`WINDOW_SLOW`], [`WINDOW_HEAVY`], [`GRACE_SLOW`] (the
/// judged time), and over one of the audio pipeline's lines ([`PIPELINE_LINE`] judged, or
/// [`PIPELINE_STARVED_WALL`] by the wall clock).
const BIT_SLOW: u8 = 1;
const BIT_HEAVY: u8 = 2;
const BIT_GRACE: u8 = 4;
const BIT_LINE: u8 = 8;

/// The step-down rules over the stage's recent blocks' times.
#[derive(Debug, Clone)]
struct Rules {
    /// Per block of the last [`WINDOW`]: the `BIT_*` lines it went over.
    ring: [u8; WINDOW],
    pos: usize,
    slow: u32,
    heavy: u32,
    grace_slow: u32,
    line: u32,
    /// Model blocks in a row over [`STARVED_WALL`].
    starved: u32,
    /// The latest block was in a grace window.
    grace: bool,
}

impl Rules {
    const fn new() -> Self {
        Self {
            ring: [0; WINDOW],
            pos: 0,
            slow: 0,
            heavy: 0,
            grace_slow: 0,
            line: 0,
            starved: 0,
            grace: false,
        }
    }

    fn clear(&mut self) {
        *self = Self::new();
    }

    /// Forgets the judged times (a grace window closed): the usual rules start afresh. The
    /// pipeline's line stays: the pipeline's window does not forget it.
    fn forget_judged(&mut self) {
        for b in &mut self.ring {
            *b &= BIT_LINE;
        }
        self.slow = 0;
        self.heavy = 0;
        self.grace_slow = 0;
    }

    /// A block of the window went over one of the pipeline's own lines.
    fn over_pipeline_line(&self) -> bool {
        self.line > 0
    }

    /// One block's times (ns): `judged` (the thread's CPU time where there is a clock for it)
    /// and `wall`; `model`: the model ran in it; `grace`: the block is in a grace window. The
    /// rule it trips, if any (only a model block trips one). The starvation cutoff applies
    /// everywhere; in a grace window only the grace rule beside it (the pipeline's own line);
    /// the window's judged times are forgotten when it closes, so the usual rules start afresh.
    fn push(&mut self, judged: u64, wall: u64, model: bool, grace: bool) -> Option<StepDownReason> {
        if self.grace && !grace {
            self.forget_judged();
        }
        self.grace = grace;
        let old = self.ring[self.pos];
        self.slow -= u32::from(old & BIT_SLOW != 0);
        self.heavy -= u32::from(old & BIT_HEAVY != 0);
        self.grace_slow -= u32::from(old & BIT_GRACE != 0);
        self.line -= u32::from(old & BIT_LINE != 0);
        let mut new = 0;
        if judged > WINDOW_SLOW {
            new |= BIT_SLOW;
        }
        if judged > WINDOW_HEAVY {
            new |= BIT_HEAVY;
        }
        if judged > GRACE_SLOW {
            new |= BIT_GRACE;
        }
        if judged > PIPELINE_LINE || wall >= PIPELINE_STARVED_WALL {
            new |= BIT_LINE;
        }
        self.ring[self.pos] = new;
        self.slow += u32::from(new & BIT_SLOW != 0);
        self.heavy += u32::from(new & BIT_HEAVY != 0);
        self.grace_slow += u32::from(new & BIT_GRACE != 0);
        self.line += u32::from(new & BIT_LINE != 0);
        self.pos = (self.pos + 1) % WINDOW;
        if !model {
            self.starved = 0;
            return None;
        }
        self.starved = if wall > STARVED_WALL {
            self.starved + 1
        } else {
            0
        };
        if self.starved >= STARVED_BLOCKS {
            return Some(StepDownReason::Starved);
        }
        if grace {
            return (self.grace_slow >= GRACE_SLOW_COUNT).then_some(StepDownReason::SlowWindow);
        }
        if judged >= SLOW_BLOCK {
            Some(StepDownReason::SlowBlock)
        } else if self.slow >= WINDOW_SLOW_COUNT {
            Some(StepDownReason::SlowWindow)
        } else if self.heavy >= WINDOW_HEAVY_COUNT {
            Some(StepDownReason::Heavy)
        } else {
            None
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    impl Rules {
        /// A model block whose judged time and wall time are both `ns`.
        fn model(&mut self, ns: u64, grace: bool) -> Option<StepDownReason> {
            self.push(ns, ns, true, grace)
        }
    }

    /// On the judged time, the rules trip strictly before kessel-audio's strike rule (two blocks in a row of 9 ms
    /// or more, or over 6 ms at the 99th percentile of 100: two blocks over 6 ms) could.
    #[test]
    fn rules() {
        let mut r = Rules::new();
        for _ in 0..1_000 {
            assert_eq!(r.model(MS, false), None, "1 ms blocks never trip");
        }
        assert_eq!(r.model(6 * MS, false), Some(StepDownReason::SlowBlock));
        r.clear();
        assert_eq!(r.model(5_999_999, false), None, "one slow block under 6 ms");
        for _ in 0..98 {
            assert_eq!(r.model(MS, false), None);
        }
        assert_eq!(
            r.model(4 * MS + 1, false),
            Some(StepDownReason::SlowWindow),
            "the second over 4 ms within 100"
        );
        r.clear();
        assert_eq!(r.model(5 * MS, false), None);
        for _ in 0..99 {
            assert_eq!(r.model(MS, false), None);
        }
        assert_eq!(
            r.model(5 * MS, false),
            None,
            "the first one left the window"
        );
        r.clear();
        for i in 0..49 {
            assert_eq!(r.model(3 * MS + 1, false), None, "{i}");
            assert_eq!(r.model(MS, false), None);
        }
        assert_eq!(
            r.model(3 * MS + 1, false),
            Some(StepDownReason::Heavy),
            "half the window over 3 ms"
        );
        // kessel-audio's first possible strike needs a second block of 6 ms or more; the
        // first one has already stepped down.
        r.clear();
        assert_eq!(r.model(9 * MS, false), Some(StepDownReason::SlowBlock));
    }

    /// In a grace window the single-block, percentile and median rules do not trip: one block
    /// over 6 ms and any number between 3 and 6 ms pass. The second block over 6 ms among the
    /// last 100 does (kessel-audio's percentile rule strikes at the same count). The same
    /// blocks after the window trip as usual, and the window's blocks are forgotten when it
    /// closes.
    #[test]
    fn grace() {
        let mut r = Rules::new();
        assert_eq!(r.model(9 * MS, true), None, "a single slow block in grace");
        for _ in 0..99 {
            assert_eq!(
                r.model(5 * MS, true),
                None,
                "p99 and median blocks in grace"
            );
        }
        assert_eq!(
            r.model(6 * MS + 1, true),
            None,
            "the 9 ms block left the window: one over 6 ms again"
        );
        assert_eq!(
            r.model(7 * MS, true),
            Some(StepDownReason::SlowWindow),
            "the second over 6 ms within 100, in grace"
        );
        // The same blocks after the window: each rule trips as usual.
        r.clear();
        assert_eq!(r.model(9 * MS, false), Some(StepDownReason::SlowBlock));
        r.clear();
        assert_eq!(r.model(5 * MS, false), None);
        assert_eq!(r.model(5 * MS, false), Some(StepDownReason::SlowWindow));
        r.clear();
        for _ in 0..49 {
            assert_eq!(r.model(3 * MS + 1, false), None);
        }
        assert_eq!(r.model(3 * MS + 1, false), Some(StepDownReason::Heavy));
        // The window's blocks do not count after it closes.
        r.clear();
        assert_eq!(r.model(7 * MS, true), None);
        for _ in 0..30 {
            assert_eq!(r.model(5 * MS, true), None);
        }
        assert_eq!(r.model(MS, false), None, "grace closed: a fresh window");
        assert_eq!(
            r.model(5 * MS, false),
            None,
            "the grace window's 5 ms blocks are gone"
        );
        assert_eq!(r.model(5 * MS, false), Some(StepDownReason::SlowWindow));
    }

    /// A step-down inside a grace window, or within 5 s after it closed, is retried after 5 s;
    /// any other keeps the schedule: 30 s, then 60 s.
    #[test]
    fn retry_waits() {
        // No grace window was opened.
        assert_eq!(retry_wait(1, 100, 0), RETRY_BLOCKS);
        assert_eq!(retry_wait(2, 100, 0), RETRY_BLOCKS << 1);
        // Inside a window (until block 300), and in the 5 s after it.
        assert_eq!(retry_wait(1, 100, 300), SHORT_RETRY_BLOCKS);
        assert_eq!(retry_wait(1, 300, 300), SHORT_RETRY_BLOCKS);
        assert_eq!(retry_wait(1, 799, 300), SHORT_RETRY_BLOCKS);
        assert_eq!(retry_wait(2, 799, 300), SHORT_RETRY_BLOCKS);
        // Later: the schedule is unchanged.
        assert_eq!(retry_wait(1, 800, 300), RETRY_BLOCKS);
        assert_eq!(retry_wait(2, 5_000, 300), RETRY_BLOCKS << 1);
        assert_eq!(SHORT_RETRY_BLOCKS, 500);
        assert_eq!(RETRY_BLOCKS, 3_000);
    }

    /// The weights in Sew's repository (`assets/dfn3/`). Outside it (the published package)
    /// the directory is absent: the test says so loudly and skips. In the repository a missing
    /// or altered file fails.
    #[allow(clippy::print_stderr)]
    pub(crate) fn model() -> Option<Arc<Dfn3Model>> {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/dfn3");
        if !dir.is_dir() {
            eprintln!(
                "SKIPPED: {} is absent: the Strong layer's fault test did not run",
                dir.display()
            );
            return None;
        }
        match Dfn3Model::load(dir.join(crate::dfn3::WEIGHTS_FILE)) {
            Ok(m) => Some(m),
            Err(e) => panic!("the repository's DeepFilterNet3 weights: {e}"),
        }
    }

    /// A model output that is not finite is a fault, final at once, inside a grace window as
    /// anywhere: no fade, no retry, retired for the stage's life.
    #[test]
    fn a_fault_in_grace_is_final() {
        let Some(model) = model() else { return };
        let mut s = Strong::new(model);
        let input = [0.01f32; FRAME];
        let mut block = [0.0f32; FRAME];
        let grace_until = 10_000;
        let mut one = |s: &mut Strong, b: u64| {
            s.begin(&input, true, b);
            let o = s.end(&mut block, b);
            s.judge(MS, MS, o.ran, b, true, false, grace_until);
            o
        };
        for b in 0..20 {
            one(&mut s, b);
        }
        assert_eq!(s.state(20), StrongState::Running);
        s.poison = true;
        let o = one(&mut s, 20);
        assert!(o.ran && !o.active, "the faulty block's output is not used");
        assert_eq!(s.last, Some(StepDownReason::Fault));
        assert_eq!(s.step_downs, 1);
        assert_eq!(s.state(21), StrongState::Retired);
        s.poison = false;
        for b in 21..2_000 {
            let o = one(&mut s, b);
            assert!(!o.ran, "never retried");
        }
        assert_eq!(s.state(2_000), StrongState::Retired);
    }

    /// The rules judge the judged time (the thread's CPU time): blocks that are long only by
    /// the wall clock, because the thread waited for a core, do not trip them, however many,
    /// short of the starvation cutoff. The same times as CPU time do.
    #[test]
    fn wall_time_alone_does_not_trip_the_rules() {
        let mut r = Rules::new();
        for i in 0..1_000 {
            // 9 ms by the wall clock, 1 ms of CPU: a pre-empted block, every block.
            assert_eq!(r.push(MS, 9 * MS, true, false), None, "{i}");
        }
        for i in 0..1_000 {
            // Every other block over the starvation line: never three in a row.
            let wall = if i % 3 == 2 { MS } else { 30 * MS };
            assert_eq!(r.push(MS, wall, true, false), None, "{i}");
        }
        r.clear();
        assert_eq!(
            r.push(6 * MS, 6 * MS, true, false),
            Some(StepDownReason::SlowBlock),
            "6 ms of CPU still steps down"
        );
        r.clear();
        assert_eq!(r.push(5 * MS, 5 * MS, true, false), None);
        assert_eq!(
            r.push(5 * MS, 5 * MS, true, false),
            Some(StepDownReason::SlowWindow),
            "the percentile on CPU time"
        );
    }

    /// The starvation cutoff: three model blocks in a row over 10 ms by the wall clock step
    /// down, in a grace window too, whatever their CPU time; a block under the line, or a
    /// block without the model, starts the count again.
    #[test]
    fn starvation_cutoff() {
        let mut r = Rules::new();
        assert_eq!(r.push(MS, 11 * MS, true, false), None);
        assert_eq!(r.push(MS, 11 * MS, true, false), None);
        assert_eq!(
            r.push(MS, 11 * MS, true, false),
            Some(StepDownReason::Starved),
            "the third in a row"
        );
        r.clear();
        assert_eq!(r.push(MS, 11 * MS, true, true), None);
        assert_eq!(r.push(MS, 11 * MS, true, true), None);
        assert_eq!(
            r.push(MS, 11 * MS, true, true),
            Some(StepDownReason::Starved),
            "in a grace window too"
        );
        r.clear();
        assert_eq!(r.push(MS, 11 * MS, true, false), None);
        assert_eq!(r.push(MS, 11 * MS, true, false), None);
        assert_eq!(
            r.push(MS, 10 * MS, true, false),
            None,
            "at the line: not over"
        );
        assert_eq!(r.push(MS, 11 * MS, true, false), None);
        assert_eq!(r.push(MS, 11 * MS, true, false), None);
        assert_eq!(
            r.push(MS, 11 * MS, false, false),
            None,
            "no model: never trips"
        );
        assert_eq!(r.push(MS, 11 * MS, true, false), None, "counted afresh");
        assert_eq!(STARVED_WALL, 10 * MS);
        assert_eq!(STARVED_BLOCKS, 3);
    }

    /// The window covers the stage's last 100 blocks, the model's or not: a block over one of
    /// the pipeline's lines (6 ms judged, or 20 ms by the wall clock) is seen for 100 blocks,
    /// then gone; a grace window closing forgets the judged times but not that line.
    #[test]
    fn the_window_ages_with_every_block() {
        let mut r = Rules::new();
        assert_eq!(r.push(MS, 19 * MS, false, false), None);
        assert!(
            !r.over_pipeline_line(),
            "long by the wall clock only, under the starvation line"
        );
        assert_eq!(r.push(MS, 20 * MS, false, false), None);
        assert!(r.over_pipeline_line(), "the starvation line");
        r.clear();
        assert_eq!(r.push(7 * MS, 7 * MS, false, false), None);
        for _ in 0..100 {
            assert!(r.over_pipeline_line());
            assert_eq!(r.push(MS, MS, false, false), None);
        }
        assert!(!r.over_pipeline_line(), "aged out after 100 blocks");
        r.clear();
        assert_eq!(r.push(7 * MS, 7 * MS, true, true), None, "one in grace");
        assert_eq!(r.push(MS, MS, false, false), None, "grace closed");
        assert!(r.over_pipeline_line(), "the pipeline's line is kept");
        assert_eq!(
            r.model(5 * MS, false),
            None,
            "the judged times were forgotten"
        );
    }

    /// A retry's warm-up is judged at its end on its 99th percentile (of 50 blocks, the second
    /// slowest) and, outside a grace window, its median.
    #[test]
    fn a_warm_up_is_judged_by_its_percentile() {
        let mut t = [MS; PROBE_BLOCKS as usize];
        assert_eq!(probe_verdict(&t, false), None);
        t[7] = 9 * MS;
        assert_eq!(
            probe_verdict(&t, false),
            None,
            "one slow block alone passes"
        );
        t[30] = 4 * MS + 1;
        assert_eq!(
            probe_verdict(&t, false),
            Some(StepDownReason::SlowWindow),
            "two over 4 ms"
        );
        assert_eq!(probe_verdict(&t, true), None, "in grace the line is 6 ms");
        t[30] = 6 * MS + 1;
        assert_eq!(probe_verdict(&t, true), Some(StepDownReason::SlowWindow));
        let mut m = [MS; PROBE_BLOCKS as usize];
        for v in m.iter_mut().take(25) {
            *v = 3 * MS + 1;
        }
        assert_eq!(probe_verdict(&m, false), None, "under half over 3 ms");
        m[25] = 3 * MS + 1;
        assert_eq!(probe_verdict(&m, false), Some(StepDownReason::Heavy));
        assert_eq!(probe_verdict(&m, true), None, "no median rule in grace");
        assert_eq!(probe_verdict(&[], false), None);
    }

    /// Runs one block of `s` at `b`: the model's blocks take `model_ns`, the others 1 ms, by
    /// both clocks.
    fn block(s: &mut Strong, b: u64, model_ns: u64, grace_until: u64) {
        let input = [0.01f32; FRAME];
        let mut out = [0.0f32; FRAME];
        s.begin(&input, true, b);
        let o = s.end(&mut out, b);
        let ns = if o.ran { model_ns } else { MS };
        s.judge(ns, ns, o.ran, b, true, false, grace_until);
    }

    /// Step-downs in a grace window do not count towards retiring the model: each is retried
    /// after 5 s, past the three that retire it outside a window, until the separate bound
    /// for them. Outside a window the third step-down is final, as before.
    #[test]
    fn step_downs_in_grace_do_not_retire_the_model() {
        let Some(model) = model() else { return };
        // Every model block is over 6 ms in a window that never closes here.
        let grace_until = 1_000_000;
        let mut s = Strong::new(Arc::clone(&model));
        let mut seen = 0;
        for b in 0..20_000 {
            block(&mut s, b, 7 * MS, grace_until);
            if s.step_downs != seen {
                seen = s.step_downs;
                assert_eq!(s.grace_step_downs, seen);
                if seen < MAX_GRACE_STEP_DOWNS {
                    assert_ne!(s.state(b + 1), StrongState::Retired, "{seen}");
                    assert_eq!(s.retry_at(), b + SHORT_RETRY_BLOCKS);
                }
            }
            if s.state(b + 1) == StrongState::Retired {
                break;
            }
        }
        assert_eq!(s.step_downs, MAX_GRACE_STEP_DOWNS);
        assert_eq!(s.grace_step_downs, MAX_GRACE_STEP_DOWNS);
        const { assert!(MAX_GRACE_STEP_DOWNS > MAX_STEP_DOWNS) };
        assert_eq!(s.state(u64::MAX), StrongState::Retired, "bounded");

        // No window: the third is final.
        let mut s = Strong::new(model);
        for b in 0..20_000 {
            block(&mut s, b, 7 * MS, 0);
            if s.state(b + 1) == StrongState::Retired {
                break;
            }
        }
        assert_eq!(s.step_downs, MAX_STEP_DOWNS);
        assert_eq!(s.grace_step_downs, 0);
        assert_eq!(s.state(u64::MAX), StrongState::Retired);
    }

    /// A step-down within a second after a reset counts as one in a grace window (5 s retry,
    /// not counted); later ones count as usual.
    #[test]
    fn a_step_down_just_after_a_reset_does_not_count() {
        let Some(model) = model() else { return };
        let mut s = Strong::new(model);
        for b in 0..1_000 {
            block(&mut s, b, MS, 0);
        }
        s.reset(1_000);
        block(&mut s, 1_000, MS, 0);
        block(&mut s, 1_001, 7 * MS, 0);
        assert_eq!((s.step_downs, s.grace_step_downs), (1, 1));
        assert_eq!(s.retry_at(), 1_001 + SHORT_RETRY_BLOCKS);
        // The retry (its 0.5 s warm-up passes), then a slow block well after the reset.
        for b in 1_002..2_000 {
            block(&mut s, b, MS, 0);
        }
        assert_eq!(s.state(2_000), StrongState::Running);
        block(&mut s, 2_000, 7 * MS, 0);
        assert_eq!((s.step_downs, s.grace_step_downs), (2, 1));
        assert_eq!(
            s.retry_at(),
            2_000 + RETRY_BLOCKS,
            "the first counted one: 30 s"
        );
    }

    /// A retry does not start while a block of the last 100 went over the audio pipeline's
    /// percentile line; the state stays stepped down meanwhile. Its warm-up is judged
    /// on its percentile: one slow block in it passes.
    #[test]
    fn a_retry_waits_for_the_pipeline_window() {
        let Some(model) = model() else { return };
        let mut s = Strong::new(model);
        let input = [0.01f32; FRAME];
        let mut out = [0.0f32; FRAME];
        block(&mut s, 0, 7 * MS, 0);
        assert_eq!(s.step_downs, 1);
        let retry = s.retry_at();
        for b in 1..retry - 10 {
            block(&mut s, b, MS, 0);
        }
        // A block over 6 ms (not the model's: it is not running), 10 blocks before the retry
        // is due.
        s.begin(&input, true, retry - 10);
        let o = s.end(&mut out, retry - 10);
        assert!(!o.ran);
        s.judge(7 * MS, 7 * MS, false, retry - 10, true, false, 0);
        for b in retry - 9..retry + 90 {
            block(&mut s, b, MS, 0);
            if b >= retry {
                assert!(s.held, "{b}");
                assert_eq!(s.state(b + 1), StrongState::SteppedDown, "{b}");
            }
        }
        // 100 blocks after it, the retry starts; one block of its warm-up over 6 ms of CPU.
        for b in retry + 90..retry + 400 {
            let ns = if b == retry + 100 { 9 * MS } else { MS };
            block(&mut s, b, ns, 0);
        }
        assert!(!s.held);
        assert_eq!(s.step_downs, 1, "the warm-up's one slow block passed");
        assert_eq!(s.state(retry + 400), StrongState::Running);
    }
}
