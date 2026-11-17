//! The API transport on the fake lane: `api` with the bearer kept in Rust, the lane's
//! allowlist and refusals, the gateway socket relayed to a page (the ticket put in by the
//! plugin, a reconnect after a drop, at most two sockets per account), uploads and the signed
//! byte routes, and a scan that no bearer or ticket reaches the page.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tauri_plugin_sew_app::fake::{FakeLane, RecordingAppSink};
use tauri_plugin_sew_app::{
    AccountKey, AccountState, ApiCall, AppConfig, AppService, ConnectError, GatewayEvent,
    GatewaySink, LaneClient, Origin, Secret, SecretChoice, SocketConnector, SocketPipe, Upload,
    UploadData, code,
};
use tauri_plugin_sew_voice::BoxFuture;

const ORIGIN: &str = "https://chat.example.com";

struct Rig {
    lane: FakeLane,
    svc: AppService,
    sink: Arc<RecordingAppSink>,
    key: AccountKey,
}

async fn rig() -> Rig {
    let lane = FakeLane::new();
    let mut cfg = AppConfig::new("com.example.transport");
    cfg.secrets = SecretChoice::Memory;
    let sink = Arc::new(RecordingAppSink::default());
    let svc = AppService::new(cfg, LaneClient::new(Arc::new(lane.clone())), sink.clone()).unwrap();
    svc.set_socket_connector(Arc::new(lane.clone()));
    let origin = Origin::parse(ORIGIN, false).unwrap();
    svc.add_server(ORIGIN).await.unwrap();
    svc.confirm_server(&origin).unwrap();
    let a = svc
        .sign_in_password(&origin, "bob", Secret::new(FakeLane::BOB_PASSWORD), None)
        .await
        .unwrap();
    Rig {
        lane,
        svc,
        sink,
        key: a.key,
    }
}

fn call(method: &str, path: &str, body: Option<Value>) -> ApiCall {
    ApiCall {
        method: method.into(),
        path: path.into(),
        body,
        idempotency_key: None,
    }
}

/// A page's channel: every event, in order.
#[derive(Clone, Default)]
struct Page(Arc<Mutex<Vec<GatewayEvent>>>);

impl Page {
    fn sink(&self) -> GatewaySink {
        let p = self.0.clone();
        Arc::new(move |e| p.lock().unwrap().push(e))
    }

    fn events(&self) -> Vec<GatewayEvent> {
        self.0.lock().unwrap().clone()
    }

