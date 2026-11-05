# Changelog

## 0.7.4 (2026-11-05)

- The version follows Sew 0.7.4. The README is revised. No API or behaviour change.

## 0.7.3 (2026-11-04)

- The version follows Sew 0.7.3. No API or behaviour change.

## 0.7.2 (2026-11-03)

- First published version.
- `MlsCrypto`, an implementation of `openmls_traits` 0.6's `OpenMlsCrypto` and `OpenMlsRand` over
  RustCrypto crates, with HPKE (RFC 9180, base mode) from the `hpke` crate 0.13.0.
- `MlsProvider`, `MlsCrypto` with `openmls_memory_storage`'s `MemoryStorage`, as an OpenMLS
  provider.
- Ciphersuites 0x0001, 0x0002 and 0x0003, with the behaviour of `openmls_rust_crypto` 0.6.0 for
  each.
- Builds for Linux, macOS, Windows and wasm32.
- Licence: MIT OR Apache-2.0. `src/crypto.rs` is derived from `openmls_rust_crypto` 0.6.0 (MIT);
  `NOTICE` carries its licence.
