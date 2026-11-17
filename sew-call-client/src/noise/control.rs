//! [`NoiseRemovalControl`]: the setting, the stage it builds for each call's audio, and the
//! state and its changes.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use kessel_audio::{AudioEngine, EngineConfig};
use sew_noise::{Mode, Stage, StageControl, StepDownReason, warm_up_thread};
use tokio::sync::broadcast;

use super::hook::{self, Bypass, CaptureStage};
use super::{
    NoiseModel, NoiseRemoval, NoiseRemovalReason, NoiseRemovalState, NoiseSource,
    NoiseStepDownReason, PhoneAudioMode,
};
use crate::audio::{PhoneOs, PhoneStart};

/// A stage can be handed to this build's audio pipeline without DeepFilterNet3: its
/// kessel-audio has the hook, and the capture path is not a phone's OS voice processing (which
/// does its own suppression). With DeepFilterNet3 loaded a stage runs on phones too
/// ([`NoiseRemovalControl::stage_runs`]).
#[cfg(test)]
const STAGE_RUNS: bool = hook::HOOKED && !crate::audio::PHONE_AUDIO;

/// The device bench's figures of a weights load: RSS before and after (KiB), and the time (ms).
#[cfg(feature = "noise-bench")]
pub(crate) type LoadFigures = (Option<u64>, Option<u64>, u64);

/// The DeepFilterNet3 weights an app gave the control (feature `dfn3`).
#[cfg(feature = "dfn3")]
#[derive(Clone, Default)]
enum Dfn3Slot {
    /// None given: RNNoise, and the state carries no model fields.
    #[default]
    NotGiven,
    /// Given and checked: the path, and the model while it is in memory (loaded on first
    /// need, released after the idle time).
    Given {
        path: std::path::PathBuf,
        model: Option<Arc<sew_noise::dfn3::Dfn3Model>>,
    },
    /// Given but refused (missing, unreadable, not the pinned file): RNNoise.
    Failed,
}

/// How long the model stays in memory after the last call's audio ends, by default
/// ([`NoiseRemovalControl::set_dfn3_idle_unload`]). Five minutes: calls come in clusters (a
/// call back, a rejoin after a drop, the next meeting), and a reload costs a file read and
/// tens of milliseconds of one core, so the model stays for those, and the memory is back soon
/// after a session of calls ends.
#[cfg(feature = "dfn3")]
pub const DFN3_IDLE_UNLOAD: Duration = Duration::from_secs(5 * 60);

/// DeepFilterNet3's weights and model (feature `dfn3`).
#[cfg(feature = "dfn3")]
struct Dfn3State {
    slot: Dfn3Slot,
    info: Option<Dfn3LoadInfo>,
    /// A loader thread runs (a load, or handing the model to the live call's stage).
    busy: bool,
    /// The last load failed (the file changed or went away since it was given).
    load_failed: bool,
    /// Bumped when a call's audio starts or ends: an idle release acts only on its own.
    idle_gen: u64,
    idle_unload: Duration,
}

#[cfg(feature = "dfn3")]
impl Default for Dfn3State {
    fn default() -> Dfn3State {
        Dfn3State {
            slot: Dfn3Slot::NotGiven,
            info: None,
            busy: false,
            load_failed: false,
            idle_gen: 0,
            idle_unload: DFN3_IDLE_UNLOAD,
        }
    }
}

/// What loading the DeepFilterNet3 weights cost (feature `dfn3`): resident memory before and
/// after (KiB; `None` where this build can't read it) and the time taken.
#[cfg(feature = "dfn3")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Dfn3LoadInfo {
    /// Resident memory before the load (KiB).
    pub rss_before_kib: Option<u64>,
    /// Resident memory after it (KiB).
    pub rss_after_kib: Option<u64>,
    /// How long the load took (ms).
    pub load_ms: u64,
}

/// `sew-noise`'s [`Stage`] as the capture hook's stage.
///
/// The stage is built on the control thread and runs on kessel-audio's processing thread, so
/// the first block warms that thread up (the model's FFT plans; it allocates once, before any
/// block is processed) unless the pipeline called `reset` there first. After that nothing
/// allocates.
pub(crate) struct NoiseStage {
    stage: Stage,
    warm: bool,
}

impl NoiseStage {
    pub(crate) fn new(stage: Stage) -> NoiseStage {
        NoiseStage { stage, warm: false }
    }
}

impl CaptureStage for NoiseStage {
    fn process(&mut self, block: &mut [f32; 480]) {
        if !self.warm {
            warm_up_thread();
            self.warm = true;
        }
        self.stage.process(block);
    }

    fn latency_samples(&self) -> usize {
        Stage::LATENCY
    }

    fn reset(&mut self) {
        // Warms the calling thread up too.
        self.stage.reset();
        self.warm = true;
    }
}

/// The stage's mode for a setting: Standard is Strong when the stage has DeepFilterNet3,
/// Standard (RNNoise) otherwise; on phones a stage without the model passes the audio through
/// (the OS's processing does the suppression until DeepFilterNet3 is there).
fn stage_mode(mode: NoiseRemoval, strong: bool) -> Mode {
    match mode {
        NoiseRemoval::Off => Mode::Off,
        NoiseRemoval::Standard if strong => Mode::Strong,
        NoiseRemoval::Standard if crate::audio::PHONE_AUDIO => Mode::Off,
        NoiseRemoval::Standard => Mode::Standard,
    }
}

fn code(mode: NoiseRemoval) -> u8 {
    match mode {
        NoiseRemoval::Off => 0,
        NoiseRemoval::Standard => 1,
    }
}

fn from_code(v: u8) -> NoiseRemoval {
    if v == 0 {
        NoiseRemoval::Off
    } else {
        NoiseRemoval::Standard
    }
}

/// How long a grace window lasts after a moment the device is busy with something else (the
/// stage's own start window is as long): `sew_noise::DEFAULT_GRACE`.
const GRACE: Duration = sew_noise::DEFAULT_GRACE;

/// The stage's rule as the state names it.
fn step_down_reason(r: StepDownReason) -> Option<NoiseStepDownReason> {
    match r {
        StepDownReason::SlowBlock => Some(NoiseStepDownReason::SlowBlock),
        StepDownReason::SlowWindow => Some(NoiseStepDownReason::SlowWindow),
        StepDownReason::Heavy => Some(NoiseStepDownReason::Heavy),
        StepDownReason::Fault => Some(NoiseStepDownReason::Fault),
        StepDownReason::Forced => Some(NoiseStepDownReason::Forced),
        _ => None,
    }
}

/// `state` with the stage's timing and kessel-audio's counters of it, for the call's log.
fn with_timing(
    mut state: NoiseRemovalState,
    stage: &StageControl,
    audio: Option<&Arc<dyn LiveAudio>>,
) -> NoiseRemovalState {
    let st = stage.stats();
    if st.step_downs > 0 {
        state.step_down_reason = st.last_step_down.and_then(step_down_reason);
        state.last_step_down_ms_ago = Some(
            st.blocks
                .saturating_sub(st.last_step_down_block)
                .saturating_mul(10),
        );
    }
    if st.blocks > 0 {
        state.slowest_block_us = Some(st.max_block_ns / 1_000);
    }
    state.blocks_over_budget = st.over_budget;
    state.in_grace = st.in_grace;
    if let Some(p) = audio.and_then(|a| a.pipeline_stage()) {
        state.capture_stage_strikes = p.strikes;
        state.capture_stage_bypassed = p.bypassed.map(|b| match b {
            Bypass::Budget => NoiseRemovalReason::Budget,
            Bypass::Fault => NoiseRemovalReason::Fault,
        });
        state.capture_stage_p99_us = Some(p.p99_us);
    }
    state
}

/// How long a switch of the phone's capture may take before it counts as failed when no error
/// was seen (kessel-audio reopens the device in milliseconds; its error event may have been
/// read by someone else).
#[cfg(not(test))]
const SWITCH_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const SWITCH_TIMEOUT: Duration = Duration::from_millis(300);

/// A call's running audio as the control sees it: the pipeline's bypass flag and, on a phone,
/// its capture mode. kessel-audio's engine, or a test's.
pub(crate) trait LiveAudio: Send + Sync {
    fn bypass(&self) -> Option<Bypass>;
    /// The phone's capture now (`None`: not a phone backend).
    fn capture_mode(&self) -> Option<PhoneAudioMode> {
        None
    }
    /// Asks the phone backend to switch its capture (never blocks).
    fn switch_capture(&self, start: PhoneStart) {
        let _ = start;
    }
    /// The switches so far, and how long the device paused for the last one that reopened it.
    fn capture_switches(&self) -> (u64, Option<Duration>) {
        (0, None)
    }
    /// The pipeline's counters of the stage (`None`: none set, or not read here).
    fn pipeline_stage(&self) -> Option<hook::PipelineStage> {
        None
    }
}

impl LiveAudio for AudioEngine {
    fn bypass(&self) -> Option<Bypass> {
        hook::bypass(self)
    }

    fn pipeline_stage(&self) -> Option<hook::PipelineStage> {
        hook::pipeline_stage(self)
    }

    #[cfg(all(feature = "mobile", any(target_os = "ios", target_os = "android")))]
    fn capture_mode(&self) -> Option<PhoneAudioMode> {
        Some(crate::audio::phone_audio_mode(self.phone_mode()))
    }

    #[cfg(all(feature = "mobile", any(target_os = "ios", target_os = "android")))]
    fn switch_capture(&self, start: PhoneStart) {
        self.set_phone_mode(crate::audio::kessel_phone_mode(start));
    }

    fn capture_switches(&self) -> (u64, Option<Duration>) {
        let st = self.stats();
        (st.phone_mode_switches, st.last_phone_mode_gap)
    }
}

/// A phone call's capture, as the control asked for it.
struct PhoneLive {
    /// The phone's OS.
    os: PhoneOs,
    /// The capture last asked of the backend (at the start, or by a switch).
    asked: PhoneAudioMode,
    /// A switch under way since then.
    pending: Option<Instant>,
    /// kessel-audio reported an error while it was under way.
    error_seen: bool,
    /// The last switch failed: the backend went back to the capture it had.
    failed: bool,
}

/// One call's audio: the stage handed over (`None`: none could be), the running audio (its
/// bypass flag, and on a phone its capture) and, on a phone, the capture asked for.
struct Live {
    id: u64,
    stage: Option<StageControl>,
    audio: Option<Weak<dyn LiveAudio>>,
    phone: Option<PhoneLive>,
}

impl Live {
    fn audio(&self) -> Option<Arc<dyn LiveAudio>> {
        self.audio.as_ref().and_then(Weak::upgrade)
    }
}

