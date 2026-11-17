//! The host's sign-in door done in Rust (ping, the lane's begin, the password to the host's
//! own `/.k2/auth/sign-in`, the lane's finish), each of its refusals, the shell's version
//! skew, pairing links, and the rule that no secret reaches the webview.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value, json};
use tauri_plugin_sew_app::deeplink::{DeepLinkRoute, route};
use tauri_plugin_sew_app::fake::{FakeLane, RecordingAppSink, k2_error, k2_rate_limited};
use tauri_plugin_sew_app::lane::route as lane_route;
use tauri_plugin_sew_app::{
    AppConfig, AppError, AppService, LaneClient, LaneResponse, Origin, Secret, SecretChoice, code,
};

const APP: &str = "https://myapp.example.com";
const PW: &str = FakeLane::BOB_PASSWORD;

struct World {
    svc: AppService,
    lane: FakeLane,
    sink: Arc<RecordingAppSink>,
    origin: Origin,
}

fn world_with(f: impl FnOnce(&mut AppConfig)) -> World {
    let lane = FakeLane::new();
    lane.set_info(|i| i.signin.door = true);
    let mut cfg = AppConfig::new("com.example.door");
    cfg.secrets = SecretChoice::Memory;
    f(&mut cfg);
    let sink = Arc::new(RecordingAppSink::default());
    let svc = AppService::new(cfg, LaneClient::new(Arc::new(lane.clone())), sink.clone()).unwrap();
    World {
        svc,
        lane,
        sink,
        origin: Origin::parse(APP, false).unwrap(),
    }
}

async fn world() -> World {
    let w = world_with(|_| {});
    w.svc.add_server(APP).await.unwrap();
    w.svc.confirm_server(&w.origin).unwrap();
    w
}

fn json_answer(status: u16, body: Value) -> LaneResponse {
    LaneResponse {
        status,
        retry_after_ms: None,
        body: serde_json::to_vec(&body).unwrap(),
    }
}

impl World {
    fn count(&self, path: &str) -> usize {
        self.lane.seen().iter().filter(|r| r.path == path).count()
    }

    async fn door(&self, password: &str) -> Result<tauri_plugin_sew_app::AccountView, AppError> {
        self.svc
            .sign_in_door(&self.origin, "bob", Secret::new(password), None)
            .await
    }

    /// No password went anywhere.
    fn no_password_sent(&self) {
        assert_eq!(self.count(lane_route::HOST_DOOR_SIGN_IN), 0);
        assert_eq!(self.count(lane_route::SIGNIN_PASSWORD), 0);
        for r in self.lane.seen() {
            assert!(
                !r.body.as_ref().is_some_and(|b| b.to_string().contains(PW)),
                "{} carried the password",
                r.path
            );
        }
    }
}

#[tokio::test]
async fn the_door_signs_in_from_rust_alone() {
    let w = world().await;
    let a = w.door(PW).await.unwrap();
    assert_eq!(a.member_id, "1002");
    assert_eq!(a.label.as_deref(), Some("Bob"));
    assert!(a.active);

    let seen = w.lane.seen();
    let paths: Vec<(&str, &str)> = seen.iter().map(|r| (r.method, r.path.as_str())).collect();
    let at = paths
        .iter()
        .position(|p| *p == ("GET", "/.k2/auth/ping"))
        .unwrap();
    assert_eq!(
        &paths[at..],
        &[
            ("GET", "/.k2/auth/ping"),
            ("POST", "/sew/native/signin/door/begin"),
            ("POST", "/.k2/auth/sign-in"),
            ("POST", "/sew/native/signin/door/finish"),
            ("GET", "/sew/native/whoami"),
        ],
        "ping, begin, the password to the host, finish"
    );
    for r in &seen {
        assert_eq!(r.origin, APP, "everything goes to the confirmed server");
    }
    let begin = &seen[at + 1];
    assert_eq!(begin.body.as_ref().unwrap()["device"]["id"], a.key.account);
    assert!(begin.bearer.is_none());
    let keys = |v: &Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    let sign_in = seen[at + 2].body.clone().unwrap();
    assert_eq!(keys(&sign_in), ["challenge", "password", "username"]);
    assert_eq!(sign_in["username"], "bob");
    assert_eq!(sign_in["password"], PW);
    assert_eq!(sign_in["challenge"].as_str().unwrap().len(), 43);
    let finish = seen[at + 3].body.clone().unwrap();
    assert_eq!(keys(&finish), ["code", "device", "id"]);
    assert!(finish["code"].as_str().unwrap().starts_with("k2lgn_"));
    assert_eq!(finish["id"].as_str().unwrap().len(), 43);
    assert_eq!(finish["device"]["id"], a.key.account);
    assert_eq!(w.lane.open_door_begins(), 0, "the begin is spent");
    assert_eq!(
        w.lane.live_sessions(),
        vec![("1002".into(), a.key.account.clone())]
    );

    // Signing the same account in again keeps its device id.
    let again = w
        .svc
        .sign_in_door(&w.origin, "bob", Secret::new(PW), Some(&a.key.account))
        .await
        .unwrap();
    assert_eq!(again.key, a.key);
}

