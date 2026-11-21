//! [`Stage`]: the native capture stage. One 480-sample block of 48 kHz mono in place, a fixed
//! latency, a mode that changes at any block without a click, and per-block timing counters.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use nnnoiseless::DenoiseState;

use crate::standard::Standard;
use crate::strong::{StepDownReason, StrongState};
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
///
/// More modes may come in a later version (a match needs a wildcard arm).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum Mode {
    /// None: the block passes through unchanged, at the same latency.
    Off = 0,
    /// RNNoise.
    Standard = 1,
    /// DeepFilterNet3's low-latency model, with Standard kept warm beside it and stepped down
    /// to when the device can't keep up (cargo feature `dfn3`; a stage built with the model,
    /// `Stage::with_dfn3`). A stage without the model refuses it ([`ModeUnavailable`]).
    Strong = 2,
}

impl Mode {
    /// Every mode, weakest first.
    pub const ALL: [Self; 3] = [Self::Off, Self::Standard, Self::Strong];

    /// Whether this build can run the mode: Strong needs cargo feature `dfn3` (and then a
    /// stage built with the model).
    pub const fn is_available(self) -> bool {
        !matches!(self, Self::Strong) || cfg!(feature = "dfn3")
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
/// One block, in nanoseconds.
const BLOCK_NS: u128 = 10_000_000;
/// The blocks the 99th percentiles in [`StageStats`] are over (1 s): the audio pipeline's own
/// window over the stage's calls.
const RECENT: usize = 100;

/// A block's times, in nanoseconds.
#[derive(Debug, Clone, Copy)]
struct BlockTimes {
    /// The wall time (or the time a caller gave): the counters' time.
    wall: u64,
    /// The thread's CPU time, where it was measured.
    cpu: Option<u64>,
    /// What Strong's rules judge.
    #[cfg_attr(not(feature = "dfn3"), allow(dead_code))]
    judged: u64,
}

/// Where a block's timing started.
#[derive(Debug, Clone, Copy)]
struct Mark {
    #[cfg(not(target_arch = "wasm32"))]
    started: std::time::Instant,
    given: Option<u64>,
    cpu: Option<u64>,
}

/// The grace window a stage opens by itself ([`StageControl::set_auto_grace`]): at its first
/// block, at [`Stage::reset`] and when it takes a model layer handed over during a call.
///
/// Three seconds. A phone call's start-up (the audio session activating, the route settling, a
/// camera opening) loads the device for about a second, each event after the one before, and
/// an app switch does the same; the stage's own step-downs came within a second of the last
/// such event. Three seconds from the latest event covers that with room, and each event
/// reported through [`StageControl::grace`] opens the window again from its own time.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(3);

/// Blocks in `d`, rounded up.
fn blocks_in(d: Duration) -> u64 {
    u64::try_from(d.as_nanos().div_ceil(BLOCK_NS)).unwrap_or(u64::MAX)
}

/// What the stage and its [`StageControl`]s share: atomics, and (feature `dfn3`) the slot a
/// model handed over later waits in, which the stage only ever try-locks.
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
    /// The stage was built with a model for Strong.
    strong_available: AtomicBool,
    strong_state: AtomicU8,
    model_blocks: AtomicU64,
    step_downs: AtomicU64,
    last_step_down: AtomicU8,
    last_step_down_block: AtomicU64,
    retry_at_block: AtomicU64,
    grace_step_downs: AtomicU64,
    retry_held: AtomicBool,
    max_block_cpu_ns: AtomicU64,
    last_block_cpu_ns: AtomicU64,
    p99_block_ns: AtomicU64,
    p99_block_cpu_ns: AtomicU64,
    /// The blocks are timed by the thread's CPU time ([`StageStats::cpu_timed`]).
    cpu_timed: AtomicBool,
    /// Measurement: the step-down rules apply (on by default).
    step_down_rules: AtomicBool,
    /// Measurement: step down at the next model block.
    force_step_down: AtomicBool,
    /// The grace window runs while the block count is under this (0: none was opened).
    grace_until: AtomicU64,
    /// The window the stage opens by itself, in blocks.
    auto_grace_blocks: AtomicU64,
    /// Grace windows opened (or extended).
    grace_opens: AtomicU64,
    /// A model layer handed over by [`StageControl::attach_dfn3`], until the stage takes it.
    #[cfg(feature = "dfn3")]
    handover: std::sync::Mutex<Option<Dfn3Layer>>,
    /// `handover` holds a layer.
    #[cfg(feature = "dfn3")]
    handover_ready: AtomicBool,
}

/// DeepFilterNet3's layer for a running stage (cargo feature `dfn3`): built and warmed up off
/// the audio thread ([`Dfn3Layer::new`]), then handed to a stage built without the model with
/// [`StageControl::attach_dfn3`].
#[cfg(feature = "dfn3")]
#[derive(Debug)]
pub struct Dfn3Layer(Box<crate::strong::Strong>);

#[cfg(feature = "dfn3")]
impl Dfn3Layer {
    /// The layer over `model`, warmed up as [`Stage::with_dfn3`] warms it. Slow (a few
    /// milliseconds) and allocates: call it off the audio thread.
    pub fn new(model: Arc<crate::dfn3::Dfn3Model>) -> Self {
        Self(Box::new(crate::strong::Strong::new(model)))
    }

