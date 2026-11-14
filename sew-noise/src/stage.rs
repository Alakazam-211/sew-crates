//! [`Stage`]: the native capture stage. One 480-sample block of 48 kHz mono in place, a fixed
//! latency, a mode that changes at any block without a click, and per-block timing counters.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use nnnoiseless::DenoiseState;

use crate::standard::Standard;
use crate::{Denoiser, FRAME};

thread_local! {
    /// Whether this thread has made its FFT plans (see [`warm_up_thread`]).
    static WARM: Cell<bool> = const { Cell::new(false) };
}

/// Makes the calling thread ready to denoise without allocating.
///
/// The RNNoise port makes its FFT plans on a thread's first frame and keeps them for that
/// thread, and making them allocates. This runs one frame through a throwaway state so the
/// plans exist; later calls on the same thread do nothing. [`Denoiser::new`], [`Stage::new`]
/// and [`Stage::reset`] call it, so a stage built on the thread that runs it needs nothing
/// more. A stage built on one thread and run on another must have this (or [`Stage::reset`])
/// called on the running thread before its first block; otherwise that first block allocates
/// (and [`StageStats::cold_thread_blocks`] counts it).
pub fn warm_up_thread() {
    if WARM.get() {
        return;
    }
    let mut warm = DenoiseState::new();
    let mut scratch_in = [0.0f32; FRAME];
    for (i, s) in scratch_in.iter_mut().enumerate() {
        *s = (i as f32 * 0.37).sin() * 1_000.0;
    }
    let mut scratch_out = [0.0f32; FRAME];
    warm.process_frame(&mut scratch_out[..], &scratch_in[..]);
    WARM.set(true);
}

/// How much noise removal a [`Stage`] applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Mode {
    /// None: the block passes through unchanged, at the same latency.
    Off = 0,
    /// RNNoise.
    Standard = 1,
    /// A stronger model. Not built into this version: asking for it is refused
    /// ([`ModeUnavailable`]).
    Strong = 2,
}

impl Mode {
    /// Every mode, weakest first.
    pub const ALL: [Self; 3] = [Self::Off, Self::Standard, Self::Strong];

    /// Whether this build can run the mode.
    pub const fn is_available(self) -> bool {
        !matches!(self, Self::Strong)
    }

    /// The mode's name: `off`, `standard` or `strong`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Standard => "standard",
            Self::Strong => "strong",
        }
    }

    /// The mode of that name ([`Self::as_str`]), if any.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == name)
    }

    const fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Off,
            2 => Self::Strong,
            _ => Self::Standard,
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A mode this build cannot run was asked for. The stage's mode is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeUnavailable(pub Mode);

impl std::fmt::Display for ModeUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "noise removal mode `{}` is not available", self.0)
    }
}

impl std::error::Error for ModeUnavailable {}

/// Blocks per timing window: 5 s of 10 ms blocks.
const WINDOW_BLOCKS: u32 = 500;
/// A window's 99th percentile is over the budget when more than this many of its blocks are.
const WINDOW_P99_BLOCKS: u32 = WINDOW_BLOCKS / 100;
/// The default block budget.
const DEFAULT_BUDGET: Duration = Duration::from_millis(6);

/// What the stage and its [`StageControl`]s share. Atomics only: no lock anywhere.
#[derive(Debug)]
struct Shared {
    mode: AtomicU8,
    active: AtomicU8,
    vad_bits: AtomicU32,
    budget_ns: AtomicU64,
    blocks: AtomicU64,
    over_budget: AtomicU64,
    max_block_ns: AtomicU64,
    last_block_ns: AtomicU64,
    windows: AtomicU64,
    window_over_budget: AtomicU32,
    window_max_ns: AtomicU64,
    cold_thread_blocks: AtomicU64,
    guard_opens: AtomicU64,
}

/// A snapshot of a stage's timing counters ([`StageControl::stats`]).
///
/// Times are the wall time of [`Stage::process`], measured on the thread that runs it. On
/// `wasm32` there is no clock: the block counts advance and every time is 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StageStats {
    /// The block budget in nanoseconds ([`Stage::set_block_budget`]; 6 ms by default).
    pub budget_ns: u64,
    /// Blocks processed.
    pub blocks: u64,
    /// Blocks that took longer than the budget.
    pub over_budget: u64,
    /// The slowest block, in nanoseconds.
    pub max_block_ns: u64,
    /// The latest block, in nanoseconds.
    pub last_block_ns: u64,
    /// Complete 500-block (5 s) windows so far. The windows follow one another; they do not
    /// overlap.
    pub windows: u64,
    /// Blocks over the budget in the latest complete window.
    pub window_over_budget: u32,
    /// The slowest block of the latest complete window, in nanoseconds.
    pub window_max_ns: u64,
    /// Blocks that ran on a thread which had not been warmed up ([`warm_up_thread`]): each
    /// such block allocated. 0 when the stage is used as documented.
    pub cold_thread_blocks: u64,
    /// How many times Standard's onset guard opened (let the input through at a lasting rise
    /// in level).
    pub guard_opens: u64,
}