struct Inner {
    mode: AtomicU8,
    /// Android: the device has the unprocessed audio source (the app says).
    unprocessed: AtomicBool,
    next_id: AtomicU64,
    live: Mutex<Option<Live>>,
    /// The state last published (its [`NoiseRemovalState::change_key`]).
    last: Mutex<Option<NoiseRemovalState>>,
    events: broadcast::Sender<NoiseRemovalState>,
    #[cfg(feature = "dfn3")]
    dfn3: Mutex<Dfn3State>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The noise removal setting of an app, and the handle on the stage of its live call's audio.
/// Clones share it; keep one per app (the setting outlives calls).
///
/// - [`NoiseRemovalControl::set`] changes the mode at once, before or during a call. In a
///   call the stage takes it at its next 10 ms block, cross-faded over one block: no click,
///   no change of delay.
/// - [`NoiseRemovalControl::state`] says what was asked and what runs
///   ([`NoiseRemovalState`]), and [`NoiseRemovalControl::subscribe`] each change of it (a
///   mode change, the pipeline bypassing the stage, a call's audio starting or ending).
/// - [`crate::AudioBridge::start_with_noise`] builds a call's stage in the current mode,
///   hands it to kessel-audio (whose own noise suppression is then off) and keeps the state
///   current while the call's audio runs.
/// - On iOS and Android (feature `mobile`) the setting also picks how the microphone is
///   captured ([`PhoneAudioMode`]): a call starts raw in Off and through the OS's voice
///   processing in Standard, and [`NoiseRemovalControl::set`] during a call switches the live
///   call's capture at once (the microphone pauses briefly while it reopens). On Android, tell
///   the control whether the device has the unprocessed audio source
///   ([`NoiseRemovalControl::set_android_unprocessed_supported`]).
///
/// Nothing is stored on disk: the host keeps the person's choice and sets it at start.
#[derive(Clone)]
pub struct NoiseRemovalControl {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for NoiseRemovalControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoiseRemovalControl")
            .field("state", &self.state())
            .finish()
    }
}

impl Default for NoiseRemovalControl {
    fn default() -> NoiseRemovalControl {
        NoiseRemovalControl::new(NoiseRemoval::default())
    }
}

impl NoiseRemovalControl {
    /// A control starting in `mode`.
    pub fn new(mode: NoiseRemoval) -> NoiseRemovalControl {
        let (events, _) = broadcast::channel(16);
        NoiseRemovalControl {
            inner: Arc::new(Inner {
                mode: AtomicU8::new(code(mode)),
                unprocessed: AtomicBool::new(false),
                next_id: AtomicU64::new(0),
                live: Mutex::new(None),
                last: Mutex::new(None),
                events,
                #[cfg(feature = "dfn3")]
                dfn3: Mutex::new(Dfn3State::default()),
            }),
        }
    }

