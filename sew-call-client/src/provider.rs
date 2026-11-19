//! The host's join callback: the engine asks the host for Sew's join answer
//! to join and to re-join when Sew dropped the join, and (Sew 0.7.3) to send the person's
//! answer to a recording notice. The web UI never sees a token.
//!
//! Recording consent goes through the host, never the call credential: the host sends it with
//! the person's own Sew session (its server route for an App), so it is the person's act with
//! Sew's `notice_version` check and audit. The engine only asks ([`JoinProvider::consent`],
//! [`JoinRequest::recording_notice`]) and reads Sew's answer; joining never implies consent and
//! no answer means not recorded.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use sew_proto::id::Id;
use sew_proto::recording::{ConsentPrompt, ConsentRequest, Recording, RecordingNotice};
use sew_proto::voice::VoiceJoinResponse;

/// A boxed future, as the trait methods return.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Why the engine asks for a join again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RejoinReason {
    /// The reconnect window (20 s) ran out, or the engine said the admit is gone.
    AdmitLost,
    /// The call credential expired (a long sleep) and can't be renewed.
    CredentialExpired,
}

/// What the host joins. Its JSON (`{target, room, muted, deafened, rejoin}`, plus
/// `recording_notice` when there is one) is what a host's provider may forward to its own server
/// (tauri-plugin-sew-voice's App server route).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct JoinRequest {
    /// Opaque to the engine: the host's target (K2: the server label; an App: its Sew).
    pub target: String,
    /// Opaque to the engine: the voice channel or DM call.
    pub room: String,
    /// Join muted.
    pub muted: bool,
    /// Join deafened (implies muted).
    pub deafened: bool,
    /// `None` for the first join; why Sew dropped the join otherwise.
    pub rejoin: Option<RejoinReason>,
    /// The person's answer to the recording notice (Sew 0.7.3): Sew's
    /// `VoiceJoinRequest.recording_notice`, to forward unchanged. The engine sets it when the
    /// host answers a call held at `consent_required` ([`crate::Call::consent`]), and keeps the
    /// last answer for its re-joins (Sew asks again when the notice changed). `None`: no
    /// answer; a join into a recorded room is then refused `409 recording_consent_required`
    /// ([`ProviderError::ConsentRequired`]). Written only when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_notice: Option<RecordingNotice>,
}

impl JoinRequest {
    /// A first join of `room` through `target`: not muted, not deafened, no `rejoin`, no
    /// `recording_notice` (set them with the `with_*` setters or on the fields).
    pub fn new(target: impl Into<String>, room: impl Into<String>) -> JoinRequest {
        JoinRequest {
            target: target.into(),
            room: room.into(),
            muted: false,
            deafened: false,
            rejoin: None,
            recording_notice: None,
        }
    }

    /// Sets `muted`.
    pub fn with_muted(mut self, muted: bool) -> JoinRequest {
        self.muted = muted;
        self
    }

    /// Sets `deafened`. A deafened join is muted, so `with_deafened(true)` also sets `muted` to
    /// `true` (un-deafening later leaves the call muted, as the engine does for its own
    /// deafened joins); `with_deafened(false)` leaves `muted` as it is. A `with_muted` after
    /// this one still sets `muted`.
    pub fn with_deafened(mut self, deafened: bool) -> JoinRequest {
        self.deafened = deafened;
        if deafened {
            self.muted = true;
        }
        self
    }

    /// Sets `rejoin` to `Some(rejoin)`: why Sew dropped the join this one replaces.
    pub fn with_rejoin(mut self, rejoin: RejoinReason) -> JoinRequest {
        self.rejoin = Some(rejoin);
        self
    }

    /// Sets `rejoin` from an `Option` (`None`: a first join), for a caller that holds one.
    pub fn with_rejoin_opt(mut self, rejoin: Option<RejoinReason>) -> JoinRequest {
        self.rejoin = rejoin;
        self
    }

    /// Sets `recording_notice`: the person's answer to the recording notice.
    pub fn with_recording_notice(mut self, notice: RecordingNotice) -> JoinRequest {
        self.recording_notice = Some(notice);
        self
    }
}

/// Sew's `409 recording_consent_required` on a join (Sew 0.7.3): the room is
/// recorded (or the channel records itself on its first join) and the join carried no answer
/// to the current notice. Its `error.details`, unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ConsentRequired {
    /// The consent modal's data: `recording_id` (an id, or `"auto"`), `notice_version` (to
    /// echo with the answer) and every slot a template shows.
    pub consent_prompt: ConsentPrompt,
    /// The recording as the joiner sees it (`None` for an auto-record channel that has none
    /// yet).
    #[serde(default)]
    pub recording: Option<Recording>,
    /// An auto-record channel's first join.
    #[serde(default)]
    pub auto: bool,
}

impl ConsentRequired {
    /// From the refusal's `error.details` (`{recording, auto, consent_prompt}`); `None` when it
    /// carries no prompt.
    pub fn from_details(details: &serde_json::Value) -> Option<ConsentRequired> {
        serde_json::from_value(details.clone()).ok()
    }

    /// The prompt's version (what the answer echoes).
    pub fn notice_version(&self) -> u32 {
        self.consent_prompt.notice_version
    }

    /// The join's `recording_notice` answering this prompt with `consent`.
    pub fn notice(&self, consent: bool) -> RecordingNotice {
        RecordingNotice {
            recording_id: self.consent_prompt.recording_id,
            consent: Some(consent),
            notice_version: Some(self.consent_prompt.notice_version),
        }
    }
}

