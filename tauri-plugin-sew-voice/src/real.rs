//! The real engine (feature `audio-core`; `audio` adds the system devices):
//! [`CallClientEngine`], the plugin's [`VoiceEngine`] over `sew-call-client`'s `CallClient`, and
//! its calls, a [`CallHandle`] over `Call` with kessel-audio through `AudioBridge`.
//!
//! - **Events.** The engine's state, participants and errors become the plugin's (the
//!   [`wire`](crate::wire) shapes). `speaking` comes from kessel-audio's levels (decoded audio for
//!   each remote track, the processed capture for this device), sampled every 100 ms and sent when
//!   they change; the plugin's own throttle keeps it at 10 a second at most.
//! - **Devices.** During a call they are kessel-audio's (`AudioBridge::engine()`: its list, its
//!   hot-plug events, its device switch). Between calls no audio engine runs (it would open the
//!   microphone), so the list comes from a [`DeviceLister`] (the system's through cpal with the
//!   `audio` feature, the same ids kessel-audio uses), and a `set_device` is kept for the next
//!   call. Nothing is listed before the first use (the first `devices`, `set_device`, `join` or
//!   `device_events`); after it the lister is polled every 2 s for hot-plug, between calls and
//!   only while something is subscribed to the device events.
//! - **One call per app**, as the engine: `Call` sits in an `RwLock<Option<Call>>` because
//!   `Call::leave(self)` consumes it. A leave goes to the engine through the call's
//!   `CallLeaver`, kept outside that lock, so it never waits for a camera or screen command
//!   holding the lock while it waits on a device: the engine ends the call and frees the app's
//!   call slot at once, those commands answer `ended`, and the `Call` is taken when they end
//!   (logged past [`TAKE_WAIT`]); the leave doesn't wait for that. The engine's answer is
//!   awaited at most [`LEAVE_WAIT`] (logged when it fires).
//! - **Recording** (Sew 0.7.3). A join held for the recording notice (`consent_required`) opens
//!   no microphone: kessel-audio starts when the call first leaves that state (after
//!   `consent`). The `recording` event is the engine's recording with `recorder_present` from
//!   its roster (the recorder is never a `participants` entry: it is no member). `consent` and
//!   `withdraw_consent` are the engine's, through the host's provider.
//! - **The phone's audio session.** The engine keeps one `AudioSession` for the app: every
//!   [`AudioSessionEvent`] goes to it, with or without a call, and each call's kessel-audio
//!   starts from what it says (on iOS, deferred until the session is active). On phones the
//!   audio is kessel-audio's OS voice processing (`sew_call_client::platform_audio_config`).
//! - **Device keys.** `forget_server` and `known_device_keys` are `CallClient`'s, over the
//!   device store of the engine's `ClientConfig`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use sew_call_client::device::{DeviceError, DeviceKeyId};
use sew_call_client::kessel_audio::{
    self, AudioError, DeviceChoice, DeviceSelection, EngineConfig, ErrorCode, Event as AudioEvent,
};
use sew_call_client::{
    AudioBridge, AudioSession, BridgeLevels, Call, CallClient, CallError, CallLeaver,
    CallRecording, EndReason, SessionEvent,
};
use tokio::sync::{RwLock, broadcast};
use tokio::task::JoinHandle;

#[cfg(feature = "video-core")]
mod backend;
#[cfg(feature = "video-core")]
mod capture;
#[cfg(feature = "video-core")]
mod publish;
#[cfg(all(
    feature = "ios-screen-ring",
    feature = "video-core",
    any(target_os = "ios", target_os = "linux")
))]
mod ring;
#[cfg(feature = "video-core")]
mod scene;
mod video;

use crate::details::{QualityInfo, RosterEntry, SecurityInfo, roster_of};
use crate::engine::{
    AudioSessionEvent, BoxFuture, CallEvent, CallHandle, CallSnapshot, EngineError, JoinAnswer,
    JoinProvider, JoinRequest, ProviderError, VoiceEngine,
};
use crate::video::VideoCodec;
use crate::wire::{
    CallState, Device, DeviceChoice as Choice, DeviceList, ErrorInfo, Participant, RecordingInfo,
    Speaking, code, reason,
};

/// The most a leave waits for the engine's answer (Sew's leave and the engine's own are each
/// bounded well within it); past it the leave is logged and the plugin goes on.
pub const LEAVE_WAIT: Duration = Duration::from_secs(12);
/// A camera or screen command that still holds a left `Call` this long after the leave is
/// logged (the leave itself never waits for it; the `Call` is taken when the command ends).
pub const TAKE_WAIT: Duration = Duration::from_secs(2);

/// How often the `speaking` levels are sampled (at most 10 a second).
pub const SPEAKING_EVERY: Duration = Duration::from_millis(100);
/// How often the device list is polled for hot-plug between calls (after the first use, while
/// something is subscribed to the device events).
pub const IDLE_DEVICE_POLL: Duration = Duration::from_secs(2);

/// The poll's pace (the unit tests' is short).
fn idle_device_poll() -> Duration {
    if cfg!(test) {
        Duration::from_millis(20)
    } else {
        IDLE_DEVICE_POLL
    }
}

/// Builds kessel-audio's configuration for a call from the chosen input and output.
pub type AudioConfig = Arc<dyn Fn(DeviceChoice, DeviceChoice) -> EngineConfig + Send + Sync>;

/// Lists the audio devices while no call (and so no kessel-audio engine) runs.
pub type DeviceLister = Arc<dyn Fn() -> kessel_audio::DeviceList + Send + Sync>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// `sew-call-client` as the plugin's engine.
pub struct CallClientEngine {
    inner: Arc<EngineInner>,
}

impl std::fmt::Debug for CallClientEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallClientEngine")
            .field("client", &self.inner.client)
            .finish_non_exhaustive()
    }
}

struct EngineInner {
    client: CallClient,
    audio: AudioConfig,
    lister: Mutex<Option<DeviceLister>>,
    /// The devices were used (listed once; the poll thread runs).
    listing: AtomicBool,
    devices_tx: broadcast::Sender<DeviceList>,
    state: Mutex<DevState>,
    /// The phone's audio session, for every call's kessel-audio.
    session: AudioSession,
    /// The video backend (the camera permission, the background signal, the native sinks'
    /// decoder, the camera and screen publish).
    #[cfg(feature = "video-core")]
    video: backend::EngineVideo,
    /// Cameras, screens and the preview over the same backend (`video-core`; use
    /// [`EngineInner::capture`]).
    #[cfg(feature = "video-core")]
    capture: capture::EngineCapture,
}

#[cfg(feature = "video-core")]
impl EngineInner {
    /// The capture, attached to the bridge's `VideoCapture` on first use (one opener holds
    /// the camera: the preview and the camera publish share it).
    fn capture(&self) -> &capture::EngineCapture {
        self.capture.attach(|| self.video.capture());
        &self.capture
    }
}

#[derive(Default)]
struct DevState {
    /// The last list seen (kessel-audio's during a call, the lister's between calls).
    list: kessel_audio::DeviceList,
    /// The chosen input and output (`None`: the OS default).
    input: Option<String>,
    output: Option<String>,
    /// The live call.
    live: Option<Weak<CallShared>>,
    /// Calls left whose `Call` a command still held at the leave (released by
    /// [`VoiceEngine::release_calls`], or when that command ends).
    lingering: Vec<Weak<CallShared>>,
}

impl DevState {
    fn wire(&self) -> DeviceList {
        let dev = |d: &kessel_audio::DeviceInfo| Device {
            id: d.id.clone(),
            name: d.name.clone(),
            default: d.is_default,
        };
        DeviceList {
            inputs: self.list.inputs.iter().map(dev).collect(),
            outputs: self.list.outputs.iter().map(dev).collect(),
            input: self.input.clone(),
            output: self.output.clone(),
        }
    }

    fn live(&self) -> Option<Arc<CallShared>> {
        self.live.as_ref().and_then(Weak::upgrade)
    }
}

impl CallClientEngine {
    /// The engine over `client`, with kessel-audio configured by `audio` for each call. Until
    /// [`CallClientEngine::devices_from`], the device list between calls is the last call's.
    pub fn new(client: CallClient, audio: AudioConfig) -> CallClientEngine {
        let (devices_tx, _) = broadcast::channel(16);
        CallClientEngine {
            inner: Arc::new(EngineInner {
                client,
                audio,
                lister: Mutex::new(None),
                listing: AtomicBool::new(false),
                devices_tx,
                state: Mutex::new(DevState::default()),
                session: AudioSession::new(),
                #[cfg(feature = "video-core")]
                video: backend::EngineVideo::default(),
                #[cfg(feature = "video-core")]
                capture: capture::EngineCapture::new(),
            }),
        }
    }

    /// The host's own video backend instead of kessel-video's `system` (tests: kessel-video's
    /// fake backend). Built engines take it before the first use of video. It is the engine's
    /// one backend: the cameras, screens, permissions and the preview (`video_capture`), the
    /// camera and screen publish (`VideoBridge`, over the same capture), the native sinks'
    /// decoder and the decoded fallback's decoders (`video_caps.decode_codecs`). Without it and
    /// without the plugin's `video` there is none (`camera`, `screen` and `preview` answer
    /// `video_unsupported` and nothing is decoded).
    #[cfg(feature = "video-core")]
    pub fn with_video_backend(
        self,
        backend: Arc<dyn sew_call_client::kessel_video::VideoBackend>,
    ) -> CallClientEngine {
        self.inner.video.set_backend(backend);
        self
    }

    /// Android: the host's JNI handles for kessel-video's Android backend
    /// (`unsafe { AndroidContext::new(vm, context) }` from `ndk_context::android_context()`),
    /// so it reads the CAMERA permission. Without it the permission reads as `prompt` until a
    /// camera opens. [`crate::VoiceControl::app_scene`] and `device_orientation` reach this
    /// context; other platforms ignore it.
    #[cfg(feature = "video-core")]
    pub fn with_android_video_context(
        self,
        ctx: sew_call_client::kessel_video::AndroidContext,
    ) -> CallClientEngine {
        self.inner.video.set_android(ctx);
        self
    }

    /// The production engine: Sew's MLS call keys ([`CallClient::with_mls`]: the device key
    /// where `cfg.device_store` says, by default a 0600 file under `cfg.data_dir`), the
    /// system's audio devices through kessel-audio, and the system device list between calls.
    ///
    /// On iOS and Android nothing is listed between calls (no device enumeration, no poll):
    /// the OS owns the route there (CallKit, Telecom), and `devices` answers the last call's
    /// list, empty before the first.
    ///
    /// With `video`, the cameras and screens are the system's (kessel-video's backend for the
    /// target, made on first use; none when it can't start).
    #[cfg(feature = "audio")]
    pub fn with_mls(cfg: sew_call_client::ClientConfig) -> CallClientEngine {
        CallClientEngine::new(CallClient::with_mls(cfg), system_audio()).with_idle_devices()
    }

