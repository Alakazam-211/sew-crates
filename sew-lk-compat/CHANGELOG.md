# Changelog

## 0.11.7 (2026-11-28)

- The version follows Sew 0.11.7. No API or behaviour change.

## 0.11.6 (2026-11-25)

- The version follows Sew 0.11.6. No API or behaviour change.

## 0.11.5 (2026-11-25)

- The version follows Sew 0.11.5. No API or behaviour change.

## 0.11.4 (2026-11-22)

- The version follows Sew 0.11.4. No API or behaviour change.

## 0.11.3 (2026-11-21)

- The version follows Sew 0.11.3. No API or behaviour change.

## 0.11.2 (2026-11-20)

- The version follows Sew 0.11.2. No API or behaviour change.

## 0.11.1 (2026-11-20)

- The version follows Sew 0.11.1. No API or behaviour change.

## 0.11.0 (2026-11-19)

### Breaking

- **`JoinParams` and `ClientInfoOpts` are `#[non_exhaustive]`**, so later fields are not
  breaking changes. Outside this crate they are built with constructors instead of struct
  literals; their fields stay public, so set the others on the value the constructor returns.
  - `JoinParams::new(token, client_info)` as before: a first join (auto-subscribe on, not a
    resume, the token kept out of the URL).
  - `ClientInfoOpts::new()` or `ClientInfoOpts::default()`: the Rust SDK family, an empty
    version, the current protocol and this OS.

### Dependencies

- Requires sew-lkproto 0.11.0 (its own `Timestamp`); see sew-lkproto's CHANGELOG.

## 0.10.6 (2026-11-19)

- The version follows Sew 0.10.6. No API or behaviour change.

## 0.10.5 (2026-11-18)

- The version follows Sew 0.10.5. No API or behaviour change.

## 0.10.4 (2026-11-17)

- The version follows Sew 0.10.4. No API or behaviour change. The package's metadata and
  README follow the published-crate standard (issues, MSRV, features, a compiling example that
  runs as a doc test).

## 0.10.3 (2026-11-16)

- The version follows Sew 0.10.3. No API or behaviour change.

## 0.10.2 (2026-11-16)

- The version follows Sew 0.10.2 (there is no 0.10.1). No API or behaviour change.

## 0.10.0 (2026-11-15)

- The version follows Sew 0.10.0. No API or behaviour change.

## 0.9.3 (2026-11-12)

- The version follows Sew 0.9.3. No API or behaviour change.

## 0.9.2 (2026-11-11)

- The version follows Sew 0.9.2. No API or behaviour change.

## 0.9.0 (2026-11-10)

- The version follows Sew 0.9.0. No API or behaviour change.

## 0.8.3 (2026-11-10)

- The version follows Sew 0.8.3. No API or behaviour change.

## 0.8.2 (2026-11-06)

- `Session::add_track` sends a caller-built `AddTrackRequest` (a video track's layers and
  simulcast codecs); new events `SessionEvent::SubscribedQuality` (the server's
  `SubscribedQualityUpdate`, for dynacast) and `SessionEvent::TrackUnpublished`.
- `SessionEvent::StreamStates`: the server's `StreamStateUpdate` (a track paused or resumed for
  this client, as a congested downlink pauses video) is passed on instead of dropped. Additive
  (`SessionEvent` is non-exhaustive).

## 0.8.1 (2026-11-06)

- The version follows Sew 0.8.1. No API or behaviour change.

## 0.8.0 (2026-11-05)

- The version follows Sew 0.8.0. The README is revised. No API or behaviour change.

## 0.7.4 (2026-11-05)

- `rust-crypto` feature (phones): LkGcm's AES-128-GCM and HKDF-SHA256 on RustCrypto
  (`aes-gcm`, `hkdf`, `sha2`) instead of aws-lc-rs; the same frames, byte for byte (a
  known-answer test pins both). aws-lc-rs is no longer built for iOS and Android, where the crate
  refuses to compile without `rust-crypto` (sew-call-client's `mobile` turns it on).

## 0.7.3 (2026-11-04)

- The version follows Sew 0.7.3. No API or behaviour change.

## 0.7.2 (2026-11-03)

- First published version.
- `signal`: the `/rtc/v1` join request, `ClientInfo` and ICE trickles.
- `session`: the sans-I/O signalling session of a single-peer-connection voice client (ping and
  round-trip time, the remote roster, leave and refusal handling). `signal` and `session` were
  rewritten from a written behaviour specification.
- `e2ee`: LiveKit's AES-GCM frame format and the production E2EE configuration; `compat`:
  client-sdk-js version comparison and the validate classifier. The frame format and `compat` are
  ported from livekit/client-sdk-js @ 53dd840.
- Public enums and read-only structs are `#[non_exhaustive]`.
- Licence: Apache-2.0, with LiveKit's NOTICE.
