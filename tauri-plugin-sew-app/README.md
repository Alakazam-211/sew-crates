# tauri-plugin-sew-app

tauri-plugin-sew-app is the shell of a Tauri 2 App on a Sew server: saved servers, sign-in with
the device session in the OS keychain, a ready voice `JoinProvider` for
[`tauri-plugin-sew-voice`](../tauri-plugin-sew-voice), the handoff into the webview, deep links,
single instance and notifications. Desktop (macOS, Windows, Linux) for now; the phone half is
next (below).

A Tauri App on a Sew server is a thin shell: its window loads the App's live site, and its Rust
signs in to the App's server. Every such shell needs the same parts; this plugin is those parts.
It talks only to the App's server, through its **native lane** (`/sew/native/*`, lane 1), and
never to Sew directly.

Licence: MIT OR Apache-2.0.

- Source: <https://github.com/Alakazam-211/sew-crates> (read-only: it is generated from Sew's
  repository at each release, so pull requests are not merged there)
- API documentation: <https://docs.rs/tauri-plugin-sew-app> once it is on crates.io; until then
  `cargo doc -p tauri-plugin-sew-app --open` in the sew-crates repository
- Issues: <https://github.com/Alakazam-211/sew-crates/issues/new/choose>,
  with the `tauri-plugin-sew-app` form, which labels the issue `crate:tauri-plugin-sew-app`
