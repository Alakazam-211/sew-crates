//! The kessel-audio bridge (feature `audio-core`; `audio` adds the system devices): the
//! capture path's `EncodedFrame`s go to the call's send queue (the engine encrypts them), and
//! the call's decrypted, verified frames go to kessel-audio's `push` / `lost`, with
//! `add_participant` first, `remove_participant` when a track ends, and the call's mute and
//! deafen as `set_capture` / `set_deafened`.
//!
//! kessel-audio never sees a key or a ciphertext; the engine never touches PCM.
//!
//! **Phones** (iOS and Android with feature `mobile`): [`platform_audio_config`] picks
//! kessel-audio's OS voice processing (iOS's VoiceProcessingIO unit; Android's AAudio with the
//! voice-communication preset), with the OS's echo cancellation and noise suppression on. The
//! phone's audio session ([`SessionEvent`]: CallKit's `didActivate` / `didDeactivate`, an
//! interruption, a route change) reaches kessel-audio through an [`AudioSession`]. On iOS the
//! engine starts deferred (kessel-audio's `start_when_session_active`): the microphone and the
//! speaker open only once the session is active. The engine never configures or activates the
//! OS's audio session (`AVAudioSession`, the Android audio mode): that is the app's job.
//! Desktop keeps cpal, and there the session events change nothing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

#[cfg(feature = "audio")]
use kessel_audio::DeviceChoice;
use kessel_audio::{AudioEngine, EngineConfig, Level, RecvFrame};
use tokio::task::JoinHandle;

use crate::call::{Call, FrameSender, FrameStream, OpusFrame, RecvItem};
use crate::noise::{NoiseRemoval, NoiseRemovalControl, NoiseRemovalState};
use crate::stats::{PlayoutStats, PlayoutTotals};

/// How often the bridge moves frames (well under one 20 ms frame).
const TICK: Duration = Duration::from_millis(5);
/// The noise removal state is checked every this many ticks (100 ms).
const NOISE_TICKS: u32 = 20;

/// Why the bridge could not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AudioBridgeError {
    /// The call's frame stream was already taken.
    #[error("the call's frames are already taken")]
    FramesTaken,
    /// kessel-audio did not start.
    #[error("audio: {0}")]
    Audio(#[from] kessel_audio::AudioError),
}

/// The audio backend [`platform_audio_config`] picks in this build: `"os-voice"` (iOS and
/// Android with feature `mobile`: the OS's own voice processing) or `"system"` (cpal).
pub const AUDIO_BACKEND: &str = if PHONE_AUDIO { "os-voice" } else { "system" };

/// This build drives kessel-audio's phone backend (iOS or Android, feature `mobile`).
pub(crate) const PHONE_AUDIO: bool = cfg!(all(
    feature = "mobile",
    any(target_os = "ios", target_os = "android")
));

/// The kessel-audio config for a call on this platform's audio.
///
/// - **iOS and Android** (feature `mobile`): `Backend::OsVoiceProcessing`, with the OS's echo
///   cancellation, noise suppression and gain control on (kessel-audio's own processing then
///   runs none of them). On Android, a call whose audio starts in [`NoiseRemoval::Off`]
///   ([`AudioBridge::start_with_noise`]) has the OS's noise suppression off; on iOS the OS's
///   voice processing keeps its own noise suppression. `input` and `output` are ignored: the OS owns the route. Start it
///   with [`AudioBridge::start_with_session`] and report the phone's audio session to the
///   [`AudioSession`]; on iOS nothing opens until [`SessionEvent::Activated`].
/// - **Desktop:** the system's devices through cpal (`Backend::System { input, output }`).
///
/// - **Noise removal** (desktop): Standard, a `sew-noise` stage in kessel-audio's capture hook
///   between echo cancellation and gain control, with kessel-audio's own noise suppression off.
///   To change it or read its state, start the call's audio with
///   [`AudioBridge::start_with_noise`] (which builds its own stage from the
///   [`NoiseRemovalControl`]). While kessel-audio bypasses the stage (over its time budget, or
///   a fault) its own noise suppression stands in. A frame's `ts48k` does not include the
///   stage's 10 ms: its audio is 480 samples older than its stamp.
///
/// Everything else is kessel-audio's default.
#[cfg(feature = "audio")]
pub fn platform_audio_config(input: DeviceChoice, output: DeviceChoice) -> EngineConfig {
    let cfg = platform_devices(input, output);
    NoiseRemovalControl::default().configure(cfg).0
}

