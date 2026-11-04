sew-proto is the wire contract of Sew, a self-hosted team chat server: the Rust types of every JSON body its HTTP API and realtime gateway send and receive.

# sew-proto

Sew is Alakazam Labs' chat server for companies that run their own (channels, threads, direct
messages, spaces with roles and permissions, voice channels, search, retention, legal hold).
This crate is the one place its wire JSON is defined; the server, its clients and the apps
built on it share it.

What it holds:

- ids and chat addresses;
- errors and their codes;
- HTTP request and response bodies, and the route paths;
- gateway frames and events (the realtime websocket);
- change-log kinds, permission bits, organisation policies and audit events;
- the management API bodies;
- the JSON Schema of every type, in `schema/` (generated with `schemars`).

The crate is pure: apart from the clock in `clock`, it does no I/O. Its dependencies are
`serde`, `serde_json`, `schemars` and `chrono`. It builds on Linux and macOS.

## Usage

The crate is not on crates.io yet. It is published in the `sew-crates` mirror, a directory of
packaged crates; point Cargo at it from your workspace's root `Cargo.toml`:

```toml
[dependencies]
sew-proto = "0.7.2"

[patch.crates-io]
sew-proto = { path = "third_party/sew-crates/sew-proto" }
```

Reading an error answer:

```rust
use sew_proto::error::{Code, ErrorBody};

fn session_expired(body: &str) -> bool {
    serde_json::from_str::<ErrorBody>(body).is_ok_and(|b| b.error.code == Code::SessionExpired)
}
```

## Licence

MIT OR Apache-2.0, at your option. See `LICENSE-MIT`, `LICENSE-APACHE` and `NOTICE`.

Copyright 2026 Alakazam Labs.