    /// The layer over `model` with its warm-up still to run, for a caller that has to spread
    /// the work over time (an audio thread with no other thread to build on): call
    /// [`Self::warm_up_step`] until it answers true. Allocates; the steps do not. A layer handed
    /// to [`StageControl::attach_dfn3`] before its warm-up is complete runs the rest there.
    pub fn cold(model: Arc<crate::dfn3::Dfn3Model>) -> Self {
        Self(Box::new(crate::strong::Strong::cold(model)))
    }

    /// Runs one step of the warm-up (a warm-up block's start, one part of the model's work, or
    /// its end: bounded work); true once the warm-up is complete, and on every call after.
    pub fn warm_up_step(&mut self) -> bool {
        self.0.warm_up_step()
    }

    /// Warm-up blocks still to run (including one begun).
    pub fn warm_up_blocks_left(&self) -> u32 {
        self.0.warm_left
    }

    /// Runs `blocks` more warm-up blocks before the layer is used (after the warm-up is
    /// complete too): for a caller that waits until the model's code runs at its usual speed
    /// (a WebAssembly engine compiles hot code again, faster, some time after it first runs).
    /// The layer is the same afterwards: its stream is cleared when the warm-up ends.
    pub fn extend_warm_up(&mut self, blocks: u32) {
        self.0.warm_left = self.0.warm_left.saturating_add(blocks);
    }
}

/// A snapshot of a stage's timing counters ([`StageControl::stats`]).
///
/// Times are the wall time of [`Stage::process`], measured on the thread that runs it, except
/// the `*_cpu_ns` fields: the thread's CPU time for the block where the target has a clock for
/// it ([`Self::cpu_timed`]), which is what Strong's step-down rules judge. On `wasm32` there is
/// no clock unless one is given ([`Stage::set_clock`]): without it the block counts advance
/// and every time is 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
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
    /// Where Strong is ([`StrongState::Idle`] for a stage without the model).
    pub strong_state: StrongState,
    /// Blocks in which Strong's model ran.
    pub model_blocks: u64,
    /// Step-downs from Strong to Standard so far.
    pub step_downs: u64,
    /// Why the latest step-down happened, if there was one.
    pub last_step_down: Option<StepDownReason>,
    /// The stage's block count ([`Self::blocks`]) at the latest step-down.
    pub last_step_down_block: u64,
    /// The block count from which a stepped-down Strong may be retried.
    pub retry_at_block: u64,
    /// A grace window is open now ([`StageControl::grace`]).
    pub in_grace: bool,
    /// The block count at which the latest grace window closes (or closed); 0 if none was
    /// opened.
    pub grace_until_block: u64,
    /// Grace windows opened or extended so far, the stage's own included.
    pub grace_opens: u64,
    /// Step-downs tied to a grace window or to a reset of the stage (counted in
    /// [`Self::step_downs`] too): they do not count towards the three that retire Strong; a
    /// separate, larger bound applies to them.
    pub grace_step_downs: u64,
    /// A retry of Strong is due but held back: a block of the last 100 went over one of the
    /// audio pipeline's own lines (6 ms on the clock the rules judge, or 20 ms by the wall
    /// clock).
    pub retry_held: bool,
    /// The blocks are timed by the thread's CPU time too (the target has a clock for it and
    /// no clock was given with [`Stage::set_clock`]): the `*_cpu_ns` fields are set and
    /// Strong's rules judge them. False: the rules judge the wall time.
    pub cpu_timed: bool,
    /// The slowest block by the thread's CPU time, in nanoseconds (0 without
    /// [`Self::cpu_timed`]).
    pub max_block_cpu_ns: u64,
    /// The latest block by the thread's CPU time, in nanoseconds (0 without
    /// [`Self::cpu_timed`]).
    pub last_block_cpu_ns: u64,
    /// The 99th percentile (nearest rank) of the last 100 blocks' wall times, or of all
    /// blocks if fewer, in nanoseconds: what the audio pipeline's own percentile rule sees.
    pub p99_block_ns: u64,
    /// The same over the blocks' CPU times (0 without [`Self::cpu_timed`]).
    pub p99_block_cpu_ns: u64,
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
        if !mode.is_available()
            || (mode == Mode::Strong && !self.shared.strong_available.load(Ordering::Relaxed))
        {
            return Err(ModeUnavailable(mode));
        }
        self.shared.mode.store(mode as u8, Ordering::Relaxed);
        Ok(())
    }

    /// Whether this stage can run [`Mode::Strong`] (it was built with the model, or was given
    /// it with `StageControl::attach_dfn3`, feature `dfn3`).
    pub fn strong_available(&self) -> bool {
        self.shared.strong_available.load(Ordering::Relaxed)
    }

    /// Gives a running stage built without the model its DeepFilterNet3 layer (cargo feature
    /// `dfn3`). Quick: the stage takes the layer at its next block, without allocating or
    /// waiting (it only try-locks the slot the layer waits in), and from then on
    /// [`Mode::Strong`] is available. Strong then starts as a stage built with the model
    /// starts it: the model runs one block unused, then fades in over one block, with no
    /// click. Answers the layer back when the stage already has a model or one is waiting.
    #[cfg(feature = "dfn3")]
    pub fn attach_dfn3(&self, mut layer: Dfn3Layer) -> Result<(), Dfn3Layer> {
        if self.shared.strong_available.load(Ordering::Acquire) {
            return Err(layer);
        }
        layer.0.finish_warm_up();
        let mut slot = self
            .shared
            .handover
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return Err(layer);
        }
        *slot = Some(layer);
        drop(slot);
        self.shared.handover_ready.store(true, Ordering::Release);
        self.shared.strong_available.store(true, Ordering::Release);
        Ok(())
    }

    /// Steps down from Strong at the next block the model runs in, as a slow block would
    /// (counted, with its retry). For measuring the switch; an app sets [`Mode::Standard`].
    #[doc(hidden)]
    pub fn force_step_down(&self) {
        self.shared.force_step_down.store(true, Ordering::Relaxed);
    }

    /// Opens a grace window of `duration` from now, or extends the open one to end then (a
    /// window never shortens here). For a moment the device is busy with something else: a
    /// call starting, the audio session activating, the audio route changing, a camera
    /// starting, the app moving between foreground and background.
    ///
    /// In a grace window Strong's timing rules do not step the model down for one slow block,
    /// for its 99th percentile or for its median. It steps down only at the second block over
    /// 6 ms among the last 100 blocks ([`StepDownReason::SlowWindow`]), the count at which
    /// the audio pipeline's own percentile rule would bypass the whole stage, or when the
    /// thread falls behind real time for several blocks in a row ([`StepDownReason::Starved`]).
    /// A model output that is not finite is still a fault, final at once. The window's block
    /// times are forgotten when it closes, so the usual rules start afresh. A step-down inside
    /// a window, or within 5 s after it closed, is retried after 5 s instead of the usual
    /// wait; one inside a window does not count towards the step-downs that retire Strong for
    /// the stage's life (a separate, larger bound applies to those).
    ///
    /// Counted in the stage's blocks (10 ms each). A few atomic operations: any thread.
    pub fn grace(&self, duration: Duration) {
        let n = blocks_in(duration);
        if n == 0 {
            return;
        }
        let s = &self.shared;
        let until = s.blocks.load(Ordering::Relaxed).saturating_add(n);
        s.grace_until.fetch_max(until, Ordering::Relaxed);
        s.grace_opens.fetch_add(1, Ordering::Relaxed);
    }

    /// Closes the grace window now, if one is open (its 5 s tail for a short retry starts
    /// now).
    pub fn end_grace(&self) {
        let s = &self.shared;
        let now = s.blocks.load(Ordering::Relaxed);
        s.grace_until.fetch_min(now, Ordering::Relaxed);
    }

    /// Whether a grace window is open now.
    pub fn in_grace(&self) -> bool {
        let s = &self.shared;
        s.blocks.load(Ordering::Relaxed) < s.grace_until.load(Ordering::Relaxed)
    }

    /// The grace window the stage opens by itself: at its first block, at [`Stage::reset`] and
    /// when it takes a model layer handed over during a call ([`DEFAULT_GRACE`] unless set).
    /// `Duration::ZERO` turns it off; a window already open is kept.
    pub fn set_auto_grace(&self, duration: Duration) {
        self.shared
            .auto_grace_blocks
            .store(blocks_in(duration), Ordering::Relaxed);
    }

    /// [`Self::set_auto_grace`]'s window.
    pub fn auto_grace(&self) -> Duration {
        let blocks = self.shared.auto_grace_blocks.load(Ordering::Relaxed);
        Duration::from_millis(blocks.saturating_mul(10))
    }

    /// Turns the step-down rules off (`false`) or back on. For measuring the model on a
    /// shared, busy machine, where a scheduling delay is not the device being too slow; never
    /// in a call.
    #[doc(hidden)]
    pub fn set_step_down_rules(&self, on: bool) {
        self.shared.step_down_rules.store(on, Ordering::Relaxed);
    }

    /// Whether the step-down rules are on ([`Self::set_step_down_rules`]).
    #[cfg(any(test, feature = "probe"))]
    #[doc(hidden)]
    pub fn step_down_rules(&self) -> bool {
        self.shared.step_down_rules.load(Ordering::Relaxed)
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
            strong_state: StrongState::from_code(s.strong_state.load(Ordering::Relaxed)),
            model_blocks: s.model_blocks.load(Ordering::Relaxed),
            step_downs: s.step_downs.load(Ordering::Relaxed),
            last_step_down: StepDownReason::from_code(s.last_step_down.load(Ordering::Relaxed)),
            last_step_down_block: s.last_step_down_block.load(Ordering::Relaxed),
            retry_at_block: s.retry_at_block.load(Ordering::Relaxed),
            in_grace: self.in_grace(),
            grace_until_block: s.grace_until.load(Ordering::Relaxed),
            grace_opens: s.grace_opens.load(Ordering::Relaxed),
            grace_step_downs: s.grace_step_downs.load(Ordering::Relaxed),
            retry_held: s.retry_held.load(Ordering::Relaxed),
            cpu_timed: s.cpu_timed.load(Ordering::Relaxed),
            max_block_cpu_ns: s.max_block_cpu_ns.load(Ordering::Relaxed),
            last_block_cpu_ns: s.last_block_cpu_ns.load(Ordering::Relaxed),
            p99_block_ns: s.p99_block_ns.load(Ordering::Relaxed),
            p99_block_cpu_ns: s.p99_block_cpu_ns.load(Ordering::Relaxed),
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
/// - [`Mode::Strong`] (cargo feature `dfn3`, a stage built by `Stage::with_dfn3`) is
///   DeepFilterNet3's low-latency model's output, with Standard running beside it. The stage
///   times its own blocks (by the thread's CPU time where the target has a clock for it, so
///   time spent waiting for a core is not counted against the model) and steps down to
///   Standard when the model is too slow for the device, without a click and without running the model again; it retries later,
///   and after repeated step-downs it stays on Standard. [`StageStats`] says where Strong is
///   and why it stepped down. While the device is busy with something else (the stage's
///   start, a reset, a model handed over, or a moment an app reports with
///   [`StageControl::grace`]) a grace window holds the timing rules back to the audio
///   pipeline's own line.
/// - A mode change is taken at a block boundary and cross-faded over one block. Turning
///   removal on spends one more block first, feeding the model without using its output.
/// - [`Self::process`] does not allocate, block or panic (a model layer handed over by
///   `StageControl::attach_dfn3` is taken with a try-lock that never waits). Non-finite input
///   is read as silence; the output is always finite, and in Standard and Strong always within
///   `[-1, 1]`.
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
    /// Strong's model and step-down.
    #[cfg(feature = "dfn3")]
    strong: Option<Box<crate::strong::Strong>>,
    /// A clock given by [`Self::set_clock`], used in place of the standard library's.
    clock: Option<fn() -> u64>,
    /// The thread's CPU time, in nanoseconds (`None`: no such clock on this target).
    cpu_clock: fn() -> Option<u64>,
    /// A clock given by [`Self::set_clock`] is what the rules judge (tests turn this off to
    /// give a CPU clock beside a wall clock).
    judge_given: bool,
    /// CPU time spent in the begun block's [`Self::begin_block`] and [`Self::step_block`]
    /// calls so far (`None`: not timed).
    pending_cpu: Option<u64>,
    /// The last [`RECENT`] blocks' wall and CPU times, for the 99th percentiles.
    recent_wall: [u64; RECENT],
    recent_cpu: [u64; RECENT],
    recent_pos: usize,
    recent_len: usize,
    /// The block begun by [`Self::begin_block`]: Standard's output for it so far, what it was,
    /// and whether one is begun.
    pending: [f32; FRAME],
    pending_active: Mode,
    pending_begun: bool,
    /// The first block has begun (the stage's own grace window is open).
    started: bool,
    /// What the last block ran (the `probe` module's `RAN_*` bits).
    #[cfg(feature = "probe")]
    probe_flags: u8,
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
                strong_available: AtomicBool::new(false),
                strong_state: AtomicU8::new(StrongState::Idle.code()),
                model_blocks: AtomicU64::new(0),
                step_downs: AtomicU64::new(0),
                last_step_down: AtomicU8::new(0),
                last_step_down_block: AtomicU64::new(0),
                retry_at_block: AtomicU64::new(0),
                grace_step_downs: AtomicU64::new(0),
                retry_held: AtomicBool::new(false),
                max_block_cpu_ns: AtomicU64::new(0),
                last_block_cpu_ns: AtomicU64::new(0),
                p99_block_ns: AtomicU64::new(0),
                p99_block_cpu_ns: AtomicU64::new(0),
                cpu_timed: AtomicBool::new(crate::thread_clock::AVAILABLE),
                step_down_rules: AtomicBool::new(true),
                force_step_down: AtomicBool::new(false),
                grace_until: AtomicU64::new(0),
                auto_grace_blocks: AtomicU64::new(blocks_in(DEFAULT_GRACE)),
                grace_opens: AtomicU64::new(0),
                #[cfg(feature = "dfn3")]
                handover: std::sync::Mutex::new(None),
                #[cfg(feature = "dfn3")]
                handover_ready: AtomicBool::new(false),
            }),
            window_blocks: 0,
            window_over: 0,
            window_max_ns: 0,
            standard: Standard::new(0.0),
            #[cfg(feature = "dfn3")]
            strong: None,
            clock: None,
            cpu_clock: crate::thread_clock::thread_cpu_ns,
            judge_given: true,
            pending_cpu: None,
            recent_wall: [0; RECENT],
            recent_cpu: [0; RECENT],
            recent_pos: 0,
            recent_len: 0,
            pending: [0.0; FRAME],
            pending_active: Mode::Off,
            pending_begun: false,
            started: false,
            #[cfg(feature = "probe")]
            probe_flags: 0,
        }
    }

    /// A stage in [`Mode::Strong`] over `model` (cargo feature `dfn3`). Builds the model's
    /// stream and warms it up here: every code path runs once on a test signal, then the state
    /// is cleared. Allocates here and never again. Slower than [`Self::new`] (a few
    /// milliseconds); build it off the audio thread.
    #[cfg(feature = "dfn3")]
    pub fn with_dfn3(model: Arc<crate::dfn3::Dfn3Model>) -> Self {
        let mut stage = Self::new();
        stage.strong = Some(Box::new(crate::strong::Strong::new(model)));
        stage.shared.strong_available.store(true, Ordering::Relaxed);
        stage
            .shared
            .mode
            .store(Mode::Strong as u8, Ordering::Relaxed);
        stage
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

    /// Times the blocks with `clock` (monotonic nanoseconds) in place of the standard library's
    /// clock. For targets that have none, such as `wasm32-unknown-unknown`, where a stage
    /// otherwise counts every block as taking no time and so never steps down from Strong.
    /// The clock is called twice a block, on the audio thread: it must not allocate or block.
    /// Strong's rules then judge this clock's times, in place of the thread's CPU time.
    pub fn set_clock(&mut self, clock: fn() -> u64) {
        self.clock = Some(clock);
        if self.judge_given {
            self.shared.cpu_timed.store(false, Ordering::Relaxed);
        }
    }

    /// Starts timing a block.
    fn mark(&self) -> Mark {
        Mark {
            #[cfg(not(target_arch = "wasm32"))]
            started: std::time::Instant::now(),
            given: self.clock.map(|c| c()),
            cpu: (self.cpu_clock)(),
        }
    }

    /// The CPU time since `mark` (`None` without a CPU clock).
    fn cpu_since(&self, mark: &Mark) -> Option<u64> {
        let t0 = mark.cpu?;
        Some((self.cpu_clock)()?.saturating_sub(t0))
    }

    /// The block's times from `mark` to now; `cpu_before`: CPU time the block spent before
    /// `mark` (its begin and steps); `block_ns`: the time a caller counts for it.
    fn times(&self, mark: &Mark, cpu_before: Option<u64>, block_ns: Option<u64>) -> BlockTimes {
        #[cfg(not(target_arch = "wasm32"))]
        let std_ns = u64::try_from(mark.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        #[cfg(target_arch = "wasm32")]
        let std_ns = 0u64;
        let given = match (self.clock, mark.given) {
            (Some(c), Some(t0)) => Some(c().saturating_sub(t0)),
            _ => None,
        };
        let wall = block_ns.or(given).unwrap_or(std_ns);
        let cpu = self
            .cpu_since(mark)
            .map(|c| c.saturating_add(cpu_before.unwrap_or(0)));
        let judged = match (block_ns, given, cpu) {
            (Some(ns), _, _) => ns,
            (None, Some(g), _) if self.judge_given => g,
            (None, _, Some(c)) => c,
            _ => wall,
        };
        BlockTimes { wall, cpu, judged }
    }

    /// [`Self::LATENCY`]: fixed for the stage's life.
    pub const fn latency_samples(&self) -> usize {
        Self::LATENCY
    }

    /// Forgets the audio held back (after a device change or a long pause): the next block's
    /// output is silence, and when removal is on, the block after fades the model back in.
    /// The model keeps its adaptive state and settles on the new signal within a few blocks.
    /// The counters are kept, and the stage's own grace window opens
    /// ([`StageControl::set_auto_grace`]). Also warms up the calling thread
    /// ([`warm_up_thread`]), which allocates the first time a thread does it and never after.
    pub fn reset(&mut self) {
        warm_up_thread();
        self.open_auto_grace();
        self.started = true;
        self.delay = [0.0; FRAME];
        self.wet = [0.0; FRAME];
        self.run = Run::Dry;
        self.standard = Standard::new(0.0);
        self.pending_begun = false;
        self.pending_cpu = None;
        #[cfg(feature = "dfn3")]
        if let Some(strong) = self.strong.as_mut() {
            strong.reset(self.shared.blocks.load(Ordering::Relaxed));
        }
        self.shared.active.store(Mode::Off as u8, Ordering::Relaxed);
    }

    /// Processes one block in place: on return `block` holds the block that went in one call
    /// earlier, with the noise removed when the mode says so.
    pub fn process(&mut self, block: &mut [f32; FRAME]) {
        let mark = self.mark();
        self.begin_inner(block);
        let ran = self.end_inner(block);
        let t = self.times(&mark, None, None);
        self.after_block(ran, t);
    }

    /// Starts a block, for a caller that spreads the model's work over time (an audio thread
    /// that runs in short slices, such as a Web Audio worklet): [`Self::begin_block`], then
    /// [`Self::step_block`] at most [`Self::block_steps_left`] times, spread as the caller likes,
    /// then [`Self::end_block`] for the block's output. The output is the one [`Self::process`]
    /// gives, bit for bit, however the steps are spread. `begin_block` does Standard's work and
    /// the start of Strong's (the cheap parts); each step is one part of Strong's model;
    /// `end_block` runs any part left and finishes. A block begun and not ended is dropped by
    /// the next `begin_block` (its model work is lost) or by [`Self::reset`].
    pub fn begin_block(&mut self, input: &[f32; FRAME]) {
        let cpu0 = (self.cpu_clock)();
        self.begin_inner(input);
        self.pending_cpu = cpu0.and_then(|t0| Some((self.cpu_clock)()?.saturating_sub(t0)));
    }

    /// [`Self::begin_block`]'s work, untimed.
    fn begin_inner(&mut self, input: &[f32; FRAME]) {
        if !WARM.get() {
            // The model's first frame on this thread makes (allocates) its FFT plans.
            WARM.set(true);
            self.shared
                .cold_thread_blocks
                .fetch_add(1, Ordering::Relaxed);
        }
        if !self.started {
            self.started = true;
            self.open_auto_grace();
        }
        let mut block = *input;
        for s in block.iter_mut() {
            if !s.is_finite() {
                *s = 0.0;
            }
        }
        #[cfg(feature = "dfn3")]
        self.take_handover();
        #[cfg(feature = "dfn3")]
        let input = block;
        let want_wet = self.mode() != Mode::Off;
        let mut active = Mode::Off;
        match (self.run, want_wet) {
            (Run::Dry, false) => std::mem::swap(&mut block, &mut self.delay),
            (Run::Dry, true) => {
                self.model(&block);
                std::mem::swap(&mut block, &mut self.delay);
                self.run = Run::Primed;
            }
            (Run::Primed, false) => {
                std::mem::swap(&mut block, &mut self.delay);
                self.run = Run::Dry;
            }
            (Run::Primed, true) => {
                self.model(&block);
                // Fade from the delayed input to the model's output.
                fade(&mut block, &mut self.delay, &self.wet, false);
                self.run = Run::Wet;
                active = Mode::Standard;
            }
            (Run::Wet, true) => {
                self.model(&block);
                let step = self.standard.step(&block);
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
                self.model(&block);
                // Fade from the model's output to the delayed input.
                fade(&mut block, &mut self.delay, &self.wet, true);
                self.run = Run::Dry;
                self.standard = Standard::new(0.0);
            }
        }
        #[cfg(feature = "dfn3")]
        if let Some(strong) = self.strong.as_mut() {
            let block_no = self.shared.blocks.load(Ordering::Relaxed);
            let want = self.shared.mode.load(Ordering::Relaxed) == Mode::Strong as u8;
            strong.begin(&input, want, block_no);
        }
        self.pending = block;
        self.pending_active = active;
        self.pending_begun = true;
    }

    /// Whether a block is begun ([`Self::begin_block`]) and not yet ended.
    pub fn block_begun(&self) -> bool {
        self.pending_begun
    }

    /// Parts of the model's work left in the block begun by [`Self::begin_block`] (0 without
    /// Strong, or once they are done). An upper bound: which parts run depends on the first.
    pub fn block_steps_left(&self) -> u32 {
        #[cfg(feature = "dfn3")]
        if self.pending_begun
            && let Some(strong) = self.strong.as_ref()
        {
            return strong.steps_left();
        }
        0
    }

    /// Runs one part of the model's work in the begun block (bounded: half of one of the
    /// model's recurrent layers and what goes with it). True once none is left. Allocates nothing.
    pub fn step_block(&mut self) -> bool {
        #[cfg(feature = "dfn3")]
        if self.pending_begun
            && let Some(strong) = self.strong.as_mut()
        {
            let cpu0 = (self.cpu_clock)();
            let done = strong.step();
            if let (Some(t0), Some(before)) = (cpu0, self.pending_cpu)
                && let Some(t1) = (self.cpu_clock)()
            {
                self.pending_cpu = Some(before.saturating_add(t1.saturating_sub(t0)));
            }
            return done;
        }
        true
    }

    /// Ends the block begun by [`Self::begin_block`]: `out` gets the block going out (the one
    /// that went in one block earlier), as [`Self::process`] would have left it. `block_ns`
    /// is the time the caller counts for this block (for example the slowest of the slices it
    /// ran the block's work in, scaled to a block); the counters and Strong's step-down use it.
    /// `None`: the time of this call. Without a begun block, `out` gets silence.
    pub fn end_block(&mut self, out: &mut [f32; FRAME], block_ns: Option<u64>) {
        let mark = self.mark();
        if !self.pending_begun {
            *out = [0.0; FRAME];
            return;
        }
        let ran = self.end_inner(out);
        let before = self.pending_cpu.take();
        let mut t = self.times(&mark, before, block_ns);
        if before.is_none() {
            // The begin was not timed by the CPU clock: no CPU time for the block.
            t.cpu = None;
            if block_ns.is_none() && !(self.clock.is_some() && self.judge_given) {
                t.judged = t.wall;
            }
        }
        self.after_block(ran, t);
    }

    /// The end of a begun block, into `out`; answers whether the model ran.
    fn end_inner(&mut self, out: &mut [f32; FRAME]) -> bool {
        self.pending_begun = false;
        #[cfg_attr(not(feature = "dfn3"), allow(unused_mut))]
        let mut active = self.pending_active;
        #[cfg(feature = "dfn3")]
        let mut ran = false;
        #[cfg(not(feature = "dfn3"))]
        let ran = false;
        #[cfg(feature = "dfn3")]
        if let Some(strong) = self.strong.as_mut() {
            let block_no = self.shared.blocks.load(Ordering::Relaxed);
            let o = strong.end(&mut self.pending, block_no);
            ran = o.ran;
            if o.active {
                active = Mode::Strong;
            }
        }
        *out = self.pending;
        self.shared.active.store(active as u8, Ordering::Relaxed);
        #[cfg(feature = "probe")]
        {
            self.probe_flags = self.flags_of(ran, active);
        }
        ran
    }

    /// After a block: Strong's rules on its times, and the counters.
    fn after_block(&mut self, ran: bool, t: BlockTimes) {
        #[cfg(feature = "dfn3")]
        self.judge_strong(ran, t);
        #[cfg(not(feature = "dfn3"))]
        let _ = ran;
        self.count(t);
    }

    /// Takes a layer handed over by [`StageControl::attach_dfn3`], if one waits: a try-lock
    /// that never waits (a busy slot is tried again next block) and a move, no allocation.
    #[cfg(feature = "dfn3")]
    fn take_handover(&mut self) {
        if self.strong.is_some() || !self.shared.handover_ready.load(Ordering::Acquire) {
            return;
        }
        let taken = match self.shared.handover.try_lock() {
            Ok(mut slot) => slot.take(),
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner().take(),
            Err(std::sync::TryLockError::WouldBlock) => None,
        };
        if let Some(layer) = taken {
            self.strong = Some(layer.0);
            self.shared.handover_ready.store(false, Ordering::Release);
            // The model starts mid-call: its first blocks get the start's grace.
            self.open_auto_grace();
        }
    }

    /// Opens (or extends) a grace window of [`StageControl::set_auto_grace`]'s length from
    /// this block. Atomics only.
    fn open_auto_grace(&self) {
        let s = &self.shared;
        let n = s.auto_grace_blocks.load(Ordering::Relaxed);
        if n == 0 {
            return;
        }
        let until = s.blocks.load(Ordering::Relaxed).saturating_add(n);
        s.grace_until.fetch_max(until, Ordering::Relaxed);
        s.grace_opens.fetch_add(1, Ordering::Relaxed);
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

    /// After a block: Strong's step-down rules on the block's time, and its state published.
    #[cfg(feature = "dfn3")]
    fn judge_strong(&mut self, ran: bool, t: BlockTimes) {
        let Some(strong) = self.strong.as_mut() else {
            return;
        };
        let s = &self.shared;
        let block_no = s.blocks.load(Ordering::Relaxed);
        if ran {
            s.model_blocks.fetch_add(1, Ordering::Relaxed);
        }
        // Every block goes to the rules (their window covers the stage's recent blocks).
        let force = ran && s.force_step_down.swap(false, Ordering::Relaxed);
        let rules = s.step_down_rules.load(Ordering::Relaxed);
        let grace_until = s.grace_until.load(Ordering::Relaxed);
        strong.judge(t.judged, t.wall, ran, block_no, rules, force, grace_until);
        s.strong_state
            .store(strong.state(block_no + 1).code(), Ordering::Relaxed);
        s.step_downs
            .store(u64::from(strong.step_downs), Ordering::Relaxed);
        s.last_step_down.store(
            strong.last.map_or(0, StepDownReason::code),
            Ordering::Relaxed,
        );
        s.last_step_down_block
            .store(strong.last_block, Ordering::Relaxed);
        s.retry_at_block.store(strong.retry_at(), Ordering::Relaxed);
        s.grace_step_downs
            .store(u64::from(strong.grace_step_downs), Ordering::Relaxed);
        s.retry_held.store(strong.held, Ordering::Relaxed);
    }

    fn count(&mut self, t: BlockTimes) {
        let ns = t.wall;
        self.recent_wall[self.recent_pos] = ns;
        self.recent_cpu[self.recent_pos] = t.cpu.unwrap_or(0);
        self.recent_pos = (self.recent_pos + 1) % RECENT;
        self.recent_len = (self.recent_len + 1).min(RECENT);
        let s = &self.shared;
        s.p99_block_ns
            .store(p99(&self.recent_wall[..self.recent_len]), Ordering::Relaxed);
        if let Some(cpu) = t.cpu {
            s.last_block_cpu_ns.store(cpu, Ordering::Relaxed);
            s.max_block_cpu_ns.fetch_max(cpu, Ordering::Relaxed);
            s.p99_block_cpu_ns
                .store(p99(&self.recent_cpu[..self.recent_len]), Ordering::Relaxed);
        }
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

    /// What a block ran, as the `probe` module's `RAN_*` bits.
    #[cfg(feature = "probe")]
    fn flags_of(&self, ran: bool, active: Mode) -> u8 {
        use crate::probe::{RAN_MODEL, STRONG_OUT};
        let mut flags = 0;
        if ran {
            flags |= RAN_MODEL;
        }
        #[cfg(feature = "dfn3")]
        if ran && let Some(strong) = self.strong.as_ref() {
            let (erb, df) = strong.decoders_ran();
            if erb {
                flags |= crate::probe::RAN_ERB;
            }
            if df {
                flags |= crate::probe::RAN_DF;
            }
        }
        if active == Mode::Strong {
            flags |= STRONG_OUT;
        }
        flags
    }

    /// What the last block ran.
    #[cfg(feature = "probe")]
    pub(crate) fn probe_flags(&self) -> u8 {
        self.probe_flags
    }

    fn control_ref(&self) -> StageControl {
        self.control()
    }
}

/// The 99th percentile (nearest rank) of `v`: of 100 values the second largest, of fewer the
/// largest. One pass, no allocation.
fn p99(v: &[u64]) -> u64 {
    let (mut first, mut second) = (0u64, 0u64);
    for &x in v {
        if x >= first {
            second = first;
            first = x;
        } else if x > second {
            second = x;
        }
    }
    if v.len() >= RECENT { second } else { first }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The nearest-rank 99th percentile: of 100 values the second largest (ties count), of
    /// fewer the largest.
    #[test]
    fn p99_is_the_nearest_rank() {
        assert_eq!(p99(&[]), 0);
        assert_eq!(p99(&[3, 9, 1]), 9);
        let mut v = [1u64; RECENT];
        v[10] = 9;
        assert_eq!(p99(&v), 1, "one slow block of 100");
        v[50] = 7;
        assert_eq!(p99(&v), 7);
        v[50] = 9;
        assert_eq!(p99(&v), 9, "a tie");
    }

    /// Without the model the stats carry the wall-time percentile and, where the target has
    /// a CPU clock, the CPU-time fields.
    #[test]
    fn cpu_and_wall_stats() {
        let mut stage = Stage::new();
        let mut block = [0.01f32; FRAME];
        for _ in 0..150 {
            stage.process(&mut block);
        }
        let st = stage.stats();
        assert_eq!(st.cpu_timed, crate::thread_clock::AVAILABLE);
        assert!(st.p99_block_ns > 0 || cfg!(target_arch = "wasm32"));
        assert!(st.p99_block_ns <= st.max_block_ns);
        if crate::thread_clock::AVAILABLE {
            assert!(st.max_block_cpu_ns > 0, "{st:?}");
            assert!(st.p99_block_cpu_ns <= st.max_block_cpu_ns);
            assert!(st.last_block_cpu_ns <= st.max_block_cpu_ns);
        } else {
            assert_eq!(st.max_block_cpu_ns, 0);
        }
        // A given clock is what the rules judge: no longer CPU-timed.
        stage.set_clock(|| 0);
        assert!(!stage.stats().cpu_timed);
    }

    #[cfg(feature = "dfn3")]
    mod dfn3 {
        use super::*;

        const MS: u64 = 1_000_000;

        thread_local! {
            static WALL_NOW: Cell<u64> = const { Cell::new(0) };
            static WALL_STEP: Cell<u64> = const { Cell::new(MS) };
            static CPU_NOW: Cell<u64> = const { Cell::new(0) };
            static CPU_STEP: Cell<u64> = const { Cell::new(MS) };
        }

        /// Each reading is the step after the one before: a block (two readings) takes the
        /// step. Per thread, so tests running at once do not share it.
        fn wall() -> u64 {
            let t = WALL_NOW.get();
            WALL_NOW.set(t + WALL_STEP.get());
            t
        }

        fn cpu() -> Option<u64> {
            let t = CPU_NOW.get();
            CPU_NOW.set(t + CPU_STEP.get());
            Some(t)
        }

        /// A stage over the model with test clocks for the wall and the thread's CPU time, the
        /// rules judging the CPU clock, no grace window.
        fn timed_stage() -> Option<Stage> {
            let model = crate::strong::test_model()?;
            let mut stage = Stage::with_dfn3(model);
            stage.control().set_auto_grace(Duration::ZERO);
            stage.judge_given = false;
            stage.set_clock(wall);
            stage.cpu_clock = cpu;
            Some(stage)
        }

        fn run(stage: &mut Stage, blocks: u32, wall_ns: u64, cpu_ns: u64) {
            WALL_STEP.set(wall_ns);
            CPU_STEP.set(cpu_ns);
            let mut block = [0.01f32; FRAME];
            for _ in 0..blocks {
                stage.process(&mut block);
            }
        }

        /// Blocks that take long by the wall clock only because the thread waited for a core
        /// (9 ms of wall time, 1 ms of CPU) keep the model; the stats carry both. Blocks that
        /// take 7 ms of CPU step it down.
        #[test]
        fn a_preempted_block_is_not_a_slow_model() {
            let Some(mut stage) = timed_stage() else {
                return;
            };
            run(&mut stage, 1_000, 9 * MS, MS);
            let st = stage.stats();
            assert_eq!(st.step_downs, 0, "{st:?}");
            assert_eq!(st.strong_state, StrongState::Running);
            assert_eq!(stage.active(), Mode::Strong);
            assert!(st.cpu_timed);
            assert_eq!(st.max_block_ns, 9 * MS);
            assert_eq!(st.p99_block_ns, 9 * MS);
            assert_eq!(st.max_block_cpu_ns, MS);
            assert_eq!(st.p99_block_cpu_ns, MS);
            assert_eq!(st.last_block_cpu_ns, MS);
            assert_eq!(st.over_budget, 1_000, "the counters stay wall time");
            run(&mut stage, 1, 7 * MS, 7 * MS);
            let st = stage.stats();
            assert_eq!(st.step_downs, 1);
            assert_eq!(st.last_step_down, Some(StepDownReason::SlowBlock));
            assert_eq!(st.max_block_cpu_ns, 7 * MS);
        }

        /// The starvation cutoff: three model blocks in a row over 10 ms by the wall clock step
        /// down, whatever their CPU time; in a grace window too.
        #[test]
        fn starved_blocks_step_down() {
            let Some(mut stage) = timed_stage() else {
                return;
            };
            run(&mut stage, 10, MS, MS);
            run(&mut stage, 2, 11 * MS, MS);
            assert_eq!(stage.stats().step_downs, 0);
            run(&mut stage, 1, 11 * MS, MS);
            let st = stage.stats();
            assert_eq!(st.step_downs, 1);
            assert_eq!(st.last_step_down, Some(StepDownReason::Starved));
            assert_eq!(st.last_step_down_block, 12);

            let Some(mut stage) = timed_stage() else {
                return;
            };
            stage.control().grace(Duration::from_secs(10));
            run(&mut stage, 10, MS, MS);
            run(&mut stage, 3, 11 * MS, MS);
            let st = stage.stats();
            assert!(st.in_grace);
            assert_eq!(st.last_step_down, Some(StepDownReason::Starved));
            assert_eq!(st.grace_step_downs, 1, "tied to the grace window");
            assert_eq!(st.retry_at_block, 12 + 500, "retried after 5 s");
        }

        /// A retry is held while a block of the last 100 went over 6 ms of CPU, and the stats
        /// say so; a block long by the wall clock only does not hold it.
        #[test]
        fn a_held_retry_is_reported() {
            let Some(mut stage) = timed_stage() else {
                return;
            };
            run(&mut stage, 10, MS, MS);
            stage.control().force_step_down();
            run(&mut stage, 1, MS, MS);
            let st = stage.stats();
            assert_eq!(st.step_downs, 1);
            let due = st.retry_at_block;
            run(&mut stage, (due - st.blocks - 20) as u32, MS, MS);
            run(&mut stage, 1, 9 * MS, MS);
            run(&mut stage, 1, 7 * MS, 7 * MS);
            run(&mut stage, 40, MS, MS);
            let st = stage.stats();
            assert!(st.blocks > due);
            assert!(st.retry_held, "{st:?}");
            assert_eq!(st.strong_state, StrongState::SteppedDown);
            run(&mut stage, 100, MS, MS);
            let st = stage.stats();
            assert!(!st.retry_held);
            assert_ne!(st.strong_state, StrongState::SteppedDown);
        }
    }
}
