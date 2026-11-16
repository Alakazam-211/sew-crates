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

Video: fourteen more commands (`video_caps`, `video_devices`, `set_video_device`, `camera`,
`screen_sources`, `screen`, `preview`, `video_sink`, `video_sink_update`, `video_sink_close`,
`request_keyframe`, `camera_permission`, `request_camera_permission`,
`set_video_codec_preference`) and four events (`sew-voice://video`, `local_video`, `video_devices`,
`policy`). Each video element gets its frames over its own Tauri raw `Channel` (a 32-byte
header, then the encoded frame, decrypted in Rust) and decodes them with WebCodecs; keys never
leave Rust. They are behind the `sew-voice:video` permission set (docs/native-voice-sdk.md §3,
Video). The engine's video is wired through `VoiceEngine` and `CallHandle` (module `video`);
an engine without it answers `video_unsupported`. With the `video-core` feature the real
engine receives remote video and publishes the camera and the screen share (sew-call-client's
`VideoBridge`), with this device's self-view on `local:camera` and `local:screen`. On phones (`video` and `mobile`) it uses
kessel-video's iOS and Android backends, follows the app's scene (`VoiceControl::app_scene`),
tells the app when the call has video (`VoiceListener::video_changed`, for the speaker route)
and gives native sinks for a picture in picture (`VoiceControl::open_native_sink`).

Video codecs: pass the webview's WebCodecs probe as `join`'s `decode` (`["vp8", "h264"]`); the
engine then offers to receive only what this device decodes (VP8 always, H.264 only when the
webview or the OS decoders do, never on Linux). `set_video_codec_preference {preference}`
(`automatic`, `prefer_h264`, `prefer_vp8`) is a per-device user setting, read back in
`video_caps.codec_preference` with the codec it gives here and the reason when Prefer H.264
can't apply. H.264 is only ever the OS's hardware encoder, found at run time. The plugin keeps
the setting until the app quits: store it with the app's settings and set it again at start.

Noise removal (desktop): the microphone's audio goes through RNNoise (`sew-noise`) between echo
cancellation and gain control, `standard` by default (`Builder::noise_removal` sets the app's
own). `set_noise_removal {mode}` (`off`, `standard`) works before or during a call, with no
click, and answers `{mode, active, reason?}`, as `noise_removal` does; the
`sew-voice://noise_removal` event carries each change. `reason` says why what runs is less than
what was asked: `budget` or `fault` (the audio pipeline stopped the stage) or `unavailable`.
`strong` is refused with `noise_mode_unavailable`. Both commands are behind the
`sew-voice:noise-removal` permission set. The plugin stores nothing: keep the person's choice
with the app's settings and set it again at start.

On iOS and Android, `off` also turns the OS's voice processing off: the microphone is captured
raw, with echo cancellation and gain control kept and no noise suppression, from the call's
start or, set during a call, at once (the microphone pauses briefly while it reopens). The
state then carries `phone_mode` (`os` or `raw`), `phone_mode_switches`, `phone_mode_gap_ms`
and, if a switch failed and the phone kept its capture, `phone_mode_reason: "switch_failed"`.
On Android, raw capture uses the unprocessed audio source where the device has it, else the
voice recognition source. The plugin has no Android code to ask, so the app's Android side
reads `AudioManager.getProperty(AudioManager.PROPERTY_SUPPORT_AUDIO_SOURCE_UNPROCESSED)` and
passes `"true"` as `Builder::android_unprocessed_supported(true)` or
`VoiceControl::set_android_unprocessed_supported(true)`;
without it the voice recognition source is used.

With feature `dfn3`, `standard` runs DeepFilterNet3's low-latency model (by Hendrik Schröter
and the DeepFilterNet authors; MIT OR Apache-2.0), with RNNoise kept beside it as its automatic
step-down, on desktop and, after the OS's voice processing, on phones; see "Shipping
DeepFilterNet3 in your app" below. Without the weights `standard` is RNNoise. The state then
also carries `model` (`dfn3`, `rnnoise`), `model_reason` (why it is RNNoise) and `step_downs`.

