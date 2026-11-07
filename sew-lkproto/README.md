# sew-lkproto

sew-lkproto is the LiveKit client protocol (version 17) as Rust types, generated from livekit/protocol's `.proto` files, plus the protocol-version gates of Sew's media server.

Sew, Alakazam Labs' self-hosted team chat server, runs its voice and video calls on a media
server that speaks the LiveKit client protocol, so LiveKit's client SDKs and Sew's own native
engine can join its calls. This crate is that protocol's wire format for Rust:

- `livekit` and `logger`: the protobuf types of `livekit_rtc.proto`, `livekit_models.proto`,
  `livekit_metrics.proto` and `logger/options.proto`. The `.proto` files are copied
  byte-for-byte from livekit/protocol and ship in `proto/`. The Rust in `src/gen/` is generated
  from them with prost and pbjson (the binary encoding and LiveKit's JSON; unknown JSON fields
  are ignored), without `protoc`, and is not edited by hand.
- `PROTOCOL_COMMIT`: the livekit/protocol commit the `.proto` files were copied from
  (`a4f4b5c0c23f167d0c07de1e1d7652125882e73c`), so you can compare them with upstream.
- `version`: the protocol-version gates (`ProtocolVersion`, `CURRENT_PROTOCOL` = 17,
  `MIN_CLIENT_PROTOCOL` = 13, `ProtocolVersion::negotiate`) with the gate table as data
  (`version::GATES`). This module was rewritten from a written behaviour specification.
  `ProtocolVersion`, `BelowFloor`, `CURRENT_PROTOCOL` and `MIN_CLIENT_PROTOCOL` are also
  re-exported at the crate root, which is where the example below imports them from.

The generated code has no `unsafe`.

Not affiliated with or endorsed by LiveKit.

- Source: <https://github.com/Alakazam-211/sew-crates> (read-only: it is generated from Sew's
  repository at each release, so pull requests are not merged there)
- API documentation: <https://docs.rs/sew-lkproto>

## Usage

```toml
[dependencies]
sew-lkproto = "0.8.2"
prost = "0.14"
```

```rust
use prost::Message;
use sew_lkproto::livekit::SignalResponse;
use sew_lkproto::{BelowFloor, ProtocolVersion};

/// One binary message from the media server's signalling socket.
fn read(bytes: &[u8]) -> Result<SignalResponse, prost::DecodeError> {
    SignalResponse::decode(bytes)
}

/// The protocol a session runs at: the client's declared number, capped at 17.
fn version(declared: i32) -> Result<ProtocolVersion, BelowFloor> {
    ProtocolVersion::negotiate(declared)
}
```

`ProtocolVersion::negotiate` refuses a client older than protocol 13 with `BelowFloor`, an
error that holds the number the client declared (`declared`). A `ProtocolVersion` then answers
feature questions, for example `supports_packet_trailer()`, so callers do not compare numbers
themselves.

## Licence

Apache-2.0. See `LICENSE` and `NOTICE`. The `.proto` files and the code generated from them are
Copyright 2023 LiveKit, Inc., under the Apache License 2.0; `NOTICE` must travel with any binary
that includes this crate.

Copyright 2026 Alakazam Labs.
