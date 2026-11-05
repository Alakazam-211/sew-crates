//! Feature `rust-crypto` (Sew 0.7.4): the call PC's pure-Rust DTLS/SRTP provider for str0m. DTLS is
//! dimpl with its RustCrypto backend (named explicitly: dimpl would otherwise take its aws-lc-rs
//! backend whenever that is compiled too, as on a desktop build with this feature); SRTP is
//! RustCrypto's AES-CTR, AES-GCM and HMAC-SHA1; the DTLS certificate is a fresh self-signed
//! P-256 one from rcgen on ring (which rustls already builds), so no aws-lc-rs on a phone.
//!
//! The cipher and DTLS glue follows str0m's own `str0m-rust-crypto` crate (MIT OR Apache-2.0,
//! Copyright Martin Algesten; see NOTICE), which can't be used as is: its dimpl `rcgen` feature
//! pulls aws-lc-rs for the certificate.

use std::sync::{Arc, OnceLock};
use std::time::Instant;

use aes::cipher::{BlockEncrypt as _, KeyInit as _};
use aes_gcm::aead::Payload;
use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::{Aes128Gcm, Aes256Gcm, Nonce};
use ctr::cipher::{KeyIvInit as _, StreamCipher as _};
use hmac::{Hmac, Mac as _};
use sha2::Digest as _;
use str0m::crypto::dtls::{
    DtlsCert, DtlsImplError, DtlsInstance, DtlsOutput, DtlsProvider, DtlsVersion, ProtocolVersion,
};
use str0m::crypto::{
    AeadAes128Gcm, AeadAes128GcmCipher, AeadAes256Gcm, AeadAes256GcmCipher, Aes128CmSha1_80Cipher,
    CryptoError, CryptoProvider, Sha1HmacProvider, Sha256Provider, SrtpProvider,
    SupportedAeadAes128Gcm, SupportedAeadAes256Gcm, SupportedAes128CmSha1_80,
};

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// The provider (one per process).
pub(crate) fn provider() -> Arc<CryptoProvider> {
    static P: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    P.get_or_init(|| {
        Arc::new(CryptoProvider {
            srtp_provider: &Srtp,
            sha1_hmac_provider: &Sha1Hmac,
            sha256_provider: &Sha256,
            dtls_provider: &Dtls,
        })
    })
    .clone()
}

// ------------------------------------------------------------------ hashes

#[derive(Debug)]
struct Sha1Hmac;

impl Sha1HmacProvider for Sha1Hmac {
    fn sha1_hmac(&self, key: &[u8], payloads: &[&[u8]]) -> [u8; 20] {
        // HMAC takes a key of any length: `new_from_slice` never fails.
        let Ok(mut mac) = <Hmac<sha1::Sha1> as hmac::Mac>::new_from_slice(key) else {
            return [0; 20];
        };
        for p in payloads {
            mac.update(p);
        }
        mac.finalize().into_bytes().into()
    }
}

#[derive(Debug)]
struct Sha256;

impl Sha256Provider for Sha256 {
    fn sha256(&self, data: &[u8]) -> [u8; 32] {
        sha2::Sha256::digest(data).into()
    }
}

// ------------------------------------------------------------------ SRTP

#[derive(Debug)]
struct Srtp;

#[derive(Debug)]
struct CmFactory;

#[derive(Debug)]
struct Gcm128Factory;

#[derive(Debug)]
struct Gcm256Factory;

struct Cm {
    key: [u8; 16],
}

impl std::fmt::Debug for Cm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cm").finish_non_exhaustive()
    }
}

struct Gcm128(Aes128Gcm);

impl std::fmt::Debug for Gcm128 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gcm128").finish_non_exhaustive()
    }
}

struct Gcm256(Aes256Gcm);

impl std::fmt::Debug for Gcm256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gcm256").finish_non_exhaustive()
    }
}

impl SupportedAes128CmSha1_80 for CmFactory {
    fn create_cipher(&self, key: [u8; 16], _encrypt: bool) -> Box<dyn Aes128CmSha1_80Cipher> {
        Box::new(Cm { key })
    }
}

impl SupportedAeadAes128Gcm for Gcm128Factory {
    fn create_cipher(&self, key: [u8; 16], _encrypt: bool) -> Box<dyn AeadAes128GcmCipher> {
        Box::new(Gcm128(Aes128Gcm::new(GenericArray::from_slice(&key))))
    }
}

impl SupportedAeadAes256Gcm for Gcm256Factory {
    fn create_cipher(&self, key: [u8; 32], _encrypt: bool) -> Box<dyn AeadAes256GcmCipher> {
        Box::new(Gcm256(Aes256Gcm::new(GenericArray::from_slice(&key))))
    }
}

