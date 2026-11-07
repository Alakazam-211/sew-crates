//! The engine's own calls to Sew's client API through the door (`JoinAnswer.api_base`) with the
//! call credential: renew, leave and self state. The key routes and the call socket are the
//! E2EE layer's (`e2ee`).

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::config::is_loopback_host;

/// One request's time limit.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// An HTTP client with the engine's TLS trust.
pub(crate) fn http_client(tls: &Arc<rustls::ClientConfig>) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .use_preconfigured_tls((**tls).clone())
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("sew-call-client/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())
}

/// Why a call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ApiError {
    /// Sew answered `status` with `details.reason` (when it gave one).
    Status {
        /// The HTTP status.
        status: u16,
        /// `details.reason`.
        reason: Option<String>,
    },
    /// The door could not be reached.
    Io(String),
}

/// `error.code` and `error.details.reason` of an error answer (Sew's `{"ok": false, "error":
/// {"code", "hint", "details"}}`; a K2 door answers in the same shape).
pub(crate) fn error_of(body: &[u8]) -> (Option<String>, Option<String>) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (None, None);
    };
    let e = &v["error"];
    let code = e["code"].as_str().map(str::to_owned);
    let reason = e["details"]["reason"].as_str().map(str::to_owned);
    (code, reason)
}

/// `POST /api/v1/me/voice/credential`'s answer.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Renewed {
    pub(crate) token: String,
    pub(crate) expires_at: sew_proto::time::Timestamp,
}

/// The URL of `path` under the door (`https`, or `http` to a loopback host only).
pub(crate) fn route(base: &url::Url, path: &str) -> Result<url::Url, ApiError> {
    let ok = match base.scheme() {
        "https" => true,
        "http" => base.host_str().is_some_and(is_loopback_host),
        _ => false,
    };
    if !ok {
        return Err(ApiError::Io(format!("the door {base} is not https")));
    }
    let mut b = base.clone();
    if !b.path().ends_with('/') {
        let p = format!("{}/", b.path());
        b.set_path(&p);
    }
    b.join(path.trim_start_matches('/'))
        .map_err(|e| ApiError::Io(e.to_string()))
}

async fn post(
    http: &reqwest::Client,
    base: &url::Url,
    path: &str,
    bearer: &str,
    body: serde_json::Value,
) -> Result<reqwest::Response, ApiError> {
    let url = route(base, path)?;
    let r = http
        .post(url)
        .bearer_auth(bearer)
        .json(&body)
        .send()
        .await
        .map_err(|e| ApiError::Io(e.without_url().to_string()))?;
    if r.status().is_success() {
        return Ok(r);
    }
    let status = r.status().as_u16();
    let body = r.bytes().await.unwrap_or_default();
    let (_, reason) = error_of(&body);
    Err(ApiError::Status { status, reason })
}

/// Renews the call credential.
pub(crate) async fn renew(
    http: &reqwest::Client,
    base: &url::Url,
    bearer: &str,
) -> Result<Renewed, ApiError> {
    let r = post(http, base, "/api/v1/me/voice/credential", bearer, json!({})).await?;
    r.json::<Renewed>()
        .await
        .map_err(|e| ApiError::Io(e.without_url().to_string()))
}

/// Leaves the call (Sew's side).
pub(crate) async fn leave(
    http: &reqwest::Client,
    base: &url::Url,
    bearer: &str,
) -> Result<(), ApiError> {
    post(http, base, "/api/v1/me/voice/leave", bearer, json!({}))
        .await
        .map(|_| ())
}

/// Sends one call-stats sample (`POST /api/v1/me/voice/stats`, in the credential's scope).
pub(crate) async fn stats(
    http: &reqwest::Client,
    base: &url::Url,
    bearer: &str,
    sample: &sew_proto::voice::CallStatsSample,
) -> crate::stats::BeaconOutcome {
    use crate::stats::BeaconOutcome;
    let url = match route(base, sew_proto::routes::ME_VOICE_STATS) {
        Ok(u) => u,
        Err(e) => {
            return BeaconOutcome::Failed {
                error: format!("{e:?}"),
            };
        }
    };
    // `json` sets Content-Length (never chunked: a K2 door refuses a chunked body).
    let r = match http.post(url).bearer_auth(bearer).json(sample).send().await {
        Ok(r) => r,
        Err(e) => {
            return BeaconOutcome::Failed {
                error: e.without_url().to_string(),
            };
        }
    };
    let http_status = r.status().as_u16();
    if r.status().is_success() {
        return BeaconOutcome::Accepted { http_status };
    }
    let body = r.bytes().await.unwrap_or_default();
    let (code, reason) = error_of(&body);
    BeaconOutcome::Refused {
        http_status,
        code,
        reason,
    }
}