#[cfg(feature = "audio")]
pub(crate) fn platform_devices(input: DeviceChoice, output: DeviceChoice) -> EngineConfig {
    #[cfg(all(feature = "mobile", any(target_os = "ios", target_os = "android")))]
    {
        let _ = (input, output);
        EngineConfig::default().with_backend(kessel_audio::Backend::OsVoiceProcessing(
            kessel_audio::OsVoiceConfig {
                os_effects: kessel_audio::OsEffects {
                    echo_cancellation: true,
                    noise_suppression: true,
                    gain_control: true,
                },
            },
        ))
    }
    #[cfg(not(all(feature = "mobile", any(target_os = "ios", target_os = "android"))))]
    {
        EngineConfig::default().with_backend(kessel_audio::Backend::System { input, output })
    }
}

/// Which phone OS the phone backend runs on ([`phone_effects`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(all(feature = "mobile", any(target_os = "ios", target_os = "android"))),
    allow(dead_code)
)]
pub(crate) enum PhoneOs {
    Ios,
    Android,
}

/// What the phone OS's voice processing does for a call: echo cancellation, noise suppression,
/// gain control (kessel-audio's `OsEffects`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(all(feature = "mobile", any(target_os = "ios", target_os = "android"))),
    allow(dead_code)
)]
pub(crate) struct PhoneEffects {
    pub(crate) echo_cancellation: bool,
    pub(crate) noise_suppression: bool,
    pub(crate) gain_control: bool,
}

/// The phone OS's voice processing for a call whose audio starts in `mode`, and the matching
/// change to kessel-audio's own processing. On Android, Off turns the OS's noise suppression
/// off, and kessel-audio's (which would otherwise run what the OS doesn't, or stand in for a
/// bypassed stage) stays off too. On iOS the OS's voice processing keeps its own noise
/// suppression: VoiceProcessingIO has no separate switch for it, and kessel-audio 0.2 refuses
/// `noise_suppression: false` there. Echo cancellation and gain control stay on everywhere.
#[cfg_attr(
    not(all(feature = "mobile", any(target_os = "ios", target_os = "android"))),
    allow(dead_code)
)]
pub(crate) fn phone_effects(
    os: PhoneOs,
    mode: NoiseRemoval,
    processing: &mut kessel_audio::ProcessingConfig,
) -> PhoneEffects {
    let off = os == PhoneOs::Android && mode == NoiseRemoval::Off;
    if off {
        processing.noise_suppression = None;
        processing.stage_fallback_noise_suppression = None;
    }
    PhoneEffects {
        echo_cancellation: true,
        noise_suppression: !off,
        gain_control: true,
    }
}

/// A call's audio is about to start in `mode`: the phone backend's OS effects follow it
/// ([`phone_effects`]). Desktop: nothing changes.
pub(crate) fn apply_phone_mode(cfg: &mut EngineConfig, mode: NoiseRemoval) {
    #[cfg(all(feature = "mobile", any(target_os = "ios", target_os = "android")))]
    {
        let os = if cfg!(target_os = "android") {
            PhoneOs::Android
        } else {
            PhoneOs::Ios
        };
        if let kessel_audio::Backend::OsVoiceProcessing(c) = &mut cfg.backend {
            let e = phone_effects(os, mode, &mut cfg.processing);
            c.os_effects.echo_cancellation = e.echo_cancellation;
            c.os_effects.noise_suppression = e.noise_suppression;
            c.os_effects.gain_control = e.gain_control;
        }
    }
    #[cfg(not(all(feature = "mobile", any(target_os = "ios", target_os = "android"))))]
    let _ = (cfg, mode);
}