- Security problems: report them privately, as
  [SECURITY.md](https://github.com/Alakazam-211/sew-crates/blob/main/SECURITY.md) says.
- MSRV: Rust 1.95.

## Quick start

The crate is not published on crates.io yet (the voice plugin and engine it builds on,
`tauri-plugin-sew-voice` and `sew-call-client`, are). Get it from the read-only
[sew-crates](https://github.com/Alakazam-211/sew-crates) repository, which holds it with the Sew
crates it depends on: clone it into your project (for example as `third_party/sew-crates`) and
copy the `[patch.crates-io]` row for `tauri-plugin-sew-app` its README lists into your app's
workspace root. It needs a Sew App server with a native lane to sign in to (see the same
README).

```toml
[dependencies]
tauri = "2"
tauri-plugin-sew-app = "0.5"
tauri-plugin-sew-voice = "0.8"
```

The shell's plugins go on the app's Tauri builder, with the voice provider that joins through
the lane:

```rust
use tauri_plugin_sew_app::{Builder, LaneJoinProvider, RemoteCapability};

fn sew_app<R: tauri::Runtime>(app: tauri::Builder<R>) -> (tauri::Builder<R>, LaneJoinProvider) {
    let shell = Builder::new()
        .deep_link_scheme("myapp")               // myapp://<host>/<path>
        .handoff_page("/native/handoff")         // the App's page that redeems a handoff code
        .remote_capability(Some(RemoteCapability::new().with_voice()));
    let provider = shell.join_provider();        // the voice plugin joins through the lane
    (shell.install(app), provider)               // single instance, deep links, notifications
}
```

and in `main`, the voice plugin on that provider:

```rust,ignore
fn main() {
    let (app, provider) = sew_app(tauri::Builder::default());
    app.plugin(tauri_plugin_sew_voice::Builder::new(provider).build())
        .run(tauri::generate_context!())
        .expect("the app runs");
}
```

`tauri.conf.json` lists the scheme for the deep-link plugin:

```json
{ "plugins": { "deep-link": { "desktop": { "schemes": ["myapp"] } } } }
```

and the bundled UI's capability lists `sew-app:default` (with `core:default` and
`sew-voice:default` as it needs). The fake mode (below) runs a shell with no server.
`docs/tauri-app-sdk.md` is the checklist for a whole shell.

## Servers and accounts

A server is added in two steps, so the person checks the host before anything is saved:

```js
const { invoke } = window.__TAURI__.core;
const app = (name, args) => invoke(`plugin:sew-app|${name}`, args);

const pending = await app("add_server", { url: "chat.example.com" });
// pending: {origin: "https://chat.example.com", host, info, saved, expires_in_ms}
// Show pending.host; on "Add":
const view = await app("confirm_server", { server: pending.origin });
```

`add_server` asks the server's `GET /sew/native/info`: an address that is not an App server
with a native lane is refused `not_a_sew_app`, an older lane `lane_too_old` (show "this server
needs an update"), `http://` `insecure_url` (loopback `http://` only with
`Builder::allow_loopback_http`, for development). A confirm waits at most 10 minutes.

`info.shell.min` is the oldest shell version the App works with. When the shell's version is
below it, every sign-in is refused `shell_too_old` with `detail: {min, version}` (show "update
the app"); the server can still be added, so the shell can say why. The shell's version is the
app's own (`tauri.conf.json`'s `version`), or `Builder::shell_version`. A `min` that doesn't
read as `x.y.z` is ignored.

Every server may hold **several accounts** (an admin's two identities, a shared tablet).
Everything is keyed by server and account: `key: {server, account}`, where `account` is the
account's own device id on this install, sent as `device.id` on its sign-ins. Two accounts on
one server are two device sessions; signing one in again (`account` named) replaces only its
own session; the same person signing in as a new account replaces their older account there.
One account is **active**: the voice provider and `open` use it when no `key` is named
(`set_active {key}`; a sign-in makes its account active).

`servers` answers the whole list, and `sew-app://servers` sends it after every change:

```json
{"servers": [{"origin": "https://chat.example.com", "host": "chat.example.com",
  "info": {"lane": 1, "signin": {"password": true, "pairing": false, "door": false},
           "shell": {"min": null}, "...": "..."},
  "accounts": [{"key": {"server": "https://chat.example.com", "account": "Qm9i..."},
    "member_id": "1002", "label": "Bob", "device_name": "Desktop (macOS)",
    "active": true, "state": "signed_in"}]}],
 "active": {"server": "https://chat.example.com", "account": "Qm9i..."},
 "secret_store": "keychain"}
```

The list lives in `servers.json` (mode 0600) under the app's data directory (`sew-app/`). It
never holds a secret.

## Secrets

Each account's device bearer (`sewdv_…`) is in a `SecretStore`:

| `Builder::secret_store` | Where |
|---|---|
| `SecretChoice::Os` (default) | The OS credential store (feature `keyring-store`): macOS Keychain, Windows Credential Manager, the Secret Service on Linux. Service `sew-app.<app id>`, one entry per server and account. With none answering, a sign-in fails `secret_store_unavailable`. |
| `SecretChoice::OsOrFile` | The OS store, else `secrets.json` (0600) under the data directory, with the `sew-app://warning` event `secret_storage_file` so the UI can say so. Only when the app opts in. |
| `SecretChoice::File` | Always the 0600 file (with the warning). |
| `SecretChoice::Memory` | Memory only (the fake mode, tests). |
| `SecretChoice::Host(store)` | The host's own (a vault). |

The webview never gets a bearer. A host whose own `keyring` uses `sync-secret-service` (keyring
refuses both Secret Service backends at once) turns this crate's default features off and
passes its store with `SecretChoice::Host`. `KeychainOptions { access_group }` is for phone
keychains (an access group shared with the App's extensions) and is ignored on desktop.

## Sign-in and sign-out

Offer the ways in in this order: the host's door when `info.signin.door`, then the password
form when `info.signin.password`, then a pairing code when `info.signin.pairing`.

| Command | What |
|---|---|
| `sign_in_door {server, username, password, account?}` | The host's sign-in door, all in Rust (below), when `info.signin.door` (else `signin_door_unavailable`). |
| `sign_in_door_set_password {server, password, account?}` | After `password_change_required`: the person's first password, then the sign-in. |
| `sign_in {server, username, password, account?}` | The password form (`POST /sew/native/signin/password`), when `info.signin.password` (else `password_unavailable`). The password is never kept. |
| `pair {server, code, account?}` | A pairing code a signed-in page showed (`POST /sew/native/pair/redeem`), when `info.signin.pairing` (else `pairing_unavailable`). |
| `pair_confirm {server, account?}` / `pair_decline {server}` | The person's answer to a pairing link (below). |
| `sign_out {key?}` → `{server_signed_out, forgot_install}` | Ends the device session (`POST /sew/native/signout`), deletes the bearer and the account. When no signed-in account is left on that Sew install, it calls the voice plugin's `forget_server` (its device keys and pins; a live call is left first). An unreachable server still loses the account here: `server_signed_out: false`, and the session ends on the server's idle limit. |
| `devices {key?}` / `revoke_device {key?, session}` | This person's device sessions; revoking this account's own is a sign-out. |

After a sign-in the plugin reads `whoami` for the person's name and the Sew install id. When the
server ends a session (a revoke, a replacement, the idle limit, an admin), the next call that
uses it answers `401 signed_out`: the account turns `{"state": "signed_out", "reason": "revoked"}`,
its bearer is deleted, `sew-app://signed_out {key, reason}` is sent, and the install's call
keys are forgotten when it was the last signed-in account there. The account stays listed
until the person signs it in again (`account` named) or signs it out.

Every rejection is `{code, detail, hint}`: the plugin's codes (`error::code`) or the lane's own
(`unauthenticated`, `locked`, `rate_limited`, `device_limit`, `code_invalid`,
`validation_failed`, …), with the server's sentence in `hint` when it sent one.

### The host's sign-in door

On a host that serves a sign-in door on the App's own address (K2's `/.k2/auth/*`), the
password goes from the shell's Rust to the host and never through the page, the App's server
or the lane. `sign_in_door` does it all:

1. `GET <server>/.k2/auth/ping` must answer exactly `{"k2": true}`; anything else is
   `signin_door_unavailable` with `detail.reason: "ping"`, and no password is sent (use the
   password form).
2. `POST /sew/native/signin/door/begin` `{device}` on the lane answers a one-time `id` and a
   PKCE `challenge` (the lane keeps the verifier), and the `door` URL. The plugin only checks
   `door`: it must be `/.k2/auth/sign-in` on the confirmed server, else `signin_door_mismatch`
   and nothing is sent. The password always goes to the confirmed server's own door, never to a
   URL from an answer or a page.
3. `POST <server>/.k2/auth/sign-in` `{username, password, challenge}`, JSON with a
   `Content-Length` and no `Origin`, answers a one-time code.
4. `POST /sew/native/signin/door/finish` `{id, code, device}` trades it for the device session.

The host's refusals keep its codes: `bad_login`, `locked`, `rate_limited` (with
`detail.retry_after_ms`; until then the plugin sends nothing there and answers `rate_limited`
at once), `retry`, and `password_change_required`. A refused password uses nothing, so the
plugin keeps the begin and a retry goes with the same challenge. On
`password_change_required` the plugin keeps K2's change token: ask for a new password and call
`sign_in_door_set_password` with the same `server` and `account`. It posts
`{token, password, challenge}` to `/.k2/auth/set-password` and finishes the sign-in; the
host's `password_too_short`, `password_too_long`, `password_unchanged` and `bad_token` pass
through, `password_set_no_signin` means the password is set but the person can't use this
App, and `no_password_change` means no change was pending. Neither the password, the code,
the challenge, the id nor the change token ever reaches the webview.

### Pairing links

A signed-in page can show a pairing code as a link, `<scheme>://<host>/native/pair#code=…`.
The plugin never opens it as a page and never passes it on. When `<host>` is a confirmed
server, it keeps the code for 2 minutes and sends `sew-app://pair_request`
`{server, host, expires_in_ms}`: ask "Sign in to <host>?", then call `pair_confirm {server}`
(which redeems the code, as `pair` does) or `pair_decline {server}`. Any other host is
`sew-app://pair_refused` `{host, reason: "unknown_server"}` (a link without a usable code:
`bad_link`), and nothing is redeemed. `pair_confirm` without a held code is
`pairing_not_pending`.

## Voice

`Builder::join_provider()` is a `JoinProvider` for `tauri-plugin-sew-voice`. It posts the
voice plugin's `JoinRequest` unchanged to `POST /sew/native/voice/join` and its consent answers
to `POST /sew/native/voice/consent`, with the active account's bearer; a call keeps the account
it joined with for its re-joins and consent, even when the person switches accounts meanwhile.
`room` is the channel id; `target: "dm"` (or `"call"`) asks for the DM call join.

| The lane answers | The voice plugin sees |
|---|---|
| `200 {join, api_base}` | the join (Sew's answer with its call credential) |
| `401` | `SigninRequired` (`signin_required`); the account is marked signed out |
| `409 recording_consent_required` with Sew's prompt | `ConsentRequired` (the call holds for the person's answer) |
| `429`, `503` | `Unavailable` with `Retry-After` |
| no answer | `Unavailable` |
| `404` with no code of its own | `Refused { code: "lane_not_found", detail: {route} }`: the server has no native lane, or it is off |
| anything else | `Refused` with the lane's `{code, detail}` |

## The webview

Two ways to bring the device's session into the App's page:

- **`open {key?, path?}`** loads `path` of the account's server in the calling window. With
  `Builder::handoff_page("/native/handoff")` it goes to
  `<page>#code=<code>&next=<path>`: the page posts the code once to
  `/sew/native/handoff/redeem` (the cookie lane: exact Origin, the App's CSRF header), drops
  the fragment and goes to `next`. Without a handoff page it loads `path` itself. The answer is
  the URL without its fragment.
- **`handoff {key?}`** → `{code, expires_in_ms, redeem_url}`, for a page that redeems the code
  itself. It is in its own permission set, `sew-app:handoff`, and the plugin answers it only to
  a page on the account's own server (`wrong_origin` otherwise): the code is a credential for
  that origin's cookie.

A code is single-use and lives 60 seconds.

## The API transport (a bundled UI)

An App that ships its own UI in the binary talks to its server through the plugin: the page
keeps its chat store and its rendering, and Rust does the connection work. The device bearer and
the gateway tickets stay in Rust; no command, event, channel message or URL the page sees holds
one. The commands are in their own set, `sew-app:transport` (below), for the bundled UI's
capability.

Everything goes through the lane's own routes (`/sew/native/api/v1/*`, `/sew/native/ticket`), so
any server that answers the lane's contract is served the same way.

- **`api {key?, method, path, body?, idempotency_key?}`** → `{status, headers, body}`. `path`
  is the route under `/api/v1` with its query (`/channels/5/messages?limit=50`); `method` is
  `GET` or `POST`. It goes to `/sew/native/api/v1<path>` with the bearer. The answer carries only
  `content-type`, `content-length`, `etag`, `retry-after`, `idempotent-replayed`,
  `cache-control` and `x-ratelimit-*`; never `Set-Cookie` or a credential. `body` is the JSON
  (or the text, or `null`). It fits a `fetch` the page's HTTP client is given: a refusal is an
  answer, not an error.
  - The path is checked first, as the lane checks it, and refused with the lane's status and
    body (`{ok: false, code, detail, error: {code, hint}}`): an auth route or the gateway's
    ticket `403 forbidden`, the gateway itself `426 upgrade_required`, another method `405
    method_not_allowed`, a dot segment, an encoded slash or an empty segment `404 not_found`, a
    body over the lane's cap `413 too_large` (`detail.max`). The signed byte routes go through
    `sewfile:` (`404`), and the image routes through `upload_bytes` (`415`). `token=` query
    parameters are dropped.
  - A lane `401` (`signed_out`, `unauthenticated`) is passed to the page and marks the account
    signed out (`sew-app://signed_out`); the next call is answered the same `401` without
    being sent.
  - Errors: `unreachable`, `unknown_account`, `no_account`, `invalid_args` (a `GET` with a body,
    a bad `idempotency_key`).
- **`gateway_open {key?, on_event}`** → `{id}`. Rust asks `POST /sew/native/ticket`, checks
  that the answer names this server's `wss://<host>/sew/native/api/v1/gateway?v=1&encoding=json`,
  opens it with the bearer as `Authorization`, and relays it on the channel `on_event`:
  `{event: "open", id}`, then `{event: "message", data}` for every text frame, then one
  `{event: "close", code, reason, clean}` (`code` is the server's close code, `4004`, `4011`,
  ...; `1006` with `clean: false` when the socket broke without one).
  - The socket is plain: it never reconnects. The page's gateway driver does that, as over a
    browser `WebSocket`, by opening a new one.
  - At most two sockets per account, the lane's limit per device (a reconnect may overlap the
    old socket): a third is refused `rate_limited` (`detail.max: 2`) before anything is sent. A
    server's refusal keeps its code (`rate_limited` with `detail.retry_after_ms`,
    `signed_out`).
  - Signing out (or the session ending) closes the account's sockets.
