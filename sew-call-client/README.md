# sew-call-client

sew-call-client is the native voice engine for Sew, a self-hosted team chat server: it joins a Sew voice call from a desktop app, with end-to-end encrypted audio, TURN relay and reconnects.

Sew is a self-hosted chat and calls server from Alakazam Labs. A company runs it on its own
Linux server for channels, threads, direct messages and voice and video calls; people reach it
from a browser or from apps built on its client API. This crate is Sew's native call client: a
desktop or phone app links it to join a Sew call from its own process, with the media encrypted
end to end between the people in the call. You need a Sew server to use it.

Two labels appear in these docs:

- **K2** is Alakazam Labs' platform, which can host Sew. `kessel-audio` and `kessel-video`, the
  audio and video pipelines this crate uses, are K2's crates (MIT OR Apache-2.0, on crates.io).
  Nothing here needs K2: the crate works against any Sew server.
- **Sew version** ("Sew 0.9.0") is a version of the Sew server. Next to a feature, it is the
  first server version that has it. In the
  CHANGELOG's headings, it is the server version that release of this crate is tested against.

Everything below the user interface for one call:

- signalling against Sew's media server over the LiveKit client protocol (through
  `sew-lk-compat`), with resume, full reconnect inside a 20 s window, and a re-join through the
  host's `JoinProvider` after that;
- transport on str0m, with TURN over UDP, TCP and TLS, relay-only when the call asks for it,
  and the platform's TLS trust;
- end-to-end encryption that cannot be turned off: MLS call keys (`sew-e2ee`) over the call
  credential and the call socket, the mandatory checks on every publication and subscription,
  and the device key store (`device`). Remote frames are decrypted and verified before the
  audio pipeline sees them; nothing is ever sent unencrypted;
- mute and deafen, the participant list, speaking levels, and the call credential's renewal;
- the call-stats beacon: when Sew's join answer carries its terms, one anonymous sample per
  window (played and concealed audio, packet loss, jitter, the longest gap, the path) to Sew,
  and the same numbers for the host (`Call::stats`, `CallEvent::Stats`; the `stats` module
  says how each is derived);
- with the `audio` features, the bridge to the `kessel-audio` audio pipeline (capture,
  processing, Opus, playout on the system's devices).

The host keeps the user's sign-in: it implements `JoinProvider`, which asks Sew for the join
answer and hands it over. No token needs to reach a web view.

Not affiliated with or endorsed by LiveKit.

- Source: <https://github.com/Alakazam-211/sew-crates> (read-only: it is generated from Sew's
  repository at each release, so pull requests are not merged there)
- API documentation: <https://docs.rs/sew-call-client>
- Issues: <https://github.com/Alakazam-211/sew-crates/issues/new/choose>,
  with the `sew-call-client` form, which labels the issue `crate:sew-call-client`
- Security problems: report them privately, as
  [SECURITY.md](https://github.com/Alakazam-211/sew-crates/blob/main/SECURITY.md) says.
- MSRV: Rust 1.95.

## Features

Which cargo features build where. Every iOS and Android build needs `mobile` (or
`rust-crypto`); the crate refuses to compile for a phone without it.

| Feature | Linux | macOS | Windows | iOS | Android |
|---|---|---|---|---|---|
| `audio` (default) | yes (ALSA development files) | yes | yes | yes (OS voice processing) | yes (OS voice processing) |
| `audio-core` | yes | yes | yes | yes | yes |
| `builtin-ring-provider` (default) | yes | yes | yes | yes | yes |
| `rust-crypto` | yes | yes | yes | yes (required) | yes (required) |
| `mobile` | not used | not used | not used | yes (required) | yes (required) |
| `keyring-store` | yes (Secret Service) | yes (Keychain) | yes (Credential Manager) | no | no |
| `dfn3` | yes | yes | yes | yes | yes |
| `noise-bench` | debug builds | debug builds | debug builds | debug builds | debug builds |
| `noise-probe` | measurement builds | measurement builds | measurement builds | measurement builds | measurement builds |
| `video-core` | yes | yes | yes | yes | yes |
| `video` | yes (VP8 only; screens need the xdg portal or libpipewire) | yes | yes | yes | yes |
| `testing` | tests only | tests only | tests only | tests only | tests only |
| `sim-software-key` | no | no | no | simulator only | x86 emulator only |

What each one does:

- `audio` (default): the `kessel-audio` bridge on the system's audio devices (cpal: CoreAudio,
  WASAPI, ALSA; Linux builds need ALSA's development files).
- `audio-core`: the bridge on `kessel-audio`'s null device only, with no system libraries.
- `keyring-store` (off by default): the device key in the OS credential store (macOS Keychain,
  Windows Credential Manager, the Linux Secret Service). Without it, or a store of your own
  (`ClientConfig::with_device_key_store`), the key is a 0600 file under the data directory.
- `rust-crypto`: the call's DTLS/SRTP in pure Rust (dimpl's RustCrypto backend and RustCrypto
  ciphers) and the E2EE frame cipher on RustCrypto, instead of aws-lc-rs; `CRYPTO_BACKEND` says
  which one a build has.
