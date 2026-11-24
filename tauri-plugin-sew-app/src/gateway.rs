//! The API transport's socket half: the App's gateway through the native lane, held in Rust
//! and relayed to the page.
//!
//! [`AppService::gateway_open`] asks the lane for a ticket (`POST /sew/native/ticket`, with
//! the device bearer), opens the socket it names (`wss://<app>/sew/native/api/v1/gateway`, the
//! bearer as `Authorization`) and relays every text frame to the page as a
//! [`GatewayEvent`]. The page's IDENTIFY, RESUME and REAUTH go out through
//! [`AppService::gateway_send`], which puts the ticket in: the page sends them with any
//! placeholder in `ticket` (IDENTIFY, RESUME) or `token` (REAUTH), and never sees a ticket or
//! the bearer.
//!
//! The socket is plain: it never reconnects by itself. A drop or a server close is a `close`
//! event with the close code and reason (`1006` when the socket broke without one), and the
//! page's gateway driver decides what comes next, as it does over a browser `WebSocket`.
//! Each account holds at most [`MAX_GATEWAY_SOCKETS`] sockets, the lane's limit per device, so
//! a reconnect can overlap the old socket; a third is refused `rate_limited` before anything
//! is sent.
//!
//! A socket a page opened answers only that page (its window and origin): `gateway_send` and
//! `gateway_close` from any other page find no such socket.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri_plugin_sew_voice::BoxFuture;
use tokio::sync::mpsc;
use url::Url;

use crate::error::{AppError, code};
use crate::lane::{LaneFailure, LaneResponse, Refusal, Secret, route};
use crate::origin::Origin;
use crate::servers::AccountKey;
use crate::service::AppService;

/// Gateway sockets per account at once (the lane's limit per device).
pub const MAX_GATEWAY_SOCKETS: usize = 2;

/// The longest frame a page may send (Sew's client frames are a few KiB).
pub const MAX_PAGE_FRAME: usize = 1 << 20;

/// A frame to the server.
#[derive(Clone, PartialEq, Eq)]
pub enum WsOut {
    /// A text frame.
    Text(String),
    /// A close frame: the code and the reason.
    Close(u16, String),
}

impl fmt::Debug for WsOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // An IDENTIFY carries a ticket.
        match self {
            WsOut::Text(t) => write!(f, "Text({} bytes)", t.len()),
            WsOut::Close(c, r) => f.debug_tuple("Close").field(c).field(r).finish(),
        }
    }
}

/// What came from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsIn {
    /// A text frame.
    Text(String),
    /// The socket ended: the server's close code and reason, or `None` when it broke without
    /// a close frame. Nothing follows.
    Closed {
        /// The close code.
        code: Option<u16>,
        /// The reason.
        reason: String,
    },
}

/// An open socket: frames out, frames in. Dropping `tx` closes the socket.
#[derive(Debug)]
pub struct SocketPipe {
    /// To the server.
    pub tx: mpsc::UnboundedSender<WsOut>,
    /// From the server; ends with [`WsIn::Closed`].
    pub rx: mpsc::UnboundedReceiver<WsIn>,
}

/// Why a socket did not open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// The server answered the upgrade with an HTTP status (the lane's `429` for a third
    /// socket, `401` for an ended session, ...): its `Retry-After` and body.
    Refused {
        /// The status.
        status: u16,
        /// `Retry-After` in milliseconds.
        retry_after_ms: Option<u64>,
        /// The body (the lane's error shape).
        body: Vec<u8>,
    },
    /// It never got an answer (what failed, never a token).
    Unreachable(String),
}

/// Opens a gateway socket: `url` with `Authorization: Bearer <bearer>`, no Origin, no cookie.
pub trait SocketConnector: Send + Sync + 'static {
    /// Opens `url`.
    fn connect<'a>(
        &'a self,
        url: &'a Url,
        bearer: &'a Secret,
    ) -> BoxFuture<'a, Result<SocketPipe, ConnectError>>;
}

/// What the page gets on its channel, in order: `open`, then `message`s, then one `close`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum GatewayEvent {
    /// The socket is open: send IDENTIFY or RESUME after the server's HELLO.
    Open {
        /// The socket's id (`gateway_send`, `gateway_close`).
        id: String,
    },
    /// A text frame from the server, as it came.
    Message {
        /// The frame.
        data: String,
    },
    /// The socket ended. `code` is the server's close code (`4004` authentication failed,
    /// `4011` session revoked, ...), or `1006` when it broke without one (`clean: false`).
    Close {
        /// The close code.
        code: u16,
        /// The reason.
        reason: String,
        /// Whether a close frame came.
        clean: bool,
    },
}

