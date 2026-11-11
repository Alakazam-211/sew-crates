# sew-call-client

sew-call-client is the native voice engine for Sew, a self-hosted team chat server: it joins a Sew voice call from a desktop app, with end-to-end encrypted audio, TURN relay and reconnects.

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

## Features

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
- `video-core` and `video` (off by default): `kessel-video`, K2's cameras, screen capture and
  video codecs, without (`video-core`) or with (`video`) the system's devices, OS codecs and
  its pure-Rust VP8. With either, calls receive video: the remote camera and screen tracks
  (`CallEvent::VideoAdded`, `VideoChanged`, `VideoRemoved`, `Participant::camera` and
  `screen`), each disabled at the media server until the host opens a sink for it
  (`Call::video_sink`), then decrypted, verified encoded VP8 and H.264 frames through
  `Call::video_frames`. `VideoCapture` opens cameras and screens (the OS picker, the screen
  list with PNG thumbnails) and runs the pre-join camera preview; `VideoBridge` publishes the
  camera and screens opened through it (one opener holds the camera), with the in-call
  self-view (`VideoBridge::self_view`). A build without them negotiates no video at all.
- `builtin-ring-provider` (default): TLS falls back to rustls's ring provider when you pass
  no provider and install no process default (see "TLS crypto provider" below). Without it,
  that case is refused with `no_crypto_provider`.
- `testing`: test-only helpers. Never in a release build. With `video-core` as well,
  `FixtureBackend` replays pre-encoded VP8 and H.264 simulcast streams.

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

The crate is not published on crates.io yet, because some of its dependencies are not:
`sew-e2ee` and `sew-mls-crypto`, and Alakazam Labs' fork of `str0m` (K2's `kessel-audio` and
`kessel-video` come from crates.io).
Until they are, get it from the read-only [sew-crates](https://github.com/Alakazam-211/sew-crates)
repository, which holds it with the Sew crates it depends on: clone it into your project (for
example as `third_party/sew-crates`) and copy the `[patch.crates-io]` rows its README lists
into your workspace's root `Cargo.toml`. It needs a Sew server to join calls on (see the same
README).

```toml
[dependencies]
sew-call-client = "0.6.1"
```

**Known issue without the `str0m` fork.** The fork's `[patch.crates-io]` rows carry a fix to
str0m's bandwidth estimate at the start of a call. A build that takes `str0m` from crates.io
instead works, but video layers can start low for a few seconds after joining until str0m
releases the fix.

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
            JoinRequest {
                target: "chat.example.com".into(),
                room: "48831765147549699".into(), // the voice channel's id
                muted: false,
                deafened: false,
                rejoin: None,
                recording_notice: None,
            },
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
- `Participant::recorder` marks the recorder in the roster.

### Network changes (phones)

Call `call.network_changed()` from your path monitor (`NWPathMonitor` on iOS,
`ConnectivityManager.NetworkCallback` on Android) when the default network changes. The engine
resumes at once on a fresh TURN allocation over the new network and restarts ICE, instead of
waiting for the old connections to time out; the state is `reconnecting` until it's done. It
is cheap and safe to repeat.

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

### Signing out

`client.forget_server(install_id)` deletes the devices of one Sew install that the data
directory holds (each key from its store, a hardware key through `HardwareDeviceKey::delete`,
then the record and the pins) and answers the removed `DeviceKeyId`s; it is refused with
`DeviceError::InCall` during a call. `client.known_device_keys()` lists every key the data
directory holds a record of (`device::forget_install` and `device::known_device_keys` take a
`VaultConfig` or a directory directly).

For a Tauri 2 app, `tauri-plugin-sew-voice` wraps this engine in eight commands and six events.

## Licence

MIT OR Apache-2.0, at your option. See `LICENSE-MIT`, `LICENSE-APACHE` and `NOTICE`.

This crate holds no LiveKit-derived code. It depends on `sew-lk-compat` and `sew-lkproto`
(Apache-2.0), which hold code ported from or generated from LiveKit, and, through `sew-e2ee`, on
`sew-mls-crypto`, which holds code derived from OpenMLS (MIT); their `NOTICE` files must travel
with any binary that includes this crate.

Copyright 2026 Alakazam Labs.