    /// The engine's capture (`video-core`; `None` without a video backend): the cameras, the
    /// screens and the preview over the engine's one backend. It is the camera publish's own
    /// (`VideoBridge::capture`): one opener holds the camera.
    #[cfg(feature = "video-core")]
    pub fn video_capture(&self) -> Option<sew_call_client::VideoCapture> {
        self.inner.capture().capture()
    }

    /// The system's devices between calls (desktop).
    #[cfg(all(feature = "audio", not(any(target_os = "ios", target_os = "android"))))]
    fn with_idle_devices(self) -> CallClientEngine {
        self.devices_from(system_devices())
    }

    /// No listing between calls on phones.
    #[cfg(all(feature = "audio", any(target_os = "ios", target_os = "android")))]
    fn with_idle_devices(self) -> CallClientEngine {
        self
    }

    /// Lists the devices between calls with `lister`: first on the first use of the devices
    /// (`devices`, `set_device`, `join` or `device_events`), then every [`IDLE_DEVICE_POLL`]
    /// between calls while something is subscribed to the device events (a change is a
    /// `devices` event). Nothing is listed at launch. The polling thread stops with the engine.
    pub fn devices_from(self, lister: DeviceLister) -> CallClientEngine {
        *lock(&self.inner.lister) = Some(lister);
        self
    }

    /// The first use of the devices: list them once and start the poll (with a lister).
    fn use_devices(&self) {
        let Some(lister) = lock(&self.inner.lister).clone() else {
            return;
        };
        if self.inner.listing.swap(true, Ordering::AcqRel) {
            return;
        }
        let list = lister();
        {
            let mut st = lock(&self.inner.state);
            if st.live().is_none() {
                st.list = list;
            }
        }
        let weak = Arc::downgrade(&self.inner);
        let spawned = std::thread::Builder::new()
            .name("sew-voice-devices".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(idle_device_poll());
                    let Some(inner) = weak.upgrade() else { return };
                    inner.poll_idle_devices();
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "sew-voice: no device polling thread");
        }
    }
}

impl EngineInner {
    fn emit_devices(&self, st: &DevState) {
        let _ = self.devices_tx.send(st.wire());
    }

    fn poll_idle_devices(&self) {
        let Some(lister) = lock(&self.lister).clone() else {
            return;
        };
        // In a call kessel-audio has the list; with nobody subscribed a change tells no one.
        if self.devices_tx.receiver_count() == 0 || lock(&self.state).live().is_some() {
            return;
        }
        let list = lister();
        let mut st = lock(&self.state);
        if st.live().is_none() && st.list != list {
            st.list = list;
            self.emit_devices(&st);
        }
    }

    /// kessel-audio's list changed during a call.
    fn call_devices(&self, list: kessel_audio::DeviceList) {
        let mut st = lock(&self.state);
        if st.list != list {
            st.list = list;
            self.emit_devices(&st);
        }
    }

    /// kessel-audio switched devices on its own (a chosen device went away: it falls back to
    /// the default). The choice is then the OS default again.
    fn call_devices_in_use(&self, in_use: &kessel_audio::DevicesInUse) {
        let mut st = lock(&self.state);
        let mut changed = false;
        if let (Some(want), Some(got)) = (&st.input, &in_use.input)
            && *want != got.id
        {
            st.input = None;
            changed = true;
        }
        if let (Some(want), Some(got)) = (&st.output, &in_use.output)
            && *want != got.id
        {
            st.output = None;
            changed = true;
        }
        if changed {
            self.emit_devices(&st);
        }
    }

    /// kessel-audio for `call` on the chosen devices.
    fn start_audio(&self, call: &Call) -> Result<AudioBridge, ErrorInfo> {
        let (input, output) = {
            let st = lock(&self.state);
            (st.input.clone(), st.output.clone())
        };
        let cfg = (self.audio)(choice(input), choice(output));
        AudioBridge::start_with_session(call, cfg, &self.session)
            .map_err(|e| ErrorInfo::with_detail("mic_unavailable", e.to_string()))
    }

    fn call_over(&self, call: &Arc<CallShared>) {
        let mut st = lock(&self.state);
        if st.live().is_some_and(|l| Arc::ptr_eq(&l, call)) {
            st.live = None;
        }
    }
}

/// The audio for a call on this platform (`sew_call_client::platform_audio_config`): the
/// system's devices through cpal on desktop; on iOS and Android (sew-call-client's `mobile`)
/// the OS's voice processing, with its echo cancellation and noise suppression on.
#[cfg(feature = "audio")]
pub fn system_audio() -> AudioConfig {
    Arc::new(sew_call_client::platform_audio_config)
}

/// The system's device list through cpal, with kessel-audio's ids and names (desktop only: on
/// iOS and Android the OS owns the route).
#[cfg(all(feature = "audio", not(any(target_os = "ios", target_os = "android"))))]
pub fn system_devices() -> DeviceLister {
    Arc::new(|| {
        use cpal::traits::{DeviceTrait, HostTrait};
        let host = cpal::default_host();
        let id_of = |d: &cpal::Device| d.id().ok().map(|i| i.to_string());
        let info = |d: &cpal::Device, default: Option<&str>| {
            let id = id_of(d)?;
            let name = d
                .description()
                .map(|x| x.name().to_string())
                .unwrap_or_else(|_| id.clone());
            Some(kessel_audio::DeviceInfo {
                is_default: default == Some(id.as_str()),
                id,
                name,
            })
        };
        let def_in = host.default_input_device().and_then(|d| id_of(&d));
        let def_out = host.default_output_device().and_then(|d| id_of(&d));
        kessel_audio::DeviceList {
            inputs: host
                .input_devices()
                .map(|it| it.filter_map(|d| info(&d, def_in.as_deref())).collect())
                .unwrap_or_default(),
            outputs: host
                .output_devices()
                .map(|it| it.filter_map(|d| info(&d, def_out.as_deref())).collect())
                .unwrap_or_default(),
        }
    })
}

fn choice(id: Option<String>) -> DeviceChoice {
    id.map_or(DeviceChoice::Default, DeviceChoice::Id)
}

/// The plugin's choice as kessel-audio's.
fn audio_choice(c: &Choice) -> DeviceChoice {
    match c {
        Choice::Default => DeviceChoice::Default,
        Choice::Id(id) => DeviceChoice::Id(id.clone()),
    }
}

/// The choice as the `devices` list keeps it (`None`: the OS default).
fn kept(c: Choice) -> Option<String> {
    match c {
        Choice::Default => None,
        Choice::Id(id) => Some(id),
    }
}

impl CallClientEngine {
    /// [`VoiceEngine::video_codec_preference`] with a video backend.
    #[cfg(feature = "video-core")]
    fn codec_readback(&self) -> Option<crate::wire::CodecPreferenceInfo> {
        let setting = self.inner.client.video_codec_preference();
        let b = self.inner.video.backend()?;
        let detected = sew_call_client::h264_encode_detection(&b.capabilities());
        let live = lock(&self.inner.state).live();
        let h264_allowed = live
            .and_then(|sh| {
                let g = sh.call.try_read().ok()?;
                g.as_ref()?.media_policy()
            })
            .is_none_or(|p| p.codecs.contains(&sew_proto::voice::VideoCodec::H264));
        let (codec, reason) =
            sew_call_client::effective_publish_codec_under_policy(setting, detected, h264_allowed);
        let effective = match codec {
            sew_call_client::VideoCodec::H264 => VideoCodec::H264,
            _ => VideoCodec::Vp8,
        };
        Some(crate::wire::CodecPreferenceInfo::new(
            setting, effective, reason,
        ))
    }

    /// [`VoiceEngine::video_codec_preference`] without video: none.
    #[cfg(not(feature = "video-core"))]
    fn codec_readback(&self) -> Option<crate::wire::CodecPreferenceInfo> {
        None
    }
}

impl CallClientEngine {
    /// The video codecs this engine declares it decodes ([`CallClient::set_video_decode`]):
    /// its video backend's OS decoders (the capabilities' decode entries, read now) and the
    /// webview's (`webview`, the join's `decode`). Nothing else is assumed; `CallClient` drops
    /// H.264 on Linux and under Prefer VP8.
    fn declared_decode(&self, webview: Option<&[VideoCodec]>) -> Vec<sew_call_client::VideoCodec> {
        #[cfg(feature = "video-core")]
        let mut codecs: Vec<VideoCodec> = self
            .inner
            .video
            .backend()
            .map(|b| crate::kessel::decode_codecs(b.as_ref()))
            .unwrap_or_default();
        #[cfg(not(feature = "video-core"))]
        let mut codecs: Vec<VideoCodec> = Vec::new();
        codecs.extend(webview.unwrap_or_default());
        let mut out: Vec<sew_call_client::VideoCodec> = codecs
            .into_iter()
            .map(|c| match c {
                VideoCodec::Vp8 => sew_call_client::VideoCodec::Vp8,
                VideoCodec::H264 => sew_call_client::VideoCodec::H264,
            })
            .collect();
        out.sort_by_key(|c| c.as_str());
        out.dedup();
        out
    }
}

