//! The engine interface: what the plugin needs from a voice engine.
//!
//! The host's side is `sew-call-client`'s own: [`JoinProvider`], [`JoinRequest`],
//! [`JoinAnswer`], [`ProviderError`] and [`RejoinReason`] are re-exported from it unchanged, so a
//! host writes one provider for the engine and the plugin alike.
//!
//! The plugin's side is [`VoiceEngine`] and [`CallHandle`]: `crate::real::CallClientEngine`
//! implements them over `sew_call_client::CallClient` and `Call` (the default engine), and
//! [`crate::fake::FakeEngine`] in memory (tests, the example's offline mode).
//!
//! The JSON shapes the webview sees (`wire.rs`) are the same for both.

use std::sync::Arc;

use sew_call_client::device::DeviceKeyId;
use tokio::sync::broadcast;

use crate::details::{QualityInfo, RosterEntry, SecurityInfo};
use crate::video::{
    CameraRequest, DecoderSpec, DecoderWake, FrameDecoder, PreviewRequest, ScreenRequest,
    TrackSettings, VideoFrames,
};
use crate::wire::{
    CallState, DeviceChoice, DeviceList, ErrorInfo, LocalVideo, Participant, PermissionState,
    RecordingInfo, ScreenSource, SinkStats, Speaking, VideoCaps, VideoDevices, VideoTrack,
    video_code,
};

/// The answer of a video method an engine doesn't implement.
fn no_video<T>() -> Result<T, EngineError> {
    Err(EngineError::failed(video_code::VIDEO_UNSUPPORTED))
}

/// The host's sign-in callback and its types, from `sew-call-client`.
///
/// One method, called to join and to re-join when Sew has dropped the join (`req.rejoin` says
/// why: [`RejoinReason::AdmitLost`] or [`RejoinReason::CredentialExpired`]). The host calls
/// Sew's voice or DM-call join with `call_credential: true` (and no `e2ee`) through its own
/// session (K2: the daemon BFF `/cli/voice/join`; an App: its server) and returns the answer
/// unchanged ([`JoinAnswer::from_json`]). The web UI never sees what it returns.
///
/// Recording (Sew 0.7.3): the provider forwards `req.recording_notice` as the voice join's
/// `recording_notice`, returns Sew's `409 recording_consent_required` as
/// [`ProviderError::ConsentRequired`] ([`ProviderError::from_sew_error`] reads Sew's error body,
/// or an App server route's that passes it on unchanged), and sends the person's consent
/// answers ([`JoinProvider::consent`], [`ConsentAnswer`]) with the person's own session.
pub use sew_call_client::{
    AnswerError, BoxFuture, CONSENT_UNSUPPORTED, CallCredential, ConsentAnswer, ConsentRequired,
    JoinAnswer, JoinProvider, JoinRequest, ProviderError, RejoinReason,
};

/// Reads the App server route's `200` body, `{"join": <Sew's VoiceJoinResponse, unchanged>,
/// "api_base": "https://…"}`, as a [`JoinAnswer`]; `join` keeps
/// its `call_credential`.
///
/// The error never quotes the body (it carries the media token and the call credential).
pub fn app_server_answer(body: &[u8]) -> Result<JoinAnswer, ProviderError> {
    let bad = |what: &str| ProviderError::Other(format!("the join answer did not parse ({what})"));
    let mut v: serde_json::Value = serde_json::from_slice(body).map_err(|_| bad("not JSON"))?;
    let api_base = v
        .get("api_base")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| bad("no api_base"))
        .and_then(|s| url::Url::parse(s).map_err(|_| bad("api_base")))?;
    let join = match v.get_mut("join").map(serde_json::Value::take) {
        Some(j) if j.is_object() => serde_json::to_vec(&j).map_err(|_| bad("join"))?,
        _ => return Err(bad("no join")),
    };
    JoinAnswer::from_json(&join, api_base).map_err(|_| bad("join"))
}

