//! The gateway socket over the network (feature `http`): a WebSocket on rustls with the
//! platform's trust store, the voice engine's TLS setup (as the HTTP transport), the device
//! bearer as `Authorization`, no Origin and no cookie. The socket is plain: it never
//! reconnects; the page's gateway driver does.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tauri_plugin_sew_voice::BoxFuture;
use tauri_plugin_sew_voice::sew_call_client::rustls;
use tauri_plugin_sew_voice::sew_call_client::rustls::crypto::CryptoProvider;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use url::Url;

use crate::gateway::{ConnectError, SocketConnector, SocketPipe, WsIn, WsOut};
use crate::lane::{Secret, TransportError};

/// How long the TCP, TLS and WebSocket handshakes may take together.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest frame either way (Sew's READY can be a few MiB).
pub const MAX_FRAME: usize = 16 << 20;

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// Opens gateway sockets over the network.
#[derive(Clone)]
pub struct WsConnector {
    tls: Arc<rustls::ClientConfig>,
    user_agent: String,
}

impl std::fmt::Debug for WsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsConnector")
            .field("user_agent", &self.user_agent)
            .finish_non_exhaustive()
    }
}

impl WsConnector {
    /// A connector with the platform's trust store, on `crypto_provider` (else the process
    /// default, else the built-in ring provider with feature `builtin-ring-provider`).
    pub fn new(
        user_agent: &str,
        crypto_provider: Option<Arc<CryptoProvider>>,
    ) -> Result<WsConnector, TransportError> {
        let mut cfg = tauri_plugin_sew_voice::sew_call_client::ClientConfig::new(
            "sew-app",
            std::env::temp_dir(),
        );
        if let Some(p) = crypto_provider {
            cfg = cfg.with_crypto_provider(p);
        }
        let tls = cfg.tls().map_err(|e| TransportError(format!("tls: {e}")))?;
        Ok(WsConnector {
            tls,
            user_agent: user_agent.to_owned(),
        })
    }

    async fn dial(&self, url: &Url, bearer: &Secret) -> Result<SocketPipe, ConnectError> {
        let unreachable = |what: &str| ConnectError::Unreachable(what.to_owned());
        let tls = match url.scheme() {
            "wss" => true,
            "ws" => false,
            _ => return Err(unreachable("not a WebSocket URL")),
        };
        let host = url.host_str().ok_or_else(|| unreachable("no host"))?;
        let bare = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let port = url
            .port_or_known_default()
            .ok_or_else(|| unreachable("no port"))?;
        let addr: SocketAddr = tokio::net::lookup_host((bare.as_str(), port))
            .await
            .map_err(|_| unreachable("the host did not resolve"))?
            .next()
            .ok_or_else(|| unreachable("the host did not resolve"))?;
        let tcp = TcpStream::connect(addr)
            .await
            .map_err(|_| unreachable("could not connect"))?;
        let _ = tcp.set_nodelay(true);
        let stream: Box<dyn Io> = if tls {
            let name = rustls::pki_types::ServerName::try_from(bare)
                .map_err(|_| unreachable("the host is not a TLS name"))?;
            let s = tokio_rustls::TlsConnector::from(self.tls.clone())
                .connect(name, tcp)
                .await
                .map_err(|_| unreachable("the TLS handshake failed"))?;
            Box::new(s)
        } else {
            Box::new(tcp)
        };
        let mut req = url
            .as_str()
            .into_client_request()
            .map_err(|_| unreachable("the socket's request"))?;
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", bearer.expose()))
            .map_err(|_| unreachable("the bearer is not a header value"))?;
        auth.set_sensitive(true);
        let h = req.headers_mut();
        h.insert(tungstenite::http::header::AUTHORIZATION, auth);
        if let Ok(ua) = HeaderValue::from_str(&self.user_agent) {
            h.insert(tungstenite::http::header::USER_AGENT, ua);
        }
        let mut cfg = WebSocketConfig::default();
        cfg.max_message_size = Some(MAX_FRAME);
        cfg.max_frame_size = Some(MAX_FRAME);
        let ws = match tokio_tungstenite::client_async_with_config(req, stream, Some(cfg)).await {
            Ok((ws, _)) => ws,
            Err(tungstenite::Error::Http(resp)) => {
                let retry_after_ms = resp
                    .headers()
                    .get(tungstenite::http::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .map(|s| s.saturating_mul(1000));
                return Err(ConnectError::Refused {
                    status: resp.status().as_u16(),
                    retry_after_ms,
                    body: resp.body().clone().unwrap_or_default(),
                });
            }
            Err(_) => return Err(unreachable("the WebSocket handshake failed")),
        };
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsOut>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<WsIn>();
        tokio::spawn(async move {
            let (mut sink, mut stream) = ws.split();
            let mut closing = false;
            let end = loop {
                tokio::select! {
                    out = out_rx.recv(), if !closing => match out {
                        Some(WsOut::Text(t)) => {
                            if sink.send(tungstenite::Message::text(t)).await.is_err() {
                                break WsIn::Closed { code: None, reason: String::new() };
                            }
                        }
                        Some(WsOut::Close(code, reason)) => {
                            closing = true;
                            let f = CloseFrame { code: CloseCode::from(code), reason: reason.into() };
                            let _ = sink.send(tungstenite::Message::Close(Some(f))).await;
                        }
                        None => {
                            closing = true;
                            let f = CloseFrame { code: CloseCode::Normal, reason: "".into() };
                            let _ = sink.send(tungstenite::Message::Close(Some(f))).await;
                        }
                    },
                    msg = stream.next() => match msg {
                        Some(Ok(tungstenite::Message::Text(t))) => {
                            let _ = in_tx.send(WsIn::Text(t.as_str().to_owned()));
                        }
                        Some(Ok(tungstenite::Message::Close(f))) => {
                            break match f {
                                Some(f) => WsIn::Closed {
                                    code: Some(u16::from(f.code)),
                                    reason: f.reason.as_str().to_owned(),
                                },
                                None => WsIn::Closed { code: Some(1005), reason: String::new() },
                            };
                        }
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => {
                            break WsIn::Closed { code: None, reason: String::new() };
                        }
                    },
                }
            };
            let _ = in_tx.send(end);
        });
        Ok(SocketPipe {
            tx: out_tx,
            rx: in_rx,
        })
    }
}

impl SocketConnector for WsConnector {
    fn connect<'a>(
        &'a self,
        url: &'a Url,
        bearer: &'a Secret,
    ) -> BoxFuture<'a, Result<SocketPipe, ConnectError>> {
        Box::pin(async move {
            match tokio::time::timeout(CONNECT_TIMEOUT, self.dial(url, bearer)).await {
                Ok(r) => r,
                Err(_) => Err(ConnectError::Unreachable("the socket timed out".into())),
            }
        })
    }
}
