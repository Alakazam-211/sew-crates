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
//! **Phones (0.3.0).** A CallKit or Telecom shim registers a [`VoiceListener`]
//! (`Builder::listener`), which sees the call whether or not a webview is awake, and acts
//! through [`VoiceControl`] (Tauri state): join, leave, mute, consent, the snapshot on wake,
//! `network_changed` and the audio-session hooks it forwards to kessel-audio. On iOS and
//! Android the plugin lists no devices between calls (the OS owns the route) and has no
//! keyring backend. It has no Swift or Kotlin of its own: on mobile it registers exactly as on
//! desktop (`.plugin(Builder::new(provider).build())` in the app's mobile entry point).

pub mod engine;
pub mod fake;
pub mod listener;
#[cfg(feature = "audio-core")]
pub mod real;
pub mod service;
pub mod wire;

#[cfg(feature = "tauri")]
mod commands;
#[cfg(feature = "tauri")]
mod plugin;

pub use engine::{
    AnswerError, AudioSessionEvent, BoxFuture, CONSENT_UNSUPPORTED, CallCredential, CallEvent,
    CallHandle, CallSnapshot, ConsentAnswer, ConsentRequired, EngineError, JoinAnswer,
    JoinProvider, JoinRequest, ProviderError, RejoinReason, VoiceEngine, app_server_answer,
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
/// The consent notice in the web kit's words (`ConsentPrompt::title_en`, `text_en`,
/// `choices_en`), from `sew-proto`.
pub use sew_proto::recording::{ConsentAsk, ConsentChoice};
pub use wire::{
    CallState, ConsentArgs, ConsentPrompt, DEFAULT_DEVICE, Device, DeviceChoice, DeviceList,
    ErrorInfo, JoinArgs, JoinResult, MuteArgs, Participant, PluginEvent, RecordingInfo,
    RecordingParticipant, SelfMute, SetDeviceArgs, Speaking, StateEvent, StateSnapshot,
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
