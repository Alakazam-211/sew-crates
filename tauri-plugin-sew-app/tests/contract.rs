//! The plugin against the App server lane's contract fixture (`tests/native-lane.json`, the
//! lane's own fixture with a neutral App: its address is `app.test` and its CSRF header, which
//! only a page sends on the cookie lane, is `x-app-csrf`): every request a native client sends
//! must be the fixture's (method, path, bearer, body), and every answer the fixture gives must
//! read.
//!
//! The steps marked `"to": "host"` are the client's own requests to the host's sign-in door
//! (`/.k2/auth/*` on the App's address); the replay answers them from the fixture like the
//! lane's. The door's `GET /.k2/auth/ping`, which the fixture leaves out, is answered
//! `{"k2": true}`. The fixture abbreviates one Sew body (the join's `state`); it is filled with
//! a full Sew shape here, and the requests stay exact.
//!
//! The API transport replays the fixture's API steps too (`post_a_message`,
//! `auth_routes_are_not_proxied`, `ticket`, `signed_out_after`): what it sends is the
//! fixture's request, and what it answers the page is the fixture's answer.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tauri_plugin_sew_app::fake::{RecordingAppSink, join_answer};
use tauri_plugin_sew_app::lane::route;
use tauri_plugin_sew_app::{
    AccountState, ApiCall, AppConfig, AppService, ByteMethod, ByteRequest, ByteResponse,
    ConnectError, DeviceName, GatewayEvent, LaneClient, LaneFailure, LaneJoinProvider, LaneRequest,
    LaneResponse, Method, Origin, Refusal, Secret, SecretChoice, SocketConnector, SocketPipe,
    Transport, TransportError, WsIn, WsOut,
};
use tauri_plugin_sew_voice::{
    BoxFuture, ConsentAnswer, JoinProvider, JoinRequest, ProviderError, RecordingRequest,
};

const ORIGIN: &str = "https://app.test";

fn fixture() -> Value {
    serde_json::from_str(include_str!("native-lane.json")).unwrap()
}

struct Replay {
    steps: HashMap<String, Value>,
    expect: Mutex<VecDeque<String>>,
    saved: Mutex<HashMap<String, String>>,
    errors: Mutex<Vec<String>>,
    overrides: Mutex<HashMap<String, Value>>,
}

fn any_for(key: &str) -> Value {
    match key {
        "identity" => json!("standalone"),
        "pairing" => json!(true),
        "member" => json!({ "id": "1002", "display_name": "Bob" }),
        "install_id" => json!("7001"),
        "spaces" => json!([]),
        "caps" => json!({}),
        // info.shell.min: no minimum.
        "min" => Value::Null,
        _ => json!("x"),
    }
}

