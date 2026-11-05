# sew-proto

sew-proto is the wire contract of Sew, a self-hosted team chat server: the Rust types of every JSON body its HTTP API and realtime gateway send and receive.

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
`serde`, `serde_json`, `schemars` and `chrono`. It has no platform-specific code and works on
Linux, macOS and Windows (Sew's own builds cover Linux and macOS; a compile check covers
Windows).

- Source: <https://github.com/Alakazam-211/sew-crates> (read-only: it is generated from Sew's
  repository at each release, so pull requests are not merged there)
- API documentation: <https://docs.rs/sew-proto>

## Usage

```toml
[dependencies]
sew-proto = "0.7.4"
serde_json = "1" # for the example below
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
