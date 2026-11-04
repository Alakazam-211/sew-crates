sew-mls-crypto is an OpenMLS crypto provider built only on permissively licensed crates: openmls_traits' crypto and randomness over RustCrypto, with HPKE (RFC 9180) from the `hpke` crate.

# sew-mls-crypto

[OpenMLS](https://crates.io/crates/openmls) needs a provider for its cryptography. The usual
one, `openmls_rust_crypto`, does HPKE through `hpke-rs`, which is MPL-2.0. This crate is a
drop-in replacement whose whole dependency graph is MIT and/or Apache-2.0:

- `MlsCrypto` implements `openmls_traits`' `OpenMlsCrypto` and `OpenMlsRand`;
- `MlsProvider` is `MlsCrypto` plus `openmls_memory_storage`'s `MemoryStorage`, in place of
  `openmls_rust_crypto::OpenMlsRustCrypto`.

It behaves as `openmls_rust_crypto` 0.6.0 does (the same key formats, errors and RNG: one
`ChaCha20Rng` per provider, seeded from the OS) for the three ciphersuites it supports:

- `MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519` (0x0001);
- `MLS_128_DHKEMP256_AES128GCM_SHA256_P256` (0x0002);
- `MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519` (0x0003).

The post-quantum draft suites are not built, and P-384 signatures are refused. It builds for
Linux, macOS, Windows and wasm32 (in browsers, through `getrandom`'s `js` backend).

It is part of Sew, Alakazam Labs' self-hosted team chat server, where it is the MLS provider of
the end-to-end encrypted calls (`sew-e2ee`).

## Usage

The crate is not on crates.io yet. It is published in the read-only
[sew-crates](https://github.com/Alakazam-211/sew-crates) repository (clone it, for example, as
`third_party/sew-crates`); point Cargo at it from your workspace's root `Cargo.toml`:

```toml
[dependencies]
sew-mls-crypto = "0.7.3"
openmls = "=0.9.0"

[patch.crates-io]
sew-mls-crypto = { path = "third_party/sew-crates/sew-mls-crypto" }
```

```rust
use openmls::prelude::*;
use sew_mls_crypto::MlsProvider;

let provider = MlsProvider::new();
// Use `provider` wherever OpenMLS takes an `OpenMlsProvider`.
```

## Licence

MIT OR Apache-2.0, at your option. See `LICENSE-MIT` and `LICENSE-APACHE`.

`src/crypto.rs` and, in part, `src/lib.rs` (`MlsProvider` and its `OpenMlsProvider` impl) are
derived from `openmls_rust_crypto` 0.6.0 (MIT, Copyright (c) 2020 OpenMLS Authors); `NOTICE` carries its copyright line and permission notice, which must travel with the
code.

Copyright 2026 Alakazam Labs.

> **Known advisory (RUSTSEC-2026-0387, crypto-bigint < 0.7.6).** The P-256 suite
> `MLS_128_DHKEMP256_AES128GCM_SHA256_P256` (0x0002) reaches crypto-bigint 0.5.5 through p256 0.13
> (DHKEM(P-256) in `hpke` 0.13 and ECDSA P-256 signatures). The X25519/Ed25519 suites (0x0001, 0x0003) don't use it, but it stays linked, so dependency scanners will still report it.
> Our assessment, from source review only (no `cargo checkct` run yet: a tool that checks compiled
> code for secret-dependent timing), is that these paths don't call the
> operations the advisory names. The fix (crypto-bigint 0.7.9+) needs p256 0.14 and `hpke` 0.14+, a RustCrypto upgrade we plan to make.
> If that's not enough for your threat model, use 0x0001 or 0x0003. That covers this crate only:
> `sew-e2ee` signs with its P-256 device key on every join, whatever the suite (see its README).
