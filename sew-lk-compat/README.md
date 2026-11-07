# sew-lk-compat

sew-lk-compat is the LiveKit-protocol layer of Sew's native voice engine: the `/rtc/v1` join request, a sans-I/O signalling session for a single-peer-connection voice client, and LiveKit's AES-GCM media frame format.

Sew, Alakazam Labs' self-hosted team chat server, runs its calls on a media server that speaks
the LiveKit client protocol. Its native voice engine, `sew-call-client`, joins those calls
through this crate:

- `signal`: the `/rtc/v1` join URL with its `join_request`, the `ClientInfo` it declares, and
  ICE trickle candidates (`JoinParams`, `ClientInfoOpts`).
- `session`: `Session`, the sans-I/O signalling session of a voice client with one peer
  connection. Feed it the server's `SignalResponse`s and the time (`handle_response`,
  `handle_timeout`); take the `SignalRequest`s to send (`poll_request`) and the
  `SessionEvent`s to act on (`poll_event`). It keeps the ping schedule and round-trip time, the
  roster of remote participants and their tracks, and the leave and refusal rules.
- `e2ee`: LiveKit's AES-GCM frame format (`LkGcmV1`, `FrameKey`, `SenderRing`),
  byte-compatible with the LiveKit client SDKs, and `ProductionConfig`: the end-to-end
  encryption settings every client of a Sew call must share (frame format, key ring size, key
  derivation and its salt, timings), whose one value is `e2ee::PRODUCTION`.
- `compat`: client-sdk-js rules the engine follows. `compare_versions` compares version strings
  the way the JS SDK does. `classify_validate` reads the answer of the media server's validate
  endpoint (which a client asks when its signalling connection is refused) and says why the
  connection failed, as the JS SDK would: not allowed, no v1 endpoint, or another error.

The protobuf types come from `sew-lkproto`.

**Not affiliated with or endorsed by LiveKit, Inc.**

- Source: <https://github.com/Alakazam-211/sew-crates> (read-only: it is generated from Sew's
  repository at each release, so pull requests are not merged there)
- API documentation: <https://docs.rs/sew-lk-compat>

## Usage

```toml
[dependencies]
sew-lk-compat = "0.8.2"
sew-lkproto = "0.8.2"
prost = "0.14"
```

A join starts from the media server's signalling URL and an access token. In a Sew call both
come from Sew's voice join answer (`url` and `token` of `sew-proto`'s `VoiceJoinResponse`). `JoinParams` turns them into the
`/rtc/v1` URL to open as a WebSocket; the token goes in that request's
`Authorization: Bearer` header, not in the URL. The session itself does no I/O: you pass it
each binary message the socket receives and send what it asks you to send.

```rust
use std::time::Instant;

use prost::Message;
use sew_lk_compat::{ClientInfoOpts, JoinParams, Session, SessionEvent};
use sew_lkproto::livekit::SignalResponse;

/// The WebSocket URL to open for a first join, and the session that will run on it.
fn start(url_from_sew: &str, token_from_sew: &str) -> (String, Session) {
    let params = JoinParams::new(token_from_sew, ClientInfoOpts::default());
    // For example "wss://voice.example.com/rtc/v1?join_request=..."; send
    // `Authorization: Bearer {params.token}` with the WebSocket upgrade.
    let ws_url = params.url(url_from_sew);
    (ws_url, Session::new(Instant::now()))
}

/// One binary message from the socket in; the binary messages to send back out.
fn on_message(session: &mut Session, bytes: &[u8]) -> Result<Vec<Vec<u8>>, prost::DecodeError> {
    let now = Instant::now();
    let response = SignalResponse::decode(bytes)?;
    session.handle_response(now, response);
    while let Some(event) = session.poll_event() {
        match event {
            SessionEvent::Joined { force_relay, .. } => {
                // Set up the peer connection (relay only when `force_relay`), then
                // session.publish_audio(..) and session.offer(..).
                println!("joined; relay only: {force_relay}");
            }
            SessionEvent::Leave { action, .. } => println!("asked to leave: {action:?}"),
            _ => {}
        }
    }
    let mut out = Vec::new();
    while let Some(request) = session.poll_request() {
        out.push(request.encode_to_vec());
    }
    Ok(out)
}

/// Call at `session.poll_timeout()` (keep-alive pings), then send what `poll_request` returns.
fn on_timer(session: &mut Session) {
    session.handle_timeout(Instant::now());
}
```

## Licence and origins

Apache-2.0. See `LICENSE` and `NOTICE`; `NOTICE` must travel with any binary that includes
this crate.

Two files are ported from LiveKit's client-sdk-js under the Apache License 2.0. Each keeps its
origin header and a "Modified by Alakazam Labs, 2026" line:

| File | Ported from (Apache-2.0) |
|---|---|
| `src/e2ee/lkgcm.rs` | livekit/client-sdk-js `src/e2ee/worker/FrameCryptor.ts`, `naluUtils.ts`, `src/e2ee/utils.ts`, `src/e2ee/constants.ts` @ 53dd840, (c) 2021 LiveKit, Inc. |
| `src/compat.rs` | livekit/client-sdk-js `src/room/utils.ts`, `src/api/SignalClient.ts` @ 53dd840, (c) 2021 LiveKit, Inc. |

`src/lib.rs` and `src/e2ee/mod.rs` are Alakazam Labs' own. `src/session/` and `src/signal/`
were rewritten from a written behaviour specification; they replace files that had been ported from livekit/livekit's test client
(`test/client/client.go`, `test/integration_helpers.go` @ 8d11efd), and `NOTICE` still names
livekit/livekit for that reason. No livekit/rust-sdks code is used.

`sew-call-client` (MIT OR Apache-2.0) depends on this crate and holds no LiveKit-derived code
itself. The crate as a whole is Apache-2.0 while it holds the two ported files.

Copyright 2026 Alakazam Labs.
