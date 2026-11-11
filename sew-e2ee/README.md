# sew-e2ee

sew-e2ee is the client library for end-to-end encrypted calls on Sew, a self-hosted team chat server: MLS (RFC 9420) key agreement per call, and the per-sender media frame keys derived from it.

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
answers, gateway events, the time) and carries out its outputs. It builds for Linux, macOS,
Windows and wasm32 (for browsers); Sew's tests run on Linux and wasm32, its builds cover
macOS, and a compile check covers Windows.

Not affiliated with or endorsed by LiveKit.

- Source: <https://github.com/Alakazam-211/sew-crates> (read-only: it is generated from Sew's
  repository at each release, so pull requests are not merged there)
- API documentation: <https://docs.rs/sew-e2ee>
- Security problems: report them privately, as
  <https://github.com/Alakazam-211/sew-crates/security/policy> says.

## Terms

- **DS** (delivery service): the Sew server's side of each call's MLS group. Its key signs the
  leaf binding table, and it is the group's only external sender (it proposes adds and removes).
- **Key package**: an MLS key package for one device, made and signed ahead of time, uploaded,
  and claimed once by a join.
- **READY**: the Sew gateway's first event on a connection; its `call_keys` carry the install's
  trust anchors (CA key, DS key, `external_sender`, revocation list, parameters).
- **Frame key ring**: the 64 key slots per sender that the media SDK holds. Each MLS epoch's
  frame key goes to slot `(slot_base + epoch) mod 64`, and old keys are retired after 10 s.

## Security model and limits

- **The server is the certificate authority.** The Sew server's call CA issues the device
  certificates, and its DS decides who is added to and removed from a call. A malicious or
  compromised server can therefore issue a certificate for a device it controls under a real
  member's name and add it to a call. This crate does not defend against that. The 30-digit
  security code (`SecurityCode`, from the epoch authenticator) lets participants confirm out of
  band that they are in the same group epoch; it does not by itself show who is in it.
- **What end-to-end encryption protects against:** the media server and anyone on the network
  never get frame keys, unless the server adds a device of its own (see above), so they cannot
  decrypt the media they forward. A call recorder gets only the keys that consenting members
  seal to it.
- **What it does not hide (metadata):** the Sew server and the media server still see who joins
  which call and when, device identities and group membership, and packet sizes and timing. The
  LiveKit frame format also leaves the first bytes of each frame's codec header unencrypted.
- **Trust on first use.** The first READY a device sees for an install pins the CA key, the DS
  key and the `external_sender` bytes in its `DeviceStore`. A later change is a hard error
  (`E2eeStatus::ServerIdentityChanged`: no joins, and a report for the admins). That catches a
  later swap of the server's call keys. It does not help if the first READY is already an
  attacker's, if the attacker holds the server's real CA and DS keys, or after the device store is
  cleared (a new enrolment accepts new keys). Transport security (TLS to the server) is the
  host's job.
- **What is tested** (the package carries only unit tests; the full suites are in Sew's
  repository): device certificates, pins and revocation; the call credential; Welcome and
  commit checks, including crafted Welcomes, wrong external senders and disallowed proposals;
  the two-phase key switch, retirement and recovery in a multi-member simulation; the recorder
  key handoff; that no key material shows in `Debug` output or logs; the RFC 9420 known-answer
  vectors (passive client and codecs); the MLS exporter against another implementation
  (`mls-rs`); and the fuzz entry points, fuzzed for panics. The HPKE provider,
  `sew-mls-crypto`, is tested against the RFC 9180 vectors and against `openmls_rust_crypto`.
- **No third-party audit.** Neither this crate nor `sew-mls-crypto` has had a third-party
  security audit.

## The `testing` feature

`testing` adds an in-process model of the delivery service, a simulated SDK
(`bridge::virtual_sdk`), test certificates and keys, and crafted protocol messages for negative
tests. **It is unstable and not covered by semver, it exposes secret key material, and it builds
malicious protocol messages. Never enable it outside tests.**

## Usage

```toml
[dependencies]
sew-e2ee = "0.9.2"
```

Most apps do not use this crate directly: they use it through the native voice engine,
`sew-call-client` (in the [sew-crates](https://github.com/Alakazam-211/sew-crates) repository;
not on crates.io yet), which is a complete host of it. Its `JoinProvider` documentation has a
compiled example of a call.

A host of its own enrols its device (`identity`), builds an `E2eeClient` from the device
identity and the server's trust anchors (`TrustAnchors`, from READY), prepares a key package
before it joins, and then drives one `CallSession` per call: it hands the session each `Input`
and applies each `Output` (key requests to send, `KeyEvent`s for its media SDK, status changes).

## Licence

MIT OR Apache-2.0, at your option. See `LICENSE-MIT`, `LICENSE-APACHE` and `NOTICE`.

Copyright 2026 Alakazam Labs.