- `mobile`: what an iOS or Android build needs: `rust-crypto` (aws-lc-rs is never built for
  phones, and the crate refuses to compile there without it) and, with `audio` or
  `audio-core`, `kessel-audio`'s phone backend. With `audio` on a phone,
  `platform_audio_config` is the OS's own voice processing (iOS's VoiceProcessingIO unit,
  Android's AAudio with the voice-communication preset) with the OS's echo cancellation and
  noise suppression on; desktop keeps cpal (`AUDIO_BACKEND` says which). On Android call
  `sew_call_client::android_init(env, context)` before the first join (the platform TLS
  verifier needs the JVM), and add rustls-platform-verifier's Kotlin component to the app.
- Noise removal (with `audio-core`, desktop): `AudioBridge::start_with_noise` hands
  kessel-audio's capture hook a `sew-noise` stage (RNNoise, between echo cancellation and gain
  control; kessel-audio's own noise suppression is then off), in the mode a
  `NoiseRemovalControl` holds (`NoiseRemoval::Off` or `Standard`, the default). `set` changes it
  mid-call without a click; `state` and `subscribe` give `{mode, active, reason}`, where
  `reason` is `budget` or `fault` when kessel-audio stopped the stage, or `unavailable` where no
  stage runs (phones). `platform_audio_config` sets Standard on desktop.
- Noise removal Off on phones (`mobile`): the OS's voice processing is off too and the
  microphone is captured raw (iOS: the plain RemoteIO unit; Android: the unprocessed source
  where the device has it, else the voice recognition source), with kessel-audio's echo
  cancellation and gain control and no noise suppression. A call that starts in Off opens raw;
  `set` during a call switches the live call's capture both ways, with a short pause of the
  microphone. The state's `phone_mode` (`os`, `raw`), `phone_mode_switches`,
  `phone_mode_gap_ms` and `phone_mode_reason` (`switch_failed`) follow it. On Android, pass
  `AudioManager.PROPERTY_SUPPORT_AUDIO_SOURCE_UNPROCESSED` to
  `NoiseRemovalControl::set_android_unprocessed_supported`, and read kessel-audio's events
  through `AudioBridge::events` so a failed switch shows at once.
