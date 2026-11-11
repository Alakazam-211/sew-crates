//! `tauri-plugin-sew-voice`: Sew native voice calls in a Tauri 2 app.
//!
//! The webview draws the UI and sends eight commands (`plugin:sew-voice|join`, `leave`, `mute`,
//! `devices`, `set_device`, `state`, and for recording consent `consent` and
//! `withdraw_consent`); the plugin answers and emits six events (`sew-voice://state`,
//! `participants`, `speaking`, `errors`, `devices`, `recording`). Sign-in stays in the host's
//! Rust: the host registers a [`JoinProvider`] that obtains Sew's join answer (and sends the
//! person's recording consent with the person's own session), so no token ever reaches the
//! webview.
//!
//! ```ignore
//! tauri::Builder::default()
//!     .plugin(tauri_plugin_sew_voice::Builder::new(MyProvider::new()).build())
//!     .run(tauri::generate_context!())?;
//! ```
//!
//! The engine is `sew-call-client` ([`real::CallClientEngine`], features `audio-core`/`audio`);
//! [`fake::FakeEngine`] stands in for it in tests. Without the `tauri` feature the crate is the
//! plugin's core ([`VoiceService`], the JSON shapes in [`wire`], the engine interface in
//! [`engine`], the real and the fake engine), which builds and tests without Tauri's system
//! libraries. The crate's README is the guide.
//!
//! **Call details (0.4.1).** Beside the frozen eight, the `call_details`, `ring` and
//! `ring_ended` commands (permission set `sew-voice:call-details`) and the `security`, `roster`,
//! `quality` and `incoming` events give a call UI the key state and security code, each media
//! participant's engine identity and label, the connection quality and an incoming DM call
//! ([`details`]).
//!
//! **Record and Stop (Sew 0.9.0).** The `start_recording` and `stop_recording` commands
//! ([`RECORDING_COMMANDS`], permission set `sew-voice:recording`) send the person's Record and
//! Stop through the host's provider ([`JoinProvider::recording`]) with the person's own
//! session, as consent goes.
//!
//! **Which buttons (0.6.1).** The host tells the plugin the person's rights in the call's room
//! with `set_recording_rights` ([`RECORDING_RIGHTS_COMMANDS`], in `sew-voice:recording`;
//! [`RecordingRights`]); the plugin then says whether Sew would take a Record or a Stop now,
//! in the `recording_controls` event ([`RecordingControls`]) and the `recording` event's
//! `can_record` and `can_stop`, and updates them as the call changes.
//!
//! **Phones (0.3.0).** A CallKit or Telecom shim registers a [`VoiceListener`]
//! (`Builder::listener`), which sees the call whether or not a webview is awake, and acts
//! through [`VoiceControl`] (Tauri state): join, leave, mute, consent, the snapshot on wake,
//! `network_changed` and the audio-session hooks it forwards to kessel-audio. On iOS and
//! Android the plugin lists no devices between calls (the OS owns the route) and has no
//! keyring backend. It has no Swift or Kotlin of its own: on mobile it registers exactly as on
//! desktop (`.plugin(Builder::new(provider).build())` in the app's mobile entry point).
//!
//! **Video.** More commands ([`VIDEO_COMMANDS`] and the codec setting's
//! [`VIDEO_CODEC_COMMANDS`], the `sew-voice:video` permission set) and four events (`sew-voice://video`, `local_video`, `video_devices`,
//! `policy`). Remote and self-view frames reach the webview over a Tauri raw `Channel` per
//! element, a 32-byte header ([`video::FrameHeader`]) then the encoded frame, decoded by the
//! webview's WebCodecs; keys never leave Rust. The engine side is in [`video`]: every video
//! method of [`VoiceEngine`] and [`CallHandle`] defaults to "no video", so an engine without it
//! is unchanged.