#[tokio::test]
async fn refused_when_the_door_is_not_offered_or_the_ping_is_not_k2() {
    // Not offered by the lane.
    let w = world().await;
    w.lane.set_info(|i| i.signin.door = false);
    let e = w.door(PW).await.unwrap_err();
    assert_eq!(e.code, code::SIGNIN_DOOR_UNAVAILABLE);
    assert_eq!(w.count(lane_route::SIGNIN_BEGIN), 0);
    w.no_password_sent();

    // Offered, but the host doesn't answer its ping.
    let w = world().await;
    w.lane.set_host_door(false);
    let e = w.door(PW).await.unwrap_err();
    assert_eq!(e.code, code::SIGNIN_DOOR_UNAVAILABLE);
    assert_eq!(e.detail, json!({ "reason": "ping" }));
    assert_eq!(
        w.count(lane_route::SIGNIN_BEGIN),
        0,
        "no begin without the ping"
    );
    w.no_password_sent();

    // Anything but exactly {"k2": true}.
    for body in [
        json!({ "k2": "true" }),
        json!({ "k2": 1 }),
        json!({}),
        json!([true]),
    ] {
        let w = world().await;
        w.lane
            .answer_next(lane_route::HOST_DOOR_PING, json_answer(200, body.clone()));
        let e = w.door(PW).await.unwrap_err();
        assert_eq!(e.code, code::SIGNIN_DOOR_UNAVAILABLE, "{body}");
        w.no_password_sent();
    }

    // A server never confirmed.
    let w = world_with(|_| {});
    w.svc.add_server(APP).await.unwrap();
    let e = w.door(PW).await.unwrap_err();
    assert_eq!(e.code, code::UNKNOWN_SERVER);
    w.no_password_sent();
}

#[tokio::test]
async fn refused_when_the_begin_names_another_door() {
    let id = "i".repeat(43);
    let ch = "c".repeat(43);
    for door in [
        "https://evil.example/.k2/auth/sign-in",
        "https://myapp.example.com.evil.example/.k2/auth/sign-in",
        "http://myapp.example.com/.k2/auth/sign-in",
        "https://myapp.example.com:8443/.k2/auth/sign-in",
        "https://myapp.example.com/.k2/auth/sign-in/../../steal",
        "https://myapp.example.com/elsewhere",
        "not a url",
    ] {
        let w = world().await;
        w.lane.answer_next(
            lane_route::SIGNIN_BEGIN,
            json_answer(
                200,
                json!({ "ok": true, "id": id, "challenge": ch, "door": door,
                        "expires_in_ms": 600000 }),
            ),
        );
        let e = w.door(PW).await.unwrap_err();
        assert_eq!(e.code, code::SIGNIN_DOOR_MISMATCH, "{door}");
        assert_eq!(e.detail["expected"], format!("{APP}/.k2/auth/sign-in"));
        w.no_password_sent();
    }
    // The door the lane names, in another spelling of the same URL, is fine.
    let w = world().await;
    w.lane.answer_next(
        lane_route::SIGNIN_BEGIN,
        json_answer(
            200,
            json!({ "ok": true, "id": id, "challenge": ch,
                    "door": "HTTPS://MyApp.Example.com:443/.k2/auth/sign-in" }),
        ),
    );
    let e = w.door(PW).await.unwrap_err();
    // The fake lane never made that begin, so its finish refuses: the password did go.
    assert_eq!(e.code, "code_invalid");
    assert_eq!(w.count(lane_route::HOST_DOOR_SIGN_IN), 1);
}

