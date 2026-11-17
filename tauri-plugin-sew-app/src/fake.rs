//! The fake mode: an App server's native lane in memory ([`FakeLane`]), and recorders for the
//! core's events and the voice plugin's `forget_server`. For tests and UI work without a
//! server; the example app runs on it when no server is configured.
//!
//! The fake answers lane 1 as the App server's documentation describes it: device sessions
//! keyed by person and install device id (signing in again with the same id replaces the
//! session, reason `replaced`), at most [`FakeLane::MAX_DEVICES`] per person, `401
//! signed_out` with `detail.reason` for an ended bearer, single-use pairing and handoff codes,
//! the voice join and consent routes with a call credential, and the host's sign-in door on
//! the same address (`/.k2/auth/ping`, `sign-in` and `set-password`, with the lane's
//! `signin/door/begin` and `finish`). It never sends anything anywhere.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use base64::Engine as _;
use serde_json::{Value, json};
use tauri_plugin_sew_voice::BoxFuture;

use tokio::sync::mpsc;
use url::Url;

use crate::error::AppError;
use crate::gateway::{ConnectError, SocketConnector, SocketPipe, WsIn, WsOut};
use crate::lane::{
    ByteMethod, ByteRequest, ByteResponse, LaneInfo, LaneRequest, LaneResponse, Method, Secret,
    ShellInfo, SigninOffer, Transport, TransportError,
};
use crate::origin::Origin;
use crate::service::{AppEvent, AppEventSink, VoiceForget};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn random(n: usize) -> String {
    let mut b = vec![0u8; n];
    if getrandom::fill(&mut b).is_err() {
        // Tests only: a fixed fill keeps the fake answering.
        b.iter_mut().enumerate().for_each(|(i, x)| *x = i as u8);
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// One request the fake got (the body's `password` and `code` are kept: tests check them).
#[derive(Debug, Clone, PartialEq)]
pub struct SeenRequest {
    /// The origin.
    pub origin: String,
    /// `GET` or `POST`.
    pub method: &'static str,
    /// The path.
    pub path: String,
    /// The bearer, when one came.
    pub bearer: Option<String>,
    /// The JSON body.
    pub body: Option<Value>,
}

#[derive(Debug, Clone)]
struct Session {
    id: String,
    member_id: String,
    device_id: String,
    name: String,
    live: bool,
    reason: Option<String>,
}

#[derive(Debug, Clone)]
struct User {
    password: String,
    member_id: String,
    display_name: String,
    /// The host door: locked (`423 locked`).
    locked: bool,
    /// The host door: `403 password_change_required` with this `changeToken`.
    change_token: Option<String>,
    /// The host door: whether the person may use this App (a set-password makes a code).
    app_access: bool,
}

impl User {
    fn new(password: &str, member_id: &str, display_name: &str) -> User {
        User {
            password: password.into(),
            member_id: member_id.into(),
            display_name: display_name.into(),
            locked: false,
            change_token: None,
            app_access: true,
        }
    }
}

/// An open door begin: the device it names and its challenge (a real lane keeps the PKCE
/// verifier here; the fake checks the challenge the code was made with instead).
#[derive(Debug, Clone)]
struct DoorBegin {
    device_id: Option<String>,
    challenge: String,
}

#[derive(Debug)]
struct State {
    info: LaneInfo,
    install_id: u64,
    users: BTreeMap<String, User>,
    /// bearer → session
    sessions: BTreeMap<String, Session>,
    pairing: BTreeMap<String, String>,
    handoffs: BTreeMap<String, String>,
    /// begin id → begin
    doors: BTreeMap<String, DoorBegin>,
    /// the host door's code → (challenge, member)
    door_codes: BTreeMap<String, (String, String)>,
    /// What the host's `/.k2/auth/ping` answers: `{"k2": true}` when true, else `404`.
    host_door: bool,
    recorded: BTreeMap<String, (u64, u32)>,
    consents: Vec<Value>,
    queued: VecDeque<(String, LaneResponse)>,
    seen: Vec<SeenRequest>,
    down: bool,
    origins: Option<Vec<String>>,
    /// gateway ticket → bearer
    tickets: BTreeMap<String, String>,
    issued_tickets: Vec<String>,
    seen_bytes: Vec<SeenBytes>,
    /// upload id → (bytes, content type)
    uploads: BTreeMap<String, (Vec<u8>, String)>,
    next_id: u64,
    /// Idempotency-Key → the first answer
    idempotent: BTreeMap<String, Value>,
    socks: Vec<FakeSock>,
    /// Every frame the gateway got, with its socket's number.
    frames: Vec<(usize, Value)>,
}

/// One request of the client API the fake got (its byte routes and uploads included).
#[derive(Debug, Clone, PartialEq)]
pub struct SeenBytes {
    /// `GET`, `HEAD` or `POST`.
    pub method: &'static str,
    /// The path and query.
    pub path: String,
    /// The bearer, when one came.
    pub bearer: Option<String>,
    /// The other headers.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Option<Vec<u8>>,
}

#[derive(Debug)]
struct FakeSock {
    bearer: String,
    to_client: mpsc::UnboundedSender<WsIn>,
    open: bool,
}

/// An App server's native lane in memory.
#[derive(Debug, Clone)]
pub struct FakeLane {
    state: Arc<Mutex<State>>,
}

impl Default for FakeLane {
    fn default() -> FakeLane {
        FakeLane::new()
    }
}

fn resp(status: u16, body: Value) -> LaneResponse {
    LaneResponse {
        status,
        retry_after_ms: None,
        body: serde_json::to_vec(&body).unwrap_or_default(),
    }
}

/// The lane's error shape.
pub fn lane_error(status: u16, code: &str, detail: Value, hint: &str) -> LaneResponse {
    let mut error = json!({ "code": code, "hint": hint });
    if !detail.is_null() {
        error["details"] = detail.clone();
    }
    resp(
        status,
        json!({ "ok": false, "code": code, "detail": detail, "error": error }),
    )
}

/// The host sign-in door's error shape: `{ok: false, error: {code, hint}}` (a `429` also says
/// `retryAfterSecs` and `Retry-After`: use [`k2_rate_limited`]).
pub fn k2_error(status: u16, code: &str, hint: &str) -> LaneResponse {
    resp(
        status,
        json!({ "ok": false, "error": { "code": code, "hint": hint } }),
    )
}

/// The host sign-in door's `429 rate_limited` with `Retry-After: <secs>`.
pub fn k2_rate_limited(secs: u64) -> LaneResponse {
    let mut r = resp(
        429,
        json!({ "ok": false, "error": { "code": "rate_limited", "hint": "Wait.",
                                         "retryAfterSecs": secs } }),
    );
    r.retry_after_ms = Some(secs.saturating_mul(1000));
    r
}

/// A Sew join answer for `member_id` in `room`, with a call credential, as the lane answers it
/// on `origin` (`url` the lane's own signalling, `api_base` the lane's call door).
pub fn join_answer(origin: &str, member_id: &str, room: &str) -> Value {
    json!({
        "join": {
            "url": format!("{}/sew/native", origin.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1)),
            "token": "fake-media-token",
            "expires_at": "2026-11-06T12:02:00.000Z",
            "room": "r_fakeroomfakeroomfakeroomfa",
            "state": {
                "member_id": member_id,
                "channel_id": room,
                "connection": "connecting",
                "secure": false,
                "self_mute": false,
                "self_deaf": false,
                "self_stream": false,
                "self_video": false,
                "server_deaf": false,
                "server_mute": false,
                "speak_denied": false,
                "joined_at": "2026-11-06T12:00:00.000Z"
            },
            "call_credential": {
                "token": "sewcc_fakefakefakefakefake.fakefakefakefakefakefakefakefakefakefakefak",
                "expires_at": "2026-11-06T12:30:00.000Z",
                "join_id": "0123456789abcdef0123456789abcdef",
                "device_scope": "fedcba9876543210fedcba9876543210"
            }
        },
        "api_base": format!("{origin}/sew/native/")
    })
}

/// Sew's `409 recording_consent_required` details for `recording_id` with notice
/// `notice_version` in channel `room`.
pub fn consent_details(recording_id: u64, notice_version: u32, room: &str) -> Value {
    json!({
        "reason": "recording_consent_required",
        "recording": null,
        "auto": false,
        "consent_prompt": {
            "recording_id": recording_id.to_string(),
            "notice_version": notice_version,
            "recorder_name": "Sew Notes",
            "feature_label": "Sew Notes",
            "started_by": {"member_id": "1001", "display_name": "Alice"},
            "room": {"channel_id": room, "kind": "voice", "name": "lounge"},
            "audio_keep_days": 30,
            "transcript": {"posted_to_channel": true, "readable_by": ["channel_members"], "e2ee": false},
            "admin_download": false,
            "decline_mode": "join_unrecorded",
            "recorded_now": ["1001"]
        }
    })
}

/// Sew's recording as the person sees it after a consent answer.
pub fn recording(recording_id: u64, room: &str, consent: bool) -> Value {
    json!({
        "id": recording_id.to_string(), "channel_id": room, "state": "recording",
        "started_by": "1001", "started_at": "2026-11-06T12:00:00Z", "auto": false,
        "message_id": "88", "mixed": false, "audio_keep_days": 30,
        "max_duration_ms": 14400000, "participants": [],
        "my_consent": if consent { "granted" } else { "declined" }
    })
}

impl FakeLane {
    /// Live device sessions per person.
    pub const MAX_DEVICES: usize = 10;

    /// The fake person's password (`bob`).
    pub const BOB_PASSWORD: &'static str = "bob-test-pw";

    /// A lane with password sign-in and pairing, the voice routes, and one person: `bob`,
    /// password [`FakeLane::BOB_PASSWORD`], member `1002`, display name `Bob`. Install
    /// `7001`. The host's sign-in door answers its ping, but the lane offers it only after
    /// `set_info(|i| i.signin.door = true)`.
    pub fn new() -> FakeLane {
        let mut users = BTreeMap::new();
        users.insert(
            "bob".to_owned(),
            User::new(FakeLane::BOB_PASSWORD, "1002", "Bob"),
        );
        FakeLane {
            state: Arc::new(Mutex::new(State {
                info: LaneInfo {
                    lane: 1,
                    identity: json!("standalone"),
                    signin: SigninOffer {
                        password: true,
                        pairing: true,
                        door: false,
                    },
                    voice: true,
                    signalling: Some("lane".into()),
                    call_door: Some("lane".into()),
                    recording: false,
                    shell: ShellInfo::default(),
                },
                install_id: 7001,
                users,
                sessions: BTreeMap::new(),
                pairing: BTreeMap::new(),
                handoffs: BTreeMap::new(),
                doors: BTreeMap::new(),
                door_codes: BTreeMap::new(),
                host_door: true,
                recorded: BTreeMap::new(),
                consents: Vec::new(),
                queued: VecDeque::new(),
                seen: Vec::new(),
                down: false,
                origins: None,
                tickets: BTreeMap::new(),
                issued_tickets: Vec::new(),
                seen_bytes: Vec::new(),
                uploads: BTreeMap::new(),
                next_id: 9000,
                idempotent: BTreeMap::new(),
                socks: Vec::new(),
                frames: Vec::new(),
            })),
        }
    }

    fn st(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Adds a person.
    pub fn add_user(&self, username: &str, password: &str, member_id: &str, display_name: &str) {
        self.st().users.insert(
            username.to_owned(),
            User::new(password, member_id, display_name),
        );
    }

    /// Changes what `info` answers.
    pub fn set_info(&self, f: impl FnOnce(&mut LaneInfo)) {
        f(&mut self.st().info);
    }

    /// Sets the Sew install id `whoami` answers.
    pub fn set_install_id(&self, id: u64) {
        self.st().install_id = id;
    }

    /// Answers only these origins (others: unreachable). Default: every origin.
    pub fn only_origins(&self, origins: &[&str]) {
        self.st().origins = Some(origins.iter().map(|s| (*s).to_owned()).collect());
    }

    /// Takes the fake off the network (every request: unreachable) or back.
    pub fn set_down(&self, down: bool) {
        self.st().down = down;
    }

    /// The next request to `path` is answered `resp` instead (once; queued in order).
    pub fn answer_next(&self, path: &str, r: LaneResponse) {
        self.st().queued.push_back((path.to_owned(), r));
    }

    /// Records room `room` as recorded: a join without an answer to notice `notice_version`
    /// is refused `409 recording_consent_required`.
    pub fn record_room(&self, room: &str, recording_id: u64, notice_version: u32) {
        self.st()
            .recorded
            .insert(room.to_owned(), (recording_id, notice_version));
    }

    /// A pairing code for `username`, as a signed-in page would show it.
    pub fn issue_pairing_code(&self, username: &str) -> Option<String> {
        let mut st = self.st();
        let member = st.users.get(username)?.member_id.clone();
        let code = random(32);
        st.pairing.insert(code.clone(), member);
        Some(code)
    }

    /// What the host's `/.k2/auth/ping` answers: `{"k2": true}` (the default) or a `404`.
    pub fn set_host_door(&self, answers: bool) {
        self.st().host_door = answers;
    }

    /// The host door locks `username` (`423 locked`) or unlocks them.
    pub fn lock_user(&self, username: &str, locked: bool) {
        if let Some(u) = self.st().users.get_mut(username) {
            u.locked = locked;
        }
    }

    /// The host door asks `username` for a first password (`403 password_change_required`
    /// with a `changeToken`); answers the token.
    pub fn require_password_change(&self, username: &str) -> Option<String> {
        let token = format!("k2chg_{}", random(24));
        self.st().users.get_mut(username)?.change_token = Some(token.clone());
        Some(token)
    }

    /// Whether `username` may use this App (a set-password without it makes no code).
    pub fn set_app_access(&self, username: &str, access: bool) {
        if let Some(u) = self.st().users.get_mut(username) {
            u.app_access = access;
        }
    }

    /// `username`'s password now (after a set-password).
    pub fn password_of(&self, username: &str) -> Option<String> {
        self.st().users.get(username).map(|u| u.password.clone())
    }

    /// The door begins still open (not yet finished).
    pub fn open_door_begins(&self) -> usize {
        self.st().doors.len()
    }

    /// Ends every live session of the device id `device_id` (an admin revoke from elsewhere).
    pub fn revoke_device_id(&self, device_id: &str, reason: &str) {
        let mut st = self.st();
        let mut ended = Vec::new();
        for (b, s) in st.sessions.iter_mut() {
            if s.device_id == device_id && s.live {
                s.live = false;
                s.reason = Some(reason.to_owned());
                ended.push(b.clone());
            }
        }
        for b in ended {
            FakeLane::close_socks_of(&mut st, &b, Some(4011), "session revoked");
        }
    }

    /// Closes the gateway sockets of `bearer` (the lane does when a session ends).
    fn close_socks_of(st: &mut State, bearer: &str, code: Option<u16>, reason: &str) {
        for s in st.socks.iter_mut().filter(|s| s.open && s.bearer == bearer) {
            s.open = false;
            let _ = s.to_client.send(WsIn::Closed {
                code,
                reason: reason.to_owned(),
            });
        }
    }

    /// Every device bearer the fake issued (tests check none reaches the page).
    pub fn bearers(&self) -> Vec<String> {
        self.st().sessions.keys().cloned().collect()
    }

    /// Every gateway ticket the fake issued.
    pub fn tickets(&self) -> Vec<String> {
        self.st().issued_tickets.clone()
    }

    /// Every client API request, in order.
    pub fn seen_bytes(&self) -> Vec<SeenBytes> {
        self.st().seen_bytes.clone()
    }

    /// An upload's bytes and content type, once its data came.
    pub fn uploaded(&self, id: &str) -> Option<(Vec<u8>, String)> {
        self.st().uploads.get(id).cloned()
    }

    /// Stores a file the byte routes serve (`files/{id}?exp&sig`, its `thumb`).
    pub fn put_file(&self, id: &str, bytes: &[u8], content_type: &str) {
        self.st()
            .uploads
            .insert(id.to_owned(), (bytes.to_vec(), content_type.to_owned()));
    }

    /// The gateway sockets open now.
    pub fn open_gateways(&self) -> usize {
        self.st().socks.iter().filter(|s| s.open).count()
    }

    /// Every frame the gateway got from the client, with its socket's number (from 0).
    pub fn gateway_frames(&self) -> Vec<(usize, Value)> {
        self.st().frames.clone()
    }

    /// Every open socket breaks without a close frame (a network drop).
    pub fn drop_gateways(&self) {
        let mut st = self.st();
        for s in st.socks.iter_mut().filter(|s| s.open) {
            s.open = false;
            let _ = s.to_client.send(WsIn::Closed {
                code: None,
                reason: String::new(),
            });
        }
    }

    /// Every open socket is closed by the server with `code` and `reason`.
    pub fn close_gateways(&self, code: u16, reason: &str) {
        let mut st = self.st();
        for s in st.socks.iter_mut().filter(|s| s.open) {
            s.open = false;
            let _ = s.to_client.send(WsIn::Closed {
                code: Some(code),
                reason: reason.to_owned(),
            });
        }
    }

    /// Sends a dispatch (`op 0`, event `t`, payload `d`) on every open socket.
    pub fn push_dispatch(&self, t: &str, d: Value) {
        let st = self.st();
        let text = json!({ "op": 0, "t": t, "s": 3, "d": d }).to_string();
        for s in st.socks.iter().filter(|s| s.open) {
            let _ = s.to_client.send(WsIn::Text(text.clone()));
        }
    }

    /// The live sessions: `(member_id, device_id)`.
    pub fn live_sessions(&self) -> Vec<(String, String)> {
        self.st()
            .sessions
            .values()
            .filter(|s| s.live)
            .map(|s| (s.member_id.clone(), s.device_id.clone()))
            .collect()
    }

    /// Every request, in order.
    pub fn seen(&self) -> Vec<SeenRequest> {
        self.st().seen.clone()
    }

    /// The consent answers, in order.
    pub fn consents(&self) -> Vec<Value> {
        self.st().consents.clone()
    }

    /// Whether a handoff code is still unused (the page has not redeemed it).
    pub fn handoff_unused(&self, code: &str) -> bool {
        self.st().handoffs.contains_key(code)
    }

    /// The page's redeem (the cookie lane): `Some(member_id)` once per code.
    pub fn redeem_handoff(&self, code: &str) -> Option<String> {
        let mut st = self.st();
        let bearer = st.handoffs.remove(code)?;
        let s = st.sessions.get(&bearer)?;
        s.live.then(|| s.member_id.clone())
    }

    fn session_json(s: &Session) -> Value {
        json!({
            "id": s.id, "device_id": s.device_id, "name": s.name,
            "created_at": "2026-11-06T12:00:00.000Z",
            "last_used_at": "2026-11-06T12:00:00.000Z",
            "expires_at": "2026-12-06T12:00:00.000Z"
        })
    }

    fn device_of(body: &Value) -> Result<(String, String), LaneResponse> {
        let id = body["device"]["id"].as_str().unwrap_or_default();
        let ok = (1..=128).contains(&id.len())
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !ok {
            return Err(lane_error(
                422,
                "validation_failed",
                json!({ "field": "device.id" }),
                "device.id is not valid",
            ));
        }
        let name = body["device"]["name"].as_str().unwrap_or_default();
        let n = name.chars().count();
        if !(1..=64).contains(&n) || name.chars().any(char::is_control) {
            return Err(lane_error(
                422,
                "validation_failed",
                json!({ "field": "device.name" }),
                "device.name is not valid",
            ));
        }
        Ok((id.to_owned(), name.to_owned()))
    }

    fn new_session(st: &mut State, member_id: &str, device_id: &str, name: &str) -> LaneResponse {
        for s in st.sessions.values_mut() {
            if s.live && s.member_id == member_id && s.device_id == device_id {
                s.live = false;
                s.reason = Some("replaced".into());
            }
        }
        let live = st
            .sessions
            .values()
            .filter(|s| s.live && s.member_id == member_id)
            .count();
        if live >= FakeLane::MAX_DEVICES {
            return lane_error(
                409,
                "device_limit",
                json!({ "max": FakeLane::MAX_DEVICES }),
                "Too many devices are signed in.",
            );
        }
        let bearer = format!("sewdv_{}", random(32));
        let s = Session {
            id: random(16),
            member_id: member_id.to_owned(),
            device_id: device_id.to_owned(),
            name: name.to_owned(),
            live: true,
            reason: None,
        };
        let body = json!({
            "ok": true, "token": bearer, "member_id": member_id,
            "session": FakeLane::session_json(&s)
        });
        st.sessions.insert(bearer, s);
        resp(200, body)
    }

    /// The live session of a request's bearer, or the lane's `401`.
    fn authed(st: &State, bearer: Option<&str>) -> Result<Session, LaneResponse> {
        let unauth = || {
            lane_error(
                401,
                "unauthenticated",
                Value::Null,
                "Sign in on this device.",
            )
        };
        let Some(b) = bearer else {
            return Err(unauth());
        };
        let Some(s) = st.sessions.get(b) else {
            return Err(unauth());
        };
        if !s.live {
            return Err(lane_error(
                401,
                "signed_out",
                json!({ "reason": s.reason.clone().unwrap_or_else(|| "signed_out".into()) }),
                "This device was signed out. Sign in again.",
            ));
        }
        Ok(s.clone())
    }

    fn handle(st: &mut State, origin: &str, req: &LaneRequest) -> LaneResponse {
        let body = req.body.clone().unwrap_or_else(|| json!({}));
        let bearer = req.bearer.as_ref().map(|b| b.expose().to_owned());
        let post = req.method == Method::Post;
        let path = req.path.as_str();
        macro_rules! authed {
            () => {
                match FakeLane::authed(st, bearer.as_deref()) {
                    Ok(s) => s,
                    Err(r) => return r,
                }
            };
        }
        match (post, path) {
            (false, "/sew/native/info") => {
                let mut v = serde_json::to_value(&st.info).unwrap_or_default();
                v["ok"] = json!(true);
                resp(200, v)
            }
            (true, "/sew/native/signin/password") => {
                if !st.info.signin.password {
                    return lane_error(404, "not_found", Value::Null, "Not found.");
                }
                let (id, name) = match FakeLane::device_of(&body) {
                    Ok(d) => d,
                    Err(r) => return r,
                };
                let user = body["username"].as_str().unwrap_or_default();
                let pw = body["password"].as_str().unwrap_or_default();
                let Some(u) = st.users.get(user).filter(|u| u.password == pw).cloned() else {
                    return lane_error(
                        401,
                        "unauthenticated",
                        Value::Null,
                        "The username or password is wrong.",
                    );
                };
                FakeLane::new_session(st, &u.member_id, &id, &name)
            }
            (true, "/sew/native/pair/redeem") => {
                if !st.info.signin.pairing {
                    return lane_error(404, "pairing_unavailable", Value::Null, "No pairing here.");
                }
                let (id, name) = match FakeLane::device_of(&body) {
                    Ok(d) => d,
                    Err(r) => return r,
                };
                let code = body["code"].as_str().unwrap_or_default();
                let Some(member) = st.pairing.remove(code) else {
                    return lane_error(
                        401,
                        "code_invalid",
                        Value::Null,
                        "The code is wrong or used.",
                    );
                };
                FakeLane::new_session(st, &member, &id, &name)
            }
            (true, "/sew/native/signin/door/begin" | "/sew/native/signin/door/finish")
                if !st.info.signin.door =>
            {
                lane_error(
                    404,
                    "door_unavailable",
                    Value::Null,
                    "No sign-in door here.",
                )
            }
            (true, "/sew/native/signin/door/begin") => {
                let device_id = if body.get("device").is_some() {
                    match FakeLane::device_of(&body) {
                        Ok((id, _)) => Some(id),
                        Err(r) => return r,
                    }
                } else {
                    None
                };
                let id = random(32);
                let challenge = random(32);
                st.doors.insert(
                    id.clone(),
                    DoorBegin {
                        device_id,
                        challenge: challenge.clone(),
                    },
                );
                resp(
                    200,
                    json!({ "ok": true, "id": id, "challenge": challenge,
                            "door": format!("{origin}/.k2/auth/sign-in"),
                            "expires_in_ms": 600000 }),
                )
            }
            (true, "/sew/native/signin/door/finish") => {
                let (device_id, name) = match FakeLane::device_of(&body) {
                    Ok(d) => d,
                    Err(r) => return r,
                };
                let invalid = || {
                    lane_error(
                        401,
                        "code_invalid",
                        Value::Null,
                        "The code is wrong or used.",
                    )
                };
                // The id is spent once, whatever happens next.
                let id = body["id"].as_str().unwrap_or_default();
                let Some(begin) = st.doors.remove(id) else {
                    return invalid();
                };
                let code = body["code"].as_str().unwrap_or_default();
                let Some((challenge, member)) = st.door_codes.remove(code) else {
                    return invalid();
                };
                if challenge != begin.challenge
                    || begin.device_id.as_ref().is_some_and(|d| *d != device_id)
                {
                    return invalid();
                }
                FakeLane::new_session(st, &member, &device_id, &name)
            }
            (false, "/.k2/auth/ping") => {
                if st.host_door {
                    resp(200, json!({ "k2": true }))
                } else {
                    k2_error(404, "not_found", "Not a door verb.")
                }
            }
            (true, "/.k2/auth/sign-in") if st.host_door => {
                let Some(challenge) = body["challenge"].as_str().filter(|c| !c.is_empty()) else {
                    return k2_error(400, "challenge_required", "Send the challenge.");
                };
                let user = body["username"].as_str().unwrap_or_default();
                let pw = body["password"].as_str().unwrap_or_default();
                let Some(u) = st.users.get(user).filter(|u| u.password == pw).cloned() else {
                    return k2_error(401, "bad_login", "The username or password is wrong.");
                };
                if u.locked {
                    return k2_error(423, "locked", "Too many tries.");
                }
                if let Some(token) = &u.change_token {
                    return resp(
                        403,
                        json!({ "ok": false, "changeToken": token,
                                "error": { "code": "password_change_required",
                                           "hint": "Set a password." } }),
                    );
                }
                let code = format!("k2lgn_{}", random(32));
                st.door_codes
                    .insert(code.clone(), (challenge.to_owned(), u.member_id.clone()));
                resp(200, json!({ "ok": true, "code": code }))
            }
            (true, "/.k2/auth/set-password") if st.host_door => {
                let token = body["token"].as_str().unwrap_or_default();
                let pw = body["password"].as_str().unwrap_or_default();
                let challenge = body["challenge"].as_str().unwrap_or_default().to_owned();
                if pw.chars().count() < 8 {
                    return k2_error(400, "password_too_short", "At least 8 characters.");
                }
                let Some(u) = st
                    .users
                    .values_mut()
                    .find(|u| u.change_token.as_deref() == Some(token) && !token.is_empty())
                else {
                    return k2_error(401, "bad_token", "The link is wrong or used.");
                };
                u.password = pw.to_owned();
                u.change_token = None;
                if !u.app_access || challenge.is_empty() {
                    return resp(200, json!({ "ok": true }));
                }
                let member = u.member_id.clone();
                let code = format!("k2lgn_{}", random(32));
                st.door_codes.insert(code.clone(), (challenge, member));
                resp(200, json!({ "ok": true, "code": code }))
            }
            (false, "/sew/native/whoami") => {
                let s = authed!();
                let display = st
                    .users
                    .values()
                    .find(|u| u.member_id == s.member_id)
                    .map(|u| u.display_name.clone());
                resp(
                    200,
                    json!({ "ok": true,
                            "member": { "id": s.member_id, "display_name": display },
                            "install_id": st.install_id.to_string(), "spaces": [], "caps": {},
                            "identity": st.info.identity, "session": FakeLane::session_json(&s) }),
                )
            }
            (false, "/sew/native/devices") => {
                let s = authed!();
                let devices: Vec<Value> = st
                    .sessions
                    .values()
                    .filter(|o| o.live && o.member_id == s.member_id)
                    .map(|o| {
                        let mut v = FakeLane::session_json(o);
                        v["current"] = json!(o.id == s.id);
                        v
                    })
                    .collect();
                resp(200, json!({ "ok": true, "devices": devices }))
            }
            (true, p) if p.starts_with("/sew/native/devices/") && p.ends_with("/revoke") => {
                let s = authed!();
                let id = &p["/sew/native/devices/".len()..p.len() - "/revoke".len()];
                let target = st
                    .sessions
                    .values_mut()
                    .find(|o| o.id == id && o.live && o.member_id == s.member_id);
                match target {
                    Some(t) => {
                        t.live = false;
                        t.reason = Some("revoked".into());
                        resp(200, json!({ "ok": true }))
                    }
                    None => lane_error(404, "not_found", Value::Null, "Not found."),
                }
            }
            (true, "/sew/native/signout") => {
                // `{ok: true}` also when the session already ended.
                match bearer.as_deref().and_then(|b| st.sessions.get_mut(b)) {
                    Some(s) => {
                        if s.live {
                            s.live = false;
                            s.reason = Some("signed_out".into());
                        }
                        let b = bearer.clone().unwrap_or_default();
                        FakeLane::close_socks_of(st, &b, Some(4011), "session revoked");
                        resp(200, json!({ "ok": true }))
                    }
                    None => lane_error(
                        401,
                        "unauthenticated",
                        Value::Null,
                        "Sign in on this device.",
                    ),
                }
            }
            (true, "/sew/native/handoff") => {
                let _s = authed!();
                let b = bearer.clone().unwrap_or_default();
                let unused = st.handoffs.values().filter(|v| **v == b).count();
                if unused >= 3 {
                    return lane_error(429, "rate_limited", Value::Null, "Too many codes.");
                }
                let code = random(32);
                st.handoffs.insert(code.clone(), b);
                resp(
                    200,
                    json!({ "ok": true, "code": code, "expires_in_ms": 60000 }),
                )
            }
            (true, "/sew/native/voice/join") => {
                let s = authed!();
                let room = body["room"].as_str().unwrap_or_default().to_owned();
                if let Some((rid, version)) = st.recorded.get(&room).copied() {
                    let answered = body["recording_notice"]["notice_version"].as_u64()
                        == Some(u64::from(version));
                    if !answered {
                        let d = consent_details(rid, version, &room);
                        return lane_error(
                            409,
                            "recording_consent_required",
                            d,
                            "This call is recorded.",
                        );
                    }
                }
                resp(200, join_answer(origin, &s.member_id, &room))
            }
            (true, "/sew/native/ticket") => {
                let _s = authed!();
                let ticket = format!("sewtk_{}", random(24));
                st.tickets
                    .insert(ticket.clone(), bearer.clone().unwrap_or_default());
                st.issued_tickets.push(ticket.clone());
                let ws = origin
                    .replacen("https://", "wss://", 1)
                    .replacen("http://", "ws://", 1);
                resp(
                    200,
                    json!({ "ticket": ticket,
                            "url": format!("{ws}/sew/native/api/v1/gateway?v=1&encoding=json"),
                            "expires_in_ms": 30000 }),
                )
            }
            (true, "/sew/native/voice/consent") => {
                let _s = authed!();
                st.consents.push(body.clone());
                let rid = body["recording_id"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let room = body["room"].as_str().unwrap_or_default().to_owned();
                let consent = body["consent"].as_bool().unwrap_or(false);
                resp(200, recording(rid, &room, consent))
            }
            _ => lane_error(404, "not_found", Value::Null, "Not found."),
        }
    }
}

impl Transport for FakeLane {
    fn send<'a>(
        &'a self,
        origin: &'a Origin,
        req: LaneRequest,
    ) -> BoxFuture<'a, Result<LaneResponse, TransportError>> {
        Box::pin(std::future::ready(self.respond(origin, req)))
    }

    fn send_bytes<'a>(
        &'a self,
        origin: &'a Origin,
        req: ByteRequest,
    ) -> BoxFuture<'a, Result<ByteResponse, TransportError>> {
        Box::pin(std::future::ready(self.respond_bytes(origin, req)))
    }
}

