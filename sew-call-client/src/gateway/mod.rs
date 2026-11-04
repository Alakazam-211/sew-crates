//! The call socket: one gateway connection opened with the call credential.
//!
//! - **Connect** `GET <door>/api/v1/gateway?v=1&encoding=json` with `Authorization: Bearer
//!   sewcc_…` on the upgrade (K2's lane needs it), then IDENTIFY with the same token.
//! - **READY** of a call socket is `{v, session_id, user: {id}}`; a full READY (a `sews_` session
//!   in tests) is read the same way. `session_id` is the join's `e2ee.gateway_session`.
//! - **Delivered** (and the only dispatches passed on, whatever the server sends):
//!   `call_keys_delivered`, `voice_server_updated`, `voice_state_updated`. Everything else is
//!   dropped and counted (defence in depth; the server filters too).
//! - **Ops sent**: HEARTBEAT, IDENTIFY once, RESUME after a drop, REAUTH with the renewed token
//!   of the same credential (the `session_id` stays). Nothing else.
//! - **Closes**: `4011` (credential revoked: the call is over for this engine; since Sew 0.7.2 its
//!   text names why, `sew_proto::voice::call_close`, [`SocketClosed::Ended`]), `4004`
//!   (authentication failed), `4001`/`4002`/`4012` are final. `op 7` or `4007`/`4009` start a new
//!   session (a fresh IDENTIFY; the key session re-binds with the `state` route). Anything else
//!   reconnects with backoff and RESUMEs.

use std::fmt;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde::Deserialize;
use serde_json::Value;
use sew_proto::e2ee::{CallKeysMessage, ReadyCallKeys};
use sew_proto::gateway::client::{
    ClientInfo, ClientOp, GatewayAuth, IdentifyRequest, ReauthRequest, ResumeRequest,
};
use sew_proto::gateway::{Frame, Opcode};
use sew_proto::id::Id;
use sew_proto::voice::{VoiceServerUpdated, VoiceStateUpdated, call_close};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use url::Url;

use crate::e2ee::credential::SharedBearer;

/// The dispatch types a call socket passes on.
pub const CALL_SOCKET_EVENTS: &[&str] = &[
    "call_keys_delivered",
    "voice_server_updated",
    "voice_state_updated",
];

/// Connect and handshake timeout.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// First reconnect wait; doubles to [`RECONNECT_MAX`], plus up to 25% jitter.
pub const RECONNECT_BASE: Duration = Duration::from_millis(250);
/// Longest reconnect wait.
pub const RECONNECT_MAX: Duration = Duration::from_secs(5);
/// After `4008` (rate limited): at least this long (Sew's rule for clients).
pub const RATE_LIMITED_MIN: Duration = Duration::from_secs(5);

/// What the socket tells its owner.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SocketEvent {
    /// A new gateway session (the first IDENTIFY, or after the server refused a RESUME).
    Ready(Box<CallReadyInfo>),
    /// The session resumed: missed dispatches follow.
    Resumed,
    /// `call_keys_delivered`.
    CallKeys(Box<CallKeysMessage>),
    /// `voice_server_updated` (a move or a token re-issue).
    VoiceServerUpdated(Box<VoiceServerUpdated>),
    /// `voice_state_updated` of the call's channel.
    VoiceStateUpdated(Box<VoiceStateUpdated>),
    /// The socket is gone for good.
    Closed(SocketClosed),
}

/// What a READY carried (the call socket's minimal READY or a full one).
#[derive(Clone)]
#[non_exhaustive]
pub struct CallReadyInfo {
    /// The join's `e2ee.gateway_session`.
    pub session_id: String,
    /// The member (`user.id` or `member.id`).
    pub member_id: Option<Id>,
    /// The install (`install.id` of a full READY; the call socket's READY carries `install_id`).
    pub install_id: Option<Id>,
    /// The trust anchors (`call_keys`), when the READY has them.
    pub call_keys: Option<ReadyCallKeys>,
}

impl fmt::Debug for CallReadyInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallReadyInfo")
            .field("member_id", &self.member_id)
            .field("install_id", &self.install_id)
            .field("call_keys", &self.call_keys.is_some())
            .finish_non_exhaustive()
    }
}

