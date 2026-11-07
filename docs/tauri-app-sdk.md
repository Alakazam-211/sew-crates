# A Tauri App shell on a Sew server (`tauri-plugin-sew-app`)

A Tauri App on a Sew server (on K2, or a standalone Sew) is a thin shell: its window loads the
App's live site, and its Rust signs in to the App's server through the server's **native lane**
(`/sew/native/*`, lane 1, served by the Seed's BFF). `tauri-plugin-sew-app` is the part every
such shell needs; `tauri-plugin-sew-voice` is the call engine beside it. The crate's README
(`crates/tauri-plugin-sew-app/README.md`) is the full guide; this page is the checklist.

## What the plugin does

- **Servers**: `add_server` then `confirm_server` (the person checks the host first), several
  accounts per server, everything keyed `{server, account}`; `servers.json` holds no secret.
- **Secrets**: the device bearer (`sewdv_…`) in the OS keychain (macOS Keychain, Windows
  Credential Manager, the Secret Service), a 0600 file only when the app opts in.
- **Sign-in**: the password form, pairing with a code from a signed-in page, and the host's
  sign-in door when the server offers it (`info.signin.door`). **Sign-out** ends the device
  session, deletes the bearer and calls the voice plugin's `forget_server` when no signed-in
  account is left on that Sew install. A server-side end (revoke, idle, replaced) turns the
  account `signed_out` with its reason on the next call and sends `sew-app://signed_out`.
- **Voice**: `Builder::join_provider()`, the voice plugin's `JoinProvider` over the lane's
  `voice/join` and `voice/consent`. A `404` with no code of its own is `lane_not_found`; a `401`
  is `signin_required`.
- **The webview**: `open` (the site's page, signed in through the App's handoff page) and
  `handoff` (a 60 s code for the page's cookie, only to a page on that server).
- **Shell plumbing**: a run-time remote capability per saved server, deep links
  (`<scheme>://<host>/<path>`), single instance, and `notify` (a notification and the badge).

## Wiring

```rust
let app = tauri_plugin_sew_app::Builder::new()
    .deep_link_scheme("myapp")
    .handoff_page("/native/handoff")
    .remote_capability(Some(tauri_plugin_sew_app::RemoteCapability::new().with_voice()));
let provider = app.join_provider();
app.install(tauri::Builder::default())   // single instance, deep links, notifications
    .plugin(tauri_plugin_sew_voice::Builder::new(provider).build())
    .run(tauri::generate_context!())?;
```

## The capability sets an App needs

| Where | Permissions |
|---|---|
| The bundled UI (`tauri://localhost`), in the app's capability file | `core:default`, `sew-app:default`, `sew-voice:default` |
| The App's live site (the plugin's run-time remote capability, `remote.urls = ["<origin>/*"]`) | `core:event:default`, `sew-app:default`, with `.with_voice()`: `sew-voice:default`, `sew-voice:call-details`; add `sew-voice:video` for video |
| A page that redeems handoff codes itself | add `sew-app:handoff` to the remote capability only |
| A window that signs out and wipes call keys by hand | `sew-voice:device-keys` (the plugin's `sign_out` does it from Rust without it) |

`sew-app:default` is `servers`, `add_server`, `confirm_server`, `remove_server`, `server_info`,
`set_active`, `sign_in`, `pair`, `signin_door_begin`, `signin_door_finish`, `sign_out`,
`devices`, `revoke_device`, `open` and `notify`; `sew-app:handoff` is `handoff`.

The remote capability needs Tauri's `dynamic-acl` (in Tauri's defaults). A shell that loads the
live site never lists `tauri://localhost` in it: the bundled UI's own capability covers that
origin, and the plugin's capability test (`tests/tauri_acl.rs`, Tauri's mock runtime) checks
that a confirmed server's origin is allowed at run time while another origin, another window
and `handoff` without its set are refused.

## Testing

- `scripts/check-tauri-app.sh` (a k2 compute job; see its header): fmt, clippy and tests of the
  core, the default features and the mock-runtime capability test, the public-crate check, and
  a build of `examples/tauri-app-example`.
- The fake lane (`fake::FakeLane`, `Builder::fake`) for tests and UI work;
  `SEW_APP_FAKE=1` runs the example with no server.
- `tests/real_lane.rs` (ignored) against a live App server's lane: `SEW_APP_LANE_URL`,
  `SEW_APP_LANE_USER`, `SEW_APP_LANE_PASSWORD` and optionally `SEW_APP_LANE_ROOM`, then
  `cargo test --test real_lane -- --ignored`.

## Not yet

The phone half: the iOS Keychain store with an access group for the App's extensions, the
Android Keystore store, data-protection classes, push tokens and in-app QR pairing.
