# Changelog

## 0.5.2 (2026-11-18, Sew 0.10.5)

- No API or behaviour change. Requires tauri-plugin-sew-voice 0.8.3.

## 0.5.1 (2026-11-17, Sew 0.10.4)

### Dependencies

- Requires tauri-plugin-sew-voice 0.8.2.
- Through tauri-plugin-sew-voice and sew-call-client, updates the WebRTC dependencies
  (`sew-str0m` 0.25.5, `sew-str0m-is` 0.12.3); see sew-call-client's CHANGELOG.

### Added

- The crate is public (MIT OR Apache-2.0): it ships in the read-only sew-crates repository
  beside `tauri-plugin-sew-voice`, with its tests. Not on crates.io yet (see README.md).
- README.md says how to run the tests (`cargo test`, `--no-default-features`,
  `--features tauri-test`).
- The API transport, for an App whose UI ships in the binary: the page never holds the device
  bearer or a gateway ticket (README.md, "The API transport").
  - `api {key?, method, path, body?, idempotency_key?}` → `{status, headers, body}`: the App's
    client API through the lane (`/sew/native/api/v1/*`) with the bearer, the lane's path
    allowlist and body caps checked first, refusals in the lane's shape and status, only the
    safe answer headers.
  - `gateway_open {key?, on_event}`, `gateway_send {id, data}`, `gateway_close {id, code?,
    reason?}`: the gateway socket held in Rust (ticket from `/sew/native/ticket`, bearer as
    `Authorization`) and relayed over a `tauri::ipc::Channel` (`open`, `message`, `close` with
    the close code). IDENTIFY, RESUME and REAUTH get their ticket in Rust. No reconnect in
    Rust (the page's driver does it); at most two sockets per account, the lane's limit.
  - The `sewfile:` URI scheme for the signed byte routes (`Range` passed through), answered
    only to the bundled UI's pages. `Builder::file_protocol`, `Builder::file_protocol_origin`.
  - `upload_bytes` (the invoke's raw body) and `upload_file` (a file read in Rust) to
    `uploads/{id}/data`, `me/avatar`, `spaces/{id}/icon` and `spaces/{id}/emoji`.
  - New permission sets: `sew-app:transport` (`api`, the three gateway commands,
    `upload_bytes`) and `sew-app:upload-file`. `sew-app:default` and the remote capability
    are unchanged and hold neither.
  - In the core: `AppService::api`, `upload`, `fetch_bytes`, `gateway_open`, `gateway_send`,
    `gateway_close`; `Transport::send_bytes` (a provided method: a transport that carries only
    the lane's own routes need not implement it); `SocketConnector`, with
    `ws::WsConnector` (feature `http`) and the fake lane's; `Builder::socket_connector`.
  - The fake lane answers the transport: a few client API routes, uploads, the signed file
    routes with `Range`, tickets and the gateway socket (`drop_gateways`, `close_gateways`,
    `push_dispatch`).
- The bearer is read from the keychain once and kept in memory (one read at a time), not on
  every call.

### Dependencies

- Feature `http` adds `tokio-tungstenite` 0.26, `tokio-rustls` 0.26 and `futures-util` 0.3 for
  the gateway socket, at the versions sew-call-client already uses (no new crate in the
  tree), and tokio's `net`.

### Changed

- The contract fixture (`tests/native-lane.json`) names a neutral App: its address is
  `app.test` and its CSRF header `x-app-csrf`. Tests only; no API or behaviour change.

## 0.5.0 (2026-11-16, Sew 0.10.2)

### Breaking

- 0.5.0 follows tauri-plugin-sew-voice 0.8 and sew-call-client 0.8, whose types its provider
  (`LaneJoinProvider`) works with (see their CHANGELOGs). This crate's own API does not change.

### Dependencies

- Through tauri-plugin-sew-voice and sew-call-client, updates the WebRTC dependencies (str0m
  fork updates); recommended for everyone. `sew-str0m` 0.25.3 and `sew-str0m-is` 0.12.2 now
  come from crates.io; see sew-call-client's CHANGELOG. A host no longer needs a
  `[patch.crates-io]` row for `sew-str0m`; remove it if it has one.

## 0.4.0 (2026-11-15, Sew 0.10.0)

### Breaking

- 0.4.0 follows tauri-plugin-sew-voice 0.7 and sew-proto 0.10, whose types its provider works with.
- tauri-plugin-sew-voice 0.7.0 and sew-call-client 0.7.0: the voice plugin's types this crate's
  provider (`LaneJoinProvider`) works with are theirs, with their breaking changes (see their
  CHANGELOGs). This crate's own API does not change.

### Dependencies

- Through tauri-plugin-sew-voice 0.7.0 and sew-call-client 0.7.0, updates the WebRTC
  dependencies (str0m fork updates); recommended for everyone. Includes upstream str0m fixes
  for TWCC feedback handling (PRs #1151 and #1152).

## 0.3.0 (2026-11-12, Sew 0.9.3)

### Breaking

- **The host's sign-in door is done in Rust.** `signin_door_begin` and `signin_door_finish`
  (which handed the door's code through the page) are gone, with `AppService::door_begin`,
  `door_finish` and `DoorBegin`. The new `sign_in_door {server, username, password, account?}`
  checks the host's `GET /.k2/auth/ping` for `{"k2": true}`, asks the lane's
  `POST /sew/native/signin/door/begin`, checks that the begin's `door` is `/.k2/auth/sign-in`
  on the confirmed server (else `signin_door_mismatch`), posts
  `{username, password, challenge}` there with no Origin and a Content-Length, and trades the
  code at `POST /sew/native/signin/door/finish` `{id, code, device}`. The password, the code,
  the begin's id and challenge never reach the webview.
- The lane's door routes are `/sew/native/signin/door/begin` and `/finish` (`route::SIGNIN_BEGIN`,
  `route::SIGNIN_FINISH`). `DoorStart` is `{id, challenge, door, expires_in_ms}` (`state` is now
  `id`; `challenge` is new), deserialize-only, with the id and challenge as `Secret`s;
  `LaneClient::door_finish` takes the id as a `Secret`.
- `LaneInfo` has `recording` and `shell` (`ShellInfo {min}`); `AppConfig` has `shell_version`;
  `AppEvent` and `DeepLinkRoute` have new variants. `sew-app:default` lists the new commands.
- The fake's person `bob` signs in with `FakeLane::BOB_PASSWORD` (`bob-test-pw`).

### Added

- The host door's refusals keep its codes: `bad_login`, `locked`, `rate_limited` (with
  `detail.retry_after_ms` from `Retry-After` or `retryAfterSecs`; until then nothing more is
  sent there), `retry`, and `password_change_required`, whose change token stays in the plugin
  for `sign_in_door_set_password {server, password, account?}` (`/.k2/auth/set-password`, then
  the finish; `password_set_no_signin`, `no_password_change`). A refused password keeps the
  begin, so a retry goes with the same challenge.
- Version skew: `info.shell.min` above the shell's version (the app's own, or
  `Builder::shell_version`) refuses every sign-in with `shell_too_old` (`detail: {min, version}`).
- Pairing links: `<scheme>://<host>/native/pair#code=…` is never navigated or passed on. For a
  confirmed server the plugin holds the code for 2 minutes and sends `sew-app://pair_request`
  `{server, host, expires_in_ms}`; `pair_confirm {server, account?}` redeems it and
  `pair_decline {server}` drops it (`pairing_not_pending` without one). Any other host is
  `sew-app://pair_refused` `{host, reason}`, and nothing is redeemed.
- The fake lane serves the door (`/.k2/auth/ping`, `sign-in`, `set-password` and the lane's
  begin and finish) with `set_host_door`, `lock_user`, `require_password_change`,
  `set_app_access`, `k2_error` and `k2_rate_limited`.
- The contract test replays the lane's current fixture, the door's steps (answering the
  client's own requests to the host's door) and Record and Stop included.

### Changed

- Through sew-call-client, the WebRTC stack moves to str0m 0.25 (`sew-str0m` 0.25.1, Alakazam
  Labs' fork, from its git repository until it is on crates.io). A host drops the four str0m
  rows from its `[patch.crates-io]`; this crate's Cargo.toml has none.
- Through sew-call-client, updates the WebRTC dependencies (str0m fork updates); recommended
  for everyone.

## 0.2.1 (2026-11-11, Sew 0.9.2)

- On tauri-plugin-sew-voice 0.6.1 and sew-call-client 0.6.1. No API or behaviour change of
  its own.

## 0.2.0 (2026-11-10, Sew 0.9.0)

### Breaking

- On tauri-plugin-sew-voice 0.6 and sew-call-client 0.6: `LaneJoinProvider` implements
  sew-call-client 0.6's `JoinProvider`, so a host pairs this release with the voice plugin's
  0.6.

### Added

- Record and Stop (Sew 0.9.0): `Builder::join_provider()`'s provider sends the voice plugin's
  `start_recording` and `stop_recording` to the lane's `POST /sew/native/voice/recording`
  (`{target, room, action, recording_id?, device_id?}`) on the call's account, and reads Sew's
  recording back. A lane without the route answers `recording_unsupported`.

## 0.1.0 (2026-11-06, Sew 0.8.2)

The first version: the desktop half (macOS, Windows, Linux) of a Tauri 2 App shell on a Sew App
server's native lane (lane 1).

- **Servers.** `add_server` (asks the server's `/sew/native/info`), `confirm_server` (saves it;
  nothing is saved before the person confirms the host), `remove_server` (signs its accounts
  out first), `server_info`, `servers`. Several accounts per server: every account has its own
  device id (`device.id` of its sign-ins), and everything is keyed `{server, account}`.
  `servers.json` (0600) holds no secret.
- **Secrets.** `SecretStore`, with the OS credential store (`keyring-store`, the default:
  macOS Keychain, Windows Credential Manager, the Secret Service), an opt-in 0600 file
  (`SecretChoice::OsOrFile` / `File`, with the `secret_storage_file` warning), memory, or the
  host's own. `KeychainOptions.access_group` is reserved for phone keychains.
- **Sign-in.** `sign_in` (the password form), `pair` (a code a signed-in page shows), and
  `signin_door_begin` / `signin_door_finish` (the host's sign-in door, used only when the server
  says `info.signin.door`). The same person signing in again replaces their older account on
  that server.
- **Sign-out.** `sign_out` ends the device session on the server, deletes the bearer and, when
  no signed-in account is left on that Sew install, calls `tauri-plugin-sew-voice`'s
  `forget_server` (leaving a live call first). A session the server ends (a revoke, the idle
  limit, a replacement) is noticed on the next call: the account turns `signed_out` with the
  reason, the bearer is deleted, and `sew-app://signed_out` is sent. `devices`,
  `revoke_device`.
- **Voice.** `LaneJoinProvider`, a `JoinProvider` for `tauri-plugin-sew-voice` over the lane's
  `voice/join` and `voice/consent` (`Builder::join_provider`).
- **The webview.** `handoff` (permission `sew-app:handoff` only; answered only to a page on the
  account's own server) and `open` (the server's page in the calling window, through the App's
  handoff page when `Builder::handoff_page` is set).
- **Shell plumbing.** `Builder::install` adds single instance, deep links and notifications;
  each saved server's origin gets a run-time capability (`RemoteCapability`); deep links
  `<scheme>://<host>/<path>` open a saved server's page; `notify` shows a notification and sets
  the app badge.
- **Fake mode.** `fake::FakeLane` (the lane in memory), `Builder::fake`; Sew's example app
  runs on it.
