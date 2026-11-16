//! [`NoiseRemovalControl`]: the setting, the stage it builds for each call's audio, and the
//! state and its changes.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use kessel_audio::{AudioEngine, EngineConfig};
use sew_noise::{Mode, Stage, StageControl, warm_up_thread};
use tokio::sync::broadcast;

use super::hook::{self, Bypass, CaptureStage};
use super::{NoiseModel, NoiseRemoval, NoiseRemovalReason, NoiseRemovalState, NoiseSource};

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
    /// Loaded: Standard runs DeepFilterNet3.
    Loaded(Arc<sew_noise::dfn3::Dfn3Model>),
    /// Given but refused (missing, unreadable, not the pinned file): RNNoise.
    Failed,
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

/// The stage's mode for a setting: Standard is Strong when the stage was built with
/// DeepFilterNet3, Standard (RNNoise) otherwise.
fn stage_mode(mode: NoiseRemoval, strong: bool) -> Mode {
    match mode {
        NoiseRemoval::Off => Mode::Off,
        NoiseRemoval::Standard if strong => Mode::Strong,
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

/// Where the pipeline's bypass flag is read: kessel-audio's engine, or a test's.
pub(crate) trait BypassSource: Send + Sync {
    fn bypass(&self) -> Option<Bypass>;
}

impl BypassSource for AudioEngine {
    fn bypass(&self) -> Option<Bypass> {
        hook::bypass(self)
    }
}

/// One call's audio: the stage handed over (`None`: none could be) and the pipeline's bypass
/// flag.
struct Live {
    id: u64,
    stage: Option<StageControl>,
    bypass: Option<Weak<dyn BypassSource>>,
}

struct Inner {
    mode: AtomicU8,
    next_id: AtomicU64,
    live: Mutex<Option<Live>>,
    /// The state last published.
    last: Mutex<Option<NoiseRemovalState>>,
    events: broadcast::Sender<NoiseRemovalState>,
    #[cfg(feature = "dfn3")]
    dfn3: Mutex<(Dfn3Slot, Option<Dfn3LoadInfo>)>,
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
                next_id: AtomicU64::new(0),
                live: Mutex::new(None),
                last: Mutex::new(None),
                events,
                #[cfg(feature = "dfn3")]
                dfn3: Mutex::new((Dfn3Slot::NotGiven, None)),
            }),
        }
    }

    /// Loads DeepFilterNet3's weights from `path` (feature `dfn3`): the file the app ships as
    /// a resource (`sew_noise::dfn3::WEIGHTS_FILE`), checked against its pinned sha256 before
    /// anything is read. From then on [`NoiseRemoval::Standard`] runs DeepFilterNet3 in each
    /// new call, with RNNoise beside it as the step-down (a live call keeps the stage it
    /// started with).
    ///
    /// Slow (tens of milliseconds, about 40 MB): call it once at start, off the UI and audio
    /// threads. On an error (a missing or unreadable file, or not the pinned weights) Standard
    /// stays RNNoise and the state says `model_reason: unavailable`; a later call may try
    /// again.
    #[cfg(feature = "dfn3")]
    pub fn set_dfn3_weights(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<(), sew_noise::dfn3::LoadError> {
        let before = super::rss_kib();
        let started = std::time::Instant::now();
        let loaded = sew_noise::dfn3::Dfn3Model::load(path);
        let info = Dfn3LoadInfo {
            rss_before_kib: before,
            rss_after_kib: super::rss_kib(),
            load_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        let out = {
            let mut slot = lock(&self.inner.dfn3);
            match loaded {
                Ok(model) => {
                    *slot = (Dfn3Slot::Loaded(model), Some(info));
                    Ok(())
                }
                Err(e) => {
                    tracing::warn!(error = %e, "noise removal: DeepFilterNet3 not loaded; Standard stays RNNoise");
                    *slot = (Dfn3Slot::Failed, Some(info));
                    Err(e)
                }
            }
        };
        self.publish();
        out
    }

    /// Whether DeepFilterNet3's weights are loaded ([`Self::set_dfn3_weights`]).
    #[cfg(feature = "dfn3")]
    pub fn dfn3_loaded(&self) -> bool {
        matches!(lock(&self.inner.dfn3).0, Dfn3Slot::Loaded(_))
    }

    /// What the last [`Self::set_dfn3_weights`] cost, if it was called.
    #[cfg(feature = "dfn3")]
    pub fn dfn3_load_info(&self) -> Option<Dfn3LoadInfo> {
        lock(&self.inner.dfn3).1
    }

    /// The loaded model, if any.
    #[cfg(feature = "dfn3")]
    fn model(&self) -> Option<Arc<sew_noise::dfn3::Dfn3Model>> {
        match &lock(&self.inner.dfn3).0 {
            Dfn3Slot::Loaded(m) => Some(Arc::clone(m)),
            _ => None,
        }
    }

    /// The device bench's view of the weights: loaded, and (RSS before, RSS after, ms) of the
    /// last load.
    #[cfg(feature = "noise-bench")]
    pub(crate) fn dfn3_info(&self) -> (bool, Option<LoadFigures>) {
        #[cfg(feature = "dfn3")]
        {
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

    /// The app gave weights (loaded or not): the state carries the model fields.
    fn dfn3_given(&self) -> bool {
        #[cfg(feature = "dfn3")]
        {
            !matches!(lock(&self.inner.dfn3).0, Dfn3Slot::NotGiven)
        }
        #[cfg(not(feature = "dfn3"))]
        {
            false
        }
    }

    fn has_model(&self) -> bool {
        #[cfg(feature = "dfn3")]
        {
            self.dfn3_loaded()
        }
        #[cfg(not(feature = "dfn3"))]
        {
            false
        }
    }

    /// A stage can be handed to this build's pipeline: kessel-audio has the hook and either
    /// the capture path is not a phone's OS voice processing, or DeepFilterNet3 is loaded (it
    /// runs after the OS's processing there).
    pub(crate) fn stage_runs(&self) -> bool {
        hook::HOOKED && (!crate::audio::PHONE_AUDIO || self.has_model())
    }

    /// The stage a call's audio starts with, in the current mode: over DeepFilterNet3 when its
    /// weights are loaded, RNNoise otherwise.
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
            // No call yet (or one without a stage): what a call would start with.
            return if self.has_model() {
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
                Some(NoiseRemovalReason::Unavailable),
                step_downs,
            );
        }
        if st.blocks == 0 || stage.active() == Mode::Strong {
            return state.with_model(Some(NoiseModel::Dfn3), None, step_downs);
        }
        let why = match st.last_step_down {
            Some(sew_noise::StepDownReason::Fault) => Some(NoiseRemovalReason::Fault),
            Some(_) => Some(NoiseRemovalReason::Budget),
            // Warming up (the call's first blocks): no step-down.
            None => None,
        };
        state.with_model(Some(NoiseModel::Rnnoise), why, step_downs)
    }

    /// The mode asked for.
    pub fn mode(&self) -> NoiseRemoval {
        from_code(self.inner.mode.load(Ordering::Acquire))
    }

    /// Asks for `mode`, for the live call (from its next block) and later ones; answers the
    /// state now. A change of state is also sent to [`NoiseRemovalControl::subscribe`]'s
    /// receivers.
    pub fn set(&self, mode: NoiseRemoval) -> NoiseRemovalState {
        {
            // Under the live lock, so a call's audio starting now builds its stage in this
            // mode or gets it here.
            let live = lock(&self.inner.live);
            self.inner.mode.store(code(mode), Ordering::Release);
            if let Some(stage) = live.as_ref().and_then(|l| l.stage.as_ref()) {
                // Off and Standard are always available; Strong when the stage has the model.
                let _ = stage.set_mode(stage_mode(mode, stage.strong_available()));
            }
        }
        self.publish()
    }

    /// The setting and what runs now.
    pub fn state(&self) -> NoiseRemovalState {
        let mode = self.mode();
        let live = lock(&self.inner.live);
        let Some(live) = live.as_ref() else {
            // No call: what a call would start with.
            return if self.stage_runs() {
                self.with_model(NoiseRemovalState::running(mode), None)
            } else {
                NoiseRemovalState::stepped_down(mode, NoiseRemovalReason::Unavailable)
            };
        };
        let Some(stage) = &live.stage else {
            return NoiseRemovalState::stepped_down(mode, NoiseRemovalReason::Unavailable);
        };
        if let Some(b) = live
            .bypass
            .as_ref()
            .and_then(Weak::upgrade)
            .and_then(|s| s.bypass())
        {
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
        let mut last = lock(&self.inner.last);
        if *last != Some(state) {
            *last = Some(state);
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
        let id = self.go_live(&mut live, stage);
        drop(live);
        // Phones: the OS's own noise suppression follows the mode the call starts in.
        crate::audio::apply_phone_mode(&mut cfg, self.mode());
        self.publish();
        (cfg, id)
    }

    fn go_live(&self, live: &mut Option<Live>, stage: Option<StageControl>) -> u64 {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        *live = Some(Live {
            id,
            stage,
            bypass: None,
        });
        id
    }

    /// The call `id`'s pipeline is running: its bypass flag is read from `source`.
    pub(crate) fn attach(&self, id: u64, source: Weak<dyn BypassSource>) {
        if let Some(l) = lock(&self.inner.live).as_mut().filter(|l| l.id == id) {
            l.bypass = Some(source);
        }
    }

    /// The call `id`'s audio ended (or never started).
    pub(crate) fn detach(&self, id: u64) {
        let mut live = lock(&self.inner.live);
        if live.as_ref().is_some_and(|l| l.id == id) {
            *live = None;
            drop(live);
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
        let id = self.go_live(&mut live, stage);
        drop(live);
        self.publish();
        id
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

    impl BypassSource for FakeBypass {
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

    fn null_cfg() -> EngineConfig {
        EngineConfig::null(NullDevice {
            block_period: Duration::from_millis(2),
            input: Some(tone()),
            ..NullDevice::default()
        })
    }

    fn wait_for(what: &str, mut pred: impl FnMut() -> bool) {
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
        // A later good path loads.
        control.set_dfn3_weights(&weights).unwrap();
        assert!(control.dfn3_loaded());
    }
}