- **`gateway_send {id, data}`** sends one frame. The page sends IDENTIFY (op 2) and RESUME
  (op 4) with any placeholder in `d.ticket`, and REAUTH (op 11) with one in `d.token`; Rust puts
  a ticket there (the socket's own for the first, a fresh one after). Other frames go as they
  are. `unknown_socket` when the socket is gone.
- **`gateway_close {id, code?, reason?}`** → whether it was open. `code` is `1000` (the
  default) or `3000` to `4999`; the `close` event follows.
- **`sewfile:` URLs** serve the signed byte routes to `<img>`, `<video>` and `fetch`:
  `sewfile://localhost/<account>/<path>`, where `<account>` is an account id or `active` and
  `<path>` the route under `/api/v1` with its query, such as
  `sewfile://localhost/active/files/9?exp=…&sig=…`. Tauri's `convertFileSrc("active/files/9?exp=…",
  "sewfile")` builds the platform's form (`http://sewfile.localhost/…` on Windows and Android).
  - Only the byte routes: `files/{id}` (with `exp` and `sig`), its `thumb`, `preview` and
    `poster`, `avatars/…`, `emoji-images/…`, `space-icons/…`, `gifs/media/…`,
    `previews/{id}/image`, `uploads/{id}/data`; never `recordings/`, `compliance/` or
    `digest/`. `GET` and `HEAD`.
  - `Range`, `If-Range`, `If-None-Match` and `If-Modified-Since` pass through; the answer keeps
    Sew's headers (its sandbox CSP, `nosniff`, `Content-Range`, ...) but the hop-by-hop ones,
    cookies and credentials. A larger file than 64 MiB is read in parts (`Range`).
  - Only the bundled UI's pages load them (`tauri://localhost`, `http(s)://tauri.localhost`, and
    the `devUrl` in a development build; `Builder::file_protocol_origin` adds one). A server's
    live site in the same webview gets `403`. `Builder::file_protocol(false)` turns the scheme
    off.