    /// Where DeepFilterNet3's weights are (feature `dfn3`): the file the app ships as a
    /// resource (`sew_noise::dfn3::WEIGHTS_FILE`). The control records the path and checks the
    /// file against its pinned sha256 (read in chunks; nothing of it stays in memory); it does
    /// not load the model. The model is loaded on a thread of its own when it is first needed:
    /// when a call's audio starts in [`NoiseRemoval::Standard`], or at the first switch to
    /// Standard during a call. Until it is ready the call runs RNNoise (on phones, the OS's
    /// processing alone) and the state says `model_reason: loading`; then the call's stage
    /// steps up to DeepFilterNet3 without a click. After the last call's audio ends the model
    /// is released once [`Self::set_dfn3_idle_unload`]'s time passes with no call; the next
    /// call loads it again.
    ///
    /// The check reads the whole file (about 40 MB): call it off the UI and audio threads. On
    /// an error (a missing or unreadable file, or not the pinned weights) Standard stays
    /// RNNoise and the state says `model_reason: unavailable`; a later call of this may try
    /// again.
    #[cfg(feature = "dfn3")]
    pub fn set_dfn3_weights(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<(), sew_noise::dfn3::LoadError> {
        let path = path.as_ref();
        let checked = sew_noise::dfn3::Dfn3Model::verify(path);
        let out = {
            let mut st = lock(&self.inner.dfn3);
            st.load_failed = false;
            match checked {
                Ok(()) => {
                    st.slot = Dfn3Slot::Given {
                        path: path.to_path_buf(),
                        model: None,
                    };
                    Ok(())
                }
                Err(e) => {
                    tracing::warn!(error = %e, "noise removal: DeepFilterNet3 weights refused; Standard stays RNNoise");
                    st.slot = Dfn3Slot::Failed;
                    Err(e)
                }
            }
        };
        self.publish();
        out
    }

    /// Loads DeepFilterNet3's model now, on the calling thread (feature `dfn3`), for an app
    /// that wants it ready before a call (most don't: a call loads it). Blocks for the load
    /// (tens of milliseconds, about 40 MB resident); a live call's stage gets it too. `Ok`
    /// when it is loaded (or already was); the load's error otherwise, and Standard stays
    /// RNNoise with `model_reason: unavailable`. Without weights given:
    /// [`sew_noise::dfn3::LoadError::Io`].
    #[cfg(feature = "dfn3")]
    pub fn preload_dfn3(&self) -> Result<(), sew_noise::dfn3::LoadError> {
        let path = {
            let st = lock(&self.inner.dfn3);
            match &st.slot {
                Dfn3Slot::Given { model: Some(_), .. } => None,
                Dfn3Slot::Given { path, .. } => Some(path.clone()),
                _ => {
                    return Err(sew_noise::dfn3::LoadError::Io(
                        "no DeepFilterNet3 weights given".into(),
                    ));
                }
            }
        };
        if let Some(path) = path {
            self.load_now(&path)?;
        }
        if let Some(model) = self.model() {
            self.attach_live(model);
        }
        self.publish();
        Ok(())
    }

    /// How long DeepFilterNet3's model stays in memory after the last call's audio ends
    /// (feature `dfn3`; [`DFN3_IDLE_UNLOAD`] by default). A call that starts in that time
    /// keeps it; after it, the memory is released and the next call loads the model again.
    /// `Duration::MAX` keeps it for the process's life; `Duration::ZERO` releases it as the
    /// call ends. Applies from the next call's end.
    #[cfg(feature = "dfn3")]
    pub fn set_dfn3_idle_unload(&self, after: Duration) {
        lock(&self.inner.dfn3).idle_unload = after;
    }

    /// [`Self::set_dfn3_idle_unload`]'s time.
    #[cfg(feature = "dfn3")]
    pub fn dfn3_idle_unload(&self) -> Duration {
        lock(&self.inner.dfn3).idle_unload
    }

    /// Whether DeepFilterNet3's model is in memory now.
    #[cfg(feature = "dfn3")]
    pub fn dfn3_loaded(&self) -> bool {
        matches!(
            lock(&self.inner.dfn3).slot,
            Dfn3Slot::Given { model: Some(_), .. }
        )
    }

    /// What the last load of the model cost, once one ran.
    #[cfg(feature = "dfn3")]
    pub fn dfn3_load_info(&self) -> Option<Dfn3LoadInfo> {
        lock(&self.inner.dfn3).info
    }

    /// The model in memory, if any.
    #[cfg(feature = "dfn3")]
    fn model(&self) -> Option<Arc<sew_noise::dfn3::Dfn3Model>> {
        match &lock(&self.inner.dfn3).slot {
            Dfn3Slot::Given { model: Some(m), .. } => Some(Arc::clone(m)),
            _ => None,
        }
    }

    /// Loads the model from `path` on this thread and keeps it, with the load's figures.
    #[cfg(feature = "dfn3")]
    fn load_now(
        &self,
        path: &std::path::Path,
    ) -> Result<Arc<sew_noise::dfn3::Dfn3Model>, sew_noise::dfn3::LoadError> {
        let before = super::rss_kib();
        let started = Instant::now();
        let loaded = sew_noise::dfn3::Dfn3Model::load(path);
        let info = Dfn3LoadInfo {
            rss_before_kib: before,
            rss_after_kib: super::rss_kib(),
            load_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        let mut st = lock(&self.inner.dfn3);
        st.info = Some(info);
        match loaded {
            Ok(m) => {
                st.load_failed = false;
                if let Dfn3Slot::Given { path: p, model } = &mut st.slot
                    && p.as_path() == path
                {
                    *model = Some(Arc::clone(&m));
                }
                Ok(m)
            }
            Err(e) => {
                tracing::warn!(error = %e, "noise removal: DeepFilterNet3 not loaded; Standard stays RNNoise");
                st.load_failed = true;
                Err(e)
            }
        }
    }

    /// Standard is wanted in a live call whose stage lacks the model: loads it (if it is not
    /// in memory) and hands it to the stage, on a thread of its own. Never blocks the caller.
    /// Nothing to do without usable weights, or while a loader runs (it hands the model to
    /// whichever call is live when it is done).
    #[cfg(feature = "dfn3")]
    fn ensure_dfn3(&self) {
        let path = {
            let mut st = lock(&self.inner.dfn3);
            if st.busy || st.load_failed {
                return;
            }
            let Dfn3Slot::Given { path, .. } = &st.slot else {
                return;
            };
            let path = path.clone();
            st.busy = true;
            path
        };
        let ctl = self.clone();
        let spawned = std::thread::Builder::new()
            .name("sew-dfn3-load".into())
            .spawn(move || {
                let model = match ctl.model() {
                    Some(m) => Some(m),
                    None => ctl.load_now(&path).ok(),
                };
                if let Some(model) = model {
                    ctl.attach_live(model);
                }
                lock(&ctl.inner.dfn3).busy = false;
                ctl.publish();
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "noise removal: no DeepFilterNet3 loader thread; Standard stays RNNoise");
            let mut st = lock(&self.inner.dfn3);
            st.busy = false;
            st.load_failed = true;
        }
    }

    /// Hands `model` to the live call's stage if it lacks one: the layer is built here (slow,
    /// off the audio thread) and taken by the stage at its next block, which then steps up to
    /// DeepFilterNet3 as a stage built with it starts.
    #[cfg(feature = "dfn3")]
    fn attach_live(&self, model: Arc<sew_noise::dfn3::Dfn3Model>) {
        let wanted = lock(&self.inner.live)
            .as_ref()
            .and_then(|l| l.stage.as_ref())
            .is_some_and(|s| !s.strong_available());
        if !wanted {
            return;
        }
        let layer = sew_noise::Dfn3Layer::new(model);
        let live = lock(&self.inner.live);
        if let Some(stage) = live
            .as_ref()
            .and_then(|l| l.stage.as_ref())
            .filter(|s| !s.strong_available())
            && stage.attach_dfn3(layer).is_ok()
        {
            // Under the live lock, as `set` changes the mode.
            let _ = stage.set_mode(stage_mode(self.mode(), true));
        }
    }

    /// A call's audio starts: a pending idle release is off.
    #[cfg(feature = "dfn3")]
    fn dfn3_call_starts(&self) {
        lock(&self.inner.dfn3).idle_gen += 1;
    }

    /// The last call's audio ended: the model is released after the idle time unless a call
    /// starts first.
    #[cfg(feature = "dfn3")]
    fn dfn3_call_ends(&self) {
        let (generation, after) = {
            let mut st = lock(&self.inner.dfn3);
            st.idle_gen += 1;
            if !matches!(st.slot, Dfn3Slot::Given { model: Some(_), .. }) {
                return;
            }
            (st.idle_gen, st.idle_unload)
        };
        if after == Duration::MAX {
            return;
        }
        if after.is_zero() {
            self.release_if_idle(generation);
            return;
        }
        let inner = Arc::downgrade(&self.inner);
        let spawned = std::thread::Builder::new()
            .name("sew-dfn3-idle".into())
            .spawn(move || {
                std::thread::sleep(after);
                if let Some(inner) = inner.upgrade() {
                    NoiseRemovalControl { inner }.release_if_idle(generation);
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "noise removal: no idle timer; DeepFilterNet3 stays in memory");
        }
    }

    /// Releases the model if no call's audio started since the release was set up.
    #[cfg(feature = "dfn3")]
    fn release_if_idle(&self, generation: u64) {
        let released = {
            let live = lock(&self.inner.live);
            let mut st = lock(&self.inner.dfn3);
            if live.is_some() || st.idle_gen != generation {
                return;
            }
            match &mut st.slot {
                Dfn3Slot::Given { model, .. } => model.take(),
                _ => None,
            }
        };
        if released.is_some() {
            // The last reference (calls' stages were dropped with their audio): the memory
            // goes back here, outside the locks.
            drop(released);
            tracing::info!("noise removal: DeepFilterNet3 released after the idle time");
            self.publish();
        }
    }

    /// Why the live call has no DeepFilterNet3 yet: `loading` while it can still come,
    /// `unavailable` when it can't.
    fn dfn3_missing_reason(&self) -> NoiseRemovalReason {
        #[cfg(feature = "dfn3")]
        {
            let st = lock(&self.inner.dfn3);
            match st.slot {
                Dfn3Slot::Given { .. } if !st.load_failed => NoiseRemovalReason::Loading,
                _ => NoiseRemovalReason::Unavailable,
            }
        }
        #[cfg(not(feature = "dfn3"))]
        {
            NoiseRemovalReason::Unavailable
        }
    }

    /// The device bench's view of the weights, after loading them now if they were given:
    /// loaded, and (RSS before, RSS after, ms) of the last load.
    #[cfg(feature = "noise-bench")]
    pub(crate) fn dfn3_info(&self) -> (bool, Option<LoadFigures>) {
        #[cfg(feature = "dfn3")]
        {
            let _ = self.preload_dfn3();
            (
                self.dfn3_loaded(),
                self.dfn3_load_info()
                    .map(|i| (i.rss_before_kib, i.rss_after_kib, i.load_ms)),
            )
        }
        #[cfg(not(feature = "dfn3"))]
        {
            (false, None)
        }
    }

    /// The app gave weights (usable or not): the state carries the model fields.
    fn dfn3_given(&self) -> bool {
        #[cfg(feature = "dfn3")]
        {
            !matches!(lock(&self.inner.dfn3).slot, Dfn3Slot::NotGiven)
        }
        #[cfg(not(feature = "dfn3"))]
        {
            false
        }
    }

    /// The app gave weights that checked out and the last load (if any) worked: a call can
    /// have DeepFilterNet3.
    fn dfn3_usable(&self) -> bool {
        #[cfg(feature = "dfn3")]
        {
            let st = lock(&self.inner.dfn3);
            matches!(st.slot, Dfn3Slot::Given { .. }) && !st.load_failed
        }
        #[cfg(not(feature = "dfn3"))]
        {
            false
        }
    }

    /// A stage can be handed to this build's pipeline: kessel-audio has the hook and either
    /// the capture path is not a phone's OS voice processing, or DeepFilterNet3 can come (it
    /// runs after the OS's processing there; until it is loaded the stage passes the audio
    /// through).
    pub(crate) fn stage_runs(&self) -> bool {
        hook::HOOKED && (!crate::audio::PHONE_AUDIO || self.dfn3_usable())
    }

    /// The stage a call's audio starts with, in the current mode: over DeepFilterNet3 when its
    /// model is in memory, RNNoise otherwise (DeepFilterNet3 is handed over once loaded).
    pub(crate) fn build_stage(&self) -> Stage {
        #[cfg(feature = "dfn3")]
        if let Some(model) = self.model() {
            let stage = Stage::with_dfn3(model);
            let _ = stage.set_mode(stage_mode(self.mode(), true));
            return stage;
        }
        let stage = Stage::new();
        let _ = stage.set_mode(stage_mode(self.mode(), false));
        stage
    }

    /// `state` with the model fields, when DeepFilterNet3 is in play.
    fn with_model(
        &self,
        state: NoiseRemovalState,
        stage: Option<&StageControl>,
    ) -> NoiseRemovalState {
        if !self.dfn3_given() || state.active == NoiseRemoval::Off {
            return state;
        }
        let Some(stage) = stage else {
            // No call yet (or one without a stage): what a call would get.
            return if self.dfn3_usable() {
                state.with_model(Some(NoiseModel::Dfn3), None, 0)
            } else {
                state.with_model(
                    Some(NoiseModel::Rnnoise),
                    Some(NoiseRemovalReason::Unavailable),
                    0,
                )
            };
        };
        let st = stage.stats();
        let step_downs = u32::try_from(st.step_downs).unwrap_or(u32::MAX);
        if !stage.strong_available() {
            return state.with_model(
                Some(NoiseModel::Rnnoise),
                Some(self.dfn3_missing_reason()),
                step_downs,
            );
        }
        if st.blocks == 0 || stage.active() == Mode::Strong {
            return state.with_model(Some(NoiseModel::Dfn3), None, step_downs);
        }
        let why = match st.last_step_down {
            Some(StepDownReason::Fault) => Some(NoiseRemovalReason::Fault),
            Some(_) => Some(NoiseRemovalReason::Budget),
            // Handed over during the call, and warming up before it fades in.
            None if st.strong_state == sew_noise::StrongState::Warming
                || st.strong_state == sew_noise::StrongState::Idle =>
            {
                Some(NoiseRemovalReason::Loading)
            }
            None => None,
        };
        state.with_model(Some(NoiseModel::Rnnoise), why, step_downs)
    }

    /// The mode asked for.
    pub fn mode(&self) -> NoiseRemoval {
        from_code(self.inner.mode.load(Ordering::Acquire))
    }

    /// Android: whether this device has the unprocessed audio source, as the app reads it
    /// (`AudioManager.getProperty(AudioManager.PROPERTY_SUPPORT_AUDIO_SOURCE_UNPROCESSED)` is
    /// `"true"`). With it, [`NoiseRemoval::Off`] captures from that source; without it (the
    /// default) from the voice recognition source, which Android keeps free of noise
    /// reduction. It applies from the next time the phone's capture opens raw (a call's start,
    /// or a switch to Off). Other platforms ignore it.
    pub fn set_android_unprocessed_supported(&self, supported: bool) {
        self.inner.unprocessed.store(supported, Ordering::Release);
    }

    /// What [`Self::set_android_unprocessed_supported`] last said (`false` until then).
    pub fn android_unprocessed_supported(&self) -> bool {
        self.inner.unprocessed.load(Ordering::Acquire)
    }

    /// Asks for `mode`, for the live call (from its next block; on a phone its capture switches
    /// at once) and later ones; answers the state now. A change of state is also sent to
    /// [`NoiseRemovalControl::subscribe`]'s receivers.
    pub fn set(&self, mode: NoiseRemoval) -> NoiseRemovalState {
        {
            // Under the live lock, so a call's audio starting now builds its stage in this
            // mode or gets it here.
            let mut live = lock(&self.inner.live);
            self.inner.mode.store(code(mode), Ordering::Release);
            if let Some(stage) = live.as_ref().and_then(|l| l.stage.as_ref()) {
                // Off and Standard are always available; Strong when the stage has the model.
                // Off is the stage passing the audio through.
                let _ = stage.set_mode(stage_mode(mode, stage.strong_available()));
                // Standard in a call whose stage lacks DeepFilterNet3: load it and hand it
                // over (on a thread of its own).
                #[cfg(feature = "dfn3")]
                if mode == NoiseRemoval::Standard && !stage.strong_available() {
                    self.ensure_dfn3();
                }
            }
            self.switch_phone(&mut live);
        }
        self.publish()
    }

    /// On a phone call whose audio runs: its capture follows the setting (a no-op when it
    /// already was asked for, or before the audio is attached: [`Self::attach`] does it then).
    fn switch_phone(&self, live: &mut Option<Live>) {
        let Some(l) = live.as_mut() else { return };
        let Some(audio) = l.audio() else { return };
        let Some(p) = l.phone.as_mut() else { return };
        let mode = self.mode();
        let want = PhoneAudioMode::for_mode(mode);
        // With no switch under way, what runs (a switch that timed out may have landed late).
        let current = match p.pending {
            Some(_) => p.asked,
            None => audio.capture_mode().unwrap_or(p.asked),
        };
        if want == current {
            return;
        }
        // The microphone reopens: the stage's timing rules hold back meanwhile.
        if let Some(stage) = &l.stage {
            stage.grace(GRACE);
        }
        audio.switch_capture(crate::audio::phone_start(
            p.os,
            mode,
            self.android_unprocessed_supported(),
        ));
        p.asked = want;
        p.pending = Some(Instant::now());
        p.error_seen = false;
        p.failed = false;
    }

    /// The phone fields of the state: the capture now, why it is not what `mode` asks, the
    /// switches and the last pause. Settles a switch under way: done once the backend runs
    /// the capture asked; failed after an error, or when [`SWITCH_TIMEOUT`] passes without it.
    fn phone_fields(
        mode: NoiseRemoval,
        p: &mut PhoneLive,
        audio: Option<&Arc<dyn LiveAudio>>,
    ) -> (
        Option<PhoneAudioMode>,
        Option<NoiseRemovalReason>,
        u32,
        Option<u32>,
    ) {
        let now = audio.and_then(|a| a.capture_mode()).unwrap_or(p.asked);
        if let Some(since) = p.pending {
            if now == p.asked {
                p.pending = None;
            } else if p.error_seen || since.elapsed() >= SWITCH_TIMEOUT {
                tracing::warn!(
                    asked = p.asked.as_str(),
                    kept = now.as_str(),
                    "noise removal: the phone's capture did not switch; it kept the one it had"
                );
                p.pending = None;
                p.failed = true;
            }
        }
        if p.pending.is_none() {
            // A later switch starts from what runs.
            p.asked = now;
        }
        if p.failed && now == PhoneAudioMode::for_mode(mode) {
            p.failed = false;
        }
        let (switches, gap) = audio.map_or((0, None), |a| a.capture_switches());
        (
            Some(now),
            p.failed.then_some(NoiseRemovalReason::SwitchFailed),
            u32::try_from(switches).unwrap_or(u32::MAX),
            gap.map(|g| u32::try_from(g.as_millis()).unwrap_or(u32::MAX)),
        )
    }

    /// kessel-audio reported an error during the call `id` ([`crate::AudioBridge::events`]):
    /// a switch of the phone's capture under way failed.
    pub(crate) fn audio_error(&self, id: u64) {
        let changed = {
            let mut live = lock(&self.inner.live);
            match live
                .as_mut()
                .filter(|l| l.id == id)
                .and_then(|l| l.phone.as_mut())
            {
                Some(p) if p.pending.is_some() => {
                    p.error_seen = true;
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.publish();
        }
    }

    /// Opens (or extends) a grace window on the live call's stage (`id`: only if it is that
    /// call's) and sends the state's change at once. Nothing without a live stage.
    fn open_grace(&self, id: Option<u64>) {
        let opened = {
            let live = lock(&self.inner.live);
            match live
                .as_ref()
                .filter(|l| id.is_none_or(|i| l.id == i))
                .and_then(|l| l.stage.as_ref())
            {
                Some(stage) => {
                    stage.grace(GRACE);
                    true
                }
                None => false,
            }
        };
        if opened {
            self.publish();
        }
    }

    /// The app moved between the foreground and the background, or its window gained or lost
    /// focus. Call it from the app's lifecycle (iOS's scene phase, Android's `onResume` and
    /// `onPause`, a window's focus); it never blocks.
    ///
    /// Such a switch loads the device for a moment. The live call's stage opens a grace window
    /// (about 3 s, `sew_noise::DEFAULT_GRACE`): its timing rules are held back to the audio
    /// pipeline's own line, so the switch does not cost the call DeepFilterNet3, and a
    /// step-down that happens anyway is retried after 5 s instead of 30 s. The state says
    /// `in_grace` while it lasts. Nothing happens without a call.
    ///
    /// The call's own events open the same window by themselves: its audio starting, the
    /// phone's audio session activating or resuming, an audio route change
    /// ([`crate::AudioSession::event`]), a device switch kessel-audio reports
    /// ([`crate::AudioBridge::events`]), a switch of the phone's capture, and a camera starting
    /// through `VideoBridge::set_camera` (feature `video-core`).
    pub fn app_scene_changed(&self) {
        self.open_grace(None);
    }

    /// The phone's audio session reported `event` to the call `id`'s audio: the device is busy
    /// for a moment when the session activates or resumes and when the route changes.
    pub(crate) fn session_event(&self, id: u64, event: crate::audio::SessionEvent) {
        use crate::audio::SessionEvent;
        match event {
            SessionEvent::Activated | SessionEvent::Resume | SessionEvent::RouteChanged => {
                self.open_grace(Some(id));
            }
            SessionEvent::Deactivated | SessionEvent::Suspend => {}
        }
    }

    /// kessel-audio switched the call `id`'s devices (a hot-plug, a default change, a fallback
    /// after a loss): the streams reopened.
    pub(crate) fn devices_changed(&self, id: u64) {
        self.open_grace(Some(id));
    }

    /// The setting and what runs now.
    pub fn state(&self) -> NoiseRemovalState {
        let mode = self.mode();
        let mut live = lock(&self.inner.live);
        let Some(live) = live.as_mut() else {
            // No call: what a call would start with.
            let state = if self.stage_runs() {
                self.with_model(NoiseRemovalState::running(mode), None)
            } else {
                NoiseRemovalState::stepped_down(mode, NoiseRemovalReason::Unavailable)
            };
            return match crate::audio::PHONE_OS {
                Some(_) => {
                    state.with_phone_mode(Some(PhoneAudioMode::for_mode(mode)), None, 0, None)
                }
                None => state,
            };
        };
        let audio = live.audio();
        let state = self.live_state(mode, live, audio.as_ref());
        match live.phone.as_mut() {
            Some(p) => {
                let (now, why, switches, gap) = Self::phone_fields(mode, p, audio.as_ref());
                state.with_phone_mode(now, why, switches, gap)
            }
            None => state,
        }
    }

    /// What a live call's stage does now, with its timing where DeepFilterNet3 is in play.
    fn live_state(
        &self,
        mode: NoiseRemoval,
        live: &Live,
        audio: Option<&Arc<dyn LiveAudio>>,
    ) -> NoiseRemovalState {
        let Some(stage) = &live.stage else {
            return NoiseRemovalState::stepped_down(mode, NoiseRemovalReason::Unavailable);
        };
        let state = self.stage_state(mode, stage, audio);
        if !self.dfn3_given() {
            return state;
        }
        with_timing(state, stage, audio)
    }

    /// What the live call's `stage` does now.
    fn stage_state(
        &self,
        mode: NoiseRemoval,
        stage: &StageControl,
        audio: Option<&Arc<dyn LiveAudio>>,
    ) -> NoiseRemovalState {
        if crate::audio::PHONE_AUDIO && mode == NoiseRemoval::Standard && !stage.strong_available()
        {
            // A phone's stage passes the audio through until DeepFilterNet3 is there.
            return NoiseRemovalState::stepped_down(mode, self.dfn3_missing_reason());
        }
        if let Some(b) = audio.and_then(|s| s.bypass()) {
            let reason = match b {
                Bypass::Budget => NoiseRemovalReason::Budget,
                Bypass::Fault => NoiseRemovalReason::Fault,
            };
            return NoiseRemovalState::stepped_down(mode, reason);
        }
        if stage.stats().blocks == 0 {
            // No block yet: the stage starts in the mode asked.
            return self.with_model(NoiseRemovalState::running(mode), Some(stage));
        }
        let active = match stage.active() {
            Mode::Off => NoiseRemoval::Off,
            _ => NoiseRemoval::Standard,
        };
        self.with_model(NoiseRemovalState::new(mode, active, None), Some(stage))
    }

    /// Each change of [`NoiseRemovalControl::state`], from now on. While a call's audio runs
    /// the state is checked about every 100 ms; [`NoiseRemovalControl::set`] sends at once.
    pub fn subscribe(&self) -> broadcast::Receiver<NoiseRemovalState> {
        self.inner.events.subscribe()
    }

    /// Sends the state if it changed since it was last sent; answers it.
    pub(crate) fn publish(&self) -> NoiseRemovalState {
        let state = self.state();
        // The counters ride along; they don't make a change by themselves.
        let key = state.change_key();
        let mut last = lock(&self.inner.last);
        if *last != Some(key) {
            *last = Some(key);
            // No receiver is fine.
            let _ = self.inner.events.send(state);
        }
        state
    }

    /// A call's audio is about to start with `cfg`: builds its stage in the current mode and
    /// hands it to kessel-audio, whose own noise suppression is then off. On a phone without
    /// DeepFilterNet3 loaded, or with a kessel-audio without the hook, `cfg` is unchanged and
    /// the state says `unavailable`.
    /// Answers the config and the call's id for [`Self::attach`] and [`Self::detach`].
    pub(crate) fn configure(&self, mut cfg: EngineConfig) -> (EngineConfig, u64) {
        // Built (and, with DeepFilterNet3, warmed up) before the live lock is taken.
        let built = self.stage_runs().then(|| self.build_stage());
        let mut live = lock(&self.inner.live);
        let stage = if let Some(stage) = built {
            let ctl = stage.control();
            // A mode set while the stage was being built.
            let _ = ctl.set_mode(stage_mode(self.mode(), ctl.strong_available()));
            if hook::install(&mut cfg.processing, Box::new(NoiseStage::new(stage))) {
                cfg.processing.noise_suppression = None;
                Some(ctl)
            } else {
                None
            }
        } else {
            None
        };
        // Phones: the call's capture starts as the setting asks (raw for Off), so it opens
        // once, in that mode.
        let phone = crate::audio::apply_phone_mode(
            &mut cfg,
            self.mode(),
            self.android_unprocessed_supported(),
        )
        .zip(crate::audio::PHONE_OS)
        .map(|(asked, os)| PhoneLive {
            os,
            asked,
            pending: None,
            error_seen: false,
            failed: false,
        });
        #[cfg(feature = "dfn3")]
        let lacks_model = stage.as_ref().is_some_and(|s| !s.strong_available());
        let id = self.go_live(&mut live, stage, phone);
        drop(live);
        #[cfg(feature = "dfn3")]
        {
            self.dfn3_call_starts();
            // A call starting in Standard without the model in memory: it starts on RNNoise
            // (on phones the stage passes the audio through and the OS's processing runs
            // alone) and DeepFilterNet3 follows once loaded, never holding the start up.
            if lacks_model && self.mode() == NoiseRemoval::Standard {
                self.ensure_dfn3();
            }
        }
        self.publish();
        (cfg, id)
    }

    fn go_live(
        &self,
        live: &mut Option<Live>,
        stage: Option<StageControl>,
        phone: Option<PhoneLive>,
    ) -> u64 {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        *live = Some(Live {
            id,
            stage,
            audio: None,
            phone,
        });
        id
    }

    /// The call `id`'s pipeline is running: its bypass flag (and on a phone its capture) is
    /// read from `source`. A change of the setting since [`Self::configure`] reaches the
    /// phone's capture now.
    pub(crate) fn attach(&self, id: u64, source: Weak<dyn LiveAudio>) {
        let mut live = lock(&self.inner.live);
        let Some(l) = live.as_mut().filter(|l| l.id == id) else {
            return;
        };
        l.audio = Some(source);
        self.switch_phone(&mut live);
    }

    /// The call `id`'s audio ended (or never started).
    pub(crate) fn detach(&self, id: u64) {
        let mut live = lock(&self.inner.live);
        if live.as_ref().is_some_and(|l| l.id == id) {
            *live = None;
            drop(live);
            #[cfg(feature = "dfn3")]
            self.dfn3_call_ends();
            self.publish();
        }
    }

    /// The call `id`'s state, for its stats ([`NoiseSource`]); `None` once its audio ended.
    pub(crate) fn source(&self, id: u64) -> Arc<dyn NoiseSource> {
        Arc::new(CallNoise {
            inner: Arc::downgrade(&self.inner),
            id,
        })
    }

    /// Test only: a call's audio with `stage` handed over (or `None`), without kessel-audio.
    #[cfg(test)]
    fn live_with(&self, stage: Option<StageControl>) -> u64 {
        let mut live = lock(&self.inner.live);
        let id = self.go_live(&mut live, stage, None);
        drop(live);
        self.publish();
        id
    }

    /// Test only: a phone call's audio on `os`, its capture started as `configure` would
    /// start it, with `stage` handed over (or `None`), without kessel-audio. Answers the id
    /// and the starting capture.
    #[cfg(test)]
    fn phone_live_with(&self, os: PhoneOs, stage: Option<StageControl>) -> (u64, PhoneStart) {
        let start =
            crate::audio::phone_start(os, self.mode(), self.android_unprocessed_supported());
        let mut live = lock(&self.inner.live);
        let id = self.go_live(
            &mut live,
            stage,
            Some(PhoneLive {
                os,
                asked: start.mode,
                pending: None,
                error_seen: false,
                failed: false,
            }),
        );
        drop(live);
        self.publish();
        (id, start)
    }
}

/// One call's view of the control.
struct CallNoise {
    inner: Weak<Inner>,
    id: u64,
}

impl NoiseSource for CallNoise {
    fn noise_state(&self) -> Option<NoiseRemovalState> {
        let inner = self.inner.upgrade()?;
        if !lock(&inner.live).as_ref().is_some_and(|l| l.id == self.id) {
            return None;
        }
        Some(NoiseRemovalControl { inner }.state())
    }

    fn grace(&self) {
        if let Some(inner) = self.inner.upgrade() {
            NoiseRemovalControl { inner }.open_grace(Some(self.id));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    use std::sync::atomic::AtomicU8 as Flag;

    /// A stage built as `configure` builds one, with its handle.
    fn stage_for(control: &NoiseRemovalControl) -> (Box<dyn CaptureStage>, StageControl) {
        let stage = control.build_stage();
        let ctl = stage.control();
        (Box::new(NoiseStage::new(stage)), ctl)
    }

    /// A pipeline's bypass flag a test sets: 0 none, 1 budget, 2 fault.
    #[derive(Default)]
    struct FakeBypass(Flag);

    impl LiveAudio for FakeBypass {
        fn bypass(&self) -> Option<Bypass> {
            match self.0.load(Ordering::Relaxed) {
                1 => Some(Bypass::Budget),
                2 => Some(Bypass::Fault),
                _ => None,
            }
        }
    }

    fn largest_jump(prev: &mut f32, block: &[f32]) -> f32 {
        let mut worst = 0.0f32;
        for &v in block {
            worst = worst.max((v - *prev).abs());
            *prev = v;
        }
        worst
    }

    /// Bar: a mode change mid-call through the handle makes no click: no sample jump above
    /// -40 dBFS on silence (digital silence, and a room's noise at -60 dBFS), across many
    /// changes. The stage runs on its own thread, as on kessel-audio's.
    #[test]
    fn a_mode_change_mid_call_makes_no_click_on_silence() {
        let limit = 0.01f32; // -40 dBFS
        for (name, amp) in [("digital silence", 0.0f32), ("room at -60 dBFS", 0.001)] {
            let control = NoiseRemovalControl::new(NoiseRemoval::Standard);
            let (mut stage, ctl) = stage_for(&control);
            control.live_with(Some(ctl));
            let worker_control = control.clone();
            let (worst, peak) = std::thread::spawn(move || {
                let mut seed = 0x9e37_79b9_u32;
                let mut prev = 0.0f32;
                let (mut worst, mut peak) = (0.0f32, 0.0f32);
                for i in 0..1_500usize {
                    if i >= 200 && i % 7 == 0 {
                        let next = match worker_control.mode() {
                            NoiseRemoval::Off => NoiseRemoval::Standard,
                            _ => NoiseRemoval::Off,
                        };
                        worker_control.set(next);
                    }
                    let mut block = [0.0f32; 480];
                    for s in block.iter_mut() {
                        // A small white noise from an LCG, at `amp` peak.
                        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        *s = amp * ((seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0);
                    }
                    stage.process(&mut block);
                    if i >= 200 {
                        worst = worst.max(largest_jump(&mut prev, &block));
                        peak = block.iter().fold(peak, |p, v| p.max(v.abs()));
                    } else {
                        prev = block[479];
                    }
                    if amp == 0.0 {
                        assert!(block.iter().all(|v| *v == 0.0), "silence in, silence out");
                    }
                }
                (worst, peak)
            })
            .join()
            .unwrap();
            assert!(
                worst <= limit,
                "{name}: a sample jump of {worst} (limit {limit})"
            );
            assert!(peak <= limit, "{name}: a peak of {peak} (limit {limit})");
        }
    }

    /// Bar: no allocation on the stage's path after the first block on its thread: blocks,
    /// mode changes through the handle, state reads and resets.
    #[test]
    fn the_stage_path_does_not_allocate() {
        let control = NoiseRemovalControl::new(NoiseRemoval::Standard);
        let (mut stage, ctl) = stage_for(&control);
        let stats = ctl.clone();
        control.live_with(Some(ctl));
        std::thread::spawn(move || {
            // The first block on this thread warms it up (allocates once).
            stage.process(&mut [0.0; 480]);
            let info = allocation_counter::measure(|| {
                let mut block = [0.0f32; 480];
                for i in 0..1_200usize {
                    for (j, s) in block.iter_mut().enumerate() {
                        *s = 0.05 * ((i * 480 + j) as f32 * 0.031).sin();
                    }
                    if i % 50 == 25 {
                        block[3] = f32::NAN;
                    }
                    stage.process(&mut block);
                    if i % 97 == 96 {
                        stage.reset();
                    }
                    let _ = stage.latency_samples();
                }
            });
            assert_eq!(info.count_total, 0, "allocations on the stage's path");
        })
        .join()
        .unwrap();
        assert_eq!(stats.stats().cold_thread_blocks, 0);
        assert_eq!(stats.stats().blocks, 1_201);

        // A stage whose pipeline resets it on its thread before the first block: no
        // allocation at all on that thread.
        let control = NoiseRemovalControl::new(NoiseRemoval::Off);
        let (mut stage, ctl) = stage_for(&control);
        std::thread::spawn(move || {
            stage.reset();
            let info = allocation_counter::measure(|| {
                for i in 0..300 {
                    if i == 100 {
                        let _ = ctl.set_mode(Mode::Standard);
                    }
                    stage.process(&mut [0.01; 480]);
                }
            });
            assert_eq!(info.count_total, 0);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn the_stage_keeps_its_latency_in_every_mode() {
        let control = NoiseRemovalControl::new(NoiseRemoval::Off);
        let (mut stage, ctl) = stage_for(&control);
        control.live_with(Some(ctl));
        assert_eq!(stage.latency_samples(), 480);
        let mut first = [0.25f32; 480];
        stage.process(&mut first);
        assert!(first.iter().all(|v| *v == 0.0), "one block of latency");
        let mut second = [0.0f32; 480];
        stage.process(&mut second);
        assert!(second.iter().all(|v| *v == 0.25), "Off is the input, late");
        control.set(NoiseRemoval::Standard);
        assert_eq!(stage.latency_samples(), 480);
    }

    #[test]
    fn mode_active_and_reason() {
        let control = NoiseRemovalControl::new(NoiseRemoval::Standard);
        let mut rx = control.subscribe();
        // No call: what a call would start with. In this build kessel-audio has no hook.
        let idle = control.state();
        assert_eq!(idle.mode, NoiseRemoval::Standard);
        if STAGE_RUNS {
            assert_eq!(idle, NoiseRemovalState::running(NoiseRemoval::Standard));
        } else {
            assert_eq!(
                idle,
                NoiseRemovalState::stepped_down(
                    NoiseRemoval::Standard,
                    NoiseRemovalReason::Unavailable
                )
            );
        }

        // A call whose pipeline took no stage: unavailable; Off asked: nothing missing.
        let id = control.live_with(None);
        assert_eq!(
            control.state().reason,
            Some(NoiseRemovalReason::Unavailable)
        );
        assert_eq!(
            control.set(NoiseRemoval::Off),
            NoiseRemovalState::running(NoiseRemoval::Off)
        );
        control.detach(id);
        control.set(NoiseRemoval::Standard);
        while rx.try_recv().is_ok() {}

        // A call with the stage: active follows the mode within two blocks.
        let (mut stage, ctl) = stage_for(&control);
        let id = control.live_with(Some(ctl));
        let src = control.source(id);
        assert_eq!(
            src.noise_state(),
            Some(NoiseRemovalState::running(NoiseRemoval::Standard))
        );
        for _ in 0..3 {
            stage.process(&mut [0.0; 480]);
        }
        assert_eq!(
            control.publish(),
            NoiseRemovalState::running(NoiseRemoval::Standard)
        );
        let s = control.set(NoiseRemoval::Off);
        assert_eq!(
            (s.mode, s.active, s.reason),
            (NoiseRemoval::Off, NoiseRemoval::Standard, None),
            "the stage takes it at its next block"
        );
        stage.process(&mut [0.0; 480]);
        assert_eq!(
            control.publish(),
            NoiseRemovalState::running(NoiseRemoval::Off)
        );
        control.set(NoiseRemoval::Standard);
        for _ in 0..2 {
            stage.process(&mut [0.0; 480]);
        }
        assert_eq!(
            control.publish(),
            NoiseRemovalState::running(NoiseRemoval::Standard)
        );

        // The pipeline bypasses the stage: budget, then (another call) fault.
        let flag = Arc::new(FakeBypass::default());
        let weak: Weak<FakeBypass> = Arc::downgrade(&flag);
        control.attach(id, weak);
        flag.0.store(1, Ordering::Relaxed);
        assert_eq!(
            control.publish(),
            NoiseRemovalState::stepped_down(NoiseRemoval::Standard, NoiseRemovalReason::Budget)
        );
        flag.0.store(2, Ordering::Relaxed);
        assert_eq!(
            src.noise_state().unwrap().reason,
            Some(NoiseRemovalReason::Fault)
        );
        assert_eq!(
            control.set(NoiseRemoval::Off),
            NoiseRemovalState::running(NoiseRemoval::Off),
            "Off asked: nothing is missing"
        );
        control.set(NoiseRemoval::Standard);

        // The events: one per change, in order, none for a repeat.
        let mut got = Vec::new();
        while let Ok(s) = rx.try_recv() {
            got.push(s);
        }
        let std_ = NoiseRemovalState::running(NoiseRemoval::Standard);
        let off_pending = NoiseRemovalState::new(NoiseRemoval::Off, NoiseRemoval::Standard, None);
        let off = NoiseRemovalState::running(NoiseRemoval::Off);
        let pending_on = NoiseRemovalState::new(NoiseRemoval::Standard, NoiseRemoval::Off, None);
        let budget =
            NoiseRemovalState::stepped_down(NoiseRemoval::Standard, NoiseRemovalReason::Budget);
        let fault =
            NoiseRemovalState::stepped_down(NoiseRemoval::Standard, NoiseRemovalReason::Fault);
        // With a stage that runs here, the idle state before the call was already Standard
        // running, so the call's start sends nothing.
        let mut want = if STAGE_RUNS { vec![] } else { vec![std_] };
        want.extend([off_pending, off, pending_on, std_, budget, off, fault]);
        assert_eq!(got, want);

        // The call's audio ends: its source reads nothing, and the state is the idle one.
        control.detach(id);
        assert_eq!(src.noise_state(), None);
        assert_eq!(control.state(), idle);
        drop(flag);
    }

    // ── a phone's capture (a fake phone backend) ──

    #[derive(Debug)]
    struct FakePhoneState {
        mode: PhoneAudioMode,
        switches: u64,
        gap: Option<Duration>,
        /// A switch won't open: the backend keeps its mode.
        fail: bool,
        /// Switches wait for `apply` (the control thread hasn't run yet).
        defer: bool,
        waiting: Vec<PhoneStart>,
        calls: Vec<PhoneStart>,
    }

    /// kessel-audio's phone backend as the control sees it.
    struct FakePhone(Mutex<FakePhoneState>);

    impl FakePhone {
        fn new(mode: PhoneAudioMode) -> Arc<FakePhone> {
            Arc::new(FakePhone(Mutex::new(FakePhoneState {
                mode,
                switches: 0,
                gap: None,
                fail: false,
                defer: false,
                waiting: Vec::new(),
                calls: Vec::new(),
            })))
        }

        fn st(&self) -> MutexGuard<'_, FakePhoneState> {
            lock(&self.0)
        }

        fn apply(st: &mut FakePhoneState, start: PhoneStart) {
            if st.fail || start.mode == st.mode {
                return;
            }
            st.mode = start.mode;
            st.switches += 1;
            st.gap = Some(Duration::from_millis(25 + st.switches));
        }

        fn run_waiting(&self) {
            let mut st = self.st();
            for s in std::mem::take(&mut st.waiting) {
                FakePhone::apply(&mut st, s);
            }
        }
    }

    impl LiveAudio for FakePhone {
        fn bypass(&self) -> Option<Bypass> {
            None
        }
        fn capture_mode(&self) -> Option<PhoneAudioMode> {
            Some(self.st().mode)
        }
        fn switch_capture(&self, start: PhoneStart) {
            let mut st = self.st();
            st.calls.push(start);
            if st.defer {
                st.waiting.push(start);
            } else {
                FakePhone::apply(&mut st, start);
            }
        }
        fn capture_switches(&self) -> (u64, Option<Duration>) {
            let st = self.st();
            (st.switches, st.gap)
        }
    }

    fn phone_fields(
        s: NoiseRemovalState,
    ) -> (
        Option<PhoneAudioMode>,
        Option<NoiseRemovalReason>,
        u32,
        Option<u32>,
    ) {
        (
            s.phone_mode,
            s.phone_mode_reason,
            s.phone_mode_switches,
            s.phone_mode_gap_ms,
        )
    }

    /// A phone call starts in the capture its mode asks for (raw for Off, with Android's
    /// unprocessed source only when the app said so), and the state says so; a desktop call
    /// has no phone fields.
    #[test]
    fn a_phone_call_starts_in_the_capture_its_mode_asks() {
        for os in [PhoneOs::Ios, PhoneOs::Android] {
            for mode in NoiseRemoval::ALL {
                for supported in [false, true] {
                    let control = NoiseRemovalControl::new(mode);
                    control.set_android_unprocessed_supported(supported);
                    assert_eq!(control.android_unprocessed_supported(), supported);
                    let (id, start) = control.phone_live_with(os, None);
                    assert_eq!(start, crate::audio::phone_start(os, mode, supported));
                    let raw = mode == NoiseRemoval::Off;
                    assert_eq!(start.mode == PhoneAudioMode::Raw, raw);
                    assert_eq!(
                        start.unprocessed,
                        raw && supported && os == PhoneOs::Android,
                        "{os:?} {mode:?} {supported}"
                    );
                    let phone = FakePhone::new(start.mode);
                    let weak: Weak<FakePhone> = Arc::downgrade(&phone);
                    control.attach(id, weak);
                    assert!(phone.st().calls.is_empty(), "no restart at the start");
                    assert_eq!(
                        phone_fields(control.state()),
                        (Some(start.mode), None, 0, None)
                    );
                    control.detach(id);
                }
            }
        }
        let control = NoiseRemovalControl::new(NoiseRemoval::Off);
        let id = control.live_with(None);
        assert_eq!(phone_fields(control.state()), (None, None, 0, None));
        control.detach(id);
        // No call: on desktop no phone fields either.
        assert_eq!(control.state().phone_mode, None);
    }

    /// Mid-call, Off switches the live phone call to raw at once, and Standard back to the
    /// OS's processing; the stage goes to pass-through in Off; the state and its events carry
    /// the capture, the switches and the last pause.
    #[test]
    fn a_mid_call_change_switches_the_phone_capture_both_ways() {
        for os in [PhoneOs::Android, PhoneOs::Ios] {
            let control = NoiseRemovalControl::new(NoiseRemoval::Standard);
            control.set_android_unprocessed_supported(true);
            let (mut stage, ctl) = stage_for(&control);
            let (id, start) = control.phone_live_with(os, Some(ctl.clone()));
            assert_eq!(start.mode, PhoneAudioMode::Os);
            let phone = FakePhone::new(PhoneAudioMode::Os);
            let weak: Weak<FakePhone> = Arc::downgrade(&phone);
            control.attach(id, weak);
            let mut rx = control.subscribe();

            let s = control.set(NoiseRemoval::Off);
            assert_eq!(ctl.mode(), Mode::Off, "the stage passes the audio through");
            assert_eq!(
                phone.st().calls,
                vec![PhoneStart {
                    mode: PhoneAudioMode::Raw,
                    unprocessed: os == PhoneOs::Android,
                }]
            );
            assert_eq!(
                phone_fields(s),
                (Some(PhoneAudioMode::Raw), None, 1, Some(26))
            );
            stage.process(&mut [0.0; 480]);
            assert_eq!(control.state().active, NoiseRemoval::Off);

            // The same mode again: no second switch.
            control.set(NoiseRemoval::Off);
            assert_eq!(phone.st().calls.len(), 1);

            let s = control.set(NoiseRemoval::Standard);
            assert_eq!(ctl.mode(), Mode::Standard);
            assert_eq!(
                phone.st().calls.last().copied(),
                Some(PhoneStart {
                    mode: PhoneAudioMode::Os,
                    unprocessed: false,
                })
            );
            assert_eq!(
                phone_fields(s),
                (Some(PhoneAudioMode::Os), None, 2, Some(27))
            );

            let mut got = Vec::new();
            while let Ok(s) = rx.try_recv() {
                got.push(s.phone_mode);
            }
            assert!(
                got.contains(&Some(PhoneAudioMode::Raw))
                    && got.last() == Some(&Some(PhoneAudioMode::Os)),
                "{got:?}"
            );
            control.detach(id);
        }
    }

    /// A switch the backend hasn't run yet: the state keeps the capture that runs, with no
    /// reason, until it does.
    #[test]
    fn a_switch_under_way_shows_the_capture_that_runs() {
        let control = NoiseRemovalControl::new(NoiseRemoval::Standard);
        let (id, _) = control.phone_live_with(PhoneOs::Android, None);
        let phone = FakePhone::new(PhoneAudioMode::Os);
        phone.st().defer = true;
        let weak: Weak<FakePhone> = Arc::downgrade(&phone);
        control.attach(id, weak);
        let s = control.set(NoiseRemoval::Off);
        assert_eq!(phone_fields(s), (Some(PhoneAudioMode::Os), None, 0, None));
        phone.run_waiting();
        assert_eq!(
            phone_fields(control.publish()),
            (Some(PhoneAudioMode::Raw), None, 1, Some(26))
        );

        // A switch that lands after the timeout: failed meanwhile, then the state follows what
        // runs, and the next change switches from there.
        let s = control.set(NoiseRemoval::Standard);
        assert_eq!(
            phone_fields(s),
            (Some(PhoneAudioMode::Raw), None, 1, Some(26))
        );
        std::thread::sleep(SWITCH_TIMEOUT + Duration::from_millis(50));
        assert_eq!(
            control.publish().phone_mode_reason,
            Some(NoiseRemovalReason::SwitchFailed)
        );
        phone.run_waiting();
        assert_eq!(
            phone_fields(control.publish()),
            (Some(PhoneAudioMode::Os), None, 2, Some(27))
        );
        control.set(NoiseRemoval::Off);
        assert_eq!(
            phone.st().calls.last().map(|s| s.mode),
            Some(PhoneAudioMode::Raw)
        );
        assert_eq!(phone.st().calls.len(), 3);
        control.detach(id);
    }

    /// A change of the setting between the call's config and its audio running reaches the
    /// phone when the audio is attached (before the session opens the device).
    #[test]
    fn a_change_before_the_audio_runs_switches_on_attach() {
        let control = NoiseRemovalControl::new(NoiseRemoval::Standard);
        let (id, start) = control.phone_live_with(PhoneOs::Ios, None);
        assert_eq!(start.mode, PhoneAudioMode::Os);
        control.set(NoiseRemoval::Off);
        let phone = FakePhone::new(PhoneAudioMode::Os);
        let weak: Weak<FakePhone> = Arc::downgrade(&phone);
        control.attach(id, weak);
        assert_eq!(
            phone.st().calls,
            vec![PhoneStart {
                mode: PhoneAudioMode::Raw,
                unprocessed: false,
            }]
        );
        assert_eq!(control.state().phone_mode, Some(PhoneAudioMode::Raw));
        control.detach(id);
    }

    /// A switch that fails (kessel-audio goes back to the capture it had and reports an
    /// error): the state says `switch_failed` with the capture that runs, until the setting
    /// agrees with it or a later switch works. Without the error event, the switch counts as
    /// failed after the timeout.
    #[test]
    fn a_failed_switch_says_switch_failed() {
        let control = NoiseRemovalControl::new(NoiseRemoval::Standard);
        let (id, _) = control.phone_live_with(PhoneOs::Android, None);
        let phone = FakePhone::new(PhoneAudioMode::Os);
        phone.st().fail = true;
        let weak: Weak<FakePhone> = Arc::downgrade(&phone);
        control.attach(id, weak);
        let mut rx = control.subscribe();

        // The error event (AudioBridge::events): failed at once.
        let s = control.set(NoiseRemoval::Off);
        assert_eq!(phone_fields(s), (Some(PhoneAudioMode::Os), None, 0, None));
        control.audio_error(id);
        let failed = control.state();
        assert_eq!(
            phone_fields(failed),
            (
                Some(PhoneAudioMode::Os),
                Some(NoiseRemovalReason::SwitchFailed),
                0,
                None
            )
        );
        let mut events = Vec::new();
        while let Ok(s) = rx.try_recv() {
            events.push(s);
        }
        assert_eq!(events.last(), Some(&failed), "the failure is an event");

        // Back to Standard: the capture that runs agrees; no switch is asked.
        let s = control.set(NoiseRemoval::Standard);
        assert_eq!(phone_fields(s), (Some(PhoneAudioMode::Os), None, 0, None));
        assert_eq!(phone.st().calls.len(), 1);

        // Off again, and this time it opens.
        phone.st().fail = false;
        let s = control.set(NoiseRemoval::Off);
        assert_eq!(
            phone_fields(s),
            (Some(PhoneAudioMode::Raw), None, 1, Some(26))
        );

        // No error event (someone else read it): failed after the timeout.
        phone.st().fail = true;
        let s = control.set(NoiseRemoval::Standard);
        assert_eq!(s.phone_mode_reason, None, "under way");
        std::thread::sleep(SWITCH_TIMEOUT + Duration::from_millis(50));
        assert_eq!(
            phone_fields(control.publish()),
            (
                Some(PhoneAudioMode::Raw),
                Some(NoiseRemovalReason::SwitchFailed),
                1,
                Some(26)
            )
        );
        // An error for another call, or with nothing under way, changes nothing.
        control.audio_error(id + 1);
        control.audio_error(id);
        assert_eq!(
            control.state().phone_mode_reason,
            Some(NoiseRemovalReason::SwitchFailed)
        );
        control.detach(id);
    }

    // ── against kessel-audio's engine (the null device, faster than real time) ──

    use kessel_audio::{CaptureSource, NullDevice};
    use std::time::{Duration, Instant};

    /// A 440 Hz tone at -20 dBFS.
    fn tone() -> CaptureSource {
        let mut n = 0u64;
        Box::new(move |buf: &mut [f32]| {
            for x in buf.iter_mut() {
                *x = 0.1 * (n as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin();
                n += 1;
            }
        })
    }

    pub(super) fn null_cfg() -> EngineConfig {
        EngineConfig::null(NullDevice {
            block_period: Duration::from_millis(2),
            input: Some(tone()),
            ..NullDevice::default()
        })
    }

    pub(super) fn wait_for(what: &str, mut pred: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if pred() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for: {what}");
    }

    /// The stage goes into kessel-audio's hook with its own noise suppression off, runs, and its
    /// mode changes mid-stream; the state follows kessel-audio's stats.
    #[test]
    fn the_stage_runs_in_kessel_audio() {
        let control = NoiseRemovalControl::new(NoiseRemoval::Standard);
        let (cfg, id) = control.configure(null_cfg());
        assert_eq!(
            cfg.processing.noise_suppression, None,
            "sonora's own NS is off"
        );
        assert!(cfg.processing.capture_stage.is_some());
        assert!(
            cfg.processing.stage_fallback_noise_suppression.is_some(),
            "the step-down floor stays at kessel-audio's default"
        );
        let engine = Arc::new(AudioEngine::start(cfg).unwrap());
        let weak: Weak<AudioEngine> = Arc::downgrade(&engine);
        control.attach(id, weak);
        let frames = engine.capture_frames();
        wait_for("the stage has run 200 blocks", || {
            engine
                .stats()
                .capture_stage
                .is_some_and(|s| s.blocks_processed >= 200)
        });
        let st = engine.stats().capture_stage.unwrap();
        assert!(st.active && st.bypassed.is_none(), "{st:?}");
        assert_eq!(st.latency_samples, 480);
        assert_eq!(
            control.publish(),
            NoiseRemovalState::running(NoiseRemoval::Standard)
        );
        control.set(NoiseRemoval::Off);
        wait_for("Off runs", || {
            control.state() == NoiseRemovalState::running(NoiseRemoval::Off)
        });
        control.set(NoiseRemoval::Standard);
        wait_for("Standard runs", || {
            control.state() == NoiseRemovalState::running(NoiseRemoval::Standard)
        });
        let mut got = 0;
        while frames.try_recv().is_some() {
            got += 1;
        }
        // The queue holds 50 frames (kessel-audio's default); it filled while the test waited.
        assert!(got >= 40, "frames flow ({got})");
        let st = engine.stats().capture_stage.unwrap();
        assert!(
            st.active && st.bypassed.is_none() && st.strikes == 0,
            "{st:?}"
        );
        engine.shutdown();
        control.detach(id);
    }

    /// A stage that sleeps 10 ms per block (a strike on the second call, and on every second
    /// one after a retry) or answers NaN (a fault).
    struct Bad {
        nan: bool,
    }

    impl CaptureStage for Bad {
        fn process(&mut self, block: &mut [f32; 480]) {
            if self.nan {
                block[7] = f32::NAN;
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn latency_samples(&self) -> usize {
            480
        }
        fn reset(&mut self) {}
    }

    /// kessel-audio bypassing the stage reads as Standard asked, nothing of ours running, with
    /// kessel-audio's reason; capture goes on.
    #[test]
    fn a_bypass_in_kessel_audio_steps_the_state_down() {
        for (nan, reason) in [
            (false, NoiseRemovalReason::Budget),
            (true, NoiseRemovalReason::Fault),
        ] {
            let control = NoiseRemovalControl::new(NoiseRemoval::Standard);
            let mut cfg = null_cfg();
            assert!(hook::install(&mut cfg.processing, Box::new(Bad { nan })));
            let id = control.live_with(Some(Stage::new().control()));
            let engine = Arc::new(AudioEngine::start(cfg).unwrap());
            let weak: Weak<AudioEngine> = Arc::downgrade(&engine);
            control.attach(id, weak);
            let mut rx = control.subscribe();
            wait_for("the bypass", || control.publish().reason == Some(reason));
            assert_eq!(
                control.state(),
                NoiseRemovalState::stepped_down(NoiseRemoval::Standard, reason)
            );
            let mut events = Vec::new();
            while let Ok(s) = rx.try_recv() {
                events.push(s);
            }
            assert_eq!(
                events.last(),
                Some(&NoiseRemovalState::stepped_down(
                    NoiseRemoval::Standard,
                    reason
                ))
            );
            let st = engine.stats().capture_stage.unwrap();
            assert!(!st.active, "{st:?}");
            if nan {
                assert!(st.permanent, "a fault is for good: {st:?}");
            }
            assert!(
                st.fallback_noise_suppression,
                "kessel-audio's own suppression stands in: {st:?}"
            );
            let frames = engine.capture_frames();
            while frames.try_recv().is_some() {}
            wait_for("capture goes on", || frames.try_recv().is_some());
            engine.shutdown();
        }
    }

    /// The call's events open its stage's grace window: the session activating or resuming
    /// and a route change (through the bridge's session sink, before kessel-audio hears it),
    /// a device switch, a camera starting (the call's noise source) and the app's scene
    /// changing. A suspend or a deactivation does not, nor another call's events, nor anything
    /// without a call.
    #[test]
    fn the_calls_events_open_grace() {
        let control = NoiseRemovalControl::new(NoiseRemoval::Standard);
        let stage = control.build_stage();
        let ctl = stage.control();
        ctl.set_auto_grace(Duration::ZERO);
        let id = control.live_with(Some(ctl.clone()));
        let session = crate::AudioSession::new();
        let sink = Arc::new(crate::audio::BridgeSink {
            engine: Weak::new(),
            noise: Some((control.clone(), id)),
        });
        let weak: Weak<crate::audio::BridgeSink> = Arc::downgrade(&sink);
        session.attach(weak, false);
        assert!(!ctl.in_grace());
        let opens = |what: &str, f: &dyn Fn()| {
            f();
            assert!(ctl.in_grace(), "{what} opens a grace window");
            assert!(ctl.stats().grace_until_block >= 300, "{what}: about 3 s");
            ctl.end_grace();
        };
        let keeps_closed = |what: &str, f: &dyn Fn()| {
            f();
            assert!(!ctl.in_grace(), "{what} opens no grace window");
        };
        use crate::SessionEvent::{Activated, Deactivated, Resume, RouteChanged, Suspend};
        opens("activated", &|| session.event(Activated));
        keeps_closed("suspend", &|| session.event(Suspend));
        opens("resume", &|| session.event(Resume));
        opens("a route change", &|| session.event(RouteChanged));
        keeps_closed("deactivated", &|| session.event(Deactivated));
        opens("a device switch", &|| control.devices_changed(id));
        keeps_closed("another call's device switch", &|| {
            control.devices_changed(id + 1)
        });
        opens("a camera starting", &|| control.source(id).grace());
        opens("the app's scene", &|| control.app_scene_changed());
        control.detach(id);
        control.app_scene_changed();
        assert!(!ctl.in_grace(), "no call: nothing");
    }
}

/// DeepFilterNet3 in the control, on the repository's copy of the weights.
#[cfg(all(test, feature = "dfn3"))]
mod dfn3_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]
    use super::*;

    /// The weights in Sew's repository. Outside it (the published crate, a mirror) there is no
    /// `assets/dfn3/`: the test says so loudly and skips. In the repository a missing file fails.
    fn weights() -> Option<std::path::PathBuf> {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/dfn3");
        if !dir.is_dir() {
            eprintln!(
                "SKIPPED: DeepFilterNet3 weights are not in this tree ({} is absent): the DeepFilterNet3 tests of sew-call-client did not run",
                dir.display()
            );
            return None;
        }
        Some(dir.join(sew_noise::dfn3::WEIGHTS_FILE))
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sew-call-dfn3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn run(stage: &mut NoiseStage, blocks: usize) {
        for i in 0..blocks {
            let mut block = [0.0f32; 480];
            for (j, s) in block.iter_mut().enumerate() {
                *s = 0.05 * ((i * 480 + j) as f32 * 0.031).sin();
            }
            stage.process(&mut block);
        }
    }

    /// Standard runs DeepFilterNet3 once its weights are loaded; a step-down shows as RNNoise
    /// for `budget`; Off shows no model.
    #[test]
    fn standard_runs_dfn3_and_reports_step_downs() {
        let Some(weights) = weights() else { return };
        let control = NoiseRemovalControl::new(NoiseRemoval::Standard);
        assert_eq!(control.state().model, None, "no weights given: as before");
        control.set_dfn3_weights(&weights).unwrap();
        assert!(!control.dfn3_loaded(), "given, not loaded");
        assert!(control.dfn3_load_info().is_none());
        control.preload_dfn3().unwrap();
        assert!(control.dfn3_loaded());
        assert!(control.dfn3_load_info().is_some());
        assert!(
            control.stage_runs(),
            "with the model a stage runs on every platform"
        );
        let idle = control.state();
        assert_eq!(
            (idle.active, idle.model, idle.model_reason),
            (NoiseRemoval::Standard, Some(NoiseModel::Dfn3), None)
        );
        let stage = control.build_stage();
        let ctl = stage.control();
        assert!(ctl.strong_available());
        assert_eq!(
            ctl.mode(),
            Mode::Strong,
            "Standard runs Strong on a stage with the model"
        );
        let mut stage = NoiseStage::new(stage);
        ctl.set_step_down_rules(false);
        let id = control.live_with(Some(ctl.clone()));
        run(&mut stage, 5);
        let s = control.state();
        assert_eq!(
            (s.active, s.model, s.model_reason, s.step_downs),
            (NoiseRemoval::Standard, Some(NoiseModel::Dfn3), None, 0)
        );
        ctl.force_step_down();
        run(&mut stage, 4);
        let s = control.state();
        assert_eq!(
            (s.active, s.model, s.model_reason, s.step_downs),
            (
                NoiseRemoval::Standard,
                Some(NoiseModel::Rnnoise),
                Some(NoiseRemovalReason::Budget),
                1
            )
        );
        assert_eq!(s.reason, None, "Standard still runs");
        control.set(NoiseRemoval::Off);
        assert_eq!(ctl.mode(), Mode::Off);
        run(&mut stage, 3);
        let s = control.state();
        assert_eq!((s.active, s.model), (NoiseRemoval::Off, None));
        control.set(NoiseRemoval::Standard);
        assert_eq!(ctl.mode(), Mode::Strong, "mid-call, back to Strong");
        control.detach(id);
    }

    /// The fallback: a missing or altered weights file is refused, Standard stays RNNoise and
    /// says why, and the call's stage is RNNoise's.
    #[test]
    fn missing_or_altered_weights_fall_back_to_rnnoise() {
        let Some(weights) = weights() else { return };
        let control = NoiseRemovalControl::new(NoiseRemoval::Standard);
        assert!(matches!(
            control.set_dfn3_weights(scratch("absent.tar")),
            Err(sew_noise::dfn3::LoadError::Io(_))
        ));
        let mut bytes = std::fs::read(&weights).unwrap();
        let mid = bytes.len() / 3;
        bytes[mid] ^= 0x40;
        let altered = scratch("altered.tar");
        std::fs::write(&altered, &bytes).unwrap();
        assert!(matches!(
            control.set_dfn3_weights(&altered),
            Err(sew_noise::dfn3::LoadError::Mismatch { .. })
        ));
        let _ = std::fs::remove_file(&altered);
        assert!(!control.dfn3_loaded());
        assert_eq!(control.stage_runs(), STAGE_RUNS);
        if STAGE_RUNS {
            let s = control.state();
            assert_eq!(
                (s.active, s.model, s.model_reason),
                (
                    NoiseRemoval::Standard,
                    Some(NoiseModel::Rnnoise),
                    Some(NoiseRemovalReason::Unavailable)
                )
            );
        }
        let stage = control.build_stage();
        let ctl = stage.control();
        assert!(!ctl.strong_available());
        assert_eq!(ctl.mode(), Mode::Standard);
        let mut stage = NoiseStage::new(stage);
        let id = control.live_with(Some(ctl));
        run(&mut stage, 4);
        let s = control.state();
        assert_eq!(
            (s.active, s.model, s.model_reason),
            (
                NoiseRemoval::Standard,
                Some(NoiseModel::Rnnoise),
                Some(NoiseRemovalReason::Unavailable)
            )
        );
        control.detach(id);
        // A later good path is taken (and loads when asked).
        control.set_dfn3_weights(&weights).unwrap();
        assert!(!control.dfn3_loaded());
        control.preload_dfn3().unwrap();
        assert!(control.dfn3_loaded());
    }

    // ── loaded on first need, released when idle (kessel-audio's null device) ──

    use super::tests::{null_cfg, wait_for};
    use std::time::Duration;

    /// A call's audio on kessel-audio's null device with the control's stage: (engine, id).
    fn call(control: &NoiseRemovalControl) -> (Arc<AudioEngine>, u64) {
        let (cfg, id) = control.configure(null_cfg());
        let engine = Arc::new(AudioEngine::start(cfg).unwrap());
        let weak: Weak<AudioEngine> = Arc::downgrade(&engine);
        control.attach(id, weak);
        (engine, id)
    }

    fn end(control: &NoiseRemovalControl, engine: &AudioEngine, id: u64) {
        engine.shutdown();
        control.detach(id);
    }

    fn model_of(s: NoiseRemovalState) -> (Option<NoiseModel>, Option<NoiseRemovalReason>) {
        (s.model, s.model_reason)
    }

    /// The call's stage has DeepFilterNet3 and ran it: it runs, or it stepped down for time on
    /// a busy test machine (the step-down is the stage's own rule, not the handover's).
    fn stepped_up(s: NoiseRemovalState) -> bool {
        s.model == Some(NoiseModel::Dfn3) || s.step_downs > 0
    }

    /// Giving the weights loads nothing; the first call in Standard starts on RNNoise with
    /// `loading`, steps up to DeepFilterNet3 once the model is in, and the pipeline never
    /// bypasses the stage for it. After the idle time with no call the model is released, and
    /// the next call loads it again.
    #[test]
    fn the_model_loads_on_the_first_call_and_goes_when_idle() {
        let Some(weights) = weights() else { return };
        let control = NoiseRemovalControl::new(NoiseRemoval::Standard);
        control.set_dfn3_idle_unload(Duration::from_millis(200));
        control.set_dfn3_weights(&weights).unwrap();
        assert!(!control.dfn3_loaded(), "no load when the weights are given");
        assert_eq!(control.dfn3_load_info(), None);
        assert_eq!(
            model_of(control.state()),
            (Some(NoiseModel::Dfn3), None),
            "what a call would get"
        );

        let (engine, id) = call(&control);
        let first = control.state();
        assert_eq!(
            first.active,
            NoiseRemoval::Standard,
            "RNNoise from the start"
        );
        assert_eq!(
            model_of(first),
            (Some(NoiseModel::Rnnoise), Some(NoiseRemovalReason::Loading))
        );
        wait_for("DeepFilterNet3 runs", || {
            stepped_up(control.publish())
                && engine
                    .stats()
                    .capture_stage
                    .is_some_and(|s| s.blocks_processed > 0)
        });
        assert!(control.dfn3_loaded());
        let info = control.dfn3_load_info().expect("a load ran");
        eprintln!(
            "noise-dfn3-lazy: load {} ms, RSS {:?} -> {:?} KiB",
            info.load_ms, info.rss_before_kib, info.rss_after_kib
        );
        // A little more audio through the stepped-up stage: no fault.
        std::thread::sleep(Duration::from_millis(300));
        let st = engine.stats().capture_stage.unwrap();
        assert!(!st.permanent, "{st:?}");
        assert_eq!(control.state().mode, NoiseRemoval::Standard);
        end(&control, &engine, id);
        drop(engine);

        assert!(control.dfn3_loaded(), "kept for the idle time");
        wait_for("released when idle", || !control.dfn3_loaded());

        let (engine, id) = call(&control);
        wait_for("loaded again for the next call", || {
            control.dfn3_loaded() && stepped_up(control.publish())
        });
        end(&control, &engine, id);
    }

    /// A call that starts in Off loads nothing; its first switch to Standard loads the model
    /// and hands it to the live call. A call starting within the idle time keeps the model.
    #[test]
    fn the_first_switch_to_standard_loads_it() {
        let Some(weights) = weights() else { return };
        let control = NoiseRemovalControl::new(NoiseRemoval::Off);
        control.set_dfn3_idle_unload(Duration::from_secs(60));
        control.set_dfn3_weights(&weights).unwrap();
        let (engine, id) = call(&control);
        std::thread::sleep(Duration::from_millis(200));
        assert!(!control.dfn3_loaded(), "Off needs no model");
        control.set(NoiseRemoval::Standard);
        wait_for("DeepFilterNet3 runs after the switch", || {
            stepped_up(control.publish())
        });
        end(&control, &engine, id);
        let (engine, id) = call(&control);
        assert!(control.dfn3_loaded(), "within the idle time: kept");
        assert_eq!(
            control.state().model,
            Some(NoiseModel::Dfn3),
            "the call's stage is built with the model"
        );
        end(&control, &engine, id);
    }

    /// The weights checked out when given, but the load fails (the file changed since): the
    /// call stays on RNNoise, and the state says `unavailable`.
    #[test]
    fn a_failed_load_stays_on_rnnoise() {
        let Some(weights) = weights() else { return };
        let copy = scratch("lazy-weights.tar");
        std::fs::copy(&weights, &copy).unwrap();
        let control = NoiseRemovalControl::new(NoiseRemoval::Standard);
        control.set_dfn3_weights(&copy).unwrap();
        let mut bytes = std::fs::read(&copy).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x10;
        std::fs::write(&copy, &bytes).unwrap();
        let (engine, id) = call(&control);
        wait_for("the load fails", || {
            model_of(control.publish())
                == (
                    Some(NoiseModel::Rnnoise),
                    Some(NoiseRemovalReason::Unavailable),
                )
        });
        assert!(!control.dfn3_loaded());
        let s = control.state();
        assert_eq!(
            (s.mode, s.active, s.reason),
            (NoiseRemoval::Standard, NoiseRemoval::Standard, None)
        );
        end(&control, &engine, id);
        let _ = std::fs::remove_file(&copy);
    }

    // ── grace (an iPhone field log: the start-up's load stepped DeepFilterNet3 down) ──

    /// A clock whose every reading is `STEP` ns after the one before: each block takes `STEP`
    /// (the stage reads the clock twice a block). Only the test below uses it.
    static NOW: AtomicU64 = AtomicU64::new(0);
    static STEP: AtomicU64 = AtomicU64::new(1_000_000);

    fn clock() -> u64 {
        NOW.fetch_add(STEP.load(Ordering::Relaxed), Ordering::Relaxed)
    }

    const MS: u64 = 1_000_000;

    /// The call's blocks: a start-up (one block of 8 ms, then 100 of 5 ms, over the stage's
    /// single-block, percentile and median lines), quiet blocks, then at 4 s, just after the
    /// audio route changed, a 7 ms block and 50 of 5 ms; quiet again.
    fn block_time(i: u64) -> u64 {
        match i {
            0 => 8 * MS,
            1..=100 => 5 * MS,
            410 => 7 * MS,
            411..=460 => 5 * MS,
            _ => MS,
        }
    }

    /// A call's stage over DeepFilterNet3 with the test clock, live in `control`, its session
    /// sink attached to `session` as the bridge attaches it.
    fn timed_call(
        control: &NoiseRemovalControl,
        session: &crate::AudioSession,
    ) -> (NoiseStage, StageControl, u64, Arc<crate::audio::BridgeSink>) {
        let mut stage = control.build_stage();
        stage.set_clock(clock);
        let ctl = stage.control();
        assert!(ctl.strong_available());
        let id = control.live_with(Some(ctl.clone()));
        let sink = Arc::new(crate::audio::BridgeSink {
            engine: Weak::new(),
            noise: Some((control.clone(), id)),
        });
        let weak: Weak<crate::audio::BridgeSink> = Arc::downgrade(&sink);
        session.attach(weak, false);
        (NoiseStage::new(stage), ctl, id, sink)
    }

    fn block(stage: &mut NoiseStage, i: u64) {
        STEP.store(block_time(i), Ordering::Relaxed);
        let mut b = [0.0f32; 480];
        for (j, s) in b.iter_mut().enumerate() {
            *s = 0.05 * ((i as usize * 480 + j) as f32 * 0.031).sin();
        }
        stage.process(&mut b);
    }

    /// The field log's shape: slow blocks at the start of a call and again just after an audio
    /// route change. The call's stage keeps DeepFilterNet3 through both (the stage's own start
    /// window, then the one the route change opens through the session); the state says
    /// `in_grace` while a window is open and carries the stage's timing. Without the route
    /// change the same blocks step it down, and the state says why and when.
    #[test]
    fn start_up_and_a_route_change_keep_dfn3() {
        let Some(weights) = weights() else { return };
        let control = NoiseRemovalControl::new(NoiseRemoval::Standard);
        control.set_dfn3_weights(&weights).unwrap();
        control.preload_dfn3().unwrap();

        let session = crate::AudioSession::new();
        let (mut stage, ctl, id, _sink) = timed_call(&control, &session);
        let mut events = control.subscribe();
        stage.reset();
        for i in 0..400 {
            block(&mut stage, i);
            if i == 150 {
                assert!(control.state().in_grace, "the start's window");
            }
        }
        let s = control.state();
        assert_eq!(
            (s.active, s.model, s.step_downs, s.in_grace),
            (NoiseRemoval::Standard, Some(NoiseModel::Dfn3), 0, false),
            "the start-up kept DeepFilterNet3: {s:?}"
        );
        assert!(s.slowest_block_us.is_some_and(|us| us >= 8_000), "{s:?}");
        assert!(s.blocks_over_budget >= 1, "{s:?}");
        assert_eq!(s.step_down_reason, None);
        session.event(crate::SessionEvent::RouteChanged);
        assert!(control.state().in_grace, "the route change's window");
        let sent: Vec<NoiseRemovalState> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        assert!(
            sent.iter().any(|s| s.in_grace),
            "the window's opening is sent at once: {sent:?}"
        );
        for i in 400..900 {
            block(&mut stage, i);
        }
        let s = control.state();
        assert_eq!(
            (s.active, s.model, s.step_downs, s.in_grace),
            (NoiseRemoval::Standard, Some(NoiseModel::Dfn3), 0, false),
            "the route change's slow blocks kept DeepFilterNet3: {s:?}"
        );
        assert_eq!(ctl.active(), Mode::Strong);
        let json = serde_json::to_value(s).unwrap();
        assert!(json.get("in_grace").is_none(), "{json}");
        assert!(json.get("slowest_block_us").is_some(), "{json}");
        control.detach(id);

        // The same blocks without the route change's window: the 7 ms block steps down.
        let session = crate::AudioSession::new();
        let (mut stage, _ctl, id, _sink) = timed_call(&control, &session);
        stage.reset();
        for i in 0..500 {
            block(&mut stage, i);
        }
        let s = control.state();
        assert_eq!(
            (s.model, s.model_reason, s.step_downs, s.step_down_reason),
            (
                Some(NoiseModel::Rnnoise),
                Some(NoiseRemovalReason::Budget),
                1,
                Some(NoiseStepDownReason::SlowBlock)
            ),
            "{s:?}"
        );
        assert_eq!(
            s.last_step_down_ms_ago,
            Some(900),
            "at block 410, read after 500"
        );
        let json = serde_json::to_value(s).unwrap();
        assert_eq!(json["step_down_reason"], "slow_block");
        assert_eq!(json["last_step_down_ms_ago"], 900);
        control.detach(id);
    }
}
