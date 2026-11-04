//! The signalling WebSocket to Sew's media server (`wss://voice.<host>/rtc/v1`), with the media
//! token as `Authorization: Bearer` (never in the URL), over the engine's TLS trust. Plain
//! `ws://` only to a loopback host.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use sew_lk_compat::JoinParams;
use sew_lkproto::livekit::{SignalRequest, SignalResponse};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message as WsMessage};

use crate::config::{ClientConfig, is_loopback_host};

/// One WebSocket handshake attempt.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The largest signalling message accepted (as the engine's own cap).
const MAX_MESSAGE: usize = 1 << 20;

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// What a link delivers.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum SignalIn {
    /// A response.
    Response(SignalResponse),
    /// The socket ended.
    Closed(String),
}

/// Why a connect failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConnectError {
    /// The engine answered the handshake with an HTTP error (401: the token is not good).
    Refused {
        /// The status.
        status: u16,
    },
    /// A transport error (refused, reset, timeout, TLS).
    Io(String),
    /// The URL is not one the engine may use.
    BadUrl(String),
}

/// One connected signalling socket.
pub(crate) struct SignalLink {
    wtx: Option<mpsc::UnboundedSender<SignalRequest>>,
    reader: JoinHandle<()>,
    writer: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for SignalLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalLink").finish_non_exhaustive()
    }
}

impl Drop for SignalLink {
    fn drop(&mut self) {
        self.reader.abort();
        if let Some(w) = &self.writer {
            w.abort();
        }
    }
}

impl SignalLink {
    /// Queues a request.
    pub(crate) fn send(&self, r: SignalRequest) {
        if let Some(w) = &self.wtx {
            let _ = w.send(r);
        }
    }

    /// Flushes and closes the socket (after a leave), giving the writer a moment.
    pub(crate) async fn close(mut self) {
        self.wtx = None;
        if let Some(w) = self.writer.take() {
            let _ = tokio::time::timeout(Duration::from_secs(1), w).await;
        }
    }
}

/// Connects to `base` (the join answer's `url`) with `params`; responses go to `tx` tagged
/// with `link`.
pub(crate) async fn connect(
    cfg: &ClientConfig,
    tls: &Arc<rustls::ClientConfig>,
    base: &str,
    params: &JoinParams,
    link: u64,
    tx: mpsc::UnboundedSender<(u64, SignalIn)>,
) -> Result<SignalLink, ConnectError> {
    let url = params.url(base);
    let parsed = url::Url::parse(&url).map_err(|e| ConnectError::BadUrl(e.to_string()))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| ConnectError::BadUrl("no host".to_owned()))?
        .to_owned();
    let secure = match parsed.scheme() {
        "wss" => true,
        "ws" if is_loopback_host(&host) => false,
        s => return Err(ConnectError::BadUrl(format!("scheme {s}"))),
    };
    let port = parsed
        .port_or_known_default()
        .unwrap_or(if secure { 443 } else { 80 });
    let addr = crate::config::resolve(cfg, &host, port)
        .await
        .map_err(|e| ConnectError::Io(e.to_string()))?;
    let mut req = url
        .as_str()
        .into_client_request()
        .map_err(|e| ConnectError::BadUrl(e.to_string()))?;
    let auth = HeaderValue::from_str(&format!("Bearer {}", params.token))
        .map_err(|e| ConnectError::BadUrl(e.to_string()))?;
    req.headers_mut().insert("Authorization", auth);
    let fut = async {
        let tcp = TcpStream::connect(addr)
            .await
            .map_err(|e| ConnectError::Io(e.to_string()))?;
        let _ = tcp.set_nodelay(true);
        let io: Box<dyn Io> = if secure {
            let name = rustls::pki_types::ServerName::try_from(host.clone())
                .map_err(|e| ConnectError::BadUrl(e.to_string()))?;
            let s = tokio_rustls::TlsConnector::from(tls.clone())
                .connect(name, tcp)
                .await
                .map_err(|e| ConnectError::Io(format!("TLS: {e}")))?;
            Box::new(s)
        } else {
            Box::new(tcp)
        };
        let mut wcfg = tungstenite::protocol::WebSocketConfig::default();
        wcfg.max_message_size = Some(MAX_MESSAGE);
        tokio_tungstenite::client_async_with_config(req, io, Some(wcfg))
            .await
            .map_err(|e| match e {
                tungstenite::Error::Http(r) => ConnectError::Refused {
                    status: r.status().as_u16(),
                },
                other => ConnectError::Io(other.to_string()),
            })
    };
    let (ws, _resp) = tokio::time::timeout(CONNECT_TIMEOUT, fut)
        .await
        .map_err(|_| ConnectError::Io("signalling connect timed out".to_owned()))??;
    let (mut sink, mut stream) = ws.split();
    let (wtx, mut wrx) = mpsc::unbounded_channel::<SignalRequest>();
    let writer = tokio::spawn(async move {
        while let Some(r) = wrx.recv().await {
            if sink
                .send(WsMessage::Binary(r.encode_to_vec().into()))
                .await
                .is_err()
            {
                return;
            }
        }
        let _ = sink.close().await;
    });
    let reader = tokio::spawn(async move {
        loop {
            let Some(m) = stream.next().await else {
                let _ = tx.send((link, SignalIn::Closed("end of stream".to_owned())));
                return;
            };
            match m {
                Ok(WsMessage::Binary(b)) => match SignalResponse::decode(b.as_ref()) {
                    Ok(r) => {
                        if tx.send((link, SignalIn::Response(r))).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(target: "sew_call_client", error = %e, "bad signal response");
                    }
                },
                Ok(WsMessage::Close(c)) => {
                    let why = c
                        .map(|c| format!("{} {}", u16::from(c.code), c.reason))
                        .unwrap_or_default();
                    let _ = tx.send((link, SignalIn::Closed(format!("closed: {why}"))));
                    return;
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = tx.send((link, SignalIn::Closed(format!("websocket: {e}"))));
                    return;
                }
            }
        }
    });
    Ok(SignalLink {
        wtx: Some(wtx),
        reader,
        writer: Some(writer),
    })
}
