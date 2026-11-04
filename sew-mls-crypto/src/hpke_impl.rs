//! HPKE (RFC 9180, base mode) for openmls' `HpkeConfig`, over the `hpke` crate.
//!
//! openmls names the suite at run time (`HpkeConfig(kem, kdf, aead)`); `hpke` takes it as type
//! parameters. [`dispatch!`] maps one to the other for the KEMs of the supported ciphersuites
//! (DHKEM(X25519, HKDF-SHA256) and DHKEM(P-256, HKDF-SHA256)), every KDF openmls names
//! (HKDF-SHA256/384/512) and every AEAD (AES-128-GCM, AES-256-GCM, ChaCha20Poly1305 and
//! export-only). Keys and encapsulations are RFC 9180's serializations: X25519 32-byte keys,
//! P-256 32-byte scalars and 65-byte uncompressed points.

use hpke::aead::{Aead, AesGcm128, AesGcm256, ChaCha20Poly1305, ExportOnlyAead};
use hpke::kdf::{HkdfSha256, HkdfSha384, HkdfSha512, Kdf};
use hpke::kem::{DhP256HkdfSha256, X25519HkdfSha256};
use hpke::{Deserializable, HpkeError, Kem, OpModeR, OpModeS, Serializable};
use openmls_traits::types::{HpkeAeadType, HpkeConfig, HpkeKdfType, HpkeKemType};

use crate::rng::Rng09;

/// Why an HPKE operation failed; each `OpenMlsCrypto` method maps it to its own `CryptoError`
/// the way `openmls_rust_crypto` mapped `hpke-rs`' errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fail {
    /// A key or encapsulation of the wrong length or not on the curve (`hpke-rs`' InvalidInput).
    Input,
    /// A KEM this provider does not build (P-384, P-521, X448, the PQ drafts).
    Unsupported,
    /// Any other failure: decapsulation, AEAD, a too-long export, export-only sealing.
    Other,
}

impl From<HpkeError> for Fail {
    fn from(e: HpkeError) -> Fail {
        match e {
            HpkeError::IncorrectInputLength(..) | HpkeError::ValidationError => Fail::Input,
            _ => Fail::Other,
        }
    }
}

/// Runs `$f::<A, Kdf, Kem>($args…)` for the suite of `$config`.
macro_rules! dispatch {
    ($config:expr, $f:ident($($arg:expr),* $(,)?)) => {{
        let HpkeConfig(kem, kdf, aead) = $config;
        match kem {
            HpkeKemType::DhKem25519 => dispatch!(@kdf X25519HkdfSha256, kdf, aead, $f($($arg),*)),
            HpkeKemType::DhKemP256 => dispatch!(@kdf DhP256HkdfSha256, kdf, aead, $f($($arg),*)),
            #[allow(unreachable_patterns)]
            _ => Err(Fail::Unsupported),
        }
    }};
    (@kdf $kem:ty, $kdf:expr, $aead:expr, $f:ident($($arg:expr),*)) => {
        match $kdf {
            HpkeKdfType::HkdfSha256 => dispatch!(@aead $kem, HkdfSha256, $aead, $f($($arg),*)),
            HpkeKdfType::HkdfSha384 => dispatch!(@aead $kem, HkdfSha384, $aead, $f($($arg),*)),
            HpkeKdfType::HkdfSha512 => dispatch!(@aead $kem, HkdfSha512, $aead, $f($($arg),*)),
        }
    };
    (@aead $kem:ty, $kdf:ty, $aead:expr, $f:ident($($arg:expr),*)) => {
        match $aead {
            HpkeAeadType::AesGcm128 => $f::<AesGcm128, $kdf, $kem>($($arg),*),
            HpkeAeadType::AesGcm256 => $f::<AesGcm256, $kdf, $kem>($($arg),*),
            HpkeAeadType::ChaCha20Poly1305 => $f::<ChaCha20Poly1305, $kdf, $kem>($($arg),*),
            HpkeAeadType::Export => $f::<ExportOnlyAead, $kdf, $kem>($($arg),*),
        }
    };
}

