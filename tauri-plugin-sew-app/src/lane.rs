//! The App server's native lane (`/sew/native/*`, lane version 1): its routes, its JSON shapes
//! and a client over a [`Transport`].
//!
//! A native client talks to the App's server, never to Sew: the lane signs the device in,
//! keeps a device session (its bearer is `sewdv_…`), hands that session to the shell's webview,
//! and answers the voice engine's joins. The lane takes no cookie and needs no `Origin`; the
//! routes that answer a secret refuse a request that carries `Origin` or `Sec-Fetch-Site`, and
//! the [`Transport`]s here never send either.
//!
//! The HTTP transport is `http::HttpTransport` (feature `http`); tests and UI work use
//! [`crate::fake::FakeLane`].

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri_plugin_sew_voice::BoxFuture;

use crate::error::{AppError, code};
use crate::origin::Origin;

/// The lane version this plugin is built for (`GET /sew/native/info` → `lane` must be at least
/// this).
pub const LANE_VERSION: u32 = 1;

/// The lane's routes, under the App's origin.
pub mod route {
    /// `GET`: what the lane offers.
    pub const INFO: &str = "/sew/native/info";
    /// `POST`: a password sign-in.
    pub const SIGNIN_PASSWORD: &str = "/sew/native/signin/password";
    /// `POST`: the host's sign-in door, step 1 (offered when `info.signin.door` is true): a
    /// one-time id and a PKCE challenge (the lane keeps the verifier).
    pub const SIGNIN_BEGIN: &str = "/sew/native/signin/door/begin";
    /// `POST`: the host's sign-in door, step 3: the door's code for a device session.
    pub const SIGNIN_FINISH: &str = "/sew/native/signin/door/finish";
    /// `GET`, on the host (not the lane): `{"k2": true}` when the host's sign-in door answers
    /// on this address.
    pub const HOST_DOOR_PING: &str = "/.k2/auth/ping";
    /// `POST`, on the host (not the lane): `{username, password, challenge}` → `{ok, code}`.
    pub const HOST_DOOR_SIGN_IN: &str = "/.k2/auth/sign-in";
    /// `POST`, on the host (not the lane): `{token, password, challenge}` → `{ok, code?}`, the
    /// first password after `password_change_required`.
    pub const HOST_DOOR_SET_PASSWORD: &str = "/.k2/auth/set-password";
    /// `POST`: redeem a pairing code a signed-in page showed.
    pub const PAIR_REDEEM: &str = "/sew/native/pair/redeem";
    /// `GET`: the device session's person, install and session.
    pub const WHOAMI: &str = "/sew/native/whoami";
    /// `GET`: this person's device sessions.
    pub const DEVICES: &str = "/sew/native/devices";
    /// `POST`: end this device's session.
    pub const SIGNOUT: &str = "/sew/native/signout";
    /// `POST`: a 60 s code for the webview's cookie.
    pub const HANDOFF: &str = "/sew/native/handoff";
    /// `POST` (the page, on the cookie lane): trade the code for the cookie.
    pub const HANDOFF_REDEEM: &str = "/sew/native/handoff/redeem";
    /// `POST`: the voice engine's join (the App server's route contract).
    pub const VOICE_JOIN: &str = "/sew/native/voice/join";
    /// `POST`: the person's answer to a recording's notice.
    pub const VOICE_CONSENT: &str = "/sew/native/voice/consent";
    /// `POST`: the person's Record or Stop (`{target, room, action, recording_id?,
    /// device_id?}`; `info.recording` says the lane serves it).
    pub const VOICE_RECORDING: &str = "/sew/native/voice/recording";
    /// `POST`: a single-use gateway ticket and the socket's URL (`{ticket, url,
    /// expires_in_ms}`).
    pub const TICKET: &str = "/sew/native/ticket";
    /// The App's client API under the lane: `GET|POST /sew/native/api/v1/*`, the signed byte
    /// routes and the gateway socket.
    pub const API: &str = "/sew/native/api/v1";
    /// The gateway socket's path (`?v=1&encoding=json` exactly).
    pub const GATEWAY: &str = "/sew/native/api/v1/gateway";
    /// The gateway socket's one query.
    pub const GATEWAY_QUERY: &str = "v=1&encoding=json";