impl StageStats {
    /// Whether the latest complete window's 99th-percentile block time was over the budget
    /// (more than 5 of its 500 blocks were): the signal to step down a mode.
    pub const fn window_p99_over_budget(&self) -> bool {
        self.window_over_budget > WINDOW_P99_BLOCKS
    }
}

/// A handle on a [`Stage`] for other threads: change the mode, read what runs, read the
/// counters. Cheap to clone; every call is a few atomic loads or stores.
#[derive(Debug, Clone)]
pub struct StageControl {
    shared: Arc<Shared>,
}

impl StageControl {
    /// Asks for `mode`. It takes effect at the stage's next block, faded over one block (two
    /// when turning removal on). A mode this build cannot run is refused and nothing changes.
    pub fn set_mode(&self, mode: Mode) -> Result<(), ModeUnavailable> {
        if !mode.is_available() {
            return Err(ModeUnavailable(mode));
        }
        self.shared.mode.store(mode as u8, Ordering::Relaxed);
        Ok(())
    }

    /// The mode last asked for.
    pub fn mode(&self) -> Mode {
        Mode::from_u8(self.shared.mode.load(Ordering::Relaxed))
    }

    /// What the latest block's output was: [`Mode::Off`] (the input, delayed) or the model that
    /// ran. It follows [`Self::mode`] by one or two blocks.
    pub fn active(&self) -> Mode {
        Mode::from_u8(self.shared.active.load(Ordering::Relaxed))
    }

    /// RNNoise's voice probability (0 to 1) for the latest block it ran on; 0 before any.
    pub fn voice_probability(&self) -> f32 {
        f32::from_bits(self.shared.vad_bits.load(Ordering::Relaxed))
    }

    /// The timing counters now.
    pub fn stats(&self) -> StageStats {
        let s = &self.shared;
        StageStats {
            budget_ns: s.budget_ns.load(Ordering::Relaxed),
            blocks: s.blocks.load(Ordering::Relaxed),
            over_budget: s.over_budget.load(Ordering::Relaxed),
            max_block_ns: s.max_block_ns.load(Ordering::Relaxed),
            last_block_ns: s.last_block_ns.load(Ordering::Relaxed),
            windows: s.windows.load(Ordering::Relaxed),
            window_over_budget: s.window_over_budget.load(Ordering::Relaxed),
            window_max_ns: s.window_max_ns.load(Ordering::Relaxed),
            cold_thread_blocks: s.cold_thread_blocks.load(Ordering::Relaxed),
            guard_opens: s.guard_opens.load(Ordering::Relaxed),
        }
    }
}

/// Where the output comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    /// The delayed input. The model is not run.
    Dry,
    /// Removal was just asked for: the model has seen one block, whose output was discarded
    /// (it would have been rebuilt from audio the model last saw before it stopped).
    Primed,
    /// The model's output.
    Wet,
}

/// Noise removal for a native capture path: one 10 ms block at a time, in place.
///
/// - A block is [`FRAME`] (480) samples of 48 kHz mono `f32` in `[-1, 1]`.
/// - The output is always [`Self::LATENCY`] (480 samples, 10 ms) behind the input, in every
///   mode, so a mode change never shifts the signal in time.
/// - [`Mode::Off`] passes the input through bit for bit (non-finite samples become 0) and does
///   not run the model, so it costs a copy.
/// - [`Mode::Standard`] is the model's output with two things done to protect the voice:
///   - a floor: 30 dB under its own level, the input always stays in the output, so nothing
///     is taken down further than that;
///   - an onset guard: when a block's level rises 20 dB above the running noise floor after
///     at least 30 ms that did not, the block is not spiky (its peak is at most 3 times its
///     RMS level, which a click is not) and the next block is still that far above the floor,
///     the input passes unchanged for 40 ms from the block that rose. The stage's own delay
///     is the look-ahead, so this adds no latency. [`StageStats::guard_opens`] counts it.
/// - A mode change is taken at a block boundary and cross-faded over one block. Turning
///   removal on spends one more block first, feeding the model without using its output.
/// - [`Self::process`] does not allocate, lock or panic. Non-finite input is read as silence;
///   the output is always finite, and in Standard always within `[-1, 1]`.
///
/// The mode is atomic: [`Self::control`] gives a handle that any thread can use while the
/// audio thread runs the stage.
///
/// Threads: build the stage on the thread that will run it, or call [`Self::reset`] (or
/// [`warm_up_thread`]) there before the first block. See [`warm_up_thread`].
///
/// ```
/// use sew_noise::{FRAME, Mode, Stage};
///
/// let mut stage = Stage::new();
/// let control = stage.control();
/// let mut block = [0.0f32; FRAME];
/// stage.process(&mut block);
/// assert_eq!(stage.latency_samples(), 480);
/// control.set_mode(Mode::Off).unwrap();
/// assert!(control.set_mode(Mode::Strong).is_err());
/// ```
pub struct Stage {
    den: Denoiser,
    /// The previous input block (non-finite samples zeroed): the dry output.
    delay: [f32; FRAME],
    /// The model's output for the previous input block.
    wet: [f32; FRAME],
    run: Run,
    shared: Arc<Shared>,
    window_blocks: u32,
    window_over: u32,
    window_max_ns: u64,
    /// Standard's floor and onset guard.
    standard: Standard,
}

