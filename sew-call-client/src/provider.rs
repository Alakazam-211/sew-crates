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

/// The call-scoped credential minted with the join answer
/// (`VoiceJoinResponse.call_credential`, flag `call_credential`, Sew 0.7.1).
///
/// The same JSON shape as `sew_proto::voice::CallCredential` (Sew 0.7.1); the engine reads it
/// from the join answer's JSON ([`JoinAnswer::from_json`]).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone)]
pub struct JoinAnswer {
    /// Sew's `VoiceJoinResponse` (voice or DM call join), unchanged.
    pub join: VoiceJoinResponse,
    /// The HTTPS base the engine uses for Sew's client API with the call credential (under K2: the
    /// hosting daemon's lane on the box's own host name; never `turn.` or `voice.`).
    pub api_base: url::Url,
    credential: Option<CallCredential>,
}

impl fmt::Debug for JoinAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinAnswer")
            .field("url", &self.join.url)
            .field("token", &"<redacted>")
            .field("room", &self.join.room)
            .field("api_base", &self.api_base.as_str())
            .field("call_credential", &self.credential)
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
    pub fn from_json(body: &[u8], api_base: url::Url) -> Result<JoinAnswer, AnswerError> {
        let v: serde_json::Value =
            serde_json::from_slice(body).map_err(|e| AnswerError(e.to_string()))?;
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
        }
    }

    /// The call credential, when the join asked for one.
    pub fn call_credential(&self) -> Option<&CallCredential> {
        self.credential.as_ref()
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
/// A minimal provider for an app that holds its own Sew session (a bearer token for Sew's client
/// API), and a call through it:
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
///         JoinRequest {
///             target: "chat.example.com".into(),
///             room: "48831765147549699".into(), // the voice channel's id
///             muted: false,
///             deafened: false,
///             rejoin: None,
///             recording_notice: None,
///         },
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
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

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
    }
}
