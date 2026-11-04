//! `sew-call-client`: Sew's native voice engine. Everything below the UI for one call:
//!
//! - LiveKit-protocol signalling against Sew's media server (the `/rtc/v1` single-PC session from
//!   `sew-lk-compat`) with resume, full reconnect inside a 20 s window and re-join through the
//!   host's [`JoinProvider`];
//! - str0m transport with TURN over UDP, TCP and TLS, relay-only when the join answer or the
//!   engine asks (`media.relay_only` / `force_relay`), production TLS trust (the platform's);
//! - the frame path: kessel-audio's encoded Opus frames are encrypted through the call's keys
//!   ([`e2ee::KeySession`]) and sent; remote frames are decrypted and verified before kessel-audio
//!   ever sees them (no plaintext mode exists);
//! - mute and deafen, participants, speaking levels, and the call credential's renewal;
//! - the call-stats beacon (Sew's `POST /api/v1/me/voice/stats`, when the join answer carries
//!   its terms) and the same per-window numbers for the host ([`stats`], [`Call::stats`]);
//! - recording consent (Sew 0.7.3): a join into a recorded room is held at
//!   [`CallState::ConsentRequired`] until the host answers the notice ([`Call::consent`]); the
//!   call's recording ([`Call::recording`], [`CallEvent::Recording`]) feeds the keys, which
//!   release this device's key to the recorder only once Sew says `granted`;
//!   [`Call::withdraw_consent`] is "Stop recording me". Consent travels through the host's
//!   [`JoinProvider`] with the person's own session.
//!
//! ```text
//! let client = CallClient::new(ClientConfig::new("com.example.app", data_dir), keys);
//! let call = client.join(provider, JoinRequest { target, room, muted: false, deafened: false, rejoin: None, recording_notice: None }).await?;
//! let audio = AudioBridge::start(&call, kessel_audio::EngineConfig::default())?; // feature `audio`
//! let mut events = call.events();
//! ```
//!
//! The E2EE layer plugs in through [`e2ee::E2eeProvider`]; `CallClient` cannot be built without
//! one. The real one, and the default ([`CallClient::with_mls`]), is [`e2ee::MlsKeys`]: MLS
//! through `sew-e2ee`, the call socket ([`gateway`]), the device store ([`device`]), the
//! mandatory checks and sew-lk-compat's LkGcm. The only other provider is the test-only
//! `StaticKeys` (feature `testing`).

mod api;
#[cfg(feature = "audio-core")]
mod audio;
mod call;
mod config;
pub mod device;
pub mod e2ee;
mod engine;
pub mod gateway;
mod pc;
mod provider;
mod signal;
pub mod stats;
mod transport;

#[cfg(feature = "audio-core")]
pub use audio::{AudioBridge, AudioBridgeError, BridgeLevels};
pub use call::{
    Call, CallClient, CallCounters, CallError, CallErrorEvent, CallEvent, CallMoved, CallRecording,
    CallSnapshot, CallState, EndReason, FrameSender, FrameStream, OpusFrame, Participant,
    RecordingConsent, RecordingMember, RecvItem, RecvOpus, Speaking,
};
pub use config::ClientConfig;
#[cfg(feature = "testing")]
pub use config::TestingOverrides;
/// kessel-audio, re-exported for hosts that configure the audio engine.
#[cfg(feature = "audio-core")]
pub use kessel_audio;
pub use provider::{
    AnswerError, BoxFuture, CONSENT_UNSUPPORTED, CallCredential, ConsentAnswer, ConsentRequired,
    JoinAnswer, JoinProvider, JoinRequest, ProviderError, RejoinReason,
};
/// Sew's recording types the engine's recording API carries (from `sew-proto`).
pub use sew_proto::recording::{ConsentPrompt, Recording, RecordingNotice, RecordingRef};
pub use stats::{
    BeaconOutcome, CallStatsReport, PlayoutSource, PlayoutStats, PlayoutTotals, StatsWindow,
};

/// The engine's name and version as Sew sees it (`ClientInfo`, the caps' `sdk_version`).
pub const ENGINE_VERSION: &str = concat!("sew-call-client/", env!("CARGO_PKG_VERSION"));

/// The user agent of the key routes and the call socket (no personal data).
pub const USER_AGENT: &str = concat!(
    "sew-call-client/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/Alakazam-211)"
);

/// Wall-clock unix milliseconds, or a test clock.
pub type Clock = std::sync::Arc<dyn Fn() -> u64 + Send + Sync>;

/// The system clock.
pub fn system_clock() -> Clock {
    use sew_proto::clock::Clock as _;
    std::sync::Arc::new(|| sew_proto::clock::SystemClock.now_unix_ms())
}
