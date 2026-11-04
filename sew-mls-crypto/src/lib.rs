//! Sew's OpenMLS crypto provider.
//!
//! [`MlsCrypto`] implements `openmls_traits`'
//! [`OpenMlsCrypto`](openmls_traits::crypto::OpenMlsCrypto) and
//! [`OpenMlsRand`](openmls_traits::random::OpenMlsRand) over RustCrypto crates, and does HPKE (RFC
//! 9180, base mode) through the `hpke` crate (MIT OR Apache-2.0). It replaces
//! `openmls_rust_crypto`'s `RustCrypto`, whose HPKE is the MPL-2.0 `hpke-rs`; [`MlsProvider`]
//! replaces `OpenMlsRustCrypto` (crypto plus an in-memory `MemoryStorage`).
//!
//! The behaviour is `openmls_rust_crypto` 0.6.0's for every ciphersuite it advertises, which
//! are the three this crate supports:
//!
//! - `MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519` (0x0001, the one Sew uses),
//! - `MLS_128_DHKEMP256_AES128GCM_SHA256_P256` (0x0002),
//! - `MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519` (0x0003).
//!
//! The same key formats (RFC 9180 `SerializePrivateKey`/`SerializePublicKey`, Ed25519 32-byte
//! seeds, P-256 32-byte scalars, SEC1 uncompressed public keys, DER ECDSA signatures), the same
//! errors, and the same RNG: one `ChaCha20Rng` per provider, seeded from the OS, behind a lock.
//! HPKE's ephemeral keys come from that RNG too. The post-quantum draft suites and the
//! `targeted-messages-draft` / `virtual-clients-draft` methods are not built (openmls'
//! features for them are forbidden in Sew), and P-384 signatures, which no supported
//! suite uses, are refused.
//!
//! Parts of this crate are derived from `openmls_rust_crypto` 0.6.0 (Copyright (c) 2020 OpenMLS
//! Authors, MIT); see NOTICE.

mod crypto;
mod hpke_impl;
mod rng;

pub use crypto::{MlsCrypto, RandError};
pub use openmls_memory_storage::{MemoryStorage, MemoryStorageError};

use openmls_traits::OpenMlsProvider;

/// An OpenMLS provider: [`MlsCrypto`] for crypto and randomness, `MemoryStorage` for storage.
/// The drop-in for `openmls_rust_crypto::OpenMlsRustCrypto`.
#[derive(Default, Debug)]
pub struct MlsProvider {
    crypto: MlsCrypto,
    storage: MemoryStorage,
}

impl MlsProvider {
    /// A provider with a fresh RNG and an empty store.
    pub fn new() -> MlsProvider {
        MlsProvider::default()
    }
}

impl OpenMlsProvider for MlsProvider {
    type CryptoProvider = MlsCrypto;
    type RandProvider = MlsCrypto;
    type StorageProvider = MemoryStorage;

    fn storage(&self) -> &Self::StorageProvider {
        &self.storage
    }

    fn crypto(&self) -> &Self::CryptoProvider {
        &self.crypto
    }

    fn rand(&self) -> &Self::RandProvider {
        &self.crypto
    }
}
