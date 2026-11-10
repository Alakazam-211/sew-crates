//! [`SignatureKeyPair`]: the signature key pair of an MLS basic credential.
//!
//! The drop-in for `openmls_basic_credential` 0.6.0's `SignatureKeyPair` (Copyright (c) 2020
//! OpenMLS Authors, MIT; see NOTICE), modified by Alakazam Labs: on the RustCrypto generation of
//! this crate (p256 0.14), Ed25519 and ECDSA P-256 only (the schemes of the supported
//! ciphersuites; P-384 and the post-quantum drafts are refused), and the P-256 key drawn as 32
//! random bytes from the OS RNG (a scalar out of range is drawn again).
//!
//! The formats are the original's, byte for byte: Ed25519 keys are the 32-byte seed and the
//! 32-byte public key, signing with Ed25519 (RFC 8032, 64-byte signatures); P-256 keys are the
//! 32-byte big-endian scalar and the 65-byte uncompressed SEC1 point, signing with
//! ECDSA-SHA256 (RFC 6979 nonces, DER-encoded signatures, no low-S normalisation). The serde
//! form (`{"private":[…],"public":[…],"signature_scheme":"ED25519"}`), the TLS form and the
//! storage id (`public ‖ "RustCryptoSignatureKey" ‖ scheme`) are the same, so a key pair either
//! one stored reads back in the other.

use std::fmt;

use openmls_traits::signatures::{Signer, SignerError};
use openmls_traits::storage::{self, CURRENT_VERSION, StorageProvider};
use openmls_traits::types::{CryptoError, SignatureScheme};
use p256::ecdsa::signature::Signer as _;
use rand_core::{OsRng, RngCore as _};
use serde::{Deserialize, Serialize};
use tls_codec::{SecretVLBytes, TlsDeserialize, TlsDeserializeBytes, TlsSerialize, TlsSize};
use zeroize::Zeroizing;

/// A signature key pair for the basic credential: the private and public key and their
/// signature scheme. The private key is zeroized on drop and never printed.
#[derive(TlsSerialize, TlsSize, TlsDeserialize, TlsDeserializeBytes, Serialize, Deserialize)]
pub struct SignatureKeyPair {
    #[serde(with = "secret_bytes_as_vec")]
    private: SecretVLBytes,
    public: Vec<u8>,
    signature_scheme: SignatureScheme,
}

/// Serializes the private key as a bare byte array (`SecretVLBytes` alone would be
/// `{"vec": […]}`), the original's format.
mod secret_bytes_as_vec {
    use tls_codec::SecretVLBytes;
    use zeroize::Zeroize as _;

    pub(super) fn serialize<S: serde::Serializer>(
        value: &SecretVLBytes,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(value.as_slice(), s)
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<SecretVLBytes, D::Error> {
        let mut bytes: Vec<u8> = serde::Deserialize::deserialize(d)?;
        let secret: SecretVLBytes = bytes.as_slice().into();
        bytes.zeroize();
        Ok(secret)
    }
}

impl fmt::Debug for SignatureKeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignatureKeyPair")
            .field("private", &"***")
            .field("public", &self.public)
            .field("signature_scheme", &self.signature_scheme)
            .finish()
    }
}

impl Signer for SignatureKeyPair {
    fn sign(&self, payload: &[u8]) -> Result<Vec<u8>, SignerError> {
        match self.signature_scheme {
            SignatureScheme::ECDSA_SECP256R1_SHA256 => {
                let k = p256::ecdsa::SigningKey::from_slice(self.private.as_slice())
                    .map_err(|_| SignerError::SigningError)?;
                let signature: p256::ecdsa::Signature = k.sign(payload);
                Ok(signature.to_der().to_bytes().into())
            }
            SignatureScheme::ED25519 => {
                let k = ed25519_dalek::SigningKey::try_from(self.private.as_slice())
                    .map_err(|_| SignerError::SigningError)?;
                Ok(ed25519_dalek::Signer::sign(&k, payload).to_bytes().into())
            }
            _ => Err(SignerError::SigningError),
        }
    }

    fn signature_scheme(&self) -> SignatureScheme {
        self.signature_scheme
    }
}

/// The storage id of a key pair: the public key, a label and the scheme (big-endian).
fn id(public_key: &[u8], signature_scheme: SignatureScheme) -> Vec<u8> {
    const LABEL: &[u8; 22] = b"RustCryptoSignatureKey";
    let mut id = public_key.to_vec();
    id.extend_from_slice(LABEL);
    id.extend_from_slice(&(signature_scheme as u16).to_be_bytes());
    id
}

/// A fresh P-256 signing key from the OS RNG.
fn p256_generate() -> p256::ecdsa::SigningKey {
    let mut bytes = Zeroizing::new([0u8; 32]);
    loop {
        OsRng.fill_bytes(bytes.as_mut());
        // Zero and values of the group order or above are refused; the chance is about 2^-32.
        if let Ok(k) = p256::ecdsa::SigningKey::from_slice(bytes.as_ref()) {
            return k;
        }
    }
}