    fn messages(&self) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                GatewayEvent::Message { data } => Some(serde_json::from_str(&data).unwrap()),
                _ => None,
            })
            .collect()
    }

    fn closed(&self) -> Option<(u16, String, bool)> {
        self.events().into_iter().find_map(|e| match e {
            GatewayEvent::Close {
                code,
                reason,
                clean,
            } => Some((code, reason, clean)),
            _ => None,
        })
    }

    async fn wait(&self, what: &str, f: impl Fn(&Page) -> bool) {
        let r = tokio::time::timeout(Duration::from_secs(5), async {
            while !f(self) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(r.is_ok(), "waited for {what}: {:?}", self.events());
    }

    async fn wait_op(&self, t: &str) {
        self.wait(t, |p| {
            p.messages().iter().any(|m| {
                m["t"] == t || (t == "HELLO" && m["op"] == 8) || (t == "ACK" && m["op"] == 9)
            })
        })
        .await;
    }
}

fn identify() -> String {
    json!({ "op": 2, "d": { "ticket": "native", "client": { "name": "test", "version": "1" },
                            "capabilities": 1 } })
    .to_string()
}

#[tokio::test]
async fn api_calls_carry_the_bearer_and_answer_only_safe_headers() {
    let r = rig().await;
    let bearer = r.lane.bearers().pop().unwrap();
    let me = r.svc.api(None, call("GET", "/me", None)).await.unwrap();
    assert_eq!(me.status, 200);
    assert_eq!(me.body["id"], "1002");
    assert_eq!(me.body["display_name"], "Bob");
    assert_eq!(me.headers["x-ratelimit-remaining"], "29");
    assert!(me.headers["content-type"].starts_with("application/json"));
    assert!(!me.headers.contains_key("set-cookie"));
    assert!(!me.headers.contains_key("www-authenticate"));
    let seen = r.lane.seen_bytes();
    let last = seen.last().unwrap();
    assert_eq!(
        (last.method, last.path.as_str()),
        ("GET", "/sew/native/api/v1/me")
    );
    assert_eq!(last.bearer.as_deref(), Some(bearer.as_str()));
    assert!(
        last.headers
            .iter()
            .all(|(k, _)| k != "authorization" && k != "origin")
    );

    // A POST with an idempotency key: the second is the first's answer, replayed.
    let mut post = call(
        "POST",
        "/channels/5/messages",
        Some(json!({ "body": "hello" })),
    );
    post.idempotency_key = Some("k-1".into());
    let a = r.svc.api(None, post.clone()).await.unwrap();
    assert_eq!(a.status, 201);
    assert_eq!(a.body["body"], "hello");
    let b = r.svc.api(Some(r.key.clone()), post).await.unwrap();
    assert_eq!(b.body["id"], a.body["id"]);
    assert_eq!(b.headers["idempotent-replayed"], "true");
    let seen = r.lane.seen_bytes();
    let last = seen.last().unwrap();
    assert!(
        last.headers
            .contains(&("idempotency-key".into(), "k-1".into()))
    );
    assert!(
        last.headers
            .contains(&("content-type".into(), "application/json".into()))
    );

    // The lane's refusals, written here: nothing is sent.
    let before = r.lane.seen_bytes().len();
    for (m, p, status, code) in [
        ("POST", "/auth/logout", 403, "forbidden"),
        ("POST", "/gateway/ticket", 403, "forbidden"),
        ("GET", "/gateway", 426, "upgrade_required"),
        ("DELETE", "/me", 405, "method_not_allowed"),
        ("GET", "/a/../me", 404, "not_found"),
        ("GET", "/files/9?exp=1&sig=2", 404, "not_found"),
        ("POST", "/me/avatar", 415, "unsupported_media_type"),
    ] {
        let x = r.svc.api(None, call(m, p, None)).await.unwrap();
        assert_eq!(
            (x.status, x.body["code"].as_str()),
            (status, Some(code)),
            "{m} {p}"
        );
        assert_eq!(x.body["ok"], false);
        assert_eq!(x.body["error"]["code"], code);
    }
    let big = "x".repeat(300 << 10);
    let x = r
        .svc
        .api(
            None,
            call("POST", "/channels/5/messages", Some(json!({ "body": big }))),
        )
        .await
        .unwrap();
    assert_eq!(
        (x.status, x.body["detail"]["max"].as_u64()),
        (413, Some(256 << 10))
    );
    assert_eq!(r.lane.seen_bytes().len(), before);

    // Bad arguments are errors.
    let e = r
        .svc
        .api(None, call("GET", "/me", Some(json!({ "a": 1 }))))
        .await
        .unwrap_err();
    assert_eq!(e.code, code::INVALID_ARGS);
    let mut k = call("POST", "/channels/5/messages", None);
    k.idempotency_key = Some("has space".into());
    assert_eq!(
        r.svc.api(None, k).await.unwrap_err().code,
        code::INVALID_ARGS
    );

    // An unknown route is the server's own 404.
    let x = r
        .svc
        .api(None, call("GET", "/nothing/here", None))
        .await
        .unwrap();
    assert_eq!(
        (x.status, x.body["code"].as_str()),
        (404, Some("not_found"))
    );
}

#[tokio::test]
async fn an_ended_session_answers_401_and_marks_the_account() {
    let r = rig().await;
    r.lane.revoke_device_id(&r.key.account, "revoked");
    let x = r.svc.api(None, call("GET", "/me", None)).await.unwrap();
    assert_eq!(x.status, 401);
    assert_eq!(x.body["code"], "signed_out");
    assert_eq!(x.body["detail"]["reason"], "revoked");
    assert_eq!(
        r.svc.book().account(&r.key).unwrap().state,
        AccountState::SignedOut {
            reason: "revoked".into()
        }
    );
    assert!(r.sink.names().contains(&"signed_out"));
    // Next time nothing is sent: the plugin answers the lane's 401 itself.
    let before = r.lane.seen_bytes().len();
    let y = r.svc.api(None, call("GET", "/me", None)).await.unwrap();
    assert_eq!(
        (y.status, y.body["code"].as_str()),
        (401, Some("signed_out"))
    );
    assert_eq!(y.body["detail"]["reason"], "revoked");
    assert_eq!(r.lane.seen_bytes().len(), before);
    // And no account at all is an error.
    let none = AccountKey {
        server: r.key.server.clone(),
        account: "nobody".into(),
    };
    let e = r
        .svc
        .api(Some(none), call("GET", "/me", None))
        .await
        .unwrap_err();
    assert_eq!(e.code, code::UNKNOWN_ACCOUNT);
}

#[tokio::test]
async fn a_burst_of_calls_on_a_cold_cache_all_answer() {
    let r = rig().await;
    let mut calls = Vec::new();
    for _ in 0..24 {
        let svc = r.svc.clone();
        calls.push(tokio::spawn(async move {
            svc.api(None, call("GET", "/me", None))
                .await
                .unwrap()
                .status
        }));
    }
    for c in calls {
        assert_eq!(c.await.unwrap(), 200);
    }
}

#[tokio::test]
async fn the_gateway_is_relayed_and_the_ticket_is_put_in_here() {
    let r = rig().await;
    let page = Page::default();
    let opened = r.svc.gateway_open(None, page.sink()).await.unwrap();
    assert_eq!(
        page.events().first(),
        Some(&GatewayEvent::Open {
            id: opened.id.clone()
        })
    );
    page.wait_op("HELLO").await;
    r.svc.gateway_send(&opened.id, &identify()).await.unwrap();
    page.wait_op("READY").await;
    // The server got the ticket the lane minted; the page sent only a placeholder.
    let frames = r.lane.gateway_frames();
    let (_, ident) = frames.iter().find(|(_, f)| f["op"] == 2).unwrap();
    let tickets = r.lane.tickets();
    assert_eq!(tickets.len(), 1);
    assert_eq!(ident["d"]["ticket"], tickets[0].as_str());
    assert_eq!(ident["d"]["client"]["name"], "test");
    assert!(ident["d"].get("token").is_none());
    // A heartbeat goes through as it is.
    r.svc
        .gateway_send(&opened.id, r#"{"op":1,"d":1}"#)
        .await
        .unwrap();
    page.wait_op("ACK").await;
    // A REAUTH gets a fresh ticket in `token`; the server spends it (no close follows).
    r.svc
        .gateway_send(&opened.id, r#"{"op":11,"d":{"token":"native"}}"#)
        .await
        .unwrap();
    // The server reads it on its own task.
    let lane = r.lane.clone();
    page.wait("the REAUTH at the server", move |_| {
        lane.gateway_frames().iter().any(|(_, f)| f["op"] == 11)
    })
    .await;
    let frames = r.lane.gateway_frames();
    let (_, re) = frames.iter().find(|(_, f)| f["op"] == 11).unwrap();
    assert_eq!(r.lane.tickets().len(), 2);
    assert_eq!(re["d"]["token"], r.lane.tickets()[1].as_str());
    // A dispatch from the server reaches the page as it came.
    r.lane
        .push_dispatch("MESSAGE_CREATE", json!({ "id": "77" }));
    page.wait_op("MESSAGE_CREATE").await;
    assert!(page.closed().is_none());
    // Bad frames are refused here.
    let e = r
        .svc
        .gateway_send(&opened.id, "not json")
        .await
        .unwrap_err();
    assert_eq!(e.code, code::INVALID_ARGS);
    let e = r
        .svc
        .gateway_send("gw_nope", &identify())
        .await
        .unwrap_err();
    assert_eq!(e.code, code::UNKNOWN_SOCKET);
    // The page closes it; the server's echo is the close event.
    assert!(
        r.svc
            .gateway_close(&opened.id, Some(4000), Some("bye".into()))
            .unwrap()
    );
    page.wait("close", |p| p.closed().is_some()).await;
    assert_eq!(page.closed(), Some((4000, "bye".into(), true)));
    assert_eq!(r.svc.gateway_count(&r.key), 0);
    assert!(!r.svc.gateway_close(&opened.id, None, None).unwrap());
    assert!(r.svc.gateway_close(&opened.id, Some(1001), None).is_err());
}

#[tokio::test]
async fn the_page_reconnects_after_a_drop() {
    let r = rig().await;
    let first = Page::default();
    let a = r.svc.gateway_open(None, first.sink()).await.unwrap();
    first.wait_op("HELLO").await;
    r.svc.gateway_send(&a.id, &identify()).await.unwrap();
    first.wait_op("READY").await;

    // The network drops: a close without a frame, 1006.
    r.lane.drop_gateways();
    first.wait("close", |p| p.closed().is_some()).await;
    assert_eq!(first.closed(), Some((1006, String::new(), false)));
    assert_eq!(r.svc.gateway_count(&r.key), 0);
    assert_eq!(r.lane.open_gateways(), 0);
    assert!(r.svc.gateway_send(&a.id, &identify()).await.is_err());

    // The page's driver opens a new socket and resumes with a new ticket.
    let second = Page::default();
    let b = r.svc.gateway_open(None, second.sink()).await.unwrap();
    assert_ne!(a.id, b.id);
    second.wait_op("HELLO").await;
    let resume = json!({ "op": 4, "d": { "ticket": "native", "session_id": "gs_0", "seq": 1 } });
    r.svc
        .gateway_send(&b.id, &resume.to_string())
        .await
        .unwrap();
    second.wait_op("RESUMED").await;
    let frames = r.lane.gateway_frames();
    let (_, res) = frames.iter().find(|(_, f)| f["op"] == 4).unwrap();
    assert_eq!(res["d"]["ticket"], r.lane.tickets()[1].as_str());

    // A server close keeps its code and reason.
    r.lane.close_gateways(4009, "session timed out");
    second.wait("close", |p| p.closed().is_some()).await;
    assert_eq!(
        second.closed(),
        Some((4009, "session timed out".into(), true))
    );
}

#[tokio::test]
async fn two_sockets_per_account_at_most() {
    let r = rig().await;
    let (p1, p2, p3) = (Page::default(), Page::default(), Page::default());
    let a = r.svc.gateway_open(None, p1.sink()).await.unwrap();
    let _b = r.svc.gateway_open(None, p2.sink()).await.unwrap();
    assert_eq!(r.svc.gateway_count(&r.key), 2);
    let tickets = r.lane.tickets().len();
    let e = r.svc.gateway_open(None, p3.sink()).await.unwrap_err();
    assert_eq!(e.code, "rate_limited");
    assert_eq!(e.detail["max"], 2);
    assert!(p3.events().is_empty());
    assert_eq!(
        r.lane.tickets().len(),
        tickets,
        "no ticket for a refused socket"
    );
    // Once one closes, a new one opens (the overlap of a reconnect).
    r.svc.gateway_close(&a.id, None, None).unwrap();
    p1.wait("close", |p| p.closed().is_some()).await;
    r.svc.gateway_open(None, p3.sink()).await.unwrap();
    assert_eq!(r.svc.gateway_count(&r.key), 2);
    assert_eq!(r.lane.open_gateways(), 2);
}

/// A connector that refuses every upgrade with this answer.
struct Refusing(u16, Option<u64>, Value);

impl SocketConnector for Refusing {
    fn connect<'a>(
        &'a self,
        _url: &'a url::Url,
        _bearer: &'a Secret,
    ) -> BoxFuture<'a, Result<SocketPipe, ConnectError>> {
        Box::pin(std::future::ready(Err(ConnectError::Refused {
            status: self.0,
            retry_after_ms: self.1,
            body: serde_json::to_vec(&self.2).unwrap(),
        })))
    }
}

#[tokio::test]
async fn a_refused_upgrade_has_the_lanes_code() {
    let r = rig().await;
    let busy = json!({ "ok": false, "code": "rate_limited", "detail": null,
                       "error": { "code": "rate_limited", "hint": "too many gateway sockets" } });
    r.svc
        .set_socket_connector(Arc::new(Refusing(429, Some(1000), busy)));
    let page = Page::default();
    let e = r.svc.gateway_open(None, page.sink()).await.unwrap_err();
    assert_eq!(e.code, "rate_limited");
    assert_eq!(e.detail["retry_after_ms"], 1000);
    assert!(page.events().is_empty());
    assert_eq!(r.svc.gateway_count(&r.key), 0, "the slot is given back");

    let ended = json!({ "ok": false, "code": "signed_out", "detail": { "reason": "idle" },
                        "error": { "code": "signed_out", "hint": "Sign in again." } });
    r.svc
        .set_socket_connector(Arc::new(Refusing(401, None, ended)));
    let e = r.svc.gateway_open(None, page.sink()).await.unwrap_err();
    assert_eq!(e.code, "signed_out");
    assert_eq!(e.detail["reason"], "idle");
    assert!(matches!(
        r.svc.book().account(&r.key).unwrap().state,
        AccountState::SignedOut { .. }
    ));
}

#[tokio::test]
async fn signing_out_closes_the_sockets() {
    let r = rig().await;
    let page = Page::default();
    r.svc.gateway_open(None, page.sink()).await.unwrap();
    page.wait_op("HELLO").await;
    r.svc.sign_out(&r.key).await.unwrap();
    page.wait("close", |p| p.closed().is_some()).await;
    let (code, _, _) = page.closed().unwrap();
    assert!(code == 1000 || code == 4011, "{code}");
    assert_eq!(r.svc.gateway_count(&r.key), 0);
}

#[tokio::test]
async fn uploads_and_byte_routes_through_the_fake_lane() {
    let r = rig().await;
    let png: Vec<u8> = (0u8..=200).collect();
    let up = r
        .svc
        .upload(
            None,
            Upload {
                path: "/uploads/9001/data".into(),
                content_type: Some("image/png".into()),
                content_range: None,
                data: UploadData::Bytes(png.clone()),
            },
        )
        .await
        .unwrap();
    assert_eq!(up.status, 200);
    assert_eq!(up.body["size"], png.len());
    assert_eq!(
        r.lane.uploaded("9001").unwrap(),
        (png.clone(), "image/png".into())
    );
    let seen = r.lane.seen_bytes();
    let last = seen.last().unwrap();
    assert_eq!(last.path, "/sew/native/api/v1/uploads/9001/data");
    assert!(last.bearer.is_some());

    // A file read in Rust.
    let dir = std::env::temp_dir().join(format!("sew-app-upload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("note.bin");
    std::fs::write(&f, b"file bytes").unwrap();
    let up = r
        .svc
        .upload(
            Some(r.key.clone()),
            Upload {
                path: "/uploads/9002/data".into(),
                content_type: None,
                content_range: Some("bytes 0-9/10".into()),
                data: UploadData::File(f.clone()),
            },
        )
        .await
        .unwrap();
    assert_eq!(up.status, 200);
    assert_eq!(
        r.lane.uploaded("9002").unwrap(),
        (b"file bytes".to_vec(), "application/octet-stream".into())
    );
    let seen = r.lane.seen_bytes();
    assert!(
        seen.last()
            .unwrap()
            .headers
            .contains(&("content-range".into(), "bytes 0-9/10".into()))
    );
    // Too large, a missing file, a JSON type, another route: refused here.
    let before = r.lane.seen_bytes().len();
    let big = r
        .svc
        .upload(
            None,
            Upload {
                path: "/spaces/2/emoji".into(),
                content_type: Some("image/png".into()),
                content_range: None,
                data: UploadData::Bytes(vec![0; (256 << 10) + 1]),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (big.status, big.body["code"].as_str()),
        (413, Some("too_large"))
    );
    let bad = |path: &str, ct: &str| Upload {
        path: path.into(),
        content_type: Some(ct.into()),
        content_range: None,
        data: UploadData::Bytes(vec![1, 2, 3]),
    };
    let x = r
        .svc
        .upload(None, bad("/uploads/9/data", "application/json"))
        .await
        .unwrap();
    assert_eq!(x.status, 415);
    let x = r
        .svc
        .upload(
            None,
            bad("/channels/5/messages", "application/octet-stream"),
        )
        .await
        .unwrap();
    assert_eq!(x.status, 404);
    let missing = r
        .svc
        .upload(
            None,
            Upload {
                path: "/uploads/9/data".into(),
                content_type: None,
                content_range: None,
                data: UploadData::File(dir.join("missing.bin")),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(missing.code, code::FILE);
    assert_eq!(r.lane.seen_bytes().len(), before);
    std::fs::remove_dir_all(&dir).unwrap();

    // The signed byte route, whole, a part, and HEAD; Sew's headers but cookies and hop-by-hop.
    let whole = r
        .svc
        .fetch_bytes("active", "GET", "/files/9001?exp=1&sig=abc", &[])
        .await;
    assert_eq!(whole.status, 200);
    assert_eq!(whole.body, png);
    assert_eq!(whole.header("content-type"), Some("image/png"));
    assert_eq!(
        whole.header("content-disposition"),
        Some("inline; filename=\"f.bin\"")
    );
    assert_eq!(whole.header("x-content-type-options"), Some("nosniff"));
    for h in ["set-cookie", "connection", "x-hop"] {
        assert!(whole.header(h).is_none(), "{h}");
    }
    let part = r
        .svc
        .fetch_bytes(
            &r.key.account,
            "GET",
            "/files/9001?exp=1&sig=abc",
            &[
                ("Range".into(), "bytes=0-3".into()),
                ("Cookie".into(), "no=1".into()),
            ],
        )
        .await;
    assert_eq!(part.status, 206);
    assert_eq!(part.body, png[..4].to_vec());
    assert_eq!(part.header("content-range"), Some("bytes 0-3/201"));
    let sent = r.lane.seen_bytes();
    let sent = sent.last().unwrap();
    assert_eq!(
        sent.headers,
        vec![("range".to_owned(), "bytes=0-3".to_owned())]
    );
    let head = r
        .svc
        .fetch_bytes("active", "HEAD", "/files/9001?exp=1&sig=abc", &[])
        .await;
    assert_eq!((head.status, head.body.len()), (200, 0));
    assert_eq!(head.header("content-length"), Some("201"));
    let thumb = r
        .svc
        .fetch_bytes("active", "GET", "/files/9001/thumb", &[])
        .await;
    assert_eq!((thumb.status, thumb.body.as_slice()), (200, &b"thumb"[..]));

    // Refused here: unsigned, outside the list, never served, another method, no account.
    let before = r.lane.seen_bytes().len();
    for (m, p, status) in [
        ("GET", "/files/9001", 404),
        ("GET", "/me", 404),
        ("GET", "/recordings/1/audio", 404),
        ("POST", "/files/9001?exp=1&sig=abc", 405),
    ] {
        let x = r.svc.fetch_bytes("active", m, p, &[]).await;
        assert_eq!(x.status, status, "{m} {p}");
        let v: Value = serde_json::from_slice(&x.body).unwrap();
        assert_eq!(v["ok"], false);
    }
    let x = r
        .svc
        .fetch_bytes("nobody", "GET", "/files/9001?exp=1&sig=abc", &[])
        .await;
    assert_eq!(x.status, 404);
    assert_eq!(r.lane.seen_bytes().len(), before);
    // An unknown file is the server's 404.
    let x = r
        .svc
        .fetch_bytes("active", "GET", "/files/1?exp=1&sig=abc", &[])
        .await;
    assert_eq!(x.status, 404);
}

/// Everything a page could see in one session, as text.
fn scan(page_saw: &[String], bearers: &[String], tickets: &[String]) {
    assert!(!bearers.is_empty() && !tickets.is_empty());
    for s in page_saw {
        for b in bearers {
            assert!(!s.contains(b.as_str()), "a bearer reached the page: {s}");
        }
        for t in tickets {
            assert!(!s.contains(t.as_str()), "a ticket reached the page: {s}");
        }
        assert!(
            !s.contains("sewdv_"),
            "a bearer's prefix reached the page: {s}"
        );
        assert!(
            !s.contains("sewtk_"),
            "a ticket's prefix reached the page: {s}"
        );
    }
}

#[tokio::test]
async fn no_bearer_or_ticket_reaches_the_page() {
    let r = rig().await;
    let mut saw: Vec<String> = Vec::new();
    let mut keep = |v: &dyn erased::Show| saw.push(v.show());

    keep(&r.svc.servers());
    keep(&r.svc.api(None, call("GET", "/me", None)).await.unwrap());
    keep(
        &r.svc
            .api(
                None,
                call(
                    "POST",
                    "/channels/5/messages",
                    Some(json!({ "body": "hi" })),
                ),
            )
            .await
            .unwrap(),
    );
    keep(
        &r.svc
            .api(None, call("POST", "/auth/logout", None))
            .await
            .unwrap(),
    );
    let up = r
        .svc
        .upload(
            None,
            Upload {
                path: "/uploads/9001/data".into(),
                content_type: None,
                content_range: None,
                data: UploadData::Bytes(b"abc".to_vec()),
            },
        )
        .await
        .unwrap();
    keep(&up);
    let f = r
        .svc
        .fetch_bytes("active", "GET", "/files/9001?exp=1&sig=abc", &[])
        .await;
    keep(&format!(
        "{:?} {}",
        f.headers,
        String::from_utf8_lossy(&f.body)
    ));
    let refused = r.svc.fetch_bytes("active", "GET", "/me", &[]).await;
    keep(&format!(
        "{:?} {}",
        refused.headers,
        String::from_utf8_lossy(&refused.body)
    ));
    // The page's own URL to a page of the server (the handoff code is its own credential,
    // never a bearer).
    keep(&r.svc.open_url(&r.key, "/app").await.unwrap().to_string());

    let page = Page::default();
    let g = r.svc.gateway_open(None, page.sink()).await.unwrap();
    keep(&serde_json::to_string(&g).unwrap());
    page.wait_op("HELLO").await;
    r.svc.gateway_send(&g.id, &identify()).await.unwrap();
    page.wait_op("READY").await;
    r.svc
        .gateway_send(&g.id, r#"{"op":11,"d":{"token":"native"}}"#)
        .await
        .unwrap();
    r.lane.drop_gateways();
    page.wait("close", |p| p.closed().is_some()).await;
    let g2 = r.svc.gateway_open(None, page.sink()).await.unwrap();
    r.svc
        .gateway_send(
            &g2.id,
            &json!({ "op": 4, "d": { "ticket": "x", "session_id": "gs_0", "seq": 1 } }).to_string(),
        )
        .await
        .unwrap();
    page.wait_op("RESUMED").await;
    for e in page.events() {
        keep(&serde_json::to_string(&e).unwrap());
    }
    // The revoke's 401 and the events after it.
    r.lane.revoke_device_id(&r.key.account, "revoked");
    keep(&r.svc.api(None, call("GET", "/me", None)).await.unwrap());
    keep(&r.svc.servers());
    for e in r.sink.events() {
        keep(&e.payload().to_string());
    }
    // Errors, too.
    keep(&r.svc.gateway_open(None, page.sink()).await.unwrap_err());
    drop(keep);
    scan(&saw, &r.lane.bearers(), &r.lane.tickets());
    assert!(
        r.lane.tickets().len() >= 3,
        "IDENTIFY, REAUTH and RESUME each got a ticket"
    );
}

/// A small helper so one closure takes every answer type as text.
mod erased {
    pub(crate) trait Show {
        fn show(&self) -> String;
    }
    impl<T: serde::Serialize> Show for T {
        fn show(&self) -> String {
            serde_json::to_string(self).unwrap()
        }
    }
}
