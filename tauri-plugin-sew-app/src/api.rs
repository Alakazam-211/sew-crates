//! The API transport's HTTP half: the App's client API, its signed byte routes and uploads,
//! through the App server's native lane with the account's device bearer, for a bundled UI
//! that never holds the bearer.
//!
//! - [`AppService::api`] (`api`): `{method, path, body, idempotency_key}` → `GET|POST
//!   /sew/native/api/v1<path>` with the bearer. The answer is `{status, headers, body}` with
//!   only the safe headers ([`API_RESPONSE_HEADERS`] and `x-ratelimit-*`).
//! - [`AppService::fetch_bytes`]: a signed byte route ([`BYTE_ROUTES`]) for the `sewfile:` URI
//!   scheme (`<img>`, `<video>`, downloads), Sew's own headers kept but the hop-by-hop ones
//!   and `Set-Cookie`.
//! - [`AppService::upload`] (`upload_bytes`, `upload_file`): raw bytes to
//!   `uploads/{id}/data`, `me/avatar`, `spaces/{id}/icon` or `spaces/{id}/emoji`.
//!   `upload_file` takes a handle of a file the person picked in the file dialog
//!   (`pick_files`, or [`AppService::picked_file`] from the host's own dialog), never a path.
//!
//! The paths are checked against the lane's own allowlist before anything is sent, and a
//! refusal has the lane's shape (`{ok: false, code, detail, error: {code, hint}}`) and the
//! status the lane would answer, so a page reads the plugin's refusals and the server's alike.
//! A `401` that ends the device session (`signed_out`, `unauthenticated`) marks the account
//! signed out ([`AppService::session_ended`]) and still answers the page.
//!
//! The transport speaks the lane's contract only: any server that answers the lane's routes
//! and fixture is served the same way.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::error::{AppError, code};
use crate::lane::{ByteMethod, ByteRequest, ByteResponse, Secret};
use crate::servers::AccountKey;
use crate::service::AppService;

/// The largest API answer read (a long message page is a few hundred KiB).
pub const API_MAX_ANSWER: usize = 8 << 20;
/// The largest byte-route answer read; a larger file is read in parts (`Range`).
pub const BYTES_MAX_ANSWER: usize = 64 << 20;
/// How long an API exchange may take.
pub const API_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a byte-route exchange or an upload may take.
pub const BYTES_TIMEOUT: Duration = Duration::from_secs(120);

/// The lane's body caps per route.
pub mod caps {
    /// An upload's bytes (`uploads/{id}/data`).
    pub const UPLOAD_DATA: usize = 16 << 20;
    /// An image (`me/avatar`, `spaces/{id}/icon`).
    pub const IMAGE: usize = 5 << 20;
    /// A custom emoji (`spaces/{id}/emoji`).
    pub const EMOJI: usize = 256 << 10;
    /// A message (`channels/{id}/messages`, `messages/{id}/edit|forward`).
    pub const MESSAGE: usize = 256 << 10;
    /// Any other JSON body.
    pub const DEFAULT: usize = 64 << 10;
}

/// The API answer's headers a page gets (and every `x-ratelimit-*`): never `Set-Cookie`, never
/// `Authorization`.
pub const API_RESPONSE_HEADERS: [&str; 6] = [
    "content-type",
    "content-length",
    "etag",
    "retry-after",
    "idempotent-replayed",
    "cache-control",
];

/// The request headers a byte route passes from the webview: a resume or a part of a file.
pub const BYTE_REQUEST_HEADERS: [&str; 4] =
    ["range", "if-range", "if-none-match", "if-modified-since"];

/// Never passed back from a byte route: the hop-by-hop headers, cookies and credentials.
const DROPPED_RESPONSE_HEADERS: [&str; 14] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "proxy-connection",
    "set-cookie",
    "set-cookie2",
    "authorization",
    "www-authenticate",
];

/// One signed byte route: its path under `/api/v1/` (`{}` is one segment of
/// `[A-Za-z0-9_.~-]`, never `.` or `..`), its methods, and whether it needs a signed query
/// (`exp` and `sig`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRoute {
    /// The pattern.
    pub pattern: &'static str,
    /// The methods the lane passes.
    pub methods: &'static [&'static str],
    /// Whether `exp` and `sig` must be in the query.
    pub signed: bool,
}

/// The lane's signed byte routes (the App's one allowlist), as the `sewfile:` scheme serves
/// them (`GET`, `HEAD`) and the upload commands post them (`uploads/{id}/data`).
pub const BYTE_ROUTES: [ByteRoute; 10] = [
    ByteRoute {
        pattern: "files/{}",
        methods: &["GET", "HEAD"],
        signed: true,
    },
    ByteRoute {
        pattern: "files/{}/thumb",
        methods: &["GET", "HEAD"],
        signed: false,
    },
    ByteRoute {
        pattern: "files/{}/preview",
        methods: &["GET", "HEAD"],
        signed: false,
    },
    ByteRoute {
        pattern: "files/{}/poster",
        methods: &["GET", "HEAD"],
        signed: false,
    },
    ByteRoute {
        pattern: "avatars/{}/{}/{}",
        methods: &["GET"],
        signed: false,
    },
    ByteRoute {
        pattern: "emoji-images/{}/{}",
        methods: &["GET"],
        signed: false,
    },
    ByteRoute {
        pattern: "space-icons/{}/{}/{}",
        methods: &["GET"],
        signed: false,
    },
    ByteRoute {
        pattern: "gifs/media/{}",
        methods: &["GET"],
        signed: false,
    },
    ByteRoute {
        pattern: "previews/{}/image",
        methods: &["GET"],
        signed: false,
    },
    ByteRoute {
        pattern: "uploads/{}/data",
        methods: &["GET", "POST"],
        signed: false,
    },
];

/// Prefixes under `/api/v1/` never served as bytes, whatever the list says.
pub const NEVER_BYTES: [&str; 3] = ["recordings/", "compliance/", "digest/"];

