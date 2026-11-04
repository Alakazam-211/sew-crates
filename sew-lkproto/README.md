sew-lkproto is the LiveKit client protocol (version 17) as Rust types, generated from livekit/protocol's `.proto` files, plus the protocol-version gates of Sew's media server.

# sew-lkproto

Sew, Alakazam Labs' self-hosted team chat server, runs its voice and video calls on a media
server that speaks the LiveKit client protocol, so LiveKit's client SDKs and Sew's own native
engine can join its calls. This crate is that protocol's wire format for Rust:

- `livekit` and `logger`: the protobuf types of `livekit_rtc.proto`, `livekit_models.proto`,
  `livekit_metrics.proto` and `logger/options.proto`. The `.proto` files are copied
  byte-for-byte from livekit/protocol at commit `a4f4b5c0c23f167d0c07de1e1d7652125882e73c`
  (`PROTOCOL_COMMIT`) and ship in `proto/`. The Rust in `src/gen/` is generated from them with
  prost and pbjson (the binary encoding and LiveKit's JSON; unknown JSON fields are ignored),
  without `protoc`, and is not edited by hand.
- `version`: the protocol-version gates (`ProtocolVersion`, `CURRENT_PROTOCOL` = 17,
  `MIN_CLIENT_PROTOCOL` = 13, `ProtocolVersion::negotiate`) with the gate table as data
  (`version::GATES`). This module was rewritten from a written behaviour specification.

The generated code has no `unsafe`.

Not affiliated with or endorsed by LiveKit.

## Usage

The crate is not on crates.io yet. It is published in the read-only
[sew-crates](https://github.com/Alakazam-211/sew-crates) repository (clone it, for example, as
`third_party/sew-crates`); point Cargo at it from your workspace's root `Cargo.toml`:

```toml
[dependencies]
sew-lkproto = "0.7.2"
prost = "0.14"

[patch.crates-io]
sew-lkproto = { path = "third_party/sew-crates/sew-lkproto" }
```

```rust
use prost::Message;
use sew_lkproto::livekit::SignalResponse;
use sew_lkproto::{BelowFloor, ProtocolVersion};

fn read(bytes: &[u8]) -> Result<SignalResponse, prost::DecodeError> {
    SignalResponse::decode(bytes)
}

// The version a client declared, capped at 17; below 13 is refused.
fn version(declared: i32) -> Result<ProtocolVersion, BelowFloor> {
    ProtocolVersion::negotiate(declared)
}
```

## Licence

Apache-2.0. See `LICENSE` and `NOTICE`. The `.proto` files and the code generated from them are
Copyright 2023 LiveKit, Inc., under the Apache License 2.0; `NOTICE` must travel with any binary
that includes this crate.

Copyright 2026 Alakazam Labs.