/// The person's answer to a live recording's notice, for the host to send with the person's
/// own Sew session: `POST {path}` with `{body}` (Sew's `POST /api/v1/recordings/{id}/consent`),
/// whose `200` is the recording as the person sees it ([`ConsentAnswer::recording_from_json`]).
/// `consent: false` while granted is a withdrawal ("Stop recording me"); before that, a
/// decline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ConsentAnswer {
    /// The call's `target` (the join's).
    pub target: String,
    /// The call's `room` (the join's, or a move's destination).
    pub room: String,
    /// The recording.
    pub recording_id: Id,
    /// Grant (`true`), or decline / withdraw (`false`).
    pub consent: bool,
    /// The notice version the person saw (`None` for a withdrawal: Sew then checks none).
    pub notice_version: Option<u32>,
    /// This device in Sew's call keys (its device id), for the host's own records; not part of
    /// Sew's body.
    pub device_id: Option<String>,
}

impl ConsentAnswer {
    /// Sew's route, under the client API (`/api/v1/recordings/{id}/consent`).
    pub fn path(&self) -> String {
        sew_proto::routes::with_id(sew_proto::routes::RECORDING_CONSENT, self.recording_id)
    }

    /// Sew's body (`{consent, notice_version}`).
    pub fn body(&self) -> ConsentRequest {
        ConsentRequest {
            consent: self.consent,
            notice_version: self.notice_version,
        }
    }

    /// Reads Sew's `200` answer (the recording as the person sees it). The error never quotes
    /// the body.
    pub fn recording_from_json(body: &[u8]) -> Result<Recording, ProviderError> {
        serde_json::from_slice(body)
            .map_err(|_| ProviderError::Other("the consent answer did not parse".to_owned()))
    }
}

/// The `Refused` code of [`JoinProvider::consent`]'s default: the host sends no consent.
pub const CONSENT_UNSUPPORTED: &str = "consent_unsupported";

/// What the person asks of the call's recording: press Record, or Stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RecordingControl {
    /// Start recording the call's room (Sew's `POST /api/v1/channels/{id}/recording/start`).
    Start,
    /// Stop the room's active recording (Sew's `POST /api/v1/recordings/{id}/stop`).
    Stop,
}

/// The person's Record or Stop, for the host to send with the person's own Sew session, as
/// [`ConsentAnswer`] is: `POST {path}` with `{body}`, whose `200` is the recording as the person
/// sees it ([`RecordingRequest::recording_from_json`]). Never the call credential: starting and
/// stopping are the person's acts, with Sew's permission checks and audit.
///
/// Its JSON, `{target, room, action: "start" | "stop", recording_id?, device_id?}`, is what a
/// provider forwards to its own server (the Sew App kit's native lane:
/// `POST /sew/native/voice/recording`), which maps `room` to Sew's channel as its join does.
///
/// Sew decides who may: a refusal comes back with Sew's reason (`not_in_voice`,
/// `recording_active`, `recording_disabled`, `recording_required` for a Stop while the room
/// requires its recording, `cap_ceiling`, …), read by [`ProviderError::from_sew_error`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecordingRequest {
    /// The call's `target` (the join's).
    pub target: String,
    /// The call's `room` (the join's, or a move's destination).
    pub room: String,
    /// Start or Stop.
    pub action: RecordingControl,
    /// The call's channel as Sew names it (the voice channel, DM or group DM of the join
    /// answer, or a move's destination), for a provider that calls Sew itself
    /// ([`RecordingRequest::path`]). Not in the JSON: a server maps `room` itself.
    #[serde(skip)]
    pub channel_id: Option<Id>,
    /// The active recording: Stop's route (`None` for a Start).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_id: Option<Id>,
    /// This device in Sew's call keys (its device id), for the host's own records; not part of
    /// Sew's body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

impl RecordingRequest {
    /// A Start of `channel_id`'s recording, for the call on `target` / `room`.
    pub fn start(target: impl Into<String>, room: impl Into<String>, channel_id: Id) -> Self {
        RecordingRequest {
            target: target.into(),
            room: room.into(),
            action: RecordingControl::Start,
            channel_id: Some(channel_id),
            recording_id: None,
            device_id: None,
        }
    }

    /// A Stop of `recording_id` in `channel_id`, for the call on `target` / `room`.
    pub fn stop(
        target: impl Into<String>,
        room: impl Into<String>,
        channel_id: Id,
        recording_id: Id,
    ) -> Self {
        RecordingRequest {
            target: target.into(),
            room: room.into(),
            action: RecordingControl::Stop,
            channel_id: Some(channel_id),
            recording_id: Some(recording_id),
            device_id: None,
        }
    }

    /// Sew's route, under the client API: `/api/v1/channels/{channel_id}/recording/start` for
    /// a Start, `/api/v1/recordings/{recording_id}/stop` for a Stop. A request without its id
    /// (read from JSON, or a Stop the engine never sends) keeps the route's `{id}` unfilled,
    /// which Sew refuses: a Stop never becomes a Start.
    pub fn path(&self) -> String {
        use sew_proto::routes;
        match (self.action, self.channel_id, self.recording_id) {
            (RecordingControl::Start, Some(ch), _) => {
                routes::with_id(routes::CHANNEL_RECORDING_START, ch)
            }
            (RecordingControl::Start, None, _) => routes::CHANNEL_RECORDING_START.to_owned(),
            (_, _, Some(id)) => routes::with_id(routes::RECORDING_STOP, id),
            (_, _, None) => routes::RECORDING_STOP.to_owned(),
        }
    }

    /// Sew's body: `{}` for both (a Start takes the room's defaults for keep days, mixing and
    /// language).
    pub fn body(&self) -> serde_json::Value {
        serde_json::Value::Object(serde_json::Map::new())
    }