#[tokio::test]
async fn refused_when_the_begin_is_malformed() {
    for body in [
        json!({ "ok": true, "id": "short", "challenge": "c".repeat(43), "door": format!("{APP}/.k2/auth/sign-in") }),
        json!({ "ok": true, "id": "i".repeat(43), "challenge": "c=".repeat(30), "door": format!("{APP}/.k2/auth/sign-in") }),
        json!({ "ok": true, "state": "i".repeat(43), "door": format!("{APP}/.k2/auth/sign-in") }),
    ] {
        let w = world().await;
        w.lane
            .answer_next(lane_route::SIGNIN_BEGIN, json_answer(200, body.clone()));
        let e = w.door(PW).await.unwrap_err();
        assert_eq!(e.code, code::BAD_ANSWER, "{body}");
        w.no_password_sent();
    }
    // The lane's own refusal keeps its code.
    let w = world().await;
    w.lane.answer_next(
        lane_route::SIGNIN_BEGIN,
        tauri_plugin_sew_app::fake::lane_error(429, "rate_limited", Value::Null, "Wait."),
    );
    assert_eq!(w.door(PW).await.unwrap_err().code, "rate_limited");
    w.no_password_sent();
}

#[tokio::test]
async fn bad_login_keeps_the_begin_for_the_retry() {
    let w = world().await;
    let e = w.door("wrong-password").await.unwrap_err();
    assert_eq!(e.code, "bad_login");
    assert_eq!(
        e.hint.as_deref(),
        Some("The username or password is wrong.")
    );
    assert_eq!(w.count(lane_route::SIGNIN_BEGIN), 1);
    let a = w.door(PW).await.unwrap();
    assert_eq!(a.member_id, "1002");
    assert_eq!(
        w.count(lane_route::SIGNIN_BEGIN),
        1,
        "the retry reused the begin"
    );
    let challenges: Vec<Value> = w
        .lane
        .seen()
        .into_iter()
        .filter(|r| r.path == lane_route::HOST_DOOR_SIGN_IN)
        .map(|r| r.body.unwrap()["challenge"].clone())
        .collect();
    assert_eq!(challenges.len(), 2);
    assert_eq!(challenges[0], challenges[1], "the same challenge");
}

#[tokio::test]
async fn locked_is_the_hosts_code() {
    let w = world().await;
    w.lane.lock_user("bob", true);
    let e = w.door(PW).await.unwrap_err();
    assert_eq!(e.code, "locked");
    assert!(w.lane.live_sessions().is_empty());
}

#[tokio::test]
async fn rate_limited_honours_retry_after() {
    let w = world().await;
    w.lane
        .answer_next(lane_route::HOST_DOOR_SIGN_IN, k2_rate_limited(30));
    let e = w.door(PW).await.unwrap_err();
    assert_eq!(e.code, "rate_limited");
    assert_eq!(e.detail["retry_after_ms"], 30000);
    let posts = w.count(lane_route::HOST_DOOR_SIGN_IN);
    let pings = w.count(lane_route::HOST_DOOR_PING);
    // Until then nothing goes to the door.
    let e = w.door(PW).await.unwrap_err();
    assert_eq!(e.code, "rate_limited");
    let left = e.detail["retry_after_ms"].as_u64().unwrap();
    assert!((1..=30000).contains(&left), "{left}");
    assert_eq!(w.count(lane_route::HOST_DOOR_SIGN_IN), posts);
    assert_eq!(w.count(lane_route::HOST_DOOR_PING), pings);
    // retryAfterSecs alone (no header) is read too.
    let w = world().await;
    w.lane.answer_next(
        lane_route::HOST_DOOR_SIGN_IN,
        json_answer(
            429,
            json!({ "ok": false, "error": { "code": "rate_limited", "retryAfterSecs": 5 } }),
        ),
    );
    assert_eq!(w.door(PW).await.unwrap_err().detail["retry_after_ms"], 5000);
    // 503 retry (K2 restarting): a short wait.
    let w = world().await;
    w.lane.answer_next(
        lane_route::HOST_DOOR_SIGN_IN,
        k2_error(503, "retry", "Restarting."),
    );
    assert_eq!(w.door(PW).await.unwrap_err().code, "retry");
    assert_eq!(w.door(PW).await.unwrap_err().code, "rate_limited");
}

