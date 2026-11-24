# Changelog

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

- **More public types are `#[non_exhaustive]`**, so later additions are not breaking changes.
  Outside this crate, a `match` on one of these enums needs a wildcard arm, and these structs
  are built with their constructors instead of struct literals (their fields stay public; set
  them on the value the constructor returns).
  - The session enums: `Input`, `Output`, `E2eeStatus`, `SessionState`, `Transition`,
    `LeftReason`, `UndecryptReason` and `EngineMisbehaviour`.
  - The error enums: `DecodeError`, `TrustError`, `IdentityError`, `SessionError`,
    `JoinError`, `ReleaseError`, `BindingError`, `ChunkError`, `RecorderCertError` and
    `LeafCheckError`.
  - `Counters`: `Counters::new()` or `Counters::default()` (every count 0).
  - `E2eeConfig`: `E2eeConfig::new()` or `E2eeConfig::default()` (the default timings and
    limits).
  - `ApiFailure`: `ApiFailure::new(status, reason)` and `ApiFailure::transport()` as before,
    and the new `ApiFailure::from_status(status)` (no reason, detail or retry delay) with the
    setters `with_reason`, `with_detail` and `with_retry_after_ms`.
- **Renamed:** the enum `EngineAttack` is `EngineMisbehaviour` (same variants:
  `UnencryptedTrack`, `SifTrailer`, `EncryptionDowngrade`), and the session input
  `Input::EngineAttack { kind, identity }` is `Input::EngineMisbehaviour { kind, identity }`.

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

- The version follows Sew 0.9.0. The first release on crates.io (0.7.2 to 0.8.3 were in the
  sew-crates mirror only).
- **Breaking:** `bridge::virtual_sdk` (`VirtualSdk`, `Flavour`, `VirtualFrame`) is now behind the
  `testing` feature, like the rest of the test harness. The `testing` feature is for tests only:
  unstable, not covered by semver, it exposes secret key material and builds malicious protocol
  messages.
- `SoftwareSigner`'s documentation says what it is: a software key held in process memory, not
  hardware-backed (no API change).
- The MLS leaf key pairs are `sew_mls_crypto::SignatureKeyPair`; `openmls_basic_credential` is no
  longer a dependency. With the `testing` feature, `MiniDs::ds_signer` and `rogue::craft_leaf`
  return that type instead of `openmls_basic_credential::SignatureKeyPair` (the same methods).
- The device key and certificate checks are on `p256` 0.14. The crate no longer links
  `crypto-bigint` 0.5.5 (RUSTSEC-2026-0387). Device signatures, key packages and every frozen
  vector are unchanged.

## 0.8.3 (2026-11-10)

- The version follows Sew 0.8.3. No API or behaviour change.

## 0.8.2 (2026-11-06)

- The version follows Sew 0.8.2. No API or behaviour change.

## 0.8.1 (2026-11-06)

- The version follows Sew 0.8.1. No API or behaviour change.

## 0.8.0 (2026-11-05)

- The version follows Sew 0.8.0. The README is revised. No API or behaviour change.

## 0.7.4 (2026-11-05)

- The version follows Sew 0.7.4. The README is revised. No API or behaviour change.

## 0.7.3 (2026-11-04)

- The version follows Sew 0.7.3. No API or behaviour change.

## 0.7.2 (2026-11-03)

- First mirror release.
- The client side of Sew's end-to-end encrypted calls: device identity and certificates, trust
  anchors and pins, key packages, the per-call MLS session (`CallSession`, openmls 0.9.0 with
  `sew-mls-crypto`), per-sender frame keys in a 64-slot ring with a two-phase key switch,
  recovery, the recorder key handoff and recording-chunk opening, and the key-event contract for
  SDK bridges.
- Sans-I/O; builds for Linux, macOS and wasm32.
- Feature `testing`: an in-process delivery-service model and a simulated SDK, for tests only.
- Licence: MIT OR Apache-2.0.