/// The phone's audio session, as the app reports it (CallKit on iOS, Telecom or the app's own
/// audio focus on Android), for [`AudioSession::event`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SessionEvent {
    /// The OS's audio session is active (CallKit's `provider(_:didActivate:)`): kessel-audio
    /// opens the microphone and the speaker.
    Activated,
    /// The session is no longer active (`didDeactivate`): I/O stops; the call goes on.
    Deactivated,
    /// An interruption began (a cellular call, hold, lost audio focus): I/O pauses; the call
    /// goes on and the far end hears a pause, not loss.
    Suspend,
    /// The interruption ended: I/O restarts.
    Resume,
    /// The OS audio route changed (speaker, earpiece, Bluetooth): kessel-audio re-reads the
    /// device and reopens it if its rate changed (always on Android).
    RouteChanged,
}

/// The phone's audio session state, shared between the app's reports and the call's audio
/// (one per app; clones share it). It keeps what the app said before a call's audio started
/// (CallKit may activate the session before the join has a call) and hands it to the next
/// [`AudioBridge::start_with_session`]:
///
/// - **Activated / Deactivated** are kept across calls (they are the OS's state). On iOS a
///   bridge starts deferred and opens its device when the session is (or becomes) active; on
///   Android it opens at once, and a later `Deactivated` closes it.
/// - **Suspend / Resume** are kept until the call's audio ends: a `Suspend` before the audio
///   starts pauses it from the start; a bridge that ends clears it.
/// - **RouteChanged** goes to the live bridge only (a new bridge reads the route anyway).
/// - A repeated event (a second `Activated`, `Resume` without `Suspend`) is passed on once.
///
/// On desktop the events change nothing (the system backend has no session); the state is
/// still kept.
#[derive(Clone, Default)]
pub struct AudioSession {
    inner: Arc<Mutex<SessionState>>,
}

impl std::fmt::Debug for AudioSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = self.lock();
        f.debug_struct("AudioSession")
            .field("active", &st.active)
            .field("suspended", &st.suspended)
            .field("attached", &st.attached.is_some())
            .finish()
    }
}

#[derive(Default)]
struct SessionState {
    /// `None` until the app first says.
    active: Option<bool>,
    suspended: bool,
    next_id: u64,
    attached: Option<Attached>,
}

struct Attached {
    id: u64,
    sink: Weak<dyn SessionSink>,
}

/// Where the session events go: kessel-audio's engine (the phone backend's calls), or a test's
/// recorder.
pub(crate) trait SessionSink: Send + Sync {
    fn apply(&self, event: SessionEvent);
}

impl SessionSink for AudioEngine {
    fn apply(&self, event: SessionEvent) {
        #[cfg(all(feature = "mobile", any(target_os = "ios", target_os = "android")))]
        match event {
            SessionEvent::Activated => self.session_activated(),
            SessionEvent::Deactivated => self.session_deactivated(),
            SessionEvent::Suspend => self.suspend(),
            SessionEvent::Resume => self.resume(),
            SessionEvent::RouteChanged => self.route_changed(),
        }
        // Desktop: the system backend has no session; nothing changes.
        #[cfg(not(all(feature = "mobile", any(target_os = "ios", target_os = "android"))))]
        let _ = event;
    }
}

impl AudioSession {
    /// A new session state (nothing reported yet).
    pub fn new() -> AudioSession {
        AudioSession::default()
    }