/// The URI scheme of the signed byte routes: `sewfile://localhost/<account>/<path>` (or the
/// platform's `http://sewfile.localhost/...`), where `<account>` is an account id or
/// `active`, and `<path>` the route under `/api/v1` with its query (see
/// [`parse_file_request`]).
pub const FILE_SCHEME: &str = "sewfile";

/// A refusal the plugin writes itself, in the lane's shape and with the status the lane would
/// answer.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalRefusal {
    /// The status.
    pub status: u16,
    /// The lane's code (`not_found`, `forbidden`, `method_not_allowed`, ...).
    pub code: String,
    /// The sentence (`error.hint`).
    pub hint: String,
    /// `detail` (`null` when none).
    pub detail: Value,
    /// `Allow` of a `405`.
    pub allow: Option<&'static str>,
}

impl LocalRefusal {
    fn new(status: u16, code: &str, hint: &str) -> LocalRefusal {
        LocalRefusal {
            status,
            code: code.to_owned(),
            hint: hint.to_owned(),
            detail: Value::Null,
            allow: None,
        }
    }

    fn detail(mut self, detail: Value) -> LocalRefusal {
        self.detail = detail;
        self
    }

    fn not_found(hint: &str) -> LocalRefusal {
        LocalRefusal::new(404, "not_found", hint)
    }

    fn method(allow: &'static str) -> LocalRefusal {
        LocalRefusal {
            allow: Some(allow),
            ..LocalRefusal::new(405, "method_not_allowed", "method not allowed")
        }
    }

    fn too_large(max: usize) -> LocalRefusal {
        LocalRefusal::new(413, "too_large", "body too large").detail(json!({ "max": max }))
    }

    /// The `401 signed_out` the lane answers an ended session.
    pub fn signed_out(reason: &str) -> LocalRefusal {
        LocalRefusal::new(
            401,
            code::SIGNED_OUT,
            "This device was signed out. Sign in again.",
        )
        .detail(json!({ "reason": reason }))
    }

    /// The lane's error body: `{ok: false, code, detail, error: {code, hint, details?}}`.
    pub fn body(&self) -> Value {
        let mut error = json!({ "code": self.code, "hint": self.hint });
        if !self.detail.is_null() {
            error["details"] = self.detail.clone();
        }
        json!({ "ok": false, "code": self.code, "detail": self.detail, "error": error })
    }

    fn headers(&self) -> Vec<(String, String)> {
        let mut h = vec![
            (
                "content-type".to_owned(),
                "application/json; charset=utf-8".to_owned(),
            ),
            ("cache-control".to_owned(), "no-store".to_owned()),
        ];
        if let Some(a) = self.allow {
            h.push(("allow".to_owned(), a.to_owned()));
        }
        h
    }

    /// As an `api` answer.
    pub fn reply(&self) -> ApiReply {
        ApiReply {
            status: self.status,
            headers: self.headers().into_iter().collect(),
            body: self.body(),
        }
    }

    /// As a `sewfile:` answer.
    pub fn bytes(&self) -> ByteReply {
        ByteReply {
            status: self.status,
            headers: self.headers(),
            body: serde_json::to_vec(&self.body()).unwrap_or_default(),
        }
    }
}

/// A checked path of the client API: Sew's path (`/api/v1/...`) and the query (`?…` or
/// empty, its `token=` parameters dropped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiTarget {
    /// `/api/v1/<rest>`.
    pub api_path: String,
    /// `?<query>` or empty.
    pub query: String,
}

impl ApiTarget {
    /// The lane's path: `/sew/native/api/v1/<rest>?<query>`.
    pub fn lane_path(&self) -> String {
        format!("/sew/native{}{}", self.api_path, self.query)
    }

    fn rest(&self) -> &str {
        &self.api_path["/api/v1/".len()..]
    }
}

fn pchar(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
                | b'%'
        )
}

/// Reads `/<rest>[?query]` (a path under `/api/v1`): every segment non-empty, no dot segment,
/// no encoded slash, backslash or dot, URL characters only; `token=` dropped from the query.
fn target_of(path: &str) -> Result<ApiTarget, LocalRefusal> {
    let nf = || LocalRefusal::not_found("not found");
    let (p, q) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path, None),
    };
    let rest = p.strip_prefix('/').ok_or_else(nf)?;
    if rest.is_empty() || rest.len() > 2048 {
        return Err(nf());
    }
    for seg in rest.split('/') {
        let lower = seg.to_ascii_lowercase();
        if seg.is_empty()
            || seg == "."
            || seg == ".."
            || lower.contains("%2f")
            || lower.contains("%5c")
            || lower.contains("%2e")
            || !seg.bytes().all(pchar)
        {
            return Err(nf());
        }
    }
    let query = match q {
        None => String::new(),
        Some(q) => {
            let ok = q.len() <= 4096 && q.bytes().all(|b| pchar(b) || b == b'/' || b == b'?');
            if !ok {
                return Err(nf());
            }
            strip_token(q)
        }
    };
    Ok(ApiTarget {
        api_path: format!("/api/v1/{rest}"),
        query,
    })
}

/// Drops every `token=` parameter, keeping the rest in order (`?…` or empty).
fn strip_token(q: &str) -> String {
    let kept: Vec<&str> = q
        .split('&')
        .filter(|kv| {
            !kv.is_empty() && {
                let k = kv.split('=').next().unwrap_or_default();
                !percent_decode(k).eq_ignore_ascii_case("token")
            }
        })
        .collect();
    if kept.is_empty() {
        String::new()
    } else {
        format!("?{}", kept.join("&"))
    }
}

/// One `%XX` decoding (an invalid escape stays as it is); `None` when not UTF-8.
fn percent_decode_bytes(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

fn percent_decode(s: &str) -> String {
    percent_decode_bytes(s).unwrap_or_else(|| s.to_owned())
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Whether `rest` (the path under `/api/v1/`) has `pattern`'s shape.
fn pattern_matches(pattern: &str, rest: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let r: Vec<&str> = rest.split('/').collect();
    p.len() == r.len()
        && p.iter().zip(&r).all(|(p, r)| {
            if *p == "{}" {
                *r != "."
                    && *r != ".."
                    && !r.is_empty()
                    && r.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'~' | b'-')
                    })
            } else {
                p == r
            }
        })
}

