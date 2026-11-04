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

pub mod engine;
pub mod fake;
#[cfg(feature = "audio-core")]
pub mod real;
pub mod service;
pub mod wire;

#[cfg(feature = "tauri")]
mod commands;
#[cfg(feature = "tauri")]
mod plugin;

pub use engine::{
    AnswerError, BoxFuture, CONSENT_UNSUPPORTED, CallCredential, CallEvent, CallHandle,
    CallSnapshot, ConsentAnswer, ConsentRequired, EngineError, JoinAnswer, JoinProvider,
    JoinRequest, ProviderError, RejoinReason, VoiceEngine, app_server_answer,
};
#[cfg(feature = "tauri")]
pub use plugin::{Builder, TauriSink};
pub use service::{EventSink, SPEAKING_MIN_INTERVAL, Spawn, VoiceService};
/// The engine crate, for hosts that build [`real::CallClientEngine`] themselves.
pub use sew_call_client;
/// The host's own device key store (`Builder::device_key_store`), re-exported, with the
/// record's public half (sew-e2ee's `StoredDevice`), so a host needs no sew-e2ee dependency.
pub use sew_call_client::device::{
    DeviceError, DeviceKeyId, DeviceKeyRecord, DeviceKeyStore, MemoryDeviceKeyStore, StoredDevice,
};
pub use wire::{
    CallState, ConsentArgs, ConsentPrompt, DEFAULT_DEVICE, Device, DeviceChoice, DeviceList,
    ErrorInfo, JoinArgs, JoinResult, MuteArgs, Participant, PluginEvent, RecordingInfo,
    RecordingParticipant, SetDeviceArgs, Speaking, StateEvent, StateSnapshot,
};

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