- **`upload_bytes`**: the invoke's raw body, with the headers `sew-path` (`/uploads/{id}/data`,
  `/me/avatar`, `/spaces/{id}/icon` or `/spaces/{id}/emoji`), `content-type`
  (`application/octet-stream`, the default, or `image/*`), optionally `content-range`, and
  `sew-server` with `sew-account` for another account than the active one:
  `invoke('plugin:sew-app|upload_bytes', bytes, {headers: {'sew-path': '/uploads/9/data'}})`.
  Answers like `api`. The lane's caps: 16 MiB for an upload's data, 5 MiB for an image, 256 KiB
  for an emoji.
- **`upload_file {key?, path, file, content_type?, content_range?}`**: the same, with the bytes
  read in Rust from `file`. In its own set, `sew-app:upload-file`: the page names a path on this
  machine, so grant it only to a bundled UI that takes the path from the person's own pick.

The bearer is read from the keychain once and kept in memory for the calls after it (one read
at a time, so a burst of calls on start reads it once); signing in again, signing out or the
session ending drops it. The device session's renewal is the server's (the lane renews its
own sign-in), so the page never refreshes anything.

## Shell plumbing

- **The remote capability.** The shell's window shows the App's live site, so that origin needs
  a capability to call the plugins. For every saved server, at start and on each confirm, the
  plugin adds one at run time (Tauri's `dynamic-acl`): `remote.urls = ["<origin>/*"]`, the
  windows and permissions of `RemoteCapability` (default window `main`, `core:event:default`
  and `sew-app:default`; `.with_voice()` adds `sew-voice:default` and `sew-voice:call-details`;
  `.permission("sew-app:handoff")` for a page that redeems codes itself). The bundled UI
  (`tauri://localhost`) is never listed in it and keeps the app's own capability files. A
  removed server's capability stays until the app restarts (Tauri can't remove one at run
  time); nothing navigates there. `Builder::remote_capability(None)` turns it off.
