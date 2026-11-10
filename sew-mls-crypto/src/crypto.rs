//! [`MlsCrypto`]: `OpenMlsCrypto` and `OpenMlsRand` over RustCrypto crates.
//!
//! Derived from `openmls_rust_crypto` 0.6.0's `provider.rs` and `hmac.rs` (Copyright (c) 2020
//! OpenMLS Authors, MIT; see NOTICE), modified by Alakazam Labs: HPKE through the `hpke` crate
//! (`hpke_impl`), the RustCrypto 0.10 generation (sha2 0.10, hkdf 0.12, hmac 0.12), no
//! post-quantum or draft methods, no P-384, and length checks that return errors where the
//! original converted a slice into a fixed-size array (a panic on a wrong length).

use std::fmt;
use std::sync::RwLock;

use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{Aead as _, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Aes256Gcm};
use chacha20poly1305::ChaCha20Poly1305;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use openmls_traits::crypto::OpenMlsCrypto;
use openmls_traits::random::OpenMlsRand;
use openmls_traits::types::{
    AeadType, Ciphersuite, CryptoError, ExporterSecret, HashType, HpkeCiphertext, HpkeConfig,
    HpkeKeyPair, SignatureScheme,
};
use p256::Sec1Point;
use p256::ecdsa::signature::{Signer as _, Verifier as _};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore as _, SeedableRng as _};
use sha2::{Digest as _, Sha256, Sha384, Sha512};
use tls_codec::SecretVLBytes;

use crate::hpke_impl::{self, Fail};

/// The AEAD nonce length of every supported AEAD (AES-GCM, ChaCha20Poly1305).
const AEAD_NONCE_LEN: usize = 12;

/// The crypto and randomness provider: one `ChaCha20Rng`, seeded from the OS, behind a lock
/// (as `openmls_rust_crypto::RustCrypto`).
pub struct MlsCrypto {
    rng: RwLock<ChaCha20Rng>,
}

impl Default for MlsCrypto {
    fn default() -> Self {
        MlsCrypto {
            rng: RwLock::new(ChaCha20Rng::from_entropy()),
        }
    }
}

impl fmt::Debug for MlsCrypto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the RNG's state.
        f.debug_struct("MlsCrypto").finish_non_exhaustive()
    }
}

/// Errors of the randomness provider (`openmls_rust_crypto::RandError`'s).
#[derive(thiserror::Error, Debug, Copy, Clone, PartialEq, Eq)]
pub enum RandError {
    /// The RNG lock is poisoned.
    #[error("Rng lock is poisoned.")]
    LockPoisoned,
    /// The RNG failed to fill the buffer.
    #[error("Unable to collect enough randomness.")]
    NotEnoughRandomness,
}

/// The ciphersuites this provider implements, in `openmls_rust_crypto` 0.6.0's order.
const SUPPORTED: [Ciphersuite; 3] = [
    Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519,
    Ciphersuite::MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519,
    Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256,
];

macro_rules! hmac_digest {
    ($hash:ty, $key:expr, $message:expr) => {{
        let mut mac = <Hmac<$hash> as Mac>::new_from_slice($key)
            .map_err(|_| CryptoError::CryptoLibraryError)?;
        mac.update($message);
        Ok(mac.finalize().into_bytes().as_slice().into())
    }};
}

macro_rules! hkdf_expand {
    ($hash:ty, $prk:expr, $info:expr, $len:expr) => {{
        let hkdf =
            Hkdf::<$hash>::from_prk($prk).map_err(|_| CryptoError::HkdfOutputLengthInvalid)?;
        let mut okm = vec![0u8; $len];
        hkdf.expand($info, &mut okm)
            .map_err(|_| CryptoError::HkdfOutputLengthInvalid)?;
        Ok(okm.into())
    }};
}

macro_rules! aead_seal {
    ($cipher:ty, $key:expr, $nonce:expr, $msg:expr, $aad:expr) => {{
        let c = <$cipher>::new_from_slice($key).map_err(|_| CryptoError::CryptoLibraryError)?;
        c.encrypt(
            GenericArray::from_slice(&$nonce[..]),
            Payload {
                msg: $msg,
                aad: $aad,
            },
        )
        .map_err(|_| CryptoError::CryptoLibraryError)
    }};
}

