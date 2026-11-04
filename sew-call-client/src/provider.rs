//! The host's join callback: the engine asks the host for Sew's join answer
//! to join and to re-join when Sew dropped the join. The web UI never sees a token.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
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

/// What the host joins. Its JSON (`{target, room, muted, deafened, rejoin}`) is what a host's
/// provider may forward to its own server (tauri-plugin-sew-voice's App server route).
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
}

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
    /// Anything else.
    #[error("{0}")]
    Other(String),
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
    /// Join (or re-join) `req.room` on `req.target`.
    fn join(&self, req: JoinRequest) -> BoxFuture<'_, Result<JoinAnswer, ProviderError>>;
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