    /// `POST /sew/native/devices/{id}/revoke`.
    pub fn revoke(session_id: &str) -> String {
        format!("/sew/native/devices/{session_id}/revoke")
    }
}

/// A secret string (a device bearer, a handoff, pairing or door code, a password, a door
/// begin's id and challenge): `Debug` never prints it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret.
    pub fn new(s: impl Into<String>) -> Secret {
        Secret(s.into())
    }

    /// The secret itself.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Whether `s` has the device bearer's shape: `sewdv_` and 43 base64url characters.
pub fn is_device_bearer(s: &str) -> bool {
    s.strip_prefix("sewdv_").is_some_and(|rest| {
        rest.len() == 43
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

/// `GET /sew/native/info`'s answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneInfo {
    /// The lane version.
    pub lane: u32,
    /// The App's identity kind (`k2-session`, `k2-pass`, `standalone`, ...).
    #[serde(default)]
    pub identity: Value,
    /// Which sign-ins it offers.
    #[serde(default)]
    pub signin: SigninOffer,
    /// Whether it answers the voice engine's joins.
    #[serde(default)]
    pub voice: bool,
    /// Where the engine signals: `lane` (the lane's own) or `engine` (the engine's address).
    #[serde(default)]
    pub signalling: Option<String>,
    /// The call door: `lane` (the lane's fallback) or `host` (the host's own door).
    #[serde(default)]
    pub call_door: Option<String>,
    /// Whether it serves Record and Stop (`/sew/native/voice/recording`).
    #[serde(default)]
    pub recording: bool,
    /// What it asks of the shell (`{min}`).
    #[serde(default)]
    pub shell: ShellInfo,
}

/// `info.shell`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellInfo {
    /// The oldest shell version (`x.y.z`) the App works with, or `None`. A shell older than it
    /// is refused sign-in with `shell_too_old` (it asks to be updated).
    #[serde(default)]
    pub min: Option<String>,
}

/// A version's numbers: `x.y.z`, read up to a `-` or `+` suffix (`x` and `x.y` read with
/// zeros). `None` when it isn't one.
pub fn version_triple(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let mut n = [0u64; 3];
    for (i, slot) in n.iter_mut().enumerate() {
        match parts.next() {
            Some(p) => *slot = p.parse().ok()?,
            None if i > 0 => break,
            None => return None,
        }
    }
    if parts.next().is_some() {
        return None;
    }
    Some((n[0], n[1], n[2]))
}

impl ShellInfo {
    /// Whether a shell at `version` is too old for this App: `Some(true)` when `version` is
    /// below `min`, `Some(false)` when it isn't (or there's no `min`), `None` when either
    /// doesn't read as a version.
    pub fn too_old(&self, version: &str) -> Option<bool> {
        let Some(min) = &self.min else {
            return Some(false);
        };
        Some(version_triple(version)? < version_triple(min)?)
    }
}

/// `info.signin`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigninOffer {
    /// The password form (`/sew/native/signin/password`).
    #[serde(default)]
    pub password: bool,
    /// Pairing from a signed-in page (`/sew/native/pair/redeem`).
    #[serde(default)]
    pub pairing: bool,
    /// The host's sign-in door (`/sew/native/signin/door/begin|finish` on the lane, the
    /// password posted to the host's own `/.k2/auth/sign-in`). The plugin uses it only when
    /// this is `true` and the host's `/.k2/auth/ping` answers `{"k2": true}`.
    #[serde(default)]
    pub door: bool,
}

/// The device a sign-in names: `{id, name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceName {
    /// The install's own stable id for this account (1 to 128 of `[A-Za-z0-9_-]`).
    pub id: String,
    /// What the device list shows (1 to 64 characters, no control characters).
    pub name: String,
}