#[tokio::test]
async fn password_change_required_then_set_password() {
    let w = world().await;
    let token = w.lane.require_password_change("bob").unwrap();
    let e = w.door(PW).await.unwrap_err();
    assert_eq!(e.code, code::PASSWORD_CHANGE_REQUIRED);
    let shown = serde_json::to_string(&e).unwrap();
    assert!(
        !shown.contains(&token),
        "the change token stays in Rust: {shown}"
    );

    // The host's own refusal keeps the change for another try.
    let e = w
        .svc
        .sign_in_door_set_password(&w.origin, Secret::new("short"), None)
        .await
        .unwrap_err();
    assert_eq!(e.code, "password_too_short");
    let a = w
        .svc
        .sign_in_door_set_password(&w.origin, Secret::new("a-new-long-pw"), None)
        .await
        .unwrap();
    assert_eq!(a.member_id, "1002");
    assert_eq!(w.lane.password_of("bob").as_deref(), Some("a-new-long-pw"));
    let set: Vec<Value> = w
        .lane
        .seen()
        .into_iter()
        .filter(|r| r.path == lane_route::HOST_DOOR_SET_PASSWORD)
        .map(|r| r.body.unwrap())
        .collect();
    assert_eq!(set.len(), 2);
    assert_eq!(set[1]["token"], token);
    assert_eq!(set[1]["challenge"].as_str().unwrap().len(), 43);
    assert_eq!(
        w.count(lane_route::SIGNIN_BEGIN),
        1,
        "one begin for the whole sign-in"
    );

    // Used.
    let e = w
        .svc
        .sign_in_door_set_password(&w.origin, Secret::new("a-new-long-pw"), None)
        .await
        .unwrap_err();
    assert_eq!(e.code, code::NO_PASSWORD_CHANGE);
}

#[tokio::test]
async fn set_password_refusals() {
    // Nothing asked for it.
    let w = world().await;
    let e = w
        .svc
        .sign_in_door_set_password(&w.origin, Secret::new("a-new-long-pw"), None)
        .await
        .unwrap_err();
    assert_eq!(e.code, code::NO_PASSWORD_CHANGE);
    assert_eq!(w.count(lane_route::HOST_DOOR_SET_PASSWORD), 0);

    // Another account's change isn't this one's.
    w.lane.require_password_change("bob").unwrap();
    assert_eq!(
        w.door(PW).await.unwrap_err().code,
        code::PASSWORD_CHANGE_REQUIRED
    );
    let e = w
        .svc
        .sign_in_door_set_password(&w.origin, Secret::new("a-new-long-pw"), Some("other"))
        .await
        .unwrap_err();
    assert_eq!(e.code, code::NO_PASSWORD_CHANGE);

    // bad_token ends the change.
    w.lane.answer_next(
        lane_route::HOST_DOOR_SET_PASSWORD,
        k2_error(401, "bad_token", "Used."),
    );
    let e = w
        .svc
        .sign_in_door_set_password(&w.origin, Secret::new("a-new-long-pw"), None)
        .await
        .unwrap_err();
    assert_eq!(e.code, "bad_token");
    let e = w
        .svc
        .sign_in_door_set_password(&w.origin, Secret::new("a-new-long-pw"), None)
        .await
        .unwrap_err();
    assert_eq!(e.code, code::NO_PASSWORD_CHANGE);

    // The password is set, but the person can't use this App.
    let w = world().await;
    w.lane.require_password_change("bob").unwrap();
    w.lane.set_app_access("bob", false);
    assert_eq!(
        w.door(PW).await.unwrap_err().code,
        code::PASSWORD_CHANGE_REQUIRED
    );
    let e = w
        .svc
        .sign_in_door_set_password(&w.origin, Secret::new("a-new-long-pw"), None)
        .await
        .unwrap_err();
    assert_eq!(e.code, code::PASSWORD_SET_NO_SIGNIN);
    assert_eq!(w.count(lane_route::SIGNIN_FINISH), 0);
    assert!(w.lane.live_sessions().is_empty());

    // A change required with no token can't be set: the next try begins again.
    let w = world().await;
    w.lane.answer_next(
        lane_route::HOST_DOOR_SIGN_IN,
        k2_error(403, "password_change_required", "Set a password."),
    );
    assert_eq!(
        w.door(PW).await.unwrap_err().code,
        code::PASSWORD_CHANGE_REQUIRED
    );
    assert_eq!(
        w.svc
            .sign_in_door_set_password(&w.origin, Secret::new("a-new-long-pw"), None)
            .await
            .unwrap_err()
            .code,
        code::NO_PASSWORD_CHANGE
    );
}

