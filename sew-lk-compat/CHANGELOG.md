# Changelog

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