impl VoiceEngine for CallClientEngine {
    fn join(
        &self,
        provider: Arc<dyn JoinProvider>,
        req: JoinRequest,
    ) -> BoxFuture<'_, Result<Arc<dyn CallHandle>, EngineError>> {
        self.join_decoding(provider, req, None)
    }

    /// The join, declaring what this engine decodes first ([`CallClient::set_video_decode`]:
    /// its backend's OS decoders and the webview's `decode`), so the call's media connection
    /// offers to receive only those (VP8 always; H.264 never on Linux).
    fn join_decoding(
        &self,
        provider: Arc<dyn JoinProvider>,
        req: JoinRequest,
        decode: Option<Vec<VideoCodec>>,
    ) -> BoxFuture<'_, Result<Arc<dyn CallHandle>, EngineError>> {
        Box::pin(async move {
            self.use_devices();
            let declared = self.declared_decode(decode.as_deref());
            tracing::info!(
                decode = ?declared,
                webview = ?decode,
                "sew-voice: the video codecs this device decodes"
            );
            self.inner.client.set_video_decode(&declared);
            let room = Arc::new(Mutex::new(None));
            let recorder: Arc<dyn JoinProvider> = Arc::new(RoomRecorder {
                inner: provider,
                room: Arc::clone(&room),
            });
            let call = self
                .inner
                .client
                .join(recorder, req)
                .await
                .map_err(join_error)?;
            // Held for the recording notice: no microphone until the person answered.
            let held = matches!(
                call.snapshot().state,
                sew_call_client::CallState::ConsentRequired { .. }
            );
            let bridge = if held {
                None
            } else {
                match self.inner.start_audio(&call) {
                    Ok(b) => Some(b),
                    Err(e) => {
                        let _ = call.leave().await;
                        return Err(EngineError::Failed(e));
                    }
                }
            };
            let real = RealCall::start(call, bridge, room, Arc::downgrade(&self.inner));
            {
                let mut st = lock(&self.inner.state);
                st.live = Some(Arc::downgrade(&real.shared));
                // A held join has no kessel-audio yet: the list stays until it starts.
                if let Some(list) = real.shared.with_bridge(|b| b.engine().devices()) {
                    st.list = list;
                }
                self.inner.emit_devices(&st);
            }
            let handle: Arc<dyn CallHandle> = Arc::new(real);
            Ok(handle)
        })
    }

    fn devices(&self) -> Result<DeviceList, EngineError> {
        self.use_devices();
        let live = lock(&self.inner.state).live();
        let list = match live {
            Some(call) => call.with_bridge(|b| b.engine().devices()),
            None => lock(&self.inner.lister).clone().map(|l| l()),
        };
        let mut st = lock(&self.inner.state);
        if let Some(list) = list {
            st.list = list;
        }
        Ok(st.wire())
    }

    fn set_device(&self, input: Option<Choice>, output: Option<Choice>) -> Result<(), EngineError> {
        self.use_devices();
        let live = {
            let st = lock(&self.inner.state);
            if let Some(Choice::Id(id)) = &input
                && !st.list.inputs.iter().any(|d| &d.id == id)
            {
                return Err(EngineError::Failed(ErrorInfo::with_detail(
                    "device_not_found",
                    json!({ "input": id }),
                )));
            }
            if let Some(Choice::Id(id)) = &output
                && !st.list.outputs.iter().any(|d| &d.id == id)
            {
                return Err(EngineError::Failed(ErrorInfo::with_detail(
                    "device_not_found",
                    json!({ "output": id }),
                )));
            }
            st.live()
        };
        if let Some(call) = live {
            // kessel-audio's DeviceChoice::Default: the OS default, followed when it changes.
            let sel = DeviceSelection {
                input: input.as_ref().map(audio_choice),
                output: output.as_ref().map(audio_choice),
            };
            if let Some(Err(e)) = call.with_bridge(|b| b.engine().set_device(sel)) {
                return Err(EngineError::Failed(audio_error(&e)));
            }
        }
        let mut st = lock(&self.inner.state);
        if let Some(c) = input {
            st.input = kept(c);
        }
        if let Some(c) = output {
            st.output = kept(c);
        }
        self.inner.emit_devices(&st);
        Ok(())
    }

    fn device_events(&self) -> broadcast::Receiver<DeviceList> {
        let rx = self.inner.devices_tx.subscribe();
        self.use_devices();
        rx
    }

    /// To the engine's `AudioSession`: the live call's kessel-audio gets it now (on phones:
    /// `session_activated` / `session_deactivated`, `suspend` / `resume`, `route_changed`), and
    /// an early `Activated` or `Suspend` is kept for the next call. On desktop it changes
    /// nothing. The plugin never configures `AVAudioSession` or the Android audio mode itself.
    fn audio_session(&self, event: AudioSessionEvent) {
        self.inner.session.event(session_event(event));
    }

    fn forget_server(
        &self,
        install_id: u64,
    ) -> BoxFuture<'_, Result<Vec<DeviceKeyId>, EngineError>> {
        Box::pin(async move {
            self.inner
                .client
                .forget_server(install_id)
                .await
                .map_err(device_error)
        })
    }

    fn known_device_keys(&self) -> BoxFuture<'_, Result<Vec<DeviceKeyId>, EngineError>> {
        Box::pin(async move {
            let inner = self.inner.clone();
            tokio::task::spawn_blocking(move || inner.client.known_device_keys())
                .await
                .map_err(|e| {
                    EngineError::Failed(ErrorInfo::with_detail(code::DEVICE_STORE, e.to_string()))
                })?
                .map_err(device_error)
        })
    }

    /// Every call the plugin no longer holds (the service calls this with no live call): one
    /// whose `Call` a command still held at its leave, and one still marked live. Each is left
    /// through its leaver (the engine frees the app's call slot) and finished.
    fn release_calls(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let calls: Vec<Arc<CallShared>> = {
                let mut st = lock(&self.inner.state);
                let mut calls: Vec<Arc<CallShared>> =
                    st.lingering.drain(..).filter_map(|w| w.upgrade()).collect();
                calls.extend(st.live());
                calls
            };
            for sh in calls {
                tracing::info!("sew-voice: releasing a call the plugin no longer holds");
                sh.leave_call().await;
                sh.finish();
            }
        })
    }

    /// With `video-core` the engine receives remote video (decoded by the webview's
    /// WebCodecs, or by the decoded fallback's backend: `decode_codecs`); with a video backend
    /// it also has the backend's cameras and screens and publishes them (`publish_codecs`: the
    /// backend's encoders).
    fn video_caps(&self) -> crate::wire::VideoCaps {
        #[cfg(feature = "video-core")]
        {
            let mut caps = self.inner.capture().caps();
            caps.decode_codecs = self
                .inner
                .video
                .backend()
                .map(|b| crate::kessel::decode_codecs(b.as_ref()))
                .unwrap_or_default();
            caps
        }
        #[cfg(not(feature = "video-core"))]
        {
            crate::wire::VideoCaps::default()
        }
    }

    /// The preference is the engine's (`CallClient`'s), so it lasts across calls.
    fn set_video_codec_preference(
        &self,
        pref: crate::wire::VideoCodecPreference,
    ) -> Result<(), EngineError> {
        self.inner.client.set_video_codec_preference(pref);
        Ok(())
    }

    /// The setting, and (with a video backend) what it gives: `effective_publish_codec` over
    /// the backend's H.264 encoder check (`h264_encode_detection`), under the live call's media
    /// policy when there is one. The open of the encoder at a camera's start can still find no
    /// hardware encoder; the readback is the capabilities' answer. `None` without a backend.
    fn video_codec_preference(&self) -> Option<crate::wire::CodecPreferenceInfo> {
        self.codec_readback()
    }

    /// The cameras, the chosen one and the OS permissions, from the video backend.
    #[cfg(feature = "video-core")]
    fn video_devices(&self) -> Result<crate::wire::VideoDevices, EngineError> {
        self.inner.capture().devices()
    }

    /// The camera `camera {on}` and `preview` open when they name none (a camera the backend
    /// doesn't list is `camera_not_found`); a camera published now switches to it at once.
    #[cfg(feature = "video-core")]
    fn set_video_device(&self, camera: Choice) -> Result<(), EngineError> {
        let inner = &self.inner;
        inner.capture().set_device(camera)?;
        let live = lock(&inner.state).live();
        if let Some(sh) = live
            && inner.video.backend().is_some()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            let bridge = inner.video.bridge();
            let device = inner.capture().chosen_camera();
            let weak = Arc::downgrade(inner);
            rt.spawn(async move {
                let g = sh.call.read().await;
                let Some(call) = g.as_ref() else { return };
                match bridge.switch_camera(call, device).await {
                    Ok(sew_call_client::CameraState::On { .. }) => {
                        if let Some(inner) = weak.upgrade() {
                            inner
                                .capture()
                                .camera_used(bridge.camera_device().await.as_deref());
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(code = e.code(), "sew-voice: the camera did not switch");
                        sh.send(CallEvent::Error(publish::error(&e).to_info()));
                    }
                }
            });
        }
        Ok(())
    }

    #[cfg(feature = "video-core")]
    fn video_device_events(&self) -> Option<broadcast::Receiver<crate::wire::VideoDevices>> {
        self.inner.capture().device_events()
    }

    #[cfg(feature = "video-core")]
    fn screen_sources(&self) -> BoxFuture<'_, Result<Vec<crate::wire::ScreenSource>, EngineError>> {
        Box::pin(self.inner.capture().screen_sources())
    }

    /// The camera outside a call. While the camera is published the call's self-view already
    /// feeds `local:camera`: nothing opens (one opener holds the camera).
    #[cfg(feature = "video-core")]
    fn preview(
        &self,
        req: crate::video::PreviewRequest,
    ) -> BoxFuture<'_, Result<Option<crate::video::VideoFrames>, EngineError>> {
        Box::pin(async move {
            if req.on
                && self.inner.video.backend().is_some()
                && matches!(
                    self.inner.video.bridge().camera_state().await,
                    sew_call_client::CameraState::On { .. }
                )
            {
                return Ok(None);
            }
            self.inner.capture().preview(req).await
        })
    }

    #[cfg(feature = "video-core")]
    fn preview_keyframe(&self) {
        self.inner.capture().preview_keyframe();
    }

    #[cfg(feature = "video-core")]
    fn camera_permission(&self) -> crate::wire::PermissionState {
        self.inner.video.camera_permission()
    }

    #[cfg(feature = "video-core")]
    fn request_camera_permission(&self) -> BoxFuture<'_, crate::wire::PermissionState> {
        Box::pin(self.inner.video.request_camera_permission())
    }

    /// To kessel-video's Android backend (its cameras close in the background); the camera
    /// publish's pause is the call's (`CallHandle::app_scene`, `real/scene.rs`).
    #[cfg(feature = "video-core")]
    fn app_scene(&self, scene: crate::engine::AppScene) {
        self.inner.video.app_scene(scene.is_background());
    }

    #[cfg(feature = "video-core")]
    fn device_orientation(&self, degrees: u32) {
        self.inner.video.device_orientation(degrees);
    }

    #[cfg(feature = "video-core")]
    fn video_background_events(&self) -> Option<broadcast::Receiver<bool>> {
        self.inner
            .video
            .background_events(Arc::downgrade(&self.inner))
    }

    #[cfg(feature = "video-core")]
    fn native_video_backend(&self) -> Option<Arc<dyn sew_call_client::kessel_video::VideoBackend>> {
        self.inner.video.backend()
    }

    #[cfg(feature = "video-core")]
    fn video_decoder(
        &self,
        spec: crate::video::DecoderSpec,
        wake: crate::video::DecoderWake,
    ) -> Result<Box<dyn crate::video::FrameDecoder>, EngineError> {
        let Some(backend) = self.inner.video.backend() else {
            return Err(EngineError::Failed(ErrorInfo::with_detail(
                crate::wire::video_code::DECODED_UNSUPPORTED,
                serde_json::json!({ "codec": spec.codec }),
            )));
        };
        crate::kessel::open_decoder(backend.as_ref(), spec, wake)
    }
}

