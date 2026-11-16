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
        let mut stream = Dfn3Stream::new(model);
        let mut input = [0.0f32; FRAME];
        let mut out = [0.0f32; FRAME];
        let mut seed = 0x2545_f491_u32;
        for b in 0..40 {
            // Noise, louder then quieter, so the model's SNR estimate crosses its thresholds
            // and every decoder runs at least once.
            let level = if b % 8 < 4 { 0.3 } else { 0.003 };
            for (i, s) in input.iter_mut().enumerate() {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let noise = (seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
                let tone = ((b * FRAME + i) as f32 * 0.0523).sin();
                *s = level * (0.5 * noise + 0.5 * tone);
            }
            stream.process(&input, &mut out);
        }
        stream.clear();
        Self {
            stream,
            out: [0.0; FRAME],
            run: Run::Idle,
            rules: Rules::new(),
            step_downs: 0,
            last: None,
            last_block: 0,
            retry_at: 0,
            retired: false,
        }
    }

    /// Forget held audio (a reset): the model starts again from a clear state, with a
    /// one-block warm-up, unless it is stepped down. The step-down history stays.
    pub(crate) fn reset(&mut self) {
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

    /// One block. `input` is the block that came in (sanitised); `block` holds Standard's
    /// output for the block going out (or the dry signal) and gets this layer's. `want`:
    /// Strong is asked for. `block_no`: the stage's block count.
    pub(crate) fn process(
        &mut self,
        input: &[f32; FRAME],
        block: &mut [f32; FRAME],
        want: bool,
        block_no: u64,
    ) -> Outcome {
        let idle = Outcome {
            ran: false,
            active: false,
        };
        if self.run == Run::Idle {
            if !want || self.retired || block_no < self.retry_at {
                return idle;
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
            Run::Idle => idle,
            Run::Leaving => {
                self.fade_out(block);
                self.run = Run::Idle;
                Outcome {
                    ran: false,
                    active: true,
                }
            }
            Run::Warm { left } => {
                if !want {
                    self.run = Run::Idle;
                    return idle;
                }
                self.stream.process(input, &mut self.out);
                if !self.out.iter().all(|v| v.is_finite()) {
                    self.fault(block_no);
                    return Outcome {
                        ran: true,
                        active: false,
                    };
                }
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
            Run::FadeIn | Run::Active => {
                if !want {
                    // FadeIn without Strong: Standard's output never left.
                    self.run = Run::Idle;
                    return idle;
                }
                self.stream.process(input, &mut self.out);
                if !self.out.iter().all(|v| v.is_finite()) {
                    self.fault(block_no);
                    return Outcome {
                        ran: true,
                        active: false,
                    };
                }
                if self.run == Run::FadeIn {
                    for (i, (b, &d)) in block.iter_mut().zip(&self.out).enumerate() {
                        let t = (i + 1) as f32 / FRAME as f32;
                        *b = (*b * (1.0 - t) + d * t).clamp(-1.0, 1.0);
                    }
                    self.run = Run::Active;
                } else {
                    for (b, &d) in block.iter_mut().zip(&self.out) {
                        *b = d.clamp(-1.0, 1.0);
                    }
                }
                Outcome {
                    ran: true,
                    active: true,
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
        self.step_down(StepDownReason::Fault, block_no);
        // The tail may be just as bad: no fade, Standard's output at once.
        self.run = Run::Idle;
        self.retired = true;
    }

    /// After a block in which the model ran, with the block's time: steps down if a rule
    /// says so (`judge`: the rules apply; off for measurement). `force`: step down now.
    pub(crate) fn judge(&mut self, ns: u64, block_no: u64, judge: bool, force: bool) {
        if !matches!(self.run, Run::Warm { .. } | Run::FadeIn | Run::Active) {
            return;
        }
        if force {
            self.step_down(StepDownReason::Forced, block_no);
            return;
        }
        if !judge {
            return;
        }
        if let Some(r) = self.rules.push(ns) {
            self.step_down(r, block_no);
        }
    }

    fn step_down(&mut self, reason: StepDownReason, block_no: u64) {
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
            self.retry_at = block_no + (RETRY_BLOCKS << (self.step_downs - 1));
        }
    }
}

/// The step-down rules over the model blocks' times.
#[derive(Debug, Clone)]
struct Rules {
    /// Per model block in the window: bit 0 over [`WINDOW_SLOW`], bit 1 over
    /// [`WINDOW_HEAVY`].
    ring: [u8; WINDOW],
    pos: usize,
    slow: u32,
    heavy: u32,
}

impl Rules {
    const fn new() -> Self {
        Self {
            ring: [0; WINDOW],
            pos: 0,
            slow: 0,
            heavy: 0,
        }
    }

    fn clear(&mut self) {
        *self = Self::new();
    }

    /// One model block's time (ns): the rule it trips, if any.
    fn push(&mut self, ns: u64) -> Option<StepDownReason> {
        let old = self.ring[self.pos];
        self.slow -= u32::from(old & 1);
        self.heavy -= u32::from((old >> 1) & 1);
        let new = u8::from(ns > WINDOW_SLOW) | (u8::from(ns > WINDOW_HEAVY) << 1);
        self.ring[self.pos] = new;
        self.slow += u32::from(new & 1);
        self.heavy += u32::from((new >> 1) & 1);
        self.pos = (self.pos + 1) % WINDOW;
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
            assert_eq!(r.push(MS), None, "1 ms blocks never trip");
        }
        assert_eq!(r.push(6 * MS), Some(StepDownReason::SlowBlock));
        r.clear();
        assert_eq!(r.push(5_999_999), None, "one slow block under 6 ms");
        for _ in 0..98 {
            assert_eq!(r.push(MS), None);
        }
        assert_eq!(
            r.push(4 * MS + 1),
            Some(StepDownReason::SlowWindow),
            "the second over 4 ms within 100"
        );
        r.clear();
        assert_eq!(r.push(5 * MS), None);
        for _ in 0..99 {
            assert_eq!(r.push(MS), None);
        }
        assert_eq!(r.push(5 * MS), None, "the first one left the window");
        r.clear();
        for i in 0..49 {
            assert_eq!(r.push(3 * MS + 1), None, "{i}");
            assert_eq!(r.push(MS), None);
        }
        assert_eq!(
            r.push(3 * MS + 1),
            Some(StepDownReason::Heavy),
            "half the window over 3 ms"
        );
        // kessel-audio's first possible strike needs a second block of 6 ms or more; the
        // first one has already stepped down.
        r.clear();
        assert_eq!(r.push(9 * MS), Some(StepDownReason::SlowBlock));
    }
}