/// Why the socket ended for good.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SocketClosed {
    /// `4011` with a close text that names no reason: the credential was revoked by a Sew
    /// older than 0.7.2, or another socket took this credential over.
    Revoked,
    /// `4011` whose close text names why the credential ended (Sew 0.7.2,
    /// [`sew_proto::voice::call_close`]): `reason` is Sew's voice leave reason as it arrived
    /// (`perms_lost`, `removed_from_space`, `replaced` for a swap, …; `session_revoked` for a
    /// sign-out, `account_removed` for a deactivation).
    Ended {
        /// The reason.
        reason: String,
    },
    /// `4004`: the credential was refused.
    AuthenticationFailed,
    /// A protocol error (`4001`, `4002`, `4012`).
    Protocol(u16),
    /// [`CallSocket::close`].
    Closed,
}

#[derive(Debug)]
enum Cmd {
    Reauth,
    Close,
}

/// Counters (no content).
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct SocketCounters {
    /// Dispatches passed on.
    pub passed: AtomicU64,
    /// Dispatches dropped (not one of the three types).
    pub dropped: AtomicU64,
    /// Reconnects.
    pub reconnects: AtomicU64,
    /// New sessions (IDENTIFYs).
    pub identifies: AtomicU64,
}

/// A running call socket. Dropping it closes the socket.
pub struct CallSocket {
    cmd: mpsc::UnboundedSender<Cmd>,
    counters: Arc<SocketCounters>,
}

impl fmt::Debug for CallSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallSocket")
            .field("counters", &self.counters)
            .finish()
    }
}

impl Drop for CallSocket {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Close);
    }
}

impl CallSocket {
    /// Opens the socket to `api_base` with the live bearer; events arrive on the returned
    /// receiver (the first is `Ready`, or `Closed`).
    pub fn open(
        api_base: &Url,
        bearer: SharedBearer,
        tls: Arc<rustls::ClientConfig>,
    ) -> Result<(CallSocket, mpsc::Receiver<SocketEvent>), SocketError> {
        let url = gateway_url(api_base)?;
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::channel(256);
        let counters = Arc::new(SocketCounters::default());
        let task = Task {
            url,
            bearer,
            tls,
            cmd: cmd_rx,
            events: ev_tx,
            counters: counters.clone(),
            session: None,
            seq: 0,
        };
        tokio::spawn(task.run());
        Ok((
            CallSocket {
                cmd: cmd_tx,
                counters,
            },
            ev_rx,
        ))
    }

    /// Sends REAUTH with the bearer's current token (after a renewal).
    pub fn reauth(&self) {
        let _ = self.cmd.send(Cmd::Reauth);
    }

    /// Closes the socket.
    pub fn close(&self) {
        let _ = self.cmd.send(Cmd::Close);
    }

    /// The counters.
    pub fn counters(&self) -> &SocketCounters {
        &self.counters
    }
}

/// Why a socket could not be opened.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SocketError {
    /// The door is not an `http(s)` URL.
    #[error("api_base must be an http(s) URL")]
    BadBase,
}

/// `ws(s)://<door>/api/v1/gateway?v=1&encoding=json` (no compression).
pub fn gateway_url(api_base: &Url) -> Result<Url, SocketError> {
    let mut u = crate::e2ee::transport::join(api_base, sew_proto::routes::GATEWAY);
    let scheme = match api_base.scheme() {
        "http" => "ws",
        "https" => "wss",
        _ => return Err(SocketError::BadBase),
    };
    u.set_scheme(scheme).map_err(|()| SocketError::BadBase)?;
    u.set_query(Some("v=1&encoding=json"));
    Ok(u)
}

enum Stream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

type Ws = WebSocketStream<Stream>;