/// The plugin's audio-session event as sew-call-client's.
fn session_event(event: AudioSessionEvent) -> SessionEvent {
    match event {
        AudioSessionEvent::Activated => SessionEvent::Activated,
        AudioSessionEvent::Deactivated => SessionEvent::Deactivated,
        AudioSessionEvent::Suspend => SessionEvent::Suspend,
        AudioSessionEvent::Resume => SessionEvent::Resume,
        AudioSessionEvent::RouteChanged => SessionEvent::RouteChanged,
    }
}

/// A device store failure as the plugin's error: `in_call`, else `device_store`.
fn device_error(e: DeviceError) -> EngineError {
    match e {
        DeviceError::InCall => EngineError::failed(code::IN_CALL),
        e => EngineError::Failed(ErrorInfo::with_detail(code::DEVICE_STORE, e.to_string())),
    }
}

/// Passes the host's provider through and keeps the answer's call room (the `call_id`).
struct RoomRecorder {
    inner: Arc<dyn JoinProvider>,
    room: Arc<Mutex<Option<String>>>,
}

impl JoinProvider for RoomRecorder {
    fn join(&self, req: JoinRequest) -> BoxFuture<'_, Result<JoinAnswer, ProviderError>> {
        Box::pin(async move {
            let answer = self.inner.join(req).await?;
            *lock(&self.room) = Some(answer.join.room.as_str().to_owned());
            Ok(answer)
        })
    }
}

/// One live call of the real engine.
struct RealCall {
    shared: Arc<CallShared>,
}

impl std::fmt::Debug for RealCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealCall").finish_non_exhaustive()
    }
}

struct CallShared {
    call: RwLock<Option<Call>>,
    /// Leaves the call without the lock above ([`CallShared::leave_call`]).
    leaver: CallLeaver,
    /// The call's `Call::local_id` (its video pipelines are closed by it).
    #[cfg_attr(not(feature = "video-core"), allow(dead_code))]
    call_id: u64,
    bridge: Mutex<Option<AudioBridge>>,
    /// kessel-audio was started (a held join starts it after the answer; once only).
    audio_started: AtomicBool,
    /// The engine's recording, and the `recording` event last sent.
    recording: Mutex<(Option<CallRecording>, Option<RecordingInfo>)>,
    /// The plugin's events; dropped when the engine's stream closes (the plugin then ends the
    /// call `engine_stopped`).
    tx: Mutex<Option<broadcast::Sender<CallEvent>>>,
    room: Arc<Mutex<Option<String>>>,
    /// The last snapshot, for when the call is being left.
    last: Mutex<CallSnapshot>,
    engine: Weak<EngineInner>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// The last stats window's quality (0.4.1).
    quality: Mutex<QualityInfo>,
    /// The call's video receive (with `video-core`).
    video: video::CallVideo,
    /// The iPhone screen share's armed ring (`ios-screen-ring`).
    #[cfg(all(
        feature = "ios-screen-ring",
        feature = "video-core",
        any(target_os = "ios", target_os = "linux")
    ))]
    ring: ring::RingSlot,
    /// The camera publish's pause in the app's background (`real/scene.rs`).
    #[cfg(feature = "video-core")]
    scene: std::sync::OnceLock<Arc<scene::ScenePause>>,
}

impl RealCall {
    /// `camera {on, device?}`; an off with `off_reason` says it as `local_video`'s reason.
    #[cfg(feature = "video-core")]
    async fn camera_with(
        &self,
        req: crate::video::CameraRequest,
        off_reason: Option<&'static str>,
    ) -> Result<(), EngineError> {
        let engine = self
            .shared
            .engine
            .upgrade()
            .ok_or_else(|| EngineError::failed(crate::wire::code::NOT_IN_CALL))?;
        let bridge = self
            .shared
            .video_bridge()
            .ok_or_else(|| EngineError::failed(crate::wire::video_code::VIDEO_UNSUPPORTED))?;
        let g = self.shared.call.read().await;
        let call = g
            .as_ref()
            .ok_or_else(|| EngineError::failed(crate::wire::code::NOT_IN_CALL))?;
        let chosen = engine.capture().camera_for_open();
        let done = publish::camera(&bridge, call, &req, chosen, off_reason).await;
        if done.is_ok() && req.on {
            // The camera that gave frames: the next open without a choice prefers it.
            engine
                .capture()
                .camera_used(bridge.camera_device().await.as_deref());
        }
        // The pause takes the call's lock itself.
        drop(g);
        if done.is_ok()
            && let Some(scene) = self.shared.scene.get()
        {
            if req.on {
                scene.camera_started().await;
            } else {
                scene.camera_stopped().await;
            }
        }
        done
    }
}

impl RealCall {
    fn start(
        call: Call,
        bridge: Option<AudioBridge>,
        room: Arc<Mutex<Option<String>>>,
        engine: Weak<EngineInner>,
    ) -> RealCall {
        let (tx, _) = broadcast::channel(256);
        let rx = call.events();
        let snap = call.snapshot();
        let last = map_snapshot(&snap);
        // Video only in a plugin built with `video-core`: without it the plugin's video seam
        // keeps its defaults (no frames).
        let frames = if cfg!(feature = "video-core") {
            call.video_frames()
        } else {
            None
        };
        let leaver = call.leaver();
        let call_id = call.local_id();
        let shared = Arc::new(CallShared {
            call: RwLock::new(Some(call)),
            leaver,
            call_id,
            audio_started: AtomicBool::new(bridge.is_some()),
            bridge: Mutex::new(bridge),
            recording: Mutex::new((snap.recording.clone(), last.recording.clone())),
            tx: Mutex::new(Some(tx)),
            room,
            last: Mutex::new(last),
            engine,
            tasks: Mutex::new(Vec::new()),
            quality: Mutex::new(QualityInfo::unknown()),
            video: video::CallVideo::default(),
            #[cfg(all(
                feature = "ios-screen-ring",
                feature = "video-core",
                any(target_os = "ios", target_os = "linux")
            ))]
            ring: ring::RingSlot::default(),
            #[cfg(feature = "video-core")]
            scene: std::sync::OnceLock::new(),
        });
        #[cfg(feature = "video-core")]
        {
            let ops = Arc::new(SceneOps {
                shared: Arc::downgrade(&shared),
            });
            let _ = shared.scene.set(scene::ScenePause::new(
                ops,
                tokio::runtime::Handle::current(),
            ));
        }
        let forward = tokio::spawn(forward(Arc::downgrade(&shared), rx));
        let audio = tokio::spawn(audio(Arc::downgrade(&shared)));
        lock(&shared.tasks).extend([forward, audio]);
        // This device's own camera and screen: the bridge's self-view into the same frame
        // channel, and its sources' pauses as `local_video`.
        #[cfg(feature = "video-core")]
        let (self_view, notes) = match shared.video_bridge() {
            Some(b) if frames.is_some() => (Some(b.self_view()), Some(b.events())),
            _ => (None, None),
        };
        #[cfg(not(feature = "video-core"))]
        let self_view = None;
        if let Some(t) = shared
            .video
            .start(frames, self_view, Arc::downgrade(&shared))
        {
            lock(&shared.tasks).push(t);
        }
        #[cfg(feature = "video-core")]
        if let Some(rx) = notes {
            let t = tokio::spawn(source_notes(Arc::downgrade(&shared), rx));
            lock(&shared.tasks).push(t);
        }
        RealCall { shared }
    }
}

impl Drop for RealCall {
    fn drop(&mut self) {
        for t in lock(&self.shared.tasks).drain(..) {
            t.abort();
        }
        self.shared.finish();
    }
}

impl CallShared {
    fn send(&self, ev: CallEvent) {
        if let Some(tx) = lock(&self.tx).as_ref() {
            let _ = tx.send(ev);
        }
    }

    fn with_bridge<T>(&self, f: impl FnOnce(&AudioBridge) -> T) -> Option<T> {
        lock(&self.bridge).as_ref().map(f)
    }

    /// A held join was let in: kessel-audio starts now (once). If it cannot, the call is left
    /// (`mic_unavailable`), as a join that could not open the microphone: `false`.
    async fn start_audio_late(self: &Arc<Self>) -> bool {
        if self.audio_started.swap(true, Ordering::AcqRel) {
            return true;
        }
        let Some(engine) = self.engine.upgrade() else {
            return true;
        };
        let started = {
            let g = self.call.read().await;
            match g.as_ref() {
                Some(call) => engine.start_audio(call),
                None => return true,
            }
        };
        match started {
            Ok(bridge) => {
                let list = bridge.engine().devices();
                *lock(&self.bridge) = Some(bridge);
                let mut st = lock(&engine.state);
                st.list = list;
                engine.emit_devices(&st);
                true
            }
            Err(e) => {
                self.send(CallEvent::Error(e.clone()));
                self.leave_call().await;
                *lock(&self.last) = CallSnapshot {
                    state: CallState::Ended {
                        reason: e.code.clone(),
                    },
                    participants: Vec::new(),
                    recording: None,
                };
                self.send(CallEvent::State(CallState::Ended { reason: e.code }));
                self.finish();
                false
            }
        }
    }

    /// The `recording` event for the engine's recording (`rec`, when it changed) and roster
    /// (`participants`, for `recorder_present`): sent when it changed.
    fn update_recording(
        &self,
        rec: Option<Option<CallRecording>>,
        participants: &[sew_call_client::Participant],
    ) {
        let info = {
            let mut r = lock(&self.recording);
            if let Some(rec) = rec {
                r.0 = rec;
            }
            let info = map_recording(r.0.as_ref(), participants);
            if info == r.1 {
                return;
            }
            r.1.clone_from(&info);
            info
        };
        self.send(CallEvent::Recording(info));
    }

    /// The engine's roster now (empty while the call is being left).
    fn engine_participants(&self) -> Vec<sew_call_client::Participant> {
        self.engine_snapshot()
            .map(|s| s.participants)
            .unwrap_or_default()
    }

    /// The app's video publisher (`None` without a video backend, or once the engine is gone).
    #[cfg(feature = "video-core")]
    fn video_bridge(&self) -> Option<Arc<sew_call_client::VideoBridge>> {
        let engine = self.engine.upgrade()?;
        engine.video.backend()?;
        Some(engine.video.bridge())
    }

    /// The engine's snapshot now, when the call is not being left.
    fn engine_snapshot(&self) -> Option<sew_call_client::CallSnapshot> {
        let g = self.call.try_read().ok()?;
        g.as_ref().map(Call::snapshot)
    }

    fn snapshot(&self) -> CallSnapshot {
        match self.engine_snapshot() {
            Some(s) => {
                let s = map_snapshot(&s);
                *lock(&self.last) = s.clone();
                s
            }
            None => lock(&self.last).clone(),
        }
    }