impl Replay {
    fn new() -> Arc<Replay> {
        let f = fixture();
        let steps = f["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["name"].as_str().unwrap().to_owned(), s.clone()))
            .collect();
        Arc::new(Replay {
            steps,
            expect: Mutex::new(VecDeque::new()),
            saved: Mutex::new(HashMap::new()),
            errors: Mutex::new(Vec::new()),
            overrides: Mutex::new(HashMap::new()),
        })
    }

    fn expect(&self, names: &[&str]) {
        let mut q = self.expect.lock().unwrap();
        assert!(q.is_empty(), "steps left unsent: {q:?}");
        q.extend(names.iter().map(|s| (*s).to_owned()));
    }

    fn done(&self) {
        let q = self.expect.lock().unwrap();
        assert!(q.is_empty(), "steps never sent: {q:?}");
        let e = self.errors.lock().unwrap();
        assert!(
            e.is_empty(),
            "requests unlike the fixture:\n{}",
            e.join("\n")
        );
    }

    fn sub(&self, v: &Value, key: &str) -> Value {
        let saved = self.saved.lock().unwrap().clone();
        match v {
            Value::String(s) if s.starts_with("{{") => {
                let name = s.trim_start_matches("{{").trim_end_matches("}}");
                json!(saved.get(name).cloned().unwrap_or_default())
            }
            Value::String(s) if s.starts_with("Bearer {{") => {
                let name = s.trim_start_matches("Bearer {{").trim_end_matches("}}");
                json!(format!(
                    "Bearer {}",
                    saved.get(name).cloned().unwrap_or_default()
                ))
            }
            Value::String(s) => match s.as_str() {
                "<device_bearer>" => json!(format!("sewdv_{}", "b".repeat(43))),
                "<code>" => json!("c".repeat(43)),
                "<door_code>" => json!(format!("k2lgn_{}", "d".repeat(43))),
                "<session_id>" => json!("s".repeat(22)),
                "<timestamp>" => json!("2026-11-06T12:00:00.000Z"),
                "<media_url>" => json!("wss://app.test/sew/native"),
                "<app_url>" => json!("https://app.test/"),
                "<any>" => any_for(key),
                _ => v.clone(),
            },
            Value::Object(m) => {
                Value::Object(m.iter().map(|(k, x)| (k.clone(), self.sub(x, k))).collect())
            }
            Value::Array(a) => Value::Array(a.iter().map(|x| self.sub(x, key)).collect()),
            _ => v.clone(),
        }
    }

    fn answer(&self, req: &LaneRequest) -> LaneResponse {
        if req.path == route::HOST_DOOR_PING && req.method == Method::Get {
            return LaneResponse {
                status: 200,
                retry_after_ms: None,
                body: br#"{"k2":true}"#.to_vec(),
            };
        }
        let name = if req.path == route::INFO && req.method == Method::Get {
            "info".to_owned()
        } else {
            match self.expect.lock().unwrap().pop_front() {
                Some(n) => n,
                None => {
                    self.errors
                        .lock()
                        .unwrap()
                        .push(format!("unexpected request {}", req.path));
                    return LaneResponse {
                        status: 599,
                        retry_after_ms: None,
                        body: vec![],
                    };
                }
            }
        };
        let step = &self.steps[&name];
        let want = &step["request"];
        let mut errs = Vec::new();
        let method = match req.method {
            Method::Get => "GET",
            Method::Post => "POST",
        };
        if want["method"] != method {
            errs.push(format!("{name}: method {method}"));
        }
        if want["path"] != req.path.as_str() {
            errs.push(format!("{name}: path {}", req.path));
        }
        let want_auth = want["headers"]["authorization"].as_str().map(|_| {
            self.sub(&want["headers"]["authorization"], "authorization")
                .as_str()
                .unwrap()
                .to_owned()
        });
        let got_auth = req
            .bearer
            .as_ref()
            .map(|b| format!("Bearer {}", b.expose()));
        if want_auth != got_auth {
            errs.push(format!(
                "{name}: authorization {:?} (want {want_auth:?})",
                got_auth.is_some()
            ));
        }
        if let Some(wb) = want.get("body") {
            let mut wb = self.sub(wb, "body");
            let mut gb = req.body.clone().unwrap_or_else(|| json!({}));
            // The install's device id is the plugin's own (random per account).
            if wb["device"]["id"].is_string() && gb["device"]["id"].is_string() {
                wb["device"]["id"] = gb["device"]["id"].clone();
            }
            if gb.get("device").is_some() && wb.get("device").is_none() {
                gb.as_object_mut().unwrap().remove("device");
            }
            if wb != gb {
                errs.push(format!("{name}: body {gb} (want {wb})"));
            }
        }
        self.errors.lock().unwrap().extend(errs);
        let r = &step["response"];
        let mut body = match self.overrides.lock().unwrap().remove(&name) {
            Some(b) => b,
            None => self.sub(&r["body"], "body"),
        };
        if name == "sign_in" || name == "door_finish" {
            // Keep the account's own device id in the answer, as a server would.
            if let Some(id) = req.body.as_ref().and_then(|b| b["device"]["id"].as_str()) {
                body["session"]["device_id"] = json!(id);
            }
        }
        if let Some(save) = step.get("save").and_then(Value::as_object) {
            for (k, path) in save {
                let mut v = &body;
                for part in path.as_str().unwrap().split('.') {
                    v = &v[part];
                }
                self.saved
                    .lock()
                    .unwrap()
                    .insert(k.clone(), v.as_str().unwrap().to_owned());
            }
        }
        LaneResponse {
            status: r["status"].as_u64().unwrap() as u16,
            retry_after_ms: None,
            body: serde_json::to_vec(&body).unwrap(),
        }
    }
}

impl Transport for Replay {
    fn send<'a>(
        &'a self,
        origin: &'a Origin,
        req: LaneRequest,
    ) -> BoxFuture<'a, Result<LaneResponse, TransportError>> {
        assert_eq!(origin.as_str(), ORIGIN);
        Box::pin(std::future::ready(Ok(self.answer(&req))))
    }

    /// The API transport's requests: the same checks (method, path, bearer, JSON body), and
    /// no header but the ones a native client sends.
    fn send_bytes<'a>(
        &'a self,
        origin: &'a Origin,
        req: ByteRequest,
    ) -> BoxFuture<'a, Result<ByteResponse, TransportError>> {
        assert_eq!(origin.as_str(), ORIGIN);
        for (k, _) in &req.headers {
            if !["accept", "content-type", "idempotency-key"].contains(&k.as_str()) {
                self.errors
                    .lock()
                    .unwrap()
                    .push(format!("{}: header {k}", req.path));
            }
        }
        let method = match req.method {
            ByteMethod::Get => Method::Get,
            ByteMethod::Post => Method::Post,
            ByteMethod::Head => {
                self.errors.lock().unwrap().push("a HEAD".into());
                Method::Get
            }
        };
        let lane = LaneRequest {
            method,
            path: req.path.clone(),
            bearer: req.bearer.clone(),
            body: req
                .body
                .as_deref()
                .map(|b| serde_json::from_slice(b).unwrap()),
        };
        let r = self.answer(&lane);
        Box::pin(std::future::ready(Ok(ByteResponse {
            status: r.status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: r.body,
        })))
    }
}