impl std::fmt::Debug for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stage")
            .field("mode", &self.mode())
            .field("run", &self.run)
            .finish_non_exhaustive()
    }
}

impl Default for Stage {
    fn default() -> Self {
        Self::new()
    }
}

impl Stage {
    /// The output lags the input by this many samples (one block, 10 ms), in every mode.
    pub const LATENCY: usize = FRAME;

    /// A stage in [`Mode::Standard`]. Allocates here and never again.
    pub fn new() -> Self {
        Self {
            den: Denoiser::new(),
            delay: [0.0; FRAME],
            wet: [0.0; FRAME],
            run: Run::Dry,
            shared: Arc::new(Shared {
                mode: AtomicU8::new(Mode::Standard as u8),
                active: AtomicU8::new(Mode::Off as u8),
                vad_bits: AtomicU32::new(0.0f32.to_bits()),
                budget_ns: AtomicU64::new(DEFAULT_BUDGET.as_nanos() as u64),
                blocks: AtomicU64::new(0),
                over_budget: AtomicU64::new(0),
                max_block_ns: AtomicU64::new(0),
                last_block_ns: AtomicU64::new(0),
                windows: AtomicU64::new(0),
                window_over_budget: AtomicU32::new(0),
                window_max_ns: AtomicU64::new(0),
                cold_thread_blocks: AtomicU64::new(0),
                guard_opens: AtomicU64::new(0),
            }),
            window_blocks: 0,
            window_over: 0,
            window_max_ns: 0,
            standard: Standard::new(0.0),
        }
    }

    /// A stage that starts in `mode`.
    pub fn with_mode(mode: Mode) -> Result<Self, ModeUnavailable> {
        let stage = Self::new();
        stage.set_mode(mode)?;
        Ok(stage)
    }

    /// A handle for other threads.
    pub fn control(&self) -> StageControl {
        StageControl {
            shared: Arc::clone(&self.shared),
        }
    }

    /// [`StageControl::set_mode`].
    pub fn set_mode(&self, mode: Mode) -> Result<(), ModeUnavailable> {
        self.control_ref().set_mode(mode)
    }

    /// [`StageControl::mode`].
    pub fn mode(&self) -> Mode {
        Mode::from_u8(self.shared.mode.load(Ordering::Relaxed))
    }

    /// [`StageControl::active`].
    pub fn active(&self) -> Mode {
        Mode::from_u8(self.shared.active.load(Ordering::Relaxed))
    }

    /// [`StageControl::stats`].
    pub fn stats(&self) -> StageStats {
        self.control().stats()
    }

    /// The time one block may take before it counts as over budget (6 ms by default).
    pub fn set_block_budget(&self, budget: Duration) {
        let ns = u64::try_from(budget.as_nanos()).unwrap_or(u64::MAX);
        self.shared.budget_ns.store(ns, Ordering::Relaxed);
    }

    /// [`Self::LATENCY`]: fixed for the stage's life.
    pub const fn latency_samples(&self) -> usize {
        Self::LATENCY
    }

    /// Forgets the audio held back (after a device change or a long pause): the next block's
    /// output is silence, and when removal is on, the block after fades the model back in.
    /// The model keeps its adaptive state and settles on the new signal within a few blocks.
    /// The counters are kept. Also warms up the calling thread ([`warm_up_thread`]), which
    /// allocates the first time a thread does it and never after.
    pub fn reset(&mut self) {
        warm_up_thread();
        self.delay = [0.0; FRAME];
        self.wet = [0.0; FRAME];
        self.run = Run::Dry;
        self.standard = Standard::new(0.0);
        self.shared.active.store(Mode::Off as u8, Ordering::Relaxed);
    }

