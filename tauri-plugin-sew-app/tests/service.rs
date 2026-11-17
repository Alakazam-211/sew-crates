//! The core over the fake lane: servers and their confirm, sign-in (password, pairing, the
//! door), several accounts per server, sign-out and `forget_server`, a revoke from the
//! server, the handoff and `open`, the stores.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use tauri_plugin_sew_app::fake::{FakeLane, RecordingAppSink, RecordingForget, lane_error};
use tauri_plugin_sew_app::{
    AccountState, AppConfig, AppEvent, AppService, LaneClient, Origin, Secret, SecretChoice, code,
};

const APP: &str = "https://myapp.example.com";

struct World {
    svc: AppService,
    lane: FakeLane,
    sink: Arc<RecordingAppSink>,
    forget: Arc<RecordingForget>,
    origin: Origin,
}

fn scratch() -> PathBuf {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    let d = std::env::temp_dir().join(format!(
        "sew-app-test-{}",
        b.iter().map(|x| format!("{x:02x}")).collect::<String>()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn world_with(lane: FakeLane, f: impl FnOnce(&mut AppConfig)) -> World {
    let mut cfg = AppConfig::new("com.example.test");
    cfg.secrets = SecretChoice::Memory;
    f(&mut cfg);
    let sink = Arc::new(RecordingAppSink::default());
    let svc = AppService::new(cfg, LaneClient::new(Arc::new(lane.clone())), sink.clone()).unwrap();
    let forget = Arc::new(RecordingForget::default());
    svc.set_voice_forget(forget.clone());
    World {
        svc,
        lane,
        sink,
        forget,
        origin: Origin::parse(APP, false).unwrap(),
    }
}

fn world() -> World {
    world_with(FakeLane::new(), |_| {})
}

async fn saved(w: &World) {
    w.svc.add_server(APP).await.unwrap();
    w.svc.confirm_server(&w.origin).unwrap();
}

async fn bob(w: &World) -> tauri_plugin_sew_app::AccountView {
    w.svc
        .sign_in_password(&w.origin, "bob", Secret::new("bob-test-pw"), None)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_server_is_saved_only_after_its_confirm() {
    let w = world();
    // Not added: nothing to confirm.
    assert_eq!(
        w.svc.confirm_server(&w.origin).unwrap_err().code,
        code::NOT_PENDING
    );
    let p = w
        .svc
        .add_server("MyApp.Example.com/some/page")
        .await
        .unwrap();
    assert_eq!(p.origin, w.origin);
    assert_eq!(p.host, "myapp.example.com");
    assert!(!p.saved);
    assert!(
        w.svc.servers().servers.is_empty(),
        "nothing saved before the confirm"
    );
    let v = w.svc.confirm_server(&w.origin).unwrap();
    assert_eq!(v.servers.len(), 1);
    assert_eq!(v.servers[0].host, "myapp.example.com");
    // A confirm is used once.
    assert_eq!(
        w.svc.confirm_server(&w.origin).unwrap_err().code,
        code::NOT_PENDING
    );
    // Adding it again says it is saved; the confirm refreshes it without a duplicate.
    assert!(w.svc.add_server(APP).await.unwrap().saved);
    assert_eq!(w.svc.confirm_server(&w.origin).unwrap().servers.len(), 1);
    assert_eq!(w.sink.names(), ["servers", "servers"]);
}

#[tokio::test]
async fn bad_addresses_and_servers_are_refused() {
    let w = world();
    assert_eq!(
        w.svc
            .add_server("http://myapp.example.com")
            .await
            .unwrap_err()
            .code,
        code::INSECURE_URL
    );
    assert_eq!(
        w.svc
            .add_server("http://127.0.0.1:8080")
            .await
            .unwrap_err()
            .code,
        code::INSECURE_URL,
        "loopback http only when the app allows it"
    );
    w.lane.answer_next(
        "/sew/native/info",
        lane_error(404, "not_found", json!(null), "x"),
    );
    assert_eq!(
        w.svc.add_server(APP).await.unwrap_err().code,
        code::NOT_A_SEW_APP
    );
    w.lane.set_info(|i| i.lane = 0);
    let e = w.svc.add_server(APP).await.unwrap_err();
    assert_eq!(e.code, code::LANE_TOO_OLD);
    assert_eq!(e.detail, json!({"lane": 0, "needs": 1}));
    w.lane.set_down(true);
    assert_eq!(
        w.svc.add_server(APP).await.unwrap_err().code,
        code::UNREACHABLE
    );
    // Signing in to a server never confirmed.
    w.lane.set_down(false);
    let e = w
        .svc
        .sign_in_password(&w.origin, "bob", Secret::new("x"), None)
        .await
        .unwrap_err();
    assert_eq!(e.code, code::UNKNOWN_SERVER);

    let dev = world_with(FakeLane::new(), |c| c.allow_loopback_http = true);
    assert_eq!(
        dev.svc
            .add_server("http://127.0.0.1:8080")
            .await
            .unwrap()
            .origin
            .as_str(),
        "http://127.0.0.1:8080"
    );
}

#[tokio::test]
async fn password_sign_in_keeps_the_bearer_in_the_store_and_nothing_secret_in_the_book() {
    let dir = scratch();
    let lane = FakeLane::new();
    let w = world_with(lane.clone(), |c| {
        c.data_dir = Some(dir.clone());
        c.secrets = SecretChoice::File;
        c.device_name = "Work laptop".into();
    });
    saved(&w).await;
    let e = w
        .svc
        .sign_in_password(&w.origin, "bob", Secret::new("wrong"), None)
        .await
        .unwrap_err();
    assert_eq!(e.code, "unauthenticated");
    let a = bob(&w).await;
    assert_eq!(a.member_id, "1002");
    assert_eq!(a.label.as_deref(), Some("Bob"));
    assert_eq!(a.device_name, "Work laptop");
    assert!(a.active);
    assert_eq!(a.state, AccountState::SignedIn);
    assert_eq!(a.key.account.len(), 22, "a fresh device id per account");
    // The sign-in sent the account's device id and name.
    let sent = w
        .lane
        .seen()
        .into_iter()
        .rfind(|r| r.path == "/sew/native/signin/password")
        .unwrap();
    assert_eq!(
        sent.body.unwrap()["device"],
        json!({"id": a.key.account, "name": "Work laptop"})
    );
    let bearer = w.svc.bearer(&a.key).await.unwrap();
    assert!(tauri_plugin_sew_app::lane::is_device_bearer(
        bearer.expose()
    ));
    // The opt-in file store warned once.
    assert!(
        w.sink
            .events()
            .iter()
            .any(|e| matches!(e, AppEvent::Warning(w) if w.code == code::SECRET_STORAGE_FILE))
    );
    assert_eq!(w.svc.servers().secret_store.as_deref(), Some("file"));
    // The book is on disk, without the bearer; the bearer is in the 0600 secret file.
    let book = std::fs::read_to_string(dir.join("servers.json")).unwrap();
    assert!(
        !book.contains("sewdv_"),
        "the server list never holds a bearer"
    );
    assert!(book.contains(&a.key.account));
    let secrets = std::fs::read_to_string(dir.join("secrets.json")).unwrap();
    assert!(secrets.contains(bearer.expose()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for f in ["servers.json", "secrets.json"] {
            let m = std::fs::metadata(dir.join(f)).unwrap().permissions().mode();
            assert_eq!(m & 0o777, 0o600, "{f}");
        }
    }
    // A restart reads both back.
    let again = world_with(lane, |c| {
        c.data_dir = Some(dir.clone());
        c.secrets = SecretChoice::File;
    });
    assert_eq!(again.svc.active(), Some(a.key.clone()));
    assert_eq!(again.svc.bearer(&a.key).await.unwrap(), bearer);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn several_accounts_on_one_server_are_separate_device_sessions() {
    let w = world();
    w.lane.add_user("carol", "carol-pw", "1003", "Carol");
    saved(&w).await;
    let b = bob(&w).await;
    let c = w
        .svc
        .sign_in_password(&w.origin, "carol", Secret::new("carol-pw"), None)
        .await
        .unwrap();
    assert_ne!(b.key, c.key);
    assert_eq!(
        w.svc.active(),
        Some(c.key.clone()),
        "the last sign-in is active"
    );
    assert_ne!(
        w.svc.bearer(&b.key).await.unwrap(),
        w.svc.bearer(&c.key).await.unwrap()
    );
    assert_eq!(w.lane.live_sessions().len(), 2);
    w.svc.set_active(&b.key).unwrap();
    assert_eq!(w.svc.active(), Some(b.key.clone()));

    // Signing bob's account in again keeps its device id and replaces its session.
    let b2 = w
        .svc
        .sign_in_password(
            &w.origin,
            "bob",
            Secret::new("bob-test-pw"),
            Some(&b.key.account),
        )
        .await
        .unwrap();
    assert_eq!(b2.key, b.key);
    let bobs: Vec<_> = w
        .lane
        .live_sessions()
        .into_iter()
        .filter(|(m, _)| m == "1002")
        .collect();
    assert_eq!(bobs, [("1002".to_owned(), b.key.account.clone())]);

    // Bob signing in as a new account replaces the old one (one account per person and server).
    let b3 = bob(&w).await;
    assert_ne!(b3.key, b.key);
    let accounts = &w.svc.servers().servers[0].accounts;
    assert_eq!(accounts.len(), 2);
    assert!(accounts.iter().all(|a| a.key != b.key));
    let bobs: Vec<_> = w
        .lane
        .live_sessions()
        .into_iter()
        .filter(|(m, _)| m == "1002")
        .collect();
    assert_eq!(bobs, [("1002".to_owned(), b3.key.account.clone())]);

    // An unknown account to sign in again.
    let e = w
        .svc
        .sign_in_password(&w.origin, "bob", Secret::new("x"), Some("nope"))
        .await
        .unwrap_err();
    assert_eq!(e.code, code::UNKNOWN_ACCOUNT);
}

#[tokio::test]
async fn sign_out_ends_the_session_and_forgets_the_install_after_its_last_account() {
    let w = world();
    w.lane.add_user("carol", "carol-pw", "1003", "Carol");
    saved(&w).await;
    let b = bob(&w).await;
    let c = w
        .svc
        .sign_in_password(&w.origin, "carol", Secret::new("carol-pw"), None)
        .await
        .unwrap();
    let out = w.svc.sign_out(&c.key).await.unwrap();
    assert!(out.server_signed_out);
    assert_eq!(
        out.forgot_install, None,
        "bob is still signed in on install 7001"
    );
    assert!(w.forget.calls().is_empty());
    assert_eq!(
        w.svc.active(),
        Some(b.key.clone()),
        "the active account moves on"
    );
    assert_eq!(
        w.svc.bearer(&c.key).await.unwrap_err().code,
        code::UNKNOWN_ACCOUNT
    );

    // The server can't be reached: the account still goes, and the answer says so.
    w.lane.set_down(true);
    let out = w.svc.sign_out(&b.key).await.unwrap();
    assert!(!out.server_signed_out);
    assert_eq!(out.forgot_install.as_deref(), Some("7001"));
    assert_eq!(w.forget.calls(), [7001]);
    assert_eq!(w.svc.active(), None);
    assert!(w.svc.servers().servers[0].accounts.is_empty());
    assert_eq!(
        w.lane.live_sessions().len(),
        1,
        "bob's session waits for the server's idle limit"
    );
}

#[tokio::test]
async fn a_revoke_on_the_server_signs_the_account_out_here() {
    let w = world();
    saved(&w).await;
    let b = bob(&w).await;
    assert_eq!(w.svc.devices(&b.key).await.unwrap().len(), 1);
    w.lane.revoke_device_id(&b.key.account, "revoked");
    let e = w.svc.devices(&b.key).await.unwrap_err();
    assert_eq!(e.code, code::SIGNED_OUT);
    assert_eq!(e.signed_out_reason(), Some("revoked"));
    assert!(e.hint.is_some(), "the lane's sentence is kept");
    let acct = &w.svc.servers().servers[0].accounts[0];
    assert_eq!(
        acct.state,
        AccountState::SignedOut {
            reason: "revoked".into()
        }
    );
    assert!(w.sink.events().iter().any(|e| matches!(
        e, AppEvent::SignedOut { key, reason } if key == &b.key && reason == "revoked"
    )));
    // The bearer is gone and the install's call keys with it (its last signed-in account).
    assert_eq!(
        w.svc.bearer(&b.key).await.unwrap_err().code,
        code::SIGNED_OUT
    );
    assert_eq!(w.forget.calls(), [7001]);
    // Signing in again brings the account back.
    let again = w
        .svc
        .sign_in_password(
            &w.origin,
            "bob",
            Secret::new("bob-test-pw"),
            Some(&b.key.account),
        )
        .await
        .unwrap();
    assert_eq!(again.state, AccountState::SignedIn);
    // Removing a signed-out account works without the server.
    w.lane.revoke_device_id(&b.key.account, "idle");
    let _ = w.svc.devices(&b.key).await;
    w.lane.set_down(true);
    assert!(w.svc.sign_out(&b.key).await.unwrap().server_signed_out);
}

#[tokio::test]
async fn revoking_another_device_and_this_one() {
    let w = world();
    saved(&w).await;
    let phone = world_with(w.lane.clone(), |_| {});
    saved(&phone).await;
    let p = bob(&phone).await;
    // The second bob sign-in from another install (another world) is another session.
    let b = bob(&w).await;
    let list = w.svc.devices(&b.key).await.unwrap();
    assert_eq!(list.len(), 2);
    let other = list.iter().find(|d| d.current == Some(false)).unwrap();
    w.svc.revoke_device(&b.key, &other.id).await.unwrap();
    assert_eq!(
        phone.svc.devices(&p.key).await.unwrap_err().code,
        code::SIGNED_OUT
    );
    // Revoking its own session is a sign-out.
    let own = w
        .svc
        .book()
        .account(&b.key)
        .unwrap()
        .session_id
        .clone()
        .unwrap();
    w.svc.revoke_device(&b.key, &own).await.unwrap();
    assert!(w.svc.servers().servers[0].accounts.is_empty());
}

#[tokio::test]
async fn pairing_and_the_door() {
    let w = world();
    saved(&w).await;
    let code_ = w.lane.issue_pairing_code("bob").unwrap();
    let a = w
        .svc
        .pair(&w.origin, Secret::new(code_.clone()), None)
        .await
        .unwrap();
    assert_eq!(a.member_id, "1002");
    let e = w
        .svc
        .pair(&w.origin, Secret::new(code_), None)
        .await
        .unwrap_err();
    assert_eq!(e.code, "code_invalid", "a pairing code is single-use");

    // The door when the server doesn't offer it (tests/door.rs has the rest).
    assert_eq!(
        w.svc
            .sign_in_door(&w.origin, "bob", Secret::new("bob-test-pw"), None)
            .await
            .unwrap_err()
            .code,
        code::SIGNIN_DOOR_UNAVAILABLE
    );

    // Pairing and passwords off.
    w.lane.set_info(|i| {
        i.signin.pairing = false;
        i.signin.password = false;
    });
    assert_eq!(
        w.svc
            .pair(&w.origin, Secret::new("x"), None)
            .await
            .unwrap_err()
            .code,
        code::PAIRING_UNAVAILABLE
    );
    assert_eq!(
        w.svc
            .sign_in_password(&w.origin, "bob", Secret::new("x"), None)
            .await
            .unwrap_err()
            .code,
        code::PASSWORD_UNAVAILABLE
    );
}

#[tokio::test]
async fn the_handoff_goes_only_to_the_servers_own_page() {
    let w = world_with(FakeLane::new(), |c| {
        c.handoff_page = Some("/native/handoff".into())
    });
    saved(&w).await;
    let b = bob(&w).await;
    let evil = url::Url::parse("https://evil.example/").unwrap();
    assert_eq!(
        w.svc.handoff(&b.key, Some(&evil)).await.unwrap_err().code,
        code::WRONG_ORIGIN
    );
    let local = url::Url::parse("tauri://localhost/").unwrap();
    assert_eq!(
        w.svc.handoff(&b.key, Some(&local)).await.unwrap_err().code,
        code::WRONG_ORIGIN,
        "the bundled UI can't redeem it either"
    );
    let page = url::Url::parse(&format!("{APP}/app/")).unwrap();
    let h = w.svc.handoff(&b.key, Some(&page)).await.unwrap();
    assert_eq!(h.redeem_url, format!("{APP}/sew/native/handoff/redeem"));
    assert_eq!(
        w.lane.redeem_handoff(h.code.expose()).as_deref(),
        Some("1002")
    );
    assert_eq!(w.lane.redeem_handoff(h.code.expose()), None, "single-use");

    // open: the handoff page with the code and the page to go to.
    let u = w.svc.open_url(&b.key, "/app/c/5?m=1").await.unwrap();
    assert_eq!(u.path(), "/native/handoff");
    let frag: Vec<(String, String)> = url::form_urlencoded::parse(u.fragment().unwrap().as_bytes())
        .into_owned()
        .collect();
    assert_eq!(frag[1], ("next".into(), "/app/c/5?m=1".into()));
    assert!(w.lane.handoff_unused(&frag[0].1));
    // A path that leaves the server is refused.
    assert!(w.svc.open_url(&b.key, "//evil.example/").await.is_err());

    // Without a handoff page, open is the plain page.
    let plain = world();
    saved(&plain).await;
    let pb = bob(&plain).await;
    assert_eq!(
        plain.svc.open_url(&pb.key, "/app/").await.unwrap().as_str(),
        format!("{APP}/app/")
    );
}

#[tokio::test]
async fn removing_a_server_signs_its_accounts_out() {
    let w = world();
    saved(&w).await;
    bob(&w).await;
    let v = w.svc.remove_server(&w.origin).await.unwrap();
    assert!(v.servers.is_empty());
    assert!(w.lane.live_sessions().is_empty());
    assert_eq!(w.forget.calls(), [7001]);
    assert_eq!(
        w.svc.remove_server(&w.origin).await.unwrap_err().code,
        code::UNKNOWN_SERVER
    );
}

/// The core build has no keyring: `Os` has no store to open.
#[cfg(not(feature = "keyring-store"))]
#[tokio::test]
async fn without_an_os_store_signing_in_fails_unless_the_app_opted_in() {
    let w = world_with(FakeLane::new(), |c| c.secrets = SecretChoice::Os);
    saved(&w).await;
    let e = w
        .svc
        .sign_in_password(&w.origin, "bob", Secret::new("bob-test-pw"), None)
        .await
        .unwrap_err();
    assert_eq!(e.code, code::SECRET_STORE_UNAVAILABLE);
    assert!(
        w.lane.live_sessions().is_empty(),
        "a session the device can't hold is ended"
    );
}