    fn lock(&self) -> MutexGuard<'_, SessionState> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The app's report. Never blocks (kessel-audio acts on its control thread).
    pub fn event(&self, event: SessionEvent) {
        let mut guard = self.lock();
        let st = &mut *guard;
        let changed = match event {
            SessionEvent::Activated => st.active.replace(true) != Some(true),
            SessionEvent::Deactivated => st.active.replace(false) != Some(false),
            SessionEvent::Suspend => !std::mem::replace(&mut st.suspended, true),
            SessionEvent::Resume => std::mem::replace(&mut st.suspended, false),
            SessionEvent::RouteChanged => true,
        };
        if !changed {
            return;
        }
        if let Some(sink) = st.attached.as_ref().and_then(|a| a.sink.upgrade()) {
            sink.apply(event);
        }
    }

    /// What the app last said: active (`Some(true)`), not active, or `None` if it never said.
    pub fn is_active(&self) -> Option<bool> {
        self.lock().active
    }

    /// An interruption is going on ([`SessionEvent::Suspend`] without a `Resume` yet).
    pub fn is_suspended(&self) -> bool {
        self.lock().suspended
    }

    /// A call's audio starts: it takes the events from now on (replacing any earlier one).
    /// `deferred`: its device waits for the session (iOS), so an active session is replayed.
    pub(crate) fn attach(&self, sink: Weak<dyn SessionSink>, deferred: bool) -> u64 {
        let mut guard = self.lock();
        let st = &mut *guard;
        st.next_id += 1;
        let id = st.next_id;
        if let Some(s) = sink.upgrade() {
            if deferred && st.active == Some(true) {
                s.apply(SessionEvent::Activated);
            }
            if st.suspended {
                s.apply(SessionEvent::Suspend);
            }
        }
        st.attached = Some(Attached { id, sink });
        id
    }

    /// The call's audio `id` ended: the events stop reaching it, and its interruption is over.
    pub(crate) fn detach(&self, id: u64) {
        let mut guard = self.lock();
        let st = &mut *guard;
        if st.attached.as_ref().is_some_and(|a| a.id == id) {
            st.attached = None;
            st.suspended = false;
        }
    }
}

/// A running bridge between one call and a kessel-audio engine.
pub struct AudioBridge {
    engine: Arc<AudioEngine>,
    identities: Identities,
    task: JoinHandle<()>,
    session: AudioSession,
    session_id: u64,
    /// The noise removal this call's audio started with, and the call's id there.
    noise: Option<(NoiseRemovalControl, u64)>,
}

/// kessel-audio's participant ids → the publishers' engine identities.
type Identities = Arc<Mutex<HashMap<u64, String>>>;

/// kessel-audio's latest levels (about every 50 ms) by engine identity
/// ([`AudioBridge::levels`]).
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct BridgeLevels {
    /// This device's processed capture (silent while muted).
    pub local: Level,
    /// Each remote track's decoded audio, by its publisher's engine identity.
    pub remotes: Vec<(String, Level)>,
}

impl std::fmt::Debug for AudioBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioBridge").finish_non_exhaustive()
    }
}

impl Drop for AudioBridge {
    fn drop(&mut self) {
        self.session.detach(self.session_id);
        self.task.abort();
        self.engine.shutdown();
        if let Some((noise, id)) = &self.noise {
            noise.detach(*id);
        }
    }
}

impl AudioBridge {
    /// Starts kessel-audio with `cfg` and bridges it to `call` (on the current tokio runtime),
    /// with a session state of its own ([`AudioBridge::session`]). A phone host keeps one
    /// [`AudioSession`] for the app instead: [`AudioBridge::start_with_session`].
    pub fn start(call: &Call, cfg: EngineConfig) -> Result<AudioBridge, AudioBridgeError> {
        AudioBridge::start_with_session(call, cfg, &AudioSession::new())
    }

