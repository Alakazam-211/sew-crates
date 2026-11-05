# tauri-plugin-sew-voice

tauri-plugin-sew-voice is a Tauri 2 plugin that adds Sew voice calls to a desktop app: join, leave, mute, device choice, recording consent and live call state for the web view, with the sign-in kept in the app's Rust code.

Sew is Alakazam Labs' self-hosted team chat server. This plugin runs its native voice engine,
`sew-call-client`, inside your Tauri app's process: end-to-end encrypted audio, TURN relay,
reconnects, and the `kessel-audio` pipeline on the system's microphone and speakers.

The web view only draws the UI. It sends eight commands (`plugin:sew-voice|join`, `leave`,
`mute`, `devices`, `set_device`, `state`, `consent`, `withdraw_consent`) and listens to six
events (`sew-voice://state`, `participants`, `speaking`, `devices`, `errors`, `recording`). Your
app's Rust code signs in: it implements `JoinProvider`, which obtains Sew's join answer (and
sends the person's recording consent with the person's own session), so no token ever reaches
the web view.

Not affiliated with or endorsed by LiveKit.

## Features

- `tauri` (default): the Tauri glue (`Builder`, the commands, the event emitter, the permission
  files). Needs Tauri's Linux system libraries (webkit2gtk-4.1, gtk3, libsoup3).
- `audio` (default): the real engine on the system's audio devices (Linux builds need ALSA's
  development files).
- `audio-core`: the real engine on `kessel-audio`'s null device only, with no system libraries.
- `keyring-store` (off by default): the device key in the OS credential store. Without it, or
  your own store (`Builder::device_key_store`), the key is a 0600 file under the app's data
  directory. On phones, `Builder::hardware_device_key` keeps it in the Secure Enclave or
  StrongBox/TEE instead (docs/native-voice-sdk.md §6).
- `sim-software-key` (off; iOS simulator and x86 Android emulator builds only, refused
  elsewhere): `SimSoftwareKey`, a software stand-in for the hardware key.
- With no features: the plugin's core (`VoiceService`, the JSON shapes in `wire`, the engine
  interface in `engine`) and the in-memory fake engine (`fake`) for tests and offline UI work.

## Usage

The crate is not published on crates.io yet, because the voice engine it runs,
`sew-call-client`, is not: the engine waits on dependencies of its own (`sew-e2ee`,
`kessel-audio`, which you supply, and Alakazam Labs' forks of `str0m` and `dimpl`). Until then,
get it from the read-only [sew-crates](https://github.com/Alakazam-211/sew-crates) repository,
which holds it with the Sew crates it depends on: clone it into your project (for example as
`third_party/sew-crates`) and copy the `[patch.crates-io]` rows its README lists into your
app's workspace root. It needs a Sew server to join calls on (see the same README).

`src-tauri/Cargo.toml`:

```toml
[dependencies]
tauri = "2"
tauri-plugin-sew-voice = "0.4.0"
# For the example provider below (sew-call-client's README):
reqwest = { version = "0.12", features = ["json"] }
url = "2"
serde_json = "1"
```

`src-tauri/src/main.rs`:

`MyJoinProvider` is your app's implementation of `sew_call_client::JoinProvider` (re-exported
here as `tauri_plugin_sew_voice::JoinProvider`): it asks Sew for the join answer with the app's
own session. `sew-call-client`'s README has a complete one.

```rust
fn main() {
    let provider = MyJoinProvider {
        http: reqwest::Client::new(),
        api_base: url::Url::parse("https://chat.example.com/").expect("url"),
        session_token: load_session(), // your app's Sew session, kept in Rust
    };
    tauri::Builder::default()
        .plugin(tauri_plugin_sew_voice::Builder::new(provider).build())
        .run(tauri::generate_context!())
        .expect("tauri");
}
```

Allow the plugin in your window's capability (`sew-voice:default` allows all eight commands; one
permission per command exists too), then from the web view (`window.__TAURI__` needs
`app.withGlobalTauri: true` in `tauri.conf.json`; with a bundler, import `invoke` and `listen`
from `@tauri-apps/api` instead). `target` is opaque to the plugin: it is handed to your
`JoinProvider` to say which Sew server (here its host):

```js
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
await listen("sew-voice://state", (e) => render(e.payload));
const { call_id } = await invoke("plugin:sew-voice|join", { target: "chat.example.com", room: channelId });
await invoke("plugin:sew-voice|mute", { muted: true });
```

### Recording consent (Sew 0.7.3)

Nobody is recorded without saying yes, and not answering is never a yes: this device's key
reaches the recorder only after Sew says the person `granted`. A join into a recorded room
answers with an empty `call_id` and the state `consent_required` (no microphone, no media) until
the person answers:

```js
await listen("sew-voice://recording", (e) => {
  const r = e.payload;                       // null: no recording
  showBadge(r !== null);
  if (r && r.my_consent === "pending") showNotice(r.consent_prompt);
});
// The modal's answer (the notice the person saw):
await invoke("plugin:sew-voice|consent", { granted: true, notice_version: prompt.notice_version });
// "Stop recording me":
await invoke("plugin:sew-voice|withdraw_consent");
```

Your `JoinProvider` forwards `JoinRequest::recording_notice` as the voice join's
`recording_notice`, returns Sew's `409 recording_consent_required` as
`ProviderError::ConsentRequired` (`ProviderError::from_sew_error` reads Sew's error body), and
implements `consent` (Sew's `POST /api/v1/recordings/{id}/consent` with the person's session;
without it the answer is refused `consent_unsupported`). The exact payloads are in
`tests/native-voice-contract.json` (`recording`).

On macOS the app needs `NSMicrophoneUsageDescription` in its `Info.plist`, and the
`com.apple.security.device.audio-input` entitlement for a hardened-runtime build.

**Known advisory (RUSTSEC-2026-0387, P-256).** Through `sew-e2ee`, every join signs with a P-256
device key via `p256` 0.13, whatever the call's MLS suite; see `sew-e2ee`'s README for what that
means and our assessment.

## Licence

MIT OR Apache-2.0, at your option. See `LICENSE-MIT`, `LICENSE-APACHE` and `NOTICE`.

This crate holds no LiveKit-derived code. Through `sew-call-client` it includes `sew-lk-compat`
and `sew-lkproto` (Apache-2.0), which hold code ported from or generated from LiveKit, and
`sew-mls-crypto`, which holds code derived from OpenMLS (MIT); their `NOTICE` files must travel
with any app that includes this plugin.

Copyright 2026 Alakazam Labs.