impl DeviceName {
    /// Checks the lane's limits (`invalid_args` otherwise).
    pub fn validate(&self) -> Result<(), AppError> {
        let id_ok = (1..=128).contains(&self.id.len())
            && self
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !id_ok {
            return Err(AppError::invalid_args("device.id"));
        }
        let n = self.name.chars().count();
        if !(1..=64).contains(&n) || self.name.chars().any(char::is_control) {
            return Err(AppError::invalid_args(
                "device name: 1 to 64 characters, no control characters",
            ));
        }
        Ok(())
    }
}

/// A device session as the lane shows it (never its bearer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// The session's public id (22 base64url): what a revoke names.
    pub id: String,
    /// The install's device id it was signed in with.
    pub device_id: String,
    /// The device's name.
    pub name: String,
    /// When it began (ISO 8601).
    #[serde(default)]
    pub created_at: Option<String>,
    /// When it was last used.
    #[serde(default)]
    pub last_used_at: Option<String>,
    /// When it ends at the latest.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// In a device list: the asker's own session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<bool>,
}

/// A sign-in's answer (password, pairing or the door).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SignIn {
    /// The device bearer.
    pub token: Secret,
    /// Sew's member id.
    pub member_id: String,
    /// The new session.
    pub session: SessionInfo,
}

/// `GET /sew/native/whoami`'s answer.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WhoAmI {
    /// The person (Sew's member).
    #[serde(default)]
    pub member: Value,
    /// The Sew install (a decimal id; a number or a string).
    #[serde(default)]
    pub install_id: Value,
    /// The person's spaces.
    #[serde(default)]
    pub spaces: Value,
    /// What the person may do.
    #[serde(default)]
    pub caps: Value,
    /// The App's identity kind.
    #[serde(default)]
    pub identity: Value,
    /// This session.
    pub session: SessionInfo,
}

impl WhoAmI {
    /// The install id as a number (`forget_server`'s), when it reads as one.
    pub fn install_id(&self) -> Option<u64> {
        match &self.install_id {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// The person's display name, when the member carries one.
    pub fn display_name(&self) -> Option<String> {
        ["display_name", "name", "username"]
            .iter()
            .find_map(|k| self.member.get(*k).and_then(Value::as_str))
            .map(str::to_owned)
    }
}

/// `POST /sew/native/handoff`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HandoffCode {
    /// The single-use code.
    pub code: Secret,
    /// How long it lives (60 000).
    pub expires_in_ms: u64,
}

/// `POST /sew/native/ticket`'s answer. The ticket stays in Rust: `Debug` never prints it, and
/// the type doesn't serialize.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TicketAnswer {
    /// The single-use ticket the socket's IDENTIFY or RESUME carries.
    pub ticket: Secret,
    /// The socket's URL (`wss://<app>/sew/native/api/v1/gateway?v=1&encoding=json`).
    pub url: String,
    /// How long the ticket lives.
    #[serde(default)]
    pub expires_in_ms: Option<u64>,
}

/// `POST /sew/native/signin/door/begin`'s answer (the host's sign-in door). The id and the
/// challenge stay in Rust: `Debug` never prints them, and the type doesn't serialize.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DoorStart {
    /// The begin's one-time id (43 base64url), named by the finish (the lane keeps the PKCE
    /// verifier under it).
    pub id: Secret,
    /// The PKCE challenge the password goes to the host's door with.
    pub challenge: Secret,
    /// Where the lane says the door is (`https://<app>/.k2/auth/sign-in`). Only checked: the
    /// plugin posts to the confirmed server's own door, never to this URL.
    pub door: String,
    /// How long the begin lives (600 000).
    #[serde(default)]
    pub expires_in_ms: Option<u64>,
}

impl DoorStart {
    /// Whether the answer has the lane's shapes: the id 43 base64url characters, the
    /// challenge 43 to 128 PKCE characters.
    pub fn well_formed(&self) -> bool {
        let id = self.id.expose();
        let ch = self.challenge.expose();
        id.len() == 43
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            && (43..=128).contains(&ch.len())
            && ch
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
    }
}

