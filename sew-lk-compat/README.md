sew-lk-compat is the LiveKit-protocol layer of Sew's native voice engine: the `/rtc/v1` join request, a sans-I/O signalling session for a single-peer-connection voice client, and LiveKit's AES-GCM media frame format.

# sew-lk-compat

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
- `e2ee`: LiveKit's AES-GCM frame format (`LkGcmV1`, `FrameKey`, `SenderRing`) and the
  production E2EE configuration (`ProductionConfig`), byte-compatible with the LiveKit client
  SDKs.
- `compat`: client-sdk-js rules the engine follows (`compare_versions`, `classify_validate`).

The protobuf types come from `sew-lkproto`.

**Not affiliated with or endorsed by LiveKit, Inc.**

## Usage

The crate is not on crates.io yet. It is published in the read-only
[sew-crates](https://github.com/Alakazam-211/sew-crates) repository (clone it, for example, as
`third_party/sew-crates`); point Cargo at it, and at `sew-lkproto`, from your workspace's root
`Cargo.toml`:

```toml
[dependencies]
sew-lk-compat = "0.7.3"

[patch.crates-io]
sew-lk-compat = { path = "third_party/sew-crates/sew-lk-compat" }
sew-lkproto = { path = "third_party/sew-crates/sew-lkproto" }
```

```rust
use std::time::Instant;
use sew_lk_compat::{Session, SessionEvent};

let mut session = Session::new(Instant::now());
// For each SignalResponse from the socket: session.handle_response(now, response);
// then send every session.poll_request() and act on every session.poll_event().
while let Some(event) = session.poll_event() {
    if let SessionEvent::Joined { .. } = event {
        // media can start
    }
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