/// How an engine call fails.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum EngineError {
    /// The provider failed (join or re-join).
    #[error("provider: {0}")]
    Provider(ProviderError),
    /// The engine failed: `mic_denied`, `device_lost`, `turn_unreachable`, `e2ee_refused`, ...
    #[error("{}", .0.code)]
    Failed(ErrorInfo),
}

impl EngineError {
    /// An engine failure with a code and no detail.
    pub fn failed(code: impl Into<String>) -> EngineError {
        EngineError::Failed(ErrorInfo::new(code))
    }

    /// The `{code, detail}` the webview gets.
    pub fn to_info(&self) -> ErrorInfo {
        use crate::wire::code;
        match self {
            EngineError::Provider(ProviderError::SigninRequired) => {
                ErrorInfo::new(code::SIGNIN_REQUIRED)
            }
            EngineError::Provider(ProviderError::Refused { code, detail }) => ErrorInfo {
                code: code.clone(),
                detail: detail.clone(),
            },
            EngineError::Provider(ProviderError::Unavailable { retry_after_ms }) => {
                ErrorInfo::with_detail(
                    code::UNAVAILABLE,
                    serde_json::json!({ "retry_after_ms": retry_after_ms }),
                )
            }
            EngineError::Provider(ProviderError::Other(s)) => {
                ErrorInfo::with_detail(code::PROVIDER_ERROR, s.clone())
            }
            // The notice changed: Sew's refusal details, for the app to show the new one.
            EngineError::Provider(ProviderError::ConsentRequired(c)) => ErrorInfo::with_detail(
                code::RECORDING_CONSENT_REQUIRED,
                serde_json::to_value(c).unwrap_or_default(),
            ),
            // A provider error sew-call-client adds later.
            EngineError::Provider(p) => ErrorInfo::with_detail(code::PROVIDER_ERROR, p.to_string()),
            EngineError::Failed(info) => info.clone(),
        }
    }

    /// The `ended` reason when a join fails this way.
    pub fn end_reason(&self) -> String {
        use crate::wire::reason;
        match self {
            EngineError::Provider(ProviderError::SigninRequired) => reason::SIGNIN_REQUIRED.into(),
            EngineError::Provider(ProviderError::Refused { code, .. }) => code.clone(),
            EngineError::Provider(ProviderError::Unavailable { .. }) => reason::UNAVAILABLE.into(),
            EngineError::Provider(ProviderError::Other(_)) => reason::PROVIDER_ERROR.into(),
            EngineError::Provider(ProviderError::ConsentRequired(_)) => {
                crate::wire::code::RECORDING_CONSENT_REQUIRED.into()
            }
            // A provider error sew-call-client adds later.
            EngineError::Provider(_) => reason::PROVIDER_ERROR.into(),
            EngineError::Failed(info) => info.code.clone(),
        }
    }
}

/// What a live call reports (the engine's `CallEvent` in the plugin's shapes; devices are
/// engine-wide here, see [`VoiceEngine::device_events`]).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
#[allow(clippy::large_enum_variant)]
pub enum CallEvent {
    /// A state change (`Idle` is never sent by an engine).
    State(CallState),
    /// The roster with mute and deaf flags.
    Participants(Vec<Participant>),
    /// Levels, this device included (the engine may send them faster than 10 Hz; the plugin
    /// throttles).
    Speaking(Vec<Speaking>),
    /// A non-fatal error or warning (`device_lost`, `key_storage_file`, ...).
    Error(ErrorInfo),
    /// A moderator moved the call (Sew's `voice_server_updated`): `room` is the destination as
    /// a host joins it (the DM call's id when it is a call, else the voice channel's id).
    Moved {
        /// The destination room.
        room: String,
    },
    /// The call's recording changed (Sew 0.7.3), or `None` once none is active.
    Recording(Option<RecordingInfo>),
    /// The call's key state or security code changed (0.4.1; [`CallHandle::security`]).
    Security(Option<SecurityInfo>),
    /// Every media participant with its engine identity and label (0.4.1;
    /// [`CallHandle::roster`]).
    Roster(Vec<RosterEntry>),
    /// A stats window's connection quality (0.4.1; [`CallHandle::quality`]). The plugin shows
    /// `lost` while the call is reconnecting, whatever the engine last said.
    Quality(QualityInfo),
    /// The call's video tracks changed (added, removed, muted, resized, paused): the whole
    /// list.
    Video(Vec<VideoTrack>),
    /// This device's camera or screen changed state.
    LocalVideo(LocalVideo),
    /// The room's media policy changed (Sew's `media_policy_updated`; the engine also sends
    /// the join answer's at the start).
    Policy(serde_json::Value),
}

