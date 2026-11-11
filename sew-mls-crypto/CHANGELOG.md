# Changelog

## 0.9.2 (2026-11-11)

- The version follows Sew 0.9.2. No API or behaviour change.

## 0.9.0 (2026-11-10)

- The version follows Sew 0.9.0. The first release on crates.io (0.7.2 to 0.8.3 were in the
  sew-crates mirror only).
- The `rfc9180-vectors` feature is gone. It only compiled a test of Sew's repository, whose
  vectors are not in the package, so turning it on broke the build; the test now runs in Sew's
  repository without a feature.
- `SignatureKeyPair` (and its `StorageId`): the signature key pair of a basic credential,
  replacing `openmls_basic_credential`'s with the same API (`new`, `from_raw`, `public`,
  `to_public_vec`, `signature_scheme`, `id`, `store`, `read`, `delete`, `Signer`) and the same
  key, signature, serde, TLS and storage formats, for Ed25519 and ECDSA P-256 (P-384 is refused,
  as the provider refuses it). Key pairs and groups made with either work with the other.
- On RustCrypto's current line: `p256` 0.14 and `hpke` 0.15.1 (both on `crypto-bigint` 0.7). The
  crate no longer links `crypto-bigint` 0.5.5 (RUSTSEC-2026-0387). Signatures, public keys, HPKE
  ciphertexts and exporters are unchanged, and the RFC 9180 vectors still pass.
- `derive_hpke_keypair` now returns clamped X25519 private keys (RFC 9180 §7.1.2); public keys,
  ciphertexts and exporters are unchanged. Before, it returned the unclamped bytes; it is the
  same key, and either form opens the same ciphertexts.

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
- `MlsCrypto`, an implementation of `openmls_traits` 0.6's `OpenMlsCrypto` and `OpenMlsRand` over
  RustCrypto crates, with HPKE (RFC 9180, base mode) from the `hpke` crate 0.13.0.
- `MlsProvider`, `MlsCrypto` with `openmls_memory_storage`'s `MemoryStorage`, as an OpenMLS
  provider.
- Ciphersuites 0x0001, 0x0002 and 0x0003, with the behaviour of `openmls_rust_crypto` 0.6.0 for
  each.
- Builds for Linux, macOS, Windows and wasm32.
- Licence: MIT OR Apache-2.0. `src/crypto.rs` is derived from `openmls_rust_crypto` 0.6.0 (MIT);
  `NOTICE` carries its licence.
