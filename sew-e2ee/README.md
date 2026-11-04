sew-e2ee is the client library for end-to-end encrypted calls on Sew, a self-hosted team chat server: MLS (RFC 9420) key agreement per call, and the per-sender media frame keys derived from it.

# sew-e2ee

Every Sew call is end-to-end encrypted. The keys exist only on the participants' devices (and,
when a call is recorded, on the recorder the members admit; see below): they agree on them with
MLS (through `openmls` 0.9.0) and feed them to the media layer's frame encryption, which uses
the AES-GCM frame format of the LiveKit client SDKs. This crate is that protocol, written once
for every client:

- **Identity** (`identity`, `trust`, `store`): the device key interface (`DeviceSigner`, P-256),
  device certificates from the server's call CA, the call credential, the server-signed leaf
  binding table, trust-on-first-use pins and the revocation list.
- **Joining** (`E2eeClient`, `PendingKp`, `PreparedKp`): key packages made ahead of time and
  claimed by the join.
- **The call** (`CallSession`): a sans-I/O state machine. Per-sender keys come from the MLS
  exporter by leaf index into a 64-slot key ring; a key change is installed, made ready and
  then activated by every member; old keys retire after 10 s; a member that falls behind
  recovers without user action.
- **Bridges** (`bridge`): the `KeyEvent` contract an SDK bridge applies, its `KeyRing` shadow
  and the capabilities (`Caps`) a bridge reports.
- **The recorder side** (`recorder`, `open_recording_chunk`): how a call recorder receives the
  keys it is allowed, and how an admin opens a recording chunk.

The crate does no I/O, reads no clock and starts no threads: the host feeds it inputs (server
answers, gateway events, the time) and carries out its outputs. It builds for Linux, macOS, Windows
and wasm32 (for browsers).

The `testing` feature adds an in-process model of the delivery service and a simulated SDK for
tests. It is never for a release build.

Not affiliated with or endorsed by LiveKit.

## Usage

The crate is not on crates.io yet. It is published in the read-only
[sew-crates](https://github.com/Alakazam-211/sew-crates) repository (clone it, for example, as
`third_party/sew-crates`); point Cargo at it, and at the Sew crates it depends on, from your workspace's
root `Cargo.toml`:

```toml
[dependencies]
sew-e2ee = "0.7.2"

[patch.crates-io]
sew-e2ee = { path = "third_party/sew-crates/sew-e2ee" }
sew-proto = { path = "third_party/sew-crates/sew-proto" }
sew-mls-crypto = { path = "third_party/sew-crates/sew-mls-crypto" }
```

A client enrols its device (`identity`), builds an `E2eeClient` from the device identity and
the server's trust anchors, prepares a key package before it joins, and then drives one
`CallSession` per call: it hands the session each `Input` and applies each `Output` (key
requests to send, `KeyEvent`s for its media SDK, status changes). The native voice engine
`sew-call-client` is a complete host of this crate.

## Known advisory: P-256 (RUSTSEC-2026-0387)

The device key signs with ECDSA P-256 through `p256` 0.13 (`identity`'s `SoftwareSigner`) on
every join, whatever MLS ciphersuite the call uses, so choosing a suite does not avoid this path.
`p256` 0.13 links `crypto-bigint` 0.5.5, which RUSTSEC-2026-0387 (crypto-bigint < 0.7.6) names,
and dependency scanners report it. Our assessment, from source review only, is that the signing
path does not call the operations the advisory names; we have not yet run `cargo checkct` (a tool
that checks compiled code for secret-dependent timing). The fix (crypto-bigint 0.7.9+) needs
`p256` 0.14, a RustCrypto upgrade we plan to make. `sew-mls-crypto`'s README covers the MLS side.

## Licence

MIT OR Apache-2.0, at your option. See `LICENSE-MIT`, `LICENSE-APACHE` and `NOTICE`.

Copyright 2026 Alakazam Labs.