/// A call's video now, for a late subscriber ([`CallHandle::video_snapshot`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VideoSnapshot {
    /// The video tracks.
    pub tracks: Vec<VideoTrack>,
    /// This device's camera and screen.
    pub local: LocalVideo,
    /// The room's media policy (`None`: the engine has none).
    pub policy: Option<serde_json::Value>,
}

/// A call's current state, for a late subscriber (the engine's `CallSnapshot`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSnapshot {
    /// The state now.
    pub state: CallState,
    /// The roster now.
    pub participants: Vec<Participant>,
    /// The active recording now (Sew 0.7.3).
    pub recording: Option<RecordingInfo>,
}

/// One live call (the engine's `Call`).
pub trait CallHandle: Send + Sync + 'static {
    /// The call's id (Sew's call room).
    fn call_id(&self) -> String;
    /// Events from now on. The plugin subscribes once, right after `join`, then reads
    /// [`CallHandle::snapshot`] so nothing sent in between is lost.
    fn events(&self) -> broadcast::Receiver<CallEvent>;
    /// The state and roster now.
    fn snapshot(&self) -> CallSnapshot;
    /// Self-mute and self-deafen (sending, `MuteTrackRequest` and Sew's self state).
    fn mute(
        &self,
        muted: Option<bool>,
        deafened: Option<bool>,
    ) -> BoxFuture<'_, Result<(), EngineError>>;
    /// Leave: Sew's leave with the credential, then the engine's.
    fn leave(&self) -> BoxFuture<'_, Result<(), EngineError>>;
    /// The room a moderator move took the call to (the last [`CallEvent::Moved`]); `None` when
    /// there was no move (or the engine does not say).
    fn moved_room(&self) -> Option<String> {
        None
    }
    /// The person's answer to the recording notice (Sew 0.7.3): held at `consent_required`,
    /// the engine joins with it; in a call, the host's provider sends it. The default answers
    /// `consent_unsupported`.
    fn consent(
        &self,
        granted: bool,
        notice_version: u32,
    ) -> BoxFuture<'_, Result<(), EngineError>> {
        let _ = (granted, notice_version);
        Box::pin(std::future::ready(Err(EngineError::failed(
            crate::wire::code::CONSENT_UNSUPPORTED,
        ))))
    }
    /// "Stop recording me": withdraws a granted consent through the host's provider. The
    /// default answers `consent_unsupported`.
    fn withdraw_consent(&self) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(std::future::ready(Err(EngineError::failed(
            crate::wire::code::CONSENT_UNSUPPORTED,
        ))))
    }
    /// The device's network changed (a phone's `NWPathMonitor` or `ConnectivityManager`): run
    /// the resume path now (session resume, then an ICE restart) on a fresh TURN-TLS socket,
    /// instead of waiting for the old one to die (`sew-call-client`'s `Call::network_changed`).
    /// It must not block: an engine starts the work and returns. The default does nothing
    /// (the engine notices the loss itself, later).
    fn network_changed(&self) {}
    /// The call's key state and security code now (0.4.1); then [`CallEvent::Security`]. The
    /// default has none.
    fn security(&self) -> Option<SecurityInfo> {
        None
    }
    /// Every media participant now (0.4.1); then [`CallEvent::Roster`]. The default has none.
    fn roster(&self) -> Vec<RosterEntry> {
        Vec::new()
    }
    /// The last stats window's quality (0.4.1); then [`CallEvent::Quality`]. The default is
    /// `unknown`.
    fn quality(&self) -> QualityInfo {
        QualityInfo::unknown()
    }

    // ── video (the defaults: an engine without video) ──

    /// The call's frame channel: every subscribed track's encoded plaintext frames (decrypted
    /// and checked; nothing that failed decryption) and this device's self-view frames, with
    /// the tracks' `Added`, `Removed`, `Broken` and `KeyframeNeeded` items. The plugin takes it
    /// once, right after `join`; a second call answers `None`. The default: `None` (no video).
    fn video_frames(&self) -> Option<VideoFrames> {
        None
    }
    /// The call's video tracks, this device's camera and screen and the media policy now
    /// (read after `join` and after a missed event). The default: none.
    fn video_snapshot(&self) -> VideoSnapshot {
        VideoSnapshot::default()
    }
    /// What the sinks of remote track `track` want now (the union of its sinks, sent on every
    /// change). The engine keeps every track disabled until told otherwise. It must not
    /// block. The default does nothing.
    fn set_video_settings(&self, track: &str, settings: TrackSettings) {
        let _ = (track, settings);
    }
    /// A sink of `track` needs a keyframe (it opened, became visible, or its decoder failed or
    /// fell behind): the engine asks the sender (rate-limited) or, for `local:*`, its own
    /// encoder. It must not block. The default does nothing.
    fn request_keyframe(&self, track: &str) {
        let _ = track;
    }
    /// The webview's decoder counters of `track`'s sinks (every `video_sink_update` with
    /// stats; summed over the track's sinks), for the stats beacon. The default drops them.
    fn video_sink_stats(&self, track: &str, stats: SinkStats) {
        let _ = (track, stats);
    }
    /// Camera on or off: the policy, the OS permission, the lease, the device, the encoders,
    /// then publishing (`local_video` follows each step). Answers once publishing (or off).
    /// The default: `video_unsupported`.
    fn camera(&self, req: CameraRequest) -> BoxFuture<'_, Result<(), EngineError>> {
        let _ = req;
        Box::pin(std::future::ready(no_video()))
    }
    /// Camera off with `reason` as `local_video`'s reason (who turned it off: `system_ui` for
    /// CallKit's or Telecom's video button), so a call log tells the offs apart. The default:
    /// [`CallHandle::camera`] off.
    fn camera_off_because(&self, reason: &'static str) -> BoxFuture<'_, Result<(), EngineError>> {
        let _ = reason;
        self.camera(CameraRequest {
            on: false,
            device: None,
        })
    }
    /// Screen share on or off, as [`CallHandle::camera`] (the picker when `source` is
    /// `None`). The default: `video_unsupported`.
    fn screen(&self, req: ScreenRequest) -> BoxFuture<'_, Result<(), EngineError>> {
        let _ = req;
        Box::pin(std::future::ready(no_video()))
    }
    /// The iPhone screen share (`VoiceControl::screen_share_ring`): arm the broadcast
    /// extension's ring in `container` (the App Group container) for the rest of the call; its
    /// frames are published as the call's screen while the extension sends them. Arming again
    /// replaces the ring. Answers once the ring's file is made. The default:
    /// `screen_unsupported`.
    fn screen_share_ring(
        &self,
        container: std::path::PathBuf,
    ) -> BoxFuture<'_, Result<(), EngineError>> {
        let _ = container;
        Box::pin(std::future::ready(Err(EngineError::failed(
            crate::wire::video_code::SCREEN_UNSUPPORTED,
        ))))
    }
    /// The app went to the background or came back, as the plugin decides it: background when
    /// the app says so ([`VoiceEngine::app_scene`]) **or** the video backend does (kessel-video's
    /// `BackendEvent::Background`), foreground only when both say foreground. The plugin has
    /// already hidden (or re-shown) the sinks; this is the engine's hook for its own sending
    /// side (pausing and resuming the camera publish). It must not block. The default does
    /// nothing.
    fn app_scene(&self, scene: AppScene) {
        let _ = scene;
    }
}

