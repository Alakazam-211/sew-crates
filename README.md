# sew-crates

The open Rust crates of Sew, Alakazam Labs' team chat server: its wire contracts, the end-to-end
encrypted call keys, the LiveKit-protocol pieces, the native voice engine and its Tauri 2
plugin. Each directory is one crate as `cargo package` ships it, with its own licence files.

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
| Voice calls in another native Rust app | `sew-call-client` | `sew-e2ee`, `sew-lk-compat`, `sew-lkproto`, `sew-proto`, `kessel-audio`, `kessel-video` |
| Your own end-to-end encrypted call client (keys only, no media) | `sew-e2ee` | `sew-mls-crypto`, `sew-proto` |
| An OpenMLS crypto provider on permissive crates | `sew-mls-crypto` | nothing of Sew's |
| Typed Sew API and gateway messages (HTTP bodies, ids, errors) | `sew-proto` | nothing of Sew's |
| LiveKit client-protocol types or signalling for Sew's media server | `sew-lkproto`, `sew-lk-compat` | `sew-lkproto` (for `sew-lk-compat`) |

How they depend on each other:

```text
tauri-plugin-sew-voice
└── sew-call-client ── kessel-audio, kessel-video (K2's, from crates.io)
    ├── sew-e2ee ── sew-mls-crypto
    │   └── sew-proto
    ├── sew-lk-compat ── sew-lkproto
    └── sew-proto
```

`sew-call-client`'s `JoinProvider` documentation has a complete, compiled example: a provider
that signs in to Sew with your app's own session, and a call through it.

## Crates and licences

| Crate | Version | Licence |
|---|---|---|
| `sew-proto` | 0.9.0 | MIT OR Apache-2.0 |
| `sew-lkproto` | 0.9.0 | Apache-2.0 |
| `sew-mls-crypto` | 0.9.0 | MIT OR Apache-2.0 |
| `sew-e2ee` | 0.9.0 | MIT OR Apache-2.0 |
| `sew-lk-compat` | 0.9.0 | Apache-2.0 |
| `sew-call-client` | 0.6.0 | MIT OR Apache-2.0 |
| `tauri-plugin-sew-voice` | 0.6.0 | MIT OR Apache-2.0 |

Sew's own crates are `MIT OR Apache-2.0`, at your option. `sew-lk-compat` and `sew-lkproto`
are `Apache-2.0` only, because they hold code ported from or generated from LiveKit, which is
Apache-2.0; their NOTICE files must travel with any binary that includes them. Not affiliated
with or endorsed by LiveKit, Inc.

`LICENSE-APACHE` and `LICENSE-MIT` at the root are the texts of those licences. Each crate's
own licence (its `Cargo.toml` `license` field and the licence files in its directory) governs
that crate.

## Using the crates

They depend on each other by version. `sew-proto`, `sew-lkproto`, `sew-mls-crypto`,
`sew-e2ee` and `sew-lk-compat` are published on crates.io as well, so on their own they need
only a dependency line. `sew-call-client` and `tauri-plugin-sew-voice` are not on crates.io
yet (they also need the forks below). To use
them from this directory, in your workspace's root
`Cargo.toml`, add this one `[patch.crates-io]` block (a `[patch]` applies only from the
workspace root; if you already have one, add these rows to it, since a second block is a TOML
error). It points each crate at its directory and adds the forked dependencies:

```toml
[patch.crates-io]
sew-proto = { path = "third_party/sew-crates/sew-proto" }
sew-lkproto = { path = "third_party/sew-crates/sew-lkproto" }
sew-mls-crypto = { path = "third_party/sew-crates/sew-mls-crypto" }
sew-e2ee = { path = "third_party/sew-crates/sew-e2ee" }
sew-lk-compat = { path = "third_party/sew-crates/sew-lk-compat" }
sew-call-client = { path = "third_party/sew-crates/sew-call-client" }
tauri-plugin-sew-voice = { path = "third_party/sew-crates/tauri-plugin-sew-voice" }
str0m           = { git = "https://github.com/Alakazam-211/str0m", rev = "907e050bfa212a23394f4482110acef04cba8fc6" }
str0m-proto     = { git = "https://github.com/Alakazam-211/str0m", rev = "907e050bfa212a23394f4482110acef04cba8fc6" }
is              = { git = "https://github.com/Alakazam-211/str0m", rev = "907e050bfa212a23394f4482110acef04cba8fc6" }
str0m-aws-lc-rs = { git = "https://github.com/Alakazam-211/str0m", rev = "907e050bfa212a23394f4482110acef04cba8fc6" }
```

The `str0m`, `str0m-proto`, `is` and `str0m-aws-lc-rs` rows are Alakazam Labs' public
fork of `str0m` (the WebRTC stack; `is` is one of its crates), which the voice engine needs.
Its DTLS, `dimpl`, comes from crates.io.

**kessel-audio** and **kessel-video** (K2's audio and video pipelines, MIT OR Apache-2.0) come
from crates.io and need no row.

## Building and testing this directory

`Cargo.toml` here is a workspace of the seven crates with those rows filled in, and
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