fn as_bytes(r: LaneResponse) -> ByteResponse {
    let mut headers = vec![(
        "content-type".to_owned(),
        "application/json; charset=utf-8".to_owned(),
    )];
    if let Some(ms) = r.retry_after_ms {
        headers.push(("retry-after".to_owned(), (ms / 1000).to_string()));
    }
    ByteResponse {
        status: r.status,
        headers,
        body: r.body,
    }
}

/// A JSON answer of the client API, with the headers a server adds that a page must never get
/// (`Set-Cookie`, `WWW-Authenticate`) and the ones it may (`X-RateLimit-*`).
fn api_json(status: u16, body: Value) -> ByteResponse {
    ByteResponse {
        status,
        headers: vec![
            ("content-type".into(), "application/json".into()),
            ("x-ratelimit-remaining".into(), "29".into()),
            ("set-cookie".into(), "upstream=1; Path=/".into()),
            ("www-authenticate".into(), "Bearer".into()),
        ],
        body: serde_json::to_vec(&body).unwrap_or_default(),
    }
}

fn query_has(q: &str, k: &str) -> bool {
    q.split('&').any(|kv| {
        kv.split_once('=')
            .is_some_and(|(n, v)| n == k && !v.is_empty())
    })
}

/// A file's bytes, whole or the `Range: bytes=a-b` part.
fn file_answer(bytes: &[u8], ct: &str, range: Option<&str>, head: bool) -> ByteResponse {
    let total = bytes.len();
    let mut headers = vec![
        ("content-type".to_owned(), ct.to_owned()),
        ("accept-ranges".to_owned(), "bytes".to_owned()),
        ("etag".to_owned(), "\"f1\"".to_owned()),
        (
            "content-disposition".to_owned(),
            "inline; filename=\"f.bin\"".to_owned(),
        ),
        ("x-content-type-options".to_owned(), "nosniff".to_owned()),
        ("set-cookie".to_owned(), "upstream=1; Path=/".to_owned()),
        ("connection".to_owned(), "keep-alive, x-hop".to_owned()),
        ("x-hop".to_owned(), "1".to_owned()),
    ];
    let span = range
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.split_once('-'))
        .and_then(|(a, b)| {
            let a: usize = a.parse().ok()?;
            let b: usize = if b.is_empty() {
                total.checked_sub(1)?
            } else {
                b.parse().ok()?
            };
            (a <= b && b < total).then_some((a, b))
        });
    let (status, body) = match span {
        Some((a, b)) => {
            headers.push(("content-range".into(), format!("bytes {a}-{b}/{total}")));
            (206, bytes[a..=b].to_vec())
        }
        None => (200, bytes.to_vec()),
    };
    headers.push(("content-length".into(), body.len().to_string()));
    ByteResponse {
        status,
        headers,
        body: if head { Vec::new() } else { body },
    }
}

