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

#[derive(Deserialize)]
struct ErrBody {
    #[serde(default)]
    details: Option<serde_json::Value>,
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
    let reason = r
        .json::<ErrBody>()
        .await
        .ok()
        .and_then(|b| b.details)
        .and_then(|d| d.get("reason").and_then(|r| r.as_str()).map(str::to_owned));
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

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
