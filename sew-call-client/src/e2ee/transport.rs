//! The key routes over HTTP: one [`KeyTransport`] to Sew's client API through the host's door
//! (`JoinAnswer.api_base`) with the call credential as the bearer.
//!
//! The routes: device enrolment, certificate renewal, the member's own devices, CRL pages,
//! KeyPackage upload, the key join, the call key routes (`state` to `report`), and the
//! credential renewal.
//! Transport failures, `503` and short `429`s are retried with backoff and **unchanged** bodies
//! (every key submission is idempotent on the server); what comes back is final.
//!
//! Redirects are never followed (the bearer goes to the door only), nothing logs a body, and the
//! bearer is read from [`SharedBearer`] per request so a renewal takes effect at once.

use std::fmt;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sew_e2ee::{ApiFailure, KeyReply, KeyRequest, PreparedKp};
use sew_proto::e2ee::{
    B64, CallKeysInit, CrlPage, DeviceCertificateResponse, DeviceEnrollRequest, DevicePage,
    KeyCommitAccepted, KeyJoinRequest, KeyPackageUploadRequest, KeyPackageUploadResponse,
    KeyStateResponse,
};
use sew_proto::id::Id;
use sew_proto::routes as r;
use url::Url;

use super::credential::{Bearer, ME_VOICE_CREDENTIAL, RenewAnswer, SharedBearer};

/// Attempts per request (the first plus five retries), as sew-e2ee's driver.
pub const ATTEMPTS: u32 = 6;
/// Longest single wait between attempts.
pub const MAX_BACKOFF_MS: u64 = 2_000;
/// A `429` is retried only when it asks for at most this long.
pub const MAX_RATE_LIMIT_WAIT_MS: u64 = 5_000;
/// Per-request timeout.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Why a transport could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TransportError {
    /// The door is not an `http(s)` base URL.
    #[error("api_base must be an http(s) URL")]
    BadBase,
    /// The HTTP client could not be built.
    #[error("http client: {0}")]
    Client(String),
}

/// The key traffic of one call.
#[derive(Clone)]
pub struct KeyTransport {
    http: reqwest::Client,
    base: Url,
    bearer: SharedBearer,
}

impl fmt::Debug for KeyTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyTransport")
            .field("host", &self.base.host_str())
            .field("bearer", &self.bearer)
            .finish()
    }
}

fn failure_from(status: u16, retry_after_s: Option<u64>, body: &[u8]) -> ApiFailure {
    let details = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("error")?.get("details").cloned());
    let s = |k: &str| {
        details
            .as_ref()
            .and_then(|d| d.get(k))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let retry_after_ms = details
        .as_ref()
        .and_then(|d| d.get("retry_after_ms"))
        .and_then(Value::as_u64)
        .or_else(|| retry_after_s.map(|s| s.saturating_mul(1000)));
    ApiFailure {
        status,
        reason: s("reason"),
        detail: s("check").or_else(|| s("unsupported")),
        retry_after_ms,
    }
}

fn retryable(f: &ApiFailure) -> bool {
    f.is_retryable()
        || (f.status == 429
            && f.retry_after_ms
                .is_some_and(|ms| ms <= MAX_RATE_LIMIT_WAIT_MS))
}

impl KeyTransport {
    /// A transport to `api_base` (the door; may carry a path prefix such as
    /// `https://chat.example.com/_k2/voice/sew/`).
    /// `tls` is the engine's trust (the platform's; `config::tls_config`).
    pub fn new(
        api_base: &Url,
        bearer: SharedBearer,
        tls: &std::sync::Arc<rustls::ClientConfig>,
    ) -> Result<KeyTransport, TransportError> {
        if !matches!(api_base.scheme(), "http" | "https") || api_base.cannot_be_a_base() {
            return Err(TransportError::BadBase);
        }
        let http = reqwest::Client::builder()
            .use_preconfigured_tls((**tls).clone())
            .user_agent(crate::USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| TransportError::Client(e.to_string()))?;
        Ok(KeyTransport {
            http,
            base: api_base.clone(),
            bearer,
        })
    }