/// The byte route `rest` names, whatever the method.
pub fn byte_route_of(rest: &str) -> Option<&'static ByteRoute> {
    BYTE_ROUTES
        .iter()
        .find(|r| pattern_matches(r.pattern, rest))
}

/// Whether `rest` is a raw-body route (an image or emoji: bytes, never JSON).
fn raw_body_route(rest: &str) -> bool {
    rest == "me/avatar"
        || pattern_matches("spaces/{}/icon", rest)
        || pattern_matches("spaces/{}/emoji", rest)
}

/// The lane's body cap for a `POST` on `rest`.
pub fn body_cap(rest: &str) -> usize {
    if pattern_matches("uploads/{}/data", rest) {
        caps::UPLOAD_DATA
    } else if rest == "me/avatar" || pattern_matches("spaces/{}/icon", rest) {
        caps::IMAGE
    } else if pattern_matches("spaces/{}/emoji", rest) {
        caps::EMOJI
    } else if pattern_matches("channels/{}/messages", rest)
        || pattern_matches("messages/{}/edit", rest)
        || pattern_matches("messages/{}/forward", rest)
    {
        caps::MESSAGE
    } else {
        caps::DEFAULT
    }
}

/// Checks an `api` call's method and path against the lane's allowlist: `GET` or `POST`; no
/// auth route or gateway ticket (`403 forbidden`); the gateway is a socket (`426
/// upgrade_required`); the byte routes and raw-body routes go through the scheme and the
/// upload commands.
pub fn api_target(method: &str, path: &str) -> Result<(ByteMethod, ApiTarget), LocalRefusal> {
    let m = match method.to_ascii_uppercase().as_str() {
        "GET" => ByteMethod::Get,
        "POST" => ByteMethod::Post,
        _ => return Err(LocalRefusal::method("GET, POST")),
    };
    let t = target_of(path)?;
    let rest = t.rest();
    let first = rest.split('/').next().unwrap_or_default();
    if first == "auth" {
        return Err(LocalRefusal::new(
            403,
            "forbidden",
            "auth routes are not proxied",
        ));
    }
    if rest == "gateway/ticket" || rest.starts_with("gateway/ticket/") {
        return Err(LocalRefusal::new(
            403,
            "forbidden",
            "the gateway's ticket stays in the app: use gateway_open",
        ));
    }
    if rest == "gateway" {
        return Err(LocalRefusal::new(
            426,
            "upgrade_required",
            "the gateway is a WebSocket: use gateway_open",
        ));
    }
    if byte_route_of(rest).is_some() {
        return Err(LocalRefusal::not_found(
            "a byte route: load it through the sewfile: scheme, or post it with upload_bytes",
        ));
    }
    if m == ByteMethod::Post && raw_body_route(rest) {
        return Err(LocalRefusal::new(
            415,
            "unsupported_media_type",
            "this route takes raw bytes: use upload_bytes",
        ));
    }
    Ok((m, t))
}

/// Checks a `sewfile:` request against the byte-route allowlist: `GET` or `HEAD` of a listed
/// route (`405` with `Allow` for another method), never under [`NEVER_BYTES`], a signed
/// route with its `exp` and `sig`.
pub fn byte_target(method: &str, path: &str) -> Result<(ByteMethod, ApiTarget), LocalRefusal> {
    let m = match method.to_ascii_uppercase().as_str() {
        "GET" => ByteMethod::Get,
        "HEAD" => ByteMethod::Head,
        _ => return Err(LocalRefusal::method("GET, HEAD")),
    };
    let t = target_of(path)?;
    let rest = t.rest();
    if NEVER_BYTES.iter().any(|p| rest.starts_with(p)) {
        return Err(LocalRefusal::not_found("not found"));
    }
    let Some(r) = byte_route_of(rest) else {
        return Err(LocalRefusal::not_found("not found"));
    };
    if !r.methods.contains(&m.as_str()) {
        return Err(LocalRefusal::method(if r.methods.contains(&"HEAD") {
            "GET, HEAD"
        } else {
            "GET"
        }));
    }
    if r.signed {
        let q = t.query.trim_start_matches('?');
        let has = |k: &str| {
            q.split('&').any(|kv| {
                kv.split_once('=')
                    .is_some_and(|(n, v)| n == k && !v.is_empty())
            })
        };
        if !(has("exp") && has("sig")) {
            return Err(LocalRefusal::not_found("not found"));
        }
    }
    Ok((m, t))
}

/// Checks an upload's route: `uploads/{id}/data`, `me/avatar`, `spaces/{id}/icon` or
/// `spaces/{id}/emoji`, and its cap.
pub fn upload_target(path: &str) -> Result<(ApiTarget, usize), LocalRefusal> {
    let t = target_of(path)?;
    let rest = t.rest();
    if pattern_matches("uploads/{}/data", rest) || raw_body_route(rest) {
        let cap = body_cap(rest);
        Ok((t, cap))
    } else {
        Err(LocalRefusal::not_found(
            "uploads go to uploads/{id}/data, me/avatar, spaces/{id}/icon or spaces/{id}/emoji",
        ))
    }
}

/// Whether a raw body's type is one the lane takes: `application/octet-stream` or `image/*`.
pub fn raw_type_allowed(content_type: &str) -> bool {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    base == "application/octet-stream"
        || base.strip_prefix("image/").is_some_and(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"+.-".contains(&b))
        })
}