/// A refusal of the host's sign-in door (`{ok: false, error: {code, hint, retryAfterSecs?}}`),
/// with K2's `changeToken` kept apart (it never goes into an error).
#[derive(Debug, Clone, PartialEq)]
pub struct HostDoorFailure {
    /// The failure (a refusal keeps K2's code: `bad_login`, `locked`, `rate_limited`,
    /// `password_change_required`, ...).
    pub failure: Box<LaneFailure>,
    /// `password_change_required`'s `changeToken`, for the host's `set-password`.
    pub change_token: Option<Secret>,
}

impl From<LaneFailure> for HostDoorFailure {
    fn from(failure: LaneFailure) -> HostDoorFailure {
        HostDoorFailure {
            failure: Box::new(failure),
            change_token: None,
        }
    }
}

/// An HTTP method of the lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST` with a JSON body.
    Post,
}

/// One request to the lane: a path under the App's origin, the device bearer (when the route
/// takes one) and a JSON body. No cookie, no `Origin`.
#[derive(Debug, Clone)]
pub struct LaneRequest {
    /// The method.
    pub method: Method,
    /// The path (`/sew/native/...`).
    pub path: String,
    /// `Authorization: Bearer <this>`.
    pub bearer: Option<Secret>,
    /// The JSON body of a `POST` (`{}` when `None`).
    pub body: Option<Value>,
}

/// The lane's answer.
#[derive(Clone)]
pub struct LaneResponse {
    /// The status.
    pub status: u16,
    /// `Retry-After` in milliseconds (from seconds), when given.
    pub retry_after_ms: Option<u64>,
    /// The body.
    pub body: Vec<u8>,
}

impl fmt::Debug for LaneResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The body may carry a bearer, a code or a media token.
        f.debug_struct("LaneResponse")
            .field("status", &self.status)
            .field("retry_after_ms", &self.retry_after_ms)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// How the request could not be sent or answered (never quoting a body or a token).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TransportError(pub String);

/// The method of a [`ByteRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteMethod {
    /// `GET`.
    Get,
    /// `HEAD`.
    Head,
    /// `POST`.
    Post,
}

impl ByteMethod {
    /// `GET`, `HEAD` or `POST`.
    pub fn as_str(self) -> &'static str {
        match self {
            ByteMethod::Get => "GET",
            ByteMethod::Head => "HEAD",
            ByteMethod::Post => "POST",
        }
    }
}

/// A request of the App's client API under the lane (`/sew/native/api/v1/*`): any body, the
/// headers the caller chose (never `Authorization`, `Cookie` or `Origin`: the bearer goes in
/// [`ByteRequest::bearer`]), and the answer's headers kept.
#[derive(Clone)]
pub struct ByteRequest {
    /// The method.
    pub method: ByteMethod,
    /// The path and query (`/sew/native/api/v1/...?...`).
    pub path: String,
    /// `Authorization: Bearer <this>`.
    pub bearer: Option<Secret>,
    /// Further request headers, lower-case names (`content-type`, `idempotency-key`,
    /// `range`, ...).
    pub headers: Vec<(String, String)>,
    /// The body of a `POST`.
    pub body: Option<Vec<u8>>,
    /// The largest answer read; a longer one is a [`TransportError`].
    pub max_answer: usize,
    /// How long the whole exchange may take.
    pub timeout: std::time::Duration,
}

impl ByteRequest {
    /// The first header named `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

impl fmt::Debug for ByteRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The path may carry a signed query; the body is the person's.
        f.debug_struct("ByteRequest")
            .field("method", &self.method)
            .field("headers", &self.headers.len())
            .field("body_len", &self.body.as_ref().map(Vec::len))
            .finish_non_exhaustive()
    }
}

/// The answer to a [`ByteRequest`]: the status, every header (lower-case names) and the body.
#[derive(Clone)]
pub struct ByteResponse {
    /// The status.
    pub status: u16,
    /// The headers, lower-case names, in order.
    pub headers: Vec<(String, String)>,
    /// The body (empty for `HEAD`).
    pub body: Vec<u8>,
}

