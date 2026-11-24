# A Tauri App shell on a Sew server (`tauri-plugin-sew-app`)

A Tauri App on a Sew server (on K2, or a standalone Sew) is a thin shell: its window loads the
App's live site, and its Rust signs in to the App's server through the server's **native lane**
(`/sew/native/*`, lane 1, served by the App's server). `tauri-plugin-sew-app` is the part every
such shell needs; `tauri-plugin-sew-voice` is the call engine beside it. The crate's own
README is the full guide; this page is the checklist.

## What the plugin does

- **Servers**: `add_server` then `confirm_server` (the person checks the host first, in the
  bundled UI: a live site can't add or confirm a server), several accounts per server,
  everything keyed `{server, account}`; `servers.json` holds no secret. A live site's
  commands act only on its own server and its accounts, and `servers` lists only those.
- **Secrets**: the device bearer (`sewdv_…`) in the OS keychain (macOS Keychain, Windows
  Credential Manager, the Secret Service), a 0600 file only when the app opts in.
- **Sign-in**: the host's sign-in door when the server offers it (`info.signin.door`; done in
  Rust: the host's `/.k2/auth/ping`, the lane's `signin/door/begin`, the password to the
  confirmed server's own `/.k2/auth/sign-in`, the lane's `signin/door/finish`; K2's
  `password_change_required` through `sign_in_door_set_password`), the password form, and
  pairing with a code from a signed-in page, typed in or from a pairing link
  (`<scheme>://<host>/native/pair#code=…`, confirmed by the person; the
  `sew-app://pair_request` signal, then `pending_pair`).
  A server whose `info.shell.min` is above the shell's version refuses sign-in with
  `shell_too_old`. The password the person types in the shell's UI passes from the webview to
  Rust once and goes only to the host's own door (a pairing code typed there goes to Rust once
  the same way); the door's code and challenge, K2's change token and a pairing link's code
  never reach the webview.
  **Sign-out** ends the device
  session, deletes the bearer and calls the voice plugin's `forget_server` when no signed-in
  account is left on that Sew install. A server-side end (revoke, idle, replaced) turns the
  account `signed_out` with its reason on the next call and signals `sew-app://signed_out`.
- **Events** are signals only (`{"changed": true}`, `{"pending": true}`): a Tauri event reaches
  every page. Pages fetch the details with the scoped commands (`servers`, and in the bundled
  UI `pending_pair`, `pending_deep_links`); the host's Rust gets the data through
  `Builder::on_event`.
- **Voice**: `Builder::join_provider()`, the voice plugin's `JoinProvider` over the lane's
  `voice/join`, `voice/consent` and (Sew 0.9.0) `voice/recording` (Record and Stop; a lane without it is `recording_unsupported`). A `404` with no code of its own is `lane_not_found`; a `401`
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
`set_active`, `sign_in`, `pair`, `sign_in_door`, `sign_in_door_set_password`, `pair_confirm`,
`pair_decline`, `sign_out`, `devices`, `revoke_device`, `open` and `notify`; `sew-app:handoff`
is `handoff`.

The remote capability needs Tauri's `dynamic-acl` (in Tauri's defaults). A shell that loads the
live site never lists `tauri://localhost` in it: the bundled UI's own capability covers that
origin, and the plugin's capability test (`tests/tauri_acl.rs`, Tauri's mock runtime) checks
that a confirmed server's origin is allowed at run time while another origin, another window
and `handoff` without its set are refused, that a live site acts only on its own server and
can't add or confirm one, that `upload_file` takes only files picked in the file dialog, and
that a gateway socket answers only the window that opened it.

`sew-app:upload-file` (`pick_files`, `upload_file`) and `sew-app:transport` are for the bundled
UI's own capability; never grant them to the live site.

## Testing

- The crate's own tests: `cargo test` (the core and HTTP, with a replay of the lane's contract
  fixture), `cargo test --no-default-features` (the core alone) and
  `cargo test --features tauri-test` (the capability test on Tauri's mock runtime). In Sew's
  own tree, `scripts/check-tauri-app.sh` runs them with fmt, clippy, the public-crate check and
  a build of Sew's example shell.
- The fake lane (`fake::FakeLane`, `Builder::fake`) for tests and UI work, with no server.
- `tests/real_lane.rs`, built only with `--cfg sew_real_lane`, against a live App server's lane:
  `SEW_APP_LANE_URL`, `SEW_APP_LANE_USER`, `SEW_APP_LANE_PASSWORD` and optionally
  `SEW_APP_LANE_ROOM`, then `RUSTFLAGS="--cfg sew_real_lane" cargo test --test real_lane`.

## Platforms

Desktop only: macOS, Windows and Linux. The plugin has no iOS or Android secret store,
data-protection classes, push tokens or in-app QR pairing.