/// Reads a `sewfile:` request: the URI's path is `/<account>/<path under /api/v1>`, either as
/// it is (`sewfile://localhost/<account>/files/9?exp=…&sig=…`) or as one percent-encoded
/// segment (Tauri's `convertFileSrc("<account>/files/9?exp=…", "sewfile")`). Answers the
/// account (an account id or `active`) and `/<path>?<query>`.
pub fn parse_file_request(path: &str, query: Option<&str>) -> Option<(String, String)> {
    let p = path.strip_prefix('/')?;
    let whole = if p.contains('/') {
        p.to_owned()
    } else {
        percent_decode_bytes(p)?
    };
    let (account, rest) = whole.split_once('/')?;
    let ok_account = !account.is_empty()
        && account.len() <= 128
        && account
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok_account || rest.is_empty() {
        return None;
    }
    let mut api = format!("/{rest}");
    if let Some(q) = query.filter(|q| !q.is_empty())
        && !api.contains('?')
    {
        api.push('?');
        api.push_str(q);
    }
    Some((account.to_owned(), api))
}

/// Whether a page at `url` may load the `sewfile:` scheme: the bundled UI
/// (`tauri://localhost`, `http(s)://tauri.localhost`) or one of `extra` (an origin, such as
/// a development server's). A server's live site in the same webview is not.
pub fn file_caller_allowed(url: &Url, extra: &[String]) -> bool {
    crate::scope::is_bundled(url, extra)
}

/// An `api` call: `{method, path, body?, idempotency_key?}`. `path` is the route under
/// `/api/v1` with its query (`/channels/5/messages?limit=50`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiCall {
    /// `GET` or `POST`.
    pub method: String,
    /// The path under `/api/v1`, with its query.
    pub path: String,
    /// The JSON body of a `POST` (`{}` when absent).
    #[serde(default)]
    pub body: Option<Value>,
    /// `Idempotency-Key` of a `POST` (1 to 255 visible ASCII characters).
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// An `api` or upload answer: the status, the safe headers (lower-case names) and the body
/// (JSON when the answer is JSON, else its text; `null` when empty).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiReply {
    /// The status.
    pub status: u16,
    /// The safe headers.
    pub headers: BTreeMap<String, String>,
    /// The body.
    pub body: Value,
}

/// A `sewfile:` answer: the status, Sew's headers (less the dropped ones) and the bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct ByteReply {
    /// The status.
    pub status: u16,
    /// The headers, lower-case names.
    pub headers: Vec<(String, String)>,
    /// The bytes.
    pub body: Vec<u8>,
}

impl ByteReply {
    /// The first header named `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

impl fmt::Debug for ByteReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ByteReply")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// What an upload sends.
#[derive(Clone)]
pub enum UploadData {
    /// These bytes.
    Bytes(Vec<u8>),
    /// The file at this path, read in Rust (it never passes through the page). A link is
    /// not followed: the path must name a regular file.
    File(PathBuf),
}

impl fmt::Debug for UploadData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UploadData::Bytes(b) => write!(f, "Bytes({} bytes)", b.len()),
            UploadData::File(p) => f.debug_tuple("File").field(p).finish(),
        }
    }
}

/// An upload: raw bytes to `path` (`/uploads/{id}/data`, `/me/avatar`, `/spaces/{id}/icon`,
/// `/spaces/{id}/emoji`).
#[derive(Debug, Clone)]
pub struct Upload {
    /// The route under `/api/v1`.
    pub path: String,
    /// `application/octet-stream` (the default) or `image/*`.
    pub content_type: Option<String>,
    /// A part of a resumable upload: `bytes <first>-<last>/<total>`.
    pub content_range: Option<String>,
    /// The bytes.
    pub data: UploadData,
}

/// A file the person picked in the file dialog, as the page sees it: a handle for
/// `upload_file`, its name and size. Never its path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickedFile {
    /// The handle `upload_file` takes (`file`), good for this session.
    pub handle: String,
    /// The file's name (its last path part).
    pub name: String,
    /// Its size in bytes when picked.
    pub size: u64,
}

/// A picked file, held in Rust.
pub(crate) struct Picked {
    path: PathBuf,
    /// The page that may use it (`None`: any page with `sew-app:upload-file`).
    #[cfg_attr(not(feature = "tauri"), allow(dead_code))]
    owner: Option<String>,
    /// The file it was when picked (device and inode, on Unix).
    id: Option<(u64, u64)>,
}

/// The one answer to a handle or file that can't be used: it says nothing about whether a
/// file exists.
fn file_error() -> AppError {
    AppError::with_detail(code::FILE, "the file can't be used")
}