impl FakeLane {
    fn respond_bytes(
        &self,
        origin: &Origin,
        req: ByteRequest,
    ) -> Result<ByteResponse, TransportError> {
        let mut st = self.st();
        st.seen_bytes.push(SeenBytes {
            method: req.method.as_str(),
            path: req.path.clone(),
            bearer: req.bearer.as_ref().map(|b| b.expose().to_owned()),
            headers: req.headers.clone(),
            body: req.body.clone(),
        });
        let off = st
            .origins
            .as_ref()
            .is_some_and(|o| !o.iter().any(|x| x == origin.as_str()));
        if st.down || off {
            return Err(TransportError(format!(
                "{}: could not connect",
                origin.authority()
            )));
        }
        let (path, query) = req.path.split_once('?').unwrap_or((req.path.as_str(), ""));
        if let Some(at) = st.queued.iter().position(|(p, _)| p.as_str() == path)
            && let Some((_, r)) = st.queued.remove(at)
        {
            return Ok(as_bytes(r));
        }
        let not_found = || as_bytes(lane_error(404, "not_found", Value::Null, "not found"));
        let Some(rest) = path.strip_prefix("/sew/native/api/v1/") else {
            return Ok(not_found());
        };
        let bearer = req.bearer.as_ref().map(|b| b.expose().to_owned());
        let s = match FakeLane::authed(&st, bearer.as_deref()) {
            Ok(s) => s,
            Err(r) => return Ok(as_bytes(r)),
        };
        let parts: Vec<&str> = rest.split('/').collect();
        let json_body = || -> Value {
            req.body
                .as_deref()
                .and_then(|b| serde_json::from_slice(b).ok())
                .unwrap_or(Value::Null)
        };
        let raw_ok = req
            .header("content-type")
            .is_some_and(|c| c == "application/octet-stream" || c.starts_with("image/"));
        let r = match (req.method, parts.as_slice()) {
            (ByteMethod::Get, ["me"]) => {
                let name = st
                    .users
                    .values()
                    .find(|u| u.member_id == s.member_id)
                    .map(|u| u.display_name.clone());
                api_json(200, json!({ "id": s.member_id, "display_name": name }))
            }
            (ByteMethod::Post, ["channels", ch, "messages"]) => {
                let key = req.header("idempotency-key").map(str::to_owned);
                if let Some(k) = &key
                    && let Some(first) = st.idempotent.get(k).cloned()
                {
                    let mut r = api_json(201, first);
                    r.headers
                        .push(("idempotent-replayed".into(), "true".into()));
                    return Ok(r);
                }
                st.next_id += 1;
                let answer = json!({ "id": st.next_id.to_string(), "channel_id": ch,
                                     "author_id": s.member_id,
                                     "body": json_body()["body"] });
                if let Some(k) = key {
                    st.idempotent.insert(k, answer.clone());
                }
                api_json(201, answer)
            }
            (ByteMethod::Post, ["uploads"]) => {
                st.next_id += 1;
                api_json(
                    201,
                    json!({ "id": st.next_id.to_string(), "size": json_body()["size"] }),
                )
            }
            (ByteMethod::Post, ["uploads", id, "data"]) => {
                if !raw_ok {
                    return Ok(as_bytes(lane_error(
                        415,
                        "unsupported_media_type",
                        Value::Null,
                        "content type not allowed",
                    )));
                }
                let body = req.body.clone().unwrap_or_default();
                let ct = req.header("content-type").unwrap_or_default().to_owned();
                let size = body.len();
                st.uploads.insert((*id).to_owned(), (body, ct));
                api_json(200, json!({ "id": id, "size": size, "state": "uploaded" }))
            }
            (ByteMethod::Post, ["me", "avatar"]) if raw_ok => {
                let size = req.body.as_ref().map_or(0, Vec::len);
                api_json(200, json!({ "ok": true, "size": size }))
            }
            (ByteMethod::Get | ByteMethod::Head, ["files", id])
                if query_has(query, "exp") && query_has(query, "sig") =>
            {
                match st.uploads.get(*id) {
                    Some((b, ct)) => {
                        file_answer(b, ct, req.header("range"), req.method == ByteMethod::Head)
                    }
                    None => not_found(),
                }
            }
            (ByteMethod::Get | ByteMethod::Head, ["files", id, "thumb"]) => {
                match st.uploads.get(*id) {
                    Some(_) => file_answer(b"thumb", "image/webp", None, false),
                    None => not_found(),
                }
            }
            _ => not_found(),
        };
        Ok(r)
    }

