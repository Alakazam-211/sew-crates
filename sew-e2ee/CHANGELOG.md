# Changelog

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