macro_rules! aead_open {
    ($cipher:ty, $key:expr, $nonce:expr, $msg:expr, $aad:expr) => {{
        let c = <$cipher>::new_from_slice($key).map_err(|_| CryptoError::CryptoLibraryError)?;
        c.decrypt(
            GenericArray::from_slice(&$nonce[..]),
            Payload {
                msg: $msg,
                aad: $aad,
            },
        )
        .map_err(|_| CryptoError::AeadDecryptionError)
    }};
}

impl MlsCrypto {
    fn with_rng<T>(&self, f: impl FnOnce(&mut ChaCha20Rng) -> T) -> Result<T, CryptoError> {
        let mut rng = self
            .rng
            .write()
            .map_err(|_| CryptoError::InsufficientRandomness)?;
        Ok(f(&mut rng))
    }
}

/// A P-256 signing key from `rng`: 32 random bytes as a big-endian scalar, drawn again when out
/// of range (zero or at least the group order; a chance of about 2^-32), which is how
/// `SigningKey::random` drew it.
fn p256_random(rng: &mut ChaCha20Rng) -> SigningKey {
    let mut bytes = zeroize::Zeroizing::new([0u8; 32]);
    loop {
        rng.fill_bytes(bytes.as_mut());
        if let Ok(k) = SigningKey::from_slice(bytes.as_ref()) {
            return k;
        }
    }
}

/// A 12-byte AEAD nonce, or the error `openmls_rust_crypto` would have panicked instead of.
fn nonce(nonce: &[u8]) -> Result<&[u8; AEAD_NONCE_LEN], CryptoError> {
    <&[u8; AEAD_NONCE_LEN]>::try_from(nonce).map_err(|_| CryptoError::CryptoLibraryError)
}

impl OpenMlsCrypto for MlsCrypto {
    fn supports(&self, ciphersuite: Ciphersuite) -> Result<(), CryptoError> {
        if SUPPORTED.contains(&ciphersuite) {
            Ok(())
        } else {
            Err(CryptoError::UnsupportedCiphersuite)
        }
    }

    fn supported_ciphersuites(&self) -> Vec<Ciphersuite> {
        SUPPORTED.to_vec()
    }

    fn hkdf_extract(
        &self,
        hash_type: HashType,
        salt: &[u8],
        ikm: &[u8],
    ) -> Result<SecretVLBytes, CryptoError> {
        Ok(match hash_type {
            HashType::Sha2_256 => Hkdf::<Sha256>::extract(Some(salt), ikm).0.as_slice().into(),
            HashType::Sha2_384 => Hkdf::<Sha384>::extract(Some(salt), ikm).0.as_slice().into(),
            HashType::Sha2_512 => Hkdf::<Sha512>::extract(Some(salt), ikm).0.as_slice().into(),
        })
    }

    fn hmac(
        &self,
        hash_type: HashType,
        key: &[u8],
        message: &[u8],
    ) -> Result<SecretVLBytes, CryptoError> {
        match hash_type {
            HashType::Sha2_256 => hmac_digest!(Sha256, key, message),
            HashType::Sha2_384 => hmac_digest!(Sha384, key, message),
            HashType::Sha2_512 => hmac_digest!(Sha512, key, message),
        }
    }

    fn hkdf_expand(
        &self,
        hash_type: HashType,
        prk: &[u8],
        info: &[u8],
        okm_len: usize,
    ) -> Result<SecretVLBytes, CryptoError> {
        match hash_type {
            HashType::Sha2_256 => hkdf_expand!(Sha256, prk, info, okm_len),
            HashType::Sha2_384 => hkdf_expand!(Sha384, prk, info, okm_len),
            HashType::Sha2_512 => hkdf_expand!(Sha512, prk, info, okm_len),
        }
    }

    fn hash(&self, hash_type: HashType, data: &[u8]) -> Result<Vec<u8>, CryptoError> {
        Ok(match hash_type {
            HashType::Sha2_256 => Sha256::digest(data).to_vec(),
            HashType::Sha2_384 => Sha384::digest(data).to_vec(),
            HashType::Sha2_512 => Sha512::digest(data).to_vec(),
        })
    }

    fn aead_encrypt(
        &self,
        alg: AeadType,
        key: &[u8],
        data: &[u8],
        nonce_bytes: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let n = nonce(nonce_bytes)?;
        match alg {
            AeadType::Aes128Gcm => aead_seal!(Aes128Gcm, key, n, data, aad),
            AeadType::Aes256Gcm => aead_seal!(Aes256Gcm, key, n, data, aad),
            AeadType::ChaCha20Poly1305 => aead_seal!(ChaCha20Poly1305, key, n, data, aad),
        }
    }