/// Whether the app is on screen (phones): [`crate::VoiceControl::app_scene`].
///
/// It is not an audio interruption: `suspend` / `resume` ([`AudioSessionEvent`]) stay for
/// those (Siri, a cellular call) and never touch video, and an app can go to the background
/// with no audio interruption at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum AppScene {
    /// On screen.
    #[default]
    Foreground,
    /// In the background (iOS `sceneDidEnterBackground`, Android `onPause` / `onStop`).
    Background,
}

impl AppScene {
    /// Background or foreground from a flag.
    pub fn background(background: bool) -> AppScene {
        if background {
            AppScene::Background
        } else {
            AppScene::Foreground
        }
    }

    /// Is it the background?
    pub fn is_background(self) -> bool {
        self == AppScene::Background
    }
}

/// The phone's audio session, as CallKit (iOS) or Telecom (Android) reports it, for the engine
/// to pass to kessel-audio unchanged. The plugin never configures `AVAudioSession` or the
/// Android audio mode itself: CallKit and Telecom own them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AudioSessionEvent {
    /// The session is active (`CXProviderDelegate` `didActivate`; on Android, the Telecom
    /// connection is active): kessel-audio's `start_when_session_active` opens the microphone
    /// now.
    Activated,
    /// The session is no longer active (`didDeactivate`).
    Deactivated,
    /// An interruption began (kessel-audio's `suspend`).
    Suspend,
    /// The interruption ended (kessel-audio's `resume`).
    Resume,
    /// The OS audio route changed (speaker, earpiece, Bluetooth; kessel-audio's
    /// `route_changed`).
    RouteChanged,
}