    /// Reads Sew's `200` answer (the recording as the person sees it). The error never quotes
    /// the body.
    pub fn recording_from_json(body: &[u8]) -> Result<Recording, ProviderError> {
        serde_json::from_slice(body)
            .map_err(|_| ProviderError::Other("the recording answer did not parse".to_owned()))
    }
}

/// The `Refused` code of [`JoinProvider::recording`]'s default: the host sends no Record or
/// Stop.
pub const RECORDING_UNSUPPORTED: &str = "recording_unsupported";

/// The call-scoped credential minted with the join answer
/// (`VoiceJoinResponse.call_credential`, flag `call_credential`, Sew 0.7.1).
///
/// The same JSON shape as `sew_proto::voice::CallCredential` (Sew 0.7.1); the engine reads it
/// from the join answer's JSON ([`JoinAnswer::from_json`]).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CallCredential {
    /// `sewcc_<selector>.<secret>`.
    pub token: String,
    /// When it stops working (RFC 3339); renewed at half its life.
    pub expires_at: sew_proto::time::Timestamp,
    /// The join it is bound to (32 hex).
    pub join_id: String,
    /// Opaque and stable for the login lineage (32 hex): the device store keys on it.
    pub device_scope: String,
}

impl CallCredential {
    /// A credential from its four fields (every field is required).
    pub fn new(
        token: impl Into<String>,
        expires_at: sew_proto::time::Timestamp,
        join_id: impl Into<String>,
        device_scope: impl Into<String>,
    ) -> CallCredential {
        CallCredential {
            token: token.into(),
            expires_at,
            join_id: join_id.into(),
            device_scope: device_scope.into(),
        }
    }
}

impl fmt::Debug for CallCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallCredential")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("join_id", &self.join_id)
            .field("device_scope", &self.device_scope)
            .finish()
    }
}

/// Sew's join answer as the host got it, and the door to Sew's client API.
///
/// A voice join answers a `VoiceJoinResponse`; a DM call join (`call/join`) answers a
/// `CallJoinResponse` (`{call, created, voice, recording}`), whose `voice` is the same voice
/// answer. [`JoinAnswer::from_json`] takes either body unchanged.
#[derive(Clone)]
pub struct JoinAnswer {
    /// Sew's `VoiceJoinResponse` (a voice join's answer, or a DM call join's `voice`),
    /// unchanged.
    pub join: VoiceJoinResponse,
    /// The HTTPS base the engine uses for Sew's client API with the call credential (under K2: the
    /// hosting daemon's lane on the box's own host name; never `turn.` or `voice.`).
    pub api_base: url::Url,
    credential: Option<CallCredential>,
    dm_call: Option<DmCall>,
}

/// The DM call of a DM call join's answer (`CallJoinResponse.call` and `created`), kept by
/// [`JoinAnswer::from_json`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DmCall {
    /// The call (`calls.id`): the id the decline, cancel and re-ring routes take.
    pub call_id: Id,
    /// The DM or group DM: the `room` a host joins (and re-joins) the call with.
    pub channel_id: Id,
    /// Who started it.
    pub started_by: Id,
    /// Whether this join started the call.
    pub created: bool,
}

impl fmt::Debug for JoinAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinAnswer")
            .field("url", &self.join.url)
            .field("token", &"<redacted>")
            .field("room", &self.join.room)
            .field("api_base", &self.api_base.as_str())
            .field("call_credential", &self.credential)
            .field("dm_call", &self.dm_call)
            .finish_non_exhaustive()
    }
}

/// Why a join answer could not be read.
#[derive(Debug, thiserror::Error)]
#[error("join answer: {0}")]
pub struct AnswerError(String);

impl JoinAnswer {
    /// The answer from the HTTP body Sew (or the host's BFF) returned, as is: the recommended
    /// constructor (it keeps `call_credential` whatever `sew-proto` version is pinned).
    ///
    /// The body is a voice join's `VoiceJoinResponse`, or a DM call join's `CallJoinResponse`
    /// (`{call, created, voice, recording}`): then the voice answer and its `call_credential`
    /// are read from `voice`, and the call from `call` ([`JoinAnswer::dm_call`]).
    pub fn from_json(body: &[u8], api_base: url::Url) -> Result<JoinAnswer, AnswerError> {
        let mut v: serde_json::Value =
            serde_json::from_slice(body).map_err(|e| AnswerError(e.to_string()))?;
        let mut dm_call = None;
        if v.get("voice").is_some_and(serde_json::Value::is_object)
            && v.get("call").is_some_and(serde_json::Value::is_object)
        {
            dm_call = Some(DmCall::from_answer(&v)?);
            v = v["voice"].take();
        }
        let credential = match v.get("call_credential") {
            None | Some(serde_json::Value::Null) => None,
            Some(c) => Some(
                serde_json::from_value::<CallCredential>(c.clone())
                    .map_err(|e| AnswerError(format!("call_credential: {e}")))?,
            ),
        };
        let join: VoiceJoinResponse =
            serde_json::from_value(v).map_err(|e| AnswerError(e.to_string()))?;
        Ok(JoinAnswer {
            join,
            api_base,
            credential,
            dm_call,
        })
    }

    /// An answer from parts (tests, hosts that already parsed the body).
    pub fn new(
        join: VoiceJoinResponse,
        api_base: url::Url,
        credential: Option<CallCredential>,
    ) -> JoinAnswer {
        JoinAnswer {
            join,
            api_base,
            credential,
            dm_call: None,
        }
    }

    /// The call credential, when the join asked for one.
    pub fn call_credential(&self) -> Option<&CallCredential> {
        self.credential.as_ref()
    }

    /// The DM call, when the answer was a DM call join's (`CallJoinResponse`).
    pub fn dm_call(&self) -> Option<&DmCall> {
        self.dm_call.as_ref()
    }
}