- **Deep links.** With `Builder::deep_link_scheme("myapp")`, `myapp://<host>[:port]/<path>`
  opens `https://<host>/<path>` in the main window when `<host>` is a saved server (signed in
  through the handoff page when the active account is on it), and brings the window forward.
  A pairing link (`myapp://<host>/native/pair#code=…`) asks first (above). Any other link is
  only the `sew-app://deep_link` event (`{url, server, path, navigated}`); a link never adds a
  server. Schemes are registered with the OS at start on Linux and Windows
  in debug builds (`Builder::register_schemes`); an installer registers them in a release.
- **Single instance.** A second launch brings the running window forward; its deep link reaches
  the running instance.
- **Notifications.** `notify {title?, body?, badge?}` → `{shown, badge}`: a notification
  through `tauri-plugin-notification`, and the app badge (`badge: 0` clears it; absent leaves
  it). The badge answers `unsupported` on Windows, which has none.

`Builder::install(tauri::Builder)` adds `tauri-plugin-single-instance` (first, as it must be),
`tauri-plugin-deep-link`, `tauri-plugin-notification` and this plugin. A host that registers
them itself uses `Builder::build()` instead.

## Capability sets

| Set | Commands |
|---|---|
| `sew-app:default` | `servers`, `add_server`, `confirm_server`, `remove_server`, `server_info`, `set_active`, `sign_in`, `pair`, `sign_in_door`, `sign_in_door_set_password`, `pair_confirm`, `pair_decline`, `sign_out`, `devices`, `revoke_device`, `open`, `notify` |
| `sew-app:handoff` | `handoff` |
| `sew-app:transport` | `api`, `gateway_open`, `gateway_send`, `gateway_close`, `upload_bytes` |
| `sew-app:upload-file` | `upload_file` |