/// The engine (`sew-call-client`'s `CallClient`): one per app.
pub trait VoiceEngine: Send + Sync + 'static {
    /// Join through the provider; the engine keeps the provider for re-joins.
    fn join(
        &self,
        provider: Arc<dyn JoinProvider>,
        req: JoinRequest,
    ) -> BoxFuture<'_, Result<Arc<dyn CallHandle>, EngineError>>;
    /// The audio devices now.
    fn devices(&self) -> Result<DeviceList, EngineError>;
    /// Switch the input and/or output (`None`: unchanged; [`DeviceChoice::Default`]: the OS
    /// default, followed when it changes); hot-swaps a live call's stream.
    fn set_device(
        &self,
        input: Option<DeviceChoice>,
        output: Option<DeviceChoice>,
    ) -> Result<(), EngineError>;
    /// Device list changes (hot-plug, a default change, a `set_device`).
    fn device_events(&self) -> broadcast::Receiver<DeviceList>;
    /// The phone's audio session changed ([`AudioSessionEvent`]): passed to kessel-audio
    /// unchanged, for the live call and (an engine that keeps it) the next one, since CallKit
    /// may activate the session before the join has a call. It must not block. The default
    /// does nothing (a desktop engine).
    fn audio_session(&self, event: AudioSessionEvent) {
        let _ = event;
    }
    /// A sign-out from one Sew install: delete every device of `install_id` the engine's device
    /// store holds (a hardware key through `HardwareDeviceKey::delete`) and its pins; answer the
    /// removed keys. The service refuses it while a call is live. The default holds no devices
    /// and answers none.
    fn forget_server(
        &self,
        install_id: u64,
    ) -> BoxFuture<'_, Result<Vec<DeviceKeyId>, EngineError>> {
        let _ = install_id;
        Box::pin(std::future::ready(Ok(Vec::new())))
    }
    /// Every device key the engine's device store holds, sorted. The default holds none.
    fn known_device_keys(&self) -> BoxFuture<'_, Result<Vec<DeviceKeyId>, EngineError>> {
        Box::pin(std::future::ready(Ok(Vec::new())))
    }

    // ── video (the defaults: an engine without video) ──

    /// What this device's video can do. The default: everything off (backend `null`).
    fn video_caps(&self) -> VideoCaps {
        VideoCaps::default()
    }
    /// The cameras, the chosen one and the OS permissions. The default: `video_unsupported`.
    fn video_devices(&self) -> Result<VideoDevices, EngineError> {
        no_video()
    }
    /// Choose the camera: switches a live camera without a republish, and is kept for the
    /// next `camera` or `preview`. The default: `video_unsupported`.
    fn set_video_device(&self, camera: DeviceChoice) -> Result<(), EngineError> {
        let _ = camera;
        no_video()
    }
    /// Camera list changes (hot-plug, a permission change, a `set_video_device`). The plugin
    /// subscribes on the first use of video. The default: `None` (no events).
    fn video_device_events(&self) -> Option<broadcast::Receiver<VideoDevices>> {
        None
    }
    /// The displays and windows to share, for an App's own picker (`[]` where only the OS
    /// picker exists). The default: `video_unsupported`.
    fn screen_sources(&self) -> BoxFuture<'_, Result<Vec<ScreenSource>, EngineError>> {
        Box::pin(std::future::ready(no_video()))
    }
    /// The camera outside a call (a pre-join check): on, it answers the preview's frame channel
    /// (`local:camera` frames); off, `None`. The camera opens **only** on this explicit call.
    /// The default: `video_unsupported`.
    fn preview(
        &self,
        req: PreviewRequest,
    ) -> BoxFuture<'_, Result<Option<VideoFrames>, EngineError>> {
        let _ = req;
        Box::pin(std::future::ready(no_video()))
    }
    /// A `local:camera` sink outside a call needs a keyframe: the preview's encoder makes one
    /// now. It must not block. The default does nothing.
    fn preview_keyframe(&self) {}

    // ── phones (the defaults: a desktop engine, or one without video) ──

    /// The camera permission now. Never prompts. The default: `unsupported`.
    fn camera_permission(&self) -> PermissionState {
        PermissionState::Unsupported
    }
    /// Asks for the camera: the OS prompt when the permission is `prompt` (iOS), else the state
    /// at once. On Android the app asks for CAMERA itself (an Activity callback, in Tauri its
    /// permission API); this then reports the state without a prompt. The default:
    /// `unsupported`.
    fn request_camera_permission(&self) -> BoxFuture<'_, PermissionState> {
        Box::pin(std::future::ready(PermissionState::Unsupported))
    }
    /// The app went to the background or came back, as the app reports it (passed on before
    /// the plugin hides or re-shows its sinks). The real engine tells kessel-video's Android
    /// backend (`AndroidContext::set_background`). It must not block. The default does
    /// nothing.
    fn app_scene(&self, scene: AppScene) {
        let _ = scene;
    }
    /// The device turned (degrees clockwise from its natural orientation, as Android's
    /// `OrientationEventListener` reports it): kessel-video's Android backend turns camera
    /// frames upright from the next one. It must not block. The default does nothing (iOS's
    /// backend follows the device itself).
    fn device_orientation(&self, degrees: u32) {
        let _ = degrees;
    }
    /// The video backend's own background signal (kessel-video's `BackendEvent::Background`:
    /// `true` in the background), the second signal next to [`VoiceEngine::app_scene`]. The
    /// plugin subscribes on the first use of a sink or a scene. The default: `None`.
    fn video_background_events(&self) -> Option<broadcast::Receiver<bool>> {
        None
    }
    /// The video backend, for the plugin's native sinks (they decode VP8 with its decoder and
    /// turn frames into OS buffers with its `convert`). The default: `None` (a native sink then
    /// delivers H.264 only).
    #[cfg(feature = "video-core")]
    fn native_video_backend(&self) -> Option<Arc<dyn sew_call_client::kessel_video::VideoBackend>> {
        None
    }
    /// A decoder for the decoded-frame fallback (a sink whose webview can't decode the track's
    /// codec): `spec.codec` in, `spec.format` out at most `spec.fit`. `wake` is called (from
    /// any thread) whenever an output or an error is queued. Only for a codec
    /// [`VoiceEngine::video_caps`] lists in `decode_codecs`. The default:
    /// `video_unsupported`.
    fn video_decoder(
        &self,
        spec: DecoderSpec,
        wake: DecoderWake,
    ) -> Result<Box<dyn FrameDecoder>, EngineError> {
        let _ = (spec, wake);
        no_video()
    }
}
