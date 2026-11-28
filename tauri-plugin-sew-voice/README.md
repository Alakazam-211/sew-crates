# tauri-plugin-sew-voice

tauri-plugin-sew-voice is a Tauri 2 plugin that adds Sew voice calls to a desktop app: join, leave, mute, device choice, recording consent and live call state for the web view, with the sign-in kept in the app's Rust code.

Sew is Alakazam Labs' self-hosted team chat server. This plugin runs its native voice engine,
`sew-call-client`, inside your Tauri app's process: end-to-end encrypted audio, TURN relay,
reconnects, and the `kessel-audio` pipeline on the system's microphone and speakers. You need
a Sew server to use it.

Two labels appear in these docs:

- **K2** is Alakazam Labs' platform, which can host Sew. `kessel-audio` and `kessel-video`, the
  audio and video pipelines underneath, are K2's crates (MIT OR Apache-2.0, on crates.io).
  Nothing here needs K2: the plugin works against any Sew server.
- **Sew version** ("Sew 0.9.0") is a version of the Sew server. Next to a feature, it is the
  first server version that has it. In the CHANGELOG's headings, it is the server version that
  release of this plugin is tested against. A plain version ("0.4.1") next to a feature is the
  plugin's own version that added it.

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
leave Rust. They are behind the `sew-voice:video` permission set ([native voice SDK guide](https://github.com/Alakazam-211/sew-crates/blob/main/docs/native-voice-sdk.md), section 3,
Video). The engine's video is wired through `VoiceEngine` and `CallHandle` (module `video`);
an engine without it answers `video_unsupported`. With the `video-core` feature the real
engine receives remote video and publishes the camera and the screen share (sew-call-client's
`VideoBridge`), with this device's self-view on `local:camera` and `local:screen`. On phones (`video` and `mobile`) it uses
kessel-video's iOS and Android backends, follows the app's scene (`VoiceControl::app_scene`),
tells the app when the call has video (`VoiceListener::video_changed`, for the speaker route)
and gives native sinks for a picture in picture (`VoiceControl::open_native_sink`). A native
sink on `local:camera` is a native self-view: the camera's own frames as captured, with no
encode and no decode (on macOS and iOS the camera's `CVPixelBuffer`, elsewhere CPU planes),
upright and never mirrored, each with the camera's facing so the app mirrors a front camera's
view; it shows whenever the camera is open (the preview or a call) and never opens it.

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
also carries `model` (`dfn3`, `rnnoise`), `model_reason` (why it is RNNoise) and `step_downs`,
and for a call's log the stage's timing: `step_down_reason`, `last_step_down_ms_ago`,
`slowest_block_us`, `blocks_over_budget`, `in_grace`, `dfn3_state` (`idle`, `warming`,
`running`, `stepped_down`, `retired`), `retry_in_ms`, `grace_step_downs`,
`slowest_block_cpu_us`, `p99_block_us`, `p99_block_cpu_us` and kessel-audio's
`capture_stage_strikes`, `capture_stage_bypassed`, `capture_stage_p99_us`, and its grace
window for the stage: `capture_stage_grace`, `capture_stage_grace_blocks`,
`capture_stage_grace_over_budget` and `capture_stage_last_transient` (`call_start`,
`camera_start`, `app_scene_change`, …), and how it times the stage: `capture_stage_clock`
(`thread_cpu`, `wall`), `capture_stage_max_wall_us`, `capture_stage_preempted_over_budget` and
`capture_stage_starved`, and how the OS schedules the processing thread the stage runs on:
`capture_stage_dsp_scheduling` (`workgroup`, `time_constraint`, `qos`, `none`; the thread asks
for real-time scheduling on macOS and iOS and keeps the OS's default elsewhere). The stage
judges its blocks by the audio thread's CPU time where the platform has a clock for it, so a
block that waited for a core while another part of the app ran does not count against the
model. A call's start,
the audio session activating, a route change, a camera starting and the app's lifecycle
(Tauri's resumed event, a window's focus, `VoiceControl::app_scene`) each open a short grace
window in which the stage keeps the model through the moment's load (`in_grace: true`), and
the audio pipeline holds its own timing rule for the stage back for as long
(`capture_stage_grace: true`).

Not affiliated with or endorsed by LiveKit.

- Source: <https://github.com/Alakazam-211/sew-crates> (read-only: it is generated from Sew's
  repository at each release, so pull requests are not merged there)
- API documentation: <https://docs.rs/tauri-plugin-sew-voice>
- Issues: <https://github.com/Alakazam-211/sew-crates/issues/new/choose>,
  with the `tauri-plugin-sew-voice` form, which labels the issue `crate:tauri-plugin-sew-voice`
- Security problems: report them privately, as
  [SECURITY.md](https://github.com/Alakazam-211/sew-crates/blob/main/SECURITY.md) says.
- MSRV: Rust 1.95.

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
  ([native voice SDK guide](https://github.com/Alakazam-211/sew-crates/blob/main/docs/native-voice-sdk.md), section 3b).
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
  ([native voice SDK guide](https://github.com/Alakazam-211/sew-crates/blob/main/docs/native-voice-sdk.md), section 3b).
- `keyring-store` (off by default): the device key in the OS credential store. Without it, or
  your own store (`Builder::device_key_store`), the key is a 0600 file under the app's data
  directory. On phones, `Builder::hardware_device_key` keeps it in the Secure Enclave or
  StrongBox/TEE instead ([native voice SDK guide](https://github.com/Alakazam-211/sew-crates/blob/main/docs/native-voice-sdk.md), section 6).
- `builtin-ring-provider` (default): the engine's TLS falls back to ring when you pass no
  rustls crypto provider (`Builder::crypto_provider`) and your app installs no process default
  (`CryptoProvider::install_default`). Without it, that case fails the join (`engine_stopped`,
  the detail naming `no_crypto_provider`; [native voice SDK guide](https://github.com/Alakazam-211/sew-crates/blob/main/docs/native-voice-sdk.md), section 1).
- `dfn3` (off by default): `standard` noise removal runs DeepFilterNet3's low-latency model,
  with RNNoise as its step-down (sew-call-client's `dfn3`); see "Shipping DeepFilterNet3 in
  your app" below.
- `noise-bench` (debug builds only, never in `default`): `VoiceControl::noise_bench`, the
  noise removal stage on the device's microphone for a while, reported as JSON.
- `noise-probe` (measurement builds only, never in `default`; turns on `dfn3`):
  `VoiceControl::arm_noise_probe` has the next call's noise removal stage record what each
  block cost, which core the processing thread was on and what DeepFilterNet3 ran;
  `noise_probe_report` answers it as JSON, and `noise_probe_reference` runs the probe's
  calibration loops on a thread of its own. No plugin command: an app wraps them in its own.
  On macOS and iOS the core number is `pthread_cpu_number_np`, linked strongly: a probe build
  needs iOS 14.2 or macOS 11 or later.
- `sim-software-key` (off; iOS simulator and x86 Android emulator builds only, refused
  elsewhere): `SimSoftwareKey`, a software stand-in for the hardware key.
- With no features: the plugin's core (`VoiceService`, the JSON shapes in `wire`, the engine
  interface in `engine`) and the in-memory fake engine (`fake`) for tests and offline UI work.

## Usage

The crate is on crates.io, with the voice engine it runs, `sew-call-client`, and every crate
they depend on: the Sew crates, Alakazam Labs' fork of `str0m` (`sew-str0m` 0.25.6) and K2's
`kessel-audio` and `kessel-video`. A dependency line is all it needs. It needs a Sew server to
join calls on (see the [sew-crates](https://github.com/Alakazam-211/sew-crates) README).

`src-tauri/Cargo.toml`:

```toml
[dependencies]
tauri = "2"
tauri-plugin-sew-voice = "0.9.7"
# For a JoinProvider like sew-call-client's example (its JoinProvider docs):
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
url = "2"
serde_json = "1"
```

`MyJoinProvider` is your app's implementation of `sew_call_client::JoinProvider` (re-exported
here as `tauri_plugin_sew_voice::JoinProvider`): it asks Sew for the join answer with the app's
own session. [`sew-call-client`'s `JoinProvider` documentation](https://docs.rs/sew-call-client/latest/sew_call_client/trait.JoinProvider.html)
says which crates it needs and has a complete one. The plugin goes on the app's Tauri
builder:

```rust
use tauri_plugin_sew_voice::{Builder, JoinProvider};

/// Adds Sew voice calls to the app, signing in through `provider`.
fn with_sew_voice<R: tauri::Runtime>(
    app: tauri::Builder<R>,
    provider: impl JoinProvider,
) -> tauri::Builder<R> {
    app.plugin(Builder::new(provider).build())
}
```

and in `src-tauri/src/main.rs`:

```rust,ignore
fn main() {
    let provider = MyJoinProvider {
        http: reqwest::Client::new(),
        api_base: url::Url::parse("https://chat.example.com/").expect("url"),
        session_token: load_session(), // your app's Sew session, kept in Rust
    };
    with_sew_voice(tauri::Builder::default(), provider)
        .run(tauri::generate_context!())
        .expect("tauri");
}
```

Allow the plugin in your window's capability. `sew-voice:default` allows the eight call
commands; the other permission sets add the rest (the table under "Commands" below says which
set holds each command), and every command also has its own `sew-voice:allow-<command>`
permission (`allow-set-device` for `set_device`). For example,
`src-tauri/capabilities/default.json` for an app whose main window runs calls with video,
recording, noise removal settings and the call details:

```json
{
  "$schema": "../gen/schemas/desktop-schema.json",
  "identifier": "default",
  "description": "The main window: Sew calls.",
  "windows": ["main"],
  "permissions": [
    "core:default",
    "sew-voice:default",
    "sew-voice:recording",
    "sew-voice:call-details",
    "sew-voice:noise-removal",
    "sew-voice:video"
  ]
}
```

Grant `sew-voice:device-keys` (`forget_server`, `known_device_keys`) only to the window that
signs the person out. A window that only watches a call can list `sew-voice:allow-state` and
`sew-voice:allow-devices` instead of `sew-voice:default`. Events need no permission: every
window gets them.

Then, from the web view (`window.__TAURI__` needs `app.withGlobalTauri: true` in
`tauri.conf.json`; with a bundler, import `invoke` and `listen` from `@tauri-apps/api`
instead). `target` is opaque to the plugin: it is handed to your `JoinProvider` to say which
Sew server (here its host):

```js
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
await listen("sew-voice://state", (e) => render(e.payload));
const { call_id } = await invoke("plugin:sew-voice|join", { target: "chat.example.com", room: channelId });
await invoke("plugin:sew-voice|mute", { muted: true });
```

### Commands

Every command is invoked as `plugin:sew-voice|<name>`. The arguments, answers and error codes
of each are in the
[native voice SDK guide](https://github.com/Alakazam-211/sew-crates/blob/main/docs/native-voice-sdk.md)
(sections 3, 3a and 3c).

| Command | Permission set | What it does |
|---|---|---|
| `join` | `sew-voice:default` | Joins a voice channel or DM call through your `JoinProvider`; answers `{call_id}`. |
| `leave` | `sew-voice:default` | Leaves the call. |
| `mute` | `sew-voice:default` | Mutes or deafens this device (`{muted?, deafened?}`). |
| `devices` | `sew-voice:default` | Lists the microphones and speakers, with the chosen ones. |
| `set_device` | `sew-voice:default` | Switches the microphone or speaker, during a call too. |
| `state` | `sew-voice:default` | Answers the call's state now, for a window opened mid-call. |
| `consent` | `sew-voice:default` | Sends the person's answer to a recording's notice. |
| `withdraw_consent` | `sew-voice:default` | "Stop recording me". |
| `start_recording` | `sew-voice:recording` | Starts a recording of the call (Record). |
| `stop_recording` | `sew-voice:recording` | Stops the call's recording (Stop). |
| `set_recording_rights` | `sew-voice:recording` | Gives the plugin the person's rights in the room, for servers that don't send recording controls. |
| `forget_server` | `sew-voice:device-keys` | Deletes the device keys of one Sew install (sign-out). |
| `known_device_keys` | `sew-voice:device-keys` | Lists the device keys the plugin's store holds. |
| `call_details` | `sew-voice:call-details` | Answers the key state, quality, roster and ring now. |
| `ring` | `sew-voice:call-details` | Sets an incoming DM call's ring. |
| `ring_ended` | `sew-voice:call-details` | Clears that ring. |
| `video_caps` | `sew-voice:video` | Answers what video this build and device can do. |
| `video_devices` | `sew-voice:video` | Lists the cameras, the chosen one and the camera and screen permissions. |
| `set_video_device` | `sew-voice:video` | Chooses the camera, during a call too. |
| `camera` | `sew-voice:video` | Turns this device's camera on or off in the call. |
| `screen_sources` | `sew-voice:video` | Lists the displays and windows to share, with thumbnails. |
| `screen` | `sew-voice:video` | Starts or stops sharing a display or window. |
| `preview` | `sew-voice:video` | Starts or stops the camera preview before or outside a call. |
| `video_sink` | `sew-voice:video` | Opens a video element's frame channel for a track. |
| `video_sink_update` | `sew-voice:video` | Updates a sink's size, visibility, tile and stats. |
| `video_sink_close` | `sew-voice:video` | Closes a sink. |
| `request_keyframe` | `sew-voice:video` | Asks the sender for a keyframe after a decoder failure. |
| `camera_permission` | `sew-voice:video` | Answers the camera permission without prompting. |
| `request_camera_permission` | `sew-voice:video` | Asks the OS for camera access where it prompts. |
| `set_video_codec_preference` | `sew-voice:video` | Sets this device's video codec preference. |
| `set_noise_removal` | `sew-voice:noise-removal` | Sets the microphone's noise removal (`off`, `standard`). |
| `noise_removal` | `sew-voice:noise-removal` | Answers the noise removal setting and what runs. |

The events (`sew-voice://<name>`): `state`, `participants`, `speaking`, `devices`, `errors`,
`recording`, `recording_controls`, `security`, `roster`, `quality`, `incoming`, `video`,
`local_video`, `video_devices`, `policy` and `noise_removal`.

### Recording consent (Sew 0.7.3)

Nobody is recorded without saying yes, and not answering is never a yes: this device's key
reaches the recorder only after Sew says the person `granted`. A join into a recorded room
answers with an empty `call_id` and the state `consent_required` (no microphone, no media) until
the person answers:

```js
await listen("sew-voice://recording", (e) => {
  const r = e.payload;                       // null: no recording
  showBadge(r !== null);
  // The running time from Sew's start, also for someone who joined mid-recording
  // (absent while a held join into an auto-record room waits).
  if (r && r.started_at) startTimer(Date.parse(r.started_at));
  // How long the audio is kept (0: until the transcript is final), also after answering.
  if (r && r.audio_keep_days !== undefined) showAudioKept(r.audio_keep_days);
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
[`contracts/native-voice-contract.json`](https://github.com/Alakazam-211/sew-crates/blob/main/tauri-plugin-sew-voice/contracts/native-voice-contract.json)
(`recording`).

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
tauri-plugin-sew-voice = { version = "0.9.7", features = ["dfn3"] }
```

The four files come in a GitHub release asset of the Sew release the plugin belongs to, on
<https://github.com/Alakazam-211/sew-releases>, at

```text
https://github.com/Alakazam-211/sew-releases/releases/download/v<sew-version>/sew-dfn3-<sew-version>.tar.gz
```

where `<sew-version>` is the Sew version in the heading of this release's CHANGELOG entry.
For plugin 0.9.7 that is Sew 0.11.7:
<https://github.com/Alakazam-211/sew-releases/releases/download/v0.11.7/sew-dfn3-0.11.7.tar.gz>
(sha256 `153721fd5f9f33663da7d831b3b611adcb9ca9e65233a6128d94941ad48d6697`). Each release's
`SHA256SUMS` and `release.json` carry the asset's sha256. The tarball holds a `dfn3/` folder
with these files:

| File | What |
|---|---|
| `dfn3_ll.nnef.tar` | the weights: 39,034,880 bytes, sha256 `b6e52c0bc66b82e42e3403d22cdd3b847143026d6a41cc4380a97b044f7766af` (also `sew_noise::dfn3::WEIGHTS_SHA256`) |
| `NOTICE` | the model's notice: its copyright, the conversion by Alakazam Labs, the citation |
| `LICENSE-MIT`, `LICENSE-APACHE` | the model's licence texts |

Fetch and check it at build time (the weights are not committed to your repository), for
example from `src-tauri/`:

```sh
v=0.11.7
curl -fLO "https://github.com/Alakazam-211/sew-releases/releases/download/v$v/sew-dfn3-$v.tar.gz"
echo "153721fd5f9f33663da7d831b3b611adcb9ca9e65233a6128d94941ad48d6697  sew-dfn3-$v.tar.gz" | sha256sum -c -
tar -xzf "sew-dfn3-$v.tar.gz"   # writes dfn3/
```

The app ships the files as resources: put them in a `dfn3/` folder of your app's resources;
the plugin looks for `<app resource dir>/dfn3/dfn3_ll.nnef.tar` at start. With Tauri's
bundler (`tauri.conf.json`):

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

### How the processing thread is scheduled

The audio pipeline runs its capture processing, noise removal included, on one thread.
`VoiceControl::set_dsp_sched` sets what that thread asks the OS for. Only macOS and iOS act
on it; on other platforms the setting is kept and has no effect. It is a Rust call, not a
plugin command: an app that wants it in its settings wraps it in a command of its own.

```rust,ignore
use tauri_plugin_sew_voice::{DspSched, DspSchedMode};

voice.set_dsp_sched(Some(DspSched::new(DspSchedMode::OwnWorkgroup)))?;
```

The modes (`DspSchedMode`):

- `Workgroup`, the default: the thread asks for the OS's real-time policy, and on iOS it
  also joins the audio unit's own workgroup while the phone's audio unit runs. This is what a
  call does when nothing is set.
- `TimeConstraintOnly`: asks for the real-time policy with the figures you give, and no
  workgroup.
- `None`: no request. The thread keeps the OS's default scheduling.
- `OwnWorkgroup`: asks for the real-time policy, and a work interval of the thread's own in place of
  the audio unit's workgroup. Each block of audio is one interval, due a set time after it starts
  (the interval deadline).

`DspSched::new(mode)` starts from the pipeline's default figures; `with_time_constraint_us`
and `with_interval_deadline_us` change them. Figures the pipeline would refuse answer
`invalid_args`, and nothing changes.

- It does nothing until you call it: a call behaves as before.
- It applies from the next call. The pipeline reads the setting once, as a call's audio
  starts, so a call under way keeps what it started with.
- `set_dsp_sched(None)` goes back to the default.
- The engine keeps the setting until the process ends: store it with the app's settings and
  set it again at start.

What a call asked for and what it got are in the `noise_removal` state and event, beside
`capture_stage_dsp_scheduling`, which says what the OS granted. Each field is absent while
unset:

- `capture_stage_dsp_sched_requested`: the mode asked for;
- `capture_stage_dsp_sched_effective`: the mode the thread runs in (`unsupported` on a
  platform without these calls);
- `capture_stage_dsp_sched_fallback`: why the two differ, when the OS refused part of the
  request and the thread fell back to `workgroup`;
- `capture_stage_dsp_period_us`, `capture_stage_dsp_computation_us`,
  `capture_stage_dsp_constraint_us`: the time-constraint figures asked for;
- `capture_stage_dsp_interval_deadline_us`: the interval deadline, in `own_workgroup`;
- `capture_stage_dsp_intervals`: how many work intervals the thread has finished.

### Content Security Policy

The page's CSP must allow Tauri's IPC origin in `connect-src`: `ipc://localhost` on macOS, iOS
and Linux, `http://ipc.localhost` on Windows and Android (`https://ipc.localhost` with Tauri's
`useHttpsScheme`). The examples use `connect-src ipc: http://ipc.localhost`. A remote page the
shell loads (a live site, not bundled assets) needs that in its own CSP header, plus a Tauri
capability whose `remote.urls` names the site's origin, with the plugin's permissions in it
([native voice SDK guide](https://github.com/Alakazam-211/sew-crates/blob/main/docs/native-voice-sdk.md), sections 1 and 4).

### Signing out (0.4.1)

`VoiceControl::forget_server(install_id)` deletes the devices of one Sew install from the
plugin's device store (a hardware key through your `HardwareDeviceKey::delete`) and its pins,
and answers the removed `DeviceKeyId`s; `VoiceControl::known_device_keys()` lists every key the
store holds. `forget_server` is refused with `in_call` during a call. It works per Sew install,
not per login: every login on the same Sew shares the install id, so call it only once no
account in your app still uses that install. The same two are webview
commands behind `sew-voice:device-keys` ([native voice SDK guide](https://github.com/Alakazam-211/sew-crates/blob/main/docs/native-voice-sdk.md), section 6).

### Call details (0.4.1)

Beside the frozen events, `sew-voice://security` (the key state and the active epoch's
security code), `roster` (every media participant with its engine identity and label: `member`,
`recorder`, `not_in_session`), `quality` (each stats window's connection quality; `lost` while
reconnecting) and `incoming` (a DM call that rings) give a call UI what a browser call shows.
`call_details` reads all four for a window opened mid-call; `ring` and `ring_ended` set and clear
the ring (from Sew's `call_ring`, or a push through `VoiceControl::ring`), and a `join` with the
ring's `target` and `room` accepts it. Allow them with `sew-voice:call-details`
([native voice SDK guide](https://github.com/Alakazam-211/sew-crates/blob/main/docs/native-voice-sdk.md), section 3c).

## Licence

MIT OR Apache-2.0, at your option. See `LICENSE-MIT`, `LICENSE-APACHE` and `NOTICE`.

This crate holds no LiveKit-derived code. Through `sew-call-client` it includes `sew-lk-compat`
and `sew-lkproto` (Apache-2.0), which hold code ported from or generated from LiveKit, and
`sew-mls-crypto`, which holds code derived from OpenMLS (MIT); their `NOTICE` files must travel
with any app that includes this plugin.

Copyright 2026 Alakazam Labs.