fn short(what: &str) -> CryptoError {
    CryptoError::Other(format!("{what}: output buffer too short"))
}

impl Aes128CmSha1_80Cipher for Cm {
    fn encrypt(
        &mut self,
        iv: &[u8; 16],
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(), CryptoError> {
        let out = output
            .get_mut(..input.len())
            .ok_or_else(|| short("AES-CM"))?;
        out.copy_from_slice(input);
        Aes128Ctr::new(&self.key.into(), iv.into()).apply_keystream(out);
        Ok(())
    }

    fn decrypt(
        &mut self,
        iv: &[u8; 16],
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(), CryptoError> {
        // CTR is its own inverse.
        self.encrypt(iv, input, output)
    }
}

/// The one AAD slice, or the slices joined.
fn joined<'a>(aads: &[&'a [u8]], buf: &'a mut Vec<u8>) -> &'a [u8] {
    if let [one] = aads {
        return one;
    }
    *buf = aads.concat();
    buf
}

fn gcm_seal(
    c: &impl aes_gcm::aead::Aead,
    iv: &[u8; 12],
    aad: &[u8],
    input: &[u8],
    output: &mut [u8],
) -> Result<(), CryptoError> {
    let sealed = c
        .encrypt(Nonce::from_slice(iv), Payload { msg: input, aad })
        .map_err(|e| CryptoError::Other(format!("AES-GCM encrypt failed: {e:?}")))?;
    output
        .get_mut(..sealed.len())
        .ok_or_else(|| short("AES-GCM"))?
        .copy_from_slice(&sealed);
    Ok(())
}

fn gcm_open(
    c: &impl aes_gcm::aead::Aead,
    iv: &[u8; 12],
    aads: &[&[u8]],
    input: &[u8],
    output: &mut [u8],
) -> Result<usize, CryptoError> {
    let mut buf = Vec::new();
    let aad = joined(aads, &mut buf);
    let plain = c
        .decrypt(Nonce::from_slice(iv), Payload { msg: input, aad })
        .map_err(|e| CryptoError::Other(format!("AES-GCM decrypt failed: {e:?}")))?;
    output
        .get_mut(..plain.len())
        .ok_or_else(|| short("AES-GCM"))?
        .copy_from_slice(&plain);
    Ok(plain.len())
}

impl AeadAes128GcmCipher for Gcm128 {
    fn encrypt(
        &mut self,
        iv: &[u8; AeadAes128Gcm::IV_LEN],
        aad: &[u8],
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(), CryptoError> {
        gcm_seal(&self.0, iv, aad, input, output)
    }

    fn decrypt(
        &mut self,
        iv: &[u8; AeadAes128Gcm::IV_LEN],
        aads: &[&[u8]],
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, CryptoError> {
        gcm_open(&self.0, iv, aads, input, output)
    }
}

impl AeadAes256GcmCipher for Gcm256 {
    fn encrypt(
        &mut self,
        iv: &[u8; AeadAes256Gcm::IV_LEN],
        aad: &[u8],
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(), CryptoError> {
        gcm_seal(&self.0, iv, aad, input, output)
    }

    fn decrypt(
        &mut self,
        iv: &[u8; AeadAes256Gcm::IV_LEN],
        aads: &[&[u8]],
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, CryptoError> {
        gcm_open(&self.0, iv, aads, input, output)
    }
}

/// One AES-ECB round of the SRTP key derivation as str0m asks for it: the 16-byte input block,
/// then a full PKCS#7 padding block (16 × 0x10), into 32 bytes of output.
fn ecb_round(encrypt: impl Fn(&mut aes::Block), input: &[u8], output: &mut [u8]) {
    if input.len() != 16 || output.len() < 32 {
        // str0m always passes 16 in and 32 out; anything else is its bug.
        output.fill(0);
        return;
    }
    let mut b1 = aes::Block::clone_from_slice(input);
    encrypt(&mut b1);
    output[..16].copy_from_slice(&b1);
    let mut b2 = aes::Block::from([0x10u8; 16]);
    encrypt(&mut b2);
    output[16..32].copy_from_slice(&b2);
}

impl SrtpProvider for Srtp {
    fn aes_128_cm_sha1_80(&self) -> &'static dyn SupportedAes128CmSha1_80 {
        &CmFactory
    }

    fn aead_aes_128_gcm(&self) -> &'static dyn SupportedAeadAes128Gcm {
        &Gcm128Factory
    }

    fn aead_aes_256_gcm(&self) -> &'static dyn SupportedAeadAes256Gcm {
        &Gcm256Factory
    }

    fn srtp_aes_128_ecb_round(&self, key: &[u8], input: &[u8], output: &mut [u8]) {
        match aes::Aes128::new_from_slice(key) {
            Ok(c) => ecb_round(|b| c.encrypt_block(b), input, output),
            Err(_) => output.fill(0),
        }
    }

    fn srtp_aes_256_ecb_round(&self, key: &[u8], input: &[u8], output: &mut [u8]) {
        match aes::Aes256::new_from_slice(key) {
            Ok(c) => ecb_round(|b| c.encrypt_block(b), input, output),
            Err(_) => output.fill(0),
        }
    }
}

// ------------------------------------------------------------------ DTLS

#[derive(Debug)]
struct Dtls;

/// A self-signed P-256 certificate (DER) and its PKCS#8 key (DER), as dimpl takes them. The
/// serial is random (Firefox refuses two peers with the same issuer and serial).
fn certificate() -> Option<DtlsCert> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).ok()?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).ok()?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "sew-call-client");
    params.is_ca = rcgen::IsCa::NoCa;
    let serial: [u8; 16] = rand::random();
    // A positive serial (the top bit clear), never zero.
    let mut serial = serial.to_vec();
    serial[0] = (serial[0] & 0x7f) | 0x01;
    params.serial_number = Some(serial.into());
    let cert = params.self_signed(&key).ok()?;
    Some(DtlsCert {
        certificate: cert.der().to_vec(),
        private_key: key.serialize_der(),
    })
}