Not affiliated with or endorsed by LiveKit.

## Features

- `tauri` (default): the Tauri glue (`Builder`, the commands, the event emitter, the permission
  files). Needs Tauri's Linux system libraries (webkit2gtk-4.1, gtk3, libsoup3).
- `audio` (default): the real engine on the system's audio devices (Linux builds need ALSA's
  development files). On iOS and Android, with `sew-call-client/mobile` (which a phone build
  needs anyway), the audio is the OS's own voice processing instead (VoiceProcessingIO;
  AAudio's voice-communication preset), with its echo cancellation and noise suppression on.
  The app reports the OS's audio session through `VoiceControl` (`session_activated`,
  `session_deactivated`, `suspend`, `resume`, `route_changed`); on iOS the microphone opens
  only after `session_activated`. The plugin never configures the session itself
  (docs/native-voice-sdk.md §3b).
- `audio-core`: the real engine on `kessel-audio`'s null device only, with no system libraries.
- `video-core` (off by default): the real engine receives the remote cameras and screens
  (decrypted, checked encoded frames for the sinks, decoded by the webview's WebCodecs;
  `video_caps.receive`) and opens native sinks (module `native`); with a video backend also
  the cameras, the screen list, the pre-join `preview`, and the camera and screen publish with
  the in-call self-view. `video` (off) adds `kessel-video`'s system backends (the camera
  permission and list; on phones the iOS and Android backends; its decoders are the decoded
  fallback for a codec the webview can't decode: VP8 in pure Rust, H.264 through VideoToolbox
  on macOS). `CallClientEngine::with_video_backend` passes another backend, the engine's one
  for capture, publish, native sinks and the decoded fallback. kessel-video comes from
  crates.io (no `[patch.crates-io]` row). On macOS and iOS its Apple frameworks link on their
  own, also when the app links Rust as a static library (Tauri on iOS: Xcode links
  `libapp.a`): kessel-video writes them into its object code as linker options. Only an app
  built with `-ignore_auto_link` (or a linker that skips those options) lists them itself:
  VideoToolbox, CoreMedia, CoreVideo, AVFoundation, CoreFoundation and Foundation.
- `mobile` (off): sew-call-client's `mobile`, for iOS and Android builds (the pure-Rust
  DTLS/SRTP provider; with `audio`, the OS's voice processing). It turns on `ios-screen-ring`.
- `ios-screen-ring` (off; iOS): with `video-core`, the iPhone screen share. The app's broadcast
  upload extension writes the screen into a `kessel-frame-ring` ring in the App Group container;
  the plugin publishes it as the call's screen (`VoiceControl::screen_share_ring`,
  `VoiceListener::screen_share_changed`; module `screen_ring`). kessel-frame-ring is a
  dependency on iOS only (and Linux, for the tests); elsewhere the feature does nothing.
  Android's screen share needs no feature: the app hands kessel-video its MediaProjection
  (docs/native-voice-sdk.md §3b).
- `keyring-store` (off by default): the device key in the OS credential store. Without it, or
  your own store (`Builder::device_key_store`), the key is a 0600 file under the app's data
  directory. On phones, `Builder::hardware_device_key` keeps it in the Secure Enclave or
  StrongBox/TEE instead (docs/native-voice-sdk.md §6).
- `builtin-ring-provider` (default): the engine's TLS falls back to ring when you pass no
  rustls crypto provider (`Builder::crypto_provider`) and your app installs no process default
  (`CryptoProvider::install_default`). Without it, that case fails the join (`engine_stopped`,
  the detail naming `no_crypto_provider`; docs/native-voice-sdk.md §1).
- `sim-software-key` (off; iOS simulator and x86 Android emulator builds only, refused
  elsewhere): `SimSoftwareKey`, a software stand-in for the hardware key.