    /// Leaves the engine's call through its leaver, so a camera or screen command holding the
    /// `Call`'s lock (waiting on a device) never holds the leave up: the engine ends the call
    /// and frees the app's call slot, and that command then answers `ended`. The engine's
    /// answer is awaited at most [`LEAVE_WAIT`] (logged when it fires). The `Call` is taken now
    /// when nothing holds it, else by a task once that command ends (logged when that takes
    /// longer than [`TAKE_WAIT`]; [`VoiceEngine::release_calls`] can reach it meanwhile).
    async fn leave_call(self: &Arc<Self>) {
        // `Ended` only: the call was already over, which is what a leave wants.
        if tokio::time::timeout(LEAVE_WAIT, self.leaver.leave())
            .await
            .is_err()
        {
            tracing::warn!(
                waited_s = LEAVE_WAIT.as_secs(),
                "sew-voice: the engine's leave did not answer in time"
            );
        }
        if let Ok(mut g) = self.call.try_write() {
            drop(g.take());
            return;
        }
        tracing::info!(
            "sew-voice: a camera or screen command still holds the call that was left; it is taken when that command ends"
        );
        if let Some(engine) = self.engine.upgrade() {
            lock(&engine.state).lingering.push(Arc::downgrade(self));
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let began = std::time::Instant::now();
            let call = me.call.write().await.take();
            drop(call);
            if began.elapsed() > TAKE_WAIT {
                tracing::warn!(
                    waited_ms = began.elapsed().as_millis() as u64,
                    "sew-voice: a command held the call that was left this long"
                );
            }
        });
    }

    /// The call is over: close the audio (the microphone) and free the app's call slot.
    fn finish(self: &Arc<Self>) {
        drop(lock(&self.bridge).take());
        self.video.clear();
        #[cfg(feature = "video-core")]
        if let Some(bridge) = self.video_bridge() {
            // This call's pipelines stop (the call already took its publications with it); a
            // later call's keep running.
            let id = self.call_id;
            tokio::spawn(async move { bridge.close_call(id).await });
        }
        if let Some(engine) = self.engine.upgrade() {
            engine.call_over(self);
        }
    }
}

impl CallHandle for RealCall {
    fn call_id(&self) -> String {
        lock(&self.shared.room).clone().unwrap_or_default()
    }

    fn events(&self) -> broadcast::Receiver<CallEvent> {
        match lock(&self.shared.tx).as_ref() {
            Some(tx) => tx.subscribe(),
            // The engine's stream is gone: a receiver that is closed at once.
            None => broadcast::channel(1).1,
        }
    }

    fn snapshot(&self) -> CallSnapshot {
        self.shared.snapshot()
    }

    fn moved_room(&self) -> Option<String> {
        let moved = self.shared.engine_snapshot()?.moved?;
        Some(moved.room().to_owned())
    }

    fn consent(
        &self,
        granted: bool,
        notice_version: u32,
    ) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(async move {
            let g = self.shared.call.read().await;
            let call = g
                .as_ref()
                .ok_or_else(|| EngineError::failed(crate::wire::code::NOT_IN_CALL))?;
            call.consent(granted, notice_version)
                .await
                .map_err(consent_error)
        })
    }

    fn withdraw_consent(&self) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(async move {
            let g = self.shared.call.read().await;
            let call = g
                .as_ref()
                .ok_or_else(|| EngineError::failed(crate::wire::code::NOT_IN_CALL))?;
            call.withdraw_consent().await.map_err(consent_error)
        })
    }

    fn security(&self) -> Option<SecurityInfo> {
        let sec = self.shared.engine_snapshot()?.security?;
        Some(SecurityInfo::from(&sec))
    }

    fn roster(&self) -> Vec<RosterEntry> {
        roster_of(&self.shared.engine_participants())
    }

    fn quality(&self) -> QualityInfo {
        lock(&self.shared.quality).clone()
    }

    fn video_frames(&self) -> Option<crate::video::VideoFrames> {
        self.shared.video.take_frames()
    }

    fn video_snapshot(&self) -> crate::engine::VideoSnapshot {
        let mut v = video::snapshot(self.shared.engine_snapshot().as_ref());
        v.local = self.shared.video.local();
        if let Ok(g) = self.shared.call.try_read()
            && let Some(call) = g.as_ref()
        {
            v.policy = call
                .media_policy()
                .and_then(|p| serde_json::to_value(p).ok());
        }
        v
    }

    #[cfg(feature = "video-core")]
    fn camera(&self, req: crate::video::CameraRequest) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(self.camera_with(req, None))
    }

    #[cfg(feature = "video-core")]
    fn camera_off_because(&self, reason: &'static str) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(self.camera_with(
            crate::video::CameraRequest {
                on: false,
                device: None,
            },
            Some(reason),
        ))
    }

    /// The camera publish pauses in the background and resumes with a keyframe on the return
    /// (`real/scene.rs`); the webview sinks are the plugin's.
    #[cfg(feature = "video-core")]
    fn app_scene(&self, scene: crate::engine::AppScene) {
        if let Some(s) = self.shared.scene.get() {
            s.scene(scene.is_background());
        }
    }

    #[cfg(feature = "video-core")]
    fn screen(&self, req: crate::video::ScreenRequest) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(async move {
            let engine = self
                .shared
                .engine
                .upgrade()
                .ok_or_else(|| EngineError::failed(crate::wire::code::NOT_IN_CALL))?;
            let bridge = self
                .shared
                .video_bridge()
                .ok_or_else(|| EngineError::failed(crate::wire::video_code::SCREEN_UNSUPPORTED))?;
            let g = self.shared.call.read().await;
            let call = g
                .as_ref()
                .ok_or_else(|| EngineError::failed(crate::wire::code::NOT_IN_CALL))?;
            publish::screen(&bridge, engine.capture(), call, &req).await
        })
    }

    #[cfg(all(
        feature = "ios-screen-ring",
        feature = "video-core",
        any(target_os = "ios", target_os = "linux")
    ))]
    fn screen_share_ring(
        &self,
        container: std::path::PathBuf,
    ) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(ring::arm(&self.shared, container))
    }

    fn set_video_settings(&self, track: &str, settings: crate::video::TrackSettings) {
        // Never blocks: a leave holding the write lock is taking the call away.
        let g = self.shared.call.try_read().ok();
        let call = g.as_ref().and_then(|g| g.as_ref());
        self.shared.video.set_settings(call, track, settings);
    }

    /// A remote track's keyframe goes to its engine sink; a self-view's to the publication's
    /// self-view layer.
    fn request_keyframe(&self, track: &str) {
        #[cfg(feature = "video-core")]
        if crate::video::is_local_track(track) {
            if let Some(bridge) = self.shared.video_bridge() {
                bridge.self_view_keyframe(if track == crate::video::LOCAL_CAMERA {
                    sew_call_client::VideoSource::Camera
                } else {
                    sew_call_client::VideoSource::ScreenShare
                });
            }
            return;
        }
        self.shared.video.request_keyframe(track);
    }

    fn video_sink_stats(&self, track: &str, stats: crate::wire::SinkStats) {
        self.shared.video.sink_stats(track, stats);
    }

    fn network_changed(&self) {
        // Sync and never blocks: `Call::network_changed` only queues the command. A leave
        // holding the write lock is taking the call away, so there is nothing to move.
        if let Ok(g) = self.shared.call.try_read()
            && let Some(call) = g.as_ref()
        {
            call.network_changed();
        }
    }

    fn mute(
        &self,
        muted: Option<bool>,
        deafened: Option<bool>,
    ) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(async move {
            let g = self.shared.call.read().await;
            let call = g
                .as_ref()
                .ok_or_else(|| EngineError::failed(crate::wire::code::NOT_IN_CALL))?;
            call.mute(muted, deafened).await.map_err(|e| match e {
                CallError::Ended => EngineError::failed(crate::wire::code::NOT_IN_CALL),
                e => join_error(e),
            })
        })
    }

    fn leave(&self) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(async move {
            self.shared.leave_call().await;
            *lock(&self.shared.last) = CallSnapshot {
                state: CallState::Ended {
                    reason: reason::LEFT.into(),
                },
                participants: Vec::new(),
                recording: None,
            };
            self.shared.finish();
            Ok(())
        })
    }
}

/// The real call's side of the camera's background pause.
#[cfg(feature = "video-core")]
struct SceneOps {
    shared: Weak<CallShared>,
}

#[cfg(feature = "video-core")]
impl scene::CameraOps for SceneOps {
    fn pause(&self, paused: bool) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            let Some(sh) = self.shared.upgrade() else {
                return false;
            };
            let Some(bridge) = sh.video_bridge() else {
                return false;
            };
            let g = sh.call.read().await;
            let Some(call) = g.as_ref() else {
                return false;
            };
            let camera = sew_call_client::VideoSource::Camera;
            if paused {
                // The frames stop first, then subscribers see the track muted.
                let on = bridge.pause_camera(true).await;
                if on {
                    call.set_video_muted(camera, true);
                }
                on
            } else {
                call.set_video_muted(camera, false);
                bridge.pause_camera(false).await
            }
        })
    }

    fn unpublish(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let Some(sh) = self.shared.upgrade() else {
                return;
            };
            let Some(bridge) = sh.video_bridge() else {
                return;
            };
            let g = sh.call.read().await;
            let Some(call) = g.as_ref() else {
                return;
            };
            sh.video.background_off();
            tracing::info!("sew-voice: the camera turns off after its time in the background");
            if let Err(e) = bridge
                .set_camera(
                    call,
                    sew_call_client::CameraRequest::off().with_reason(video::BACKGROUND),
                )
                .await
            {
                tracing::info!(error = %e, "sew-voice: the background's camera off failed");
            }
        })
    }

    fn paused_changed(&self, paused: bool) {
        if let Some(sh) = self.shared.upgrade() {
            let local = sh.video.set_camera_paused(paused);
            sh.send(CallEvent::LocalVideo(local));
        }
    }
}