/// `(enc, ciphertext)` of SealBase(pkR, info, aad, pt).
pub(crate) fn seal<R>(
    config: HpkeConfig,
    pk_r: &[u8],
    info: &[u8],
    aad: &[u8],
    pt: &[u8],
    rng: &mut R,
) -> Result<(Vec<u8>, Vec<u8>), Fail>
where
    R: rand_core::RngCore + rand_core::CryptoRng,
{
    dispatch!(config, seal_t(pk_r, info, aad, pt, rng))
}

fn seal_t<A: Aead, K: Kdf, M: Kem>(
    pk_r: &[u8],
    info: &[u8],
    aad: &[u8],
    pt: &[u8],
    rng: &mut (impl rand_core::RngCore + rand_core::CryptoRng),
) -> Result<(Vec<u8>, Vec<u8>), Fail> {
    let pk = M::PublicKey::from_bytes(pk_r)?;
    let (enc, ct) =
        hpke::single_shot_seal::<A, K, M, _>(&OpModeS::Base, &pk, info, pt, aad, &mut Rng09(rng))?;
    Ok((enc.to_bytes().to_vec(), ct))
}

/// OpenBase(enc, skR, info, aad, ct).
pub(crate) fn open(
    config: HpkeConfig,
    enc: &[u8],
    sk_r: &[u8],
    info: &[u8],
    aad: &[u8],
    ct: &[u8],
) -> Result<Vec<u8>, Fail> {
    dispatch!(config, open_t(enc, sk_r, info, aad, ct))
}

fn open_t<A: Aead, K: Kdf, M: Kem>(
    enc: &[u8],
    sk_r: &[u8],
    info: &[u8],
    aad: &[u8],
    ct: &[u8],
) -> Result<Vec<u8>, Fail> {
    let sk = M::PrivateKey::from_bytes(sk_r)?;
    let enc = M::EncappedKey::from_bytes(enc)?;
    Ok(hpke::single_shot_open::<A, K, M>(
        &OpModeR::Base,
        &sk,
        &enc,
        info,
        ct,
        aad,
    )?)
}

/// SetupBaseS(pkR, info), then Export(exporter_context, len): `(enc, secret)`.
pub(crate) fn sender_export<R>(
    config: HpkeConfig,
    pk_r: &[u8],
    info: &[u8],
    exporter_context: &[u8],
    len: usize,
    rng: &mut R,
) -> Result<(Vec<u8>, Vec<u8>), Fail>
where
    R: rand_core::RngCore + rand_core::CryptoRng,
{
    dispatch!(
        config,
        sender_export_t(pk_r, info, exporter_context, len, rng)
    )
}

fn sender_export_t<A: Aead, K: Kdf, M: Kem>(
    pk_r: &[u8],
    info: &[u8],
    exporter_context: &[u8],
    len: usize,
    rng: &mut (impl rand_core::RngCore + rand_core::CryptoRng),
) -> Result<(Vec<u8>, Vec<u8>), Fail> {
    let pk = M::PublicKey::from_bytes(pk_r)?;
    let (enc, ctx) = hpke::setup_sender::<A, K, M, _>(&OpModeS::Base, &pk, info, &mut Rng09(rng))?;
    let mut out = vec![0u8; len];
    ctx.export(exporter_context, &mut out)?;
    Ok((enc.to_bytes().to_vec(), out))
}

/// SetupBaseR(enc, skR, info), then Export(exporter_context, len).
pub(crate) fn receiver_export(
    config: HpkeConfig,
    enc: &[u8],
    sk_r: &[u8],
    info: &[u8],
    exporter_context: &[u8],
    len: usize,
) -> Result<Vec<u8>, Fail> {
    dispatch!(
        config,
        receiver_export_t(enc, sk_r, info, exporter_context, len)
    )
}

fn receiver_export_t<A: Aead, K: Kdf, M: Kem>(
    enc: &[u8],
    sk_r: &[u8],
    info: &[u8],
    exporter_context: &[u8],
    len: usize,
) -> Result<Vec<u8>, Fail> {
    let sk = M::PrivateKey::from_bytes(sk_r)?;
    let enc = M::EncappedKey::from_bytes(enc)?;
    let ctx = hpke::setup_receiver::<A, K, M>(&OpModeR::Base, &sk, &enc, info)?;
    let mut out = vec![0u8; len];
    ctx.export(exporter_context, &mut out)?;
    Ok(out)
}

