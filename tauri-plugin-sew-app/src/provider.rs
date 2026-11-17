//! A ready [`JoinProvider`] for `tauri-plugin-sew-voice`: the App server lane's
//! `voice/join`, `voice/consent` and `voice/recording`, with the account's device bearer.
//!
//! The account is the active one at the first join of a call; re-joins and consent answers
//! of that call keep it, even when the person switches accounts meanwhile. `JoinRequest`'s
//! `target` and `room` go to the lane unchanged: `room` is the channel id, and `target: "dm"`
//! (or `"call"`) asks for the DM call join.
//!
//! Errors, as the voice plugin reads them:
//! - `401` → [`ProviderError::SigninRequired`] (and the account is marked signed out: its
//!   bearer is deleted and `sew-app://signed_out` sent);
//! - `409 recording_consent_required` with Sew's prompt → [`ProviderError::ConsentRequired`];
//! - `429` and `503` → [`ProviderError::Unavailable`] with `Retry-After`; no answer at all →
//!   `Unavailable` too;
//! - `404` with no code of its own (or the lane's generic `not_found`) → `Refused` with
//!   `lane_not_found` (the server has no native lane, or it is off), `detail.route` naming the
//!   route; any other status → `Refused` with the lane's `{code, detail}`.

use std::fmt;
use std::sync::{Arc, OnceLock};

use serde_json::Value;
use tauri_plugin_sew_voice::sew_call_client::Recording;
use tauri_plugin_sew_voice::{
    BoxFuture, ConsentAnswer, ConsentRequired, JoinAnswer, JoinProvider, JoinRequest,
    ProviderError, RecordingRequest, app_server_answer,
};

use crate::error::code;
use crate::lane::{LaneRequest, LaneResponse, Method, Refusal, route};
use crate::servers::AccountKey;
use crate::service::AppService;

/// The lane's join and consent, with the active account's device bearer.
///
/// `Builder::join_provider` hands one out before the app is set up (the voice
/// plugin's builder needs it first); the sew-app plugin's setup binds it to the core. A join
/// before that is refused `provider_error` ("not set up").
#[derive(Clone)]
pub struct LaneJoinProvider {
    slot: Arc<OnceLock<AppService>>,
}

impl fmt::Debug for LaneJoinProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaneJoinProvider").finish_non_exhaustive()
    }
}

/// A non-`2xx` answer of `route` as the voice plugin reads it.
pub fn provider_error(route: &str, resp: &LaneResponse) -> ProviderError {
    let r = Refusal::from_response(resp);
    match resp.status {
        401 => ProviderError::SigninRequired,
        429 | 503 => ProviderError::Unavailable {
            retry_after_ms: resp.retry_after_ms,
        },
        404 if matches!(r.code.as_deref(), None | Some("not_found")) => ProviderError::Refused {
            code: code::LANE_NOT_FOUND.to_owned(),
            detail: serde_json::json!({ "route": route, "status": 404 }),
        },
        _ => {
            let c = r.code.unwrap_or_else(|| format!("http_{}", resp.status));
            if c == "recording_consent_required"
                && let Some(prompt) = ConsentRequired::from_details(&r.detail)
            {
                return ProviderError::ConsentRequired(Box::new(prompt));
            }
            ProviderError::Refused {
                code: c,
                detail: r.detail,
            }
        }
    }
}

impl LaneJoinProvider {
    /// The provider over the core.
    pub fn new(svc: AppService) -> LaneJoinProvider {
        let slot = Arc::new(OnceLock::new());
        let _ = slot.set(svc);
        LaneJoinProvider { slot }
    }

    /// A provider bound later ([`LaneJoinProvider::bind`]).
    pub fn unbound() -> LaneJoinProvider {
        LaneJoinProvider {
            slot: Arc::new(OnceLock::new()),
        }
    }

    /// Binds an unbound provider (and its clones) to the core; a second bind is ignored.
    pub fn bind(&self, svc: AppService) {
        let _ = self.slot.set(svc);
    }

