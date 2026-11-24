//! The run-time remote capability, on Tauri's mock runtime (feature `tauri-test`): a confirmed
//! server's origin gets `sew-app:default` in the App's window at run time; another origin, a
//! window outside the capability, and `handoff` without `sew-app:handoff` are refused; the
//! bundled UI (`tauri://localhost`) needs only its own capability, never the remote one. The
//! API transport needs `sew-app:transport`, which neither `sew-app:default` nor the remote
//! capability holds.
//!
//! What the commands do for each caller: one server's page can't list or act on another
//! server; `add_server` and `confirm_server` are refused from a live site; `upload_file`
//! accepts only files picked in the file dialog; a gateway socket is refused from another
//! window; a frame of another server inside a site's window is refused. The events every
//! window gets are signals with no server, account, link or code; the bundled UI fetches the
//! details, a live site can't, and the host's hook gets the whole data.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tauri::ipc::{CallbackFn, CapabilityBuilder, InvokeBody, RuntimeAuthority};
use tauri::test::{INVOKE_KEY, get_ipc_response, mock_builder, mock_context, noop_assets};
use tauri::utils::acl::manifest::Manifest;
use tauri::utils::acl::resolved::Resolved;
use tauri::webview::InvokeRequest;

use tauri::{Listener, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_sew_app::fake::FakeLane;
use tauri_plugin_sew_app::wire::{COMMANDS, TRANSPORT_COMMANDS};
use tauri_plugin_sew_app::{AppEvent, Builder, RemoteCapability};

const APP: &str = "https://myapp.example.com";

/// The plugin's manifest as its permission files say (default: all but `handoff`).
fn manifest() -> Manifest {
    let mut m = Manifest::new(Vec::new(), None);
    let allow = |c: &str| format!("allow-{}", c.replace('_', "-"));
    for c in COMMANDS {
        let id = allow(c);
        m.permissions.insert(
            id.clone(),
            serde_json::from_value(json!({ "identifier": id, "commands": { "allow": [c] } }))
                .unwrap(),
        );
    }
    let default: Vec<String> = COMMANDS
        .iter()
        .filter(|c| {
            !["handoff", "upload_file", "pick_files"].contains(c) && !TRANSPORT_COMMANDS.contains(c)
        })
        .map(|c| allow(c))
        .collect();
    assert_eq!(default.len(), 19);
    m.default_permission = Some(
        serde_json::from_value(json!({
            "identifier": "default", "description": "d", "permissions": default
        }))
        .unwrap(),
    );
    m.permission_sets.insert(
        "handoff".into(),
        serde_json::from_value(json!({
            "identifier": "handoff", "description": "h", "permissions": ["allow-handoff"]
        }))
        .unwrap(),
    );
    let transport: Vec<String> = TRANSPORT_COMMANDS.iter().map(|c| allow(c)).collect();
    m.permission_sets.insert(
        "upload-file".into(),
        serde_json::from_value(json!({
            "identifier": "upload-file", "description": "u",
            "permissions": ["allow-pick-files", "allow-upload-file"]
        }))
        .unwrap(),
    );
    m.permission_sets.insert(
        "transport".into(),
        serde_json::from_value(json!({
            "identifier": "transport", "description": "t", "permissions": transport
        }))
        .unwrap(),
    );
    m
}

/// The permission files say what the test's manifest says.
#[test]
fn the_permission_files_name_the_sets() {
    let read = |f: &str| {
        std::fs::read_to_string(format!("{}/permissions/{f}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    };
    let t = read("transport.toml");
    for c in TRANSPORT_COMMANDS {
        assert!(
            t.contains(&format!("\"allow-{}\"", c.replace('_', "-"))),
            "{c}"
        );
    }
    let u = read("upload-file.toml");
    assert!(u.contains("\"allow-upload-file\""));
    assert!(u.contains("\"allow-pick-files\""));
    let d = read("default.toml");
    let listed = d.split("permissions = [").nth(1).unwrap();
    for c in TRANSPORT_COMMANDS
        .iter()
        .chain(&["upload_file", "pick_files", "handoff"])
    {
        assert!(
            !listed.contains(&format!("\"allow-{}\"", c.replace('_', "-"))),
            "{c} is not in the default set"
        );
    }
}

#[test]
fn the_transport_needs_its_own_set_and_answers_no_secret() {
    let mut ctx = mock_context(noop_assets());
    let mut acl = BTreeMap::new();
    acl.insert("sew-app".to_owned(), manifest());
    *ctx.runtime_authority_mut() = RuntimeAuthority::new(acl, Resolved::default());
    let lane = FakeLane::new();
    let app = mock_builder()
        .plugin(
            Builder::fake(lane.clone())
                // The default remote capability less `core:event:default` (the mock manifest
                // knows only this plugin).
                .remote_capability(Some(RemoteCapability {
                    windows: vec!["main".into()],
                    permissions: vec!["sew-app:default".into()],
                }))
                .build(),
        )
        .build(ctx)
        .unwrap();
    app.add_capability(
        CapabilityBuilder::new("bundled")
            .window("local")
            .permission("sew-app:default")
            .permission("sew-app:transport"),
    )
    .unwrap();
    app.add_capability(
        CapabilityBuilder::new("plain")
            .window("plain")
            .permission("sew-app:default"),
    )
    .unwrap();
    let local = WebviewWindowBuilder::new(&app, "local", WebviewUrl::App("index.html".into()))
        .build()
        .unwrap();
    let plain = WebviewWindowBuilder::new(&app, "plain", WebviewUrl::App("index.html".into()))
        .build()
        .unwrap();
    let main = WebviewWindowBuilder::new(
        &app,
        "main",
        WebviewUrl::External(format!("{APP}/app/").parse().unwrap()),
    )
    .build()
    .unwrap();
    let local_url = local.url().unwrap().to_string();
    let main_url = format!("{APP}/app/");
    invoke(&local, &local_url, "add_server", json!({ "url": APP })).unwrap();
    invoke(
        &local,
        &local_url,
        "confirm_server",
        json!({ "server": APP }),
    )
    .unwrap();
    let pw = tauri_plugin_sew_app::fake::FakeLane::BOB_PASSWORD;
    invoke(
        &local,
        &local_url,
        "sign_in",
        json!({ "server": APP, "username": "bob", "password": pw }),
    )
    .unwrap();

    // The bundled UI with the transport set: the API through the lane, no secret back.
    let me = invoke(
        &local,
        &local_url,
        "api",
        json!({ "method": "GET", "path": "/me" }),
    )
    .unwrap();
    assert_eq!(me["status"], 200);
    assert_eq!(me["body"]["id"], "1002");
    let post = invoke(
        &local,
        &local_url,
        "api",
        json!({ "method": "POST", "path": "/channels/5/messages",
                "body": { "body": "hi" }, "idempotency_key": "k-9" }),
    )
    .unwrap();
    assert_eq!(post["status"], 201);
    let sent = lane.seen_bytes();
    assert!(
        sent.last()
            .unwrap()
            .headers
            .contains(&("idempotency-key".into(), "k-9".into())),
        "snake_case arguments"
    );
    let all = format!("{me}{post}");
    for b in lane.bearers() {
        assert!(!all.contains(&b));
    }
    assert!(!all.contains("sewdv_"));

    // Without the set: refused, in the bundled UI and on the live site.
    denied(
        invoke(
            &plain,
            &local_url,
            "api",
            json!({ "method": "GET", "path": "/me" }),
        ),
        "api with sew-app:default only",
    );
    denied(
        invoke(
            &main,
            &main_url,
            "api",
            json!({ "method": "GET", "path": "/me" }),
        ),
        "api from the live site",
    );
    denied(
        invoke(
            &main,
            &main_url,
            "gateway_send",
            json!({ "id": "x", "data": "{}" }),
        ),
        "the gateway from the live site",
    );
    // upload_file has its own set, which nobody here has.
    denied(
        invoke(
            &local,
            &local_url,
            "upload_file",
            json!({ "path": "/uploads/1/data", "file": "/etc/hostname" }),
        ),
        "upload_file without its set",
    );
}

fn invoke(
    w: &tauri::WebviewWindow<tauri::test::MockRuntime>,
    url: &str,
    cmd: &str,
    body: Value,
) -> Result<Value, Value> {
    get_ipc_response(
        w,
        InvokeRequest {
            cmd: format!("plugin:sew-app|{cmd}"),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: url.parse().unwrap(),
            body: InvokeBody::Json(body),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        },
    )
    .map(|b| b.deserialize::<Value>().unwrap())
}

fn denied(r: Result<Value, Value>, what: &str) {
    let e = r.expect_err(what);
    let s = e.to_string();
    assert!(
        s.contains("not allowed"),
        "{what}: refused for another reason: {s}"
    );
}

#[test]
fn a_confirmed_servers_origin_gets_the_plugin_at_run_time() {
    let mut ctx = mock_context(noop_assets());
    let mut acl = BTreeMap::new();
    acl.insert("sew-app".to_owned(), manifest());
    *ctx.runtime_authority_mut() = RuntimeAuthority::new(acl, Resolved::default());
    let rc = RemoteCapability {
        windows: vec!["main".into(), "stray".into()],
        permissions: vec!["sew-app:default".into()],
    };
    let app = mock_builder()
        .plugin(
            Builder::fake(FakeLane::new())
                .remote_capability(Some(rc))
                .build(),
        )
        .build(ctx)
        .unwrap();
    // The bundled UI's own capability (an App's capability file), local only.
    app.add_capability(
        CapabilityBuilder::new("bundled")
            .window("local")
            .permission("sew-app:default"),
    )
    .unwrap();
    let local = WebviewWindowBuilder::new(&app, "local", WebviewUrl::App("index.html".into()))
        .build()
        .unwrap();
    let main = WebviewWindowBuilder::new(
        &app,
        "main",
        WebviewUrl::External(format!("{APP}/app/").parse().unwrap()),
    )
    .build()
    .unwrap();
    let stray = WebviewWindowBuilder::new(
        &app,
        "stray",
        WebviewUrl::External("https://evil.example/".parse().unwrap()),
    )
    .build()
    .unwrap();
    let local_url = local.url().unwrap().to_string();
    let main_url = format!("{APP}/app/");

    // Before the server is confirmed, its site gets nothing.
    denied(
        invoke(&main, &main_url, "servers", json!({})),
        "before the confirm",
    );

    // The bundled UI adds and confirms it.
    let p = invoke(&local, &local_url, "add_server", json!({ "url": APP })).unwrap();
    assert_eq!(p["origin"], APP);
    invoke(
        &local,
        &local_url,
        "confirm_server",
        json!({ "server": APP }),
    )
    .unwrap();

    // Now the site can call the plugin from the main window.
    let v = invoke(&main, &main_url, "servers", json!({})).unwrap();
    assert_eq!(v["servers"][0]["origin"], APP);
    // Another origin in a window the capability names gets nothing.
    denied(
        invoke(&stray, "https://evil.example/", "servers", json!({})),
        "another origin",
    );
    // `handoff` is not in sew-app:default.
    denied(
        invoke(&main, &main_url, "handoff", json!({})),
        "handoff without its set",
    );
    denied(
        invoke(&local, &local_url, "handoff", json!({})),
        "handoff in the bundled UI",
    );
    // The remote capability is not the bundled UI's: a local window outside it.
    let other = WebviewWindowBuilder::new(&app, "other", WebviewUrl::App("index.html".into()))
        .build()
        .unwrap();
    denied(
        invoke(&other, &local_url, "servers", json!({})),
        "a window with no capability",
    );
    assert!(
        app.try_state::<tauri_plugin_sew_app::AppService>()
            .is_some()
    );
}

const OTHER: &str = "https://other.example.com";

/// The shell of a scoped test: the fake lane, the remote capability in `main`, the bundled UI
/// in `local` and `local2` (with the transport and `upload-file`), and two confirmed servers
/// ([`APP`], [`OTHER`]) with bob signed in on each.
struct Shell {
    app: tauri::App<tauri::test::MockRuntime>,
    local: tauri::WebviewWindow<tauri::test::MockRuntime>,
    local2: tauri::WebviewWindow<tauri::test::MockRuntime>,
    main: tauri::WebviewWindow<tauri::test::MockRuntime>,
    local_url: String,
    main_url: String,
    /// bob's account on APP, then on OTHER.
    keys: [Value; 2],
    /// What the host's hook (`Builder::on_event`) got.
    hooked: Arc<Mutex<Vec<AppEvent>>>,
}

fn shell() -> Shell {
    let mut ctx = mock_context(noop_assets());
    let mut acl = BTreeMap::new();
    acl.insert("sew-app".to_owned(), manifest());
    *ctx.runtime_authority_mut() = RuntimeAuthority::new(acl, Resolved::default());
    let hooked = Arc::new(Mutex::new(Vec::new()));
    let h = hooked.clone();
    let app = mock_builder()
        .plugin(
            Builder::fake(FakeLane::new())
                .remote_capability(Some(RemoteCapability {
                    windows: vec!["main".into()],
                    permissions: vec!["sew-app:default".into()],
                }))
                .on_event(move |e| h.lock().unwrap().push(e.clone()))
                .build(),
        )
        .build(ctx)
        .unwrap();
    app.add_capability(
        CapabilityBuilder::new("bundled")
            .window("local")
            .window("local2")
            .permission("sew-app:default")
            .permission("sew-app:transport")
            .permission("sew-app:upload-file"),
    )
    .unwrap();
    let local = WebviewWindowBuilder::new(&app, "local", WebviewUrl::App("index.html".into()))
        .build()
        .unwrap();
    let local2 = WebviewWindowBuilder::new(&app, "local2", WebviewUrl::App("index.html".into()))
        .build()
        .unwrap();
    let main = WebviewWindowBuilder::new(
        &app,
        "main",
        WebviewUrl::External(format!("{APP}/app/").parse().unwrap()),
    )
    .build()
    .unwrap();
    let local_url = local.url().unwrap().to_string();
    let pw = FakeLane::BOB_PASSWORD;
    let mut keys = Vec::new();
    for server in [APP, OTHER] {
        invoke(&local, &local_url, "add_server", json!({ "url": server })).unwrap();
        invoke(
            &local,
            &local_url,
            "confirm_server",
            json!({ "server": server }),
        )
        .unwrap();
        let a = invoke(
            &local,
            &local_url,
            "sign_in",
            json!({ "server": server, "username": "bob", "password": pw }),
        )
        .unwrap();
        keys.push(a["key"].clone());
    }
    Shell {
        app,
        local,
        local2,
        main,
        local_url,
        main_url: format!("{APP}/app/"),
        keys: [keys[0].clone(), keys[1].clone()],
        hooked,
    }
}

/// Refused by the plugin (not the capability) with `code`.
fn refused(r: Result<Value, Value>, code: &str, what: &str) {
    let e = r.expect_err(what);
    assert_eq!(e["code"], code, "{what}: {e}");
}

#[test]
fn one_servers_page_cant_list_or_act_on_another_server() {
    let s = shell();
    let (main, url) = (&s.main, s.main_url.as_str());
    let [own, other] = &s.keys;

    // The bundled UI lists both; the site lists only its own server and accounts.
    let all = invoke(&s.local, &s.local_url, "servers", json!({})).unwrap();
    assert_eq!(all["servers"].as_array().unwrap().len(), 2);
    let v = invoke(main, url, "servers", json!({})).unwrap();
    let listed = v["servers"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["origin"], APP);
    assert!(!v.to_string().contains(OTHER), "{v}");

    // Nothing of the other server: not its info, its accounts, its devices or the server.
    let o = || json!({ "server": OTHER });
    refused(
        invoke(main, url, "server_info", o()),
        "wrong_origin",
        "server_info",
    );
    refused(
        invoke(main, url, "remove_server", o()),
        "wrong_origin",
        "remove_server",
    );
    refused(
        invoke(main, url, "pair_decline", o()),
        "wrong_origin",
        "pair_decline",
    );
    refused(
        invoke(
            main,
            url,
            "sign_in",
            json!({ "server": OTHER, "username": "bob", "password": "x" }),
        ),
        "wrong_origin",
        "sign_in",
    );
    refused(
        invoke(main, url, "set_active", json!({ "key": other })),
        "wrong_origin",
        "set_active",
    );
    refused(
        invoke(main, url, "sign_out", json!({ "key": other })),
        "wrong_origin",
        "sign_out",
    );
    refused(
        invoke(main, url, "devices", json!({ "key": other })),
        "wrong_origin",
        "devices",
    );
    refused(
        invoke(
            main,
            url,
            "revoke_device",
            json!({ "key": other, "session": "s" }),
        ),
        "wrong_origin",
        "revoke_device",
    );
    refused(
        invoke(main, url, "open", json!({ "key": other })),
        "wrong_origin",
        "open",
    );

    // Without a key, the active account (bob on OTHER, signed in last) is not the site's.
    refused(
        invoke(main, url, "devices", json!({})),
        "no_account",
        "the active account on another server",
    );

    // Its own server and account still answer.
    invoke(main, url, "devices", json!({ "key": own })).unwrap();
    invoke(main, url, "server_info", json!({ "server": APP })).unwrap();
    let v = invoke(main, url, "set_active", json!({ "key": own })).unwrap();
    assert_eq!(v["active"], *own);
    assert_eq!(v["servers"].as_array().unwrap().len(), 1);

    // Both servers and accounts are still saved.
    let all = invoke(&s.local, &s.local_url, "servers", json!({})).unwrap();
    assert_eq!(all["servers"].as_array().unwrap().len(), 2);
    for srv in all["servers"].as_array().unwrap() {
        assert_eq!(srv["accounts"].as_array().unwrap().len(), 1, "{srv}");
    }
}

#[test]
fn confirm_server_is_refused_from_a_live_site() {
    let s = shell();
    let (main, url) = (&s.main, s.main_url.as_str());
    const NEW: &str = "https://new.example.com";
    // Neither the probe nor the confirm answers a live site.
    refused(
        invoke(main, url, "add_server", json!({ "url": NEW })),
        "bundled_only",
        "add_server from the site",
    );
    // A server the bundled UI is holding for the person's confirm: the site can't confirm it.
    invoke(&s.local, &s.local_url, "add_server", json!({ "url": NEW })).unwrap();
    refused(
        invoke(main, url, "confirm_server", json!({ "server": NEW })),
        "bundled_only",
        "confirm_server from the site",
    );
    refused(
        invoke(main, url, "confirm_server", json!({ "server": APP })),
        "bundled_only",
        "confirm_server of its own server",
    );
    let all = invoke(&s.local, &s.local_url, "servers", json!({})).unwrap();
    assert!(!all.to_string().contains(NEW), "not saved: {all}");
    // The person's confirm in the bundled UI still works.
    invoke(
        &s.local,
        &s.local_url,
        "confirm_server",
        json!({ "server": NEW }),
    )
    .unwrap();
}

#[test]
fn upload_file_accepts_only_dialog_picked_files() {
    let s = shell();
    let dir = std::env::temp_dir().join(format!("sew-app-acl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("picked.bin");
    std::fs::write(&f, b"picked bytes").unwrap();
    let up = |file: &str| {
        invoke(
            &s.local,
            &s.local_url,
            "upload_file",
            json!({ "path": "/uploads/77/data", "file": file }),
        )
    };
    // A path, a missing path and an unknown handle: one answer, saying nothing of the file.
    let a = up(f.to_str().unwrap()).expect_err("a path");
    let b = up(dir.join("missing.bin").to_str().unwrap()).expect_err("a missing path");
    let c = up("sf_not-a-handle").expect_err("an unknown handle");
    assert_eq!(a["code"], "file");
    assert_eq!(a, b);
    assert_eq!(a, c);
    // A file picked in a dialog (the host's, here: the plugin's own needs a person) goes.
    let svc = s.app.state::<tauri_plugin_sew_app::AppService>();
    let picked = svc.picked_file(&f).unwrap();
    assert_eq!(picked.name, "picked.bin");
    assert_eq!(picked.size, 12);
    assert!(
        !serde_json::to_string(&picked)
            .unwrap()
            .contains(dir.to_str().unwrap())
    );
    let r = up(&picked.handle).unwrap();
    assert_eq!(r["status"], 200, "{r}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_gateway_socket_is_refused_from_another_window() {
    let s = shell();
    let opened = invoke(
        &s.local,
        &s.local_url,
        "gateway_open",
        json!({ "key": s.keys[0], "on_event": "__CHANNEL__:7" }),
    )
    .unwrap();
    let id = opened["id"].as_str().unwrap().to_owned();
    // Another window of the same bundled UI: no such socket.
    refused(
        invoke(
            &s.local2,
            &s.local_url,
            "gateway_send",
            json!({ "id": id, "data": "{\"op\":1,\"d\":null}" }),
        ),
        "unknown_socket",
        "gateway_send from another window",
    );
    let closed = invoke(
        &s.local2,
        &s.local_url,
        "gateway_close",
        json!({ "id": id }),
    )
    .unwrap();
    assert_eq!(closed, json!(false), "gateway_close from another window");
    // Its own window still drives it.
    invoke(
        &s.local,
        &s.local_url,
        "gateway_send",
        json!({ "id": id, "data": "{\"op\":1,\"d\":null}" }),
    )
    .unwrap();
    let closed = invoke(&s.local, &s.local_url, "gateway_close", json!({ "id": id })).unwrap();
    assert_eq!(closed, json!(true));
}

#[test]
fn a_frame_of_another_server_in_a_sites_window_is_refused() {
    let s = shell();
    // OTHER's capability names window `main` too, so Tauri lets a frame of OTHER in; the
    // plugin refuses it, since `main` shows APP's page.
    refused(
        invoke(&s.main, &format!("{OTHER}/frame"), "servers", json!({})),
        "wrong_origin",
        "a frame of another server",
    );
    invoke(&s.main, &s.main_url, "servers", json!({})).unwrap();
}

/// Every `sew-app://*` payload a listener got: on the app (any target, as `listen_any`) and on
/// the live site's own webview (the target a page's default `listen` matches). Tauri hands
/// the same serialized payload to the webviews' JS listeners and to these.
#[derive(Clone, Default)]
struct Heard(Arc<Mutex<Vec<(String, String)>>>);

impl Heard {
    fn all(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().clone()
    }
}

const NAMES: [&str; 5] = [
    "servers",
    "signed_out",
    "deep_link",
    "pair_request",
    "pair_refused",
];

fn listen_all(on: &impl Listener<tauri::test::MockRuntime>, any: bool) -> Heard {
    let heard = Heard::default();
    for n in NAMES {
        let h = heard.clone();
        let name = format!("sew-app://{n}");
        let f = move |e: tauri::Event| {
            h.0.lock()
                .unwrap()
                .push((n.to_owned(), e.payload().to_owned()))
        };
        if any {
            on.listen_any(name, f);
        } else {
            on.listen(name, f);
        }
    }
    heard
}

/// Every event: the list (a sign-in), a server-side end, a pairing link held and one refused,
/// and a deep link.
fn raise_every_event(s: &Shell) {
    let svc = s.app.state::<tauri_plugin_sew_app::AppService>();
    let pw = FakeLane::BOB_PASSWORD;
    invoke(
        &s.local,
        &s.local_url,
        "sign_in",
        json!({ "server": OTHER, "username": "bob", "password": pw, "account": s.keys[1]["account"] }),
    )
    .unwrap();
    let other: tauri_plugin_sew_app::AccountKey =
        serde_json::from_value(s.keys[1].clone()).unwrap();
    tauri::async_runtime::block_on(svc.session_ended(&other, "revoked"));
    let o = tauri_plugin_sew_app::Origin::parse(OTHER, false).unwrap();
    svc.offer_pairing(
        o.authority(),
        Some(&o),
        Some(tauri_plugin_sew_app::Secret::new("pAiRcOdE123")),
    );
    svc.offer_pairing("stranger.example.net", None, None);
    svc.emit(&AppEvent::DeepLink(tauri_plugin_sew_app::DeepLinkEvent {
        url: "myapp://other.example.com/c/5".to_owned(),
        server: Some(o.clone()),
        path: Some("/c/5".into()),
        navigated: false,
    }));
}

#[test]
fn a_live_sites_listener_gets_only_the_signal_never_another_servers_data() {
    let s = shell();
    let site = listen_all(&s.main, false);
    let any = listen_all(&s.app, true);
    raise_every_event(&s);
    let heard = site.all();
    for n in NAMES {
        assert!(
            heard.iter().any(|(m, _)| m == n),
            "{n} reached the site: {heard:?}"
        );
    }
    for (n, payload) in heard.iter().chain(any.all().iter()) {
        let v: Value = serde_json::from_str(payload).unwrap();
        let want = match n.as_str() {
            "servers" | "signed_out" => json!({ "changed": true }),
            _ => json!({ "pending": true }),
        };
        assert_eq!(v, want, "{n}");
        for secret in [
            "example.com",
            "example.net",
            "pAiRcOdE123",
            "myapp:",
            s.keys[0]["account"].as_str().unwrap(),
            s.keys[1]["account"].as_str().unwrap(),
        ] {
            assert!(!payload.contains(secret), "{n} carries {secret}: {payload}");
        }
    }
}

#[test]
fn the_bundled_ui_fetches_the_details() {
    let s = shell();
    raise_every_event(&s);
    let p = invoke(&s.local, &s.local_url, "pending_pair", json!({})).unwrap();
    assert_eq!(p["requests"][0]["server"], OTHER, "{p}");
    assert_eq!(p["requests"][0]["host"], "other.example.com");
    assert!(
        !p.to_string().contains("pAiRcOdE123"),
        "never the code: {p}"
    );
    assert_eq!(p["refused"]["host"], "stranger.example.net");
    assert_eq!(p["refused"]["reason"], "unknown_server");
    // The refusal is answered once; the held link stays until it is confirmed or declined.
    let again = invoke(&s.local, &s.local_url, "pending_pair", json!({})).unwrap();
    assert_eq!(again["refused"], Value::Null);
    assert_eq!(again["requests"].as_array().unwrap().len(), 1);
    let links = invoke(&s.local, &s.local_url, "pending_deep_links", json!({})).unwrap();
    assert_eq!(links[0]["url"], "myapp://other.example.com/c/5", "{links}");
    assert_eq!(links[0]["server"], OTHER);
    let none = invoke(&s.local, &s.local_url, "pending_deep_links", json!({})).unwrap();
    assert_eq!(none, json!([]));
    // The list and the ended account: through `servers`.
    let v = invoke(&s.local, &s.local_url, "servers", json!({})).unwrap();
    assert!(v.to_string().contains("revoked"), "{v}");
}

#[test]
fn the_hosts_hook_gets_the_whole_data() {
    let s = shell();
    raise_every_event(&s);
    let got = s.hooked.lock().unwrap().clone();
    let servers = got
        .iter()
        .rev()
        .find_map(|e| match e {
            AppEvent::Servers(v) => Some(v.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(servers.servers.len(), 2);
    assert!(got.iter().any(|e| matches!(e,
        AppEvent::SignedOut { key, reason } if key.server.as_str() == OTHER && reason == "revoked")));
    assert!(got.iter().any(|e| matches!(e,
        AppEvent::PairRequest { server, .. } if server.as_str() == OTHER)));
    assert!(got.iter().any(|e| matches!(e,
        AppEvent::PairRefused { host, reason } if host == "stranger.example.net" && reason == "unknown_server")));
    assert!(got.iter().any(|e| matches!(e,
        AppEvent::DeepLink(d) if d.url == "myapp://other.example.com/c/5")));
}

#[test]
fn a_live_site_cant_fetch_the_details() {
    let s = shell();
    raise_every_event(&s);
    refused(
        invoke(&s.main, &s.main_url, "pending_pair", json!({})),
        "bundled_only",
        "pending_pair from the site",
    );
    refused(
        invoke(&s.main, &s.main_url, "pending_deep_links", json!({})),
        "bundled_only",
        "pending_deep_links from the site",
    );
}
