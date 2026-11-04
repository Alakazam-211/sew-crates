sew-call-client is the native voice engine for Sew, a self-hosted team chat server: it joins a Sew voice call from a desktop app, with end-to-end encrypted audio, TURN relay and reconnects.

# sew-call-client

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
- `testing`: test-only helpers. Never in a release build.

## Usage

The crate is not on crates.io yet. It is published in the read-only
[sew-crates](https://github.com/Alakazam-211/sew-crates) repository, with the crates it depends
on. Its dependencies include `kessel-audio`, which you supply, and forks of `str0m` and `dimpl`;
that repository's README lists the `[patch.crates-io]` rows to copy into your workspace's root
`Cargo.toml`. It needs a Sew server to join calls on (see the same README).

```toml
[dependencies]
sew-call-client = "0.2.0"
```

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
- `Participant::recorder` marks the recorder in the roster.

For a Tauri 2 app, `tauri-plugin-sew-voice` wraps this engine in eight commands and six events.

**Known advisory (RUSTSEC-2026-0387, P-256).** Through `sew-e2ee`, every join signs with a P-256
device key via `p256` 0.13, whatever the call's MLS suite; see `sew-e2ee`'s README for what that
means and our assessment.

## Licence

MIT OR Apache-2.0, at your option. See `LICENSE-MIT`, `LICENSE-APACHE` and `NOTICE`.

This crate holds no LiveKit-derived code. It depends on `sew-lk-compat` and `sew-lkproto`
(Apache-2.0), which hold code ported from or generated from LiveKit, and, through `sew-e2ee`, on
`sew-mls-crypto`, which holds code derived from OpenMLS (MIT); their `NOTICE` files must travel
with any binary that includes this crate.

Copyright 2026 Alakazam Labs.
