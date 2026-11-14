//! Standard: what the stage and the stream do with the model's output to protect the voice.
//! A floor (the input always stays in the output, 30 dB under its own level) and an onset
//! guard (the input passes unchanged for 40 ms from a lasting, not spiky, 20 dB rise in level).
//! Both work on whole [`FRAME`]-sample frames: the frame going out and the newest frame in,
//! which is one frame ahead of it.

use crate::FRAME;

/// Standard keeps this much of the input in its output (-30 dB), so nothing is ever taken
/// down further than that.
pub(crate) const FLOOR: f32 = 0.031_622_78;
/// The onset guard opens at a block whose level is this many times (20 dB) the noise floor.
const GUARD_RISE: f32 = 100.0;
/// A block whose peak is more than this many times its RMS level does not open the guard: a
/// click is far spikier than the start of a word.
const GUARD_MAX_CREST: f32 = 3.0;
/// The guard needs this many blocks (30 ms) under the rise before it can open again.
const GUARD_QUIET_BLOCKS: u32 = 3;
/// Once open, the guard passes the input for this many blocks (40 ms).
const GUARD_HOLD_BLOCKS: u32 = 4;
/// The noise floor falls at once and rises by this factor a block (about 25 dB a second).
const FLOOR_RISE: f32 = 1.06;
/// The noise floor never goes under the level of a very quiet room: digital silence would
/// otherwise leave it so low that everything after counts as a rise.
const FLOOR_MIN: f32 = 1e-9;

/// Standard's state from one frame to the next.
#[derive(Debug, Clone)]
pub(crate) struct Standard {
    /// The dry share of the previous frame's last sample.
    mix: f32,
    /// Frames the onset guard still holds the input through.
    guard_left: u32,
    /// The onset guard's noise floor (power of the differenced input), and the frames since
    /// a frame was last above it by the guard's rise.
    level_floor: f32,
    quiet_blocks: u32,
    /// The previous frame rose and waits for this one to confirm it.
    pending: bool,
}

/// How one frame is mixed: the dry share moves in a straight line from `from` to `to` over
/// the frame's first `ramp` samples and stays there.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Step {
    from: f32,
    to: f32,
    ramp: usize,
    /// The onset guard opened at this frame.
    pub(crate) opened: bool,
}

impl Step {
    /// Sample `i` of the frame going out, from the model's output `wet` and the delayed
    /// input `dry` (held to full scale, as the model's output is).
    pub(crate) fn mixed(&self, i: usize, wet: f32, dry: f32) -> f32 {
        let t = ((i + 1) as f32 / self.ramp as f32).min(1.0);
        let share = self.from + (self.to - self.from) * t;
        wet + (dry.clamp(-1.0, 1.0) - wet) * share
    }
}

impl Standard {
    /// A fresh state whose first frame starts from a dry share of `mix` (0 after a fade to
    /// the model's output; [`FLOOR`] to start on the floor at once).
    pub(crate) fn new(mix: f32) -> Self {
        Self {
            mix,
            guard_left: 0,
            level_floor: 0.0,
            quiet_blocks: 0,
            pending: false,
        }
    }

    /// The mix for the frame about to go out. `newest` is the frame that just came in, one
    /// frame ahead of it.
    pub(crate) fn step(&mut self, newest: &[f32; FRAME]) -> Step {
        let (level, crest) = level_and_crest(newest);
        let level = level.max(1e-12);
        let was_open = self.guard_left > 0;
        let mut quick = false;
        if self.level_floor > 0.0 && level > self.level_floor * GUARD_RISE {
            if self.pending {
                // The rise lasted: the frame that rose is the one going out now.
                self.guard_left = self.guard_left.max(GUARD_HOLD_BLOCKS);
                quick = true;
                self.pending = false;
            } else {
                self.pending = self.quiet_blocks >= GUARD_QUIET_BLOCKS && crest <= GUARD_MAX_CREST;
            }
            self.quiet_blocks = 0;
        } else {
            self.pending = false;
            self.quiet_blocks = self.quiet_blocks.saturating_add(1);
        }
        self.level_floor = if self.level_floor == 0.0 || level < self.level_floor {
            level.max(FLOOR_MIN)
        } else {
            self.level_floor * FLOOR_RISE
        };
        let opened = !was_open && self.guard_left > 0;
        let to = if self.guard_left > 0 {
            self.guard_left -= 1;
            1.0
        } else {
            FLOOR
        };
        let step = Step {
            from: self.mix,
            to,
            // The guard fades to the input within the frame's first millisecond.
            ramp: if quick { FRAME / 10 } else { FRAME },
            opened,
        };
        self.mix = to;
        step
    }
}

/// A block's level for the onset guard, and how spiky it is: the mean power of its first
/// difference (rumble and hum mostly out), and its peak over its RMS level (about 1.4 for a
/// sine).
fn level_and_crest(block: &[f32; FRAME]) -> (f32, f32) {
    let (mut diff, mut peak, mut sum) = (0.0f32, 0.0f32, 0.0f32);
    let mut prev = block[0];
    for &x in block {
        diff += (x - prev) * (x - prev);
        prev = x;
        peak = peak.max(x.abs());
        sum += x * x;
    }
    let rms = (sum / FRAME as f32).sqrt();
    let crest = if rms > 0.0 { peak / rms } else { 0.0 };
    (diff / (FRAME - 1) as f32, crest)
}