/// A gateway connector that records what it was asked and keeps the socket open.
#[derive(Default)]
struct Sockets {
    asked: Mutex<Vec<(String, String)>>,
    frames: Arc<Mutex<Vec<Value>>>,
    held: Mutex<Vec<tokio::sync::mpsc::UnboundedSender<WsIn>>>,
}

impl SocketConnector for Sockets {
    fn connect<'a>(
        &'a self,
        url: &'a url::Url,
        bearer: &'a Secret,
    ) -> BoxFuture<'a, Result<SocketPipe, ConnectError>> {
        self.asked
            .lock()
            .unwrap()
            .push((url.to_string(), bearer.expose().to_owned()));
        let (tx, mut from_client) = tokio::sync::mpsc::unbounded_channel::<WsOut>();
        let (to_client, rx) = tokio::sync::mpsc::unbounded_channel::<WsIn>();
        self.held.lock().unwrap().push(to_client);
        let frames = self.frames.clone();
        tokio::spawn(async move {
            while let Some(m) = from_client.recv().await {
                if let WsOut::Text(t) = m {
                    frames
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str(&t).unwrap());
                }
            }
        });
        Box::pin(std::future::ready(Ok(SocketPipe { tx, rx })))
    }
}

/// Whether `got` has `want`'s shape, `"<any>"` matching anything.
fn shaped(want: &Value, got: &Value) -> bool {
    match (want, got) {
        (Value::String(w), _) if w == "<any>" => true,
        (Value::Object(w), Value::Object(g)) => {
            w.len() == g.len()
                && w.iter()
                    .all(|(k, v)| g.get(k).is_some_and(|x| shaped(v, x)))
        }
        _ => want == got,
    }
}

fn service(replay: &Arc<Replay>) -> AppService {
    let mut cfg = AppConfig::new("com.example.contract");
    cfg.secrets = SecretChoice::Memory;
    cfg.device_name = "Laptop".into();
    AppService::new(
        cfg,
        LaneClient::new(replay.clone()),
        Arc::new(RecordingAppSink::default()),
    )
    .unwrap()
}