/// Where a socket's events go (the page's `tauri::ipc::Channel` in the Tauri glue).
pub type GatewaySink = Arc<dyn Fn(GatewayEvent) + Send + Sync>;

/// `gateway_open`'s answer: the socket's id (no ticket, no URL with a secret).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayOpened {
    /// The socket's id.
    pub id: String,
}

struct Sock {
    key: AccountKey,
    /// The page that opened it (`None`: the host's Rust).
    owner: Option<String>,
    /// `None` while the socket opens.
    tx: Option<mpsc::UnboundedSender<WsOut>>,
    /// The ticket minted with the socket, for its first IDENTIFY or RESUME.
    ticket: Option<Secret>,
}

/// The open sockets.
#[derive(Default)]
pub(crate) struct GatewayTable {
    socks: Mutex<HashMap<String, Sock>>,
}

impl fmt::Debug for GatewayTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayTable")
            .field("open", &self.lock().len())
            .finish()
    }
}

fn new_id() -> Result<String, AppError> {
    use base64::Engine as _;
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|_| AppError::new("no_random_source"))?;
    Ok(format!(
        "gw_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    ))
}

impl GatewayTable {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Sock>> {
        self.socks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A slot for `key`'s next socket, or `None` at the limit.
    fn reserve(&self, key: &AccountKey, owner: Option<String>) -> Result<Option<String>, AppError> {
        let id = new_id()?;
        let mut s = self.lock();
        if s.values().filter(|x| &x.key == key).count() >= MAX_GATEWAY_SOCKETS {
            return Ok(None);
        }
        s.insert(
            id.clone(),
            Sock {
                key: key.clone(),
                owner,
                tx: None,
                ticket: None,
            },
        );
        Ok(Some(id))
    }

    fn attach(&self, id: &str, tx: mpsc::UnboundedSender<WsOut>, ticket: Secret) -> bool {
        match self.lock().get_mut(id) {
            Some(s) => {
                s.tx = Some(tx);
                s.ticket = Some(ticket);
                true
            }
            None => false,
        }
    }

    fn release(&self, id: &str) {
        self.lock().remove(id);
    }

    /// `key`'s open sockets (opening ones included).
    pub(crate) fn count(&self, key: &AccountKey) -> usize {
        self.lock().values().filter(|x| &x.key == key).count()
    }

    /// Closes every socket of `key` (its session ended): `1000 signed_out`.
    pub(crate) fn close_account(&self, key: &AccountKey) {
        for s in self.lock().values().filter(|x| &x.key == key) {
            if let Some(tx) = &s.tx {
                let _ = tx.send(WsOut::Close(1000, "signed_out".into()));
            }
        }
    }
}

/// Releases a reserved slot unless the socket opened.
struct Slot<'a> {
    table: &'a GatewayTable,
    id: String,
    kept: bool,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        if !self.kept {
            self.table.release(&self.id);
        }
    }
}

/// The gateway URL the lane must name for `origin`: `wss://<host>/sew/native/api/v1/gateway?
/// v=1&encoding=json` (`ws://` for a loopback `http://` server).
pub fn gateway_url(origin: &Origin) -> Result<Url, AppError> {
    let ws = origin
        .as_str()
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    Url::parse(&format!("{ws}{}?{}", route::GATEWAY, route::GATEWAY_QUERY))
        .map_err(|_| AppError::with_detail(code::BAD_ANSWER, "the gateway's URL"))
}

/// Whether `owner` may use `sock`: the host's Rust any; a page only its own.
fn owns(owner: Option<&str>, sock: &Sock) -> bool {
    owner.is_none_or(|w| sock.owner.as_deref() == Some(w))
}

fn limit_error() -> AppError {
    AppError {
        code: code::RATE_LIMITED.to_owned(),
        detail: json!({ "max": MAX_GATEWAY_SOCKETS, "reason": "gateway_sockets" }),
        hint: Some(format!(
            "This device already has {MAX_GATEWAY_SOCKETS} gateway sockets open."
        )),
    }
}

