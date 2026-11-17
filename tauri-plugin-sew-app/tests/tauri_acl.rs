//! The run-time remote capability, on Tauri's mock runtime (feature `tauri-test`): a confirmed
//! server's origin gets `sew-app:default` in the App's window at run time; another origin, a
//! window outside the capability, and `handoff` without `sew-app:handoff` are refused; the
//! bundled UI (`tauri://localhost`) needs only its own capability, never the remote one. The
//! API transport needs `sew-app:transport`, which neither `sew-app:default` nor the remote
//! capability holds.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use serde_json::{Value, json};
use tauri::ipc::{CallbackFn, CapabilityBuilder, InvokeBody, RuntimeAuthority};
use tauri::test::{INVOKE_KEY, get_ipc_response, mock_builder, mock_context, noop_assets};
use tauri::utils::acl::manifest::Manifest;
use tauri::utils::acl::resolved::Resolved;
use tauri::webview::InvokeRequest;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_sew_app::fake::FakeLane;
use tauri_plugin_sew_app::wire::{COMMANDS, TRANSPORT_COMMANDS};
use tauri_plugin_sew_app::{Builder, RemoteCapability};

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
        .filter(|c| **c != "handoff" && **c != "upload_file" && !TRANSPORT_COMMANDS.contains(c))
        .map(|c| allow(c))
        .collect();
    assert_eq!(default.len(), 17);
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
    assert!(read("upload-file.toml").contains("\"allow-upload-file\""));
    let d = read("default.toml");
    let listed = d.split("permissions = [").nth(1).unwrap();
    for c in TRANSPORT_COMMANDS.iter().chain(&["upload_file", "handoff"]) {
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