#[tokio::test]
async fn the_plugin_speaks_the_lane_fixture() {
    let f = fixture();
    assert_eq!(f["lane"], 1);
    assert_eq!(tauri_plugin_sew_app::LANE_VERSION, 1);
    let replay = Replay::new();
    let svc = service(&replay);
    let origin = Origin::parse(ORIGIN, false).unwrap();

    // Adding the server asks info; confirming saves it.
    let p = svc.add_server("app.test").await.unwrap();
    assert_eq!(p.origin, origin);
    assert!(p.info.signin.password);
    assert_eq!(p.info.signalling.as_deref(), Some("lane"));
    svc.confirm_server(&origin).unwrap();

    // A bad device id is the lane's 422, read with its field.
    replay.expect(&["sign_in_bad_device"]);
    let lane = LaneClient::new(replay.clone());
    let bad = DeviceName {
        id: String::new(),
        name: "Laptop".into(),
    };
    let e = lane
        .sign_in_password(&origin, "bob", &Secret::new("lane-test-bob-pw"), &bad)
        .await
        .unwrap_err();
    let LaneFailure::Refused(r) = e else {
        panic!("{e:?}")
    };
    assert_eq!(
        (r.status, r.code.as_deref()),
        (422, Some("validation_failed"))
    );
    assert_eq!(r.detail["field"], "device.id");
    assert_eq!(r.hint.as_deref(), Some("device.id is not valid"));
    // The plugin itself never sends one.
    assert!(bad.validate().is_err());

    // Sign-in, then whoami.
    replay.expect(&["sign_in", "whoami"]);
    let acct = svc
        .sign_in_password(&origin, "bob", Secret::new("lane-test-bob-pw"), None)
        .await
        .unwrap();
    assert_eq!(acct.member_id, "1002");
    assert_eq!(acct.label.as_deref(), Some("Bob"));
    assert!(acct.active);
    assert_eq!(
        svc.book().account(&acct.key).unwrap().install_id,
        Some(7001)
    );

    // The handoff code and where the page redeems it.
    replay.expect(&["handoff"]);
    let h = svc.handoff(&acct.key, None).await.unwrap();
    assert_eq!(h.code.expose().len(), 43);
    assert_eq!(h.expires_in_ms, 60000);
    assert_eq!(h.redeem_url, "https://app.test/sew/native/handoff/redeem");
    assert_eq!(
        f["steps"][10]["request"]["path"],
        route::HANDOFF_REDEEM,
        "the page's redeem route"
    );

    // Voice: the join with the bearer, unchanged, and its answer with the credential.
    let provider = LaneJoinProvider::new(svc.clone());
    replay.expect(&["voice_join"]);
    let mut answer = join_answer(ORIGIN, "1002", "5");
    answer["join"]["url"] = json!("wss://app.test/sew/native");
    replay
        .overrides
        .lock()
        .unwrap()
        .insert("voice_join".into(), answer);
    let req = JoinRequest::new("acme", "5");
    let a = provider.join(req.clone()).await.unwrap();
    assert_eq!(a.api_base.as_str(), "https://app.test/sew/native/");
    assert_eq!(a.join.url.as_str(), "wss://app.test/sew/native");
    assert!(a.call_credential().is_some(), "the call credential is kept");

    // A recorded room: Sew's prompt, read as the engine's consent question.
    replay.expect(&["voice_join_consent_required"]);
    let e = provider
        .join({
            let mut req = req;
            req.room = "7".into();
            req
        })
        .await
        .unwrap_err();
    match e {
        ProviderError::ConsentRequired(c) => {
            assert_eq!(c.notice_version(), 357303234);
            let p = serde_json::to_value(&c.consent_prompt).unwrap();
            assert_eq!(p["recording_id"], "77");
            assert_eq!(p["recorder_name"], "Bobbin");
            assert!(c.recording.is_some());
            assert!(!c.auto);
        }
        e => panic!("{e:?}"),
    }

    // The consent answer, sent with the bearer; Sew's recording comes back.
    replay.expect(&["voice_consent"]);
    let ans: ConsentAnswer = serde_json::from_value(json!({
        "target": "acme", "room": "7", "recording_id": "77", "consent": true,
        "notice_version": 357303234, "device_id": "dev-1"
    }))
    .unwrap();
    let rec = provider.consent(ans).await.unwrap();
    assert_eq!(serde_json::to_value(rec.id).unwrap(), json!("77"));

    // Record and Stop.
    replay.expect(&["voice_recording_start"]);
    let mut start = RecordingRequest::start("acme", "5", "5".parse().unwrap());
    start.device_id = Some("dev-1".into());
    let rec = provider.recording(start).await.unwrap();
    assert_eq!(serde_json::to_value(rec.id).unwrap(), json!("79"));
    replay.expect(&["voice_recording_stop_required"]);
    let mut stop = RecordingRequest::stop("acme", "7", "7".parse().unwrap(), "77".parse().unwrap());
    stop.device_id = Some("dev-1".into());
    match provider.recording(stop).await.unwrap_err() {
        ProviderError::Refused { code, detail } => {
            assert_eq!(code, "invalid_state");
            assert_eq!(detail["reason"], "recording_required");
        }
        e => panic!("{e:?}"),
    }
    replay.expect(&["voice_recording_stop"]);
    let mut stop = RecordingRequest::stop("acme", "5", "5".parse().unwrap(), "79".parse().unwrap());
    stop.device_id = Some("dev-1".into());
    provider.recording(stop).await.unwrap();

    // The device list.
    replay.expect(&["devices"]);
    let d = svc.devices(&acct.key).await.unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].current, Some(true));

    // Sign-out.
    replay.expect(&["sign_out"]);
    let out = svc.sign_out(&acct.key).await.unwrap();
    assert!(out.server_signed_out);
    assert!(svc.servers().servers[0].accounts.is_empty());
    replay.done();

    // Bob on a second install, through the host's sign-in door: the lane's begin, the
    // password to the host's own door with the begin's challenge (the fixture's "to": "host"
    // step), the lane's finish with the door's code.
    let mut cfg = AppConfig::new("com.example.contract.b");
    cfg.secrets = SecretChoice::Memory;
    cfg.device_name = "Desktop".into();
    let svc_b = AppService::new(
        cfg,
        LaneClient::new(replay.clone()),
        Arc::new(RecordingAppSink::default()),
    )
    .unwrap();
    assert!(svc_b.add_server("app.test").await.unwrap().info.signin.door);
    svc_b.confirm_server(&origin).unwrap();
    replay.expect(&[
        "door_begin",
        "door_sign_in_at_the_host",
        "door_finish",
        "door_whoami",
    ]);
    let b = svc_b
        .sign_in_door(&origin, "bob", Secret::new("lane-test-bob-pw"), None)
        .await
        .unwrap();
    assert_eq!(b.member_id, "1002");
    replay.done();
    let saved = replay.saved.lock().unwrap().clone();
    assert!(saved["door_code"].starts_with("k2lgn_"));
    let device = DeviceName {
        id: b.key.account.clone(),
        name: "Desktop".into(),
    };
    // The finish's refusals read with the lane's code.
    replay.expect(&["door_finish_unknown_id"]);
    let e = lane
        .door_finish(
            &origin,
            &Secret::new("A".repeat(43)),
            &Secret::new(saved["door_code"].clone()),
            &device,
        )
        .await
        .unwrap_err();
    assert_eq!(e.to_error().code, "code_invalid");
    replay.expect(&["door_finish_is_single_use"]);
    let e = lane
        .door_finish(
            &origin,
            &Secret::new(saved["door_id"].clone()),
            &Secret::new(saved["door_code"].clone()),
            &device,
        )
        .await
        .unwrap_err();
    assert_eq!(e.to_error().code, "code_invalid");
    replay.done();
}

