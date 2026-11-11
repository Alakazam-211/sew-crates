# sew-mls-crypto

sew-mls-crypto is an OpenMLS crypto provider built only on permissively licensed crates: openmls_traits' crypto and randomness over RustCrypto, with HPKE (RFC 9180) from the `hpke` crate.

[OpenMLS](https://crates.io/crates/openmls) needs a provider for its cryptography. The usual
one, `openmls_rust_crypto`, does HPKE through `hpke-rs`, which is MPL-2.0. This crate is a
drop-in replacement whose whole dependency graph is MIT and/or Apache-2.0:

- `MlsCrypto` implements `openmls_traits`' `OpenMlsCrypto` and `OpenMlsRand`;
- `MlsProvider` is `MlsCrypto` plus `openmls_memory_storage`'s `MemoryStorage`, in place of
  `openmls_rust_crypto::OpenMlsRustCrypto`.

It behaves as `openmls_rust_crypto` 0.6.0 does (the same key formats, errors and RNG: one
`ChaCha20Rng` per provider, seeded from the OS), except that derived X25519 private keys are
clamped (see CHANGELOG), for the three ciphersuites it supports:

- `MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519` (0x0001);
- `MLS_128_DHKEMP256_AES128GCM_SHA256_P256` (0x0002);
- `MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519` (0x0003).

The post-quantum draft suites are not built, and P-384 signatures are refused. It builds for
Linux, macOS, Windows and wasm32 (in browsers, through `getrandom`'s `js` backend).

It is part of Sew, Alakazam Labs' self-hosted team chat server, where it is the MLS provider of
the end-to-end encrypted calls (`sew-e2ee`).

- Source: <https://github.com/Alakazam-211/sew-crates> (read-only: it is generated from Sew's
  repository at each release, so pull requests are not merged there)
- API documentation: <https://docs.rs/sew-mls-crypto>
- Security problems: report them privately, as
  <https://github.com/Alakazam-211/sew-crates/security/policy> says.
- No third-party audit: this crate has not had a third-party security audit.

## Usage

```toml
[dependencies]
sew-mls-crypto = "0.9.2"
openmls = "=0.9.0"
```

```rust
use openmls::prelude::*;
use sew_mls_crypto::MlsProvider;

let provider = MlsProvider::new();
// Use `provider` wherever OpenMLS takes an `OpenMlsProvider`.
```

The version numbers don't clash. OpenMLS numbers its helper crates apart from `openmls`
itself: `openmls` 0.9.0 depends on `openmls_traits` 0.6, and this crate implements
`openmls_traits` 0.6.0's traits and uses `openmls_memory_storage` 0.6.0 (built on the same
`openmls_traits`). This crate pins both at exactly `=0.6.0`, so Cargo resolves one
`openmls_traits` for your `openmls` 0.9.0 and this provider.

Leave openmls' `targeted-messages-draft` and `virtual-clients-draft` features off: they add
required methods to `OpenMlsCrypto` that this provider does not implement.

MSRV: Rust 1.95.

`SignatureKeyPair` is the signature key pair of a basic credential (Ed25519 or ECDSA P-256), in
place of `openmls_basic_credential`'s: the same keys, signatures, serde and TLS forms and storage
id, so key pairs and groups made with either work with the other.

## Licence

MIT OR Apache-2.0, at your option. See `LICENSE-MIT` and `LICENSE-APACHE`.

`src/crypto.rs` and, in part, `src/lib.rs` (`MlsProvider` and its `OpenMlsProvider` impl) are
derived from `openmls_rust_crypto` 0.6.0, and `src/credential.rs` (`SignatureKeyPair`) from
`openmls_basic_credential` 0.6.0 (both MIT, Copyright (c) 2020 OpenMLS Authors); `src/rng.rs`
follows the shape of `openmls_rust_crypto`'s `rand_shim`. `NOTICE` carries their copyright line
and permission notice, which must travel with the code.

Copyright 2026 Alakazam Labs.
