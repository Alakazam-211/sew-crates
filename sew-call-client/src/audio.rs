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
use crate::noise::{NoiseRemoval, NoiseRemovalControl, NoiseRemovalState, PhoneAudioMode};
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
///   runs none of them). A call whose audio starts in [`NoiseRemoval::Off`]
///   ([`AudioBridge::start_with_noise`]) captures raw instead ([`crate::PhoneAudioMode::Raw`]:
///   no OS processing; kessel-audio cancels echo and controls gain, with no noise
///   suppression), and a change of the setting during the call switches between the two.
///   `input` and `output` are ignored: the OS owns the route. Start it with
///   [`AudioBridge::start_with_session`] and report the phone's audio session to the
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

/// Which phone OS the phone backend runs on ([`phone_start`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PhoneOs {
    Ios,
    Android,
}

/// The phone OS of this build: `None` off phones (and without feature `mobile`).
pub(crate) const PHONE_OS: Option<PhoneOs> = if !PHONE_AUDIO {
    None
} else if cfg!(target_os = "android") {
    Some(PhoneOs::Android)
} else {
    Some(PhoneOs::Ios)
};

/// How a phone's microphone is to be captured: the capture, and for raw on Android whether
/// the device's unprocessed source may be used (else its voice recognition source).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PhoneStart {
    pub(crate) mode: PhoneAudioMode,
    pub(crate) unprocessed: bool,
}

/// The capture for `mode` on `os`: raw for Off (with the unprocessed source on Android when
/// the app says the device supports it), the OS's voice processing for Standard. The OS
/// effects stay all on in the config: raw capture ignores them.
pub(crate) fn phone_start(
    os: PhoneOs,
    mode: NoiseRemoval,
    unprocessed_supported: bool,
) -> PhoneStart {
    let mode = PhoneAudioMode::for_mode(mode);
    PhoneStart {
        mode,
        unprocessed: mode == PhoneAudioMode::Raw && os == PhoneOs::Android && unprocessed_supported,
    }
}

/// kessel-audio's phone mode for `start`.
#[cfg(all(feature = "mobile", any(target_os = "ios", target_os = "android")))]
pub(crate) fn kessel_phone_mode(start: PhoneStart) -> kessel_audio::PhoneMode {
    match start.mode {
        PhoneAudioMode::Raw => kessel_audio::PhoneMode::Raw(
            kessel_audio::RawCapture::default().with_unprocessed_supported(start.unprocessed),
        ),
        PhoneAudioMode::Os => kessel_audio::PhoneMode::Os,
    }
}

/// Sew's name for kessel-audio's phone mode.
#[cfg(all(feature = "mobile", any(target_os = "ios", target_os = "android")))]
pub(crate) fn phone_audio_mode(mode: kessel_audio::PhoneMode) -> PhoneAudioMode {
    if mode.is_raw() {
        PhoneAudioMode::Raw
    } else {
        PhoneAudioMode::Os
    }
}

