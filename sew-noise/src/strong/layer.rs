//! The Strong layer itself (cargo feature `dfn3`): see the parent module.

use super::{StepDownReason, StrongState};
// The thresholds below are explained in the module docs of src/strong.rs.
use std::sync::Arc;

use crate::FRAME;
use crate::dfn3::{Dfn3Model, Dfn3Stream, tail_weight};

/// One block at or over this steps down at once (ns).
pub(crate) const SLOW_BLOCK: u64 = 6_000_000;
/// The rolling window, in model blocks.
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
/// The step-down that is final.
pub(crate) const MAX_STEP_DOWNS: u32 = 3;
/// In a grace window a block over this counts towards [`GRACE_SLOW_COUNT`] (ns): the line of
/// the audio pipeline's own percentile rule.
pub(crate) const GRACE_SLOW: u64 = 6_000_000;
/// This many blocks over [`GRACE_SLOW`] among the last [`WINDOW`] model blocks step down in a
/// grace window: the pipeline's percentile rule strikes at the same count.
pub(crate) const GRACE_SLOW_COUNT: u32 = 2;
/// A step-down in a grace window, or within this many blocks (5 s) after one closed, is
/// retried after [`SHORT_RETRY_BLOCKS`].
pub(crate) const GRACE_TAIL_BLOCKS: u64 = 500;
/// The retry of a step-down tied to a grace window waits this many blocks (5 s).
pub(crate) const SHORT_RETRY_BLOCKS: u64 = 500;
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
    pub(crate) step_downs: u32,
    pub(crate) last: Option<StepDownReason>,
    pub(crate) last_block: u64,
    retry_at: u64,
    retired: bool,
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
            last: None,
            last_block: 0,
            retry_at: 0,
            retired: false,
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

    /// Forget held audio (a reset): the model starts again from a clear state, with a
    /// one-block warm-up, unless it is stepped down. The step-down history stays.
    pub(crate) fn reset(&mut self) {
        self.plan = Plan::Idle;
        self.stream.clear();
        self.rules.clear();
        self.run = Run::Idle;
    }

    pub(crate) fn state(&self, block: u64) -> StrongState {
        if self.retired {
            return StrongState::Retired;
        }
        match self.run {
            Run::Warm { .. } => StrongState::Warming,
            Run::FadeIn | Run::Active | Run::Leaving => StrongState::Running,
            Run::Idle if self.step_downs > 0 && block < self.retry_at => StrongState::SteppedDown,
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
                return;
            }
            self.stream.clear();
            self.rules.clear();
            let left = if self.step_downs == 0 {
                1
            } else {
                PROBE_BLOCKS
            };
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

    /// After a block in which the model ran, with the block's time: steps down if a rule
    /// says so (`judge`: the rules apply; off for measurement). `force`: step down now.
    /// `grace_until`: the stage's grace window runs until this block count (0: none was
    /// opened); inside it only the grace rule applies.
    pub(crate) fn judge(
        &mut self,
        ns: u64,
        block_no: u64,
        judge: bool,
        force: bool,
        grace_until: u64,
    ) {
        if !matches!(self.run, Run::Warm { .. } | Run::FadeIn | Run::Active) {
            return;
        }
        if force {
            self.step_down(StepDownReason::Forced, block_no, grace_until);
            return;
        }
        if !judge {
            return;
        }
        if let Some(r) = self.rules.push(ns, block_no < grace_until) {
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
        self.rules.clear();
        if self.step_downs >= MAX_STEP_DOWNS {
            self.retired = true;
        } else {
            self.retry_at = block_no + retry_wait(self.step_downs, block_no, grace_until);
        }
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

/// The step-down rules over the model blocks' times.
#[derive(Debug, Clone)]
struct Rules {
    /// Per model block in the window: bit 0 over [`WINDOW_SLOW`], bit 1 over
    /// [`WINDOW_HEAVY`], bit 2 over [`GRACE_SLOW`].
    ring: [u8; WINDOW],
    pos: usize,
    slow: u32,
    heavy: u32,
    grace_slow: u32,
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
            grace: false,
        }
    }

    fn clear(&mut self) {
        *self = Self::new();
    }

    /// One model block's time (ns); `grace`: the block is in a grace window. The rule it
    /// trips, if any. In a grace window only the grace rule applies (the pipeline's own line);
    /// the window's blocks are forgotten when it closes, so the usual rules start afresh.
    fn push(&mut self, ns: u64, grace: bool) -> Option<StepDownReason> {
        if self.grace && !grace {
            self.clear();
        }
        self.grace = grace;
        let old = self.ring[self.pos];
        self.slow -= u32::from(old & 1);
        self.heavy -= u32::from((old >> 1) & 1);
        self.grace_slow -= u32::from((old >> 2) & 1);
        let new = u8::from(ns > WINDOW_SLOW)
            | (u8::from(ns > WINDOW_HEAVY) << 1)
            | (u8::from(ns > GRACE_SLOW) << 2);
        self.ring[self.pos] = new;
        self.slow += u32::from(new & 1);
        self.heavy += u32::from((new >> 1) & 1);
        self.grace_slow += u32::from((new >> 2) & 1);
        self.pos = (self.pos + 1) % WINDOW;
        if grace {
            return (self.grace_slow >= GRACE_SLOW_COUNT).then_some(StepDownReason::SlowWindow);
        }
        if ns >= SLOW_BLOCK {
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
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    /// The rules trip strictly before kessel-audio's strike rule (two blocks in a row of 9 ms
    /// or more, or over 6 ms at the 99th percentile of 100: two blocks over 6 ms) could.
    #[test]
    fn rules() {
        let mut r = Rules::new();
        for _ in 0..1_000 {
            assert_eq!(r.push(MS, false), None, "1 ms blocks never trip");
        }
        assert_eq!(r.push(6 * MS, false), Some(StepDownReason::SlowBlock));
        r.clear();
        assert_eq!(r.push(5_999_999, false), None, "one slow block under 6 ms");
        for _ in 0..98 {
            assert_eq!(r.push(MS, false), None);
        }
        assert_eq!(
            r.push(4 * MS + 1, false),
            Some(StepDownReason::SlowWindow),
            "the second over 4 ms within 100"
        );
        r.clear();
        assert_eq!(r.push(5 * MS, false), None);
        for _ in 0..99 {
            assert_eq!(r.push(MS, false), None);
        }
        assert_eq!(r.push(5 * MS, false), None, "the first one left the window");
        r.clear();
        for i in 0..49 {
            assert_eq!(r.push(3 * MS + 1, false), None, "{i}");
            assert_eq!(r.push(MS, false), None);
        }
        assert_eq!(
            r.push(3 * MS + 1, false),
            Some(StepDownReason::Heavy),
            "half the window over 3 ms"
        );
        // kessel-audio's first possible strike needs a second block of 6 ms or more; the
        // first one has already stepped down.
        r.clear();
        assert_eq!(r.push(9 * MS, false), Some(StepDownReason::SlowBlock));
    }

    /// In a grace window the single-block, percentile and median rules do not trip: one block
    /// over 6 ms and any number between 3 and 6 ms pass. The second block over 6 ms among the
    /// last 100 does (kessel-audio's percentile rule strikes at the same count). The same
    /// blocks after the window trip as usual, and the window's blocks are forgotten when it
    /// closes.
    #[test]
    fn grace() {
        let mut r = Rules::new();
        assert_eq!(r.push(9 * MS, true), None, "a single slow block in grace");
        for _ in 0..99 {
            assert_eq!(r.push(5 * MS, true), None, "p99 and median blocks in grace");
        }
        assert_eq!(
            r.push(6 * MS + 1, true),
            None,
            "the 9 ms block left the window: one over 6 ms again"
        );
        assert_eq!(
            r.push(7 * MS, true),
            Some(StepDownReason::SlowWindow),
            "the second over 6 ms within 100, in grace"
        );
        // The same blocks after the window: each rule trips as usual.
        r.clear();
        assert_eq!(r.push(9 * MS, false), Some(StepDownReason::SlowBlock));
        r.clear();
        assert_eq!(r.push(5 * MS, false), None);
        assert_eq!(r.push(5 * MS, false), Some(StepDownReason::SlowWindow));
        r.clear();
        for _ in 0..49 {
            assert_eq!(r.push(3 * MS + 1, false), None);
        }
        assert_eq!(r.push(3 * MS + 1, false), Some(StepDownReason::Heavy));
        // The window's blocks do not count after it closes.
        r.clear();
        assert_eq!(r.push(7 * MS, true), None);
        for _ in 0..30 {
            assert_eq!(r.push(5 * MS, true), None);
        }
        assert_eq!(r.push(MS, false), None, "grace closed: a fresh window");
        assert_eq!(
            r.push(5 * MS, false),
            None,
            "the grace window's 5 ms blocks are gone"
        );
        assert_eq!(r.push(5 * MS, false), Some(StepDownReason::SlowWindow));
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
    fn model() -> Option<Arc<Dfn3Model>> {
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
            if o.ran {
                s.judge(MS, b, true, false, grace_until);
            }
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
}