/// The engine's events in the plugin's shapes, while the call lives.
async fn forward(
    shared: Weak<CallShared>,
    mut rx: broadcast::Receiver<sew_call_client::CallEvent>,
) {
    use sew_call_client::CallEvent as E;
    loop {
        let ev = rx.recv().await;
        let Some(sh) = shared.upgrade() else { return };
        match ev {
            Ok(E::State(s)) => {
                let state = map_state(&s);
                let ended = matches!(state, CallState::Ended { .. });
                let held = matches!(state, CallState::ConsentRequired { .. });
                sh.send(CallEvent::State(state));
                if ended {
                    sh.finish();
                    return;
                }
                if !held && !sh.start_audio_late().await {
                    return;
                }
            }
            Ok(E::Participants(p)) => {
                sh.send(CallEvent::Participants(map_participants(&p)));
                sh.send(CallEvent::Roster(roster_of(&p)));
                sh.update_recording(None, &p);
            }
            Ok(E::Security(sec)) => sh.send(CallEvent::Security(Some(SecurityInfo::from(&sec)))),
            Ok(E::Stats(w)) => {
                let q = QualityInfo::from_window(&w);
                *lock(&sh.quality) = q.clone();
                sh.send(CallEvent::Quality(q));
            }
            Ok(E::Recording(r)) => {
                let p = sh.engine_participants();
                sh.update_recording(Some(r.map(|b| *b)), &p);
            }
            // The engine's levels are the media server's; kessel-audio's are the plugin's.
            Ok(E::Speaking(_)) => {}
            Ok(E::Error(e)) => sh.send(CallEvent::Error(map_error(&e))),
            Ok(E::Moved(m)) => sh.send(CallEvent::Moved {
                room: m.room().to_owned(),
            }),
            // The whole video list on every change (the snapshot already has it).
            Ok(E::VideoAdded(_) | E::VideoRemoved { .. } | E::VideoChanged(_)) => {
                let list = sh
                    .engine_snapshot()
                    .map(|s| video::tracks(&s))
                    .unwrap_or_default();
                sh.video.forget(&list);
                sh.send(CallEvent::Video(list));
            }
            // This device's camera or screen went on or off.
            Ok(E::LocalVideo(l)) => {
                if let Some(local) = sh.video.local_changed(&l) {
                    sh.send(CallEvent::LocalVideo(local));
                }
            }
            Ok(E::MediaPolicy(p)) => {
                if let Ok(v) = serde_json::to_value(&*p) {
                    sh.send(CallEvent::Policy(v));
                }
            }
            // An event sew-call-client adds later.
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let snap = sh.snapshot();
                let ended = matches!(snap.state, CallState::Ended { .. });
                if let Some(engine) = sh.engine_snapshot() {
                    sh.update_recording(Some(engine.recording.clone()), &engine.participants);
                    sh.send(CallEvent::Roster(roster_of(&engine.participants)));
                    if let Some(sec) = &engine.security {
                        sh.send(CallEvent::Security(Some(SecurityInfo::from(sec))));
                    }
                }
                sh.send(CallEvent::Participants(snap.participants));
                sh.send(CallEvent::State(snap.state));
                if ended {
                    sh.finish();
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => {
                drop(lock(&sh.tx).take());
                sh.finish();
                return;
            }
        }
    }
}

/// The published sources' pauses and resumes (a hidden window, another app took the camera,
/// back) as `local_video`, while the call lives. Their ends come as the engine's `LocalVideo`
/// (the bridge unpublishes them with the source's reason).
#[cfg(feature = "video-core")]
async fn source_notes(
    shared: Weak<CallShared>,
    mut rx: broadcast::Receiver<sew_call_client::SourceNote>,
) {
    use sew_call_client::{CaptureEvent, CaptureKind, VideoSource};
    loop {
        let note = match rx.recv().await {
            Ok(n) => n,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let Some(sh) = shared.upgrade() else { return };
        if note.source == VideoSource::Camera
            && note.event == CaptureEvent::Ended(sew_call_client::CaptureEnd::DeviceLost)
        {
            // The camera went away while sending (a Continuity Camera gone): unpublished by the
            // bridge, `local_video` off with `camera_lost` from the engine; the call goes on.
            sh.send(CallEvent::Error(ErrorInfo::new(
                crate::wire::video_code::CAMERA_LOST,
            )));
            continue;
        }
        if !matches!(note.event, CaptureEvent::Paused(_) | CaptureEvent::Resumed) {
            continue;
        }
        let kind = match note.source {
            VideoSource::Camera => CaptureKind::Camera,
            _ => CaptureKind::Screen,
        };
        if let Some(local) = sh.video.source_changed(kind, &note.event) {
            sh.send(CallEvent::LocalVideo(local));
        }
    }
}

/// kessel-audio's side of the call: its device events and errors, and the `speaking` levels.
async fn audio(shared: Weak<CallShared>) {
    let mut tick = tokio::time::interval(SPEAKING_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last: Vec<Speaking> = Vec::new();
    loop {
        tick.tick().await;
        let Some(sh) = shared.upgrade() else { return };
        let Some((levels, events)) = sh.with_bridge(|b| {
            let rx = b.engine().events();
            let mut events = Vec::new();
            while let Some(e) = rx.try_recv() {
                events.push(e);
            }
            (b.levels(), events)
        }) else {
            // A held join has no audio yet; once started, no bridge means the call is over.
            if sh.audio_started.load(Ordering::Acquire) {
                return;
            }
            continue;
        };
        let engine = sh.engine.upgrade();
        for e in events {
            match e {
                AudioEvent::Devices(list) => {
                    if let Some(engine) = &engine {
                        engine.call_devices(list);
                    }
                }
                AudioEvent::DevicesInUse(in_use) => {
                    if let Some(engine) = &engine {
                        engine.call_devices_in_use(&in_use);
                    }
                }
                AudioEvent::Error(err) => sh.send(CallEvent::Error(audio_error(&err))),
                _ => {}
            }
        }
        let Some(snap) = sh.engine_snapshot() else {
            continue;
        };
        let now = speaking(&levels, &snap.participants);
        if now != last {
            last = now.clone();
            sh.send(CallEvent::Speaking(now));
        }
    }
}

/// -60 dBov and below reads as silence, 0 dBov as 1 (the engine's scale).
fn unit(level_dbov: f32) -> f64 {
    f64::from(((level_dbov + 60.0) / 60.0).clamp(0.0, 1.0))
}

/// Who is speaking now, by member: this device (unless muted) and each remote track that
/// kessel-audio hears above its speaking threshold.
pub(crate) fn speaking(
    levels: &BridgeLevels,
    participants: &[sew_call_client::Participant],
) -> Vec<Speaking> {
    let mut out = Vec::new();
    if levels.local.speaking
        && let Some(me) = participants.iter().find(|p| p.is_self)
        && !me.muted
        && let Some(member) = &me.member_id
    {
        out.push(Speaking {
            member_id: member.clone(),
            level: unit(levels.local.level_dbov),
        });
    }
    for (identity, level) in &levels.remotes {
        if !level.speaking {
            continue;
        }
        if let Some(member) = participants
            .iter()
            .find(|p| !p.is_self && &p.identity == identity)
            .and_then(|p| p.member_id.clone())
        {
            out.push(Speaking {
                member_id: member,
                level: unit(level.level_dbov),
            });
        }
    }
    out
}

pub(crate) fn map_state(s: &sew_call_client::CallState) -> CallState {
    use sew_call_client::CallState as S;
    match s {
        S::Connecting => CallState::Connecting,
        S::Connected => CallState::Connected,
        S::Reconnecting { attempt } => CallState::Reconnecting { attempt: *attempt },
        S::Ended { reason } => CallState::Ended {
            reason: end_reason(reason),
        },
        S::ConsentRequired {
            recording_id,
            notice_version,
        } => CallState::ConsentRequired {
            recording_id: recording_id.clone(),
            notice_version: *notice_version,
        },
        // A state sew-call-client adds later: the call is not up.
        _ => CallState::Connecting,
    }
}

/// The engine's end reasons as `ended.reason` (a refusal's code as given).
pub(crate) fn end_reason(r: &EndReason) -> String {
    match r {
        EndReason::Left => reason::LEFT.into(),
        EndReason::SigninRequired => reason::SIGNIN_REQUIRED.into(),
        EndReason::Network => "network".into(),
        EndReason::E2eeRefused => "e2ee_refused".into(),
        EndReason::CredentialLost => reason::CREDENTIAL_LOST.into(),
        EndReason::CredentialExpired => reason::CREDENTIAL_EXPIRED.into(),
        EndReason::Kicked => reason::KICKED.into(),
        EndReason::RoomClosed => reason::ROOM_CLOSED.into(),
        EndReason::Refused(code) => code.clone(),
        // Sew's voice leave reason, unchanged (Sew 0.7.2).
        EndReason::Removed(why) => why.clone(),
        // A reason sew-call-client adds later.
        _ => reason::ENGINE_STOPPED.into(),
    }
}

/// The engine's participants as the plugin's: those the keys' roster or the call socket has
/// named a member for (the plugin's participants are members; a participant appears once its
/// member is known).
pub(crate) fn map_participants(p: &[sew_call_client::Participant]) -> Vec<Participant> {
    p.iter()
        .filter_map(|p| {
            Some(Participant {
                member_id: p.member_id.clone()?,
                is_self: p.is_self,
                muted: p.muted || p.deafened,
                deafened: p.deafened,
                server_muted: p.server_muted,
                server_deafened: p.server_deafened,
                speak_denied: p.speak_denied,
            })
        })
        .collect()
}

pub(crate) fn map_snapshot(s: &sew_call_client::CallSnapshot) -> CallSnapshot {
    CallSnapshot {
        state: map_state(&s.state),
        participants: map_participants(&s.participants),
        recording: map_recording(s.recording.as_ref(), &s.participants),
    }
}

/// The engine's recording as the `recording` event, with `recorder_present` from its roster.
pub(crate) fn map_recording(
    r: Option<&CallRecording>,
    participants: &[sew_call_client::Participant],
) -> Option<RecordingInfo> {
    r.map(|r| RecordingInfo::from_engine(r, participants.iter().any(|p| p.recorder)))
}

/// A failed `consent` or `withdraw_consent`.
fn consent_error(e: CallError) -> EngineError {
    match e {
        CallError::NoRecording => EngineError::failed(crate::wire::code::NO_RECORDING),
        CallError::Ended => EngineError::failed(crate::wire::code::NOT_IN_CALL),
        e => join_error(e),
    }
}

/// The engine's `{code, detail}`: an empty detail is `null`, a JSON object (a refusal's detail
/// as the provider gave it) is that object, anything else is the text.
pub(crate) fn map_error(e: &sew_call_client::CallErrorEvent) -> ErrorInfo {
    let detail = if e.detail.is_empty() {
        Value::Null
    } else {
        match serde_json::from_str::<Value>(&e.detail) {
            Ok(v @ Value::Object(_)) => v,
            _ => Value::String(e.detail.clone()),
        }
    };
    ErrorInfo {
        code: e.code.clone(),
        detail,
    }
}

/// A kessel-audio device error as the plugin's: `mic_denied` (the OS refused), `device_lost`
/// (unplugged; kessel-audio falls back to the default), `mic_unavailable` otherwise, with
/// kessel-audio's own code in the detail.
pub(crate) fn audio_error(e: &AudioError) -> ErrorInfo {
    let code = match e.code {
        ErrorCode::NoPermission => "mic_denied",
        ErrorCode::DeviceLost => "device_lost",
        _ => "mic_unavailable",
    };
    ErrorInfo::with_detail(
        code,
        json!({ "audio": e.code.as_str(), "detail": e.detail }),
    )
}

/// A failed join or command.
pub(crate) fn join_error(e: CallError) -> EngineError {
    match e {
        CallError::AlreadyInCall => EngineError::failed(crate::wire::code::ALREADY_IN_CALL),
        CallError::Provider(p) => EngineError::Provider(p),
        CallError::Ended => EngineError::failed(reason::ENGINE_STOPPED),
        CallError::Setup(s) => {
            EngineError::Failed(ErrorInfo::with_detail(reason::ENGINE_STOPPED, s))
        }
        // An error sew-call-client adds later.
        e => EngineError::Failed(ErrorInfo::with_detail(
            reason::ENGINE_STOPPED,
            e.to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    // sew-call-client's Participant and BridgeLevels are non_exhaustive: built from Default.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;
    use sew_call_client::kessel_audio::Level;

    fn p(identity: &str, member: Option<&str>, is_self: bool) -> sew_call_client::Participant {
        let mut p = sew_call_client::Participant::default();
        p.identity = identity.into();
        p.member_id = member.map(str::to_owned);
        p.is_self = is_self;
        p
    }

    fn lvl(db: f32, speaking: bool) -> Level {
        Level {
            level_dbov: db,
            speaking,
        }
    }

    #[test]
    fn states_and_end_reasons_map_to_j4() {
        use sew_call_client::CallState as S;
        assert_eq!(map_state(&S::Connecting), CallState::Connecting);
        assert_eq!(map_state(&S::Connected), CallState::Connected);
        assert_eq!(
            map_state(&S::Reconnecting { attempt: 3 }),
            CallState::Reconnecting { attempt: 3 }
        );
        for (r, want) in [
            (EndReason::Left, "left"),
            (EndReason::SigninRequired, "signin_required"),
            (EndReason::Network, "network"),
            (EndReason::E2eeRefused, "e2ee_refused"),
            (EndReason::CredentialLost, "credential_lost"),
            (EndReason::CredentialExpired, "credential_expired"),
            (EndReason::Kicked, "kicked"),
            (EndReason::RoomClosed, "room_closed"),
            (EndReason::Refused("channel_full".into()), "channel_full"),
            (EndReason::Removed("perms_lost".into()), "perms_lost"),
            (
                EndReason::Removed("removed_from_space".into()),
                "removed_from_space",
            ),
            (EndReason::Removed("timed_out".into()), "timed_out"),
            (
                EndReason::Removed("channel_archived".into()),
                "channel_archived",
            ),
            (
                EndReason::Removed("account_removed".into()),
                "account_removed",
            ),
            (
                EndReason::Removed("removed_by_moderator".into()),
                "removed_by_moderator",
            ),
            (
                EndReason::Removed("session_revoked".into()),
                "session_revoked",
            ),
            (
                EndReason::Removed("device_revoked".into()),
                "device_revoked",
            ),
        ] {
            assert_eq!(
                map_state(&S::Ended { reason: r }),
                CallState::Ended {
                    reason: want.into()
                }
            );
        }
    }

    #[test]
    fn participants_are_members_and_deafened_is_muted() {
        let mut me = p("p_me", Some("1001"), true);
        me.deafened = true;
        let unnamed = p("p_new", None, false);
        let mut bob = p("p_bob", Some("1002"), false);
        bob.server_muted = true;
        bob.muted = true;
        let mut eve = p("p_eve", Some("1003"), false);
        eve.speak_denied = true;
        eve.muted = true;
        let out = map_participants(&[me, unnamed, bob, eve]);
        assert_eq!(
            out,
            vec![
                Participant {
                    member_id: "1001".into(),
                    is_self: true,
                    muted: true,
                    deafened: true,
                    server_muted: false,
                    server_deafened: false,
                    speak_denied: false,
                },
                Participant {
                    member_id: "1002".into(),
                    is_self: false,
                    muted: true,
                    deafened: false,
                    server_muted: true,
                    server_deafened: false,
                    speak_denied: false,
                },
                // SPEAK denied is its own flag, not a moderator mute.
                Participant {
                    member_id: "1003".into(),
                    is_self: false,
                    muted: true,
                    deafened: false,
                    server_muted: false,
                    server_deafened: false,
                    speak_denied: true,
                },
            ]
        );
    }

    #[test]
    fn speaking_comes_from_kessel_audio_levels_by_member() {
        let roster = vec![
            p("p_me", Some("1001"), true),
            p("p_bob", Some("1002"), false),
            p("p_eve", Some("1003"), false),
            p("p_new", None, false),
        ];
        let mut levels = BridgeLevels::default();
        levels.local = lvl(-20.0, true);
        levels.remotes = vec![
            ("p_bob".into(), lvl(-30.0, true)),
            ("p_eve".into(), lvl(-80.0, false)),
            ("p_new".into(), lvl(-10.0, true)),
        ];
        let out = speaking(&levels, &roster);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0].member_id, "1001");
        assert!((out[0].level - 40.0 / 60.0).abs() < 1e-6);
        assert_eq!(out[1].member_id, "1002");
        assert!((out[1].level - 0.5).abs() < 1e-6);
        // Muted, this device is never speaking.
        let mut muted = roster.clone();
        muted[0].muted = true;
        assert!(
            speaking(&levels, &muted)
                .iter()
                .all(|s| s.member_id != "1001")
        );
    }

    #[test]
    fn errors_keep_their_codes_and_a_refusal_keeps_its_object() {
        let e = map_error(&sew_call_client::CallErrorEvent::new(
            "turn_unreachable",
            "tls: timed out",
        ));
        assert_eq!(e.code, "turn_unreachable");
        assert_eq!(e.detail, json!("tls: timed out"));
        let e = map_error(&sew_call_client::CallErrorEvent::new(
            "channel_full",
            r#"{"limit":25}"#,
        ));
        assert_eq!(e.detail, json!({"limit": 25}));
        let e = map_error(&sew_call_client::CallErrorEvent::new(
            "key_storage_file",
            String::new(),
        ));
        assert_eq!(e.detail, Value::Null);
    }

    /// An engine whose lister counts its calls and lists `in:1` (the null audio otherwise).
    fn counted() -> (CallClientEngine, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::AtomicUsize;
        let n = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&n);
        let lister: DeviceLister = Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            let mut d = kessel_audio::DeviceList::default();
            d.inputs.push(kessel_audio::DeviceInfo {
                id: "in:1".into(),
                name: "Mic".into(),
                is_default: true,
            });
            d
        });
        let dir = std::env::temp_dir().join("sew-voice-real-unit");
        let client = CallClient::with_mls(sew_call_client::ClientConfig::new("sew.test", dir));
        let audio: AudioConfig = Arc::new(|_, _| EngineConfig::default());
        (CallClientEngine::new(client, audio).devices_from(lister), n)
    }

    fn wait_for(what: &str, mut pred: impl FnMut() -> bool) {
        for _ in 0..500 {
            if pred() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for: {what}");
    }

    /// Nothing is listed at launch; the first device query lists once; the poll runs only
    /// while something is subscribed.
    #[test]
    fn devices_are_listed_from_the_first_use_and_polled_while_subscribed() {
        let (engine, n) = counted();
        std::thread::sleep(idle_device_poll() * 5);
        assert_eq!(
            n.load(Ordering::SeqCst),
            0,
            "no listing before the first use"
        );
        let list = engine.devices().unwrap();
        assert_eq!(list.inputs.len(), 1);
        let after_first = n.load(Ordering::SeqCst);
        assert!(after_first >= 1);
        // Nobody subscribed: no poll.
        std::thread::sleep(idle_device_poll() * 5);
        assert_eq!(
            n.load(Ordering::SeqCst),
            after_first,
            "no poll without a subscriber"
        );
        // Subscribed: polled.
        let rx = engine.device_events();
        wait_for("a poll", || n.load(Ordering::SeqCst) >= after_first + 2);
        drop(rx);
        std::thread::sleep(idle_device_poll() * 3);
        let stopped = n.load(Ordering::SeqCst);
        std::thread::sleep(idle_device_poll() * 5);
        assert_eq!(
            n.load(Ordering::SeqCst),
            stopped,
            "the poll stops with the subscriber"
        );
    }

    /// The phone's audio session goes to the engine's `AudioSession` with or without a call
    /// (kept for the next call), mapped one to one.
    #[test]
    fn audio_session_events_reach_the_engines_session() {
        let (engine, _) = counted();
        assert_eq!(engine.inner.session.is_active(), None);
        engine.audio_session(AudioSessionEvent::Activated);
        engine.audio_session(AudioSessionEvent::Suspend);
        assert_eq!(engine.inner.session.is_active(), Some(true));
        assert!(engine.inner.session.is_suspended());
        engine.audio_session(AudioSessionEvent::Resume);
        engine.audio_session(AudioSessionEvent::RouteChanged);
        engine.audio_session(AudioSessionEvent::Deactivated);
        assert_eq!(engine.inner.session.is_active(), Some(false));
        assert!(!engine.inner.session.is_suspended());
        for (a, b) in [
            (AudioSessionEvent::Activated, SessionEvent::Activated),
            (AudioSessionEvent::Deactivated, SessionEvent::Deactivated),
            (AudioSessionEvent::Suspend, SessionEvent::Suspend),
            (AudioSessionEvent::Resume, SessionEvent::Resume),
            (AudioSessionEvent::RouteChanged, SessionEvent::RouteChanged),
        ] {
            assert_eq!(session_event(a), b);
        }
    }

    /// What the engine declares it decodes before a join: its backend's decoders and the
    /// webview's, nothing assumed; and the codec preference, kept by the client across calls,
    /// read back over the backend's H.264 encoder check (the fake's H.264 is software: never
    /// usable, so Prefer H.264 says `not_detected`).
    #[cfg(feature = "video-core")]
    #[test]
    fn the_declared_decoders_and_the_codec_preference() {
        use crate::wire::{CodecPreferenceReason, VideoCodecPreference};
        use sew_call_client::VideoCodec as E;
        let dir = std::env::temp_dir().join("sew-voice-real-codecs");
        let audio: AudioConfig = Arc::new(|_, _| EngineConfig::default());
        let bare = CallClientEngine::new(
            CallClient::with_mls(sew_call_client::ClientConfig::new("sew.test", dir.clone())),
            Arc::clone(&audio),
        );
        assert_eq!(
            bare.declared_decode(None),
            Vec::<E>::new(),
            "nothing assumed"
        );
        assert_eq!(
            bare.declared_decode(Some(&[VideoCodec::H264])),
            vec![E::H264],
            "the webview's"
        );
        assert_eq!(
            bare.video_codec_preference(),
            None,
            "no backend, no publish"
        );

        let mut cfg = sew_call_client::kessel_video::fake::FakeConfig::default();
        cfg.codec = sew_call_client::kessel_video::fake::FakeCodec::None;
        let vp8_only = sew_call_client::kessel_video::fake::FakeBackend::new(cfg);
        let engine = CallClientEngine::new(
            CallClient::with_mls(sew_call_client::ClientConfig::new("sew.test", dir)),
            audio,
        )
        .with_video_backend(vp8_only);
        assert_eq!(engine.declared_decode(None), vec![E::Vp8], "the backend's");
        assert_eq!(
            engine.declared_decode(Some(&[VideoCodec::H264, VideoCodec::Vp8])),
            vec![E::H264, E::Vp8]
        );
        let auto = engine.video_codec_preference().expect("a readback");
        assert_eq!(
            (auto.setting, auto.effective, auto.reason),
            (VideoCodecPreference::Automatic, VideoCodec::Vp8, None)
        );
        engine
            .set_video_codec_preference(VideoCodecPreference::PreferH264)
            .expect("set");
        let h264 = engine.video_codec_preference().expect("a readback");
        assert_eq!(
            (h264.setting, h264.effective, h264.reason),
            (
                VideoCodecPreference::PreferH264,
                VideoCodec::Vp8,
                Some(CodecPreferenceReason::NotDetected)
            )
        );
        assert_eq!(
            engine.inner.client.video_codec_preference(),
            VideoCodecPreference::PreferH264,
            "the client keeps it"
        );
    }

    /// The decoded fallback's decoders: none until a backend is given (nothing in
    /// `decode_codecs`, `decoded_unsupported`), then that backend's (kessel-video's fake: its
    /// pure-Rust VP8 and its own H.264).
    #[cfg(feature = "video-core")]
    #[test]
    fn the_decoded_fallback_decodes_with_the_given_backend() {
        use crate::video::{DecodedFormat, DecoderSpec, VideoCodec};
        let dir = std::env::temp_dir().join("sew-voice-real-decoders");
        let audio: AudioConfig = Arc::new(|_, _| EngineConfig::default());
        let spec = DecoderSpec {
            codec: VideoCodec::Vp8,
            format: DecodedFormat::I420,
            fit: (640, 360),
        };
        let wake: crate::video::DecoderWake = Arc::new(|| {});
        let bare = CallClientEngine::new(
            CallClient::with_mls(sew_call_client::ClientConfig::new("sew.test", dir.clone())),
            Arc::clone(&audio),
        );
        assert_eq!(bare.video_caps().decode_codecs, vec![]);
        let Err(e) = bare.video_decoder(spec, Arc::clone(&wake)) else {
            panic!("a decoder without a backend");
        };
        assert_eq!(
            e.to_info().code,
            crate::wire::video_code::DECODED_UNSUPPORTED
        );
        let fake = sew_call_client::kessel_video::fake::FakeBackend::new(
            sew_call_client::kessel_video::fake::FakeConfig::default(),
        );
        let engine = CallClientEngine::new(
            CallClient::with_mls(sew_call_client::ClientConfig::new("sew.test", dir)),
            audio,
        )
        .with_video_backend(fake);
        let caps = engine.video_caps();
        assert!(caps.receive);
        assert!(caps.decode_codecs.contains(&VideoCodec::H264), "{caps:?}");
        // The plugin's own tests turn on kessel-video's VP8 (the engine's `testing`).
        assert!(caps.decode_codecs.contains(&VideoCodec::Vp8), "{caps:?}");
        assert!(engine.video_decoder(spec, wake).is_ok());
        // One backend: the publisher's is the decoders' (and the native sinks').
        let one = engine.inner.video.backend().expect("the given backend");
        let bridge = engine.inner.video.bridge();
        assert!(
            std::ptr::addr_eq(Arc::as_ptr(&one), Arc::as_ptr(bridge.backend())),
            "the publish and the decoded fallback share the engine's backend"
        );
        assert!(
            Arc::ptr_eq(&bridge, &engine.inner.video.bridge()),
            "one bridge"
        );
        // The capture (cameras, screens, the preview) is the bridge's own.
        let capture = engine.video_capture().expect("a capture");
        assert!(
            std::ptr::addr_eq(Arc::as_ptr(capture.backend()), Arc::as_ptr(&one)),
            "the capture shares the engine's backend"
        );
        assert_eq!(
            caps.publish_codecs,
            vec![VideoCodec::Vp8, VideoCodec::H264],
            "publish_codecs: the backend's encoders"
        );
    }

    /// The device-key calls are the client's, over its data directory; a live call is
    /// `in_call`, any other store failure `device_store`.
    #[tokio::test(flavor = "multi_thread")]
    async fn device_keys_are_the_clients() {
        let dir = std::env::temp_dir().join(format!(
            "sew-voice-real-keys-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        let client =
            CallClient::with_mls(sew_call_client::ClientConfig::new("sew.test", dir.clone()));
        let audio: AudioConfig = Arc::new(|_, _| EngineConfig::default());
        let engine = CallClientEngine::new(client, audio);
        assert_eq!(engine.known_device_keys().await.unwrap(), vec![]);
        assert_eq!(engine.forget_server(77).await.unwrap(), vec![]);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            device_error(DeviceError::InCall).to_info().code,
            code::IN_CALL
        );
        let e = device_error(DeviceError::Unavailable(
            "errSecInteractionNotAllowed".into(),
        ))
        .to_info();
        assert_eq!(e.code, code::DEVICE_STORE);
        assert!(
            e.detail
                .as_str()
                .unwrap()
                .contains("errSecInteractionNotAllowed")
        );
    }

    /// A per-run unique suffix for a temp dir: std's randomly seeded hasher, no clock read.
    fn rand_suffix() -> u64 {
        use std::hash::BuildHasher as _;
        std::collections::hash_map::RandomState::new().hash_one(std::process::id())
    }

    /// Subscribing to the device events is a first use too, and so is `set_device`.
    #[test]
    fn a_subscription_or_set_device_is_a_first_use() {
        let (engine, n) = counted();
        let _rx = engine.device_events();
        wait_for("listed", || n.load(Ordering::SeqCst) >= 1);

        let (engine, n) = counted();
        // `in:1` is known at once (listed on this first use), not refused.
        engine
            .set_device(Some(Choice::Id("in:1".into())), None)
            .unwrap();
        assert!(n.load(Ordering::SeqCst) >= 1);
        assert_eq!(engine.devices().unwrap().input.as_deref(), Some("in:1"));
    }

    /// `set_device` with the OS default (no call): the choice is `None` again, and no
    /// device check applies to it.
    #[test]
    fn set_device_default_between_calls() {
        let (engine, _) = counted();
        engine
            .set_device(Some(Choice::Id("in:1".into())), None)
            .unwrap();
        engine
            .set_device(Some(Choice::Default), Some(Choice::Default))
            .unwrap();
        let list = engine.devices().unwrap();
        assert_eq!((list.input, list.output), (None, None));
        assert_eq!(audio_choice(&Choice::Default), DeviceChoice::Default);
        assert_eq!(
            audio_choice(&Choice::Id("x".into())),
            DeviceChoice::Id("x".into())
        );
        // An unknown id is still refused.
        let e = engine
            .set_device(Some(Choice::Id("in:nope".into())), None)
            .unwrap_err();
        assert_eq!(e.to_info().code, "device_not_found");
    }

    #[test]
    fn join_errors_map_to_plugin_errors() {
        assert_eq!(
            join_error(CallError::AlreadyInCall).to_info().code,
            "already_in_call"
        );
        assert_eq!(
            join_error(CallError::Provider(ProviderError::SigninRequired)).end_reason(),
            "signin_required"
        );
        assert_eq!(
            join_error(CallError::Setup("no roots".into())).to_info(),
            ErrorInfo::with_detail("engine_stopped", "no roots")
        );
    }

    /// The video backend (kessel-video's fake here): the camera permission and its prompt,
    /// the camera list and choice, the backend's background signal, and the Android context
    /// the scene and orientation reach.
    #[cfg(feature = "video-core")]
    #[tokio::test]
    async fn the_video_backend_answers_permission_cameras_and_background() {
        use crate::wire::{Facing, PermissionState};
        use sew_call_client::kessel_video::fake::{FakeBackend, FakeConfig, FakePermission};
        use sew_call_client::kessel_video::{AndroidContext, BackendEvent, Permission};

        let (bare, _) = counted();
        assert_eq!(bare.camera_permission(), PermissionState::Unsupported);
        assert!(bare.native_video_backend().is_none());
        assert!(bare.video_background_events().is_none());
        assert_eq!(
            bare.video_devices().unwrap_err().to_info().code,
            "video_unsupported"
        );

        let mut cfg = FakeConfig::default();
        cfg.camera_permission = FakePermission::Prompt {
            answer: Permission::Granted,
            after: Duration::from_millis(20),
        };
        cfg.backend_events = vec![
            (Duration::from_millis(300), BackendEvent::LowPower(true)),
            (Duration::from_millis(350), BackendEvent::Background(true)),
            (Duration::from_millis(450), BackendEvent::Background(false)),
        ];
        let ctx = AndroidContext::default();
        let (engine, _) = counted();
        let engine = engine
            .with_video_backend(FakeBackend::new(cfg))
            .with_android_video_context(ctx.clone());
        let mut rx = engine.video_background_events().unwrap();
        assert!(engine.native_video_backend().is_some());

        assert_eq!(engine.camera_permission(), PermissionState::Prompt);
        assert_eq!(
            engine.request_camera_permission().await,
            PermissionState::Granted,
            "the prompt's answer"
        );
        assert_eq!(engine.camera_permission(), PermissionState::Granted);

        let d = engine.video_devices().unwrap();
        assert_eq!(d.cameras.len(), 1);
        assert_eq!(
            (
                d.cameras[0].id.as_str(),
                d.cameras[0].facing,
                d.cameras[0].default
            ),
            ("fake:camera:0", Facing::Front, true)
        );
        assert_eq!(d.camera, None);
        assert_eq!(d.permission.camera, PermissionState::Granted);
        assert_eq!(
            engine
                .set_video_device(Choice::Id("nope".into()))
                .unwrap_err()
                .to_info()
                .code,
            "camera_not_found"
        );
        engine
            .set_video_device(Choice::Id("fake:camera:0".into()))
            .unwrap();
        assert_eq!(
            engine.video_devices().unwrap().camera.as_deref(),
            Some("fake:camera:0")
        );

        // Only the background events come through, in order.
        async fn next(rx: &mut broadcast::Receiver<bool>) -> bool {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap()
        }
        assert!(next(&mut rx).await);
        assert!(!next(&mut rx).await);

        // The scene and the orientation reach the Android context.
        engine.app_scene(crate::engine::AppScene::Background);
        assert!(ctx.is_background());
        engine.app_scene(crate::engine::AppScene::Foreground);
        assert!(!ctx.is_background());
        engine.device_orientation(100);
        assert_eq!(ctx.device_orientation(), 90);
    }
}