/// A call's audio is about to start in `mode`: on a phone backend its starting capture follows
/// the setting ([`phone_start`]), so a call that starts in Off opens raw with no restart.
/// Answers the capture set, or `None` (desktop, or a config without the phone backend: nothing
/// changes).
pub(crate) fn apply_phone_mode(
    cfg: &mut EngineConfig,
    mode: NoiseRemoval,
    unprocessed_supported: bool,
) -> Option<PhoneAudioMode> {
    #[cfg(all(feature = "mobile", any(target_os = "ios", target_os = "android")))]
    if let Some(os) = PHONE_OS
        && matches!(cfg.backend, kessel_audio::Backend::OsVoiceProcessing(_))
    {
        let start = phone_start(os, mode, unprocessed_supported);
        cfg.phone_mode = kessel_phone_mode(start);
        return Some(start.mode);
    }
    let _ = (cfg, mode, unprocessed_supported);
    None
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

/// A bridge's session sink: the call's noise removal hears an event first (an activation, a
/// resume or a route change opens its stage's grace window before the device reopens), then
/// kessel-audio's engine.
pub(crate) struct BridgeSink {
    pub(crate) engine: Weak<AudioEngine>,
    pub(crate) noise: Option<(NoiseRemovalControl, u64)>,
}

impl SessionSink for BridgeSink {
    fn apply(&self, event: SessionEvent) {
        if let Some((noise, id)) = &self.noise {
            noise.session_event(*id, event);
        }
        if let Some(engine) = self.engine.upgrade() {
            engine.apply(event);
        }
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
    /// The session's sink for this call (the session holds it weakly).
    _sink: Arc<BridgeSink>,
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
        // The noise removal first: a change of the setting since `configure` reaches the
        // phone's capture before the session's replayed `Activated` opens the device (iOS), so
        // the device opens once, in the mode asked.
        if let Some((control, id)) = &noise {
            let weak: Weak<_> = Arc::downgrade(&engine);
            let source: Weak<dyn crate::noise::LiveAudio> = weak;
            control.attach(*id, source);
            call.set_noise_source(control.source(*id));
        }
        let bridge_sink = Arc::new(BridgeSink {
            engine: Arc::downgrade(&engine),
            noise: noise.clone(),
        });
        let weak: Weak<BridgeSink> = Arc::downgrade(&bridge_sink);
        let sink: Weak<dyn SessionSink> = weak;
        let session_id = session.attach(sink, deferred);
        // The beacon's played and concealed audio are kessel-audio's own counters.
        call.set_playout_stats(Arc::new(KesselPlayout(Arc::downgrade(&engine))));
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
            _sink: bridge_sink,
        })
    }

    /// The noise removal state of this call's audio ([`AudioBridge::start_with_noise`]);
    /// `None` for a bridge started without it.
    pub fn noise_removal(&self) -> Option<NoiseRemovalState> {
        self.noise.as_ref().map(|(c, _)| c.state())
    }

    /// kessel-audio's events since the last read: the engine's queue
    /// (`engine().events()`), drained. Read them here rather than from the engine: the
    /// bridge looks at each on the way (an error while the phone's capture is switching means
    /// the switch failed: [`NoiseRemovalState::phone_mode_reason`]; a change of the devices
    /// in use opens the noise removal stage's grace window,
    /// [`NoiseRemovalState::in_grace`]) and hands every one on.
    pub fn events(&self) -> Vec<kessel_audio::Event> {
        let rx = self.engine.events();
        let mut out = Vec::new();
        while let Some(e) = rx.try_recv() {
            match (&e, &self.noise) {
                (kessel_audio::Event::Error(_), Some((noise, id))) => noise.audio_error(*id),
                // The streams reopened on other devices: the stage's grace window.
                (kessel_audio::Event::DevicesInUse(_), Some((noise, id))) => {
                    noise.devices_changed(*id);
                }
                _ => {}
            }
            out.push(e);
        }
        out
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

    /// A call's starting capture per mode and phone OS: Off is raw (on Android with the
    /// unprocessed source only when the app says the device has it; iOS ignores the flag),
    /// Standard the OS's voice processing.
    #[test]
    fn the_starting_capture_follows_the_mode() {
        let raw = |unprocessed| PhoneStart {
            mode: PhoneAudioMode::Raw,
            unprocessed,
        };
        let os = PhoneStart {
            mode: PhoneAudioMode::Os,
            unprocessed: false,
        };
        for (phone, mode, supported, want) in [
            (PhoneOs::Android, NoiseRemoval::Off, true, raw(true)),
            (PhoneOs::Android, NoiseRemoval::Off, false, raw(false)),
            (PhoneOs::Ios, NoiseRemoval::Off, true, raw(false)),
            (PhoneOs::Ios, NoiseRemoval::Off, false, raw(false)),
            (PhoneOs::Android, NoiseRemoval::Standard, true, os),
            (PhoneOs::Android, NoiseRemoval::Standard, false, os),
            (PhoneOs::Ios, NoiseRemoval::Standard, true, os),
            (PhoneOs::Ios, NoiseRemoval::Standard, false, os),
        ] {
            assert_eq!(
                phone_start(phone, mode, supported),
                want,
                "{phone:?} {mode:?} unprocessed={supported}"
            );
        }
    }

    /// Desktop (and any config without the phone backend): the config is untouched in every
    /// mode, kessel-audio's own processing included.
    #[test]
    fn desktop_config_is_untouched_by_the_mode() {
        assert_eq!(PHONE_OS, None);
        for mode in NoiseRemoval::ALL {
            let mut cfg = EngineConfig::default();
            let (ns, fallback) = (
                cfg.processing.noise_suppression,
                cfg.processing.stage_fallback_noise_suppression,
            );
            assert_eq!(apply_phone_mode(&mut cfg, mode, true), None);
            assert_eq!(cfg.processing.noise_suppression, ns, "{mode:?}");
            assert_eq!(
                cfg.processing.stage_fallback_noise_suppression, fallback,
                "{mode:?}"
            );
        }
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
