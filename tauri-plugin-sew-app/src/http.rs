//! The lane over HTTPS (feature `http`): reqwest on rustls with the platform's trust store,
//! the same TLS setup as the voice engine (`sew_call_client::ClientConfig::tls`: the host's
//! rustls crypto provider, else the process default, else the built-in ring one).
//!
//! It sends no cookie, no `Origin` and no `Sec-Fetch-*` header, follows no redirect (a lane
//! answer is never a redirect; following one could carry the bearer elsewhere), and never logs
//! a body, a query or a token.

use std::sync::Arc;
use std::time::Duration;

use tauri_plugin_sew_voice::BoxFuture;
use tauri_plugin_sew_voice::sew_call_client::rustls::crypto::CryptoProvider;

use crate::lane::{
    ByteMethod, ByteRequest, ByteResponse, LaneRequest, LaneResponse, Method, Transport,
    TransportError,
};
use crate::origin::Origin;

/// The largest lane answer read (a device list or a join answer is a few KiB).
pub const MAX_BODY: usize = 1 << 20;

/// The HTTP transport.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    client: reqwest::Client,
}

impl HttpTransport {
    /// A transport with the platform's trust store, on `crypto_provider` (else the process
    /// default, else the built-in ring provider with feature `builtin-ring-provider`).
    /// `user_agent` names the app (`<app>/<version>`).
    pub fn new(
        user_agent: &str,
        crypto_provider: Option<Arc<CryptoProvider>>,
    ) -> Result<HttpTransport, TransportError> {
        let mut cfg = tauri_plugin_sew_voice::sew_call_client::ClientConfig::new(
            "sew-app",
            std::env::temp_dir(),
        );
        if let Some(p) = crypto_provider {
            cfg = cfg.with_crypto_provider(p);
        }
        let tls = cfg.tls().map_err(|e| TransportError(format!("tls: {e}")))?;
        let client = reqwest::Client::builder()
            .use_preconfigured_tls((*tls).clone())
            .user_agent(user_agent)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| TransportError(format!("http client: {}", e.without_url())))?;
        Ok(HttpTransport { client })
    }
}

/// `Retry-After` in seconds, as milliseconds.
pub(crate) fn retry_after_ms(v: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
    v.and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|s| s.saturating_mul(1000))
}

impl Transport for HttpTransport {
    fn send<'a>(
        &'a self,
        origin: &'a Origin,
        req: LaneRequest,
    ) -> BoxFuture<'a, Result<LaneResponse, TransportError>> {
        Box::pin(async move {
            let url = origin
                .join(&req.path)
                .map_err(|e| TransportError(format!("path: {}", e.code)))?;
            let mut b = match req.method {
                Method::Get => self.client.get(url),
                Method::Post => {
                    let body =
                        serde_json::to_vec(&req.body.unwrap_or_else(|| serde_json::json!({})))
                            .map_err(|_| TransportError("encode the body".into()))?;
                    self.client
                        .post(url)
                        .header(reqwest::header::CONTENT_TYPE, "application/json")
                        .body(body)
                }
            };
            b = b.header(reqwest::header::ACCEPT, "application/json");
            if let Some(bearer) = &req.bearer {
                b = b.bearer_auth(bearer.expose());
            }
            let mut resp = b.send().await.map_err(|e| {
                let kind = if e.is_timeout() {
                    "timed out"
                } else if e.is_connect() {
                    "could not connect"
                } else {
                    "request failed"
                };
                TransportError(format!("{}: {kind}", origin.authority()))
            })?;
            let status = resp.status().as_u16();
            let retry_after_ms = retry_after_ms(resp.headers().get(reqwest::header::RETRY_AFTER));
            let mut body = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(|_| {
                TransportError(format!("{}: the answer broke off", origin.authority()))
            })? {
                if body.len() + chunk.len() > MAX_BODY {
                    return Err(TransportError(format!(
                        "{}: the answer is too large",
                        origin.authority()
                    )));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(LaneResponse {
                status,
                retry_after_ms,
                body,
            })
        })
    }

    fn send_bytes<'a>(
        &'a self,
        origin: &'a Origin,
        req: ByteRequest,
    ) -> BoxFuture<'a, Result<ByteResponse, TransportError>> {
        Box::pin(async move {
            let url = origin
                .join(&req.path)
                .map_err(|e| TransportError(format!("path: {}", e.code)))?;
            let mut b = match req.method {
                ByteMethod::Get => self.client.get(url),
                ByteMethod::Head => self.client.head(url),
                ByteMethod::Post => self.client.post(url),
            };
            for (k, v) in &req.headers {
                let k = k.to_ascii_lowercase();
                // The caller's headers never name the credential, a cookie or an origin.
                if matches!(
                    k.as_str(),
                    "authorization" | "cookie" | "origin" | "host" | "content-length"
                ) || k.starts_with("sec-")
                {
                    continue;
                }
                let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                    .map_err(|_| TransportError("a header name".into()))?;
                let value = reqwest::header::HeaderValue::from_str(v)
                    .map_err(|_| TransportError("a header value".into()))?;
                b = b.header(name, value);
            }
            if let Some(bearer) = &req.bearer {
                b = b.bearer_auth(bearer.expose());
            }
            if let Some(body) = req.body {
                b = b.body(body);
            }
            let mut resp = b.timeout(req.timeout).send().await.map_err(|e| {
                let kind = if e.is_timeout() {
                    "timed out"
                } else if e.is_connect() {
                    "could not connect"
                } else {
                    "request failed"
                };
                TransportError(format!("{}: {kind}", origin.authority()))
            })?;
            let status = resp.status().as_u16();
            let headers = resp
                .headers()
                .iter()
                .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
                .collect();
            let mut body = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(|_| {
                TransportError(format!("{}: the answer broke off", origin.authority()))
            })? {
                if body.len() + chunk.len() > req.max_answer {
                    return Err(TransportError(format!(
                        "{}: the answer is too large",
                        origin.authority()
                    )));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(ByteResponse {
                status,
                headers,
                body,
            })
        })
    }
}