    /// As [`AudioBridge::start`], with the app's [`AudioSession`]: what the app reported before
    /// this call (an active session, an interruption) applies from the start, and its reports
    /// reach this call's kessel-audio until the bridge is dropped. On iOS with the phone
    /// backend the engine starts deferred (`start_when_session_active`): nothing opens until the
    /// session is active.
    pub fn start_with_session(
        call: &Call,
        cfg: EngineConfig,
        session: &AudioSession,
    ) -> Result<AudioBridge, AudioBridgeError> {
        AudioBridge::start_inner(call, cfg, session, None)
    }

    /// As [`AudioBridge::start_with_session`], with noise removal from `noise` (the app's
    /// setting; keep one [`NoiseRemovalControl`] for the app): the call's capture stage is built
    /// in its mode and handed to kessel-audio (whose own noise suppression is then off), in
    /// place of any stage `cfg` carries. While the bridge runs, [`NoiseRemovalControl::set`]
    /// changes the mode from the next 10 ms block without a click, its state follows the
    /// pipeline (a bypass for time or a fault steps it down), and the call's stats windows
    /// carry it ([`crate::stats::StatsWindow::noise_removal`]).
    pub fn start_with_noise(
        call: &Call,
        cfg: EngineConfig,
        session: &AudioSession,
        noise: &NoiseRemovalControl,
    ) -> Result<AudioBridge, AudioBridgeError> {
        let (cfg, id) = noise.configure(cfg);
        AudioBridge::start_inner(call, cfg, session, Some((noise.clone(), id))).inspect_err(|_| {
            noise.detach(id);
        })
    }

    fn start_inner(
        call: &Call,
        cfg: EngineConfig,
        session: &AudioSession,
        noise: Option<(NoiseRemovalControl, u64)>,
    ) -> Result<AudioBridge, AudioBridgeError> {
        let frames = call.frames().ok_or(AudioBridgeError::FramesTaken)?;
        let (engine, deferred) = start_engine(cfg)?;
        let engine = Arc::new(engine);
        let weak: Weak<_> = Arc::downgrade(&engine);
        let sink: Weak<dyn SessionSink> = weak;
        let session_id = session.attach(sink, deferred);
        // The beacon's played and concealed audio are kessel-audio's own counters.
        call.set_playout_stats(Arc::new(KesselPlayout(Arc::downgrade(&engine))));
        if let Some((control, id)) = &noise {
            let weak: Weak<_> = Arc::downgrade(&engine);
            let source: Weak<dyn crate::noise::BypassSource> = weak;
            control.attach(*id, source);
            call.set_noise_source(control.source(*id));
        }
        let identities = Identities::default();
        let task = tokio::spawn(bridge(
            engine.clone(),
            identities.clone(),
            call.frame_sender(),
            frames,
            noise.as_ref().map(|(c, _)| c.clone()),
        ));
        Ok(AudioBridge {
            engine,
            identities,
            task,
            session: session.clone(),
            session_id,
            noise,
        })
    }

    /// The noise removal state of this call's audio ([`AudioBridge::start_with_noise`]);
    /// `None` for a bridge started without it.
    pub fn noise_removal(&self) -> Option<NoiseRemovalState> {
        self.noise.as_ref().map(|(c, _)| c.state())
    }

    /// The session state this bridge takes its events from.
    pub fn session(&self) -> &AudioSession {
        &self.session
    }

    /// The kessel-audio engine (levels, stats, devices).
    pub fn engine(&self) -> &AudioEngine {
        &self.engine
    }

    /// kessel-audio's latest levels, the remote ones by engine identity (a track whose
    /// identity the bridge has not seen yet is left out).
    pub fn levels(&self) -> BridgeLevels {
        let levels = self.engine.levels();
        let ids = self
            .identities
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        BridgeLevels {
            local: levels.local,
            remotes: levels
                .participants
                .iter()
                .filter_map(|(p, l)| ids.get(p).map(|i| (i.clone(), *l)))
                .collect(),
        }
    }
}