async fn connect(
    url: &Url,
    bearer: &SharedBearer,
    tls_cfg: &Arc<rustls::ClientConfig>,
) -> Result<Ws, String> {
    let fut = async {
        let tls = url.scheme() == "wss";
        let host = url.host_str().ok_or("no host")?.to_owned();
        let port = url.port_or_known_default().ok_or("no port")?;
        let bare = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let addr: SocketAddr = tokio::net::lookup_host((bare.as_str(), port))
            .await
            .map_err(|e| format!("resolve: {e}"))?
            .next()
            .ok_or("resolve: no address")?;
        let tcp = TcpStream::connect(addr)
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let _ = tcp.set_nodelay(true);
        let stream = if tls {
            let name = rustls::pki_types::ServerName::try_from(bare.clone())
                .map_err(|e| format!("tls server name: {e}"))?;
            let s = tokio_rustls::TlsConnector::from(tls_cfg.clone())
                .connect(name, tcp)
                .await
                .map_err(|e| format!("tls handshake: {e}"))?;
            Stream::Tls(Box::new(s))
        } else {
            Stream::Plain(tcp)
        };
        let mut req = url
            .as_str()
            .into_client_request()
            .map_err(|e| format!("request: {e}"))?;
        let token = bearer.get();
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
            .map_err(|_| "bearer is not a header value".to_owned())?;
        auth.set_sensitive(true);
        req.headers_mut().insert(http::header::AUTHORIZATION, auth);
        req.headers_mut().insert(
            http::header::USER_AGENT,
            HeaderValue::from_static(crate::USER_AGENT),
        );
        let mut cfg = WebSocketConfig::default();
        cfg.max_message_size = Some(16 << 20);
        cfg.max_frame_size = Some(16 << 20);
        let (ws, _) = tokio_tungstenite::client_async_with_config(req, stream, Some(cfg))
            .await
            .map_err(|e| format!("websocket handshake: {e}"))?;
        Ok(ws)
    };
    match tokio::time::timeout(CONNECT_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => Err("connect timed out".to_owned()),
    }
}