    /// The door.
    pub fn base(&self) -> &Url {
        &self.base
    }

    /// The live bearer.
    pub fn bearer(&self) -> &SharedBearer {
        &self.bearer
    }

    /// `base` + `path` (the base's own path kept as a prefix).
    pub fn url(&self, path_and_query: &str) -> Url {
        join(&self.base, path_and_query)
    }

    async fn once(
        &self,
        post: bool,
        path: &str,
        body: Option<&Value>,
    ) -> Result<bytes_or_empty::Body, ApiFailure> {
        let url = self.url(path);
        let token = self.bearer.get();
        let mut req = if post {
            self.http.post(url)
        } else {
            self.http.get(url)
        };
        req = req.bearer_auth(token.expose());
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.map_err(|_| ApiFailure::transport())?;
        let status = resp.status().as_u16();
        let retry_after_s = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let bytes = resp.bytes().await.map_err(|_| ApiFailure::transport())?;
        if (200..300).contains(&status) {
            Ok(bytes_or_empty::Body(bytes.to_vec()))
        } else {
            Err(failure_from(status, retry_after_s, &bytes))
        }
    }

    async fn call(
        &self,
        post: bool,
        path: &str,
        body: Option<&Value>,
    ) -> Result<bytes_or_empty::Body, ApiFailure> {
        let mut backoff = 100u64;
        let mut last = ApiFailure::transport();
        for attempt in 0..ATTEMPTS {
            match self.once(post, path, body).await {
                Ok(b) => return Ok(b),
                Err(f) if retryable(&f) && attempt + 1 < ATTEMPTS => {
                    let wait = f.retry_after_ms.unwrap_or(backoff).min(if f.status == 429 {
                        MAX_RATE_LIMIT_WAIT_MS
                    } else {
                        MAX_BACKOFF_MS
                    });
                    tracing::debug!(target: "sew_call_client::keys", status = f.status, attempt, "key route retried");
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF_MS);
                    last = f;
                }
                Err(f) => return Err(f),
            }
        }
        Err(last)
    }

    /// POST `body` to `path` and decode the answer.
    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ApiFailure> {
        let v = serde_json::to_value(body).map_err(|_| ApiFailure::new(0, "encode"))?;
        self.call(true, path, Some(&v)).await?.json()
    }

    /// GET `path` and decode the answer.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ApiFailure> {
        self.call(false, path, None).await?.json()
    }

    /// The call key routes: what a `CallSession` asked for (`Output::Send`).
    pub async fn key_request(&self, req: &KeyRequest) -> Result<KeyReply, ApiFailure> {
        let body = req.body();
        let b = self.call(true, &req.path(), Some(&body)).await?;
        Ok(match req {
            KeyRequest::State(..) => KeyReply::State(b.json::<KeyStateResponse>()?),
            KeyRequest::Commit(..) => KeyReply::CommitAccepted(b.json::<KeyCommitAccepted>()?),
            _ => KeyReply::Accepted,
        })
    }

    /// Uploads one prepared package; the answered ref must be the package's own `hash_ref`.
    pub async fn upload_key_package(&self, kp: &mut PreparedKp) -> Result<(), ApiFailure> {
        let req = KeyPackageUploadRequest {
            key_packages: vec![B64::new(kp.key_package_bytes().to_vec())],
        };
        let resp: KeyPackageUploadResponse = self.post(r::ME_VOICE_KEY_PACKAGES, &req).await?;
        let item = resp
            .items
            .first()
            .ok_or_else(|| ApiFailure::new(422, "no_ref"))?;
        kp.set_ref(item)
            .map_err(|_| ApiFailure::new(422, "ref_mismatch"))
    }

    /// The (deferred) key join with the credential's `join_id`.
    pub async fn key_join(&self, req: &KeyJoinRequest) -> Result<CallKeysInit, ApiFailure> {
        self.post(r::ME_VOICE_KEYS_JOIN, req).await
    }

    /// Enrols a device.
    pub async fn enroll(
        &self,
        req: &DeviceEnrollRequest,
    ) -> Result<DeviceCertificateResponse, ApiFailure> {
        self.post(r::DEVICES, req).await
    }

    /// Renews a device's certificate.
    pub async fn renew_device(&self, device: Id) -> Result<DeviceCertificateResponse, ApiFailure> {
        self.post(&r::with_id(r::DEVICE_RENEW, device), &serde_json::json!({}))
            .await
    }

    /// The member's devices.
    pub async fn my_devices(&self) -> Result<DevicePage, ApiFailure> {
        self.get(r::ME_DEVICES).await
    }

    /// One page of revoked serials after `after`.
    pub async fn crl_page(&self, after: Option<Id>) -> Result<CrlPage, ApiFailure> {
        let path = match after {
            Some(a) => format!("{}?after={a}", r::DEVICES_CRL),
            None => r::DEVICES_CRL.to_owned(),
        };
        self.get(&path).await
    }

    /// Renews the call credential; on success every later request (and the socket's
    /// REAUTH) uses the new token. The old secret stays valid for 30 s on the server.
    pub async fn renew_credential(&self) -> Result<RenewAnswer, ApiFailure> {
        let a: RenewAnswer = self
            .post(ME_VOICE_CREDENTIAL, &serde_json::json!({}))
            .await?;
        self.bearer.set(Bearer::new(a.token.clone()));
        Ok(a)
    }
}

