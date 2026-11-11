# Native voice in a Tauri app (`tauri-plugin-sew-voice`)

This guide is for developers who add Sew voice calls to a Tauri 2 app on desktop, Android or
iOS: K2's own desktop, or a third-party App on K2's App system. The call runs in native Rust inside your app's
process. Your webview only draws the UI, and **no token ever reaches the webview**: your app's
Rust code signs in, and the plugin keeps every credential in Rust memory.

> **Status (0.4.1).** The plugin's API below is frozen with K2. It runs on Sew's native voice engine, `sew-call-client`: MLS call keys, the mandatory
> E2EE checks, TURN relay, and K2's kessel-audio pipeline on the system's microphone and
> speakers (on phones, the OS's own voice processing). An in-memory **fake engine** is there for tests and offline UI work (§8).

Browsers and web Apps don't use this plugin. They keep `@sew/voice`.

## 1. Add the plugin

`src-tauri/Cargo.toml`:

```toml
[dependencies]
tauri = { version = "2" }
tauri-plugin-sew-voice = { git = "…", tag = "…" }   # vendored or mirrored; not on crates.io yet
```

The plugin depends on `sew-call-client` and its path crates (`sew-lk-compat`, `sew-lkproto`,
`sew-proto`, `sew-e2ee`, `sew-mls-crypto`), and through it on K2's `kessel-audio` and
`kessel-video` (both from crates.io) and Sew's str0m fork (dimpl comes from crates.io). Cargo applies `[patch]` only from the workspace you build, so copy the plugin's
`[patch.crates-io]` block (the end of its `Cargo.toml`) into your app's workspace root. The
packaged crates (`scripts/package-public-crates.sh`, published read-only as `sew-crates`) carry a
README with the same rows. Linux builds need ALSA's development files (`libasound2-dev`; Arch:
`alsa-lib`).

`src-tauri/src/main.rs`:

```rust
fn main() {
    let provider = MyJoinProvider::new(/* your App's session, kept in Rust */);
    tauri::Builder::default()
        .plugin(tauri_plugin_sew_voice::Builder::new(provider).build())
        .run(tauri::generate_context!())
        .expect("tauri");
}
```

`build()` makes the real engine (`real::CallClientEngine::with_mls`). These settings are all
optional:

- `.data_dir(dir)`: where the engine keeps its device store (§6). Default: the app's data
  directory, `sew-voice/`.
- `.device_key_store(store)`: your own store for the device key (§6). Default: a 0600 file
  under the data directory.
- `.hardware_device_key(key)` (0.3.0): the device key in the Secure Enclave or StrongBox/TEE
  (§6); no software fallback.
- `.device_platform(platform)` (0.3.0): the platform the device enrols as. Default from
  `target_os` (`ios`, `android`, otherwise `desktop`).
- `.app_id(id)`: the name the OS credential store entries go under (`sew-voice.<id>`), only
  with the optional `keyring-store` feature. Default: the app's identifier from
  `tauri.conf.json`.
- `.crypto_provider(provider)` (0.4.1): the rustls crypto provider for every TLS connection the
  engine makes (signalling, TURN over TLS, Sew's API), as an
  `Arc<sew_call_client::rustls::crypto::CryptoProvider>`. See "TLS crypto provider" below.

`.engine(..)` replaces the engine, for tests and the fake (§8). The plugin never falls back to
the fake by itself.

`src-tauri/capabilities/default.json` (allow the plugin for your window):

```json
{ "identifier": "default", "windows": ["main"], "permissions": ["core:default", "sew-voice:default"] }
```

### TLS crypto provider (0.4.1)

The engine picks its rustls crypto provider once per TLS setup, in this order:

1. **Yours**, if you pass one: `.crypto_provider(provider)` on the plugin's builder, or
   `ClientConfig::with_crypto_provider(provider)` when you use `sew-call-client` directly.
2. **The process default**, if your app installed one with
   `rustls::crypto::CryptoProvider::install_default` (for example a desktop app that already
   picks its own provider for all its rustls connections). Nothing else needs doing: the engine
   uses it.
3. **The built-in one, ring**, only if neither of the above is there. It sits behind the
   feature `builtin-ring-provider`, on by default in both crates. Turn it off (in
   `tauri-plugin-sew-voice`, `default-features = false` and list the features you keep) and
   the engine never guesses: with no provider passed or installed, a join fails with
   `engine_stopped`, its detail naming `no_crypto_provider`.

The engine logs its choice at info level the first time and whenever it changes:
`tls provider: <name> (caller | process default | built-in)`, where the name is the provider's
random source's debug name (`Ring` for ring's). The provider covers TLS only; the call's own
DTLS and SRTP keep their own backend (aws-lc-rs on desktop, RustCrypto with `rust-crypto` or
`mobile`).

### Content Security Policy