/// The client name the socket identifies with.
pub fn client_info() -> ClientInfo {
    ClientInfo {
        name: "sew-call-client".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

#[derive(Deserialize)]
struct ReadyWire {
    session_id: String,
    #[serde(default)]
    user: Option<IdOnly>,
    #[serde(default)]
    member: Option<IdOnly>,
    #[serde(default)]
    install: Option<IdOnly>,
    /// The call socket's READY (`CallReady.install_id`).
    #[serde(default)]
    install_id: Option<Id>,
    #[serde(default)]
    call_keys: Option<ReadyCallKeys>,
}

#[derive(Deserialize)]
struct IdOnly {
    id: Id,
}

/// Reads a READY body (minimal or full).
pub fn parse_ready(d: &Value) -> Option<CallReadyInfo> {
    let w: ReadyWire = serde_json::from_value(d.clone()).ok()?;
    Some(CallReadyInfo {
        session_id: w.session_id,
        member_id: w.user.or(w.member).map(|m| m.id),
        install_id: w.install_id.or(w.install.map(|i| i.id)),
        call_keys: w.call_keys,
    })
}

/// The event a dispatch becomes, or `None` (dropped).
pub fn filter_dispatch(t: &str, d: &Value) -> Option<SocketEvent> {
    match t {
        "call_keys_delivered" => serde_json::from_value::<CallKeysMessage>(d.clone())
            .ok()
            .map(|m| SocketEvent::CallKeys(Box::new(m))),
        "voice_server_updated" => serde_json::from_value::<VoiceServerUpdated>(d.clone())
            .ok()
            .map(|m| SocketEvent::VoiceServerUpdated(Box::new(m))),
        "voice_state_updated" => serde_json::from_value::<VoiceStateUpdated>(d.clone())
            .ok()
            .map(|m| SocketEvent::VoiceStateUpdated(Box::new(m))),
        _ => None,
    }
}

enum Outcome {
    /// Reconnect after this long and RESUME (or IDENTIFY when there is no session).
    Retry(Duration),
    /// The session is gone: IDENTIFY again after this long.
    NewSession(Duration),
    /// Final.
    Final(SocketClosed),
}

/// What a close frame (`None`: none) means.
fn frame_outcome(f: Option<&CloseFrame>, backoff: Duration) -> Outcome {
    match f {
        Some(f) => close_outcome(Some(u16::from(f.code)), f.reason.as_str(), backoff),
        None => close_outcome(None, "", backoff),
    }
}

fn close_outcome(code: Option<u16>, text: &str, backoff: Duration) -> Outcome {
    match code {
        Some(4011) => Outcome::Final(match call_close::reason_of(text) {
            Some(reason) => SocketClosed::Ended { reason },
            None => SocketClosed::Revoked,
        }),
        Some(4004) => Outcome::Final(SocketClosed::AuthenticationFailed),
        Some(c @ (4001 | 4002 | 4012)) => Outcome::Final(SocketClosed::Protocol(c)),
        Some(4007 | 4009) => Outcome::NewSession(backoff),
        Some(4008) => Outcome::Retry(backoff.max(RATE_LIMITED_MIN)),
        _ => Outcome::Retry(backoff),
    }
}

fn jitter(d: Duration) -> Duration {
    let ms = d.as_millis() as u64;
    Duration::from_millis(ms + rand::random::<u64>() % (ms / 4 + 1))
}

struct Task {
    url: Url,
    bearer: SharedBearer,
    tls: Arc<rustls::ClientConfig>,
    cmd: mpsc::UnboundedReceiver<Cmd>,
    events: mpsc::Sender<SocketEvent>,
    counters: Arc<SocketCounters>,
    session: Option<String>,
    seq: u64,
}

impl Task {
    async fn run(mut self) {
        let mut backoff = RECONNECT_BASE;
        loop {
            let outcome = match connect(&self.url, &self.bearer, &self.tls).await {
                Ok(ws) => {
                    let o = self.serve(ws, &mut backoff).await;
                    self.counters.reconnects.fetch_add(1, Ordering::Relaxed);
                    o
                }
                Err(e) => {
                    tracing::debug!(target: "sew_call_client::socket", error = %e, "call socket connect failed");
                    Outcome::Retry(backoff)
                }
            };
            let wait = match outcome {
                Outcome::Final(why) => {
                    let _ = self.events.send(SocketEvent::Closed(why)).await;
                    return;
                }
                Outcome::Retry(w) => w,
                Outcome::NewSession(w) => {
                    self.session = None;
                    self.seq = 0;
                    w
                }
            };
            backoff = (backoff * 2).min(RECONNECT_MAX);
            tokio::select! {
                _ = tokio::time::sleep(jitter(wait)) => {}
                c = self.cmd.recv() => match c {
                    Some(Cmd::Reauth) => {}
                    Some(Cmd::Close) | None => {
                        let _ = self.events.send(SocketEvent::Closed(SocketClosed::Closed)).await;
                        return;
                    }
                },
            }
        }
    }

    async fn send(ws: &mut Ws, op: &ClientOp) -> Result<(), ()> {
        let frame = op.encode();
        let text = serde_json::to_string(&frame).map_err(|_| ())?;
        ws.send(Message::text(text)).await.map_err(|_| ())
    }

    /// One connection, until it ends.
    async fn serve(&mut self, mut ws: Ws, backoff: &mut Duration) -> Outcome {
        // HELLO first.
        let hello = tokio::time::timeout(CONNECT_TIMEOUT, ws.next()).await;
        let interval = match hello {
            Ok(Some(Ok(Message::Text(t)))) => match serde_json::from_str::<Frame>(t.as_str()) {
                Ok(f) if f.op == Opcode::Hello.as_u8() => {
                    f.d.get("heartbeat_interval_ms")
                        .and_then(Value::as_u64)
                        .unwrap_or(30_000)
                }
                _ => return Outcome::Retry(*backoff),
            },
            Ok(Some(Ok(Message::Close(f)))) => {
                return frame_outcome(f.as_ref(), *backoff);
            }
            _ => return Outcome::Retry(*backoff),
        };
        let token = self.bearer.get().expose().to_owned();
        let first = match &self.session {
            Some(sid) => ClientOp::Resume(ResumeRequest {
                auth: GatewayAuth::Token(token),
                session_id: sid.clone(),
                seq: self.seq,
            }),
            None => ClientOp::Identify(IdentifyRequest {
                auth: GatewayAuth::Token(token),
                client: client_info(),
                capabilities: 0,
            }),
        };
        if Self::send(&mut ws, &first).await.is_err() {
            return Outcome::Retry(*backoff);
        }
        let mut beat = tokio::time::interval(Duration::from_millis(interval.max(1_000)));
        beat.tick().await;
        let mut acked = true;
        loop {
            tokio::select! {
                _ = beat.tick() => {
                    if !acked {
                        // A zombie connection: drop it and resume.
                        let _ = ws.close(None).await;
                        return Outcome::Retry(*backoff);
                    }
                    acked = false;
                    let s = (self.seq > 0).then_some(self.seq);
                    if Self::send(&mut ws, &ClientOp::Heartbeat(s)).await.is_err() {
                        return Outcome::Retry(*backoff);
                    }
                }
                c = self.cmd.recv() => match c {
                    Some(Cmd::Reauth) => {
                        let token = self.bearer.get().expose().to_owned();
                        if Self::send(&mut ws, &ClientOp::Reauth(ReauthRequest { token })).await.is_err() {
                            return Outcome::Retry(*backoff);
                        }
                    }
                    Some(Cmd::Close) | None => {
                        let _ = ws.close(None).await;
                        return Outcome::Final(SocketClosed::Closed);
                    }
                },
                m = ws.next() => {
                    let text = match m {
                        Some(Ok(Message::Text(t))) => t,
                        Some(Ok(Message::Close(f))) => {
                            return frame_outcome(f.as_ref(), *backoff);
                        }
                        Some(Ok(_)) => continue,
                        Some(Err(_)) | None => return Outcome::Retry(*backoff),
                    };
                    let frames: Vec<Frame> = match serde_json::Deserializer::from_str(text.as_str())
                        .into_iter::<Frame>()
                        .collect::<Result<Vec<_>, _>>()
                    {
                        Ok(f) => f,
                        Err(_) => return Outcome::Retry(*backoff),
                    };
                    for f in frames {
                        if f.op == Opcode::HeartbeatAck.as_u8() {
                            acked = true;
                        }
                        if let Some(o) = self.on_frame(f, backoff).await {
                            let _ = ws.close(None).await;
                            return o;
                        }
                    }
                    if self.events.is_closed() {
                        let _ = ws.close(None).await;
                        return Outcome::Final(SocketClosed::Closed);
                    }
                }
            }
        }
    }

    /// Handles one frame; `Some` ends the connection.
    async fn on_frame(&mut self, f: Frame, backoff: &mut Duration) -> Option<Outcome> {
        let op = Opcode::from_u8(f.op)?;
        match op {
            Opcode::HeartbeatAck => None,
            Opcode::Reconnect => {
                let after = f.d.get("after_ms").and_then(Value::as_u64).unwrap_or(0);
                Some(Outcome::Retry(Duration::from_millis(after)))
            }
            Opcode::InvalidSession => {
                let resumable =
                    f.d.get("resumable")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                if resumable {
                    Some(Outcome::Retry(*backoff))
                } else {
                    Some(Outcome::NewSession(*backoff))
                }
            }
            Opcode::Dispatch => {
                if let Some(s) = f.s {
                    self.seq = self.seq.max(s);
                }
                let t = f.t.as_deref()?;
                match t {
                    "ready" => {
                        let Some(info) = parse_ready(&f.d) else {
                            return Some(Outcome::NewSession(*backoff));
                        };
                        self.session = Some(info.session_id.clone());
                        self.counters.identifies.fetch_add(1, Ordering::Relaxed);
                        *backoff = RECONNECT_BASE;
                        let _ = self.events.send(SocketEvent::Ready(Box::new(info))).await;
                        None
                    }
                    "resumed" => {
                        *backoff = RECONNECT_BASE;
                        let _ = self.events.send(SocketEvent::Resumed).await;
                        None
                    }
                    _ => {
                        match filter_dispatch(t, &f.d) {
                            Some(ev) => {
                                self.counters.passed.fetch_add(1, Ordering::Relaxed);
                                let _ = self.events.send(ev).await;
                            }
                            None => {
                                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        None
                    }
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_url_follows_the_door() {
        let u = gateway_url(&Url::parse("https://example.com/_k2/voice/sew/").unwrap()).unwrap();
        assert_eq!(
            u.as_str(),
            "wss://example.com/_k2/voice/sew/api/v1/gateway?v=1&encoding=json"
        );
        let u = gateway_url(&Url::parse("http://127.0.0.1:8470").unwrap()).unwrap();
        assert_eq!(
            u.as_str(),
            "ws://127.0.0.1:8470/api/v1/gateway?v=1&encoding=json"
        );
    }

    #[test]
    fn only_three_dispatch_types_pass() {
        let msg = serde_json::json!({"id": "1", "channel_id": "2"});
        for t in [
            "message_created",
            "presence_updated",
            "ready",
            "typing_started",
        ] {
            assert!(filter_dispatch(t, &msg).is_none(), "{t} must be dropped");
        }
        let vs = serde_json::json!({"member_id": "7"});
        assert!(matches!(
            filter_dispatch("voice_state_updated", &vs),
            Some(SocketEvent::VoiceStateUpdated(_))
        ));
    }

    #[test]
    fn minimal_ready_is_read() {
        let d = serde_json::json!({"v": 1, "session_id": "gw_1", "user": {"id": "42"}});
        let r = parse_ready(&d).unwrap();
        assert_eq!(r.session_id, "gw_1");
        assert_eq!(r.member_id, Id::new(42));
        assert!(r.call_keys.is_none());
        assert!(r.install_id.is_none());
    }

    #[test]
    fn call_ready_install_id_is_read() {
        // The call socket's READY carries `install_id` (and `call_keys`) at the top.
        let d = serde_json::json!({
            "v": 1, "session_id": "gw_2", "user": {"id": "42"}, "install_id": "9001"
        });
        let r = parse_ready(&d).unwrap();
        assert_eq!(r.install_id, Id::new(9001));
        assert_eq!(r.member_id, Id::new(42));
    }

    #[test]
    fn closes_map_to_outcomes() {
        let b = Duration::from_millis(100);
        // Sew 0.7.1's text (and a takeover's) names no reason.
        assert!(matches!(
            close_outcome(Some(4011), "session_revoked", b),
            Outcome::Final(SocketClosed::Revoked)
        ));
        assert!(matches!(
            close_outcome(Some(4004), "", b),
            Outcome::Final(SocketClosed::AuthenticationFailed)
        ));
        assert!(matches!(
            close_outcome(Some(4009), "", b),
            Outcome::NewSession(_)
        ));
        assert!(
            matches!(close_outcome(Some(4008), "", b), Outcome::Retry(d) if d >= RATE_LIMITED_MIN)
        );
        assert!(matches!(close_outcome(Some(1006), "", b), Outcome::Retry(d) if d == b));
        assert!(matches!(frame_outcome(None, b), Outcome::Retry(d) if d == b));
    }

    #[test]
    fn a_4011_keeps_the_reason_sew_gave() {
        // Sew 0.7.2: the close text of a call socket names why its credential ended.
        let b = Duration::from_millis(100);
        for (text, want) in [
            ("call_ended:perms_lost", "perms_lost"),
            ("call_ended:removed_from_space", "removed_from_space"),
            ("call_ended:timed_out", "timed_out"),
            ("call_ended:channel_archived", "channel_archived"),
            ("call_ended:removed_by_moderator", "removed_by_moderator"),
            ("call_ended:device_revoked", "device_revoked"),
            ("call_ended:session_revoked", "session_revoked"),
            ("call_ended:replaced", "replaced"),
            ("signed_out", "session_revoked"),
            ("deactivated", "account_removed"),
        ] {
            match close_outcome(Some(4011), text, b) {
                Outcome::Final(SocketClosed::Ended { reason }) => assert_eq!(reason, want),
                _ => panic!("{text}: not an ended close"),
            }
        }
        let f = CloseFrame {
            code: 4011.into(),
            reason: "call_ended:perms_lost".into(),
        };
        assert!(matches!(
            frame_outcome(Some(&f), b),
            Outcome::Final(SocketClosed::Ended { reason }) if reason == "perms_lost"
        ));
        // Another code with the same text is not an end.
        assert!(matches!(
            close_outcome(Some(4004), "call_ended:perms_lost", b),
            Outcome::Final(SocketClosed::AuthenticationFailed)
        ));
    }
}