#[cfg(unix)]
fn file_id(m: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn file_id(_m: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// Reads the regular file at `path` without following a link at its end, up to `cap` bytes
/// (`Ok(None)` when larger). With `want`, the opened file must be the one picked.
fn read_regular(path: &Path, cap: usize, want: Option<(u64, u64)>) -> Result<Option<Vec<u8>>, ()> {
    use std::io::Read as _;
    let link = std::fs::symlink_metadata(path).map_err(drop)?;
    if !link.file_type().is_file() {
        return Err(());
    }
    if link.len() > cap as u64 {
        return Ok(None);
    }
    let f = std::fs::File::open(path).map_err(drop)?;
    let opened = f.metadata().map_err(drop)?;
    if !opened.is_file() || file_id(&opened) != file_id(&link) {
        return Err(());
    }
    if want.is_some() && file_id(&opened) != want {
        return Err(());
    }
    let mut buf = Vec::new();
    f.take(cap as u64 + 1).read_to_end(&mut buf).map_err(drop)?;
    if buf.len() > cap {
        return Ok(None);
    }
    Ok(Some(buf))
}

fn new_handle() -> Result<String, AppError> {
    use base64::Engine as _;
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|_| file_error())?;
    Ok(format!(
        "sf_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    ))
}

fn is_header_value(v: &str, max: usize) -> bool {
    !v.is_empty() && v.len() <= max && v.bytes().all(|b| (0x20..0x7f).contains(&b))
}

fn is_idempotency_key(k: &str) -> bool {
    (1..=255).contains(&k.len()) && k.bytes().all(|b| (0x21..0x7f).contains(&b))
}

fn is_content_range(v: &str) -> bool {
    let Some(rest) = v.strip_prefix("bytes ") else {
        return false;
    };
    let Some((span, total)) = rest.split_once('/') else {
        return false;
    };
    let Some((a, b)) = span.split_once('-') else {
        return false;
    };
    let num = |s: &str| !s.is_empty() && s.len() <= 20 && s.bytes().all(|c| c.is_ascii_digit());
    num(a) && num(b) && (num(total) || total == "*")
}

/// Whether `hay` holds `needle`.
pub(crate) fn holds(hay: &[u8], needle: &str) -> bool {
    let n = needle.as_bytes();
    !n.is_empty() && hay.len() >= n.len() && hay.windows(n.len()).any(|w| w == n)
}

/// The `detail.reason` of a lane-written `401` that ends the device session (`signed_out`,
/// or `unauthenticated`: the bearer isn't a session's).
fn ended_session(resp: &ByteResponse) -> Option<String> {
    if resp.status != 401 {
        return None;
    }
    let v: Value = serde_json::from_slice(&resp.body).ok()?;
    match v.get("code").and_then(Value::as_str)? {
        "signed_out" => Some(
            v["detail"]["reason"]
                .as_str()
                .unwrap_or("signed_out")
                .to_owned(),
        ),
        "unauthenticated" => Some("unauthenticated".to_owned()),
        _ => None,
    }
}

fn body_value(content_type: Option<&str>, body: &[u8]) -> Value {
    if body.is_empty() {
        return Value::Null;
    }
    let json = content_type.is_some_and(|c| c.to_ascii_lowercase().contains("json"));
    if json && let Ok(v) = serde_json::from_slice(body) {
        return v;
    }
    Value::String(String::from_utf8_lossy(body).into_owned())
}

fn api_headers(resp: &ByteResponse) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in &resp.headers {
        if (API_RESPONSE_HEADERS.contains(&k.as_str()) || k.starts_with("x-ratelimit-"))
            && !out.contains_key(k)
        {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

fn byte_headers(resp: &ByteResponse, bearer: &Secret) -> Vec<(String, String)> {
    let listed: Vec<String> = resp
        .headers
        .iter()
        .filter(|(k, _)| k == "connection")
        .flat_map(|(_, v)| v.split(',').map(|t| t.trim().to_ascii_lowercase()))
        .collect();
    resp.headers
        .iter()
        .filter(|(k, v)| {
            !DROPPED_RESPONSE_HEADERS.contains(&k.as_str())
                && !listed.contains(k)
                && !holds(v.as_bytes(), bearer.expose())
        })
        .cloned()
        .collect()
}

impl AppService {
    /// The account's bearer, or the lane's `401 signed_out` when its session is gone.
    async fn bearer_or_refusal(
        &self,
        key: &AccountKey,
    ) -> Result<Result<Secret, LocalRefusal>, AppError> {
        match self.bearer(key).await {
            Ok(b) => Ok(Ok(b)),
            Err(e) if e.code == code::SIGNED_OUT => {
                let reason = e.signed_out_reason().unwrap_or("signed_out").to_owned();
                Ok(Err(LocalRefusal::signed_out(&reason)))
            }
            Err(e) => Err(e),
        }
    }

    /// Sends `req` on `key`'s server; a lane `401` that ends the session marks the account.
    async fn exchange(&self, key: &AccountKey, req: ByteRequest) -> Result<ByteResponse, AppError> {
        let resp = self
            .lane()
            .send_bytes(&key.server, req)
            .await
            .map_err(|e| AppError::with_detail(code::UNREACHABLE, e.0))?;
        if let Some(reason) = ended_session(&resp) {
            self.session_ended(key, &reason).await;
        }
        Ok(resp)
    }

    fn api_reply(resp: &ByteResponse, bearer: &Secret) -> Result<ApiReply, AppError> {
        // A lane answer never holds the bearer; if one ever did, the page doesn't get it.
        if holds(&resp.body, bearer.expose()) {
            return Err(AppError::with_detail(
                code::BAD_ANSWER,
                "the answer held the device's credential",
            ));
        }
        let headers = api_headers(resp);
        let body = body_value(headers.get("content-type").map(String::as_str), &resp.body);
        Ok(ApiReply {
            status: resp.status,
            headers,
            body,
        })
    }

    /// `api`: one call of the App's client API with the account's bearer (the active account
    /// when `key` is `None`). The plugin's refusals and the server's answers come back alike
    /// (`{status, headers, body}`); only an unreachable server, an unknown account or a bad
    /// argument is an error.
    pub async fn api(&self, key: Option<AccountKey>, call: ApiCall) -> Result<ApiReply, AppError> {
        let key = self.account_or_active(key)?;
        let (method, target) = match api_target(&call.method, &call.path) {
            Ok(t) => t,
            Err(r) => return Ok(r.reply()),
        };
        let mut headers = vec![("accept".to_owned(), "application/json".to_owned())];
        let body = match method {
            ByteMethod::Post => {
                let b = serde_json::to_vec(&call.body.unwrap_or_else(|| json!({})))
                    .map_err(|_| AppError::invalid_args("body"))?;
                let cap = body_cap(target.rest());
                if b.len() > cap {
                    return Ok(LocalRefusal::too_large(cap).reply());
                }
                headers.push(("content-type".to_owned(), "application/json".to_owned()));
                Some(b)
            }
            _ => {
                if call.body.as_ref().is_some_and(|b| !b.is_null()) {
                    return Err(AppError::invalid_args("a GET has no body"));
                }
                None
            }
        };
        if let Some(k) = call.idempotency_key {
            if method != ByteMethod::Post || !is_idempotency_key(&k) {
                return Err(AppError::invalid_args(
                    "idempotency_key: a POST's, 1 to 255 visible ASCII characters",
                ));
            }
            headers.push(("idempotency-key".to_owned(), k));
        }
        let bearer = match self.bearer_or_refusal(&key).await? {
            Ok(b) => b,
            Err(r) => return Ok(r.reply()),
        };
        let req = ByteRequest {
            method,
            path: target.lane_path(),
            bearer: Some(bearer.clone()),
            headers,
            body,
            max_answer: API_MAX_ANSWER,
            timeout: API_TIMEOUT,
        };
        let resp = self.exchange(&key, req).await?;
        AppService::api_reply(&resp, &bearer)
    }

    /// An upload: raw bytes (or a file read here) to an upload's data or a raw-body route,
    /// with the account's bearer. Answers like [`AppService::api`].
    pub async fn upload(&self, key: Option<AccountKey>, up: Upload) -> Result<ApiReply, AppError> {
        let key = self.account_or_active(key)?;
        let (target, cap) = match upload_target(&up.path) {
            Ok(t) => t,
            Err(r) => return Ok(r.reply()),
        };
        let ct = up
            .content_type
            .unwrap_or_else(|| "application/octet-stream".to_owned());
        if !is_header_value(&ct, 128) || !raw_type_allowed(&ct) {
            return Ok(LocalRefusal::new(
                415,
                "unsupported_media_type",
                "content type not allowed",
            )
            .reply());
        }
        let mut headers = vec![
            ("accept".to_owned(), "application/json".to_owned()),
            ("content-type".to_owned(), ct),
        ];
        if let Some(r) = up.content_range {
            if !is_content_range(&r) {
                return Err(AppError::invalid_args(
                    "content_range: bytes <first>-<last>/<total>",
                ));
            }
            headers.push(("content-range".to_owned(), r));
        }
        let bytes = match up.data {
            UploadData::Bytes(b) => b,
            UploadData::File(path) => {
                let want = self.picked_id(&path);
                let read = tokio::task::spawn_blocking(move || read_regular(&path, cap, want))
                    .await
                    .map_err(|_| file_error())?;
                match read {
                    Ok(Some(b)) => b,
                    Ok(None) => return Ok(LocalRefusal::too_large(cap).reply()),
                    Err(()) => return Err(file_error()),
                }
            }
        };
        if bytes.len() > cap {
            return Ok(LocalRefusal::too_large(cap).reply());
        }
        let bearer = match self.bearer_or_refusal(&key).await? {
            Ok(b) => b,
            Err(r) => return Ok(r.reply()),
        };
        let req = ByteRequest {
            method: ByteMethod::Post,
            path: target.lane_path(),
            bearer: Some(bearer.clone()),
            headers,
            body: Some(bytes),
            max_answer: API_MAX_ANSWER,
            timeout: BYTES_TIMEOUT,
        };
        let resp = self.exchange(&key, req).await?;
        AppService::api_reply(&resp, &bearer)
    }

    /// A handle for the file at `path`, which the person picked in the host's own file
    /// dialog: `upload_file` takes it from any page with `sew-app:upload-file`. The plugin's
    /// `pick_files` gives handles that only the page that asked may use. A path that is not a
    /// regular file is refused (`file`, one answer for every case).
    pub fn picked_file(&self, path: &Path) -> Result<PickedFile, AppError> {
        self.picked_file_for(path, None)
    }

    /// A handle for a picked file that only `owner` (a page) may use.
    pub(crate) fn picked_file_for(
        &self,
        path: &Path,
        owner: Option<String>,
    ) -> Result<PickedFile, AppError> {
        // The person picked this file: a link they picked is resolved once, here.
        let path = std::fs::canonicalize(path).map_err(|_| file_error())?;
        let m = std::fs::symlink_metadata(&path).map_err(|_| file_error())?;
        if !m.file_type().is_file() {
            return Err(file_error());
        }
        let handle = new_handle()?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.picked().insert(
            handle.clone(),
            Picked {
                path,
                owner,
                id: file_id(&m),
            },
        );
        Ok(PickedFile {
            handle,
            name,
            size: m.len(),
        })
    }

    /// The picked file's path for `handle`, when `owner` may use it (`file` otherwise, the
    /// same answer as an unreadable file).
    #[cfg_attr(not(feature = "tauri"), allow(dead_code))]
    pub(crate) fn picked_path(
        &self,
        handle: &str,
        owner: Option<&str>,
    ) -> Result<PathBuf, AppError> {
        let picked = self.picked();
        let p = picked.get(handle).ok_or_else(file_error)?;
        if let (Some(o), Some(w)) = (&p.owner, owner)
            && o != w
        {
            return Err(file_error());
        }
        Ok(p.path.clone())
    }

    /// The file a picked `path` was when picked, if it was picked.
    fn picked_id(&self, path: &Path) -> Option<(u64, u64)> {
        self.picked()
            .values()
            .find(|p| p.path == path)
            .and_then(|p| p.id)
    }

    /// The account a `sewfile:` URL names: an account id, or `active`.
    pub fn file_account(&self, account: &str) -> Option<AccountKey> {
        if account == "active" {
            return self.active();
        }
        self.book()
            .accounts()
            .map(|(k, _)| k)
            .find(|k| k.account == account)
    }

    /// A signed byte route for the `sewfile:` scheme: `method` (`GET`, `HEAD`) of `path` (the
    /// route under `/api/v1` with its query) for `account` (an id or `active`), with the
    /// webview's `Range`, `If-Range`, `If-None-Match` and `If-Modified-Since`. Never fails:
    /// a refusal is an answer in the lane's shape.
    pub async fn fetch_bytes(
        &self,
        account: &str,
        method: &str,
        path: &str,
        headers: &[(String, String)],
    ) -> ByteReply {
        let Some(key) = self.file_account(account) else {
            return LocalRefusal::new(404, code::UNKNOWN_ACCOUNT, "no such account").bytes();
        };
        let (m, target) = match byte_target(method, path) {
            Ok(t) => t,
            Err(r) => return r.bytes(),
        };
        let pass: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .filter(|(k, v)| BYTE_REQUEST_HEADERS.contains(&k.as_str()) && is_header_value(v, 256))
            .collect();
        let bearer = match self.bearer_or_refusal(&key).await {
            Ok(Ok(b)) => b,
            Ok(Err(r)) => return r.bytes(),
            Err(e) => {
                return LocalRefusal::new(503, &e.code, "the device session can't be read").bytes();
            }
        };
        let req = ByteRequest {
            method: m,
            path: target.lane_path(),
            bearer: Some(bearer.clone()),
            headers: pass,
            body: None,
            max_answer: BYTES_MAX_ANSWER,
            timeout: BYTES_TIMEOUT,
        };
        match self.exchange(&key, req).await {
            Ok(resp) => ByteReply {
                status: resp.status,
                headers: byte_headers(&resp, &bearer),
                body: if m == ByteMethod::Head {
                    Vec::new()
                } else {
                    resp.body
                },
            },
            Err(e) => LocalRefusal::new(502, &e.code, "the server could not be reached").bytes(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::lane::route;

    #[test]
    fn the_api_allowlist_mirrors_the_lanes() {
        let ok = |m: &str, p: &str| api_target(m, p).unwrap().1.lane_path();
        assert_eq!(ok("GET", "/me"), "/sew/native/api/v1/me");
        assert_eq!(
            ok("post", "/channels/5/messages"),
            "/sew/native/api/v1/channels/5/messages"
        );
        assert_eq!(
            ok("GET", "/channels/5/messages?limit=50&token=x&before=9"),
            "/sew/native/api/v1/channels/5/messages?limit=50&before=9"
        );
        assert_eq!(ok("GET", "/search?token=x"), "/sew/native/api/v1/search");
        let refused = |m: &str, p: &str| {
            let r = api_target(m, p).unwrap_err();
            (r.status, r.code)
        };
        assert_eq!(refused("GET", "/auth/me"), (403, "forbidden".into()));
        assert_eq!(refused("POST", "/auth/logout"), (403, "forbidden".into()));
        assert_eq!(
            refused("POST", "/gateway/ticket"),
            (403, "forbidden".into())
        );
        assert_eq!(refused("GET", "/gateway"), (426, "upgrade_required".into()));
        assert_eq!(refused("DELETE", "/me"), (405, "method_not_allowed".into()));
        assert_eq!(refused("HEAD", "/me"), (405, "method_not_allowed".into()));
        for p in [
            "me",
            "/",
            "//me",
            "/a//b",
            "/a/./b",
            "/a/../b",
            "/a/%2e%2e/b",
            "/a%2Fb",
            "/a%5cb",
            "/a\\b",
            "/a b",
            "/me?x=1#f",
            "/m\u{e9}",
        ] {
            assert_eq!(refused("GET", p), (404, "not_found".into()), "{p}");
        }
        // Byte routes and raw bodies have their own doors.
        assert_eq!(
            refused("GET", "/files/9?exp=1&sig=2"),
            (404, "not_found".into())
        );
        assert_eq!(
            refused("POST", "/uploads/9/data"),
            (404, "not_found".into())
        );
        assert_eq!(
            refused("POST", "/me/avatar"),
            (415, "unsupported_media_type".into())
        );
        assert_eq!(
            refused("POST", "/spaces/3/emoji"),
            (415, "unsupported_media_type".into())
        );
        // A route that only looks like one is the API's.
        ok("POST", "/uploads");
        ok("GET", "/me/avatar");
    }

    #[test]
    fn the_byte_allowlist_mirrors_the_lanes() {
        let ok = |m: &str, p: &str| byte_target(m, p).unwrap().1.lane_path();
        assert_eq!(
            ok("GET", "/files/9?exp=1&sig=abc"),
            "/sew/native/api/v1/files/9?exp=1&sig=abc"
        );
        ok("HEAD", "/files/9/thumb");
        ok("GET", "/avatars/1/2/64");
        ok("GET", "/emoji-images/7/static");
        ok("GET", "/space-icons/1/2/64");
        ok("GET", "/gifs/media/tok_1");
        ok("GET", "/previews/5/image");
        ok("GET", "/uploads/5/data");
        let refused = |m: &str, p: &str| byte_target(m, p).unwrap_err();
        assert_eq!(refused("GET", "/files/9").status, 404, "unsigned");
        assert_eq!(refused("GET", "/files/9?exp=1&sig=").status, 404);
        assert_eq!(refused("GET", "/me").status, 404);
        assert_eq!(refused("GET", "/recordings/5/audio").status, 404);
        assert_eq!(refused("GET", "/compliance/x").status, 404);
        assert_eq!(refused("GET", "/files/../thumb").status, 404);
        let r = refused("HEAD", "/avatars/1/2/64");
        assert_eq!((r.status, r.allow), (405, Some("GET")));
        let r = refused("POST", "/uploads/5/data");
        assert_eq!((r.status, r.allow), (405, Some("GET, HEAD")));
        // `token=` never reaches the server.
        assert_eq!(
            ok("GET", "/files/9?token=x&exp=1&sig=2"),
            "/sew/native/api/v1/files/9?exp=1&sig=2"
        );
    }

    #[test]
    fn uploads_and_caps() {
        assert_eq!(
            upload_target("/uploads/9/data").unwrap().1,
            caps::UPLOAD_DATA
        );
        assert_eq!(upload_target("/me/avatar").unwrap().1, caps::IMAGE);
        assert_eq!(upload_target("/spaces/2/icon").unwrap().1, caps::IMAGE);
        assert_eq!(upload_target("/spaces/2/emoji").unwrap().1, caps::EMOJI);
        assert_eq!(
            upload_target("/channels/5/messages").unwrap_err().status,
            404
        );
        assert_eq!(body_cap("channels/5/messages"), caps::MESSAGE);
        assert_eq!(body_cap("messages/5/edit"), caps::MESSAGE);
        assert_eq!(body_cap("me"), caps::DEFAULT);
        assert!(raw_type_allowed("application/octet-stream"));
        assert!(raw_type_allowed("image/png"));
        assert!(raw_type_allowed("image/svg+xml; charset=utf-8"));
        assert!(!raw_type_allowed("application/json"));
        assert!(!raw_type_allowed("image/"));
        assert!(is_content_range("bytes 0-99/1000"));
        assert!(is_content_range("bytes 0-99/*"));
        assert!(!is_content_range("bytes 0-/1000"));
        assert!(!is_content_range("0-99/1000"));
        assert!(is_idempotency_key("k-1"));
        assert!(!is_idempotency_key("k 1"));
        assert!(!is_idempotency_key(""));
    }

    #[test]
    fn a_local_refusal_has_the_lanes_shape() {
        let r = api_target("POST", "/auth/logout").unwrap_err().reply();
        assert_eq!(r.status, 403);
        assert_eq!(r.body["ok"], false);
        assert_eq!(r.body["code"], "forbidden");
        assert_eq!(r.body["detail"], Value::Null);
        assert_eq!(r.body["error"]["code"], "forbidden");
        assert!(r.body["error"].get("details").is_none());
        let s = LocalRefusal::signed_out("revoked").reply();
        assert_eq!(s.status, 401);
        assert_eq!(s.body["detail"]["reason"], "revoked");
        assert_eq!(s.body["error"]["details"]["reason"], "revoked");
        let m = api_target("PUT", "/me").unwrap_err().reply();
        assert_eq!(m.headers["allow"], "GET, POST");
    }

    #[test]
    fn file_requests_read_both_forms() {
        assert_eq!(
            parse_file_request("/acct_1/files/9", Some("exp=1&sig=2")).unwrap(),
            ("acct_1".into(), "/files/9?exp=1&sig=2".into())
        );
        assert_eq!(
            parse_file_request("/active%2Ffiles%2F9%3Fexp%3D1%26sig%3D2", None).unwrap(),
            ("active".into(), "/files/9?exp=1&sig=2".into())
        );
        assert!(parse_file_request("/acct", None).is_none());
        assert!(parse_file_request("/a.b/files/9", None).is_none());
        assert!(parse_file_request("acct/files/9", None).is_none());
        // A decoded dot segment still meets the allowlist.
        let (_, p) = parse_file_request("/a%2Ffiles%2F..%2Fme", None).unwrap();
        assert!(byte_target("GET", &p).is_err());
    }

    #[test]
    fn only_the_bundled_ui_loads_files() {
        let u = |s: &str| Url::parse(s).unwrap();
        assert!(file_caller_allowed(&u("tauri://localhost/index.html"), &[]));
        assert!(file_caller_allowed(&u("http://tauri.localhost/"), &[]));
        assert!(file_caller_allowed(&u("https://tauri.localhost/a"), &[]));
        assert!(!file_caller_allowed(&u("https://app.example.com/"), &[]));
        assert!(!file_caller_allowed(&u("http://localhost:1420/"), &[]));
        assert!(file_caller_allowed(
            &u("http://localhost:1420/x"),
            &["http://localhost:1420".into()]
        ));
    }

    #[test]
    fn route_constants_agree() {
        assert_eq!(route::API, "/sew/native/api/v1");
        assert!(route::GATEWAY.starts_with(route::API));
    }

    fn service() -> AppService {
        let mut cfg = crate::service::AppConfig::new("com.example.picked");
        cfg.secrets = crate::secrets::SecretChoice::Memory;
        AppService::new(
            cfg,
            crate::lane::LaneClient::new(std::sync::Arc::new(crate::fake::FakeLane::new())),
            std::sync::Arc::new(crate::service::NoEvents),
        )
        .unwrap()
    }

    #[test]
    fn a_picked_file_answers_only_its_page_and_never_a_link() {
        let svc = service();
        let dir = std::env::temp_dir().join(format!("sew-app-picked-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.bin");
        std::fs::write(&f, b"abc").unwrap();
        let p = svc
            .picked_file_for(&f, Some("main https://a.example.com".into()))
            .unwrap();
        assert_eq!((p.name.as_str(), p.size), ("a.bin", 3));
        // Its own page gets the path; another page, or a made-up handle, the one answer.
        assert!(
            svc.picked_path(&p.handle, Some("main https://a.example.com"))
                .is_ok()
        );
        let other = svc
            .picked_path(&p.handle, Some("main https://b.example.com"))
            .unwrap_err();
        let unknown = svc.picked_path("sf_x", None).unwrap_err();
        assert_eq!(other, file_error());
        assert_eq!(unknown, file_error());
        // A directory can't be picked; neither can a missing file: the same answer.
        assert_eq!(svc.picked_file(&dir).unwrap_err(), file_error());
        assert_eq!(
            svc.picked_file(&dir.join("missing")).unwrap_err(),
            file_error()
        );
        // The file read is the one picked, and a link at the path is not followed.
        let path = svc.picked_path(&p.handle, None).unwrap();
        assert_eq!(
            read_regular(&path, 16, file_id(&std::fs::metadata(&path).unwrap())).unwrap(),
            Some(b"abc".to_vec())
        );
        assert_eq!(read_regular(&path, 2, None).unwrap(), None);
        #[cfg(unix)]
        {
            let secret = dir.join("secret.bin");
            std::fs::write(&secret, b"not picked").unwrap();
            let link = dir.join("link.bin");
            std::os::unix::fs::symlink(&secret, &link).unwrap();
            assert!(read_regular(&link, 64, None).is_err());
            // The picked file swapped for a link to another: refused.
            let want = file_id(&std::fs::metadata(&path).unwrap());
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&secret, &path).unwrap();
            assert!(read_regular(&path, 64, want).is_err());
            // ... and for another regular file: refused too.
            std::fs::remove_file(&path).unwrap();
            std::fs::write(&path, b"swapped").unwrap();
            let fresh = file_id(&std::fs::metadata(&path).unwrap());
            if fresh != want {
                assert!(read_regular(&path, 64, want).is_err());
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