/// DeriveKeyPair(ikm) of the suite's KEM: `(private, public)`.
pub(crate) fn derive_keypair(config: HpkeConfig, ikm: &[u8]) -> Result<(Vec<u8>, Vec<u8>), Fail> {
    // The KDF and AEAD do not take part in DeriveKeyPair (the KEM has its own KDF); dispatching
    // on the whole config still refuses an unsupported KEM the same way.
    dispatch!(config, derive_t(ikm))
}

#[allow(clippy::extra_unused_type_parameters)]
fn derive_t<A: Aead, K: Kdf, M: Kem>(ikm: &[u8]) -> Result<(Vec<u8>, Vec<u8>), Fail> {
    let (sk, pk) = M::derive_keypair(ikm);
    Ok((sk.to_bytes().to_vec(), pk.to_bytes().to_vec()))
}

/// RFC 9180 Appendix A (the base-mode vectors of the supported KEMs, every KDF and AEAD the
/// published set has for them), through the crate-internal entry points, so the sender side can
/// be fed the vector's ephemeral IKM: `hpke` draws Nsk bytes from the RNG and runs
/// DeriveKeyPair on them, so an RNG that returns `ikmE` reproduces `enc` and the ciphertext.
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "rfc9180-vectors")]
    use serde_json::Value;

    /// Returns the bytes it was given, then panics (a test RNG; never a CSPRNG).
    struct Replay(Vec<u8>, usize);

    impl rand_core::RngCore for Replay {
        fn next_u32(&mut self) -> u32 {
            let mut b = [0u8; 4];
            self.fill_bytes(&mut b);
            u32::from_le_bytes(b)
        }
        fn next_u64(&mut self) -> u64 {
            let mut b = [0u8; 8];
            self.fill_bytes(&mut b);
            u64::from_le_bytes(b)
        }
        fn fill_bytes(&mut self, dst: &mut [u8]) {
            let end = self.1 + dst.len();
            assert!(end <= self.0.len(), "Replay RNG exhausted");
            dst.copy_from_slice(&self.0[self.1..end]);
            self.1 = end;
        }
        fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), rand_core::Error> {
            self.fill_bytes(dst);
            Ok(())
        }
    }

    impl rand_core::CryptoRng for Replay {}

    #[cfg(feature = "rfc9180-vectors")]
    fn h(v: &Value, k: &str) -> Vec<u8> {
        hex::decode(v[k].as_str().unwrap_or_else(|| panic!("{k}"))).unwrap()
    }

    #[cfg(feature = "rfc9180-vectors")]
    fn config(v: &Value) -> HpkeConfig {
        let kem = match v["kem_id"].as_u64().unwrap() {
            0x10 => HpkeKemType::DhKemP256,
            0x20 => HpkeKemType::DhKem25519,
            k => panic!("kem {k}"),
        };
        let kdf = match v["kdf_id"].as_u64().unwrap() {
            1 => HpkeKdfType::HkdfSha256,
            2 => HpkeKdfType::HkdfSha384,
            3 => HpkeKdfType::HkdfSha512,
            k => panic!("kdf {k}"),
        };
        let aead = match v["aead_id"].as_u64().unwrap() {
            1 => HpkeAeadType::AesGcm128,
            2 => HpkeAeadType::AesGcm256,
            3 => HpkeAeadType::ChaCha20Poly1305,
            0xffff => HpkeAeadType::Export,
            a => panic!("aead {a}"),
        };
        HpkeConfig(kem, kdf, aead)
    }

    // The vectors are in Sew's repository, not in the package (feature `rfc9180-vectors`).
    #[cfg(feature = "rfc9180-vectors")]
    fn vectors() -> Vec<Value> {
        let v: Value = serde_json::from_str(include_str!("../tests/vectors/rfc9180.json")).unwrap();
        v.as_array().unwrap().clone()
    }

    #[cfg(feature = "rfc9180-vectors")]
    #[test]
    fn rfc9180_base_mode_vectors() {
        let all = vectors();
        assert_eq!(
            all.len(),
            16,
            "the vector subset (X25519 and P-256; 8 suites each)"
        );
        let mut sealed = 0;
        for v in &all {
            assert_eq!(v["mode"], 0);
            let name = format!(
                "kem {} kdf {} aead {}",
                v["kem_id"], v["kdf_id"], v["aead_id"]
            );
            let (sk_r, pk_r) = (h(v, "skRm"), h(v, "pkRm"));
            let info = h(v, "info");
            // DeriveKeyPair, both key pairs.
            assert_eq!(
                derive_keypair(config(v), &h(v, "ikmR")).unwrap(),
                (sk_r.clone(), pk_r.clone()),
                "{name}: DeriveKeyPair(ikmR)"
            );
            assert_eq!(
                derive_keypair(config(v), &h(v, "ikmE")).unwrap(),
                (h(v, "skEm"), h(v, "pkEm")),
                "{name}: DeriveKeyPair(ikmE)"
            );
            let enc = h(v, "enc");
            // Exports, both sides (the sender side with the vector's ephemeral key).
            for x in v["exports"].as_array().unwrap() {
                let ctx = h(x, "exporter_context");
                let len = usize::try_from(x["L"].as_u64().unwrap()).unwrap();
                let want = h(x, "exported_value");
                let got = receiver_export(config(v), &enc, &sk_r, &info, &ctx, len).unwrap();
                assert_eq!(got, want, "{name}: receiver export");
                let mut rng = Replay(h(v, "ikmE"), 0);
                let sent = sender_export(config(v), &pk_r, &info, &ctx, len, &mut rng).unwrap();
                assert_eq!(sent, (enc.clone(), want), "{name}: sender export");
            }
            // Seal and open (sequence number 0; export-only suites refuse both).
            for x in v["encryptions"].as_array().unwrap() {
                assert_eq!(x["seq"], 0);
                let (pt, aad, ct) = (h(x, "pt"), h(x, "aad"), h(x, "ct"));
                let mut rng = Replay(h(v, "ikmE"), 0);
                let s = seal(config(v), &pk_r, &info, &aad, &pt, &mut rng);
                let o = open(config(v), &enc, &sk_r, &info, &aad, &ct);
                if v["aead_id"] == 0xffff {
                    assert!(
                        s.is_err() && o.is_err(),
                        "{name}: export-only seals nothing"
                    );
                    continue;
                }
                assert_eq!(s.unwrap(), (enc.clone(), ct.clone()), "{name}: SealBase");
                assert_eq!(o.unwrap(), pt, "{name}: OpenBase");
                let mut bad = ct.clone();
                bad[0] ^= 1;
                assert_eq!(
                    open(config(v), &enc, &sk_r, &info, &aad, &bad),
                    Err(Fail::Other),
                    "{name}: a flipped bit fails"
                );
                sealed += 1;
            }
        }
        assert_eq!(sealed, 12, "every non-export-only suite sealed and opened");
    }

    #[test]
    fn bad_inputs_and_unsupported_kems() {
        let x25519 = || {
            HpkeConfig(
                HpkeKemType::DhKem25519,
                HpkeKdfType::HkdfSha256,
                HpkeAeadType::AesGcm128,
            )
        };
        let mut rng = Replay(vec![9u8; 64], 0);
        assert_eq!(
            seal(x25519(), &[1u8; 31], b"", b"", b"pt", &mut rng),
            Err(Fail::Input),
            "a short X25519 public key"
        );
        assert_eq!(
            open(x25519(), &[1u8; 31], &[2u8; 32], b"", b"", &[0u8; 32]),
            Err(Fail::Input),
            "a short enc"
        );
        // The all-zero X25519 point gives an all-zero DH output, which RFC 9180 §7.1.4 refuses.
        let mut rng = Replay(vec![9u8; 64], 0);
        assert!(seal(x25519(), &[0u8; 32], b"", b"", b"pt", &mut rng).is_err());
        let p256 = HpkeConfig(
            HpkeKemType::DhKemP256,
            HpkeKdfType::HkdfSha256,
            HpkeAeadType::AesGcm128,
        );
        let mut rng = Replay(vec![9u8; 64], 0);
        assert_eq!(
            seal(p256, &[4u8; 65], b"", b"", b"pt", &mut rng),
            Err(Fail::Input),
            "a P-256 point off the curve"
        );
        for kem in [
            HpkeKemType::DhKemP384,
            HpkeKemType::DhKemP521,
            HpkeKemType::DhKem448,
        ] {
            let c = HpkeConfig(kem, HpkeKdfType::HkdfSha256, HpkeAeadType::AesGcm128);
            assert_eq!(derive_keypair(c, &[0u8; 64]), Err(Fail::Unsupported));
        }
    }
}