pub mod details;
pub mod engine;
pub mod fake;
#[cfg(feature = "video-core")]
pub mod kessel;
pub mod listener;
#[cfg(feature = "video-core")]
pub mod native;
#[cfg(feature = "audio-core")]
pub mod real;
mod rights;
/// The iPhone screen share's ring (feature `ios-screen-ring` with `video-core`; iOS, and Linux
/// for tests off the device).
#[cfg(all(
    feature = "ios-screen-ring",
    feature = "video-core",
    any(target_os = "ios", target_os = "linux")
))]
pub mod screen_ring;
pub mod service;
mod sink;
pub mod video;
pub mod wire;

#[cfg(feature = "tauri")]
mod commands;
#[cfg(feature = "tauri")]
mod plugin;

pub use details::{
    CallDetails, DETAIL_COMMANDS, DETAIL_EVENTS, IncomingCall, QualityInfo, RingEndedArgs,
    RosterEntry, SecurityInfo,
};
pub use engine::{
    AnswerError, AppScene, AudioSessionEvent, BoxFuture, CONSENT_UNSUPPORTED, CallCredential,
    CallEvent, CallHandle, CallSnapshot, ConsentAnswer, ConsentRequired, EngineError, JoinAnswer,
    JoinProvider, JoinRequest, ProviderError, RECORDING_UNSUPPORTED, RecordingControl,
    RecordingRequest, RejoinReason, VideoSnapshot, VoiceEngine, app_server_answer,
};
pub use listener::{VoiceControl, VoiceListener};
#[cfg(feature = "tauri")]
pub use plugin::{Builder, TauriSink};
pub use service::{EventSink, SPEAKING_MIN_INTERVAL, Spawn, VoiceService};
/// The engine crate, for hosts that build [`real::CallClientEngine`] themselves.
pub use sew_call_client;
/// The simulator's and x86 emulator's software stand-in (feature `sim-software-key`).
#[cfg(feature = "sim-software-key")]
pub use sew_call_client::device::SimSoftwareKey;
/// The host's own device key store (`Builder::device_key_store`), re-exported, with the
/// record's public half (sew-e2ee's `StoredDevice`), so a host needs no sew-e2ee dependency.
pub use sew_call_client::device::{
    DeviceError, DeviceKeyId, DeviceKeyRecord, DeviceKeyStore, MemoryDeviceKeyStore, StoredDevice,
};
/// The hardware device key (`Builder::hardware_device_key`: Secure Enclave, StrongBox/TEE) and
/// the enrolment platform (`Builder::device_platform`), re-exported.
pub use sew_call_client::device::{DevicePlatform, HardwareDeviceKey};
/// The consent notice in the Sew App kit's words (`ConsentPrompt::title_en`, `text_en`,
/// `choices_en`), from `sew-proto`.
pub use sew_proto::recording::{ConsentAsk, ConsentChoice};
pub use sink::FrameChannel;
pub use video::{
    CameraRequest, DecodeFailure, DecodedFormat, DecodedFrame, DecoderSpec, DecoderWake,
    EncodedVideoFrame, FrameDecoder, FrameHeader, ParentWindow, PreviewRequest, ScreenRequest,
    TrackSettings, VideoFrames, VideoRecvItem,
};
pub use wire::{
    CallState, ConsentArgs, ConsentPrompt, DEFAULT_DEVICE, Device, DeviceChoice, DeviceKeyInfo,
    DeviceList, ErrorInfo, ForgetServerArgs, JoinArgs, JoinOptions, JoinResult, MuteArgs,
    Participant, PluginEvent, RecordingControls, RecordingInfo, RecordingParticipant,
    RecordingRights, SelfMute, SetDeviceArgs, Speaking, StateEvent, StateSnapshot,
};
pub use wire::{
    Camera, CameraArgs, CodecPreferenceInfo, CodecPreferenceReason, Facing, LocalVideo,
    LocalVideoState, PermissionState, PreviewAnswer, PreviewArgs, ScreenArgs, ScreenCaps,
    ScreenKind, ScreenSource, SetVideoCodecPreferenceArgs, SetVideoDeviceArgs, SinkIdArgs,
    SinkMode, SinkStats, Tile, VideoCaps, VideoCodec, VideoCodecPreference, VideoDevices,
    VideoPaused, VideoPermissions, VideoSinkAnswer, VideoSinkArgs, VideoSinkUpdateArgs,
    VideoSource, VideoTrack,
};