/// `base` + `path_and_query`, keeping the base's path as a prefix.
pub fn join(base: &Url, path_and_query: &str) -> Url {
    let mut u = base.clone();
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };
    let full = format!("{}{}", base.path().trim_end_matches('/'), path);
    u.set_path(&full);
    u.set_query(query);
    u
}

mod bytes_or_empty {
    use serde::de::DeserializeOwned;
    use sew_e2ee::ApiFailure;

    /// A 2xx body.
    pub(super) struct Body(pub(super) Vec<u8>);

    impl Body {
        pub(super) fn json<T: DeserializeOwned>(&self) -> Result<T, ApiFailure> {
            serde_json::from_slice(&self.0).map_err(|_| ApiFailure::new(502, "bad_answer"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_keeps_the_door_prefix() {
        let b = Url::parse("https://chat.example.com/_k2/voice/sew/").unwrap();
        assert_eq!(
            join(&b, "/api/v1/devices/crl?after=5").as_str(),
            "https://chat.example.com/_k2/voice/sew/api/v1/devices/crl?after=5"
        );
        let b = Url::parse("http://127.0.0.1:8470").unwrap();
        assert_eq!(
            join(&b, "/api/v1/me/devices").as_str(),
            "http://127.0.0.1:8470/api/v1/me/devices"
        );
    }

    #[test]
    fn failure_reads_the_envelope() {
        let body = br#"{"ok":false,"error":{"code":"invalid","hint":"x","details":{"reason":"device_revoked","check":"device_cert","retry_after_ms":250}}}"#;
        let f = failure_from(422, None, body);
        assert_eq!(f.status, 422);
        assert!(f.is("device_revoked"));
        assert_eq!(f.detail.as_deref(), Some("device_cert"));
        assert_eq!(f.retry_after_ms, Some(250));
        let f = failure_from(503, Some(1), b"not json");
        assert!(f.reason.is_none());
        assert_eq!(f.retry_after_ms, Some(1000));
        assert!(retryable(&f));
        assert!(!retryable(&failure_from(429, None, b"")));
        assert!(retryable(&failure_from(429, Some(1), b"")));
        assert!(!retryable(&failure_from(429, Some(60), b"")));
    }

    #[test]
    fn a_bad_door_is_refused() {
        let b = SharedBearer::new(Bearer::new("sewcc_x.y"));
        // ring passed in, so this runs without the built-in provider too (`--no-default-features`).
        let cfg = crate::ClientConfig::new("t", std::env::temp_dir())
            .with_crypto_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()));
        let tls = crate::config::tls_config(&cfg).unwrap();
        assert_eq!(
            KeyTransport::new(&Url::parse("ftp://x/").unwrap(), b, &tls).err(),
            Some(TransportError::BadBase)
        );
    }
}