impl DmCall {
    /// From a `CallJoinResponse` body: `call.id`, `call.channel_id`, `call.started_by` and
    /// `created`. The error never quotes the body.
    fn from_answer(v: &serde_json::Value) -> Result<DmCall, AnswerError> {
        let id = |field: &str| -> Result<Id, AnswerError> {
            v["call"][field]
                .as_str()
                .and_then(|s| s.parse::<Id>().ok())
                .ok_or_else(|| AnswerError(format!("call.{field}: not an id")))
        };
        Ok(DmCall {
            call_id: id("id")?,
            channel_id: id("channel_id")?,
            started_by: id("started_by")?,
            created: v["created"].as_bool().unwrap_or(false),
        })
    }
}

/// Why the host could not join.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderError {
    /// The host has no Sew session (the call ends with `signin_required`).
    #[error("sign-in required")]
    SigninRequired,
    /// Sew refused the join (`code` and `detail` passed through to the `errors` event).
    #[error("refused: {code}")]
    Refused {
        /// Sew's error code (or the host's).
        code: String,
        /// Its details.
        detail: serde_json::Value,
    },
    /// Sew or the host is unavailable; retry after `retry_after_ms` when given.
    #[error("unavailable")]
    Unavailable {
        /// Sew's `Retry-After`, when it gave one.
        retry_after_ms: Option<u64>,
    },
    /// Sew's `409 recording_consent_required` (Sew 0.7.3): the room is recorded and the
    /// person has not answered its current notice. A join holds the call at
    /// `consent_required` until the host answers ([`crate::Call::consent`]) or leaves.
    #[error("recording_consent_required")]
    ConsentRequired(Box<ConsentRequired>),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

impl ProviderError {
    /// Sew's error answer as the engine reads it: `401` is [`ProviderError::SigninRequired`];
    /// `409 recording_consent_required` with a prompt is [`ProviderError::ConsentRequired`];
    /// `429` and `5xx` are [`ProviderError::Unavailable`] (with `retry_after_ms`, from
    /// `Retry-After`); anything else is [`ProviderError::Refused`] with `error.details.reason`
    /// (else `error.code`, else `http_<status>`) and `error.details`. A host's server route that
    /// returns Sew's error body unchanged is read the same way.
    pub fn from_sew_error(status: u16, retry_after_ms: Option<u64>, body: &[u8]) -> ProviderError {
        let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
        let e = &v["error"];
        let details = &e["details"];
        let reason = details["reason"].as_str();
        if status == 401 {
            return ProviderError::SigninRequired;
        }
        if reason == Some(sew_proto::recording::reasons::RECORDING_CONSENT_REQUIRED)
            && let Some(c) = ConsentRequired::from_details(details)
        {
            return ProviderError::ConsentRequired(Box::new(c));
        }
        if status == 429 || status >= 500 {
            return ProviderError::Unavailable { retry_after_ms };
        }
        let code = reason
            .or_else(|| e["code"].as_str())
            .map_or_else(|| format!("http_{status}"), str::to_owned);
        ProviderError::Refused {
            code,
            detail: details.clone(),
        }
    }
}