    fn gateway_connect(&self, url: &Url, bearer: &Secret) -> Result<SocketPipe, ConnectError> {
        let mut st = self.st();
        if st.down {
            return Err(ConnectError::Unreachable("could not connect".into()));
        }
        let refused = |r: LaneResponse| ConnectError::Refused {
            status: r.status,
            retry_after_ms: r.retry_after_ms,
            body: r.body,
        };
        if url.path() != "/sew/native/api/v1/gateway" || url.query() != Some("v=1&encoding=json") {
            return Err(refused(lane_error(
                400,
                "validation_failed",
                Value::Null,
                "the gateway takes exactly ?v=1&encoding=json",
            )));
        }
        let b = bearer.expose().to_owned();
        if let Err(r) = FakeLane::authed(&st, Some(b.as_str())) {
            return Err(refused(r));
        }
        if st.socks.iter().filter(|s| s.open && s.bearer == b).count() >= 2 {
            let mut r = lane_error(
                429,
                "rate_limited",
                Value::Null,
                "too many gateway sockets for this device",
            );
            r.retry_after_ms = Some(1000);
            return Err(refused(r));
        }
        let (client_tx, mut server_rx) = mpsc::unbounded_channel::<WsOut>();
        let (server_tx, client_rx) = mpsc::unbounded_channel::<WsIn>();
        let n = st.socks.len();
        let _ = server_tx.send(WsIn::Text(
            json!({ "op": 8, "d": { "heartbeat_interval": 41250 }, "s": null, "t": null })
                .to_string(),
        ));
        st.socks.push(FakeSock {
            bearer: b.clone(),
            to_client: server_tx.clone(),
            open: true,
        });
        drop(st);
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            while let Some(m) = server_rx.recv().await {
                let mut st = lock(&state);
                if !st.socks[n].open {
                    continue;
                }
                let to = st.socks[n].to_client.clone();
                let close = |st: &mut State, code: u16, reason: &str| {
                    st.socks[n].open = false;
                    let _ = to.send(WsIn::Closed {
                        code: Some(code),
                        reason: reason.to_owned(),
                    });
                };
                let text = match m {
                    WsOut::Close(code, reason) => {
                        close(&mut st, code, &reason);
                        continue;
                    }
                    WsOut::Text(t) => t,
                };
                let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                    close(&mut st, 4002, "decode error");
                    continue;
                };
                st.frames.push((n, frame.clone()));
                let spend = |st: &mut State, t: &Value| -> bool {
                    let t = t.as_str().unwrap_or_default();
                    st.tickets.get(t) == Some(&b) && st.tickets.remove(t).is_some()
                };
                match frame["op"].as_u64() {
                    Some(1) => {
                        let _ = to.send(WsIn::Text(json!({ "op": 9, "d": null }).to_string()));
                    }
                    Some(2) => {
                        if spend(&mut st, &frame["d"]["ticket"]) {
                            let _ = to.send(WsIn::Text(
                                json!({ "op": 0, "t": "READY", "s": 1,
                                        "d": { "session_id": format!("gs_{n}") } })
                                .to_string(),
                            ));
                        } else {
                            close(&mut st, 4004, "authentication failed");
                        }
                    }
                    Some(4) => {
                        if spend(&mut st, &frame["d"]["ticket"]) {
                            let _ = to.send(WsIn::Text(
                                json!({ "op": 0, "t": "RESUMED", "s": 2, "d": {} }).to_string(),
                            ));
                        } else {
                            close(&mut st, 4004, "authentication failed");
                        }
                    }
                    Some(11) if !spend(&mut st, &frame["d"]["token"]) => {
                        close(&mut st, 4004, "authentication failed");
                    }
                    _ => {}
                }
            }
            let mut st = lock(&state);
            st.socks[n].open = false;
        });
        Ok(SocketPipe {
            tx: client_tx,
            rx: client_rx,
        })
    }
}