// On phones the device key is the host's (Builder::device_key_store, or a hardware key):
// keyring's backends are desktop-only.
#[cfg(all(
    feature = "keyring-store",
    any(target_os = "ios", target_os = "android")
))]
compile_error!(
    "tauri-plugin-sew-voice: `keyring-store` is desktop-only; on iOS and Android pass the \
     host's store with Builder::device_key_store"
);

/// The plugin's name: commands are `plugin:sew-voice|<command>`, permissions `sew-voice:*`.
pub const PLUGIN_NAME: &str = "sew-voice";

/// The eight commands, as the webview invokes them after `plugin:sew-voice|`.
pub const COMMANDS: [&str; 8] = [
    "join",
    "leave",
    "mute",
    "devices",
    "set_device",
    "state",
    "consent",
    "withdraw_consent",
];

/// The device-key commands (0.4.1), outside `sew-voice:default`: a window gets them only with
/// an explicit permission (`sew-voice:allow-forget-server`, `sew-voice:allow-known-device-keys`,
/// or the set `sew-voice:device-keys`). `forget_server {install_id}` deletes the devices of one
/// Sew install (a sign-out) and answers the removed keys; `known_device_keys` lists the keys
/// the plugin's device store holds.
pub const DEVICE_KEY_COMMANDS: [&str; 2] = ["forget_server", "known_device_keys"];

/// The recording commands (Sew 0.9.0), outside `sew-voice:default`: a window gets them with
/// the set `sew-voice:recording` or each one's own `sew-voice:allow-<command>`.
/// `start_recording` (Record) and `stop_recording` (Stop) are the person's own acts, sent by
/// the host's provider with the person's session ([`JoinProvider::recording`]); Sew decides
/// who may.
pub const RECORDING_COMMANDS: [&str; 2] = ["start_recording", "stop_recording"];

/// The recording rights' command (plugin 0.6.1), in the `sew-voice:recording` set after
/// [`RECORDING_COMMANDS`] (its own permission `sew-voice:allow-set-recording-rights`):
/// `set_recording_rights {room, my_perms, dm?, recording_enabled?, recording_dm_enabled?,
/// record_capped?, agent?}` ([`RecordingRights`]) gives the plugin what it needs for
/// [`RecordingControls`]. It answers `null`, or `invalid_args` when `my_perms` is not a
/// decimal number.
pub const RECORDING_RIGHTS_COMMANDS: [&str; 1] = ["set_recording_rights"];

/// The video commands, outside `sew-voice:default`: a window gets them with the set
/// `sew-voice:video` (all thirteen, and [`VIDEO_CODEC_COMMANDS`]) or each one's own
/// `sew-voice:allow-<command>`. With an engine
/// without video, `video_caps` answers "nothing" and the others reject `video_unsupported`.
pub const VIDEO_COMMANDS: [&str; 13] = [
    "video_caps",
    "video_devices",
    "set_video_device",
    "camera",
    "screen_sources",
    "screen",
    "preview",
    "video_sink",
    "video_sink_update",
    "video_sink_close",
    "request_keyframe",
    "camera_permission",
    "request_camera_permission",
];

/// The video codec setting's command, in the `sew-voice:video` set after [`VIDEO_COMMANDS`]
/// (its own permission `sew-voice:allow-set-video-codec-preference`):
/// `set_video_codec_preference {preference}` (`"automatic"`, `"prefer_h264"`, `"prefer_vp8"`)
/// sets this device's video codec preference and answers `video_caps.codec_preference`
/// (`{setting, effective, reason}`). The engine keeps it until the process ends; persisting it
/// is the host's job.
pub const VIDEO_CODEC_COMMANDS: [&str; 1] = ["set_video_codec_preference"];