/// The host's join callback: call Sew's voice or DM call join with `call_credential: true`
/// (and no `e2ee`) and return the answer unchanged.
///
/// # What implementing it needs
///
/// The trait and its types ([`BoxFuture`], [`JoinRequest`], [`JoinAnswer`], [`ProviderError`])
/// are this crate's. Beside `sew-call-client`, a provider needs:
///
/// - `url = "2"`: [`JoinAnswer::from_json`] takes Sew's client API base as a `url::Url`;
/// - an HTTP client of the app's choice to call Sew with the app's own session. The full
///   example below uses `reqwest = { version = "0.12", default-features = false, features =
///   ["json", "rustls-tls"] }` and `serde_json = "1"`;
/// - to run a call, a Tokio runtime (`tokio = { version = "1", features = ["rt-multi-thread",
///   "macros"] }`): [`crate::CallClient::join`] must be called from inside one.
///
/// The smallest provider, over the app's own HTTP code:
///
/// ```
/// use sew_call_client::{BoxFuture, JoinAnswer, JoinProvider, JoinRequest, ProviderError};
///
/// struct AppJoin {
///     /// Sew's client API base, e.g. `https://chat.example.com/`.
///     api_base: url::Url,
/// }
///
/// impl AppJoin {
///     /// The app's own call to Sew's voice join for `req.room` (with `call_credential: true`),
///     /// answering the response body.
///     async fn post_voice_join(&self, req: &JoinRequest) -> Result<Vec<u8>, ProviderError> {
///         # let _ = req;
///         # Err(ProviderError::Unavailable { retry_after_ms: None })
///     }
/// }
///
/// impl JoinProvider for AppJoin {
///     fn join(&self, req: JoinRequest) -> BoxFuture<'_, Result<JoinAnswer, ProviderError>> {
///         Box::pin(async move {
///             let body = self.post_voice_join(&req).await?;
///             JoinAnswer::from_json(&body, self.api_base.clone())
///                 .map_err(|e| ProviderError::Other(e.to_string()))
///         })
///     }
/// }
/// ```
///
/// A complete provider for an app that holds its own Sew session (a bearer token for Sew's
/// client API), and a call through it:
///
/// ```no_run
/// use std::path::PathBuf;
/// use std::sync::Arc;
///
/// use sew_call_client::{
///     BoxFuture, CallClient, ClientConfig, JoinAnswer, JoinProvider, JoinRequest, ProviderError,
/// };
///
/// /// The app's sign-in, kept in Rust: Sew's client API base and the app's session token.
/// struct MyJoinProvider {
///     http: reqwest::Client,
///     api_base: url::Url,
///     session_token: String,
/// }
///
/// impl JoinProvider for MyJoinProvider {
///     fn join(&self, req: JoinRequest) -> BoxFuture<'_, Result<JoinAnswer, ProviderError>> {
///         Box::pin(async move {
///             // Sew's voice join for the channel, asking for a call credential (no `e2ee`).
///             let path = format!("api/v1/channels/{}/voice/join", req.room);
///             let url = self
///                 .api_base
///                 .join(&path)
///                 .map_err(|e| ProviderError::Other(e.to_string()))?;
///             let body = serde_json::json!({
///                 "self_mute": req.muted,
///                 "self_deaf": req.deafened,
///                 "call_credential": true,
///             });
///             let resp = self
///                 .http
///                 .post(url)
///                 .bearer_auth(&self.session_token)
///                 .json(&body)
///                 .send()
///                 .await
///                 .map_err(|_| ProviderError::Unavailable { retry_after_ms: None })?;
///             if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
///                 return Err(ProviderError::SigninRequired);
///             }
///             if !resp.status().is_success() {
///                 return Err(ProviderError::Refused {
///                     code: resp.status().as_u16().to_string(),
///                     detail: serde_json::Value::Null,
///                 });
///             }
///             let bytes = resp
///                 .bytes()
///                 .await
///                 .map_err(|e| ProviderError::Other(e.to_string()))?;
///             // The answer, unchanged; the engine reaches Sew's client API at `api_base`.
///             JoinAnswer::from_json(&bytes, self.api_base.clone())
///                 .map_err(|e| ProviderError::Other(e.to_string()))
///         })
///     }
/// }
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let provider = Arc::new(MyJoinProvider {
///     http: reqwest::Client::new(),
///     api_base: url::Url::parse("https://chat.example.com/")?,
///     session_token: "the app's Sew session".into(),
/// });
/// // Where the engine keeps the device key and its pins: a directory of the app's own.
/// let data_dir = PathBuf::from("/var/lib/example-app/sew-voice");
/// let client = CallClient::with_mls(ClientConfig::new("com.example.app", data_dir));
/// let call = client
///     .join(
///         provider,
///         // The room is the voice channel's id.
///         JoinRequest::new("chat.example.com", "48831765147549699"),
///     )
///     .await?;
/// let mut events = call.events(); // CallEvent::State, Participants, Speaking, Error
/// while let Ok(event) = events.recv().await {
///     println!("{event:?}");
/// }
/// # Ok(())
/// # }
/// ```
pub trait JoinProvider: Send + Sync + 'static {
    /// Join (or re-join) `req.room` on `req.target`, with `req.recording_notice` as the voice
    /// join's `recording_notice` when it has one. A refusal that is Sew's
    /// `409 recording_consent_required` should come back as
    /// [`ProviderError::ConsentRequired`] ([`ProviderError::from_sew_error`] reads it), so the
    /// call can hold for the person's answer.
    fn join(&self, req: JoinRequest) -> BoxFuture<'_, Result<JoinAnswer, ProviderError>>;

    /// Send the person's answer to a live recording's notice with the person's own Sew
    /// session (Sew 0.7.3): `POST answer.path()` with `answer.body()`, and return Sew's
    /// answer ([`ConsentAnswer::recording_from_json`]); a refusal as in
    /// [`ProviderError::from_sew_error`] (a stale `notice_version` is
    /// [`ProviderError::ConsentRequired`] with the current prompt). The engine never treats a
    /// call as consented by itself: only Sew's answer, or a later `recording_updated`, moves
    /// this device to `granted` and releases its key to the recorder.
    ///
    /// The default sends nothing and answers `Refused { code: "consent_unsupported" }`: the
    /// person then stays unrecorded (`pending`) in a call recorded after they joined.
    fn consent(&self, answer: ConsentAnswer) -> BoxFuture<'_, Result<Recording, ProviderError>> {
        let _ = answer;
        Box::pin(std::future::ready(Err(ProviderError::Refused {
            code: CONSENT_UNSUPPORTED.to_owned(),
            detail: serde_json::Value::Null,
        })))
    }

    /// Send the person's Record or Stop with the person's own Sew session (Sew 0.9.0):
    /// `POST req.path()` with `req.body()`, and return Sew's answer
    /// ([`RecordingRequest::recording_from_json`]); a refusal as in
    /// [`ProviderError::from_sew_error`]. Sew's answer becomes the call's recording at once,
    /// and the call socket's `recording_updated` follows for everyone in the room. A Start is
    /// the starter's own consent (Sew records it as their answer, so show the notice as a
    /// confirmation first) and nobody else's: everyone else in the room is `pending` until
    /// they answer.
    ///
    /// The default sends nothing and answers `Refused { code: "recording_unsupported" }`.
    fn recording(&self, req: RecordingRequest) -> BoxFuture<'_, Result<Recording, ProviderError>> {
        let _ = req;
        Box::pin(std::future::ready(Err(ProviderError::Refused {
            code: RECORDING_UNSUPPORTED.to_owned(),
            detail: serde_json::Value::Null,
        })))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn with_deafened_true_also_mutes_and_false_leaves_muted() {
        let r = JoinRequest::new("t", "r").with_deafened(true);
        assert!(r.deafened, "with_deafened(true) did not set deafened");
        assert!(r.muted, "with_deafened(true) did not set muted");

        let r = JoinRequest::new("t", "r").with_deafened(false);
        assert!(!r.deafened);
        assert!(
            !r.muted,
            "with_deafened(false) changed an unmuted join's muted"
        );

        let r = JoinRequest::new("t", "r")
            .with_muted(true)
            .with_deafened(false);
        assert!(!r.deafened);
        assert!(r.muted, "with_deafened(false) cleared muted");

        let r = JoinRequest::new("t", "r")
            .with_deafened(true)
            .with_deafened(false);
        assert!(!r.deafened);
        assert!(r.muted, "un-deafening the builder cleared muted");
    }

    #[test]
    fn with_rejoin_opt_sets_and_clears_rejoin() {
        let r = JoinRequest::new("t", "r").with_rejoin_opt(Some(RejoinReason::CredentialExpired));
        assert_eq!(r.rejoin, Some(RejoinReason::CredentialExpired));

        let r = JoinRequest::new("t", "r")
            .with_rejoin(RejoinReason::AdmitLost)
            .with_rejoin_opt(None);
        assert_eq!(
            r.rejoin, None,
            "with_rejoin_opt(None) kept the earlier reason"
        );

        let a = JoinRequest::new("t", "r").with_rejoin(RejoinReason::AdmitLost);
        let b = JoinRequest::new("t", "r").with_rejoin_opt(Some(RejoinReason::AdmitLost));
        assert_eq!(a, b);
    }

    fn body(cred: bool) -> Vec<u8> {
        let mut v = serde_json::json!({
            "url": "wss://voice.example",
            "token": "media-token",
            "expires_at": "2026-11-02T10:00:00Z",
            "room": sew_proto::e2ee::CallId::from_random([7; 16]),
            "state": {
                "member_id": "1001",
                "channel_id": "2002",
                "connection": "connecting",
                "secure": false,
                "self_mute": false,
                "self_deaf": false,
                "server_mute": false,
                "server_deaf": false,
                "speak_denied": false,
                "joined_at": "2026-11-02T09:58:00Z"
            }
        });
        if cred {
            v["call_credential"] = serde_json::json!({
                "token": "sewcc_sel.secret",
                "expires_at": "2026-11-02T10:08:00Z",
                "join_id": "0123456789abcdef0123456789abcdef",
                "device_scope": "fedcba9876543210fedcba9876543210"
            });
        }
        serde_json::to_vec(&v).unwrap()
    }

    fn prompt() -> serde_json::Value {
        serde_json::json!({
            "recording_id": "77", "notice_version": 3141, "recorder_name": "Sew Notes",
            "feature_label": "Sew Notes",
            "started_by": {"member_id": "1001", "display_name": "Alice"},
            "room": {"channel_id": "2002", "kind": "voice", "name": "lounge"},
            "audio_keep_days": 30,
            "transcript": {"posted_to_channel": true, "readable_by": ["channel_members"],
                           "e2ee": false},
            "admin_download": false, "decline_mode": "join_unrecorded",
            "recorded_now": ["1001"]
        })
    }

    #[test]
    fn sews_consent_refusal_becomes_consent_required() {
        let body = serde_json::json!({"ok": false, "error": {
            "code": "conflict", "hint": "answer the recording notice",
            "details": {"reason": "recording_consent_required", "recording": null, "auto": false,
                        "consent_prompt": prompt()}}});
        let e = ProviderError::from_sew_error(409, None, &serde_json::to_vec(&body).unwrap());
        let ProviderError::ConsentRequired(c) = e else {
            panic!("not consent_required: {e:?}");
        };
        assert_eq!(c.notice_version(), 3141);
        let n = serde_json::to_value(c.notice(true)).unwrap();
        assert_eq!(
            n,
            serde_json::json!({"recording_id": "77", "consent": true, "notice_version": 3141})
        );
        // The other answers.
        assert_eq!(
            ProviderError::from_sew_error(401, None, b"{}"),
            ProviderError::SigninRequired
        );
        assert_eq!(
            ProviderError::from_sew_error(503, Some(2000), b"not json"),
            ProviderError::Unavailable {
                retry_after_ms: Some(2000)
            }
        );
        let denied = serde_json::json!({"ok": false, "error": {"code": "policy_denied",
            "hint": "x", "details": {"reason": "recording_consent_required_to_join"}}});
        match ProviderError::from_sew_error(403, None, &serde_json::to_vec(&denied).unwrap()) {
            ProviderError::Refused { code, detail } => {
                assert_eq!(code, "recording_consent_required_to_join");
                assert_eq!(detail["reason"], "recording_consent_required_to_join");
            }
            e => panic!("{e:?}"),
        }
        // A 409 without a prompt is a plain refusal.
        let bare = serde_json::json!({"ok": false, "error": {"code": "conflict", "hint": "x",
            "details": {"reason": "recording_consent_required"}}});
        assert!(matches!(
            ProviderError::from_sew_error(409, None, &serde_json::to_vec(&bare).unwrap()),
            ProviderError::Refused { .. }
        ));
    }

    #[test]
    fn join_request_writes_the_notice_only_when_there_is_one() {
        let mut req = JoinRequest {
            target: "acme".into(),
            room: "2002".into(),
            muted: false,
            deafened: false,
            rejoin: None,
            recording_notice: None,
        };
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            serde_json::json!({"target": "acme", "room": "2002", "muted": false,
                               "deafened": false, "rejoin": null})
        );
        req.recording_notice = Some(RecordingNotice {
            recording_id: sew_proto::recording::RecordingRef::Auto,
            consent: Some(false),
            notice_version: Some(7),
        });
        assert_eq!(
            serde_json::to_value(&req).unwrap()["recording_notice"],
            serde_json::json!({"recording_id": "auto", "consent": false, "notice_version": 7})
        );
        let a = ConsentAnswer {
            target: "acme".into(),
            room: "2002".into(),
            recording_id: Id::new(77).unwrap(),
            consent: false,
            notice_version: None,
            device_id: None,
        };
        assert_eq!(a.path(), "/api/v1/recordings/77/consent");
        assert_eq!(
            serde_json::to_value(a.body()).unwrap(),
            serde_json::json!({"consent": false})
        );
    }

    #[test]
    fn a_recording_says_what_started_it() {
        use crate::call::CallRecording;
        use sew_proto::recording::RecordingTrigger;
        let rec = |auto: bool| {
            serde_json::json!({
                "id": "77", "channel_id": "2002", "state": "recording", "started_by": "1001",
                "started_at": "2026-11-02T09:58:00Z", "auto": auto, "message_id": "88",
                "mixed": false, "audio_keep_days": 30, "max_duration_ms": 14_400_000u64,
                "participants": []
            })
        };
        for (auto, want) in [
            (false, RecordingTrigger::Manual),
            (true, RecordingTrigger::Auto),
        ] {
            let r: Recording = serde_json::from_value(rec(auto)).unwrap();
            let v = CallRecording::from_sew(&r);
            assert_eq!(v.trigger, want);
            assert!(!v.required, "a 0.8.x recording is never required");
            assert_eq!(
                serde_json::to_value(&v).unwrap()["trigger"],
                serde_json::to_value(want).unwrap()
            );
        }
        // A held join with no recording yet: an auto-record room's first join.
        let c = ConsentRequired::from_details(&serde_json::json!({
            "recording": null, "auto": true, "consent_prompt": prompt()
        }))
        .unwrap();
        let v = CallRecording::from_consent_required(&c);
        assert_eq!(v.trigger, RecordingTrigger::Auto);
        assert!(!v.required);
    }

    #[test]
    fn a_recording_carries_its_start_clock() {
        use crate::call::CallRecording;
        let rec = serde_json::json!({
            "id": "77", "channel_id": "2002", "state": "recording", "started_by": "1001",
            "started_at": "2026-11-02T09:58:00Z", "auto": false, "message_id": "88",
            "mixed": false, "audio_keep_days": 30, "max_duration_ms": 14_400_000u64,
            "participants": []
        });
        let r: Recording = serde_json::from_value(rec.clone()).unwrap();
        let v = CallRecording::from_sew(&r);
        assert_eq!(v.started_by.as_deref(), Some("1001"));
        let at = v.started_at.expect("Sew's recording carries its start");
        assert_eq!(at.to_wire(), "2026-11-02T09:58:00.000Z");
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["started_at"], "2026-11-02T09:58:00.000Z");
        assert_eq!(json["started_by"], "1001");
        // Joining a call already being recorded: the held join's recording keeps both.
        let c = ConsentRequired::from_details(&serde_json::json!({
            "recording": rec, "auto": false, "consent_prompt": prompt()
        }))
        .unwrap();
        let held = CallRecording::from_consent_required(&c);
        assert_eq!(held.started_at, Some(at));
        assert_eq!(held.started_by.as_deref(), Some("1001"));
        // An auto-record room's first join: no recording yet, so no start.
        let c = ConsentRequired::from_details(&serde_json::json!({
            "recording": null, "auto": true, "consent_prompt": prompt()
        }))
        .unwrap();
        let v = CallRecording::from_consent_required(&c);
        assert_eq!(v.started_at, None);
        let json = serde_json::to_value(&v).unwrap();
        assert!(
            json.get("started_at").is_none(),
            "left out when unknown: {json}"
        );
    }

    #[test]
    fn a_recording_carries_its_audio_keep_days() {
        use crate::call::{CallRecording, RecordingConsent};
        let rec = |days: u16| {
            serde_json::json!({
                "id": "77", "channel_id": "2002", "state": "recording", "started_by": "1001",
                "started_at": "2026-11-02T09:58:00Z", "auto": false, "message_id": "88",
                "mixed": false, "audio_keep_days": days, "max_duration_ms": 14_400_000u64,
                "participants": [], "my_consent": "granted"
            })
        };
        // Sew's recording, after the person answered: the keep days stay with it.
        let r: Recording = serde_json::from_value(rec(7)).unwrap();
        let v = CallRecording::from_sew(&r);
        assert_eq!(v.audio_keep_days, 7);
        assert_eq!(v.my_consent, RecordingConsent::Granted);
        assert!(v.consent_prompt.is_none(), "answered: no notice");
        assert_eq!(serde_json::to_value(&v).unwrap()["audio_keep_days"], 7);
        // 0 (deleted once the transcript is final) is a real value and is written.
        let r: Recording = serde_json::from_value(rec(0)).unwrap();
        let v = CallRecording::from_sew(&r);
        assert_eq!(v.audio_keep_days, 0);
        assert_eq!(serde_json::to_value(&v).unwrap()["audio_keep_days"], 0);
        // A held join into a call already being recorded: the recording's.
        let mut p = prompt();
        p["audio_keep_days"] = serde_json::json!(7);
        let c = ConsentRequired::from_details(&serde_json::json!({
            "recording": rec(7), "auto": false, "consent_prompt": p
        }))
        .unwrap();
        assert_eq!(CallRecording::from_consent_required(&c).audio_keep_days, 7);
        // An auto-record room's first join: no recording yet, so the notice's.
        let mut p = prompt();
        p["recording_id"] = serde_json::json!("auto");
        p["audio_keep_days"] = serde_json::json!(14);
        let c = ConsentRequired::from_details(&serde_json::json!({
            "recording": null, "auto": true, "consent_prompt": p
        }))
        .unwrap();
        let v = CallRecording::from_consent_required(&c);
        assert_eq!(v.recording_id, "auto");
        assert_eq!(v.audio_keep_days, 14);
        assert_eq!(serde_json::to_value(&v).unwrap()["audio_keep_days"], 14);
    }

    #[test]
    fn recording_requests_name_sews_routes() {
        let ch = Id::new(2002).unwrap();
        let start = RecordingRequest::start("acme", "2002", ch);
        assert_eq!(start.path(), "/api/v1/channels/2002/recording/start");
        assert_eq!(start.body(), serde_json::json!({}));
        assert_eq!(
            serde_json::to_value(&start).unwrap(),
            serde_json::json!({"target": "acme", "room": "2002", "action": "start"})
        );
        let stop = RecordingRequest::stop("acme", "2002", ch, Id::new(77).unwrap());
        assert_eq!(stop.path(), "/api/v1/recordings/77/stop");
        assert_eq!(stop.body(), serde_json::json!({}));
        assert_eq!(
            serde_json::to_value(&stop).unwrap(),
            serde_json::json!({"target": "acme", "room": "2002", "action": "stop",
                               "recording_id": "77"})
        );
        // Read from a server's JSON: no channel, so a Start's route stays unfilled.
        let read: RecordingRequest = serde_json::from_value(
            serde_json::json!({"target": "acme", "room": "2002", "action": "start"}),
        )
        .unwrap();
        assert_eq!(read.channel_id, None);
        assert_eq!(read.path(), "/api/v1/channels/{id}/recording/start");
        // A Stop that names no recording never becomes a Start.
        let mut bare = stop.clone();
        bare.recording_id = None;
        assert_eq!(bare.path(), "/api/v1/recordings/{id}/stop");
        assert!(RecordingRequest::recording_from_json(b"not json").is_err());
    }

    #[test]
    fn the_default_provider_sends_no_record_or_stop() {
        struct P;
        impl JoinProvider for P {
            fn join(&self, _: JoinRequest) -> BoxFuture<'_, Result<JoinAnswer, ProviderError>> {
                Box::pin(std::future::ready(Err(ProviderError::SigninRequired)))
            }
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let req = RecordingRequest::start("acme", "2002", Id::new(2002).unwrap());
        match rt.block_on(P.recording(req)) {
            Err(ProviderError::Refused { code, .. }) => assert_eq!(code, RECORDING_UNSUPPORTED),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn answer_keeps_the_credential_and_redacts_tokens() {
        let base = url::Url::parse("https://chat.example.com/_k2/voice/sew/").unwrap();
        let a = match JoinAnswer::from_json(&body(true), base.clone()) {
            Ok(a) => a,
            Err(e) => panic!("{e}"),
        };
        let c = a.call_credential().unwrap();
        assert_eq!(c.token, "sewcc_sel.secret");
        assert_eq!(c.device_scope.len(), 32);
        let dbg = format!("{a:?}");
        assert!(
            !dbg.contains("sewcc_sel.secret") && !dbg.contains("media-token"),
            "{dbg}"
        );
        let a = JoinAnswer::from_json(&body(false), base).unwrap();
        assert!(a.call_credential().is_none());
        assert!(
            a.dm_call().is_none(),
            "a voice join's answer has no DM call"
        );
    }

    /// A DM call join's answer (`CallJoinResponse`), as Sew writes it.
    fn call_join_body(cred: bool) -> Vec<u8> {
        let voice: serde_json::Value = serde_json::from_slice(&body(cred)).unwrap();
        let v = serde_json::json!({
            "call": {
                "id": "3003",
                "channel_id": "2002",
                "message_id": "4004",
                "started_by": "1001",
                "started_at": "2026-11-02T09:58:00Z",
                "state": "active",
                "participant_ids": ["1001"],
                "rings": [{"member_id": "1005", "state": "ringing",
                           "expires_at": "2026-11-02T09:58:45Z"}]
            },
            "created": true,
            "voice": voice
        });
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn a_dm_call_join_answer_is_read_unchanged() {
        let base = url::Url::parse("https://chat.example.com/_k2/voice/sew/").unwrap();
        for cred in [true, false] {
            let a = match JoinAnswer::from_json(&call_join_body(cred), base.clone()) {
                Ok(a) => a,
                Err(e) => panic!("{e}"),
            };
            // The voice half is the same answer as a voice join's.
            let plain = JoinAnswer::from_json(&body(cred), base.clone()).unwrap();
            assert_eq!(a.join, plain.join);
            assert_eq!(a.call_credential(), plain.call_credential());
            assert_eq!(a.call_credential().is_some(), cred);
            assert_eq!(a.api_base, base);
            let call = a.dm_call().unwrap();
            assert_eq!(call.call_id, Id::new(3003).unwrap());
            assert_eq!(call.channel_id, Id::new(2002).unwrap());
            assert_eq!(call.started_by, Id::new(1001).unwrap());
            assert!(call.created);
            // The DM is the room a host joins and re-joins with: the voice state's channel.
            assert_eq!(a.join.state.channel_id, call.channel_id);
            let dbg = format!("{a:?}");
            assert!(
                !dbg.contains("sewcc_sel.secret") && !dbg.contains("media-token"),
                "{dbg}"
            );
        }
        // A call/join answer that does not parse fails without quoting it.
        let mut bad: serde_json::Value = serde_json::from_slice(&call_join_body(true)).unwrap();
        bad["call"]["id"] = serde_json::json!(null);
        let e = JoinAnswer::from_json(&serde_json::to_vec(&bad).unwrap(), base.clone())
            .err()
            .unwrap();
        assert!(!e.to_string().contains("sewcc_"), "{e}");
        let mut bad: serde_json::Value = serde_json::from_slice(&call_join_body(true)).unwrap();
        bad["voice"]["token"] = serde_json::json!(7);
        assert!(JoinAnswer::from_json(&serde_json::to_vec(&bad).unwrap(), base).is_err());
    }
}