impl DtlsProvider for Dtls {
    fn generate_certificate(&self) -> Option<DtlsCert> {
        certificate()
    }

    fn new_dtls(
        &self,
        cert: &DtlsCert,
        now: Instant,
        dtls_version: DtlsVersion,
        mtu: Option<usize>,
    ) -> Result<Box<dyn DtlsInstance>, CryptoError> {
        // ICE checks return routability before DTLS: no server cookie (as str0m's providers).
        let mut b = dimpl::Config::builder()
            .use_server_cookie(false)
            .with_crypto_provider(dimpl::crypto::rust_crypto::default_provider());
        if let Some(mtu) = mtu {
            b = b.mtu(mtu);
        }
        let config = Arc::new(
            b.build()
                .map_err(|e| CryptoError::Other(format!("dimpl config: {e}")))?,
        );
        let c = dimpl::DtlsCertificate {
            certificate: cert.certificate.clone(),
            private_key: cert.private_key.clone(),
        };
        let dtls = match dtls_version {
            DtlsVersion::Dtls12 => dimpl::Dtls::new_12(config, c, now),
            DtlsVersion::Dtls13 => dimpl::Dtls::new_13(config, c, now),
            DtlsVersion::Auto => dimpl::Dtls::new_auto(config, c, now),
            v => return Err(CryptoError::Other(format!("unsupported DTLS version {v}"))),
        };
        Ok(Box::new(Instance(dtls)))
    }
}

struct Instance(dimpl::Dtls);

impl std::fmt::Debug for Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instance").finish_non_exhaustive()
    }
}

impl DtlsInstance for Instance {
    fn set_active(&mut self, active: bool) {
        self.0.set_active(active);
    }

    fn handle_packet(&mut self, packet: &[u8]) -> Result<(), DtlsImplError> {
        self.0.handle_packet(packet)
    }