impl AppService {
    /// A fresh ticket for `key`'s gateway (a `401` ends the account's session).
    async fn mint_ticket(&self, key: &AccountKey) -> Result<crate::lane::TicketAnswer, AppError> {
        let origin = key.server.clone();
        self.with_bearer(key, |lane, b| async move { lane.ticket(&origin, &b).await })
            .await
    }

    /// Opens a gateway socket for `key` (the active account when `None`) and relays it to
    /// `sink`: `open` (before this returns), every text frame, then one `close`. Errors: the
    /// lane's code for a refused ticket or upgrade (`rate_limited` with `retry_after_ms`,
    /// `signed_out`, ...), `rate_limited` for a third socket of the account, `unreachable`.
    pub async fn gateway_open(
        &self,
        key: Option<AccountKey>,
        sink: GatewaySink,
    ) -> Result<GatewayOpened, AppError> {
        let key = self.account_or_active(key)?;
        self.gateway_open_as(key, None, sink).await
    }

    /// [`AppService::gateway_open`] for a page: only `owner` may send on or close it.
    pub(crate) async fn gateway_open_as(
        &self,
        key: AccountKey,
        owner: Option<String>,
        sink: GatewaySink,
    ) -> Result<GatewayOpened, AppError> {
        let connector = self.socket_connector().ok_or_else(|| {
            AppError::with_detail(code::UNREACHABLE, "no gateway connector is configured")
        })?;
        let table = self.gateways();
        let id = table.reserve(&key, owner)?.ok_or_else(limit_error)?;
        let mut slot = Slot {
            table,
            id: id.clone(),
            kept: false,
        };
        let t = self.mint_ticket(&key).await?;
        let want = gateway_url(&key.server)?;
        if Url::parse(&t.url).ok().as_ref() != Some(&want) {
            return Err(AppError::with_detail(
                code::BAD_ANSWER,
                "the ticket names another socket than this server's gateway",
            ));
        }
        let bearer = self.bearer(&key).await?;
        let pipe = match connector.connect(&want, &bearer).await {
            Ok(p) => p,
            Err(ConnectError::Unreachable(e)) => {
                return Err(AppError::with_detail(code::UNREACHABLE, e));
            }
            Err(ConnectError::Refused {
                status,
                retry_after_ms,
                body,
            }) => {
                let mut r = Refusal::from_response(&LaneResponse {
                    status,
                    retry_after_ms,
                    body,
                });
                if status == 429 && r.code.is_none() {
                    r.code = Some(code::RATE_LIMITED.to_owned());
                }
                if status == 401 && r.is_signed_out() {
                    let reason = r.signed_out_reason();
                    self.session_ended(&key, &reason).await;
                    return Err(AppError::with_detail(
                        code::SIGNED_OUT,
                        json!({ "reason": reason }),
                    ));
                }
                return Err(LaneFailure::Refused(r).to_error());
            }
        };
        let SocketPipe { tx, mut rx } = pipe;
        if !table.attach(&id, tx, t.ticket) {
            // Signed out meanwhile.
            return Err(AppError::with_detail(
                code::SIGNED_OUT,
                json!({ "reason": "signed_out" }),
            ));
        }
        slot.kept = true;
        drop(slot);
        sink(GatewayEvent::Open { id: id.clone() });
        let svc = self.clone();
        let sock = id.clone();
        tokio::spawn(async move {
            let mut end = (1006u16, String::new(), false);
            while let Some(m) = rx.recv().await {
                match m {
                    WsIn::Text(t) => {
                        // A server frame never carries the device's credential; if one did,
                        // the page doesn't get it.
                        if crate::api::holds(t.as_bytes(), bearer.expose()) {
                            tracing::warn!("sew-app: a gateway frame held the device's credential");
                            continue;
                        }
                        sink(GatewayEvent::Message { data: t });
                    }
                    WsIn::Closed { code, reason } => {
                        end = (code.unwrap_or(1006), reason, code.is_some());
                        break;
                    }
                }
            }
            svc.gateways().release(&sock);
            sink(GatewayEvent::Close {
                code: end.0,
                reason: end.1,
                clean: end.2,
            });
        });
        Ok(GatewayOpened { id })
    }