The webview reaches the plugin through Tauri's IPC, so the page's CSP must allow Tauri's IPC
origin in `connect-src`. Tauri 2 serves it as `ipc://localhost` on macOS, iOS and Linux, and as
`http://ipc.localhost` on Windows and Android (`https://ipc.localhost` there if your app turns
on Tauri's `useHttpsScheme`). Our examples use this line, which covers every platform:

```
connect-src ipc: http://ipc.localhost
```

(in `app.security.csp` of `tauri.conf.json`, beside your other directives). Without it the
commands fail and no `sew-voice://` event arrives.

A remote page the shell loads (a live site, not the app's bundled assets) sends its own CSP, so
that line goes in the site's `Content-Security-Policy` header. The site's origin also needs a
Tauri capability with `remote.urls` naming it, listing the plugin's permissions (§4); without
one, Tauri refuses every command from that page.

## 2. Write a `JoinProvider`

The provider is the only thing your app has to write. Its `join` is called to join and again
to re-join when Sew drops the join (`req.rejoin` says why); its `consent` sends the person's
answer to a recording's notice (Sew 0.7.3, §3a), and its `recording` the person's Record or
Stop (Sew 0.9.0, §3a). Both have a default that sends nothing:

```rust
pub trait JoinProvider: Send + Sync + 'static {
    fn join(&self, req: JoinRequest) -> BoxFuture<'_, Result<JoinAnswer, ProviderError>>;
    fn consent(&self, answer: ConsentAnswer) -> BoxFuture<'_, Result<Recording, ProviderError>> { /* consent_unsupported */ }
    fn recording(&self, req: RecordingRequest) -> BoxFuture<'_, Result<Recording, ProviderError>> { /* recording_unsupported */ }  // Sew 0.9.0
}
pub enum RecordingControl { Start, Stop }                      // "start", "stop"
pub struct RecordingRequest { pub target: String, pub room: String, pub action: RecordingControl,
                              pub channel_id: Option<Id> /* not in the JSON */,
                              pub recording_id: Option<Id>, pub device_id: Option<String> }
impl RecordingRequest  { pub fn path(&self) -> String; pub fn body(&self) -> serde_json::Value;
                         pub fn recording_from_json(body: &[u8]) -> Result<Recording, ProviderError>; }
pub struct JoinRequest { pub target: String, pub room: String, pub muted: bool,
                         pub deafened: bool, pub rejoin: Option<RejoinReason>,
                         pub recording_notice: Option<RecordingNotice> }   // Sew 0.7.3
pub enum RejoinReason  { AdmitLost, CredentialExpired }        // "admit_lost", "credential_expired"
pub struct JoinAnswer  { pub join: sew_proto::voice::VoiceJoinResponse, pub api_base: url::Url, /* + the call credential */ }
impl JoinAnswer        { pub fn from_json(body: &[u8], api_base: url::Url) -> Result<JoinAnswer, AnswerError>; }
pub enum ProviderError { SigninRequired, Refused { code: String, detail: serde_json::Value },
                         Unavailable { retry_after_ms: Option<u64> }, Other(String),
                         ConsentRequired(Box<ConsentRequired>) }            // Sew 0.7.3
impl ProviderError     { pub fn from_sew_error(status: u16, retry_after_ms: Option<u64>, body: &[u8]) -> ProviderError; }
```

These are `sew-call-client`'s own types, re-exported by the plugin, so one provider serves the
plugin and the engine alike.

- Build the answer from the body Sew returned with **`JoinAnswer::from_json(body, api_base)`**:
  it keeps the answer's `call_credential`, which the engine needs. A provider that talks to the
  App server route below can use **`tauri_plugin_sew_voice::app_server_answer(body)`** on the
  route's `200` body instead.
- `RejoinReason::AdmitLost`: the 20 s reconnect window ran out, or the media engine said the
  admit is gone. `CredentialExpired`: the call credential expired (a long sleep) and can't be
  renewed.

- `target` and `room` are whatever your webview passed to `join`. The plugin doesn't read them.
  An App usually sends a channel id as `room`, and a space or workspace name as `target`.
- **Your provider calls Sew's join for the signed-in person with `call_credential: true`**, and
  without an `e2ee` object. That's Sew's voice join `POST /api/v1/channels/{id}/voice/join`,
  or the DM call join `POST /api/v1/channels/{id}/call/join`. It returns Sew's answer **unchanged** as `join`. That answer carries the
  media token and the call credential (`sewcc_…`), which the engine uses for its own key
  traffic, leave, mute state and stats. Renewals don't go through your provider: the engine
  renews the media token over signalling and the credential with Sew itself, at half of each
  token's life, retrying a renewal that fails on the network or with a `5xx` (1 s, doubling to
  8 s) until the credential runs out.
- **`api_base` is the door**: the HTTPS base where the engine reaches Sew's client API for
  this call.
  - **K2:** the hosting daemon on `https://<label>.k2.dev`, which forwards only the
    call-credential routes and the gateway socket to Sew.
  - **An App:** the App's native lane on its own origin. With `@sew/bff` that's
    `https://<your app>/sew/native/`.
  - **A standalone Sew that publishes its API:** Sew's `server.public_url`.
  - Never `turn.<label>` or `voice.<label>`: those are the media engine's, and they don't reach
    Sew's API.
- Map failures (`ProviderError::from_sew_error` does it for Sew's error body):
  - your session is gone → `SigninRequired` (the call ends `signin_required`; show sign-in);
  - Sew's `409 recording_consent_required` → `ConsentRequired` with its `error.details`
    unchanged (the call is held for the person's answer, §3a; mapped to `Refused` the join
    just fails);
  - Sew or your server refused → `Refused { code, detail }`, passed to the webview as given;
  - down or rate-limited → `Unavailable`;
  - anything else → `Other`.
- Forward `req.recording_notice`, when there is one, as the voice join's `recording_notice`
  unchanged: it is the person's answer to the notice, and Sew lets a join into a recorded room
  in only with it.

  Keep tokens out of `Other`'s text and out of `Refused.detail`: both reach the webview.

There are two shapes in practice:

- **K2's desktop:** the provider calls the daemon BFF (`/cli/voice/join`) over the saved login.
- **A third-party App:** the provider calls **your App's server**, which holds your App's Sew
  access and does the join. The example's provider (`examples/tauri-voice-example/src-tauri/src/provider.rs`)
  is a complete one.

### The App server's route

The example's provider uses this route on your server, and nothing else:

| | |
|---|---|
| Request | `POST <your join URL>`, `Authorization: Bearer <your App's session>`, body `{"target", "room", "muted", "deafened", "rejoin", "recording_notice"?}` (`rejoin`: `null`, `"admit_lost"` or `"credential_expired"`; `recording_notice` only when the person answered a notice; the `JoinRequest`'s JSON) |
| `200` | `{"join": <Sew's VoiceJoinResponse, unchanged>, "api_base": "https://…"}` |
| `401` | the session is gone → `SigninRequired` |
| `409` | `{"code": "recording_consent_required", "detail": <Sew's error.details, unchanged>}` → `ConsentRequired` (§3a) |
| `429` / `503` | busy; `Retry-After` in seconds → `Unavailable` |
| other | `{"code", "detail"}` → `Refused`, shown to the user as given |

Your server checks that the person may join `room`, calls Sew's join with `call_credential:
true` (and the `recording_notice` as given), and returns the answer. Leave, mute and stats go
from the engine to Sew directly with the call credential.

Recording consent (Sew 0.7.3) is the person's act, so it goes through your server with the
person's session, never with the call credential. The example's provider uses a second route
for it (`SEW_VOICE_CONSENT_URL`):

| | |
|---|---|
| Request | `POST <your consent URL>`, `Authorization: Bearer <your App's session>`, body the `ConsentAnswer`'s JSON: `{"target", "room", "recording_id", "consent", "notice_version", "device_id"}` |
| Your server | `POST /api/v1/recordings/{recording_id}/consent` for the person with `{"consent", "notice_version"}` (`ConsentAnswer::body()`; `notice_version` is `null` for "Stop recording me") |
| `200` | Sew's answer, the recording as the person sees it, unchanged (`ConsentAnswer::recording_from_json`) |
| `409` | as the join's (the notice changed: `ConsentRequired` with the new one) |
| other | as the join's |

A host without such a route keeps `consent`'s default: the answer is refused
`consent_unsupported` and the person stays unrecorded.

Record and Stop (Sew 0.9.0) are the person's acts too, so they go the same way, never with the
call credential. The example's provider uses a third route for them
(`SEW_VOICE_RECORDING_URL`); the Sew App kit's native lane serves the same body at
`POST /sew/native/voice/recording` (beside `/sew/native/voice/join` and `…/consent`; its
`/sew/native/info` says `recording: true`), and `tauri-plugin-sew-app`'s provider uses it:

| | |
|---|---|
| Request | `POST <your recording URL>`, `Authorization: Bearer <your App's session>`, body the `RecordingRequest`'s JSON: `{"target", "room", "action", "recording_id"?, "device_id"?}` (`action`: `"start"` or `"stop"`; `recording_id` on a Stop) |
| Your server | checks that the person may act on `room` and maps it to Sew's channel (as its join does), then calls Sew for the person with `{}`: `POST /api/v1/channels/{channel_id}/recording/start` for a Start, `POST /api/v1/recordings/{recording_id}/stop` for a Stop. A provider that calls Sew itself uses `RecordingRequest::path()` (the engine fills in the call's channel, which the JSON leaves out) |
| `200` | Sew's answer, the recording as the person sees it, unchanged (`RecordingRequest::recording_from_json`) |
| other | as the join's: `{"code", "detail"}` with Sew's reason as `code` (`recording_active`, `recording_disabled`, `not_in_voice`, `cap_ceiling`, `recording_required`, …) |

A host without it keeps `recording`'s default: Record and Stop are refused
`recording_unsupported`. Leave the `sew-voice:recording` permission out (§4) and show no
buttons.

**An App whose server serves the native lane** (`/sew/native/*`, the Seed's BFF) doesn't write
a provider: `tauri-plugin-sew-app` has one (`Builder::join_provider()`), with the sign-in, the
device bearer in the OS keychain and the sign-out that calls `forget_server`. See
[docs/tauri-app-sdk.md](tauri-app-sdk.md).

## 3. The webview API

Commands are invoked as `plugin:sew-voice|<name>`. Every command rejects with `{code, detail}`.

| Command | Arguments | Answer |
|---|---|---|
| `join` | `{target, room, muted?, deafened?, decode?}` | `{call_id}`. One call per app: a second `join` while one is live rejects with `already_in_call`. `decode`: the video codecs the webview decodes (its WebCodecs probe, `["vp8", "h264"]`); the engine offers to receive only what it or the webview decodes (VP8 always; H.264 never on Linux). Without it nothing is assumed about the webview |
| `leave` | — | `null`. The state goes `ended {reason: "left"}` at once. A no-op with no call. If Sew's leave fails, an `errors` event says so. It never waits on a camera or screen command that waits on a device (a slow or suspended camera), and a `join` right after it is never refused `already_in_call` |
| `mute` | `{muted?, deafened?}` (at least one) | `null`. Deafened implies muted; un-deafening returns `muted` to its value before the deafen. `not_in_call` with no call |
| `devices` | — | `{inputs: [{id, name, default}], outputs: [...], input, output}` (`input`/`output`: `null` = the OS default) |
| `set_device` | `{input?, output?}` (at least one): a device id, or `"default"` for the OS default (followed when it changes later; `devices` then says `null`). An absent or `null` direction stays as it is | `null`. Hot-swaps the stream without leaving the call. On an input, it resolves while the OS permission prompt is up and rejects `mic_denied` once the microphone was refused (§5) |
| `state` | — | `{state, attempt?, reason?, recording_id?, notice_version?, target?, room?, participants, call_id, recording?, recording_controls?}`. Use it when a window opens mid-call (`recording_controls` from plugin 0.6.1, only while one of them is `true`) |
| `consent` | `{granted, notice_version}` | `null`. The person's answer to the recording notice (§3a). Rejects `not_in_call`, `no_recording`, `recording_consent_required` (the notice changed: `detail` has the new one), `consent_unsupported`, or your provider's refusal |
| `withdraw_consent` | — | `null`. "Stop recording me" (§3a). Rejects as `consent` |
| `start_recording` | — | `null`. Record (Sew 0.9.0, §3a): the person's own act, sent by your provider; the `recording` event follows Sew's answer. Not in `sew-voice:default` (§4). Rejects `not_in_call`, `not_in_voice` (the join is held for a notice), `recording_unsupported`, or Sew's refusal as your provider gave it (`recording_active`, `recording_disabled`, `cap_ceiling`, …) |
| `stop_recording` | — | `null`. Stop (Sew 0.9.0, §3a). Not in `sew-voice:default`. Rejects `not_in_call`, `no_recording`, `recording_unsupported`, or Sew's refusal (`recording_required` while the room requires its recording and two or more people are in it) |
| `set_recording_rights` | `{room, my_perms, dm?, recording_enabled?, recording_dm_enabled?, record_capped?, agent?}` | `null`. The person's rights in `room` (plugin 0.6.1, §3a, Which buttons): the `recording_controls` event and the `recording` event's `can_record` and `can_stop` follow them. Any time, with or without a call. Not in `sew-voice:default` (in `sew-voice:recording`). Rejects `invalid_args` (`my_perms` is not a decimal string) |
| `forget_server` | `{install_id}` (a decimal string) | `[{install_id, member_id, device_scope, account}]`, the device keys removed (§6). Not in `sew-voice:default` (§4). Rejects `in_call`, `invalid_args`, `device_store` |
| `known_device_keys` | — | `[{install_id, member_id, device_scope, account}]`, every key the device store holds (§6). Not in `sew-voice:default`. Rejects `device_store` |

Events go to every window as `sew-voice://<name>`:

| Event | Payload |
|---|---|
| `sew-voice://state` | `{state: "idle" \| "connecting" \| "connected"}`, `{state: "reconnecting", attempt}` or `{state: "ended", reason}`, with `target` and `room` (below) |
| `sew-voice://participants` | `[{member_id, is_self, muted, deafened, server_muted, server_deafened, speak_denied?}]` (`speak_denied` only when `true`, Sew 0.7.2) |
| `sew-voice://speaking` | `[{member_id, level}]`, `level` 0–1, at most 10 a second, your own member included |
| `sew-voice://devices` | the `devices` answer, on hot-plug, a default change or a `set_device` |
| `sew-voice://errors` | `{code, detail}` |
| `sew-voice://recording` | `{recording_id, auto, started_by?, my_consent, consent_prompt?, participants: [{member_id, consent, recorded}], recorder_present, trigger, required, can_record?, can_stop?}`, or `null` once no recording is active (Sew 0.7.3, §3a; `trigger` and `required` Sew 0.9.0; `can_record` and `can_stop` plugin 0.6.1, each only when `true`) |
| `sew-voice://recording_controls` | `{can_record, can_stop}`: whether Sew would take a Record or a Stop from this person now, sent when either changes (plugin 0.6.1, §3a, Which buttons) |

**States.**
- `idle` → `connecting` (on `join`) → `connected` ⇄ `reconnecting {attempt}` → `ended {reason}`.
- A join into a recorded room goes `connecting` → `consent_required {recording_id,
  notice_version}` until `consent` (then `connecting` → `connected`) or `leave` (§3a).
- After `ended`, the next `join` starts again at `connecting`.
- `connected` means you can hear and be heard: media is up and a call key is active.
- A moderator moving you to another channel keeps the state at `connected`.
- **Which call.** From `connecting` until its `ended`, the `state` event and the `state`
  command carry the call's `target` and `room`: your `join` arguments as given. Both are
  absent while `idle`. After a moderator move (Sew's `voice_server_updated`) `room` is the
  destination as Sew names it, the voice channel's id (the DM call's id when the destination
  is a call), and a `state` event says so, with the state unchanged. A re-join after the move
  asks your provider for that room. `target` doesn't change.
- `ended.reason` is Sew's leave reason as it arrives (Sew 0.7.2: `perms_lost`,
  `removed_from_space`, `timed_out`, `channel_archived`, `account_removed`,
  `removed_by_moderator`, `session_revoked`, `device_revoked`, `media_path_down`, and
  Sew 0.7.3's `recording_declined`, `recording_withdrawn`, `recording_consent_timeout` where
  declining means not joining, Sew 0.8.0's `app_unlinked`, …), or one of the plugin's: `left`, `signin_required`
  (also when Sew refuses a credential renewal `signed_out`: the login has no live session),
  `network`, `e2ee_refused`, `credential_lost` (another device took the call over),
  `credential_expired` (Sew refused a renewal `expired`, or no renewal got through before the
  credential ran out), `unavailable`, `provider_error`, `engine_stopped`, or your provider's
  refusal code. The `errors` event before the end carries Sew's renewal refusal reason as its
  code.
- Media and Sew's word can arrive in either order. When the media server drops you first, the
  engine waits up to 1.5 s for Sew's reason, then ends `kicked` (removed) or `room_closed`.
  The key group's removal waits the same way for the call socket's reason (the key group says
  `disconnected` for a reason it has no code for, such as `media_path_down`), then ends with
  the key group's.
  Against a Sew older than 0.7.1 a removal ends `kicked`; against one older than 0.7.2 a
  sign-out ends `credential_lost`.
- Treat reasons you don't know as "call ended".
- The exact payloads are in the plugin's `tests/native-voice-contract.json`; the fake engine
  (`fake::FakeCall::end`, `fake::FakeCall::set_speak_denied`) produces them with no call.

**SPEAK.** A member without SPEAK in the channel (a role or an overwrite, not a moderator) has
`speak_denied: true` and `muted: true`, for you and for others; `server_muted` stays a
moderator's mute. While you are denied, the engine captures and sends nothing and `mute`
cannot publish; when SPEAK comes back the microphone returns on its own (no re-join), and if
you muted yourself meanwhile you stay muted.

**Participants have no names.** The media token and the call keys carry none. Look each
`member_id` up in your own member list (K2 uses its rooms list).

**Error codes.**
- From the engine: `mic_denied`, `mic_unavailable`, `device_lost`, `turn_unreachable`,
  `relay_only_no_turns`, `e2ee_refused`, `ca_pin_mismatch`, and `key_storage_file` (a warning:
  no OS keystore, so the device key is in a file).
- From commands: `already_in_call`, `not_in_call`, `invalid_args`, `left` (a `join` that a
  `leave` cancelled), for `consent` / `withdraw_consent` `no_recording`,
  `recording_consent_required` and `consent_unsupported`, and for `start_recording` /
  `stop_recording` (Sew 0.9.0) `not_in_voice`, `no_recording` and `recording_unsupported`.
- From your provider: `signin_required`, `unavailable`, `provider_error`, and your refusals as
  you gave them.

```js
const { invoke } = window.__TAURI__.core;     // or @tauri-apps/api/core
const { listen } = window.__TAURI__.event;
const voice = (cmd, args) => invoke(`plugin:sew-voice|${cmd}`, args);

await listen("sew-voice://state", (e) => render(e.payload));
await listen("sew-voice://participants", (e) => renderRoster(e.payload));
const snap = await voice("state");             // a late window catches up here
try {
  const { call_id } = await voice("join", { target: "acme", room: channelId });
} catch ({ code, detail }) {
  if (code === "signin_required") showSignIn();
}
await voice("mute", { muted: true });
```

### Video

Fourteen more commands show and send video. They are not in `sew-voice:default`: give the call
window `sew-voice:video` (§4). An engine without video answers `video_caps` with
everything off, and the other commands reject `video_unsupported`; the voice payloads never
change (no `video`, `local_video` or `policy` in `state` while there is
none).

| Command | Arguments | Answer |
|---|---|---|
| `video_caps` | — | `{camera, screen: {supported, system_picker, list, windows}, publish_codecs: ["vp8", "h264"], decode_codecs, receive, self_view, live_switch, backend}`. `self_view`: the `local:camera` and `local:screen` sinks are fed during a call (the published camera's and screen's own layers) as well as by `preview`; `false` or missing: only `preview` feeds `local:camera`. `live_switch`: `set_video_device` switches a camera that is being published now; `false` or missing: the choice applies at the next `camera {on}`. `decode_codecs` are the engine's own decoders; the webview's are its WebCodecs probe. `receive`: the engine receives remote cameras and screens for the sinks (the plugin's `video-core`); missing in an older plugin's answer. `publish_backup` and `codec_preference`: see "Codec choice and the VP8 backup" below (missing in an older plugin's answer) |
| `video_devices` | — | `{cameras: [{id, name, facing, default}], camera, permission: {camera, screen}}`. `facing`: `front`, `back`, `external`, `unknown`. `camera`: the chosen one (`null`: the OS default). Each permission is `"granted"`, `"denied"` (the OS's restricted state too), `"prompt"` (the OS will ask) or `"unsupported"` (no such capture on this OS or in this build: screen capture on Linux without the portal or libpipewire, a build without video) |
| `set_video_device` | `{camera}`: an id or `"default"` | `null`. Switches a live camera without a republish (when `video_caps.live_switch`); kept for the next call or preview (a running preview keeps its camera until `preview` is called again). Rejects `camera_not_found` (`detail.camera`) |
| `camera` | `{on, device?}` | `null` once publishing (or off). Rejects `video_unsupported` (no camera here: checked first, before the permission), `not_in_call`, `video_not_allowed`, `video_disabled`, `camera_permission_denied` and `camera_permission_not_determined` (the plugin never prompts: below), `camera_denied` (the OS refused the device itself), `camera_busy`, `camera_not_found`, `no_frames` (the camera opened but gave no frame within 2.5 s: closed, nothing published), `video_slots_full`, `media_busy` (`detail.retry_after`), `media_capacity`, `codec_unavailable`. Without `device` and with none chosen, it prefers the camera that last gave frames while it is listed and not suspended, else the OS default (a suspended default is passed over). The track is published from the camera's first frame |
| `screen_sources` | — | `[{id, kind: "display" \| "window", title, app?, width, height, thumbnail_png?}]` (`thumbnail_png`: an 8-bit RGBA PNG at most 320 px wide, aspect kept, base64 with the standard alphabet and no `data:` prefix; absent when the OS gives no preview). `[]` where only the OS picker exists. Rejects `screen_unsupported`, `screen_denied` |
| `screen` | `{on, source?}` | `null` once publishing (or off). Without `source`, the OS picker (on Windows its parent is the calling window). The share runs at most 1080p15 (lower when `policy.screen` says so) with the screen content hint, under the `self_stream` lease. Rejects as `camera`, plus `screen_denied`, `screen_slots_full`, `picker_cancelled`, `screen_unsupported` (no screen capture here: Linux without the xdg portal or libpipewire, phones; or no OS picker and no `source`), `screen_not_found` (the listed `source` is gone) |
| `preview` | `{on, device?}` | `{sink_track: "local:camera"}`. The camera before or outside a call (a pre-join check): one encoder at up to 640x360 (the camera's aspect kept; VP8, kessel-video's pure-Rust encoder), nothing sent, no lease. The camera opens only on this command, never on its own between calls, and only with the OS permission granted (the plugin never prompts); `{on: false}` (or a new `preview`) closes it at once (the light goes off). One opener holds the camera: `camera {on}` takes it over from the preview (the `local:camera` sinks go on from the call's own keyframe), and `preview {on}` while the camera is published opens nothing (the call's self-view already feeds `local:camera`). Rejects `camera_permission_denied` and `camera_permission_not_determined` as `camera`, `camera_denied`, `camera_busy`, `camera_not_found`, `codec_unavailable`, `video_unsupported` (no camera here) |
| `video_sink` | `{track, channel, width, height, visible, tile: "large" \| "grid", decode: ["vp8", "h264"]}` | `{sink_id, codec, mode: "encoded" \| "decoded"}`. `track`: a `video` entry's `track`, `"local:camera"` or `"local:screen"` (this device's own camera or screen: the preview's frames outside a call; in a call the published camera's `h` layer and the screen's `q` layer, the same encoder's plaintext, nothing sent twice). `channel`: a Tauri `Channel` (below). `width`/`height`: the element in device pixels. `decode`: the webview's WebCodecs probe (`[]` without WebCodecs). When the webview can't decode the track's codec, the sink is `decoded` (the engine decodes it; below). Rejects `not_in_call` and `unknown_track` (a remote track), `decoded_unsupported` (`detail.codec`) when the engine can't decode the codec either, and `decoded_tiles_full` (`detail.max`, 4) when this device already decodes its most tiles |
| `video_sink_update` | `{sink_id, width?, height?, visible?, tile?, stats?: {decoded, dropped, freezes, freeze_ms}}` | `null`. `stats` are cumulative since the sink opened (every 1–10 s is fine). Rejects `unknown_sink` |
| `video_sink_close` | `{sink_id}` | `null` (also for a sink already closed). With no sink left, the track is turned off at the media server again |
| `request_keyframe` | `{sink_id}` | `null`. The webview's decoder failed or fell behind: the sink sends nothing until the next keyframe, and the sender is asked for one (rate-limited). Rejects `unknown_sink` |
| `set_video_codec_preference` | `{preference}`: `"automatic"` (the default: H.264 where this device's OS has a usable hardware encoder, found at run time, else VP8), `"prefer_h264"` (H.264 only where that check passes; otherwise VP8, with the reason), `"prefer_vp8"` | `video_caps.codec_preference`. A per-device user setting for every camera or screen turned on after it (the live call's too); `"prefer_vp8"` also stops this device receiving H.264 from its next join. Its own permission, `sew-voice:allow-set-video-codec-preference`. The plugin keeps it until the app quits: store it with the app's settings and set it again at start. Rejects `video_unsupported` without video |
| `camera_permission` | — | `"granted"`, `"denied"`, `"prompt"` or `"unsupported"` (a build or an engine without a video backend). Never prompts |
| `request_camera_permission` | — | the same, after the OS prompt when it was `"prompt"` (iOS, macOS). On Android ask for CAMERA with Tauri's permission API first (it needs an Activity callback); this then reports the state without a prompt |

**The camera permission is the app's to ask for.** The plugin never shows the OS prompt on its
own: `camera {on}` and `preview {on}` need the permission `granted` and otherwise reject at once
with `camera_permission_not_determined` (never asked) or `camera_permission_denied` (denied or
restricted: the person changes it in the OS settings), opening nothing. The app's camera button
calls `request_camera_permission` first (on Android after its own CAMERA request), then
`camera {on}`. An engine without a camera permission (`"unsupported"`) decides itself.

**Fixed rules** (K2's room video UI is built on them):
1. Command arguments and answers are snake_case (`sink_id`, `install_id`), as for every other
   command.
2. `sink_id` is a number (u32), the same value as the frame header's. In `mode: "decoded"`,
   `codec` is `null`.
3. Permission values are exactly `"granted"`, `"denied"`, `"prompt"` or `"unsupported"`.
4. `thumbnail_png` is base64 (standard alphabet, no `data:` prefix, at most 320 px wide);
   `kind` is `"display"` or `"window"`.
5. `policy` is sew-proto's `MediaPolicy` serialised as it is (snake_case: `tier`,
   `video_enabled`, `e2ee`, `audio_codecs`, `codecs`, `backup_codec`, `vp9?`, `camera` and
   `screen` hints, `dynacast`, `adaptive_stream`, `camera_allowed`).
6. There is no `pick_screen` command: `screen {on: true}` without `source` opens the OS picker,
   and the plugin passes the Windows window handle itself.
7. `video_caps` always exists, even in a plugin or engine without video, which answers
   `{camera: false, screen: {supported: false, system_picker: false, list: false, windows: false}, publish_codecs: [], decode_codecs: [], receive: false, self_view: false, live_switch: false, backend: "null"}` (no `codec_preference`).
   An older plugin lacks the command: treat a rejection as "no video". A field an older
   plugin doesn't send (`receive`, `self_view`, `live_switch`) reads as `false`.
8. Every (re)start of frames begins at a keyframe with the discontinuity flag: a sink opening,
   `visible` going false→true, a remote unmute, a layer change, `request_keyframe`, and recovery
   after loss. The engine asks the sender for the keyframe itself; an extra `request_keyframe`
   is harmless (rate-limited).
9. `stats` in `video_sink_update` are cumulative since the sink opened.

The `state` command adds `video`, `local_video` and `policy` (as the events below) while they
say something, so a window opened mid-call can draw everything.

| Event | Payload |
|---|---|
| `sew-voice://video` | the whole list on any change: `[{member_id, is_self, source: "camera" \| "screen", track, codec, width, height, muted, paused: null \| "congestion" \| "background" \| "policy", unsupported?}]` (`unsupported` only when `true`) |
| `sew-voice://local_video` | `{camera: "off" \| "asking" \| "on" \| "paused", screen: same, reason?}`. `reason`: a reject code, `hidden` (a shared window is minimised: `paused`, the share stays), `background`, `thermal`, `user_stopped` (stopped from the OS's own UI), `source_closed` (the shared window closed or the display went away), `suspended` (`paused`: the camera says it is suspended, as a MacBook's built-in camera with the lid closed; `on` again when it comes back; no error), `camera_lost` (`off`: the camera went away; the track is unpublished, the lease released and the call goes on), `camera_denied` / `screen_denied` (the OS permission was revoked while sending) or `video_disabled` |
| `sew-voice://video_devices` | the `video_devices` answer, on hot-plug or a `set_video_device` |
| `sew-voice://policy` | the room's media policy (Sew's `MediaPolicy`, as the join answer has it), when Sew changes it |
| `sew-voice://errors` | as for voice. Video adds `video_stalled`, `encoder_lost` and `camera_lost` |

Any video command may also reject `capture_failed` (`detail.error`: the video backend's own
code, such as `os` or `invalid`) for a camera or screen failure none of the codes above says.

**Video codes**, one table for every video command and the `local_video` reasons (a reject's
`code`; `detail` where noted):

| Code | Where | Meaning |
|---|---|---|
| `video_unsupported` | any video command | no video backend in this build or engine (no camera here for `preview`) |
| `video_not_allowed` | `camera`, `screen` | the room's policy has video off, or this session may not send that source |
| `video_disabled` | `camera`, `screen`, `local_video.reason` | Sew's `[voice.video] enabled = false` |
| `video_slots_full` / `screen_slots_full` | `camera` / `screen` | the room's camera or screen-share cap (the lease) |
| `media_busy` | `camera`, `screen` | the media budget refuses new video now; `detail.retry_after` seconds |
| `media_capacity` | `camera`, `screen` | the install's video publisher cap |
| `no_lease`, `e2ee_downgraded`, `already_published`, `publish_failed`, `lease_failed` | `camera`, `screen` | the engine's publish refusals (no lease taken; the media server dropped GCM; already sending; the media server refused or timed out; another lease failure) |
| `codec_unavailable` | `camera`, `screen`, `preview` | no encoder for a codec the policy allows (or the preview's VP8) |
| `camera_denied` / `screen_denied` | `camera`, `preview` / `screen`, `screen_sources`, `local_video.reason` | the OS permission is denied or restricted, or was revoked while sending |
| `camera_busy` | `camera`, `preview`, `local_video.reason` | another app holds the camera (as a reason: it took it, `paused`) |
| `camera_not_found` | `camera`, `preview`, `set_video_device` | no such camera (`detail.camera` for `set_video_device`) |
| `camera_lost` | `local_video.reason`, `errors` | the camera went away while sending |
| `no_frames` | `camera` | the camera opened but gave no frame within 2.5 s (closed, the lease released, nothing published) |
| `screen_unsupported` | `screen`, `screen_sources` | no screen capture here, or no OS picker and no `source` |
| `picker_cancelled` | `screen` | the person cancelled the OS picker |
| `screen_not_found` | `screen` | the listed `source` is gone |
| `encoder_lost` | `preview` (its channel), `errors` | an encoder failed and could not be reopened |
| `capture_failed` | any capture | a camera or screen failure none of the codes says; `detail.error` is the video backend's own code (`os`, `invalid`, …) |
| `decoded_unsupported`, `decoded_tiles_full`, `unknown_track`, `unknown_sink` | `video_sink`, `video_sink_update`, `request_keyframe` | the sinks' refusals (above) |
| `hidden`, `background`, `thermal`, `suspended`, `paused` | `local_video.reason` (`paused`) | a shared window is minimised or hidden; the app went to the background; thermal pressure; the camera is suspended (a laptop lid closed); another pause. The track stays; back, every layer restarts on a keyframe |
| `user_stopped`, `source_closed`, `unpublished` | `local_video.reason` (`off`) | stopped from the OS's own UI; the shared window or display went away; turned off by `camera` / `screen` |

**Codec choice and the VP8 backup.** What a device publishes follows the media policy and what
this device can do, checked at run time (never assumed from the OS):

- **H.264 with a VP8 backup** when the policy's `backup_codec` is `true` (the server's
  `[voice.video] backup_codec`, on by default, with both `vp8` and `h264` listed in either
  order), dynacast is on, and this device has a usable OS H.264 encoder: a hardware encoder that
  is listed, opens hardware-only, and reports hardware once open. Today that is macOS and iOS
  (VideoToolbox); Android and Windows publish VP8 only for now; Linux always publishes VP8 only.
  The backup's m-line is negotiated with the publish, and its VP8 encoders run only while the
  media server says some subscriber needs VP8 (a Linux client, a browser without H.264). It
  never stops for heat while a VP8-only subscriber depends on it; on phones it is capped at
  360p.
- **One codec otherwise**, as before: VP8 when it is listed and there is an encoder, else H.264
  when it is listed and usable, else `codec_unavailable`. `backup_codec: false` is exactly this.
- **Browsers** always publish one codec, the first of `codecs` they can send, and never a
  backup. With `h264` listed first, a subscriber that can't decode H.264 (the Linux native
  client) can't see them; with `codecs = ["h264"]` it can't see anyone (Sew's media engine
  operator guide, "Linux clients and H.264").

`video_caps.publish_codecs` lists what this device can publish, in the order it would use them
(`["h264", "vp8"]` on the H.264 path), and `video_caps.publish_backup` is `true` when it would
add the VP8 backup under a policy that allows one.

**The device's codec setting.** `VoiceControl::set_video_codec_preference(pref)` takes
`Automatic` (the default: H.264 with the backup where detection passes, else VP8),
`PreferH264` (the same: it takes effect only where detection passes, otherwise VP8) or
`PreferVp8` (always honoured: VP8 only, no backup needed; the device also stops offering to
receive H.264 from its next join, so every sender reaches it in VP8). The webview's command is
`set_video_codec_preference {preference}` (its own permission,
`sew-voice:allow-set-video-codec-preference`, in `sew-voice:video`); it answers the readback
below. Keep it per device, in the app's device settings ("Video codec"); the plugin keeps it
until the app quits, so set it again at start. `video_caps.codec_preference` reads it back as `{setting, effective, reason?}`: `setting`
(`"automatic"`, `"prefer_h264"`, `"prefer_vp8"`), `effective` the codec it gives on this device
(`"h264"`, with the backup, or `"vp8"`) and, only when Prefer H.264 can't apply, `reason`:
`"not_detected"` (no usable hardware H.264 encoder found at run time), `"platform"` (no run-time
check on Linux, Android or Windows yet, or an iOS before 17.4) or `"policy"` (the live call's
policy doesn't allow H.264). An engine without video publish leaves `codec_preference` out.
`effective` is what the backend's capabilities say; the encoder's hardware-only open when the
camera turns on is the last word. A change applies from the next camera or screen publish.
Every H.264 sender carries the VP8 backup, whatever the setting.

**What a device receives.** A subscriber gets the codec its answer can decode: H.264 when it
can, else the sender's VP8 backup. So the `codec` of a `video` entry is the codec **this
device** receives, and two devices can see one track in different codecs; it can change during
a call (the entry is sent again). A native offers H.264 only when the engine's video backend
decodes it or the webview said it does: pass the webview's WebCodecs probe as the join's
optional `decode` field (`@sew/voice` does this); without it only the engine's own decoders
count. Linux offers VP8 only, whatever the webview says. `unsupported` is set only when this
device can decode none of the track's codecs.

**The frame API.** A host that drives encoders itself (`Call::publish_video` without the
bridge) asks for the backup with `VideoPublish::with_backup`. It then gets `VideoControl`
`BackupStart(layers)` when the backup is needed (open VP8 encoders for those layers, first
frame a keyframe), `BackupRates`, `BackupKeyframe { rid }` and `BackupStop` (stop and close
them). A host that never calls `with_backup` never sees these.

**Frames.** Each `video_sink` gets its own Tauri raw `Channel` (`new Channel()` from
`@tauri-apps/api/core`, or `window.__TAURI__.core.Channel`); its `onmessage` receives one
`ArrayBuffer` per message: a 32-byte little-endian header, then the payload.

| Offset | Type | Field |
|---|---|---|
| 0 | u8 | version, `1` |
| 1 | u8 | kind: `1` encoded, `2` I420, `3` RGBA, `4` reset (no payload) |
| 2 | u8 | codec: `1` VP8, `2` H.264 (`0` for decoded) |
| 3 | u8 | flags: bit 0 keyframe, bit 1 discontinuity (reset the decoder first), bit 2 self-view, bit 3 front camera, bit 4 back camera |
| 4 | u32 | sink_id |
| 8 | u64 | timestamp_us (arrival; capture for self-view) |
| 16 | u16 | width (`0` = unchanged) |
| 18 | u16 | height (`0` = unchanged) |
| 20 | u32 | the sink's message sequence |
| 24 | u32 | payload length |
| 28 | u32 | reserved, 0 |

**Mirroring a camera's self-view.** On a `local:camera` frame, bit 3 says the camera that captured
that frame faces front (mirror it) and bit 4 that it faces back (don't); neither: the camera's
facing is unknown or external (most desktop cameras: the host's own default). The bits are per frame: on a `set_video_device` flip the old camera's frames still carry
its facing, and the switch lands on the new camera's first frame (a keyframe), so a host that
mirrors by these bits, not when the flip is pressed, never shows a frame the wrong way round. A
remote track's frames carry neither bit.

An encoded payload is one VP8 frame, or one H.264 access unit in Annex B with 4-byte start codes:
configure WebCodecs' `VideoDecoder` **without** `description` (H.264 as Annex B), and again when a
keyframe's SPS names another profile or level. The frames are already decrypted and checked in
Rust: the keys never reach the webview, and a frame that failed decryption never reaches a sink.
It is the browser path's trust: there, too, the page's own E2EE worker hands decrypted frames to
the browser's decoder.

The plugin gates every sink before anything crosses the bridge:
- after the sink opens, becomes visible again, asks for a keyframe, or its track breaks (a gap
  or a decryption failure), it sends nothing until the next keyframe, which carries the
  discontinuity flag; a broken track first sends a `reset` message;
- a hidden sink (`visible: false`) gets nothing, and a track with no visible sink is turned off at
  the media server (the plugin hands the engine the largest visible sink's size, and 30 fps for a
  `large` tile, 15 for a `grid` one);
- the sinks of a closed webview are closed on the first failed send.

**Self-view.** `local:camera` receives this device's own camera as it is sent (the encoded output
of the layer at or above 360p, plaintext: no second encoder) with the self-view flag; mirror it
with CSS, not in pixels. `local:screen` receives the low layer of your screen share. Outside a
call, `preview` feeds `local:camera`.

**The decoded fallback** (`mode: "decoded"`). A sink whose `decode` probe lacks the track's codec
(a WebKitGTK without WebCodecs, a webview without H.264) gets frames the engine decoded in Rust,
with one of its own decoders (`video_caps.decode_codecs`: the decoders of the engine's video
backend, the same one that publishes the camera; with the plugin's `video`, VP8 in pure Rust
everywhere and H.264 through VideoToolbox on macOS; a host's own backend through
`CallClientEngine::with_video_backend`). The same gating decides which frames
reach that decoder. Its outputs are scaled to fit the sink (never up), at most 640x360 and
15 frames a second, and come as kind `2`, I420 (the Y plane, then U, then V, tightly packed,
chroma rounded up; draw it as a WebCodecs `VideoFrame` with `format: "I420"`), or, when `decode`
is empty (no WebCodecs at all), kind `3`, RGBA (tightly packed; `putImageData` on a canvas). Each
decoded message carries its size in the header, codec `0`, the discontinuity flag on the first
frame after a reset, and the self-view flag for `local:*`. A decoder error resets it, sends the
sink a `reset` message and asks the sender for a keyframe. At most 4 sinks are decoded at once:
a fifth is refused with `decoded_tiles_full`, and its tile shows the participant's avatar and
"video paused (this device)" (open it again once another decoded sink closes). A decoded sink's
stats (the beacon's) are the engine decoder's own counts, not the webview's.

`@sew/voice` does all of this for you (its native renderer: the probe, one `VideoDecoder` per
sink, a `MediaStreamTrackGenerator` where the webview has one, else a canvas, decoded I420 and
RGBA frames, and a `VideoHandle` whose `attach(el)` opens the sink and `detach(el)` closes it 2 s
after the last element; its `paused` is `"device"` after `decoded_tiles_full`).

## 3a. Recording (Sew 0.7.3)

A Sew call can be recorded (Sew Notes: a recorder in the call, a transcript posted to the
channel). Each person decides for themselves. **Nobody is recorded without saying yes, and
not answering is never a yes**: this device's call key reaches the recorder only after Sew says
the person `granted`, and the engine stops (and rekeys) the moment they withdraw. The engine
takes that from Sew alone (the join answer, the call socket's own `recording_updated`, Sew's
answer to a consent); it never treats a call as consented by itself.

What your app does:

- **The badge.** Show it while `sew-voice://recording` is non-null. `participants` says who is
  recorded now (`recorded`), `recorder_present` that the recorder is in the call. The recorder
  is never in `sew-voice://participants` (it is not a member).
- **The modal.** While `my_consent` is `pending`, `consent_prompt` is Sew's notice (its slots:
  recorder name, starter, room, keep days, who reads the transcript, the decline mode, who is
  recorded now; build your sentences from them). Answer with
  `consent {granted, notice_version: consent_prompt.notice_version}`.
- **Joining a recorded room.** `join` answers with an empty `call_id` and the state is
  `consent_required`: no microphone is opened, no media, no keys, and Sew has no voice entry for
  the person. Show the modal; `consent` joins with the answer (a decline joins unrecorded, or
  ends the call when declining means not joining: `recording_consent_required_to_join`); `leave`
  gives up. The held state waits as long as you do (a lock-screen answer may take a while).
- **Mid-call.** A recording started while the person is in the call makes them `pending`: they
  stay unrecorded until they answer. `consent` sends the answer through your provider.
- **"Stop recording me".** `withdraw_consent` while `granted`.
- **A changed notice.** Who is recorded is part of the notice, so it changes while the modal is
  up. An answer to an old one rejects `recording_consent_required` with the new notice in
  `detail.consent_prompt`; the `recording` event shows it too. Show it and ask again.
- **Re-joins.** After a long network loss the engine re-joins with the person's last answer.
  If the notice changed meanwhile the call is held at `consent_required` again.
- **Tones.** Play your start and stop tones on the `recording` event going non-null and null.

Your provider's part is in §2: forward `recording_notice`, map the `409` to `ConsentRequired`,
and send `consent` with the person's own session.

### Record and Stop (Sew 0.9.0)

`start_recording` and `stop_recording` (permission set `sew-voice:recording`, §4) are the
person pressing Record and Stop. Your provider's `recording` sends each with the person's own
session (§2, the recording route), and Sew decides who may: a refusal comes back as the
command's error with Sew's reason. In Rust they are `Call::start_recording` and
`Call::stop_recording` (sew-call-client) and `VoiceControl::start_recording` and
`stop_recording` (the plugin).

- **Record is the starter's own consent, and nobody else's.** Show the modal as a confirmation
  first (its notice is the same one the others get). Everyone else in the room is `pending`
  until they answer, as for any recording started mid-call.
- **Show the buttons from `recording_controls` (plugin 0.6.1).** See Which buttons, below.
  Sew still decides, so show a refusal's code as a message: `recording_active` (someone
  started one first), `recording_disabled`, `dm_recording_disabled`, `recording_capacity`,
  `not_in_voice` (the join is still held for a notice), `cap_ceiling` (the App's pass may not
  record), or a missing permission.
- **Rooms that require recording.** An admin can require every call with two or more people
  in it to be recorded. Sew then starts the recording itself when the second person joins
  (`trigger: "required"`), and refuses Stop with `recording_required` while two or more are in
  the room. "Stop recording me" (`withdraw_consent`) always works, and declining at join still
  joins unrecorded unless declining means not joining. A person who joins such a room alone
  gets the notice at join (held, as for an auto-record room); their answer is kept for the
  recording that starts when someone joins.
- **The notice's new slots.** `consent_prompt` may carry `required: true` ("this room records
  every call with two or more people"), `app_readers` (the display names of Apps whose
  server can read the transcript, such as the App that runs the room) and, where Sew sends it,
  `agent_readers` (the display names of agents that can read it). Each is written only when
  set, so an older notice reads as before. Add them to your sentences when present;
  `ConsentPrompt::text_en` does.
- **What started it.** The `recording` event carries `trigger` (`"manual"`, `"auto"` or
  `"required"`; `"manual"` or `"auto"` against an older Sew) and `required` (the room requires
  its recording, so hide Stop while two or more are in the room); in Rust they are
  sew-call-client's `CallRecording::trigger` and `required`.

Without the recording route your provider keeps `recording`'s default and both commands
reject `recording_unsupported`.

### Which buttons (plugin 0.6.1)

The plugin can't read the person's permissions itself (the call socket carries none), so your
app tells it what it knows of the room with `set_recording_rights` (in Rust,
`VoiceControl::set_recording_rights` with a `RecordingRights`):

```js
await voice("set_recording_rights", {
  room: channelId,                      // as join's room (the state event's after a move)
  my_perms: channel.my_perms,           // the voice channel's or DM's, as Sew reports them
  dm: isDm,
  recording_enabled: policy.recording_enabled,        // the policy digest's
  recording_dm_enabled: policy.recording_dm_enabled,
  record_capped: false,                 // the pass lacks voice:record where its App requires it
  agent: false,                         // the person is an agent
});
```

Send it on (or before) each join and again whenever one of them changes: Sew's permission
updates for the channel or the person's roles, a policy change, a move (the `state` event's new
`room`). It applies while the call is in `room` and is forgotten when the call ends.

The plugin then sends `sew-voice://recording_controls` `{can_record, can_stop}` whenever
either changes, puts the same two in the `recording` event (`can_stop` is the one that matters
there: `can_record` is always `false` while a recording is active) and in the `state`
command's `recording_controls`. Show Record while `can_record`, Stop while `can_stop`, and
"Stop recording me" while `my_consent` is `granted`, whatever `can_stop` says. They follow
Sew's own checks:

| | `true` when |
|---|---|
| both | the call is `connected` or `reconnecting` in `room`, and the person is not an `agent` and not `record_capped` |
| `can_record` | no recording is active, `recording_enabled`, in a DM also `recording_dm_enabled`, and `my_perms` holds RECORD_CALLS |
| `can_stop` | a recording is active and, in a DM, always (anyone in the call); elsewhere the person started it, or `my_perms` holds RECORD_CALLS or MANAGE_CHANNELS. Never for a recording Sew started because the room requires one (`trigger: "required"`) while two or more people are in the call |

They change as the call does: the rights, the state, the room, the participants (a required
room's headcount) and the recording. Both are `false` with no call, while a join is held for a
notice, and until you send the rights. Sew still decides, and checks things the plugin can't
see (the recorder switched off on the server, an App's Space without uploads, capacity), so
show its refusal when one comes.

## 3b. Phones: the native listener and control (plugin 0.3.0)

A CallKit (iOS) or Telecom (Android) shim needs the call when no webview is awake. The plugin
gives it two things, both in Rust; the objc2 or JNI bridge is the host's.

- **`VoiceListener`** (`Builder::listener`, at most one): `on_state`, `on_participants`,
  `on_self_mute` (a `SelfMute`: `muted`, which CallKit shows; `deafened`; `self_muted`, your
  own choice; `server_muted`; `speak_denied`), `on_recording`, `on_error`, `on_devices`,
  `on_incoming` (0.4.1: a DM call rings, then `None`; §3c) and `video_changed` (the call gained or lost video: the speaker route, below).
  Every method has an empty default. The calls come from the plugin's task, in order, one at
  a time, under the plugin's lock, right after the matching `sew-voice://` event and with the
  same payload. So don't block in them and don't call back into the plugin from them: copy
  and hop to your own queue. There is no `on_speaking`.
- **`VoiceControl`** (Tauri state: `app.state::<VoiceControl>().inner().clone()`): async
  `join`, `leave` (safe to repeat), `set_muted`, `set_deafened` (both `not_in_call` without a
  call), `consent`, `withdraw_consent`, `start_recording` and `stop_recording` (Sew 0.9.0),
  `set_recording_rights` (plugin 0.6.1);
  sync `snapshot` (read it on wake instead of replaying events); `network_changed` (the live call resumes now); `session_activated`,
  `session_deactivated`, `suspend`, `resume` and `route_changed`, which go to kessel-audio
  (below); for a sign-out, async `forget_server(install_id)` and `known_device_keys()`
  (§6); and for rings, `ring`, `ring_ended`, `incoming`, and `details` (0.4.1, §3c); for video, `join_with` (a join with `JoinOptions`: the camera on), `set_camera`, `app_scene`, `device_orientation`, `camera_permission`, `request_camera_permission`, `has_video` and `open_native_sink` (Video on phones, below). The plugin never configures `AVAudioSession` or the Android audio mode.
- **Consent on a lock screen.** A held join is `on_state(consent_required)` then
  `on_recording` with the notice; a recording that starts mid-call is `on_recording` with
  `my_consent: pending` while you stay connected. `ConsentPrompt::title_en(ask)`,
  `text_en(ask, names)` and `choices_en(ask)` (sew-proto; `ask` is `ConsentAsk::Join` or
  `InCall`) give the Sew App kit's words and answers. Echo `notice_version` in `consent`.
- **On iOS and Android** the plugin lists no devices between calls and has no keyring
  backend (`keyring-store` doesn't build there). It has no Swift or Kotlin of its own, so it
  registers on mobile as on desktop.

### The phone audio backend (0.4.1)

On iOS and Android (sew-call-client's `mobile` feature, with the plugin's default `audio`)
the call's audio is kessel-audio's OS voice processing, not cpal: iOS's VoiceProcessingIO
audio unit, Android's AAudio with the voice-communication preset (API 28 or later). The OS's
echo cancellation and noise suppression are on, and kessel-audio's own processing runs only
what the OS doesn't do. Desktop keeps cpal. `sew_call_client::AUDIO_BACKEND` says which one a
build has (`"os-voice"` or `"system"`).

The OS's audio session is your app's, never the plugin's. Report it to `VoiceControl`:

| Your app sees | Call | kessel-audio |
|---|---|---|
| CallKit `provider(_:didActivate:)` (Android: the call's audio is yours: the Telecom connection is active, or you set the call audio mode) | `session_activated()` | opens the microphone and the speaker |
| `provider(_:didDeactivate:)` (Android: the call audio ends) | `session_deactivated()` | stops I/O; the call goes on |
| An interruption begins (a cellular call, hold, lost audio focus) | `suspend()` | pauses I/O; the far end hears a pause, not loss |
| The interruption ends | `resume()` | restarts I/O |
| `AVAudioSession.routeChangeNotification` (speaker, earpiece, Bluetooth) | `route_changed()` | re-reads the device; reopens it if the rate changed (always on Android) |

- **iOS: start after the session activates.** The engine starts kessel-audio deferred: the
  microphone and the speaker open only after `session_activated()`. Until then the call
  connects and gets its keys, and received audio is decoded but not played. CallKit (or your
  app) owns `AVAudioSession`: set its category (`.playAndRecord`, mode `.voiceChat`) and let
  CallKit activate it. Without `session_activated()` an iOS call stays silent both ways.
- **Android** starts at once; `session_deactivated()` and `session_activated()` close and
  reopen I/O. The call audio mode (`MODE_IN_COMMUNICATION`) is still the app's.
- **Before a call.** CallKit may activate the session before the join has a call: the plugin
  keeps the last `session_activated` / `session_deactivated` for the next call, and a
  `suspend` until that call's audio ends. A repeated event (a second `session_activated`) is
  passed on once. `route_changed` only reaches a live call.
- **Desktop** ignores these calls.

`examples/tauri-voice-mobile` is a minimal phone app on all of this: the microphone prompt,
the Keystore device key, the network watch and the call audio mode (§7).

The exact calls for each scenario (held join, mid-call pending, granted, declined, withdrawn,
`on_self_mute` while server-muted and while SPEAK is denied, a stop) are in the plugin's
`tests/sew-native-voice-listener.json`.

### Video on phones

Build the plugin with `video` and `mobile` for iOS and Android (`mobile` is sew-call-client's:
the pure-Rust DTLS/SRTP provider and the OS voice processing):

```toml
[target.'cfg(any(target_os = "android", target_os = "ios"))'.dependencies]
tauri-plugin-sew-voice = { version = "…", features = ["video", "mobile"] }
```

The screen share's features go on this **plugin** line too, not on sew-call-client's:
`mobile` turns on the plugin's `ios-screen-ring` (the iPhone's broadcast-extension ring, §3b);
a build without `mobile` names `ios-screen-ring` itself (`features = ["video",
"ios-screen-ring"]`). Android's screen share needs no feature beyond `video`.

**Build the Rust library in release.** Video runs through pure-Rust codecs and packetisers
(kessel-video's VP8, the engine, str0m), which in a debug build are far too slow for a live
camera or screen: frames drop and the far side freezes. Build the app's Rust static library
(iOS) or shared library (Android) with `--release`, as `tauri ios build` / `tauri android
build` do. For a debug app, optimise those crates alone in the app's workspace root:

```toml
[profile.dev.package.vp8-decoder]
opt-level = 3
[profile.dev.package.vp8-encoder]
opt-level = 3
[profile.dev.package.kessel-video]
opt-level = 3
[profile.dev.package.sew-call-client]
opt-level = 3
[profile.dev.package.str0m]
opt-level = 3
```

The same holds on the desktop for `video-core`.

The engine builds kessel-video's backend for the OS on first use (`kessel_video::system`):
AVFoundation and VideoToolbox on iOS, Camera2 and MediaCodec through the NDK on Android
(API 24). The app declares the camera: `NSCameraUsageDescription` in `Info.plist` (without it
the permission reads `denied` instead of iOS ending the app); on Android
`<uses-permission android:name="android.permission.CAMERA" />` and
`<uses-feature android:name="android.hardware.camera" android:required="false" />`.

- **Android context.** Give the plugin the app's JNI handles so kessel-video can read the
  CAMERA permission:
  ```rust
  let ctx = ndk_context::android_context();
  let video = unsafe { AndroidContext::new(ctx.vm(), ctx.context()) };
  Builder::new(provider).android_video_context(video)
  ```
  (`AndroidContext` is `sew_call_client::kessel_video::AndroidContext`.) Without it the
  permission reads `prompt` until a camera opens.
- **Permission.** `camera_permission` never prompts; `request_camera_permission` prompts on iOS.
  On Android ask for CAMERA yourself (Tauri's permission API), then call it to read the answer.
  The same two are on `VoiceControl` for native code. Nothing else prompts: `camera {on}`,
  `VoiceControl::set_camera`, a join's camera and `preview` answer
  `camera_permission_not_determined` or `camera_permission_denied` without the permission
  (§3, Video). On Android, without `android_video_context` the permission reads `prompt`, so
  the camera can't turn on: give the plugin the context.
- **Camera from CallKit or Telecom.** `VoiceControl::join_with(args, JoinOptions::with_camera())`
  (optionally `.camera_device(id)`) is a video call's start or answer (`CXStartCallAction` with
  `isVideo`, a Telecom video state): the call joins as `join` does, and the camera turns on as
  soon as the call is connected and the app is in front. Answered on the lock screen, it waits
  for the app's `app_scene(Foreground)`: the camera never starts from the lock screen. Its
  refusals (no permission, `video_slots_full`, `video_not_allowed`, `media_busy`, …) come back
  as `local_video {camera: "off", reason: <code>}`, and the call goes on without the camera.
  `VoiceControl::set_camera(on, device)` is the system UI's video button (the webview's
  `camera`; iOS brings the app forward first).
- **The camera in the background.** The camera **pauses** rather than unpublishing: in the
  background (the combined signal below) its capture and encoders stop and its track is muted at
  the media server, so the lease and the tile stay, and `local_video` says
  `{camera: "paused", reason: "background"}` (the UI shows "camera paused";
  `video_changed` stays `true`). Back in front it unmutes and resumes, each encoder starting at
  a keyframe, and `local_video` says `on`. After 5 minutes in the background the camera turns
  off (unpublished, the lease released) with `{camera: "off", reason: "background"}`, and stays
  off when the app returns. A camera turned on while in the background pauses at once. There is
  no multitasking camera in v1: the same rule on both phones. A screen share is not paused.
- **The audio session.** Starting the camera never touches the call's audio: the capture
  session has no microphone input and leaves the app's audio session alone (iOS
  `automaticallyConfiguresApplicationAudioSession = false`; CallKit owns the session and
  kessel-audio the microphone). The route stays what it was; switching to the speaker for video
  is the app's (`video_changed`, below).
- **Background.** Call `VoiceControl::app_scene(AppScene::Background)` when the app leaves the
  screen (iOS `sceneDidEnterBackground`, Android `onPause`/`onStop`) and
  `app_scene(AppScene::Foreground)` when it comes back. It is not `suspend`/`resume`: those stay
  audio interruptions (Siri, a cellular call) and never touch video, and an app can go to the
  background with no audio interruption. In the background every webview sink is hidden (its
  track is turned off at the media server) and only native sinks (below) stay enabled; back in
  front each visible sink starts again at a keyframe. kessel-video's own
  `BackendEvent::Background` counts too: either one pauses, and both must say foreground to
  resume. On Android `app_scene` also tells kessel-video (its cameras close in the background).
  Audio is untouched.
- **Orientation.** On Android call `VoiceControl::device_orientation(degrees)` from
  `OrientationEventListener` (`(360 - 90 * Display.getRotation()) % 360`): camera frames are
  turned upright from the next one. On iOS the app sends nothing: kessel-video's camera turns
  frames upright itself (`AVCaptureDevice.RotationCoordinator`, iOS 17 and later), and
  `device_orientation` is ignored there.
- **Speaker with video.** The plugin never sets the audio route. `VoiceListener::video_changed
  (has_video)` says when the call gains or loses video: `true` while this device sends its
  camera or screen (on or paused) or any remote camera or screen is unmuted. Route the call's
  audio to the speaker while it is `true` (iOS `.defaultToSpeaker` in the category options or
  `overrideOutputAudioPort(.speaker)`; Android `setCommunicationDevice` with the built-in
  speaker, or `setSpeakerphoneOn(true)`), unless the person picked a headset or Bluetooth, and
  report `hasVideo` in `CXCallUpdate` (Telecom: `setVideoState`). `VoiceControl::has_video()`
  and the `state` command's `has_video` (present only while `true`) give the same flag on wake.

**Screen share on iPhone: the broadcast extension's ring.** iOS captures the screen only in a
broadcast upload extension, a separate process the person starts from the system's broadcast
picker. The extension writes raw frames into a shared-memory ring
([kessel-frame-ring](https://crates.io/crates/kessel-frame-ring) 0.1, an NV12 ring in a file);
the plugin reads them and publishes them as the call's screen. The extension never sees a key:
the app encodes and encrypts. Build with `video` and `mobile` (`mobile` turns on the plugin's
`ios-screen-ring`, which builds only on iOS).

- **App Group.** The app and the extension share one App Group (the
  `com.apple.security.application-groups` entitlement on both, e.g. `group.com.example.app`).
  The id is the app's: the plugin never names one.
- **Arm the ring once the call is connected**, with the group's container:
  ```swift
  let dir = FileManager.default
      .containerURL(forSecurityApplicationGroupIdentifier: "group.com.example.app")!
  ```
  ```rust
  control.screen_share_ring(container_path).await?; // makes <container>/sew-screen.ring
  ```
  It lasts until the call ends (the ring is then removed and the extension asked to finish);
  arming again replaces it. Refused with `not_in_call`, or `screen_unsupported` (a build
  without the ring, or a container the file can't be made in: `detail` says why).
- **Start** with `RPSystemBroadcastPickerView` (its `preferredExtension` set to your
  extension's bundle id). The webview's `screen {on: true}` can't start a broadcast on iOS.
  The share starts with the extension's first frame: the track is published (the same codec
  choice, encoders and layers as a desktop screen, at most 1280 × 720, at the room's screen
  rate) and `VoiceListener::screen_share_changed(true)` follows. Drive the app's share button
  from that event, not from your own taps: the share starts and stops outside the app.
- **Stop.** It ends, with `screen_share_changed(false)`, when the extension finishes or dies,
  when it stays quiet 2 s without having said it runs, when the ring's file shrinks or goes, or
  when the app calls `screen {on: false}` (the extension is then asked to finish). A running
  extension that sends nothing (iOS sends screen frames only when the screen changes) keeps its
  share; the plugin sends the last frame again each second. Each end makes a fresh ring, so the
  next broadcast is a new share.
- **Background.** The share keeps publishing while the app is in the background (that's the
  point); `app_scene(Background)` pauses the camera, not the screen.
- **The extension's contract** (Swift through a bridging header; the C functions and their
  safety rules are in kessel-frame-ring's README). Link kessel-frame-ring **0.1** as a static
  library (`crate-type` `staticlib`; the same format version as the app's, else attach fails
  with -4). On one serial queue:
  1. `broadcastStarted`: `kfr_producer_attach("<container>/sew-screen.ring", &err)`. `-2` means
     no ring (the app isn't in a call): finish the broadcast with an error saying so. Then
     `kfr_producer_start`.
  2. `processSampleBuffer(_, with: .video)`: if `kfr_producer_wants_frames` is false, return.
     Lock the pixel buffer read-only and call `kfr_producer_push_nv12` with planes 0 and 1
     (`CVPixelBufferGetBaseAddressOfPlane`, `GetBytesPerRowOfPlane`, and bytes per row ×
     `GetHeightOfPlane` as each length), the size, the rotation clockwise to upright from
     `RPVideoSampleOrientationKey` (`.up` 0, `.right` 90, `.down` 180, `.left` 270), the colour
     from the pixel format (`420v`: 1, BT.709 limited; `420f`: 2, BT.709 full; skip any other
     format), and the presentation time on the host clock in ns (or 0); unlock. A result below
     -1 is a broken ring: `kfr_producer_finish(p, 3)` and finish the broadcast.
  3. Poll `kfr_producer_stop_requested` on each frame and on a timer (about every 0.5 s): when
     true, finish the broadcast (the call ended or the app stopped the share).
  4. `broadcastFinished`: `kfr_producer_finish(p, 1)`, last and once.
  The extension writes raw frames only. It has about 50 MB of memory on iOS; a 720p ring is
  about 4 MB of it.
- **Trust.** The app treats the ring as hostile input (the extension is another process with
  the file open): every frame is checked against the app's own layout before a byte is read,
  and a bad one is skipped. What the ring can't guard against: an extension that shrinks the
  file between the check and a read crashes the app (SIGBUS; Apple has no file seals), so keep
  the ring in your own App Group only. iOS ends a suspended app that holds a file lock in a
  shared container (`0xdead10cc`): the plugin takes the ring's lock for a moment only while a
  share runs (the call's audio keeps the app awake then), never between shares. Neither this
  nor the rotation mapping above has been checked on a device yet.

**Screen share on Android: MediaProjection.** The app asks, the plugin publishes:

1. Declare a foreground service of type `mediaProjection`
   (`<service android:foregroundServiceType="mediaProjection" …/>`) and the permissions
   `FOREGROUND_SERVICE` and (Android 14+) `FOREGROUND_SERVICE_MEDIA_PROJECTION`.
2. Ask with `MediaProjectionManager.createScreenCaptureIntent()`. With the person's yes, start
   your foreground service (before `getMediaProjection`; Android 14 refuses otherwise), then
   get the projection and register a `MediaProjection.Callback` whose `onStop` calls
   `AndroidContext::media_projection_stopped`.
3. Hand it over on the same `AndroidContext` you gave `Builder::android_video_context` (a
   clone shares its state): `unsafe { ctx.set_media_projection(global_ref, width, height,
   density_dpi) }` with a JNI global reference, kept until `media_projection_stopped` returns.
   Call `set_screen_size` when the display turns.
4. Publish with `screen {on: true}` (no `source`: the projection is the choice, there is no
   picker); `screen {on: false}` unpublishes. Then stop the projection and your service.

The plugin never starts or stops services. The share keeps publishing in the background.

**Native sinks (picture in picture).** `VoiceControl::open_native_sink(target, max_size,
consumer)` (feature `video-core`) delivers a call's video in Rust, not over the bridge:

```rust
use tauri_plugin_sew_voice::native::{NativeSinkTarget, NativePayload, NativeVideoEvent};

let pip = control.open_native_sink(NativeSinkTarget::ActiveSpeaker, (640, 360),
    Arc::new(move |ev: NativeVideoEvent| match ev {
        NativeVideoEvent::Frame(f) => match f.payload {
            // AVSampleBufferDisplayLayer decodes it: h264_annex_b_to_avcc(&access_unit) for the
            // CMSampleBuffer, h264_parameter_sets(&access_unit) on a keyframe for the format
            // description; flush the layer first when f.discontinuity.
            NativePayload::H264 { access_unit, .. } => { /* … */ }
            // VP8, decoded by kessel-video: a CVPixelBuffer (NativeBuffer) on Apple.
            NativePayload::Decoded(frame) => { /* … */ }
            _ => {}
        },
        NativeVideoEvent::Reset { .. } => { /* flush */ }
        NativeVideoEvent::Switched { track, member_id } => { /* new speaker */ }
        _ => {}
    }))?;
// … drop(pip) closes it.
```

- `NativeSinkTarget::Track(sid)` shows one track; `ActiveSpeaker` follows whoever speaks
  (their camera, else their screen; before anyone speaks the first remote camera), holds a
  speaker at least 1.5 s, sends `Switched` and asks for a keyframe on every switch.
- `max_size` is the largest size in device pixels: the media server sends the layer that fits
  (`(640, 360)`: its 360p layer) at 30 fps, and VP8 is decoded no larger.
  `NativeSink::set_max_size` changes it; `request_keyframe` asks for one.
- It is the one sink that stays enabled in the background.
- It never shows this device's own video: an active-speaker sink skips this device's camera and
  screen (and any track of your own member), and `Track("local:camera")`, `"local:screen"` or a
  track marked `is_self` is refused with `unknown_track` (`detail.self_view: true`). Self-view
  is a webview `local:camera` sink or the `preview`, never the picture in picture.
- H.264 comes as access units (Annex B, 4-byte start codes, SPS and PPS before every IDR). VP8
  comes decoded through kessel-video's decoder (NV12, turned into a `CVPixelBuffer` on Apple:
  `payload.apple_pixel_buffer()` answers its `CVPixelBufferRef`, borrowed while the frame lives;
  `CVBufferRetain` it to keep it longer, then wrap it in a `CMSampleBuffer` for the layer).
  Without a video backend a VP8 track can't be opened (`decoded_unsupported`), and an
  active-speaker sink shows only H.264 speakers. Where the backend makes no OS buffer (kessel-video
  0.1.1's Apple backend makes none from a decoded frame yet), the frame comes as NV12 CPU planes
  (`apple_pixel_buffer()` answers `None`): the sink logs that once and counts the frames in
  `stats().cpu_planes`; the consumer then copies the planes into a `CVPixelBuffer` of its own
  (a `CVPixelBufferPool`), and that copy counts in `consumer_us_avg`.
- Every (re)start (an open, a reset, a switch, a frame the sink had to drop) begins at a keyframe
  with `discontinuity` set. The consumer is called on the sink's own thread, in order; a slow
  consumer makes the sink drop frames and wait for a keyframe, never block the call. The sink
  takes the decoder's output when its wake callback runs (never on the next input or a timer).
  VP8 frames the decoder has no room for (its input queue of 4 full) wait in order in the sink's
  backlog (`DECODE_BACKLOG`, 8) and go in as outputs make room; a keyframe supersedes the deltas
  waiting before it (`dropped_superseded`); a backlog overflow drops them and the sink skips to a
  keyframe it asks for (`dropped_busy`; never a silent drop).
- The sink's thread runs at the user-interactive QoS class on Apple (the VP8 decoder's thread
  is started from it).
- `NativeSink::stats()` (`NativeSinkStats`): `frames_in`, `frames_out`, `fps` (last 10 s),
  `decode_us` / `decode_us_avg` / `decode_us_peak` (submit to output), split into
  `submit_to_wake_us_avg` / `_peak` (to the decoder's wake callback) and `wake_to_drain_us_avg` /
  `_peak` (from the wake to the sink's thread taking the output), `decoder_in_flight` / `_peak`,
  `busy_retried`, `dropped_superseded`, `convert_us_avg`,
  `consumer_us_avg` / `consumer_us_peak`, `queue_depth` / `queue_peak`, drops by reason
  (`dropped_queue_full`, `dropped_busy`, `dropped_decoder_output`, `skipped_to_keyframe`),
  `decode_errors`, `keyframes_asked`, `cpu_planes`, and the step-down (`low_layer`,
  `step_downs`, `step_ups`). The sink logs them (`tracing`, info, "native sink stats") every
  10 s while open.
- **Step-down.** More than 3 drops within 2 s (any reason) and the sink asks the media server for
  the 180p layer (`LOW_LAYER`, 320 x 180) instead of the one `max_size` fits; after 10 s with no
  drop and a decode time (moving average) under a third of the frame interval (11 ms at 30 fps)
  it asks for its own layer again. A step-down within 10 s of a step-up doubles the wait before
  the next step-up (up to 80 s), so a sink that can't hold the upper layer doesn't flap.
- Refused with `not_in_call`, `unknown_track` (also self-view, above) or `decoded_unsupported`.

## 3c. Call details (0.4.1)

The frozen events above carry what K2's own call UI needs. A call UI that shows what a browser
call shows (the security code panel, the "not in the encrypted session" warning, the recorder's
tile, the connection bars, an incoming DM call) also uses these. They are not in
`sew-voice:default`: add `sew-voice:call-details` to the window's capability (§4).

| Command | Arguments | Answer |
|---|---|---|
| `call_details` | — | `{security, quality, roster, incoming}`: the four events' payloads now, for a window opened mid-call (`security` and `incoming` are `null` when there is none, `roster` is `[]` with no call) |
| `ring` | `{call_id, target, room, started_by?, silent?, reason?}` | `null`. A DM call rings: every window gets `incoming`, and a native listener `on_incoming`. `target` and `room` are the `join` that accepts it; that `join` clears it. A newer ring replaces the one before. `invalid_args` without `call_id`, `target` or `room` |
| `ring_ended` | `{call_id}` | `null`. That ring stopped (Sew's ring ended, the call ended, or the person declined). Another call's id changes nothing |

| Event | Payload |
|---|---|
| `sew-voice://security` | `{state, epoch?, since_ms?, show_after_ms?, reason?, code?}`, or `null` with no call |
| `sew-voice://roster` | `[{identity, member_id?, is_self, label}]`: every media participant, this device and the recorder included |
| `sew-voice://quality` | `{quality, packet_loss?, concealed?, jitter_ms?, rtt_ms?}` |
| `sew-voice://incoming` | `{call_id, target, room, started_by?, silent, reason?}`, or `null` once it stops ringing |

- **The security code.** `state` is the call's key state: `preparing`, `joining`, `active`,
  `securing` (show "Securing…" only `show_after_ms` after `since_ms`), `recovering`,
  `device_not_enrolled`, `server_identity_changed`, `unsupported`, `failed` or `left`
  (`reason` says why for the ones that have one). While `active`, `epoch` is the key epoch and
  `code` is its security code: six groups of five digits (`"01234 56789 …"`), the same on
  every device in the epoch, browsers included. People compare it out loud. It changes with
  every epoch (someone joins or leaves), and it is absent when Sew's policy hides it.
- **Identity and label.** `identity` is the engine identity of one join (`p_…`; a re-join
  gets a new one). `label` is `member` once the call's keys bind the identity to a member (then
  `member_id` is written), `recorder` for Sew Notes' recorder (no member), and
  `not_in_session` for a participant in the media room with no key: nothing of theirs plays,
  so show it as "not in the encrypted session". `participants` stays the members' list.
- **Quality** comes from the engine's stats windows (15 s): `lost` when audio was expected
  and none arrived, `poor` at 8 % loss, 10 % concealed audio, 80 ms jitter or a 400 ms round
  trip, `good` at 2 %, 3 %, 30 ms or 200 ms, else `excellent`; `unknown` before the first
  window and with no call. While the call is `reconnecting` it is `lost`, whatever the last
  window said. `packet_loss` and `concealed` are shares (0 to 1).
- **Rings.** The plugin learns of a ring from your page (Sew's `call_ring` on its gateway) or
  your host (a push: `VoiceControl::ring`). It holds no session of its own: declining is your
  page's or host's request to Sew (`POST /api/v1/channels/{id}/call/decline`), then
  `ring_ended`. On a phone, the listener's `on_incoming(Some)` is where CallKit's
  `reportNewIncomingCall` (or Telecom's `addNewIncomingCall`) goes, and answering is
  `VoiceControl::join` with the ring's `target` and `room`.
- The exact payloads are in the plugin's `tests/native-voice-details.json`; the fake call
  (`fake::FakeCall::set_security`, `set_roster`, `set_quality`) produces them with no call.

## 4. Permissions

The plugin ships Tauri 2 permission files. Put them in your capability:

| Permission | Allows |
|---|---|
| `sew-voice:default` | the eight call commands |
| `sew-voice:allow-join`, `allow-leave`, `allow-mute`, `allow-devices`, `allow-set-device`, `allow-state`, `allow-consent`, `allow-withdraw-consent` | one command each (`deny-*` exist too) |
| `sew-voice:device-keys` | `forget_server` and `known_device_keys` (0.4.1; not in `default`) |
| `sew-voice:allow-forget-server`, `allow-known-device-keys` | one each (`deny-*` too) |
| `sew-voice:recording` | `start_recording` and `stop_recording` (Sew 0.9.0), and `set_recording_rights` (plugin 0.6.1); not in `default`, §3a. Grant it only to a window with Record and Stop buttons, and only when your provider sends them |
| `sew-voice:allow-start-recording`, `allow-stop-recording`, `allow-set-recording-rights` | one each (`deny-*` too). A host that sends the rights from Rust (`VoiceControl`) needs no `allow-set-recording-rights` |
| `sew-voice:call-details` | `call_details`, `ring` and `ring_ended` (0.4.1; not in `default`, §3c) |
| `sew-voice:allow-call-details`, `allow-ring`, `allow-ring-ended` | one each (`deny-*` too) |
| `sew-voice:video` | the thirteen video commands (§3, Video; not in `default`) |
| `sew-voice:allow-video-caps`, `allow-video-devices`, `allow-set-video-device`, `allow-camera`, `allow-screen-sources`, `allow-screen`, `allow-preview`, `allow-video-sink`, `allow-video-sink-update`, `allow-video-sink-close`, `allow-request-keyframe`, `allow-camera-permission`, `allow-request-camera-permission` | one each (`deny-*` too). A window that only shows video needs the four sink ones |

A capability covers the app's bundled pages. For a remote page (a live site the shell loads),
add a capability with `"remote": { "urls": ["https://app.example.com/*"] }` and the permissions
that page needs; its CSP needs the IPC origin too (§1, Content Security Policy).

A window that only shows the call can get `allow-state` and `allow-devices` without `join` or
`set_device`. The device-key commands delete keys, so grant them only to the window that signs
the person out (or keep sign-out in Rust with `VoiceControl`). Events need Tauri's `core:event:default`, which `core:default` includes.

## 5. Microphone access on each OS

The example has all of these; copy them into your app.

- **macOS:**
  - `NSMicrophoneUsageDescription` in `src-tauri/Info.plist`. Tauri merges it into the bundle's
    plist. Without it, macOS never shows the permission prompt: the microphone is refused and
    the plugin reports `mic_denied` (kessel-audio's `no_permission` in `detail.audio`).
  - The `com.apple.security.device.audio-input` entitlement in `src-tauri/Entitlements.plist`,
    referenced from `tauri.conf.json` as `bundle.macOS.entitlements`. The hardened runtime
    that notarization needs blocks the mic without it.
  - A sandboxed Mac App Store build also needs `com.apple.security.app-sandbox` and
    `com.apple.security.network.client`.
  - The engine checks the microphone authorization before it opens the mic, and reports a
    denial as `mic_denied`.
  - The first time, macOS shows its prompt while the call starts. A `set_device` that picks an
    input during the call resolves (`null`) while the prompt is still up; if the user then
    refuses, the plugin reports `mic_denied`, and a later `set_device` on an input rejects with
    `mic_denied` (`detail.audio`: `no_permission`) until the user allows the app in System
    Settings › Privacy & Security › Microphone.
- **Windows:**
  - Tauri's MSI and NSIS installers build an unpackaged app. It needs no capability, but the
    user's switch at Settings › Privacy & security › Microphone › "Let desktop apps access your
    microphone" must be on. When it's off, the plugin probably reports `mic_unavailable` with
    `detail.audio` `backend`, not `mic_denied`: cpal, under kessel-audio, doesn't map WASAPI's
    `E_ACCESSDENIED` to a permission error. Treat `mic_unavailable` on Windows as "check the
    microphone privacy switch" too.
  - An MSIX-packaged build needs `<DeviceCapability Name="microphone"/>` in its
    `Package.appxmanifest`. See `src-tauri/windows/microphone-capability.xml`.
- **Linux:**
  - There's no permission prompt.
  - Your package depends on ALSA (`libasound2`; Arch: `alsa-lib`), and on PipeWire's ALSA
    plugin on PipeWire desktops (usually installed).
  - The example's `.deb` declares `libasound2`. For AppImage and PKGBUILD, add the same
    dependency.
  - Flatpak needs `--socket=pulseaudio` (PipeWire serves it) and `--device=all` for raw ALSA.
- **Android:**
  - `RECORD_AUDIO` and `MODIFY_AUDIO_SETTINGS` in the app's `AndroidManifest.xml`, and the
    runtime microphone prompt before the first join (`RECORD_AUDIO` is a runtime permission:
    without a yes the plugin reports `mic_denied`).
  - `sew_call_client::android_init(env, context)` once before the first join (the platform TLS
    verifier needs the JVM), with the `rustls-platform-verifier-android` package in the Gradle
    build.
  - The call audio mode (`AudioManager.MODE_IN_COMMUNICATION` during a call) is the app's job;
    the plugin never sets it. The audio itself is the OS's voice processing (§3b).
  - `examples/tauri-voice-mobile` does all of this; see its README.
- **iOS:** `NSMicrophoneUsageDescription` (Tauri merges `src-tauri/Info.ios.plist`). The
  `AVAudioSession` category and its activation are the app's (CallKit's): the plugin opens the
  microphone only after `session_activated()` (§3b). The example's iOS side has not been built
  yet.

## 6. Device keys

The engine enrols each install as a Sew E2EE device (`desktop`; `ios` or `android` on phones).
It keeps two things for it:
the device's private key and its public record (device id, certificate). Where they go:

- **Your own store**, if you pass one (recommended for an app that already keeps secrets). See
  below.
- **By default, a file.** The key goes in `device-<scope>.key` (mode 0600, in a 0700
  directory) and the record in `device-<scope>.json`, both under the plugin's data directory.
  On Windows there are no Unix modes: the file has the ACL it inherits from the data directory
  (by default the app data directory, `%APPDATA%\<identifier>`, which only that user and
  administrators can read; a `data_dir` of your own must be as private).
  `errors` carries a `key_storage_file` warning once per call setup, so your UI can say so.
- **The OS credential store**, only if you build with the optional `keyring-store` feature
  (Keychain, Windows Credential Manager, Secret Service). It's off by default in
  `sew-call-client` and in the plugin. It uses `keyring` with `async-secret-service` on Linux,
  and keyring won't build if anything else in your app turns on `sync-secret-service`. If your
  app already uses `keyring`, leave this off and pass your own store.

The device is bound to the login through the credential's `device_scope`. A new sign-in enrols
a new device, and signing out revokes it on Sew's side.

### Supplying your own store

Implement `DeviceKeyStore` (re-exported by the plugin; it's
`sew_call_client::device::DeviceKeyStore`) over your keyring or vault, and give it to the
builder:

```rust
use std::sync::Arc;
use tauri_plugin_sew_voice::{DeviceError, DeviceKeyId, DeviceKeyRecord, DeviceKeyStore};

#[derive(Debug)]
struct MyVaultStore { /* your vault handle */ }

impl DeviceKeyStore for MyVaultStore {
    fn load(&self, id: &DeviceKeyId) -> Result<Option<DeviceKeyRecord>, DeviceError> {
        // Look up id.account() ("<install_id>:<member_id>:<device_scope>").
        // Return Ok(None) if there's nothing, or if the entry is unreadable.
        todo!()
    }
    fn store(&self, id: &DeviceKeyId, record: &DeviceKeyRecord) -> Result<(), DeviceError> {
        // Replace the whole entry: record.key (32 secret bytes) and record.device.
        todo!()
    }
    fn clear(&self, id: &DeviceKeyId) -> Result<(), DeviceError> {
        // Remove the entry. Ok(()) if there wasn't one.
        todo!()
    }
}

tauri_plugin_sew_voice::Builder::new(provider)
    .device_key_store(Arc::new(MyVaultStore { /* … */ }))
    .build()
```

Without the plugin, use
`ClientConfig::new(app_id, data_dir).with_device_key_store(store)`, then
`CallClient::with_mls(cfg)`. Or pass `StoreChoice::Host(store)` to `MlsKeys::new`.

The rules:

- **Keys.** Each entry is keyed by `DeviceKeyId`: Sew's install id, the member id and the
  login's `device_scope`. Keep entries for different ids apart.
- **Blocking.** The engine calls the store on a blocking thread, so a call may block (D-Bus,
  the Keychain, a vault round trip). It's called when the call's keys are set up, at
  enrolment, at certificate renewal, and when a device is forgotten.
- **What you store.** `record.key` is the device's private key, so treat it as a secret.
  `record.device` is public: device id, public key, certificate, install and member. You can
  serialise it however you like. All of `StoredDevice`'s fields are public, and the plugin
  re-exports it (`tauri_plugin_sew_voice::StoredDevice`, from sew-e2ee through
  `sew_call_client::device`), so you need no sew-e2ee dependency.
- **Recovery.** The engine handles it the same way as with the file:
  - If `load` answers `Ok(None)`, or a record whose key or certificate doesn't verify, the
    engine enrols a new device and `store`s it.
  - If Sew revokes or refuses the device, the engine `clear`s it, enrols once and retries
    once.
  - The host's `reset_device` also `clear`s it.
- **Errors.** `load` returning `Err` fails the call's key setup. The engine never falls back
  to the file, so keep `Err` for a store that can't answer at all.
- **Pins.** The server's call identity (the CA, the DS key and `external_sender`) is pinned
  on first sight, and the pins stay in `device-<scope>.json` under the data directory either
  way. With your own store, that file holds only the pins: there's no key file, and no
  `key_storage_file` warning. If the pins are lost while your store still has the device, you
  get a `pins_reset` warning, and the server's identity is pinned again on the next call. A
  changed pin is always refused (`ca_pin_mismatch`). Only `reset_device` clears pins.

`MemoryDeviceKeyStore` is an in-memory store for tests.

### A hardware key on phones (0.3.0)

On iOS and Android the device key can live in hardware: a non-exportable P-256 key in the
Secure Enclave or in StrongBox/TEE. Sew sees only its public key and its signatures. Implement
`HardwareDeviceKey` (re-exported by the plugin; it's
`sew_call_client::device::HardwareDeviceKey`) over the platform API, and give it to the
builder:

```rust
use tauri_plugin_sew_voice::{BoxFuture, DeviceError, DeviceKeyId, DevicePlatform, HardwareDeviceKey};

struct MyKeychainKey { /* your bridge to Swift or Kotlin */ }

impl HardwareDeviceKey for MyKeychainKey {
    fn public_key(&self, id: &DeviceKeyId) -> BoxFuture<'_, Result<Option<[u8; 65]>, DeviceError>> {
        let tag = id.account(); // the Keychain tag / Keystore alias
        Box::pin(async move { todo!("SEC1 uncompressed public key of `tag`, or None") })
    }
    fn generate(&self, id: &DeviceKeyId) -> BoxFuture<'_, Result<[u8; 65], DeviceError>> {
        let tag = id.account();
        Box::pin(async move { todo!("replace any key of `tag`; return its public key") })
    }
    fn sign(&self, id: &DeviceKeyId, msg: &[u8]) -> BoxFuture<'_, Result<Vec<u8>, DeviceError>> {
        let (tag, msg) = (id.account(), msg.to_vec());
        Box::pin(async move { todo!("ECDSA P-256 SHA-256 over `msg`: the platform's DER, as is") })
    }
    fn delete(&self, id: &DeviceKeyId) -> BoxFuture<'_, Result<(), DeviceError>> {
        let tag = id.account();
        Box::pin(async move { todo!("delete `tag`; Ok(()) if absent") })
    }
}

tauri_plugin_sew_voice::Builder::new(provider)
    .hardware_device_key(Arc::new(MyKeychainKey { /* … */ }))
    .device_platform(DevicePlatform::Ios) // optional: the default comes from target_os
    .build()
```

Without the plugin, use `ClientConfig::with_hardware_device_key(key)` (and
`with_device_platform`), or `StoreChoice::Hardware(key)` with `MlsKeys::new`.

The rules:

- **What it signs.** The enrolment proof and the call KeyPackages: about once per join. The
  MLS leaf keys are separate per-call keys in memory and never touch it.
- **Signatures.** Sign SHA-256 ECDSA over the message the way the platform does
  (iOS `.ecdsaSignatureMessageX962SHA256`, Android `"SHA256withECDSA"`) and return the DER
  signature as is. The engine converts it to the raw `r‖s` Sew wants, normalises it to low-S,
  and checks it under the key's public key before it uses it.
- **Async.** StrongBox can take 100 ms or more. Run the platform call on your own executor.
  The engine never calls the key with the plugin's lock held. The futures borrow only `self`:
  copy `id` and `msg` into them.
- **Access.** iOS `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`, with no user presence.
  On Android, no user authentication, so a lock-screen answer can sign.
- **The record.** The device id and certificate (public) stay in `device-<scope>.json` under
  the data directory, with the pins. There is no key file and no `key_storage_file` warning.
- **A key that's gone** (a restored backup, a wiped Keystore): answer `Ok(None)` from
  `public_key`. The engine then enrols a new device (`generate`). The old device is revoked by
  the person or the admin, as today.
- **Errors.** Map platform errors to `DeviceError::Unavailable(String)`. The join then fails
  with `e2ee_refused`. While a hardware key is set the engine never falls back to a software
  key, and the hardware key replaces any `device_key_store`.
- **The platform.** The device enrols as `ios` or `android` from `target_os` (`desktop`
  elsewhere); `device_platform` overrides it.
- **Lookup errors.** Answer `Ok(None)` from `public_key` only when the platform says the key
  doesn't exist (iOS `errSecItemNotFound`; Android: no entry for the alias). Any other error
  (iOS `errSecInteractionNotAllowed` before the first unlock, a Keystore failure) is
  `DeviceError::Unavailable`: reading it as "no key" would enrol a new device behind the
  person's back.
- **The public key** is 65 bytes, uncompressed P-256 (`0x04‖x‖y`). Refuse anything else in
  your bridge.
- **Simulators.** The iOS simulator has no Secure Enclave. The optional feature
  `sim-software-key` gives `SimSoftwareKey` (`in_memory()`, or `in_dir(dir)` to keep the
  device across launches), a software key behind the same trait that answers DER like the
  platforms. It builds only for `aarch64-apple-ios-sim`, `x86_64-apple-ios`,
  `x86_64-linux-android` and `i686-linux-android`; any other target refuses to compile with
  it, so a phone or desktop build can never carry it. (An arm64 Android emulator can't be told
  from a phone at compile time, so it's refused too.)

### Signing out: `forget_server` and `known_device_keys` (0.4.1)

When the person signs out of a Sew (or your app forgets one), delete its devices:

```rust
let control = app.state::<VoiceControl>().inner().clone();
let removed = control.forget_server(install_id).await?;   // Vec<DeviceKeyId>
let left = control.known_device_keys().await?;           // every key the store still holds
```

- `forget_server(install_id)` resets every device of that Sew install that the data directory
  holds a record of: its key is deleted (a hardware key through your
  `HardwareDeviceKey::delete`, once per key; a file key's file), and its record and the pins
  are removed. It answers the `DeviceKeyId`s it removed. The next call to that Sew enrols a new
  device and pins again on first sight. Revoking the old device on Sew's side is still the
  sign-out's (your server's) job.
- **It works per Sew install, not per login.** Every login on the same Sew (several accounts,
  or the same person through several Apps on one box) shares that install id, so
  `forget_server` deletes all of their devices on that install. Call it only once no saved
  account or server in your app still uses that install. Keep your own list of which accounts
  belong to which install id (the `install_id` of the `DeviceKeyId`s you see), and when a
  single account signs out but others remain, revoke just that account's device on Sew's side
  and leave the local store alone.
- It's refused with `in_call` while a call is live (connecting, held for consent, connected or
  reconnecting): leave first. A key that can't be deleted stops it with `device_store` and
  leaves that device listed, so a retry finds it again.
- `known_device_keys()` lists every key the store holds (all installs, sorted): with a hardware
  key, one Keychain item or Keystore alias each (`DeviceKeyId::account()`). Use it to clean up
  keys of a Sew your app no longer knows.
- With your own `DeviceKeyStore` the records are yours: these calls see only the data
  directory (the pins), so clear your store yourself.
- The webview commands are `forget_server {install_id}` (Sew's install id as a decimal
  string) and `known_device_keys`, both answering `[{install_id, member_id, device_scope,
  account}]` (ids as decimal strings), behind `sew-voice:device-keys` (§4). Without the plugin:
  `CallClient::forget_server` and `CallClient::known_device_keys`.

## 7. Run the example

`examples/tauri-voice-example` is a one-window app: a join form, mute and deafen buttons, the
participant list with speaking highlights, device pickers and an error log. It's plain
HTML/JS, with no bundler and no npm.

1. **Linux system packages (once).** Ubuntu/Debian:
   `sudo apt install libwebkit2gtk-4.1-dev build-essential libssl-dev libayatana-appindicator3-dev librsvg2-dev libasound2-dev pkg-config`.
   Arch: `webkit2gtk-4.1 base-devel openssl librsvg alsa-lib`.
   macOS: the Xcode command-line tools. Windows: WebView2 (preinstalled on 10/11) and the MSVC
   build tools.
2. **Offline, with no server.** The provider answers from memory and the engine is the fake
   (no audio, no network), so you can work on the UI:
   ```sh
   cd examples/tauri-voice-example/src-tauri
   cargo run
   ```
   Or use `cargo tauri dev` if you have the Tauri CLI (`cargo install tauri-cli --version '^2'`).
3. **Against your App's server.** It needs the route from §2 (and, for recording consent, the
   consent route; for Record and Stop, the recording route):
   ```sh
   SEW_VOICE_JOIN_URL=https://your-app.example/sew/voice/join \
   SEW_VOICE_CONSENT_URL=https://your-app.example/sew/voice/consent \
   SEW_VOICE_RECORDING_URL=https://your-app.example/sew/voice/recording \
   SEW_VOICE_APP_TOKEN=<a dev session> cargo run
   ```
   `http://` is accepted on loopback only. This is a real call: the real engine, on your
   system's microphone and speakers.
4. **Bundles.** `cargo tauri build` needs the full icon set: `cargo tauri icon your-icon.png`
   writes it into `src-tauri/icons/`, and you then list the files in `bundle.icon`.

The example isn't part of Sew's CI gate, and it lives in Sew's source tree, not in the crates
mirror. To build and test the plugin yourself (on Linux with the WebKitGTK 4.1 and ALSA
development files, e.g. Ubuntu's `libwebkit2gtk-4.1-dev` and `libasound2-dev`), from the crates
mirror's root, where every crate is a member of one workspace:

```sh
cargo test -p tauri-plugin-sew-voice --locked
```

In Sew's source tree the plugin is its own workspace; there, test it with
`cargo test --manifest-path crates/tauri-plugin-sew-voice/Cargo.toml --locked` and build the
example with `cargo build --manifest-path examples/tauri-voice-example/src-tauri/Cargo.toml --locked`.

### On a phone

`examples/tauri-voice-mobile` is the phone version: a dev sign-in form, join, mute, deafen,
leave, the participant list and plain-word errors. Its README covers the setup. It has the
microphone prompt, the Keystore device key, `android_init`, the network watch and the call
audio mode, with a small Kotlin and Swift bridge (`src-tauri/voice-bridge`). Its README lists
the bridge behaviours a third-party app should copy (key lookup errors, the public key length,
the network watch, the iOS permission and Bluetooth calls). Android builds and its iOS side is
not built yet. `scripts/android-example-build.sh` installs the Android
tools in user space (pinned, checksums checked) and builds debug APKs for aarch64 and the
x86_64 emulator.

## 8. Test your app without Sew

`tauri_plugin_sew_voice::fake` has an in-memory `FakeEngine`, a `FakeProvider` (answering,
failing or gated) and a `RecordingSink`. `VoiceService` is the plugin's logic without Tauri
(build with `default-features = false`). Drive it in a plain tokio test the way the plugin's
own tests do (the plugin crate's `tests/service.rs`):

- `FakeCall::connect`, `reconnecting(n)`, `end(reason)`, `add_remote(id)` and
  `emit(CallEvent::…)` play the engine's side;
- `FakeEngine::set_devices` plays a hot-plug.

### The real engine without Tauri

`tauri_plugin_sew_voice::real::CallClientEngine` is the plugin's engine over
`sew_call_client::CallClient` (feature `audio-core`; `audio`, on by default, adds the system
devices). `CallClientEngine::new(client, audio)` takes a `CallClient` you built and a function
that makes kessel-audio's `EngineConfig` for each call, so a test can run real calls on
kessel-audio's null device. `sew-call-client`'s `it` tests do that against a real Sew test
server (`plugin_adapter_join_connected_mute_leave` in `it/tests/real_call.rs`).

With `video-core`, `.with_video_backend(backend)` gives it a kessel-video backend (the default
engine of a plugin built with `video` takes `kessel_video::system`, made on first use): the
engine's one backend for `video_caps`, the cameras, `screen_sources`, `preview`, the camera and
screen publish, the native sinks and the decoded fallback. A test can pass
`kessel_video::fake::FakeBackend` (scripted cameras, permissions, screens and source events).
`video_capture()` is the engine's `VideoCapture` (the one the camera publish opens through).

How the real engine fills the plugin's events:

- `participants` lists a participant once the call keys or the call socket have named its
  member.
- `speaking` comes from kessel-audio's levels: the decoded audio of each remote track, and this
  device's processed microphone unless muted.
- `devices`: during a call, kessel-audio's list and hot-plug. Between calls, the system list.
  Nothing is listed at app launch: the first `devices`, `set_device` or `join` (or a host's
  `device_events`) lists the devices, and from then on the list is polled every 2 s between
  calls while something is subscribed to the device events (the plugin is, once used). Call
  `devices` before you show a picker. A `set_device` between calls applies to the next call;
  `"default"` is kessel-audio's `DeviceChoice::Default` (the OS default, followed when it
  changes).
- Audio errors: the OS refused the microphone → `mic_denied`; the device in use went away →
  `device_lost` (kessel-audio falls back to the default); anything else → `mic_unavailable`.
  `detail.audio` carries kessel-audio's own code (`no_permission` for a refusal; a Windows
  refusal probably arrives as `backend`, §5).

## 9. Licences

- `tauri-plugin-sew-voice` and the example are `MIT OR Apache-2.0`.
- `sew-call-client` is `MIT OR Apache-2.0`. Its LiveKit-derived pieces are in `sew-lk-compat`
  and `sew-lkproto` (Apache-2.0), so when you distribute your app, ship both crates' `NOTICE`
  files (`crates/sew-lk-compat/NOTICE`, `crates/sew-lkproto/NOTICE`).
- `kessel-audio` (K2) is `MIT OR Apache-2.0`.
- `sew-lkproto` and `sew-lk-compat` are Apache-2.0 (they hold LiveKit-derived code);
  `sew-proto`, `sew-e2ee` and `sew-mls-crypto` are `MIT OR Apache-2.0` (`sew-mls-crypto`'s
  `NOTICE` carries `openmls_rust_crypto`'s MIT licence).
- Sew's server is FSL-1.1-ALv2. Your app doesn't link it.