    fn svc(&self) -> Result<&AppService, ProviderError> {
        self.slot
            .get()
            .ok_or_else(|| ProviderError::Other("sew-app is not set up yet".into()))
    }

    /// The account for this request: the call's own on a re-join (or a consent), else the
    /// active one (pinned for the call).
    fn account(&self, rejoin: bool) -> Result<AccountKey, ProviderError> {
        let svc = self.svc()?;
        if rejoin && let Some(k) = svc.call_account() {
            return Ok(k);
        }
        let k = svc.active().ok_or(ProviderError::SigninRequired)?;
        svc.set_call_account(Some(k.clone()));
        Ok(k)
    }

    async fn post(
        &self,
        key: &AccountKey,
        path: &str,
        body: Value,
    ) -> Result<LaneResponse, ProviderError> {
        let svc = self.svc()?;
        let bearer = match svc.bearer(key).await {
            Ok(b) => b,
            Err(e) if e.code == code::SIGNED_OUT || e.code == code::UNKNOWN_ACCOUNT => {
                return Err(ProviderError::SigninRequired);
            }
            Err(e) => return Err(ProviderError::Other(e.code)),
        };
        let req = LaneRequest {
            method: Method::Post,
            path: path.to_owned(),
            bearer: Some(bearer),
            body: Some(body),
        };
        let resp =
            svc.lane()
                .send(&key.server, req)
                .await
                .map_err(|_| ProviderError::Unavailable {
                    retry_after_ms: None,
                })?;
        if (200..300).contains(&resp.status) {
            return Ok(resp);
        }
        if resp.status == 401 {
            let reason = Refusal::from_response(&resp).signed_out_reason();
            svc.session_ended(key, &reason).await;
        }
        Err(provider_error(path, &resp))
    }
}

impl JoinProvider for LaneJoinProvider {
    fn join(&self, req: JoinRequest) -> BoxFuture<'_, Result<JoinAnswer, ProviderError>> {
        Box::pin(async move {
            let key = self.account(req.rejoin.is_some())?;
            let body = serde_json::to_value(&req)
                .map_err(|_| ProviderError::Other("the join request did not encode".into()))?;
            let resp = self.post(&key, route::VOICE_JOIN, body).await?;
            // Never echo the body: it carries the media token and the call credential.
            app_server_answer(&resp.body)
        })
    }

    fn consent(&self, answer: ConsentAnswer) -> BoxFuture<'_, Result<Recording, ProviderError>> {
        Box::pin(async move {
            let key = self.account(true)?;
            let body = serde_json::to_value(&answer)
                .map_err(|_| ProviderError::Other("the consent did not encode".into()))?;
            let resp = self.post(&key, route::VOICE_CONSENT, body).await?;
            ConsentAnswer::recording_from_json(&resp.body)
        })
    }

    /// The person's Record or Stop (Sew 0.9.0): `POST /sew/native/voice/recording` with the
    /// request's JSON (`{target, room, action, recording_id?, device_id?}`), on the call's
    /// account. A lane without the route (`/sew/native/info` without `recording: true`) is
    /// `recording_unsupported`.
    fn recording(&self, req: RecordingRequest) -> BoxFuture<'_, Result<Recording, ProviderError>> {
        Box::pin(async move {
            let key = self.account(true)?;
            let body = serde_json::to_value(&req)
                .map_err(|_| ProviderError::Other("the recording request did not encode".into()))?;
            let resp = match self.post(&key, route::VOICE_RECORDING, body).await {
                Ok(r) => r,
                Err(ProviderError::Refused { code: c, .. }) if c == code::LANE_NOT_FOUND => {
                    return Err(ProviderError::Refused {
                        code: tauri_plugin_sew_voice::RECORDING_UNSUPPORTED.to_owned(),
                        detail: serde_json::json!({ "route": route::VOICE_RECORDING }),
                    });
                }
                Err(e) => return Err(e),
            };
            RecordingRequest::recording_from_json(&resp.body)
        })
    }
}