#[tokio::test]
async fn a_refused_finish_spends_the_begin() {
    let w = world().await;
    w.lane.answer_next(
        lane_route::SIGNIN_FINISH,
        tauri_plugin_sew_app::fake::lane_error(401, "code_invalid", Value::Null, "Used."),
    );
    let e = w.door(PW).await.unwrap_err();
    assert_eq!(e.code, "code_invalid");
    assert!(w.lane.live_sessions().is_empty());
    w.door(PW).await.unwrap();
    assert_eq!(
        w.count(lane_route::SIGNIN_BEGIN),
        2,
        "a new begin after a finish"
    );
    // A host answer that isn't a code is refused before the finish.
    let w = world().await;
    w.lane.answer_next(
        lane_route::HOST_DOOR_SIGN_IN,
        json_answer(200, json!({ "ok": true, "code": "has a space" })),
    );
    assert_eq!(w.door(PW).await.unwrap_err().code, code::BAD_ANSWER);
    assert_eq!(w.count(lane_route::SIGNIN_FINISH), 0);
}

#[tokio::test]
async fn an_older_shell_than_info_shell_min_is_refused_sign_in() {
    let w = world_with(|c| c.shell_version = "1.4.2".into());
    w.lane.set_info(|i| i.shell.min = Some("1.5.0".into()));
    w.svc.add_server(APP).await.unwrap();
    w.svc.confirm_server(&w.origin).unwrap();
    let pairing = w.lane.issue_pairing_code("bob").unwrap();
    let errs = [
        w.door(PW).await.unwrap_err(),
        w.svc
            .sign_in_password(&w.origin, "bob", Secret::new(PW), None)
            .await
            .unwrap_err(),
        w.svc
            .pair(&w.origin, Secret::new(pairing), None)
            .await
            .unwrap_err(),
    ];
    for e in errs {
        assert_eq!(e.code, code::SHELL_TOO_OLD);
        assert_eq!(e.detail, json!({ "min": "1.5.0", "version": "1.4.2" }));
    }
    w.no_password_sent();
    assert_eq!(w.count(lane_route::PAIR_REDEEM), 0);
    assert_eq!(w.count(lane_route::SIGNIN_BEGIN), 0);

    // At min, above it, no min, or a min that doesn't read: allowed.
    for min in [
        Some("1.4.2"),
        Some("1.4"),
        Some("0.9.9-rc.1"),
        None,
        Some("soon"),
    ] {
        w.lane.set_info(|i| i.shell.min = min.map(str::to_owned));
        w.svc
            .sign_in_password(&w.origin, "bob", Secret::new(PW), None)
            .await
            .unwrap_or_else(|e| panic!("{min:?}: {e:?}"));
    }
    // The default shell version is the plugin's own.
    assert_eq!(AppConfig::new("x").shell_version, env!("CARGO_PKG_VERSION"));
}