    /// Sends a page's frame on socket `id`. IDENTIFY (op 2) and RESUME (op 4) get the
    /// socket's ticket in `d.ticket` (a fresh one when it was used), REAUTH (op 11) a fresh
    /// ticket in `d.token`; whatever the page put there is replaced.
    pub async fn gateway_send(&self, id: &str, data: &str) -> Result<(), AppError> {
        self.gateway_send_as(None, id, data).await
    }

    /// [`AppService::gateway_send`] from page `owner` (`None`: the host's Rust, any socket):
    /// another page's socket is `unknown_socket`.
    pub(crate) async fn gateway_send_as(
        &self,
        owner: Option<&str>,
        id: &str,
        data: &str,
    ) -> Result<(), AppError> {
        if data.len() > MAX_PAGE_FRAME {
            return Err(AppError::invalid_args("a frame up to 1 MiB"));
        }
        let (key, tx, held) = {
            let mut s = self.gateways().lock();
            let sock = s
                .get_mut(id)
                .filter(|x| owns(owner, x))
                .ok_or_else(|| AppError::new(code::UNKNOWN_SOCKET))?;
            let tx = sock
                .tx
                .clone()
                .ok_or_else(|| AppError::new(code::UNKNOWN_SOCKET))?;
            (sock.key.clone(), tx, sock.ticket.take())
        };
        let mut frame: Value = serde_json::from_str(data)
            .map_err(|_| AppError::invalid_args("a frame is a JSON object with `op`"))?;
        let op = frame.get("op").and_then(Value::as_u64);
        match op {
            Some(2) | Some(4) => {
                let ticket = match held {
                    Some(t) => t,
                    None => self.mint_ticket(&key).await?.ticket,
                };
                let d = frame
                    .get_mut("d")
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| AppError::invalid_args("IDENTIFY and RESUME carry `d`"))?;
                d.remove("token");
                d.insert("ticket".into(), Value::String(ticket.expose().to_owned()));
            }
            Some(11) => {
                if let Some(t) = held {
                    // Not used by an IDENTIFY: keep it for one.
                    if let Some(s) = self.gateways().lock().get_mut(id) {
                        s.ticket = Some(t);
                    }
                }
                let ticket = self.mint_ticket(&key).await?.ticket;
                let d = frame
                    .get_mut("d")
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| AppError::invalid_args("REAUTH carries `d`"))?;
                d.insert("token".into(), Value::String(ticket.expose().to_owned()));
            }
            Some(_) => {
                if let Some(t) = held
                    && let Some(s) = self.gateways().lock().get_mut(id)
                {
                    s.ticket = Some(t);
                }
            }
            None => return Err(AppError::invalid_args("a frame is a JSON object with `op`")),
        }
        let text = serde_json::to_string(&frame).map_err(|_| AppError::invalid_args("frame"))?;
        tx.send(WsOut::Text(text))
            .map_err(|_| AppError::new(code::UNKNOWN_SOCKET))
    }

    /// Closes socket `id` with `code` (`1000`, or `3000` to `4999`; default `1000`) and
    /// `reason` (up to 123 bytes). Its `close` event follows. `false` when no such socket is
    /// open.
    pub fn gateway_close(
        &self,
        id: &str,
        code: Option<u16>,
        reason: Option<String>,
    ) -> Result<bool, AppError> {
        self.gateway_close_as(None, id, code, reason)
    }

    /// [`AppService::gateway_close`] from page `owner` (`None`: the host's Rust, any socket):
    /// another page's socket is `false`, as if none were open.
    pub(crate) fn gateway_close_as(
        &self,
        owner: Option<&str>,
        id: &str,
        code: Option<u16>,
        reason: Option<String>,
    ) -> Result<bool, AppError> {
        let c = code.unwrap_or(1000);
        if !(c == 1000 || (3000..=4999).contains(&c)) {
            return Err(AppError::invalid_args(
                "a close code is 1000 or 3000 to 4999",
            ));
        }
        let reason = reason.unwrap_or_default();
        if reason.len() > 123 {
            return Err(AppError::invalid_args("a close reason is up to 123 bytes"));
        }
        let s = self.gateways().lock();
        match s
            .get(id)
            .filter(|x| owns(owner, x))
            .and_then(|x| x.tx.as_ref())
        {
            Some(tx) => Ok(tx.send(WsOut::Close(c, reason)).is_ok()),
            None => Ok(false),
        }
    }

    /// The account's open gateway sockets.
    pub fn gateway_count(&self, key: &AccountKey) -> usize {
        self.gateways().count(key)
    }
}
