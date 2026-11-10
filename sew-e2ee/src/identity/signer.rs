//! The device-key interface: signing happens outside the core, so hardware and
//! async keystores fit. The core asks with `Output::NeedDeviceSignature { request, message }`
//! (or a [`crate::SignRequest`]) and takes the raw `r‖s` signature (IEEE P1363, 64 bytes).

use std::fmt;

use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use rand::{CryptoRng, RngCore};
use zeroize::Zeroizing;

/// A platform device key (P-256, non-exportable where the platform allows; no user-presence
/// requirement).
pub trait DeviceSigner {
    /// The uncompressed SEC1 public key (65 bytes).
    fn public_key_sec1(&self) -> [u8; 65];
}

/// A software P-256 device key (tests, Sew's Rust SDK test lane, the driver). The private scalar is
/// zeroized on drop.
pub struct SoftwareSigner {
    key: SigningKey,
}

impl SoftwareSigner {
    /// A fresh key: 32 bytes from `rng` as a big-endian scalar, drawn again when out of range
    /// (zero or at least the group order; a chance of about 2^-32).
    pub fn generate<R: RngCore + CryptoRng>(rng: &mut R) -> SoftwareSigner {
        let mut scalar = Zeroizing::new([0u8; 32]);
        loop {
            rng.fill_bytes(scalar.as_mut());
            if let Ok(key) = SigningKey::from_slice(scalar.as_ref()) {
                return SoftwareSigner { key };
            }
        }
    }

    /// A key from a 32-byte scalar (test vectors). `None` for an invalid scalar.
    pub fn from_bytes(scalar: &[u8; 32]) -> Option<SoftwareSigner> {
        SigningKey::from_slice(scalar)
            .ok()
            .map(|key| SoftwareSigner { key })
    }

    /// The scalar (zeroizing copy), for a store that persists a software key.
    pub fn to_bytes(&self) -> Zeroizing<[u8; 32]> {
        let mut out = Zeroizing::new([0u8; 32]);
        out.copy_from_slice(&self.key.to_bytes());
        out
    }

    /// ECDSA-P256-SHA256 over `message`, as raw `r‖s`.
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        let sig: Signature = self.key.sign(message);
        let mut out = [0u8; 64];
        out.copy_from_slice(&sig.to_bytes());
        out
    }
}

impl DeviceSigner for SoftwareSigner {
    fn public_key_sec1(&self) -> [u8; 65] {
        let point = self.key.verifying_key().to_sec1_point(false);
        let mut out = [0u8; 65];
        let b = point.as_bytes();
        if b.len() == 65 {
            out.copy_from_slice(b);
        }
        out
    }
}

impl fmt::Debug for SoftwareSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SoftwareSigner(***)")
    }
}
