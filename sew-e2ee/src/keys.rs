//! Per-sender frame keys.
//!
//! ```text
//! ctx(L, e) = TLS(struct { opaque group_id<V> = G; uint64 epoch = e; uint32 leaf_index = L; })
//! ikm(L, e) = MLS-Exporter("sew call frame key v1", ctx(L, e), 32)          // RFC 9420 §8.5
//! ```
//!
//! The SDK then derives the AES-128-GCM key with `HKDF-SHA256(ikm, salt = "LKFrameEncryptionKey",
//! info = 128 × 0x00, L = 16)`; [`lk_frame_key`] is a reference implementation of that step
//! (`sew-lk-compat`'s `LkGcmV1`, the recorder's decryptor, the vectors).

use std::fmt;

use openmls_traits::crypto::OpenMlsCrypto;
use openmls_traits::types::HashType;
use rand::{CryptoRng, RngCore};
use sew_mls_crypto::MlsCrypto;
use sew_proto::e2ee::tls::Writer;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::config::LK_SALT;

/// A 32-byte frame-key input (IKM). No `Clone`, no `PartialEq`, no `Serialize`; zeroized on
/// drop; `Debug` prints `FrameIkm(***)`.
pub struct FrameIkm(Zeroizing<[u8; 32]>);

impl FrameIkm {
    /// Wraps 32 bytes (the caller's copy should be zeroized by the caller).
    pub fn from_bytes(bytes: [u8; 32]) -> FrameIkm {
        FrameIkm(Zeroizing::new(bytes))
    }

    /// Takes an exporter output (a plain `Vec`, wrapped at the call site). `None`
    /// unless it is 32 bytes; the vector is zeroized either way.
    pub fn from_exporter(v: Vec<u8>) -> Option<FrameIkm> {
        let v = Zeroizing::new(v);
        let a: [u8; 32] = v.as_slice().try_into().ok()?;
        Some(FrameIkm::from_bytes(a))
    }

    /// 32 fresh random bytes (retirement fillers; never derived from anything).
    pub fn random<R: RngCore + CryptoRng>(rng: &mut R) -> FrameIkm {
        let mut b = Zeroizing::new([0u8; 32]);
        rng.fill_bytes(b.as_mut());
        FrameIkm(b)
    }

    /// 32 fresh bytes from the OS RNG.
    pub fn random_os() -> FrameIkm {
        FrameIkm::random(&mut rand::rngs::OsRng)
    }

    /// The raw bytes, for a bridge that imports them (its copy must be zeroized after use).
    pub fn expose(&self) -> &[u8; 32] {
        &self.0
    }

    /// An explicit zeroizing copy (bridges and the `KeyRing` shadow hold their own).
    pub fn duplicate(&self) -> FrameIkm {
        FrameIkm::from_bytes(*self.0)
    }

    /// Constant-time equality.
    pub fn ct_eq(&self, other: &FrameIkm) -> bool {
        self.0.as_slice().ct_eq(other.0.as_slice()).into()
    }
}

impl fmt::Debug for FrameIkm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FrameIkm(***)")
    }
}

/// `ctx(L, e)`: `TLS{opaque group_id<V>; uint64 epoch; uint32 leaf_index}`.
pub fn ikm_context(group_id: &[u8], epoch: u64, leaf_index: u32) -> Vec<u8> {
    let mut w = Writer::new();
    // A group id is 16 bytes; a vector of any realistic size encodes.
    if w.opaque(group_id).is_err() {
        return Vec::new();
    }
    w.u64(epoch);
    w.u32(leaf_index);
    w.into_bytes()
}

/// The SDK step: `HKDF-SHA256(IKM = ikm, salt = "LKFrameEncryptionKey", info = 128 × 0x00,
/// L = 16)` (JS `utils.ts:61-80`; native `frame_crypto_transformer.cc:249-262`).
pub fn lk_frame_key(ikm: &FrameIkm) -> Zeroizing<[u8; 16]> {
    let crypto = MlsCrypto::default();
    let mut out = Zeroizing::new([0u8; 16]);
    let Ok(prk) = crypto.hkdf_extract(HashType::Sha2_256, LK_SALT, ikm.expose()) else {
        return out;
    };
    if let Ok(okm) = crypto.hkdf_expand(HashType::Sha2_256, prk.as_slice(), &[0u8; 128], 16)
        && okm.as_slice().len() == 16
    {
        out.copy_from_slice(okm.as_slice());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_layout() {
        let c = ikm_context(&[7u8; 16], 3, 9);
        assert_eq!(c.len(), 1 + 16 + 8 + 4);
        assert_eq!(c[0], 16);
        assert_eq!(&c[17..25], &3u64.to_be_bytes());
        assert_eq!(&c[25..], &9u32.to_be_bytes());
    }

    #[test]
    fn ikm_debug_hides_bytes() {
        let k = FrameIkm::from_bytes([0xab; 32]);
        assert_eq!(format!("{k:?}"), "FrameIkm(***)");
        assert!(k.ct_eq(&k.duplicate()));
        assert!(!k.ct_eq(&FrameIkm::from_bytes([0; 32])));
    }
}