impl ByteResponse {
    /// The first header named `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

impl fmt::Debug for ByteResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ByteResponse")
            .field("status", &self.status)
            .field("headers", &self.headers.len())
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// Sends a [`LaneRequest`] to an App's origin.
pub trait Transport: Send + Sync + 'static {
    /// Send `req` to `origin` and answer whatever status came back.
    fn send<'a>(
        &'a self,
        origin: &'a Origin,
        req: LaneRequest,
    ) -> BoxFuture<'a, Result<LaneResponse, TransportError>>;

    /// Send a [`ByteRequest`] (the client API, its byte routes and uploads) and answer
    /// whatever status came back. The default refuses: a transport that carries only the
    /// lane's own JSON routes need not implement it.
    fn send_bytes<'a>(
        &'a self,
        origin: &'a Origin,
        req: ByteRequest,
    ) -> BoxFuture<'a, Result<ByteResponse, TransportError>> {
        let _ = (origin, req);
        Box::pin(std::future::ready(Err(TransportError(
            "this transport carries the lane's own routes only".into(),
        ))))
    }
}

/// A lane refusal: the status and the lane's `{code, detail, error: {hint}}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    /// The status.
    pub status: u16,
    /// The lane's `code` (or `error.code`); `None` when the body has neither.
    pub code: Option<String>,
    /// The lane's `detail` (or `error.details`).
    pub detail: Value,
    /// `error.hint`.
    pub hint: Option<String>,
    /// `Retry-After`.
    pub retry_after_ms: Option<u64>,
}

impl Refusal {
    /// Reads a non-`2xx` answer.
    pub fn from_response(resp: &LaneResponse) -> Refusal {
        let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        let err = v.get("error");
        let code = v
            .get("code")
            .and_then(Value::as_str)
            .or_else(|| err.and_then(|e| e.get("code")).and_then(Value::as_str))
            .map(str::to_owned);
        let detail = match v.get("detail") {
            Some(d) if !d.is_null() => d.clone(),
            _ => err
                .and_then(|e| e.get("details"))
                .cloned()
                .unwrap_or(Value::Null),
        };
        let hint = err
            .and_then(|e| e.get("hint"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        // `Retry-After`, else the host door's `error.retryAfterSecs`.
        let retry_after_ms = resp.retry_after_ms.or_else(|| {
            err.and_then(|e| e.get("retryAfterSecs"))
                .and_then(Value::as_u64)
                .map(|s| s.saturating_mul(1000))
        });
        Refusal {
            status: resp.status,
            code,
            detail,
            hint,
            retry_after_ms,
        }
    }

    /// The refusal as a command's error: the lane's code; a `404` without one is
    /// `lane_not_found`, any other status without one `http_<status>`.
    pub fn to_error(&self) -> AppError {
        let code = match (&self.code, self.status) {
            (Some(c), _) => c.clone(),
            (None, 404) => code::LANE_NOT_FOUND.to_owned(),
            (None, s) => format!("http_{s}"),
        };
        let mut detail = self.detail.clone();
        if let Some(ms) = self.retry_after_ms {
            match &mut detail {
                Value::Object(m) => {
                    m.insert("retry_after_ms".into(), ms.into());
                }
                Value::Null => detail = serde_json::json!({ "retry_after_ms": ms }),
                _ => {}
            }
        }
        AppError {
            code,
            detail,
            hint: self.hint.clone(),
        }
    }

    /// Whether the device session is gone (`401`: `signed_out` or `unauthenticated`).
    pub fn is_signed_out(&self) -> bool {
        self.status == 401
            && matches!(
                self.code.as_deref(),
                Some("signed_out") | Some("unauthenticated") | None
            )
    }

    /// `detail.reason` of a `signed_out`, else `unauthenticated` for a `401`.
    pub fn signed_out_reason(&self) -> String {
        self.detail
            .get("reason")
            .and_then(Value::as_str)
            .map_or_else(|| "unauthenticated".to_owned(), str::to_owned)
    }
}

/// How a lane call failed.
#[derive(Debug, Clone, PartialEq)]
pub enum LaneFailure {
    /// It never got an answer.
    Unreachable(TransportError),
    /// A non-`2xx` answer.
    Refused(Refusal),
    /// A `2xx` that did not parse (what failed, never the body).
    BadAnswer(String),
}

impl LaneFailure {
    /// As a command's error.
    pub fn to_error(&self) -> AppError {
        match self {
            LaneFailure::Unreachable(e) => AppError::with_detail(code::UNREACHABLE, e.0.clone()),
            LaneFailure::Refused(r) => r.to_error(),
            LaneFailure::BadAnswer(what) => AppError::with_detail(code::BAD_ANSWER, what.clone()),
        }
    }

    /// The refusal, when it is one.
    pub fn refusal(&self) -> Option<&Refusal> {
        match self {
            LaneFailure::Refused(r) => Some(r),
            _ => None,
        }
    }
}

impl From<LaneFailure> for AppError {
    fn from(f: LaneFailure) -> AppError {
        f.to_error()
    }
}

/// The lane client: one per app, any number of origins.
#[derive(Clone)]
pub struct LaneClient {
    transport: Arc<dyn Transport>,
}

impl fmt::Debug for LaneClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaneClient").finish_non_exhaustive()
    }
}