The default set is the shell's own UI; it holds neither the transport nor `upload_file`, and the
remote capability holds `sew-app:default` only. Each command also has its own
`sew-app:allow-<command>`. Events (`sew-app://servers`, `signed_out`, `warning`, `deep_link`,
`pair_request`, `pair_refused`) go to every window. A typical App lists, for its bundled UI:
`core:default`, `sew-app:default`, `sew-app:transport`, `sew-voice:default`; and for its live
site (the remote capability): `core:event:default`, `sew-app:default`, `sew-voice:default`,
`sew-voice:call-details` (and `sew-voice:video` for video).

## Fake mode

`fake::FakeLane` is the lane in memory: password sign-in and pairing, device sessions per person
and device id (a sign-in with the same id replaces, 10 at most), `401 signed_out` with its
reason, single-use codes, the voice join (with a call credential) and consent, recorded rooms,
and the host's sign-in door on the same address (`set_info(|i| i.signin.door = true)` offers
it; `set_host_door`, `lock_user`, `require_password_change` and `set_app_access` steer it).
`Builder::fake(lane)` runs the plugin on it with memory secrets and nothing saved; tests build
`AppService` over it directly. It also answers the API transport: a few client API routes
(`me`, posting a message with `Idempotency-Key`, uploads and their data, `me/avatar`), the
signed file routes with `Range`, gateway tickets, and the gateway socket (`SocketConnector`:
HELLO, READY for a valid ticket, RESUMED, heartbeats, 2 sockets per device;
`drop_gateways`, `close_gateways` and `push_dispatch` steer it). Its test person is `bob`, with the placeholder password
`FakeLane::BOB_PASSWORD`.

## Features

| Feature | Default | What |
|---|---|---|
| `tauri` | yes | The Tauri glue (needs Tauri's Linux libraries: webkit2gtk-4.1, gtk3, libsoup3). |
| `http` | yes | The lane over HTTPS: rustls with the platform's trust store, the voice engine's TLS setup. No cookie, no Origin, no redirects followed. The gateway socket over WebSocket (`tokio-tungstenite`, as sew-call-client). |
| `keyring-store` | yes | The OS credential store. |
| `builtin-ring-provider` | yes | The TLS fallback to ring when the host installs no rustls provider. |
| `tauri-test` | no | Tests only: Tauri's mock runtime. |

With none, the crate is the core (`AppService`, the lane client over a `Transport`, the stores,
the provider, the fake lane), which builds and tests without Tauri's system libraries.

## Tests

`cargo test` runs the core and HTTP tests, among them a replay of the App server lane's
contract fixture (`tests/native-lane.json`, the API transport's steps included) and the API
transport on the fake lane (`tests/transport.rs`); `cargo test --no-default-features` the core
alone; `cargo test --features tauri-test` adds the capability tests on Tauri's mock runtime.
`tests/real_lane.rs` runs against a live App server and is built only with
`--cfg sew_real_lane` (its header says how).

## Next: phones

Not in this version: the iOS Keychain store with an access group shared with the App's
extensions (a Notification Service Extension, a ReplayKit extension), the Android
Keystore-wrapped store, data-protection classes for the files Rust writes, push-token
registration, and in-app QR pairing. The store interface (`SecretStore`, `KeychainOptions`) is
shaped for them.