/// Starts kessel-audio; `true` when it waits for the session (iOS's phone backend).
pub(crate) fn start_engine(
    cfg: EngineConfig,
) -> Result<(AudioEngine, bool), kessel_audio::AudioError> {
    #[cfg(all(feature = "mobile", target_os = "ios"))]
    if matches!(cfg.backend, kessel_audio::Backend::OsVoiceProcessing(_)) {
        return Ok((AudioEngine::start_when_session_active(cfg)?, true));
    }
    Ok((AudioEngine::start(cfg)?, false))
}

/// kessel-audio's per-participant playout counters for the stats windows (none once the
/// bridge is gone).
struct KesselPlayout(Weak<AudioEngine>);

impl PlayoutStats for KesselPlayout {
    fn playout_totals(&self) -> Vec<(u64, PlayoutTotals)> {
        let Some(engine) = self.0.upgrade() else {
            return Vec::new();
        };
        engine
            .stats()
            .participants
            .iter()
            .map(|(id, p)| {
                (
                    *id,
                    PlayoutTotals::new(p.frames_decoded, p.frames_fec, p.frames_concealed),
                )
            })
            .collect()
    }
}

async fn bridge(
    engine: Arc<AudioEngine>,
    identities: Identities,
    send: FrameSender,
    mut frames: FrameStream,
    noise: Option<NoiseRemovalControl>,
) {
    let capture = engine.capture_frames();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ticks = 0u32;
    loop {
        tick.tick().await;
        ticks = ticks.wrapping_add(1);
        if let Some(n) = &noise
            && ticks.is_multiple_of(NOISE_TICKS)
        {
            // Sends a `NoiseRemovalControl::subscribe` event when the state changed.
            n.publish();
        }
        while let Some(f) = capture.try_recv() {
            send.push_opus(OpusFrame {
                payload: f.payload,
                ts48k: f.ts48k,
                vad: f.vad,
                level_dbov: f.level_dbov,
            });
        }
        loop {
            match frames.0.try_recv() {
                Ok(item) => apply(&engine, &identities, item),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return,
            }
        }
    }
}