    fn poll_output<'a>(&mut self, buf: &'a mut [u8]) -> DtlsOutput<'a> {
        self.0.poll_output(buf)
    }

    fn handle_timeout(&mut self, now: Instant) -> Result<(), DtlsImplError> {
        self.0.handle_timeout(now)
    }

    fn send_application_data(&mut self, data: &[u8]) -> Result<(), DtlsImplError> {
        self.0.send_application_data(data)
    }

    fn is_active(&self) -> bool {
        self.0.is_active()
    }

    fn protocol_version(&self) -> Option<ProtocolVersion> {
        self.0.protocol_version()
    }

    fn is_closing(&self) -> bool {
        self.0.is_closing()
    }

    fn is_closed(&self) -> bool {
        self.0.is_closed()
    }

    fn close(&mut self) -> Result<(), DtlsImplError> {
        self.0.close()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_provider_is_this_crates() {
        let p = provider();
        assert_eq!(format!("{:?}", p.dtls_provider), "Dtls");
        assert_eq!(format!("{:?}", p.srtp_provider), "Srtp");
        let cert = p
            .dtls_provider
            .generate_certificate()
            .expect("a certificate");
        assert!(!cert.certificate.is_empty() && !cert.private_key.is_empty());
        let other = p.dtls_provider.generate_certificate().unwrap();
        assert_ne!(cert.certificate, other.certificate, "a fresh key each time");
    }

    #[test]
    fn known_answers() {
        let p = provider();
        // RFC 2202 §3 test case 2, in two payloads.
        assert_eq!(
            hex::encode(
                p.sha1_hmac_provider
                    .sha1_hmac(b"Jefe", &[b"what do ya want ", b"for nothing?"])
            ),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
        );
        assert_eq!(
            hex::encode(p.sha256_provider.sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // FIPS-197 C.1: AES-128 of 00112233…ff under 000102…0f.
        let key: Vec<u8> = (0u8..16).collect();
        let input = hex::decode("00112233445566778899aabbccddeeff").unwrap();
        let mut out = [0u8; 32];
        p.srtp_provider
            .srtp_aes_128_ecb_round(&key, &input, &mut out);
        assert_eq!(hex::encode(&out[..16]), "69c4e0d86a7b0430d8cdb78070b4c55a");
        // FIPS-197 C.3: AES-256.
        let key: Vec<u8> = (0u8..32).collect();
        p.srtp_provider
            .srtp_aes_256_ecb_round(&key, &input, &mut out);
        assert_eq!(hex::encode(&out[..16]), "8ea2b7ca516745bfeafc49904b496089");
    }

    /// The provider against str0m's aws-lc-rs one (on in this build: a desktop target): every
    /// SRTP profile's cipher both ways, the derivation rounds, the hashes, and DTLS 1.2
    /// handshakes in both roles that agree on the keying material and the profile.
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    mod against_aws_lc {
        use super::*;

        fn aws() -> CryptoProvider {
            let p = str0m::crypto::from_feature_flags();
            assert!(
                format!("{:?}", p.dtls_provider).contains("AwsLc"),
                "str0m's feature provider is aws-lc-rs here: {:?}",
                p.dtls_provider
            );
            p
        }

        #[test]
        fn srtp_ciphers_interoperate() {
            let ours = provider();
            let aws = aws();
            let iv16 = [7u8; 16];
            let iv12 = [9u8; 12];
            let aad = [1u8; 12];
            let msg: Vec<u8> = (0..200u32).map(|i| (i * 7) as u8).collect();
            for (a, b) in [
                (ours.srtp_provider, aws.srtp_provider),
                (aws.srtp_provider, ours.srtp_provider),
            ] {
                // AES128_CM_SHA1_80.
                let mut ct = vec![0u8; msg.len()];
                a.aes_128_cm_sha1_80()
                    .create_cipher([3; 16], true)
                    .encrypt(&iv16, &msg, &mut ct)
                    .unwrap();
                assert_ne!(ct, msg);
                let mut pt = vec![0u8; msg.len()];
                b.aes_128_cm_sha1_80()
                    .create_cipher([3; 16], false)
                    .decrypt(&iv16, &ct, &mut pt)
                    .unwrap();
                assert_eq!(pt, msg, "AES-CM");
                // AEAD_AES_128_GCM.
                let mut ct = vec![0u8; msg.len() + AeadAes128Gcm::TAG_LEN];
                a.aead_aes_128_gcm()
                    .create_cipher([4; 16], true)
                    .encrypt(&iv12, &aad, &msg, &mut ct)
                    .unwrap();
                let mut pt = vec![0u8; ct.len()];
                let n = b
                    .aead_aes_128_gcm()
                    .create_cipher([4; 16], false)
                    .decrypt(&iv12, &[&aad[..4], &aad[4..]], &ct, &mut pt)
                    .unwrap();
                assert_eq!(&pt[..n], &msg[..], "AES-128-GCM");
                let mut bad = ct.clone();
                bad[0] ^= 1;
                assert!(
                    b.aead_aes_128_gcm()
                        .create_cipher([4; 16], false)
                        .decrypt(&iv12, &[&aad], &bad, &mut pt)
                        .is_err(),
                    "a flipped bit fails the tag"
                );
                // AEAD_AES_256_GCM.
                let mut ct = vec![0u8; msg.len() + AeadAes256Gcm::TAG_LEN];
                a.aead_aes_256_gcm()
                    .create_cipher([5; 32], true)
                    .encrypt(&iv12, &aad, &msg, &mut ct)
                    .unwrap();
                let mut pt = vec![0u8; ct.len()];
                let n = b
                    .aead_aes_256_gcm()
                    .create_cipher([5; 32], false)
                    .decrypt(&iv12, &[&aad], &ct, &mut pt)
                    .unwrap();
                assert_eq!(&pt[..n], &msg[..], "AES-256-GCM");
            }
            let (mut x, mut y) = ([0u8; 32], [0u8; 32]);
            ours.srtp_provider
                .srtp_aes_128_ecb_round(&[6; 16], &[8; 16], &mut x);
            aws.srtp_provider
                .srtp_aes_128_ecb_round(&[6; 16], &[8; 16], &mut y);
            // str0m reads the first block only (aws-lc-rs leaves the padding block zero).
            assert_eq!(x[..16], y[..16], "AES-128 derivation round");
            ours.srtp_provider
                .srtp_aes_256_ecb_round(&[6; 32], &[8; 16], &mut x);
            aws.srtp_provider
                .srtp_aes_256_ecb_round(&[6; 32], &[8; 16], &mut y);
            assert_eq!(x[..16], y[..16], "AES-256 derivation round");
            assert_eq!(
                ours.sha1_hmac_provider.sha1_hmac(b"k", &[b"a", b"bc"]),
                aws.sha1_hmac_provider.sha1_hmac(b"k", &[b"abc"])
            );
            assert_eq!(
                ours.sha256_provider.sha256(&msg),
                aws.sha256_provider.sha256(&msg)
            );
        }

        /// One side's result: keying material, SRTP profile, the peer's certificate.
        type Side = (Vec<u8>, String, Vec<u8>);

        /// Drives a client (`active`) and a server to completion in memory; returns each side's
        /// keying material and profile.
        fn handshake(client: &CryptoProvider, server: &CryptoProvider) -> [Side; 2] {
            let now = Instant::now();
            let cc = client.dtls_provider.generate_certificate().unwrap();
            let sc = server.dtls_provider.generate_certificate().unwrap();
            let mut c = client
                .dtls_provider
                .new_dtls(&cc, now, DtlsVersion::Dtls12, None)
                .unwrap();
            let mut s = server
                .dtls_provider
                .new_dtls(&sc, now, DtlsVersion::Dtls12, None)
                .unwrap();
            c.set_active(true);
            s.set_active(false);
            c.handle_timeout(now).unwrap();
            s.handle_timeout(now).unwrap();
            let mut got: [Option<Side>; 2] = [None, None];
            let mut certs: [Option<Vec<u8>>; 2] = [None, None];
            let mut buf = vec![0u8; 65_536];
            for _ in 0..200 {
                let mut moved = false;
                for side in 0..2 {
                    let (from, to) = if side == 0 {
                        (&mut c, &mut s)
                    } else {
                        (&mut s, &mut c)
                    };
                    loop {
                        match from.poll_output(&mut buf) {
                            DtlsOutput::Packet(p) => {
                                let p = p.to_vec();
                                to.handle_packet(&p).unwrap();
                                moved = true;
                            }
                            DtlsOutput::PeerCert(der) => certs[side] = Some(der.to_vec()),
                            DtlsOutput::KeyingMaterial(m, profile) => {
                                got[side] = Some((
                                    m.to_vec(),
                                    format!("{profile:?}"),
                                    certs[side].clone().unwrap_or_default(),
                                ));
                            }
                            DtlsOutput::Timeout(_) => break,
                            DtlsOutput::BufferTooSmall { needed } => {
                                panic!("a {needed}-byte DTLS output")
                            }
                            _ => {}
                        }
                    }
                }
                if got.iter().all(Option::is_some) && !moved {
                    break;
                }
            }
            let [a, b] = got;
            let (a, b) = (a.expect("client keys"), b.expect("server keys"));
            assert_eq!(
                a.2, sc.certificate,
                "the client saw the server's certificate"
            );
            assert_eq!(
                b.2, cc.certificate,
                "the server saw the client's certificate"
            );
            [a, b]
        }

        #[test]
        fn dtls_handshakes_interoperate_both_roles() {
            let ours = provider();
            let aws = aws();
            for (name, client, server) in [
                ("rust-crypto client, aws-lc server", &*ours, &aws),
                ("aws-lc client, rust-crypto server", &aws, &*ours),
            ] {
                let [c, s] = handshake(client, server);
                assert_eq!(c.0, s.0, "{name}: the same keying material");
                assert_eq!(c.1, s.1, "{name}: the same profile");
                assert!(!c.0.is_empty(), "{name}: keying material");
                assert!(
                    c.1.contains("GCM"),
                    "{name}: dimpl's preferred AEAD: {}",
                    c.1
                );
            }
        }
    }
}