impl SocketConnector for FakeLane {
    fn connect<'a>(
        &'a self,
        url: &'a Url,
        bearer: &'a Secret,
    ) -> BoxFuture<'a, Result<SocketPipe, ConnectError>> {
        Box::pin(std::future::ready(self.gateway_connect(url, bearer)))
    }
}

impl FakeLane {
    fn respond(&self, origin: &Origin, req: LaneRequest) -> Result<LaneResponse, TransportError> {
        {
            let mut st = self.st();
            st.seen.push(SeenRequest {
                origin: origin.as_str().to_owned(),
                method: match req.method {
                    Method::Get => "GET",
                    Method::Post => "POST",
                },
                path: req.path.clone(),
                bearer: req.bearer.as_ref().map(|b| b.expose().to_owned()),
                body: req.body.clone(),
            });
            let off = st
                .origins
                .as_ref()
                .is_some_and(|o| !o.iter().any(|x| x == origin.as_str()));
            if st.down || off {
                return Err(TransportError(format!(
                    "{}: could not connect",
                    origin.authority()
                )));
            }
            if let Some(at) = st.queued.iter().position(|(p, _)| p == &req.path)
                && let Some((_, r)) = st.queued.remove(at)
            {
                return Ok(r);
            }
            Ok(FakeLane::handle(&mut st, origin.as_str(), &req))
        }
    }
}

/// Records the core's events.
#[derive(Debug, Default)]
pub struct RecordingAppSink {
    events: Mutex<Vec<AppEvent>>,
}

impl RecordingAppSink {
    /// Every event, in order.
    pub fn events(&self) -> Vec<AppEvent> {
        lock(&self.events).clone()
    }

    /// The events' names, in order.
    pub fn names(&self) -> Vec<&'static str> {
        lock(&self.events).iter().map(AppEvent::name).collect()
    }
}

impl AppEventSink for RecordingAppSink {
    fn emit(&self, event: &AppEvent) {
        lock(&self.events).push(event.clone());
    }
}

/// Records the voice plugin's `forget_server` calls.
#[derive(Debug, Default)]
pub struct RecordingForget {
    calls: Mutex<Vec<u64>>,
}

impl RecordingForget {
    /// The installs forgotten, in order.
    pub fn calls(&self) -> Vec<u64> {
        lock(&self.calls).clone()
    }
}

impl VoiceForget for RecordingForget {
    fn forget(&self, install_id: u64) -> BoxFuture<'_, Result<(), AppError>> {
        lock(&self.calls).push(install_id);
        Box::pin(std::future::ready(Ok(())))
    }
}