fn parse<T: serde::de::DeserializeOwned>(
    resp: &LaneResponse,
    what: &str,
) -> Result<T, LaneFailure> {
    serde_json::from_slice(&resp.body).map_err(|_| LaneFailure::BadAnswer(what.to_owned()))
}

impl LaneClient {
    /// A client over `transport`.
    pub fn new(transport: Arc<dyn Transport>) -> LaneClient {
        LaneClient { transport }
    }

    /// Sends one request; a `2xx` answer comes back, anything else as a [`LaneFailure`].
    pub async fn call(
        &self,
        origin: &Origin,
        req: LaneRequest,
    ) -> Result<LaneResponse, LaneFailure> {
        let resp = self
            .transport
            .send(origin, req)
            .await
            .map_err(LaneFailure::Unreachable)?;
        if (200..300).contains(&resp.status) {
            Ok(resp)
        } else {
            Err(LaneFailure::Refused(Refusal::from_response(&resp)))
        }
    }

    /// The raw answer, whatever its status (the JoinProvider reads refusals itself).
    pub async fn send(
        &self,
        origin: &Origin,
        req: LaneRequest,
    ) -> Result<LaneResponse, TransportError> {
        self.transport.send(origin, req).await
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        origin: &Origin,
        path: &str,
        bearer: Option<&Secret>,
        what: &str,
    ) -> Result<T, LaneFailure> {
        let req = LaneRequest {
            method: Method::Get,
            path: path.to_owned(),
            bearer: bearer.cloned(),
            body: None,
        };
        let resp = self.call(origin, req).await?;
        parse(&resp, what)
    }

    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        origin: &Origin,
        path: &str,
        bearer: Option<&Secret>,
        body: Value,
        what: &str,
    ) -> Result<T, LaneFailure> {
        let req = LaneRequest {
            method: Method::Post,
            path: path.to_owned(),
            bearer: bearer.cloned(),
            body: Some(body),
        };
        let resp = self.call(origin, req).await?;
        parse(&resp, what)
    }

    /// A [`ByteRequest`], whatever its status.
    pub async fn send_bytes(
        &self,
        origin: &Origin,
        req: ByteRequest,
    ) -> Result<ByteResponse, TransportError> {
        self.transport.send_bytes(origin, req).await
    }

    /// `POST /sew/native/ticket`: a gateway ticket for the device.
    pub async fn ticket(
        &self,
        origin: &Origin,
        bearer: &Secret,
    ) -> Result<TicketAnswer, LaneFailure> {
        self.post(
            origin,
            route::TICKET,
            Some(bearer),
            serde_json::json!({}),
            "ticket",
        )
        .await
    }

    /// `GET /sew/native/info`.
    pub async fn info(&self, origin: &Origin) -> Result<LaneInfo, LaneFailure> {
        self.get(origin, route::INFO, None, "info").await
    }

    /// `POST /sew/native/signin/password`.
    pub async fn sign_in_password(
        &self,
        origin: &Origin,
        username: &str,
        password: &Secret,
        device: &DeviceName,
    ) -> Result<SignIn, LaneFailure> {
        let body = serde_json::json!({
            "username": username,
            "password": password.expose(),
            "device": device,
        });
        let s: SignIn = self
            .post(origin, route::SIGNIN_PASSWORD, None, body, "sign-in")
            .await?;
        check_bearer(s)
    }

    /// `POST /sew/native/pair/redeem`.
    pub async fn pair_redeem(
        &self,
        origin: &Origin,
        code: &Secret,
        device: &DeviceName,
    ) -> Result<SignIn, LaneFailure> {
        let body = serde_json::json!({ "code": code.expose(), "device": device });
        let s: SignIn = self
            .post(origin, route::PAIR_REDEEM, None, body, "pairing")
            .await?;
        check_bearer(s)
    }

    /// `POST /sew/native/signin/door/begin` `{device}`: a one-time id and a PKCE challenge
    /// (an answer without the lane's shapes is [`LaneFailure::BadAnswer`]).
    pub async fn door_begin(
        &self,
        origin: &Origin,
        device: &DeviceName,
    ) -> Result<DoorStart, LaneFailure> {
        let body = serde_json::json!({ "device": device });
        let s: DoorStart = self
            .post(origin, route::SIGNIN_BEGIN, None, body, "sign-in begin")
            .await?;
        if !s.well_formed() {
            return Err(LaneFailure::BadAnswer(
                "the sign-in begin's id or challenge".into(),
            ));
        }
        Ok(s)
    }

    /// `POST /sew/native/signin/door/finish` `{id, code, device}`.
    pub async fn door_finish(
        &self,
        origin: &Origin,
        id: &Secret,
        code: &Secret,
        device: &DeviceName,
    ) -> Result<SignIn, LaneFailure> {
        let body = serde_json::json!({
            "id": id.expose(),
            "code": code.expose(),
            "device": device,
        });
        let s: SignIn = self
            .post(origin, route::SIGNIN_FINISH, None, body, "sign-in finish")
            .await?;
        check_bearer(s)
    }

    /// `GET <origin>/.k2/auth/ping` on the host: whether it answers exactly `{"k2": true}`
    /// (any other answer, a refusal included, is `false`: send no password).
    pub async fn host_door_ping(&self, origin: &Origin) -> Result<bool, TransportError> {
        let req = LaneRequest {
            method: Method::Get,
            path: route::HOST_DOOR_PING.to_owned(),
            bearer: None,
            body: None,
        };
        let resp = self.transport.send(origin, req).await?;
        if !(200..300).contains(&resp.status) {
            return Ok(false);
        }
        let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        Ok(v.get("k2") == Some(&Value::Bool(true)))
    }

    async fn host_door_post(
        &self,
        origin: &Origin,
        path: &str,
        body: Value,
    ) -> Result<Option<Secret>, HostDoorFailure> {
        let req = LaneRequest {
            method: Method::Post,
            path: path.to_owned(),
            bearer: None,
            body: Some(body),
        };
        let resp = self
            .transport
            .send(origin, req)
            .await
            .map_err(LaneFailure::Unreachable)?;
        let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        if !(200..300).contains(&resp.status) {
            let change_token = v
                .get("changeToken")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(Secret::new);
            return Err(HostDoorFailure {
                failure: Box::new(LaneFailure::Refused(Refusal::from_response(&resp))),
                change_token,
            });
        }
        if v.get("ok") != Some(&Value::Bool(true)) {
            return Err(LaneFailure::BadAnswer("the host's sign-in door".into()).into());
        }
        match v.get("code") {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(c)) if is_door_code(c) => Ok(Some(Secret::new(c.clone()))),
            Some(_) => Err(LaneFailure::BadAnswer("the host door's code".into()).into()),
        }
    }

    /// `POST <origin>/.k2/auth/sign-in` `{username, password, challenge}` on the host: the
    /// door's one-time code. A refusal keeps K2's code; `password_change_required` carries its
    /// `changeToken` in [`HostDoorFailure::change_token`].
    pub async fn host_door_sign_in(
        &self,
        origin: &Origin,
        username: &str,
        password: &Secret,
        challenge: &Secret,
    ) -> Result<Secret, HostDoorFailure> {
        let body = serde_json::json!({
            "username": username,
            "password": password.expose(),
            "challenge": challenge.expose(),
        });
        self.host_door_post(origin, route::HOST_DOOR_SIGN_IN, body)
            .await?
            .ok_or_else(|| LaneFailure::BadAnswer("the host door answered no code".into()).into())
    }

    /// `POST <origin>/.k2/auth/set-password` `{token, password, challenge}` on the host: the
    /// first password after `password_change_required`. `Ok(None)`: the password is set, but
    /// the host made no code (the person can't use this App).
    pub async fn host_door_set_password(
        &self,
        origin: &Origin,
        token: &Secret,
        password: &Secret,
        challenge: &Secret,
    ) -> Result<Option<Secret>, HostDoorFailure> {
        let body = serde_json::json!({
            "token": token.expose(),
            "password": password.expose(),
            "challenge": challenge.expose(),
        });
        self.host_door_post(origin, route::HOST_DOOR_SET_PASSWORD, body)
            .await
    }

    /// `GET /sew/native/whoami`.
    pub async fn whoami(&self, origin: &Origin, bearer: &Secret) -> Result<WhoAmI, LaneFailure> {
        self.get(origin, route::WHOAMI, Some(bearer), "whoami")
            .await
    }

    /// `GET /sew/native/devices`.
    pub async fn devices(
        &self,
        origin: &Origin,
        bearer: &Secret,
    ) -> Result<Vec<SessionInfo>, LaneFailure> {
        #[derive(Deserialize)]
        struct List {
            devices: Vec<SessionInfo>,
        }
        let l: List = self
            .get(origin, route::DEVICES, Some(bearer), "devices")
            .await?;
        Ok(l.devices)
    }

    /// `POST /sew/native/devices/{id}/revoke`.
    pub async fn revoke(
        &self,
        origin: &Origin,
        bearer: &Secret,
        session_id: &str,
    ) -> Result<(), LaneFailure> {
        let ok = !session_id.is_empty()
            && session_id.len() <= 128
            && session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !ok {
            return Err(LaneFailure::BadAnswer("a session id".into()));
        }
        let _: Value = self
            .post(
                origin,
                &route::revoke(session_id),
                Some(bearer),
                serde_json::json!({}),
                "revoke",
            )
            .await?;
        Ok(())
    }

    /// `POST /sew/native/signout`.
    pub async fn sign_out(&self, origin: &Origin, bearer: &Secret) -> Result<(), LaneFailure> {
        let _: Value = self
            .post(
                origin,
                route::SIGNOUT,
                Some(bearer),
                serde_json::json!({}),
                "sign-out",
            )
            .await?;
        Ok(())
    }

    /// `POST /sew/native/handoff`.
    pub async fn handoff(
        &self,
        origin: &Origin,
        bearer: &Secret,
    ) -> Result<HandoffCode, LaneFailure> {
        self.post(
            origin,
            route::HANDOFF,
            Some(bearer),
            serde_json::json!({}),
            "handoff",
        )
        .await
    }
}

/// Whether `s` reads as the host door's one-time code (1 to 512 visible ASCII characters).
fn is_door_code(s: &str) -> bool {
    (1..=512).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_graphic())
}

fn check_bearer(s: SignIn) -> Result<SignIn, LaneFailure> {
    if is_device_bearer(s.token.expose()) {
        Ok(s)
    } else {
        Err(LaneFailure::BadAnswer(
            "the sign-in answered no device bearer".into(),
        ))
    }
}
