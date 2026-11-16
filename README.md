# sew-crates

The open Rust crates of Sew, Alakazam Labs' team chat server: its wire contracts, the end-to-end
encrypted call keys, the LiveKit-protocol pieces, the native voice engine and its Tauri 2
plugin, and the microphone noise removal. Each directory is one crate as `cargo package` ships it, with its own licence files.

## Getting the code

This repository, <https://github.com/Alakazam-211/sew-crates>, is read-only: it is generated from Sew's private
repository at each Sew release, so pull requests are not merged here. Clone it, or download a
release's tree, into your own project, for example as `third_party/sew-crates` (the paths
below assume that place; any directory works):

```sh
git clone https://github.com/Alakazam-211/sew-crates third_party/sew-crates
```

Security problems: see [SECURITY.md](SECURITY.md) (private reporting only).

## What you need: a Sew server

These crates are clients. They talk to a Sew server (the chat server: accounts, channels,
voice calls and their key delivery service) and to its media server; without one there is
nothing to join. A Sew server runs on a K2 box (K2 is Alakazam Labs' workspace platform, where
Sew is its Chat / Voice Hosting) or on its own on Linux. The server is not published: it is not
in this repository or anywhere else public.

## Start here

| You are building | Use | It brings in |
|---|---|---|
| Voice calls in a Tauri 2 desktop app | `tauri-plugin-sew-voice` | `sew-call-client` and everything below it |
| Voice calls in another native Rust app | `sew-call-client` | `sew-e2ee`, `sew-lk-compat`, `sew-lkproto`, `sew-proto`, `sew-noise`, `kessel-audio`, `kessel-video` |
| Your own end-to-end encrypted call client (keys only, no media) | `sew-e2ee` | `sew-mls-crypto`, `sew-proto` |
| An OpenMLS crypto provider on permissive crates | `sew-mls-crypto` | nothing of Sew's |
| Typed Sew API and gateway messages (HTTP bodies, ids, errors) | `sew-proto` | nothing of Sew's |
| LiveKit client-protocol types or signalling for Sew's media server | `sew-lkproto`, `sew-lk-compat` | `sew-lkproto` (for `sew-lk-compat`) |
| Microphone noise removal for a real-time audio path | `sew-noise` | nothing of Sew's (`nnnoiseless`, from crates.io) |

How they depend on each other:

```text
tauri-plugin-sew-voice
└── sew-call-client ── kessel-audio, kessel-video (K2's, from crates.io)
    ├── sew-e2ee ── sew-mls-crypto
    │   └── sew-proto
    ├── sew-lk-compat ── sew-lkproto
    ├── sew-noise ── nnnoiseless (from crates.io)
    └── sew-proto
```

`sew-call-client`'s `JoinProvider` documentation has a complete, compiled example: a provider
that signs in to Sew with your app's own session, and a call through it.

## Crates and licences

| Crate | Version | Licence |
|---|---|---|
| `sew-proto` | 0.10.3 | MIT OR Apache-2.0 |
| `sew-lkproto` | 0.10.3 | Apache-2.0 |
| `sew-mls-crypto` | 0.10.3 | MIT OR Apache-2.0 |
| `sew-e2ee` | 0.10.3 | MIT OR Apache-2.0 |
| `sew-lk-compat` | 0.10.3 | Apache-2.0 |
| `sew-noise` | 0.10.3 | MIT OR Apache-2.0 |
| `sew-call-client` | 0.8.1 | MIT OR Apache-2.0 |
| `tauri-plugin-sew-voice` | 0.8.1 | MIT OR Apache-2.0 |

Sew's own crates are `MIT OR Apache-2.0`, at your option. `sew-lk-compat` and `sew-lkproto`
are `Apache-2.0` only, because they hold code ported from or generated from LiveKit, which is
Apache-2.0; their NOTICE files must travel with any binary that includes them. Not affiliated
with or endorsed by LiveKit, Inc.

`sew-noise` runs the RNNoise model through `nnnoiseless` (BSD-3-Clause), whose notice is in
`sew-noise/NOTICE` and must travel with any binary that includes it;
`sew-noise/PROVENANCE.md` records where the model comes from.

`LICENSE-APACHE` and `LICENSE-MIT` at the root are the texts of those licences. Each crate's
own licence (its `Cargo.toml` `license` field and the licence files in its directory) governs
that crate.

## Using the crates

They depend on each other by version. `sew-proto`, `sew-lkproto`, `sew-mls-crypto`,
`sew-e2ee`, `sew-lk-compat` and `sew-noise` are published on crates.io as well, so on their
own they need only a dependency line. `sew-call-client` and `tauri-plugin-sew-voice` are not
on crates.io yet. To use
them from this directory, in your workspace's root
`Cargo.toml`, add this one `[patch.crates-io]` block (a `[patch]` applies only from the
workspace root; if you already have one, add these rows to it, since a second block is a TOML
error). It points each crate at its directory:

```toml
[patch.crates-io]
sew-proto = { path = "third_party/sew-crates/sew-proto" }
sew-lkproto = { path = "third_party/sew-crates/sew-lkproto" }
sew-mls-crypto = { path = "third_party/sew-crates/sew-mls-crypto" }
sew-e2ee = { path = "third_party/sew-crates/sew-e2ee" }
sew-lk-compat = { path = "third_party/sew-crates/sew-lk-compat" }
sew-call-client = { path = "third_party/sew-crates/sew-call-client" }
tauri-plugin-sew-voice = { path = "third_party/sew-crates/tauri-plugin-sew-voice" }
sew-noise = { path = "third_party/sew-crates/sew-noise" }
```

The voice engine's WebRTC stack is `sew-str0m`, Alakazam Labs' public fork of `str0m` 0.25
(library name `str0m`). It comes from crates.io (`sew-str0m` 0.25.3), with its own fork of
`is`, `sew-str0m-is`, so it needs no row; the other str0m crates and its DTLS, `dimpl`,
come from crates.io too.

**kessel-audio** and **kessel-video** (K2's audio and video pipelines, MIT OR Apache-2.0) come
from crates.io and need no row.

## Building and testing this directory

`Cargo.toml` here is a workspace of the eight crates with those rows filled in, and
`Cargo.lock` is the lock this release was resolved and tested with (`cargo test --workspace`
and `cargo doc`, on Linux). So, for example,
`cargo build --locked -p sew-call-client --no-default-features --features audio-core` builds from
this directory alone. The voice engine's default `audio` feature needs ALSA's development files
on Linux, and the plugin's default `tauri` feature needs Tauri's (webkit2gtk-4.1, gtk3,
libsoup3). `deny.toml` is the cargo-deny configuration this release was checked with, and
`clippy.toml` the clippy configuration (`cargo clippy --workspace --all-targets -- -D warnings`
passes with it).

## Guides

`docs/native-voice-sdk.md` is the guide to the native voice engine and the Tauri plugin (the
webview API, video, device keys, testing an app without Sew), and `docs/tauri-app-sdk.md` the
checklist for a Tauri App shell on a Sew server. The crates' READMEs cite them by those paths.
They are written for Sew's own source tree as well, so some of the paths they name (Sew's
scripts and examples) are not in this directory.
