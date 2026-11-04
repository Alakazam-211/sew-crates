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

use tokio::sync::broadcast;

use crate::wire::{CallState, DeviceChoice, DeviceList, ErrorInfo, Participant, Speaking};

/// The host's sign-in callback and its types, from `sew-call-client`.
///
/// One method, called to join and to re-join when Sew has dropped the join (`req.rejoin` says
/// why: [`RejoinReason::AdmitLost`] or [`RejoinReason::CredentialExpired`]). The host calls
/// Sew's voice or DM-call join with `call_credential: true` (and no `e2ee`) through its own
/// session (K2: the daemon BFF `/cli/voice/join`; an App: its server) and returns the answer
/// unchanged ([`JoinAnswer::from_json`]). The web UI never sees what it returns.
pub use sew_call_client::{
    AnswerError, BoxFuture, CallCredential, JoinAnswer, JoinProvider, JoinRequest, ProviderError,
    RejoinReason,
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
}

/// A call's current state, for a late subscriber (the engine's `CallSnapshot`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSnapshot {
    /// The state now.
    pub state: CallState,
    /// The roster now.
    pub participants: Vec<Participant>,
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
}