#[test]
fn version_triples() {
    use tauri_plugin_sew_app::ShellInfo;
    use tauri_plugin_sew_app::lane::version_triple;
    assert_eq!(version_triple("1.2.3"), Some((1, 2, 3)));
    assert_eq!(version_triple(" 1.2 "), Some((1, 2, 0)));
    assert_eq!(version_triple("2"), Some((2, 0, 0)));
    assert_eq!(version_triple("1.2.3-beta.1+b5"), Some((1, 2, 3)));
    for bad in ["", "x", "1..2", "1.2.3.4", "v1.2.3", "1.-2.3"] {
        assert_eq!(version_triple(bad), None, "{bad}");
    }
    let s = |m: &str| ShellInfo {
        min: Some(m.into()),
    };
    assert_eq!(s("0.10.0").too_old("0.9.9"), Some(true));
    assert_eq!(s("0.10.0").too_old("0.10.0"), Some(false));
    assert_eq!(s("0.10.0").too_old("1.0.0"), Some(false));
    assert_eq!(s("junk").too_old("1.0.0"), None);
    assert_eq!(ShellInfo::default().too_old("junk"), Some(false));
}

fn pair_link(host: &str, code: Option<&str>) -> url::Url {
    let frag = code.map(|c| format!("#code={c}")).unwrap_or_default();
    url::Url::parse(&format!("myapp://{host}/native/pair{frag}")).unwrap()
}

fn offer(w: &World, link: &url::Url) {
    let saved: Vec<Origin> = w.svc.book().servers.into_iter().map(|s| s.origin).collect();
    match route(link, "myapp", &saved) {
        DeepLinkRoute::Pair { host, server, code } => {
            w.svc.offer_pairing(&host, server.as_ref(), code);
        }
        r => panic!("{r:?}"),
    }
}

#[tokio::test]
async fn a_pairing_link_asks_first_and_only_for_a_confirmed_server() {
    let w = world().await;
    let code_ = w.lane.issue_pairing_code("bob").unwrap();
    offer(&w, &pair_link("myapp.example.com", Some(&code_)));
    let ev = w.sink.events().pop().unwrap();
    assert_eq!(ev.name(), "pair_request");
    assert_eq!(
        ev.payload(),
        json!({ "server": APP, "host": "myapp.example.com", "expires_in_ms": 120000 })
    );
    assert_eq!(
        w.count(lane_route::PAIR_REDEEM),
        0,
        "nothing redeemed before the confirm"
    );
    let a = w.svc.pair_confirm(&w.origin, None).await.unwrap();
    assert_eq!(a.member_id, "1002");
    let redeem = w
        .lane
        .seen()
        .into_iter()
        .find(|r| r.path == lane_route::PAIR_REDEEM)
        .unwrap();
    assert_eq!(redeem.body.unwrap()["code"], code_.as_str());
    assert_eq!(
        w.svc.pair_confirm(&w.origin, None).await.unwrap_err().code,
        code::PAIRING_NOT_PENDING,
        "once"
    );

    // Declined.
    let code_ = w.lane.issue_pairing_code("bob").unwrap();
    offer(&w, &pair_link("myapp.example.com", Some(&code_)));
    assert!(w.svc.pair_decline(&w.origin));
    assert!(!w.svc.pair_decline(&w.origin));
    assert_eq!(
        w.svc.pair_confirm(&w.origin, None).await.unwrap_err().code,
        code::PAIRING_NOT_PENDING
    );
    assert_eq!(w.count(lane_route::PAIR_REDEEM), 1);
}

#[tokio::test]
async fn a_pairing_link_for_another_host_is_refused_and_never_redeemed() {
    let w = world().await;
    // An unknown host, a host only added (not confirmed), and a link without a code.
    let pending = Origin::parse("https://pending.example.com", false).unwrap();
    w.svc.add_server(pending.as_str()).await.unwrap();
    offer(&w, &pair_link("evil.example", Some("abc")));
    offer(&w, &pair_link("pending.example.com", Some("abc")));
    offer(&w, &pair_link("myapp.example.com", None));
    offer(&w, &pair_link("myapp.example.com", Some("not%20base64url")));
    let refused: Vec<Value> = w
        .sink
        .events()
        .into_iter()
        .filter(|e| e.name() == "pair_refused")
        .map(|e| e.payload())
        .collect();
    assert_eq!(
        refused,
        [
            json!({ "host": "evil.example", "reason": "unknown_server" }),
            json!({ "host": "pending.example.com", "reason": "unknown_server" }),
            json!({ "host": "myapp.example.com", "reason": "bad_link" }),
            json!({ "host": "myapp.example.com", "reason": "bad_link" }),
        ]
    );
    assert!(!w.sink.names().contains(&"pair_request"));
    for o in [&pending, &w.origin] {
        assert_eq!(
            w.svc.pair_confirm(o, None).await.unwrap_err().code,
            code::PAIRING_NOT_PENDING
        );
    }
    assert_eq!(w.count(lane_route::PAIR_REDEEM), 0);
}