#[test]
fn the_fixtures_refusals_read() {
    let f = fixture();
    let step = |n: &str| {
        f["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == n)
            .unwrap()["response"]
            .clone()
    };
    let read = |r: Value| {
        Refusal::from_response(&LaneResponse {
            status: r["status"].as_u64().unwrap() as u16,
            retry_after_ms: None,
            body: serde_json::to_vec(&r["body"]).unwrap(),
        })
    };
    let r = read(step("signed_out_after"));
    assert!(r.is_signed_out());
    assert_eq!(r.signed_out_reason(), "signed_out");
    assert_eq!(
        r.hint.as_deref(),
        Some("This device was signed out. Sign in again.")
    );
    assert_eq!(r.to_error().signed_out_reason(), Some("signed_out"));
    let r = read(step("no_bearer"));
    assert!(r.is_signed_out());
    assert_eq!(r.code.as_deref(), Some("unauthenticated"));
    let r = read(step("call_door_refuses_a_chat_route"));
    assert_eq!((r.status, r.code.as_deref()), (404, Some("not_found")));
    let r = read(step("sign_in_refused_to_a_page"));
    assert_eq!(r.code.as_deref(), Some("native_only"));
    // A native client never sends Origin: a lane request has no headers but the bearer.
    let req = LaneRequest {
        method: Method::Post,
        path: route::SIGNIN_PASSWORD.into(),
        bearer: None,
        body: None,
    };
    assert!(!format!("{req:?}").to_ascii_lowercase().contains("origin"));
}

/// The API transport against the fixture: a proxied POST, an auth route refused as the lane
/// refuses it, the gateway's ticket (its URL the fixture's, the ticket put in by the plugin),
/// and the ended session's `401`.
#[tokio::test]
async fn the_transport_speaks_the_lane_fixture() {
    let f = fixture();
    let step = |n: &str| {
        f["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == n)
            .unwrap()
            .clone()
    };
    let replay = Replay::new();
    let svc = service(&replay);
    let sockets = Arc::new(Sockets::default());
    svc.set_socket_connector(sockets.clone());
    let origin = Origin::parse(ORIGIN, false).unwrap();
    svc.add_server("app.test").await.unwrap();
    svc.confirm_server(&origin).unwrap();
    replay.expect(&["sign_in", "whoami"]);
    let acct = svc
        .sign_in_password(&origin, "bob", Secret::new("lane-test-bob-pw"), None)
        .await
        .unwrap();
    let call = |method: &str, path: &str, body: Option<Value>| ApiCall {
        method: method.into(),
        path: path.into(),
        body,
        idempotency_key: None,
    };

    // A proxied POST: the fixture's request, its answer as it is.
    replay.expect(&["post_a_message"]);
    let s = step("post_a_message");
    let r = svc
        .api(
            None,
            call(
                "POST",
                "/channels/5/messages",
                Some(json!({ "body": "hello from the shell" })),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        u64::from(r.status),
        s["response"]["status"].as_u64().unwrap()
    );
    assert_eq!(r.body, s["response"]["body"]);

    // An auth route: refused here, as the lane refuses it (nothing is sent).
    let s = step("auth_routes_are_not_proxied");
    assert_eq!(
        s["request"]["path"], "/sew/native/api/v1/auth/logout",
        "the fixture's path is the transport's"
    );
    let r = svc
        .api(None, call("POST", "/auth/logout", Some(json!({}))))
        .await
        .unwrap();
    assert_eq!(
        u64::from(r.status),
        s["response"]["status"].as_u64().unwrap()
    );
    assert!(
        shaped(&s["response"]["body"], &r.body),
        "{} is not {}",
        r.body,
        s["response"]["body"]
    );
    replay.done();

    // The gateway: the ticket from the lane, the socket the fixture names with the bearer.
    replay.expect(&["ticket"]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let ev = events.clone();
    let g = svc
        .gateway_open(
            None,
            Arc::new(move |e: GatewayEvent| ev.lock().unwrap().push(e)),
        )
        .await
        .unwrap();
    replay.done();
    let s = step("ticket");
    let saved = replay.saved.lock().unwrap().clone();
    let asked = sockets.asked.lock().unwrap().clone();
    assert_eq!(
        asked,
        vec![(
            s["response"]["body"]["url"].as_str().unwrap().to_owned(),
            saved["bearer"].clone()
        )]
    );
    svc.gateway_send(
        &g.id,
        r#"{"op":2,"d":{"ticket":"native","client":{"name":"t","version":"1"},"capabilities":1}}"#,
    )
    .await
    .unwrap();
    // The fixture's ticket is "<any>", answered "x" by the replay.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while sockets.frames.lock().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(sockets.frames.lock().unwrap()[0]["d"]["ticket"], "x");
    let seen = serde_json::to_string(&*events.lock().unwrap()).unwrap();
    assert!(!seen.contains(&saved["bearer"]));

    // The session ended: the fixture's 401, passed to the page, and the account marked.
    replay.expect(&["signed_out_after"]);
    let s = step("signed_out_after");
    let r = svc.api(None, call("GET", "/me", None)).await.unwrap();
    assert_eq!(
        u64::from(r.status),
        s["response"]["status"].as_u64().unwrap()
    );
    assert_eq!(r.body, s["response"]["body"]);
    replay.done();
    assert_eq!(
        svc.book().account(&acct.key).unwrap().state,
        AccountState::SignedOut {
            reason: "signed_out".into()
        }
    );
    // The plugin's own answer for it has the same shape.
    let again = svc.api(None, call("GET", "/me", None)).await.unwrap();
    assert_eq!(again.body, s["response"]["body"]);
    replay.done();
}