    fn aead_decrypt(
        &self,
        alg: AeadType,
        key: &[u8],
        ct_tag: &[u8],
        nonce_bytes: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let n = nonce(nonce_bytes)?;
        match alg {
            AeadType::Aes128Gcm => aead_open!(Aes128Gcm, key, n, ct_tag, aad),
            AeadType::Aes256Gcm => aead_open!(Aes256Gcm, key, n, ct_tag, aad),
            AeadType::ChaCha20Poly1305 => aead_open!(ChaCha20Poly1305, key, n, ct_tag, aad),
        }
    }

    fn signature_key_gen(&self, alg: SignatureScheme) -> Result<(Vec<u8>, Vec<u8>), CryptoError> {
        match alg {
            SignatureScheme::ECDSA_SECP256R1_SHA256 => self.with_rng(|rng| {
                let k = p256_random(rng);
                let pk = k.verifying_key().to_sec1_point(false).as_bytes().to_vec();
                (k.to_bytes().to_vec(), pk)
            }),
            SignatureScheme::ED25519 => self.with_rng(|rng| {
                let sk = ed25519_dalek::SigningKey::generate(rng);
                let pk = sk.verifying_key().to_bytes().to_vec();
                (sk.to_bytes().to_vec(), pk)
            }),
            _ => Err(CryptoError::UnsupportedSignatureScheme),
        }
    }

    fn verify_signature(
        &self,
        alg: SignatureScheme,
        data: &[u8],
        pk: &[u8],
        signature: &[u8],
    ) -> Result<(), CryptoError> {
        match alg {
            SignatureScheme::ECDSA_SECP256R1_SHA256 => {
                let point =
                    Sec1Point::from_bytes(pk).map_err(|_| CryptoError::CryptoLibraryError)?;
                let k = VerifyingKey::from_sec1_point(&point)
                    .map_err(|_| CryptoError::CryptoLibraryError)?;
                let sig =
                    Signature::from_der(signature).map_err(|_| CryptoError::InvalidSignature)?;
                k.verify(data, &sig)
                    .map_err(|_| CryptoError::InvalidSignature)
            }
            SignatureScheme::ED25519 => {
                let k = ed25519_dalek::VerifyingKey::try_from(pk)
                    .map_err(|_| CryptoError::CryptoLibraryError)?;
                let sig = <[u8; ed25519_dalek::SIGNATURE_LENGTH]>::try_from(signature)
                    .map_err(|_| CryptoError::CryptoLibraryError)?;
                k.verify_strict(data, &ed25519_dalek::Signature::from(sig))
                    .map_err(|_| CryptoError::InvalidSignature)
            }
            _ => Err(CryptoError::UnsupportedSignatureScheme),
        }
    }

    fn sign(&self, alg: SignatureScheme, data: &[u8], key: &[u8]) -> Result<Vec<u8>, CryptoError> {
        match alg {
            SignatureScheme::ECDSA_SECP256R1_SHA256 => {
                if key.len() != 32 {
                    return Err(CryptoError::CryptoLibraryError);
                }
                let k = SigningKey::from_slice(key).map_err(|_| CryptoError::CryptoLibraryError)?;
                let signature: Signature = k.sign(data);
                Ok(signature.to_der().to_bytes().to_vec())
            }
            SignatureScheme::ED25519 => {
                let k = ed25519_dalek::SigningKey::try_from(key)
                    .map_err(|_| CryptoError::CryptoLibraryError)?;
                Ok(ed25519_dalek::Signer::sign(&k, data).to_bytes().to_vec())
            }
            _ => Err(CryptoError::UnsupportedSignatureScheme),
        }
    }

    fn hpke_seal(
        &self,
        config: HpkeConfig,
        pk_r: &[u8],
        info: &[u8],
        aad: &[u8],
        ptxt: &[u8],
    ) -> Result<HpkeCiphertext, CryptoError> {
        let (kem_output, ciphertext) = self
            .with_rng(|rng| hpke_impl::seal(config, pk_r, info, aad, ptxt, rng))?
            .map_err(|e| match e {
                Fail::Input => CryptoError::InvalidLength,
                Fail::Unsupported | Fail::Other => CryptoError::CryptoLibraryError,
            })?;
        Ok(HpkeCiphertext {
            kem_output: kem_output.into(),
            ciphertext: ciphertext.into(),
        })
    }