    /// Processes one block in place: on return `block` holds the block that went in one call
    /// earlier, with the noise removed when the mode says so.
    pub fn process(&mut self, block: &mut [f32; FRAME]) {
        #[cfg(not(target_arch = "wasm32"))]
        let started = std::time::Instant::now();

        if !WARM.get() {
            // The model's first frame on this thread makes (allocates) its FFT plans.
            WARM.set(true);
            self.shared
                .cold_thread_blocks
                .fetch_add(1, Ordering::Relaxed);
        }
        for s in block.iter_mut() {
            if !s.is_finite() {
                *s = 0.0;
            }
        }
        let want_wet = self.mode() != Mode::Off;
        let mut active = Mode::Off;
        match (self.run, want_wet) {
            (Run::Dry, false) => std::mem::swap(block, &mut self.delay),
            (Run::Dry, true) => {
                self.model(block);
                std::mem::swap(block, &mut self.delay);
                self.run = Run::Primed;
            }
            (Run::Primed, false) => {
                std::mem::swap(block, &mut self.delay);
                self.run = Run::Dry;
            }
            (Run::Primed, true) => {
                self.model(block);
                // Fade from the delayed input to the model's output.
                fade(block, &mut self.delay, &self.wet, false);
                self.run = Run::Wet;
                active = Mode::Standard;
            }
            (Run::Wet, true) => {
                self.model(block);
                let step = self.standard.step(block);
                if step.opened {
                    self.shared.guard_opens.fetch_add(1, Ordering::Relaxed);
                }
                for (i, ((b, d), &w)) in block
                    .iter_mut()
                    .zip(self.delay.iter_mut())
                    .zip(&self.wet)
                    .enumerate()
                {
                    let out = step.mixed(i, w, *d);
                    *d = *b;
                    *b = out;
                }
                active = Mode::Standard;
            }
            (Run::Wet, false) => {
                self.model(block);
                // Fade from the model's output to the delayed input.
                fade(block, &mut self.delay, &self.wet, true);
                self.run = Run::Dry;
                self.standard = Standard::new(0.0);
            }
        }
        self.shared.active.store(active as u8, Ordering::Relaxed);

        #[cfg(not(target_arch = "wasm32"))]
        let ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        #[cfg(target_arch = "wasm32")]
        let ns = 0u64;
        self.count(ns);
    }

    /// Runs the model on `input`; its output (for the block before) lands in `self.wet`.
    fn model(&mut self, input: &[f32; FRAME]) {
        // The lengths are both FRAME, so the call cannot be refused.
        let vad = self
            .den
            .process_frame(&input[..], &mut self.wet[..])
            .unwrap_or(0.0);
        self.shared.vad_bits.store(vad.to_bits(), Ordering::Relaxed);
    }

    fn count(&mut self, ns: u64) {
        let s = &self.shared;
        let over = ns > s.budget_ns.load(Ordering::Relaxed);
        s.blocks.fetch_add(1, Ordering::Relaxed);
        s.last_block_ns.store(ns, Ordering::Relaxed);
        s.max_block_ns.fetch_max(ns, Ordering::Relaxed);
        if over {
            s.over_budget.fetch_add(1, Ordering::Relaxed);
            self.window_over += 1;
        }
        self.window_max_ns = self.window_max_ns.max(ns);
        self.window_blocks += 1;
        if self.window_blocks >= WINDOW_BLOCKS {
            s.window_over_budget
                .store(self.window_over, Ordering::Relaxed);
            s.window_max_ns.store(self.window_max_ns, Ordering::Relaxed);
            s.windows.fetch_add(1, Ordering::Relaxed);
            self.window_blocks = 0;
            self.window_over = 0;
            self.window_max_ns = 0;
        }
    }

    fn control_ref(&self) -> StageControl {
        self.control()
    }
}

/// One block of cross-fade. `block` holds the new input and `delay` the previous one; on
/// return `block` is the output and `delay` the new input. The output moves from the model's
/// output to the delayed input when `to_dry`, the other way otherwise; both are the same block
/// of audio at the same delay, so the fade is between two versions of one signal.
fn fade(block: &mut [f32; FRAME], delay: &mut [f32; FRAME], wet: &[f32; FRAME], to_dry: bool) {
    for (i, ((b, d), &w)) in block.iter_mut().zip(delay.iter_mut()).zip(wet).enumerate() {
        // 0 just after the block's start, 1 at its last sample.
        let t = (i + 1) as f32 / FRAME as f32;
        let (dry_gain, wet_gain) = if to_dry { (t, 1.0 - t) } else { (1.0 - t, t) };
        let out = *d * dry_gain + w * wet_gain;
        *d = *b;
        *b = out;
    }
}