fn apply(engine: &AudioEngine, identities: &Identities, item: RecvItem) {
    match item {
        RecvItem::Added {
            participant,
            identity,
        } => {
            if let Ok(mut m) = identities.lock() {
                m.insert(participant, identity);
            }
            engine.add_participant(participant);
        }
        RecvItem::Removed { participant } => {
            if let Ok(mut m) = identities.lock() {
                m.remove(&participant);
            }
            engine.remove_participant(participant);
        }
        RecvItem::Frame(f) => engine.push(RecvFrame {
            participant: f.participant,
            payload: f.payload,
            seq: f.seq,
            ts48k: f.ts48k,
            arrival: f.arrival,
        }),
        RecvItem::Lost { participant, seq } => engine.lost(participant, seq),
        RecvItem::Capture(on) => engine.set_capture(on),
        RecvItem::Deafened(on) => engine.set_deafened(on),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    use SessionEvent::{Activated, Deactivated, Resume, RouteChanged, Suspend};
    use kessel_audio::{CaptureSource, NullDevice, ProcessingConfig};

    /// Off on Android: the OS's noise suppression off, and kessel-audio's own too (no stand-in
    /// for it or for a bypassed stage); echo cancellation and gain control on. iOS keeps the
    /// OS's noise suppression (kessel-audio 0.2 needs it there). Standard changes nothing.
    #[test]
    fn phone_effects_follow_the_mode() {
        let all_on = PhoneEffects {
            echo_cancellation: true,
            noise_suppression: true,
            gain_control: true,
        };
        let mut cfg = EngineConfig::default();
        let (ns, fallback) = (
            cfg.processing.noise_suppression,
            cfg.processing.stage_fallback_noise_suppression,
        );
        let e = phone_effects(PhoneOs::Android, NoiseRemoval::Off, &mut cfg.processing);
        assert_eq!(
            e,
            PhoneEffects {
                echo_cancellation: true,
                noise_suppression: false,
                gain_control: true,
            }
        );
        assert_eq!(cfg.processing.noise_suppression, None);
        assert_eq!(cfg.processing.stage_fallback_noise_suppression, None);
        for (os, mode) in [
            (PhoneOs::Ios, NoiseRemoval::Off),
            (PhoneOs::Ios, NoiseRemoval::Standard),
            (PhoneOs::Android, NoiseRemoval::Standard),
        ] {
            let mut cfg = EngineConfig::default();
            assert_eq!(
                phone_effects(os, mode, &mut cfg.processing),
                all_on,
                "{os:?} {mode:?}"
            );
            assert_eq!(cfg.processing.noise_suppression, ns, "{os:?} {mode:?}");
            assert_eq!(
                cfg.processing.stage_fallback_noise_suppression, fallback,
                "{os:?} {mode:?}"
            );
        }
        // Desktop: apply_phone_mode changes nothing.
        let mut cfg = EngineConfig::default();
        apply_phone_mode(&mut cfg, NoiseRemoval::Off);
        assert_eq!(cfg.processing.noise_suppression, ns);
    }

    /// Records what reaches "kessel-audio", in order.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<SessionEvent>>);

    impl SessionSink for Recorder {
        fn apply(&self, event: SessionEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    impl Recorder {
        fn take(&self) -> Vec<SessionEvent> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    fn attach(session: &AudioSession, deferred: bool) -> (Arc<Recorder>, u64) {
        let rec = Arc::new(Recorder::default());
        let weak: Weak<_> = Arc::downgrade(&rec);
        let sink: Weak<dyn SessionSink> = weak;
        let id = session.attach(sink, deferred);
        (rec, id)
    }

    #[test]
    fn events_before_the_call_are_replayed_on_a_deferred_start() {
        let s = AudioSession::new();
        // CallKit activates the session before the join has a call; a route change then is
        // the new bridge's to read.
        s.event(Activated);
        s.event(RouteChanged);
        assert_eq!(s.is_active(), Some(true));
        let (rec, _id) = attach(&s, true);
        assert_eq!(rec.take(), vec![Activated]);
        // Live from now on.
        s.event(RouteChanged);
        s.event(Deactivated);
        assert_eq!(rec.take(), vec![RouteChanged, Deactivated]);
    }

    #[test]
    fn a_deferred_start_waits_for_the_session() {
        let s = AudioSession::new();
        let (rec, _id) = attach(&s, true);
        // Never said: nothing opens.
        assert_eq!(rec.take(), vec![]);
        assert_eq!(s.is_active(), None);
        s.event(Activated);
        assert_eq!(rec.take(), vec![Activated]);
    }

    #[test]
    fn a_deactivated_session_is_not_replayed_as_active() {
        let s = AudioSession::new();
        s.event(Activated);
        s.event(Deactivated);
        let (rec, _id) = attach(&s, true);
        assert_eq!(rec.take(), vec![]);
        s.event(Activated);
        assert_eq!(rec.take(), vec![Activated]);
    }

    #[test]
    fn an_immediate_start_is_not_told_activated_again() {
        // Android: the engine opens at once; an earlier Activated is not replayed.
        let s = AudioSession::new();
        s.event(Activated);
        let (rec, _id) = attach(&s, false);
        assert_eq!(rec.take(), vec![]);
        s.event(Deactivated);
        s.event(Activated);
        assert_eq!(rec.take(), vec![Deactivated, Activated]);
    }

    #[test]
    fn a_double_activated_is_passed_on_once() {
        let s = AudioSession::new();
        let (rec, _id) = attach(&s, true);
        s.event(Activated);
        s.event(Activated);
        s.event(Deactivated);
        s.event(Deactivated);
        s.event(Activated);
        assert_eq!(rec.take(), vec![Activated, Deactivated, Activated]);
    }

    #[test]
    fn suspend_then_resume_in_order_and_once_each() {
        let s = AudioSession::new();
        let (rec, _id) = attach(&s, true);
        s.event(Activated);
        s.event(Suspend);
        s.event(Suspend);
        assert!(s.is_suspended());
        s.event(Resume);
        s.event(Resume);
        assert!(!s.is_suspended());
        assert_eq!(rec.take(), vec![Activated, Suspend, Resume]);
    }

    #[test]
    fn a_suspend_before_the_call_pauses_it_from_the_start() {
        let s = AudioSession::new();
        s.event(Activated);
        s.event(Suspend);
        let (rec, _id) = attach(&s, true);
        assert_eq!(rec.take(), vec![Activated, Suspend]);
        s.event(Resume);
        assert_eq!(rec.take(), vec![Resume]);
    }

    #[test]
    fn an_ended_call_gets_nothing_and_its_interruption_ends() {
        let s = AudioSession::new();
        let (first, id) = attach(&s, true);
        s.event(Activated);
        s.event(Suspend);
        assert_eq!(first.take(), vec![Activated, Suspend]);
        s.detach(id);
        assert!(!s.is_suspended());
        s.event(RouteChanged);
        s.event(Deactivated);
        assert_eq!(first.take(), vec![]);
        // The next call: the session is not active, nothing is suspended.
        let (second, _id) = attach(&s, true);
        assert_eq!(second.take(), vec![]);
        s.event(Activated);
        assert_eq!(second.take(), vec![Activated]);
        assert_eq!(first.take(), vec![]);
    }

    #[test]
    fn a_stale_detach_leaves_the_new_call_attached() {
        let s = AudioSession::new();
        let (_first, old) = attach(&s, true);
        let (second, _new) = attach(&s, true);
        s.detach(old);
        s.event(Activated);
        assert_eq!(second.take(), vec![Activated]);
    }

    #[test]
    fn a_dropped_engine_is_skipped() {
        let s = AudioSession::new();
        let (rec, _id) = attach(&s, true);
        drop(rec);
        // No panic, and the state is still kept.
        s.event(Activated);
        s.event(Suspend);
        assert_eq!(s.is_active(), Some(true));
        assert!(s.is_suspended());
    }

    /// A 440 Hz tone at 48 kHz (never DTX), for the null device.
    fn tone() -> CaptureSource {
        let mut n = 0u64;
        Box::new(move |buf: &mut [f32]| {
            for x in buf.iter_mut() {
                *x = 0.3 * (n as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin();
                n += 1;
            }
        })
    }

    #[test]
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    fn this_build_picks_the_system_backend_on_desktop() {
        assert_eq!(AUDIO_BACKEND, "system");
    }

    #[test]
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    fn desktop_audio_is_unchanged_by_session_events() {
        // A real kessel-audio engine on the null device: on desktop the session events reach
        // it and change nothing (no suspend, no closed device).
        let mut cfg = EngineConfig::null(NullDevice {
            input: Some(tone()),
            ..NullDevice::default()
        });
        cfg.processing = ProcessingConfig::off();
        let (engine, deferred) = start_engine(cfg).unwrap();
        assert!(!deferred);
        let engine = Arc::new(engine);
        let session = AudioSession::new();
        session.event(Suspend);
        let weak: Weak<_> = Arc::downgrade(&engine);
        let sink: Weak<dyn SessionSink> = weak;
        let id = session.attach(sink, deferred);
        let frames = engine.capture_frames();
        session.event(Deactivated);
        session.event(RouteChanged);
        // Drain what came before, then frames must keep coming while "suspended" and
        // "deactivated".
        while frames.try_recv().is_some() {}
        let mut got = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while got < 10 && std::time::Instant::now() < deadline {
            if frames.recv_timeout(Duration::from_millis(200)).is_some() {
                got += 1;
            }
        }
        assert_eq!(got, 10, "the desktop engine stopped producing frames");
        session.detach(id);
        engine.shutdown();
    }
}