/// Sets self mute and deafen.
pub(crate) async fn self_state(
    http: &reqwest::Client,
    base: &url::Url,
    bearer: &str,
    muted: bool,
    deafened: bool,
) -> Result<(), ApiError> {
    post(
        http,
        base,
        "/api/v1/me/voice/state",
        bearer,
        json!({"self_mute": muted, "self_deaf": deafened}),
    )
    .await
    .map(|_| ())
}

/// Why Sew refused a video lease (`POST /api/v1/me/voice/state` with `self_video` or
/// `self_stream`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LeaseRefused {
    /// The HTTP status (0: the door could not be reached).
    pub(crate) status: u16,
    /// `error.code`.
    pub(crate) code: Option<String>,
    /// `error.details.reason`.
    pub(crate) reason: Option<String>,
    /// `Retry-After`, seconds.
    pub(crate) retry_after: Option<u64>,
    /// Transport failure text (status 0).
    pub(crate) detail: String,
}

/// Takes or releases a video lease (`self_video` for a camera, `self_stream` for a screen
/// share; `None` leaves that one), in the call credential's scope.
pub(crate) async fn video_state(
    http: &reqwest::Client,
    base: &url::Url,
    bearer: &str,
    self_video: Option<bool>,
    self_stream: Option<bool>,
) -> Result<(), LeaseRefused> {
    let io = |detail: String| LeaseRefused {
        status: 0,
        code: None,
        reason: None,
        retry_after: None,
        detail,
    };
    let url = route(base, "/api/v1/me/voice/state").map_err(|e| io(format!("{e:?}")))?;
    let mut body = serde_json::Map::new();
    if let Some(v) = self_video {
        body.insert("self_video".to_owned(), v.into());
    }
    if let Some(v) = self_stream {
        body.insert("self_stream".to_owned(), v.into());
    }
    let r = http
        .post(url)
        .bearer_auth(bearer)
        .json(&serde_json::Value::Object(body))
        .send()
        .await
        .map_err(|e| io(e.without_url().to_string()))?;
    if r.status().is_success() {
        return Ok(());
    }
    let status = r.status().as_u16();
    let retry_after = r
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let body = r.bytes().await.unwrap_or_default();
    let (code, reason) = error_of(&body);
    Err(LeaseRefused {
        status,
        code,
        reason,
        retry_after,
        detail: String::new(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_reason_is_read_from_the_error_details() {
        let body = br#"{"ok":false,"error":{"code":"unauthorized","hint":"x","details":{"reason":"signed_out"}}}"#;
        assert_eq!(
            error_of(body),
            (
                Some("unauthorized".to_owned()),
                Some("signed_out".to_owned())
            )
        );
        assert_eq!(error_of(b"not json"), (None, None));
        assert_eq!(
            error_of(br#"{"ok":false,"error":{"code":"rate_limited","hint":"x"}}"#),
            (Some("rate_limited".to_owned()), None)
        );
    }

    #[test]
    fn routes_join_under_the_door() {
        let b = url::Url::parse("https://chat.example.com/_k2/voice/sew").unwrap();
        assert_eq!(
            route(&b, "/api/v1/me/voice/credential").unwrap().as_str(),
            "https://chat.example.com/_k2/voice/sew/api/v1/me/voice/credential"
        );
        let b = url::Url::parse("https://sew.example/").unwrap();
        assert_eq!(
            route(&b, "/api/v1/me/voice/leave").unwrap().as_str(),
            "https://sew.example/api/v1/me/voice/leave"
        );
        let plain = url::Url::parse("http://sew.example/").unwrap();
        assert!(route(&plain, "/x").is_err(), "plain http only to loopback");
        let local = url::Url::parse("http://127.0.0.1:8470/").unwrap();
        assert!(route(&local, "/x").is_ok());
    }
}