fn shown<T: serde::Serialize>(out: &mut Vec<Value>, r: Result<T, AppError>) {
    match r {
        Ok(v) => out.push(serde_json::to_value(&v).unwrap()),
        Err(e) => {
            out.push(serde_json::to_value(&e).unwrap());
            out.push(json!(format!("{e:?} {e}")));
        }
    }
}

/// Every secret of every sign-in path, against every event and every command answer the
/// webview could see (the commands answer these values as they are).
#[tokio::test]
async fn no_secret_reaches_the_webview() {
    let w = world().await;
    let mut out: Vec<Value> = Vec::new();
    let new_pw = "a-new-long-pw";

    // The door: a refused password, a lock, a change asked, a refused and an accepted new
    // password, then a plain door sign-in.
    shown(&mut out, w.door("wrong-password").await);
    w.lane.lock_user("bob", true);
    shown(&mut out, w.door(PW).await);
    w.lane.lock_user("bob", false);
    let token = w.lane.require_password_change("bob").unwrap();
    shown(&mut out, w.door(PW).await);
    shown(
        &mut out,
        w.svc
            .sign_in_door_set_password(&w.origin, Secret::new("tiny-pw"), None)
            .await,
    );
    shown(
        &mut out,
        w.svc
            .sign_in_door_set_password(&w.origin, Secret::new(new_pw), None)
            .await,
    );
    shown(
        &mut out,
        w.svc
            .sign_in_door(&w.origin, "bob", Secret::new(new_pw), None)
            .await,
    );
    // The password form, a pairing link (and one for another host), the lists, sign-out.
    shown(
        &mut out,
        w.svc
            .sign_in_password(&w.origin, "bob", Secret::new(new_pw), None)
            .await,
    );
    let pairing = w.lane.issue_pairing_code("bob").unwrap();
    offer(&w, &pair_link("myapp.example.com", Some(&pairing)));
    offer(&w, &pair_link("evil.example", Some(&pairing)));
    shown(&mut out, w.svc.pair_confirm(&w.origin, None).await);
    shown(&mut out, Ok::<_, AppError>(w.svc.servers()));
    shown(&mut out, w.svc.refresh_info(&w.origin).await);
    let key = w.svc.active().unwrap();
    shown(&mut out, w.svc.devices(&key).await);
    shown(&mut out, w.svc.sign_out(&key).await);
    for e in w.sink.events() {
        out.push(e.payload());
        out.push(json!(format!("{e:?}")));
    }

    // Every secret the requests carried.
    let mut secrets: Vec<String> = vec![
        PW.into(),
        new_pw.into(),
        "wrong-password".into(),
        "tiny-pw".into(),
        token,
        pairing,
    ];
    for r in w.lane.seen() {
        if let Some(b) = r.bearer {
            secrets.push(b);
        }
        let Some(body) = r.body else { continue };
        for k in ["password", "challenge", "code", "token", "id"] {
            if let Some(s) = body[k].as_str() {
                secrets.push(s.to_owned());
            }
        }
    }
    let challenges = w
        .lane
        .seen()
        .iter()
        .filter(|r| r.path == lane_route::HOST_DOOR_SIGN_IN)
        .count();
    assert!(challenges >= 4, "the scenario went through the door");
    assert!(secrets.len() > 20, "the scenario carried its secrets");
    let all = out
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    for s in &secrets {
        assert!(
            !all.contains(s.as_str()),
            "a secret ({} characters) reached the webview",
            s.len()
        );
    }
    // The grep sees what the webview sees.
    assert!(all.contains("myapp.example.com"));
    assert!(all.contains("password_change_required"));
    assert!(all.contains("PairRequest") && all.contains("pair") && all.contains("1002"));
}