    fn hpke_open(
        &self,
        config: HpkeConfig,
        input: &HpkeCiphertext,
        sk_r: &[u8],
        info: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        hpke_impl::open(
            config,
            input.kem_output.as_slice(),
            sk_r,
            info,
            aad,
            input.ciphertext.as_slice(),
        )
        .map_err(|_| CryptoError::HpkeDecryptionError)
    }

    fn hpke_setup_sender_and_export(
        &self,
        config: HpkeConfig,
        pk_r: &[u8],
        info: &[u8],
        exporter_context: &[u8],
        exporter_length: usize,
    ) -> Result<(Vec<u8>, ExporterSecret), CryptoError> {
        let (enc, secret) = self
            .with_rng(|rng| {
                hpke_impl::sender_export(config, pk_r, info, exporter_context, exporter_length, rng)
            })?
            .map_err(|e| match e {
                // openmls_rust_crypto: a setup failure is SenderSetupError, an export failure
                // ExporterError; `hpke` reports a too-long export as KdfOutputTooLong (Other).
                Fail::Other => CryptoError::ExporterError,
                Fail::Input | Fail::Unsupported => CryptoError::SenderSetupError,
            })?;
        Ok((enc, secret.into()))
    }

    fn hpke_setup_receiver_and_export(
        &self,
        config: HpkeConfig,
        enc: &[u8],
        sk_r: &[u8],
        info: &[u8],
        exporter_context: &[u8],
        exporter_length: usize,
    ) -> Result<ExporterSecret, CryptoError> {
        hpke_impl::receiver_export(config, enc, sk_r, info, exporter_context, exporter_length)
            .map(Into::into)
            .map_err(|e| match e {
                Fail::Other => CryptoError::ExporterError,
                Fail::Input | Fail::Unsupported => CryptoError::ReceiverSetupError,
            })
    }

    fn derive_hpke_keypair(
        &self,
        config: HpkeConfig,
        ikm: &[u8],
    ) -> Result<HpkeKeyPair, CryptoError> {
        let (private, public) = hpke_impl::derive_keypair(config, ikm).map_err(|e| match e {
            Fail::Input => CryptoError::InvalidLength,
            Fail::Unsupported | Fail::Other => CryptoError::CryptoLibraryError,
        })?;
        Ok(HpkeKeyPair {
            private: private.into(),
            public,
        })
    }
}

impl OpenMlsRand for MlsCrypto {
    type Error = RandError;

    fn random_array<const N: usize>(&self) -> Result<[u8; N], Self::Error> {
        let mut rng = self.rng.write().map_err(|_| RandError::LockPoisoned)?;
        let mut out = [0u8; N];
        rng.try_fill_bytes(&mut out)
            .map_err(|_| RandError::NotEnoughRandomness)?;
        Ok(out)
    }

    fn random_vec(&self, len: usize) -> Result<Vec<u8>, Self::Error> {
        let mut rng = self.rng.write().map_err(|_| RandError::LockPoisoned)?;
        let mut out = vec![0u8; len];
        rng.try_fill_bytes(&mut out)
            .map_err(|_| RandError::NotEnoughRandomness)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_is_consistent_with_supported_ciphersuites() {
        let crypto = MlsCrypto::default();
        for cs in crypto.supported_ciphersuites() {
            assert!(crypto.supports(cs).is_ok(), "{cs:?}");
        }
        assert_eq!(
            crypto.supports(Ciphersuite::MLS_256_DHKEMP384_AES256GCM_SHA384_P384),
            Err(CryptoError::UnsupportedCiphersuite)
        );
    }

    #[test]
    fn wrong_nonce_length_is_an_error_not_a_panic() {
        let crypto = MlsCrypto::default();
        let key = [7u8; 16];
        assert_eq!(
            crypto.aead_encrypt(AeadType::Aes128Gcm, &key, b"x", &[0u8; 11], b""),
            Err(CryptoError::CryptoLibraryError)
        );
        assert_eq!(
            crypto.aead_decrypt(AeadType::Aes128Gcm, &key, &[0u8; 17], &[0u8; 13], b""),
            Err(CryptoError::CryptoLibraryError)
        );
        assert_eq!(
            crypto.sign(SignatureScheme::ECDSA_SECP256R1_SHA256, b"x", &[1u8; 31]),
            Err(CryptoError::CryptoLibraryError)
        );
    }

    #[test]
    fn debug_hides_the_rng() {
        assert_eq!(format!("{:?}", MlsCrypto::default()), "MlsCrypto { .. }");
    }
}