- With no features: the plugin's core (`VoiceService`, the JSON shapes in `wire`, the engine
  interface in `engine`) and the in-memory fake engine (`fake`) for tests and offline UI work.

## Usage

The crate is not published on crates.io yet, because the voice engine it runs,
`sew-call-client`, is not. Its WebRTC stack is Alakazam Labs' fork of `str0m`, here
`sew-str0m` 0.25.3, from crates.io like `sew-e2ee`, `kessel-audio` and `kessel-video`. Until
the engine is on crates.io,
get it from the read-only [sew-crates](https://github.com/Alakazam-211/sew-crates) repository,
which holds it with the Sew crates it depends on: clone it into your project (for example as
`third_party/sew-crates`) and copy the `[patch.crates-io]` rows its README lists into your
app's workspace root. It needs a Sew server to join calls on (see the same README).

`src-tauri/Cargo.toml`:

```toml
[dependencies]
tauri = "2"
tauri-plugin-sew-voice = "0.8.1"
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
permission per command exists too; the two device-key commands, `forget_server` and
`known_device_keys`, are only in `sew-voice:device-keys` or their own `allow-*`, and the three
call-detail commands, `call_details`, `ring` and `ring_ended`, in `sew-voice:call-details`), then from the web view (`window.__TAURI__` needs
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

Record and Stop (Sew 0.9.0) are `start_recording` and `stop_recording`, outside
`sew-voice:default` (permission set `sew-voice:recording`). Your provider's
`JoinProvider::recording` sends each with the person's session (`RecordingRequest::path` and
`body`); without it both are refused `recording_unsupported`. Record is the starter's own
consent and nobody else's:

```js
await invoke("plugin:sew-voice|start_recording");   // the recording event follows Sew's answer
await invoke("plugin:sew-voice|stop_recording");    // refused `recording_required` in a room that requires it
```

Which of the two to show (0.6.1): send the person's rights in the room with
`set_recording_rights` (in `sew-voice:recording`), and the plugin says whether Sew would take a
Record or a Stop now, following the call as it changes:

```js
await invoke("plugin:sew-voice|set_recording_rights", {
  room: channelId, my_perms: channel.my_perms, dm: false,
  recording_enabled: policy.recording_enabled, recording_dm_enabled: policy.recording_dm_enabled,
});
await listen("sew-voice://recording_controls", (e) => {
  showRecord(e.payload.can_record);
  showStop(e.payload.can_stop);
});
```

A Sew server that computes each person's Record and Stop (0.7.0) sends them to the call
itself: the same event then says `source: "sew"` (and may carry Sew's `record_unavailable`
hint), and your rights are not used. `set_recording_rights` is for Sew servers that don't send
recording controls.

On macOS the app needs `NSMicrophoneUsageDescription` in its `Info.plist`, and the
`com.apple.security.device.audio-input` entitlement for a hardened-runtime build.

### Shipping DeepFilterNet3 in your app

With the plugin's `dfn3` cargo feature, `standard` noise removal runs DeepFilterNet3's
low-latency model, by Hendrik Schröter and the DeepFilterNet authors
(<https://github.com/Rikorose/DeepFilterNet>, MIT OR Apache-2.0), with sew-noise's own Rust
inference. Its weights are not in any crate: your app ships them.

```toml
tauri-plugin-sew-voice = { version = "…", features = ["dfn3"] }
```

The four files come in the asset `sew-dfn3-<version>.tar.gz` of the Sew release the plugin
belongs to, on <https://github.com/Alakazam-211/sew-releases> (a `dfn3/` folder holding them;
the asset's sha256 is in the release's `SHA256SUMS` and `release.json`):

| File | What |
|---|---|
| `dfn3_ll.nnef.tar` | the weights: 39,034,880 bytes, sha256 `b6e52c0bc66b82e42e3403d22cdd3b847143026d6a41cc4380a97b044f7766af` (also `sew_noise::dfn3::WEIGHTS_SHA256`) |
| `NOTICE` | the model's notice: its copyright, the conversion by Alakazam Labs, the citation |
| `LICENSE-MIT`, `LICENSE-APACHE` | the model's licence texts |

Put them in a `dfn3/` folder of your app's resources; the plugin looks for
`<app resource dir>/dfn3/dfn3_ll.nnef.tar` at start. With Tauri's bundler:

```json
{
  "bundle": {
    "resources": {
      "dfn3/dfn3_ll.nnef.tar": "dfn3/dfn3_ll.nnef.tar",
      "dfn3/NOTICE": "dfn3/NOTICE",
      "dfn3/LICENSE-MIT": "dfn3/LICENSE-MIT",
      "dfn3/LICENSE-APACHE": "dfn3/LICENSE-APACHE"
    }
  }
}
```

For another location, give the path to `Builder::dfn3_weights(path)`.

At start the plugin only checks the file, on its own thread: its sha256 against the one this
version pins, read through in chunks, with nothing kept in memory. The model is loaded when a
call first needs it (a call starting in `standard`, or the first switch to `standard` during
one), off the audio thread: that call starts on RNNoise (on phones, the OS's voice processing
alone) with `model_reason: "loading"` and steps up to DeepFilterNet3 without a click once it is
ready. After the last call the model is
released when no call has started for `Builder::dfn3_idle_unload` (five minutes by default),
and the next call loads it again. When the file is missing or is not the pinned one,
`standard` stays on RNNoise (on phones, the OS's voice processing alone) and the noise
removal state says why (`model: "rnnoise"`,
`model_reason: "unavailable"`); nothing else changes.

Carry `dfn3/NOTICE` (and the two licence texts) in your app's third-party notices.

### Content Security Policy

The page's CSP must allow Tauri's IPC origin in `connect-src`: `ipc://localhost` on macOS, iOS
and Linux, `http://ipc.localhost` on Windows and Android (`https://ipc.localhost` with Tauri's
`useHttpsScheme`). The examples use `connect-src ipc: http://ipc.localhost`. A remote page the
shell loads (a live site, not bundled assets) needs that in its own CSP header, plus a Tauri
capability whose `remote.urls` names the site's origin, with the plugin's permissions in it
(docs/native-voice-sdk.md §1 and §4).

### Signing out (0.4.1)

`VoiceControl::forget_server(install_id)` deletes the devices of one Sew install from the
plugin's device store (a hardware key through your `HardwareDeviceKey::delete`) and its pins,
and answers the removed `DeviceKeyId`s; `VoiceControl::known_device_keys()` lists every key the
store holds. `forget_server` is refused with `in_call` during a call. It works per Sew install,
not per login: every login on the same Sew shares the install id, so call it only once no
account in your app still uses that install. The same two are webview
commands behind `sew-voice:device-keys` (docs/native-voice-sdk.md §6).

### Call details (0.4.1)

Beside the frozen events, `sew-voice://security` (the key state and the active epoch's
security code), `roster` (every media participant with its engine identity and label: `member`,
`recorder`, `not_in_session`), `quality` (each stats window's connection quality; `lost` while
reconnecting) and `incoming` (a DM call that rings) give a call UI what a browser call shows.
`call_details` reads all four for a window opened mid-call; `ring` and `ring_ended` set and clear
the ring (from Sew's `call_ring`, or a push through `VoiceControl::ring`), and a `join` with the
ring's `target` and `room` accepts it. Allow them with `sew-voice:call-details`
(docs/native-voice-sdk.md §3c).

## Licence

MIT OR Apache-2.0, at your option. See `LICENSE-MIT`, `LICENSE-APACHE` and `NOTICE`.

This crate holds no LiveKit-derived code. Through `sew-call-client` it includes `sew-lk-compat`
and `sew-lkproto` (Apache-2.0), which hold code ported from or generated from LiveKit, and
`sew-mls-crypto`, which holds code derived from OpenMLS (MIT); their `NOTICE` files must travel
with any app that includes this plugin.

Copyright 2026 Alakazam Labs.