- `dfn3` (off by default): Standard runs DeepFilterNet3's low-latency model (by Hendrik
  Schröter and the DeepFilterNet authors; MIT OR Apache-2.0), with RNNoise beside it as its
  automatic step-down, on desktop and, after the OS's voice processing, on phones. The app
  ships the weights as a resource (`DFN3_WEIGHTS_FILE`, with their NOTICE and licence texts)
  and gives their path to `NoiseRemovalControl::set_dfn3_weights`, which only checks the
  file's pinned sha256; the model is loaded off the audio thread when a call first needs it
  (that call starts on RNNoise, on phones the OS's voice processing alone, with
  `model_reason: loading`, and steps up without a click), and released after
  `set_dfn3_idle_unload`'s time with no call (`DFN3_IDLE_UNLOAD` by default; `preload_dfn3`
  loads it at once). Without usable weights Standard stays RNNoise (on phones, the OS's voice
  processing alone). The state then also says which `model` runs, why it is RNNoise
  (`model_reason`) and how often the call's stage stepped down (`step_downs`), with the
  stage's timing for a call's log (`step_down_reason`, `last_step_down_ms_ago`,
  `slowest_block_us`, `blocks_over_budget`, `in_grace`, `dfn3_state`, `retry_in_ms`,
  `grace_step_downs`, `slowest_block_cpu_us`, `p99_block_us`, `p99_block_cpu_us`, and
  kessel-audio's `capture_stage_strikes`, `capture_stage_bypassed`, `capture_stage_p99_us`,
  `capture_stage_grace`, `capture_stage_grace_blocks`, `capture_stage_grace_over_budget`,
  `capture_stage_last_transient`, `capture_stage_clock`, `capture_stage_max_wall_us`,
  `capture_stage_preempted_over_budget`, `capture_stage_starved`, and how the OS schedules
  the processing thread the stage runs on, `capture_stage_dsp_scheduling`: `workgroup`,
  `time_constraint`, `qos`, `none` or `own_workgroup`; the thread asks for real-time scheduling on macOS and
  iOS). The call's start and its
  busy moments (the session activating, a route change, a device switch, a camera starting,
  and the app's lifecycle through `NoiseRemovalControl::app_scene_changed`) open a grace
  window in which the stage keeps the model through the load, and kessel-audio holds its own
  timing rule for the stage back for as long. Which files to ship and where:
  [tauri-plugin-sew-voice's README, "Shipping DeepFilterNet3 in your app"](https://github.com/Alakazam-211/sew-crates/tree/main/tauri-plugin-sew-voice#shipping-deepfilternet3-in-your-app).
- `noise-bench` (debug builds only, never in `default`): `noise_bench::run`, the stage on the
  device's microphone for a while, reported as JSON.
- `noise-probe` (measurement builds only, never in `default`; turns on `dfn3`):
  `NoiseRemovalControl::arm_probe` has the next call's stage record what each block cost, which
  core the processing thread was on and what DeepFilterNet3 ran, beside two fixed calibration
  loops on that thread; `probe_report` gives it as JSON, and `noise_probe::reference` runs the
  same loops on a thread of the app's. On macOS and iOS the app gives the function that reads
  the thread's core number (the system call needs `unsafe`, which this crate has none of);
  tauri-plugin-sew-voice's probe uses `pthread_cpu_number_np`, so a probe build of it needs
  iOS 14.2 or macOS 11 or later.
- `video-core` and `video` (off by default): `kessel-video`, K2's cameras, screen capture and
  video codecs, without (`video-core`) or with (`video`) the system's devices, OS codecs and
  its pure-Rust VP8. With either, calls receive video: the remote camera and screen tracks
  (`CallEvent::VideoAdded`, `VideoChanged`, `VideoRemoved`, `Participant::camera` and
  `screen`), each disabled at the media server until the host opens a sink for it
  (`Call::video_sink`), then decrypted, verified encoded VP8 and H.264 frames through
  `Call::video_frames`. `VideoCapture` opens cameras and screens (the OS picker, the screen
  list with PNG thumbnails) and runs the pre-join camera preview; `VideoBridge` publishes the
  camera and screens opened through it (one opener holds the camera), with the in-call
  self-view (`VideoBridge::self_view`). `VideoCapture::camera_tap` hands the camera's raw
  frames, as captured and before any encoder, to a native self-view (on macOS and iOS the
  camera's `CVPixelBuffer`; upright, never mirrored, with the camera's facing). A build without
  them negotiates no video at all.
- `builtin-ring-provider` (default): TLS falls back to rustls's ring provider when you pass
  no provider and install no process default (see "TLS crypto provider" below). Without it,
  that case is refused with `no_crypto_provider`.
- `testing`: test-only helpers. Never in a release build. With `video-core` as well,
  `FixtureBackend` replays pre-encoded VP8 and H.264 simulcast streams.
- `sim-software-key` (off; iOS simulator and x86 Android emulator builds only, refused
  elsewhere): `SimSoftwareKey`, a software stand-in for the hardware device key.

## TLS crypto provider

Every TLS connection the engine makes (signalling, TURN over TLS, Sew's API and the key routes)
uses one rustls crypto provider, chosen in this order:

1. yours, from `ClientConfig::with_crypto_provider(Arc<rustls::crypto::CryptoProvider>)`
   (`sew_call_client::rustls` is the rustls the engine uses);
2. else the process default, if your app installed one with `CryptoProvider::install_default`;
3. else ring, the built-in (feature `builtin-ring-provider`, on by default). With that feature
   off and neither of the above, TLS setup fails with `no_crypto_provider` (a join answers
   `CallError::Setup` naming it); the engine never picks whatever crypto happens to be linked.

The choice is logged at info, once per change: `tls provider: <name> (caller | process default |
built-in)`. It covers TLS only; the call's DTLS and SRTP keep their own backend.

## Usage

The crate is on crates.io, and so is every crate it depends on: the Sew crates (`sew-proto`,
`sew-lkproto`, `sew-lk-compat`, `sew-e2ee`, `sew-mls-crypto`, `sew-noise`), Alakazam Labs' fork
of `str0m` and K2's `kessel-audio` and `kessel-video`. A dependency line is all it needs. It
needs a Sew server to join calls on (see the
[sew-crates](https://github.com/Alakazam-211/sew-crates) README).

```toml
[dependencies]
sew-call-client = "0.9.4"
```

An app also needs, for its `JoinProvider` and to run a call:

```toml
# The engine runs on Tokio: call `CallClient::join` from inside a Tokio runtime.
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
# `JoinAnswer::from_json` takes Sew's client API base as a `url::Url`.
url = "2"
# Only for the example provider below, which calls Sew over HTTP with reqwest. Any HTTP
# client works: the provider only has to return the body of Sew's join answer.
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
serde_json = "1"
```

The engine's WebRTC stack is `sew-str0m`, Alakazam Labs' fork of `str0m` 0.25 (library name
`str0m`), which carries Sew's fixes (among them one to the bandwidth estimate at the start of a
call). This release uses `sew-str0m` 0.25.6 from crates.io.

Your app implements `JoinProvider`: it asks Sew for the join answer with the app's own
session and returns it. A minimal one, and a call through it (the same code is compiled as the
`JoinProvider` documentation's example):

```rust
use std::path::PathBuf;
use std::sync::Arc;

use sew_call_client::{
    BoxFuture, CallClient, ClientConfig, JoinAnswer, JoinProvider, JoinRequest, ProviderError,
};

/// The app's sign-in, kept in Rust: Sew's client API base and the app's session token.
struct MyJoinProvider {
    http: reqwest::Client,
    api_base: url::Url,
    session_token: String,
}

impl JoinProvider for MyJoinProvider {
    fn join(&self, req: JoinRequest) -> BoxFuture<'_, Result<JoinAnswer, ProviderError>> {
        Box::pin(async move {
            // Sew's voice join for the channel, asking for a call credential (instead of the
            // browser's `e2ee` join field; the engine's encryption is always on).
            let path = format!("api/v1/channels/{}/voice/join", req.room);
            let url = self
                .api_base
                .join(&path)
                .map_err(|e| ProviderError::Other(e.to_string()))?;
            let body = serde_json::json!({
                "self_mute": req.muted,
                "self_deaf": req.deafened,
                "call_credential": true,
            });
            let resp = self
                .http
                .post(url)
                .bearer_auth(&self.session_token)
                .json(&body)
                .send()
                .await
                .map_err(|_| ProviderError::Unavailable { retry_after_ms: None })?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                return Err(ProviderError::SigninRequired);
            }
            if !resp.status().is_success() {
                return Err(ProviderError::Refused {
                    code: resp.status().as_u16().to_string(),
                    detail: serde_json::Value::Null,
                });
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| ProviderError::Other(e.to_string()))?;
            // The answer, unchanged; the engine reaches Sew's client API at `api_base`.
            JoinAnswer::from_json(&bytes, self.api_base.clone())
                .map_err(|e| ProviderError::Other(e.to_string()))
        })
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let provider = Arc::new(MyJoinProvider {
        http: reqwest::Client::new(),
        api_base: url::Url::parse("https://chat.example.com/")?,
        session_token: "the app's Sew session".into(),
    });
    // Where the engine keeps the device key and its pins: a directory of the app's own.
    let data_dir = PathBuf::from("/var/lib/example-app/sew-voice");
    let client = CallClient::with_mls(ClientConfig::new("com.example.app", data_dir));
    let call = client
        .join(
            provider,
            // The room is the voice channel's id.
            JoinRequest::new("chat.example.com", "48831765147549699"),
        )
        .await?;
    // With the `audio` feature: capture and playout on the system's devices.
    let _audio = sew_call_client::AudioBridge::start(
        &call,
        sew_call_client::kessel_audio::EngineConfig::default(),
    )?;
    let mut events = call.events(); // CallEvent::State, Participants, Speaking, Error
    while let Ok(event) = events.recv().await {
        println!("{event:?}");
    }
    Ok(())
}
```

### `CallClient::with_mls` or `CallClient::new`

Both build the engine; they differ in who builds the call keys.

- `CallClient::with_mls(config)` is the one to use in an app. It builds Sew's MLS call keys
  (`e2ee::MlsKeys`) from the config: the device key where `ClientConfig` says (a 0600 file by
  default, your store with `ClientConfig::with_device_key_store`, a hardware key with
  `ClientConfig::with_hardware_device_key`) and the platform the device enrols as.
- `CallClient::new(config, keys)` takes any `e2ee::E2eeProvider` you built. Use it when you
  configure `MlsKeys` yourself, for example to set the device name Sew shows in the person's
  device list (`MlsKeys::with_label`), or in tests with the `testing` feature's `StaticKeys`.
  There is no provider that turns encryption off.

```rust
use std::path::PathBuf;
use std::sync::Arc;

use sew_call_client::e2ee::MlsKeys;
use sew_call_client::{CallClient, ClientConfig};

let data_dir = PathBuf::from("/var/lib/example-app/sew-voice");
let config = ClientConfig::new("com.example.app", data_dir);
// What an app needs: Sew's MLS call keys, from the config.
let _client = CallClient::with_mls(config.clone());
// The same keys built by hand, with the device name Sew shows.
let keys = MlsKeys::new(config.device_store.clone())
    .with_platform(config.device_platform)
    .with_label("Example app on Linux");
let _client = CallClient::new(config, Arc::new(keys));
```

### Recording consent (Sew 0.7.3)

A recorded call never records this device without the person's yes, and no answer is never a
yes: the engine feeds the call keys the recording state Sew sends (the join answer, the call
socket's own `recording_updated`, Sew's answer to a consent), and the keys release this
device's key to the recorder only while Sew says `granted`.

- A join that Sew refuses `409 recording_consent_required` (your provider returns
  `ProviderError::ConsentRequired`; `ProviderError::from_sew_error` reads Sew's error body)
  comes back held: `CallState::ConsentRequired { recording_id, notice_version }`, no media and
  no keys, the notice in `Call::recording()`. `call.consent(granted, notice_version)` joins
  again with the answer as `JoinRequest::recording_notice` (forward it as the voice join's
  `recording_notice`); `leave()` gives up.
- In a call, `Call::recording()` and `CallEvent::Recording` are the call's recording as Sew
  says it; `call.consent(..)` and `call.withdraw_consent()` ("Stop recording me") go through
  your provider's `JoinProvider::consent` with the person's own session (`ConsentAnswer::path`
  and `body` are Sew's route and body). The default `consent` sends nothing
  (`consent_unsupported`): the person stays unrecorded.
- Record and Stop (Sew 0.9.0): `call.start_recording()` and `call.stop_recording()` go through
  your provider's `JoinProvider::recording` with the person's own session
  (`RecordingRequest::path` and `body` are Sew's route and body); Sew's refusals come back as
  given. Record is the starter's own consent, nobody else's. The default `recording` sends
  nothing (`recording_unsupported`). `CallRecording::trigger` says what started a recording
  and `CallRecording::required` whether the room requires it (Stop is then refused
  `recording_required` while two or more people are in the room).
  `CallRecording::started_by` and `CallRecording::started_at` say who started it and when, by
  Sew's clock: count the running time from `started_at`, so someone who joins a call already
  being recorded sees the real time (`None` while a held join into an auto-record room waits).
  `CallRecording::audio_keep_days` says how many days Sew keeps the audio (0: deleted once the
  transcript is final), also after the person answered the notice; a held join into an
  auto-record room takes it from the notice.
- `Participant::recorder` marks the recorder in the roster.

### Network changes (phones)

Call `call.network_changed()` from your path monitor (`NWPathMonitor` on iOS,
`ConnectivityManager.NetworkCallback` on Android) when the default network changes. The engine
resumes at once on a fresh TURN allocation over the new network and restarts ICE, instead of
waiting for the old connections to time out; the state is `reconnecting` until it's done.
Calling it again for the same change does no harm.

### The phone's audio session

Keep one `AudioSession` for the app, start each call's audio with
`AudioBridge::start_with_session(&call, platform_audio_config(input, output), &session)`, and
report the OS's audio session to it with `session.event(SessionEvent::…)`: `Activated` and
`Deactivated` (CallKit's `didActivate` / `didDeactivate`), `Suspend` and `Resume` (an
interruption), `RouteChanged`. On iOS the audio starts deferred: the microphone and the speaker
open only once the session is active. Deactivated and Suspend pause I/O without ending the
call; Resume restarts it; RouteChanged re-reads the device. What the app said before a call
(an early `Activated`, a `Suspend`) applies when its audio starts. The engine never configures
or activates `AVAudioSession` or the Android audio mode: that stays the app's job. On desktop
the events change nothing.

### How the processing thread is scheduled

kessel-audio runs its capture processing, the noise removal stage included, on one thread.
`NoiseRemovalControl::set_dsp_sched` sets what that thread asks the OS for. Only macOS and
iOS act on it; on other platforms the setting is kept and has no effect.

```rust,ignore
use sew_call_client::{CaptureDspSched, CaptureDspSchedMode};

noise.set_dsp_sched(Some(CaptureDspSched::new(CaptureDspSchedMode::OwnWorkgroup)))?;
```

The modes (`CaptureDspSchedMode`):

- `Workgroup`, the default: the thread asks for the OS's real-time policy, and on iOS it
  also joins the audio unit's own workgroup while the phone's audio unit runs. This is what a
  call does when nothing is set.
- `TimeConstraintOnly`: asks for the real-time policy with the figures you give, and no
  workgroup.
- `None`: no request. The thread keeps the OS's default scheduling.
- `OwnWorkgroup`: asks for the real-time policy, and a work interval of the thread's own in place of
  the audio unit's workgroup. Each block of audio is one interval, due a set time after it starts
  (the interval deadline).

`CaptureDspSched::new(mode)` starts from the pipeline's default figures;
`with_time_constraint_us` and `with_interval_deadline_us` change them. Figures the pipeline
would refuse are refused when you set them, and nothing changes.

- It does nothing until you call it: a call behaves as before.
- It applies from the next call. The pipeline reads the setting once, as a call's audio
  starts, so a call under way keeps what it started with.
- `set_dsp_sched(None)` goes back to the default.
- Nothing is stored: keep the choice with the app's settings and set it again at start.

What a call asked for and what it got are in its noise removal state (`NoiseRemovalState`),
beside `capture_stage_dsp_scheduling`, which says what the OS granted. Each field is absent
while unset:

- `capture_stage_dsp_sched_requested`: the mode asked for;
- `capture_stage_dsp_sched_effective`: the mode the thread runs in (`unsupported` on a
  platform without these calls);
- `capture_stage_dsp_sched_fallback`: why the two differ, when the OS refused part of the
  request and the thread fell back to `workgroup`;
- `capture_stage_dsp_period_us`, `capture_stage_dsp_computation_us`,
  `capture_stage_dsp_constraint_us`: the time-constraint figures asked for;
- `capture_stage_dsp_interval_deadline_us`: the interval deadline, in `own_workgroup`;
- `capture_stage_dsp_intervals`: how many work intervals the thread has finished.

### Signing out

`client.forget_server(install_id)` deletes the devices of one Sew install that the data
directory holds (each key from its store, a hardware key through `HardwareDeviceKey::delete`,
then the record and the pins) and answers the removed `DeviceKeyId`s; it is refused with
`DeviceError::InCall` during a call. `client.known_device_keys()` lists every key the data
directory holds a record of (`device::forget_install` and `device::known_device_keys` take a
`VaultConfig` or a directory directly).

For a Tauri 2 app, [`tauri-plugin-sew-voice`](https://docs.rs/tauri-plugin-sew-voice) wraps
this engine in Tauri commands and events (its README lists them).

## Licence

MIT OR Apache-2.0, at your option. See `LICENSE-MIT`, `LICENSE-APACHE` and `NOTICE`.

This crate holds no LiveKit-derived code. It depends on `sew-lk-compat` and `sew-lkproto`
(Apache-2.0), which hold code ported from or generated from LiveKit, and, through `sew-e2ee`, on
`sew-mls-crypto`, which holds code derived from OpenMLS (MIT); their `NOTICE` files must travel
with any binary that includes this crate.

Copyright 2026 Alakazam Labs.