impl SignatureKeyPair {
    /// A fresh key pair of `signature_scheme` (Ed25519 or ECDSA P-256), from the OS RNG.
    pub fn new(signature_scheme: SignatureScheme) -> Result<SignatureKeyPair, CryptoError> {
        let (private, public): (SecretVLBytes, Vec<u8>) = match signature_scheme {
            SignatureScheme::ECDSA_SECP256R1_SHA256 => {
                let k = p256_generate();
                let public = k.verifying_key().to_sec1_point(false).as_bytes().to_vec();
                let mut scalar = Zeroizing::new([0u8; 32]);
                scalar.copy_from_slice(&k.to_bytes());
                (scalar.as_slice().into(), public)
            }
            SignatureScheme::ED25519 => {
                let sk = ed25519_dalek::SigningKey::generate(&mut OsRng);
                let public = sk.verifying_key().to_bytes().to_vec();
                (sk.as_bytes().as_slice().into(), public)
            }
            _ => return Err(CryptoError::UnsupportedSignatureScheme),
        };
        Ok(SignatureKeyPair {
            private,
            public,
            signature_scheme,
        })
    }

    /// A key pair from its raw keys (no check that they match).
    pub fn from_raw(
        signature_scheme: SignatureScheme,
        private: Vec<u8>,
        public: Vec<u8>,
    ) -> SignatureKeyPair {
        SignatureKeyPair {
            private: private.into(),
            public,
            signature_scheme,
        }
    }

    /// The key pair's storage id.
    pub fn id(&self) -> StorageId {
        StorageId {
            value: id(&self.public, self.signature_scheme),
        }
    }

    /// Stores the key pair in `store`.
    pub fn store<T>(&self, store: &T) -> Result<(), T::Error>
    where
        T: StorageProvider<CURRENT_VERSION>,
    {
        store.write_signature_key_pair(&self.id(), self)
    }

    /// Reads a key pair from `store` by its public key and scheme.
    pub fn read(
        store: &impl StorageProvider<CURRENT_VERSION>,
        public_key: &[u8],
        signature_scheme: SignatureScheme,
    ) -> Option<SignatureKeyPair> {
        store
            .signature_key_pair(&StorageId {
                value: id(public_key, signature_scheme),
            })
            .ok()
            .flatten()
    }

    /// Deletes a key pair from `store`.
    pub fn delete<T: StorageProvider<CURRENT_VERSION>>(
        store: &T,
        public_key: &[u8],
        signature_scheme: SignatureScheme,
    ) -> Result<(), T::Error> {
        store.delete_signature_key_pair(&StorageId {
            value: id(public_key, signature_scheme),
        })
    }

    /// The public key.
    pub fn public(&self) -> &[u8] {
        &self.public
    }

    /// The public key, copied.
    pub fn to_public_vec(&self) -> Vec<u8> {
        self.public.clone()
    }

    /// The signature scheme.
    pub fn signature_scheme(&self) -> SignatureScheme {
        self.signature_scheme
    }
}

/// The storage id of a [`SignatureKeyPair`].
#[derive(Debug, Serialize, Deserialize)]
pub struct StorageId {
    value: Vec<u8>,
}

impl From<Vec<u8>> for StorageId {
    fn from(value: Vec<u8>) -> StorageId {
        StorageId { value }
    }
}

impl storage::Key<CURRENT_VERSION> for StorageId {}
impl storage::traits::SignaturePublicKey<CURRENT_VERSION> for StorageId {}
impl storage::Entity<CURRENT_VERSION> for SignatureKeyPair {}
impl storage::traits::SignatureKeyPair<CURRENT_VERSION> for SignatureKeyPair {}

#[cfg(test)]
mod tests {
    use super::*;
    use tls_codec::{DeserializeBytes as _, Serialize as _};

    #[test]
    fn new_refuses_the_unsupported_schemes() {
        for s in [
            SignatureScheme::ECDSA_SECP384R1_SHA384,
            SignatureScheme::ECDSA_SECP521R1_SHA512,
            SignatureScheme::ED448,
        ] {
            assert_eq!(
                SignatureKeyPair::new(s).err(),
                Some(CryptoError::UnsupportedSignatureScheme),
                "{s:?}"
            );
            let kp = SignatureKeyPair::from_raw(s, vec![1; 48], vec![4; 97]);
            assert_eq!(kp.sign(b"x"), Err(SignerError::SigningError), "{s:?}");
        }
    }

    #[test]
    fn key_shapes() {
        let ed = SignatureKeyPair::new(SignatureScheme::ED25519).unwrap();
        assert_eq!((ed.private.as_slice().len(), ed.public().len()), (32, 32));
        assert_eq!(ed.sign(b"m").unwrap().len(), 64);
        let p = SignatureKeyPair::new(SignatureScheme::ECDSA_SECP256R1_SHA256).unwrap();
        assert_eq!((p.private.as_slice().len(), p.public().len()), (32, 65));
        assert_eq!(p.public()[0], 0x04);
        let sig = p.sign(b"m").unwrap();
        assert_eq!(sig[0], 0x30, "DER");
        assert!(p256::ecdsa::Signature::from_der(&sig).is_ok());
    }

    /// The serde (JSON) round trip is tests/credential_serde.rs: `serde_json` is a
    /// dev-dependency, which the packaged crate does not carry.
    #[test]
    fn tls_round_trips() {
        let kp = SignatureKeyPair::new(SignatureScheme::ED25519).unwrap();
        let tls = kp.tls_serialize_detached().unwrap();
        let (back, rest) = SignatureKeyPair::tls_deserialize_bytes(&tls).unwrap();
        assert!(rest.is_empty());
        assert_eq!(back.private.as_slice(), kp.private.as_slice());
        assert_eq!(back.signature_scheme(), SignatureScheme::ED25519);
    }

    #[test]
    fn debug_hides_the_private_key() {
        let kp = SignatureKeyPair::from_raw(SignatureScheme::ED25519, vec![0xab; 32], vec![1]);
        let s = format!("{kp:?}");
        assert!(s.contains("***") && !s.contains("171"), "{s}");
    }
}
