//! The hardware device key: a non-exportable P-256 key in the Secure Enclave (iOS) or
//! StrongBox/TEE (Android), reached through the host's [`HardwareDeviceKey`].
//!
//! The device key signs the enrolment proof and the per-call KeyPackages (about once per join).
//! The MLS leaf keys are separate per-call keys held in memory and never touch it. With a
//! hardware key the non-secret record (device id, certificate) stays in the public device file
//! ([`super::DeviceFile`]); only the private key is in hardware, and Sew sees only public keys
//! and signatures.
//!
//! - **No software fallback.** With [`super::StoreChoice::Hardware`] every signature comes from
//!   the host's key. An error from it fails the call's key setup (`e2ee_refused`); the engine
//!   never generates or stores a software key instead.
//! - **A key that is gone** (a restored backup, a wiped Keystore): `public_key` answers
//!   `Ok(None)` and a new device enrols with a new key (`generate`). The old device is revoked by
//!   the person or the admin, as today.
//! - **Signatures**: the host returns the platform's DER signature as is
//!   (iOS `.ecdsaSignatureMessageX962SHA256`, Android `"SHA256withECDSA"`). The engine converts
//!   it to the raw `r‖s` Sew wants, normalises it to low-S and checks it under the key's public
//!   key before using it.

use std::fmt;
use std::sync::Arc;

use sew_e2ee::SoftwareSigner;
use sew_e2ee::identity::DeviceSigner as _;
use sew_e2ee::identity::cert::{valid_p256_point, verify_p256};

use super::{DeviceError, DeviceKeyId};
use crate::provider::BoxFuture;

/// A non-exportable P-256 key in the Secure Enclave (iOS) or StrongBox/TEE (Android).
/// Sew sees only public keys and signatures.
///
/// The engine calls it from its own task on the async runtime, never with the Tauri plugin's
/// lock held, so a method may take its time (StrongBox can take 100 ms or more): run the
/// platform call on your own executor. The returned futures borrow only `self`; copy `id` and
/// `msg` into them. Keychain tag / Keystore alias: [`DeviceKeyId::account`].
///
/// Access: iOS `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly` with no user presence;
/// Android with no user authentication, so a lock-screen answer can sign. Map platform errors to
/// [`DeviceError::Unavailable`]: the join then fails with `e2ee_refused`.
pub trait HardwareDeviceKey: Send + Sync + 'static {
    /// The key's SEC1 public key (65 bytes, uncompressed), if one exists for `id`.
    fn public_key(&self, id: &DeviceKeyId) -> BoxFuture<'_, Result<Option<[u8; 65]>, DeviceError>>;
    /// Create a key for `id` (replacing any old one); return its public key.
    fn generate(&self, id: &DeviceKeyId) -> BoxFuture<'_, Result<[u8; 65], DeviceError>>;
    /// ECDSA P-256 with SHA-256 over `msg`, as the platform does it (iOS
    /// .ecdsaSignatureMessageX962SHA256, Android "SHA256withECDSA"). Return the platform's
    /// DER signature (an ASN.1 SEQUENCE of r and s) as is; Sew converts it to r||s and normalises low-S.
    fn sign(&self, id: &DeviceKeyId, msg: &[u8]) -> BoxFuture<'_, Result<Vec<u8>, DeviceError>>;
    /// Delete the key (no error if absent).
    fn delete(&self, id: &DeviceKeyId) -> BoxFuture<'_, Result<(), DeviceError>>;
}

/// The device's signing key as the key session holds it: a software key (the built-in and host
/// stores) or the host's hardware key.
#[derive(Clone)]
pub struct DeviceKey {
    inner: Inner,
}

#[derive(Clone)]
enum Inner {
    Software(Arc<SoftwareSigner>),
    Hardware {
        key: Arc<dyn HardwareDeviceKey>,
        id: DeviceKeyId,
        public: [u8; 65],
    },
}

impl fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            Inner::Software(_) => f.write_str("DeviceKey(software)"),
            Inner::Hardware { .. } => f.write_str("DeviceKey(hardware)"),
        }
    }
}

impl DeviceKey {
    /// A software key.
    pub fn software(signer: SoftwareSigner) -> DeviceKey {
        DeviceKey {
            inner: Inner::Software(Arc::new(signer)),
        }
    }

    /// The host's hardware key of `id`, whose public key is `public` (checked to be a P-256
    /// point).
    pub fn hardware(
        key: Arc<dyn HardwareDeviceKey>,
        id: DeviceKeyId,
        public: [u8; 65],
    ) -> Result<DeviceKey, DeviceError> {
        if !valid_p256_point(&public) {
            return Err(DeviceError::Unavailable(
                "the hardware key's public key is not a P-256 point".to_owned(),
            ));
        }
        Ok(DeviceKey {
            inner: Inner::Hardware { key, id, public },
        })
    }

    /// Whether the key is the host's hardware key.
    pub fn is_hardware(&self) -> bool {
        matches!(self.inner, Inner::Hardware { .. })
    }

    /// The uncompressed SEC1 public key (65 bytes).
    pub fn public_key_sec1(&self) -> [u8; 65] {
        match &self.inner {
            Inner::Software(s) => s.public_key_sec1(),
            Inner::Hardware { public, .. } => *public,
        }
    }

    /// The software key's scalar, for a store that persists it (`None` for a hardware key).
    pub(super) fn software_bytes(&self) -> Option<zeroize::Zeroizing<[u8; 32]>> {
        match &self.inner {
            Inner::Software(s) => Some(s.to_bytes()),
            Inner::Hardware { .. } => None,
        }
    }

    /// The software key itself, when this is the only holder of it.
    pub(super) fn into_software(self) -> Option<SoftwareSigner> {
        match self.inner {
            Inner::Software(s) => Arc::try_unwrap(s).ok(),
            Inner::Hardware { .. } => None,
        }
    }

    /// ECDSA-P256-SHA256 over `msg` as raw `r‖s`, low-S. A hardware key's DER answer is
    /// converted, normalised and checked under its public key; anything else is
    /// [`DeviceError::Unavailable`].
    pub async fn sign(&self, msg: &[u8]) -> Result<[u8; 64], DeviceError> {
        match &self.inner {
            Inner::Software(s) => Ok(s.sign(msg)),
            Inner::Hardware { key, id, public } => {
                let der = key.sign(id, msg).await?;
                let raw = p1363_from_der(&der).ok_or_else(|| {
                    DeviceError::Unavailable(
                        "the hardware key's signature is not a DER ECDSA signature".to_owned(),
                    )
                })?;
                if !verify_p256(public, msg, &raw) {
                    return Err(DeviceError::Unavailable(
                        "the hardware key's signature does not verify under its public key"
                            .to_owned(),
                    ));
                }
                Ok(raw)
            }
        }
    }
}

/// The P-256 group order n, big-endian.
const N: [u8; 32] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0xBC, 0xE6, 0xFA, 0xAD, 0xA7, 0x17, 0x9E, 0x84, 0xF3, 0xB9, 0xCA, 0xC2, 0xFC, 0x63, 0x25, 0x51,
];
/// ⌊n / 2⌋, big-endian: an `s` above it is high.
const HALF_N: [u8; 32] = [
    0x7F, 0xFF, 0xFF, 0xFF, 0x80, 0x00, 0x00, 0x00, 0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0xDE, 0x73, 0x7D, 0x56, 0xD3, 0x8B, 0xCF, 0x42, 0x79, 0xDC, 0xE5, 0x61, 0x7E, 0x31, 0x92, 0xA8,
];

/// `a - b` for 256-bit big-endian numbers with `a >= b`.
fn sub(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let mut d = i16::from(a[i]) - i16::from(b[i]) - borrow;
        borrow = 0;
        if d < 0 {
            d += 256;
            borrow = 1;
        }
        out[i] = d as u8;
    }
    out
}

/// One DER INTEGER (positive, minimally encoded, at most 32 value bytes) at the start of `b`:
/// the value left-padded to 32 bytes and the rest of `b`.
fn der_uint(b: &[u8]) -> Option<([u8; 32], &[u8])> {
    let (&tag, b) = b.split_first()?;
    let (&len, b) = b.split_first()?;
    let len = usize::from(len);
    if tag != 0x02 || len == 0 || len > 33 || b.len() < len {
        return None;
    }
    let (v, rest) = b.split_at(len);
    // Negative numbers are not signatures.
    if v[0] & 0x80 != 0 {
        return None;
    }
    // Minimal: a leading zero only before a byte with the high bit set.
    let v = if v[0] == 0 {
        if v.len() == 1 || v[1] & 0x80 == 0 {
            return None;
        }
        &v[1..]
    } else {
        v
    };
    if v.len() > 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out[32 - v.len()..].copy_from_slice(v);
    Some((out, rest))
}

/// A DER ECDSA-P256 signature `SEQUENCE { INTEGER r, INTEGER s }` as raw `r‖s`, with
/// `s` normalised to low-S (`n - s` when `s > n/2`). `None` for anything that is not exactly
/// one such signature with `0 < r, s < n`.
pub fn p1363_from_der(der: &[u8]) -> Option<[u8; 64]> {
    let (&tag, b) = der.split_first()?;
    let (&len, b) = b.split_first()?;
    // Short-form length only: a P-256 signature is at most 72 bytes.
    if tag != 0x30 || len & 0x80 != 0 || usize::from(len) != b.len() {
        return None;
    }
    let (r, b) = der_uint(b)?;
    let (mut s, b) = der_uint(b)?;
    if !b.is_empty() {
        return None;
    }
    let zero = [0u8; 32];
    if r == zero || s == zero || r >= N || s >= N {
        return None;
    }
    if s > HALF_N {
        s = sub(&N, &s);
    }
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&r);
    out[32..].copy_from_slice(&s);
    Some(out)
}

/// A raw `r‖s` signature as DER (the shape a platform answers; the simulator key's and the
/// tests'). With `high_s`, `s` is replaced by `n - s` first (the other valid signature).
#[cfg(any(feature = "sim-software-key", test))]
pub(crate) fn der_from_p1363(raw: &[u8; 64], high_s: bool) -> Vec<u8> {
    let r: [u8; 32] = raw[..32].try_into().unwrap_or([0; 32]);
    let mut s: [u8; 32] = raw[32..].try_into().unwrap_or([0; 32]);
    if high_s && s <= HALF_N {
        s = sub(&N, &s);
    }
    let int = |v: &[u8; 32]| {
        let start = v.iter().position(|&x| x != 0).unwrap_or(31);
        let v = &v[start..];
        let mut out = vec![0x02];
        if v[0] & 0x80 != 0 {
            out.push((v.len() + 1) as u8);
            out.push(0);
        } else {
            out.push(v.len() as u8);
        }
        out.extend_from_slice(v);
        out
    };
    let (r, s) = (int(&r), int(&s));
    let mut out = vec![0x30, (r.len() + s.len()) as u8];
    out.extend(r);
    out.extend(s);
    out
}

/// SIMULATOR AND EMULATOR ONLY (feature `sim-software-key`): a software key behind the
/// [`HardwareDeviceKey`] interface, for the iOS simulator (no Secure Enclave) and the x86
/// Android emulator, so a host runs its hardware-key path there. It answers DER like the
/// platforms. The feature does not build for any other target (a `compile_error!`): a phone or
/// desktop build can never ship it.
#[cfg(any(feature = "sim-software-key", test))]
pub struct SimSoftwareKey {
    dir: Option<std::path::PathBuf>,
    keys: std::sync::Mutex<std::collections::BTreeMap<DeviceKeyId, Arc<SoftwareSigner>>>,
}

#[cfg(any(feature = "sim-software-key", test))]
impl fmt::Debug for SimSoftwareKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SimSoftwareKey(***)")
    }
}

#[cfg(any(feature = "sim-software-key", test))]
impl SimSoftwareKey {
    /// Keys in memory only (a new device enrols on every launch).
    pub fn in_memory() -> SimSoftwareKey {
        SimSoftwareKey {
            dir: None,
            keys: Default::default(),
        }
    }

    /// Keys also in 0600 files under `dir` (`<install>-<member>/device-<scope>.key`), so a
    /// simulator keeps its device across launches.
    pub fn in_dir(dir: impl Into<std::path::PathBuf>) -> SimSoftwareKey {
        SimSoftwareKey {
            dir: Some(dir.into()),
            keys: Default::default(),
        }
    }

    fn file(&self, id: &DeviceKeyId) -> Option<super::FileSecretStore> {
        let dir = self.dir.as_ref()?;
        Some(super::FileSecretStore::new(
            dir.join(format!("{}-{}", id.install_id, id.member_id)),
        ))
    }

    fn get(&self, id: &DeviceKeyId) -> Result<Option<Arc<SoftwareSigner>>, DeviceError> {
        use super::SecretStore as _;
        if let Some(k) = crate::e2ee::keys::lock(&self.keys).get(id) {
            return Ok(Some(k.clone()));
        }
        let Some(file) = self.file(id) else {
            return Ok(None);
        };
        let Some(scalar) = file.load(&id.account())? else {
            return Ok(None);
        };
        let Some(signer) = SoftwareSigner::from_bytes(&scalar) else {
            return Ok(None);
        };
        let signer = Arc::new(signer);
        crate::e2ee::keys::lock(&self.keys).insert(id.clone(), signer.clone());
        Ok(Some(signer))
    }
}

#[cfg(any(feature = "sim-software-key", test))]
impl HardwareDeviceKey for SimSoftwareKey {
    fn public_key(&self, id: &DeviceKeyId) -> BoxFuture<'_, Result<Option<[u8; 65]>, DeviceError>> {
        let id = id.clone();
        Box::pin(async move { Ok(self.get(&id)?.map(|k| k.public_key_sec1())) })
    }

    fn generate(&self, id: &DeviceKeyId) -> BoxFuture<'_, Result<[u8; 65], DeviceError>> {
        use super::SecretStore as _;
        let id = id.clone();
        Box::pin(async move {
            let signer = SoftwareSigner::generate(&mut rand::rngs::OsRng);
            if let Some(file) = self.file(&id) {
                file.save(&id.account(), &signer.to_bytes())?;
            }
            let public = signer.public_key_sec1();
            crate::e2ee::keys::lock(&self.keys).insert(id, Arc::new(signer));
            Ok(public)
        })
    }

    fn sign(&self, id: &DeviceKeyId, msg: &[u8]) -> BoxFuture<'_, Result<Vec<u8>, DeviceError>> {
        let (id, msg) = (id.clone(), msg.to_vec());
        Box::pin(async move {
            let k = self
                .get(&id)?
                .ok_or_else(|| DeviceError::Unavailable("no key".to_owned()))?;
            Ok(der_from_p1363(&k.sign(&msg), false))
        })
    }

    fn delete(&self, id: &DeviceKeyId) -> BoxFuture<'_, Result<(), DeviceError>> {
        use super::SecretStore as _;
        let id = id.clone();
        Box::pin(async move {
            crate::e2ee::keys::lock(&self.keys).remove(&id);
            if let Some(file) = self.file(&id) {
                file.delete(&id.account())?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn id() -> DeviceKeyId {
        DeviceKeyId {
            install_id: 7,
            member_id: sew_proto::id::Id::new(5).unwrap(),
            device_scope: "abc".to_owned(),
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }

    #[test]
    fn der_round_trips_and_normalises_high_s() {
        let k = SoftwareSigner::generate(&mut rand::rngs::OsRng);
        let pk = k.public_key_sec1();
        for i in 0..64u8 {
            let msg = [i; 40];
            let raw = k.sign(&msg);
            let low = p1363_from_der(&der_from_p1363(&raw, false)).unwrap();
            let high = p1363_from_der(&der_from_p1363(&raw, true)).unwrap();
            assert_eq!(
                low, high,
                "the high-S twin normalises to the same signature"
            );
            let s: [u8; 32] = low[32..].try_into().unwrap();
            assert!(s <= HALF_N, "low-S");
            assert!(verify_p256(&pk, &msg, &low));
            assert_eq!(low[..32], raw[..32], "r is kept");
        }
    }

    #[test]
    fn der_parsing_is_strict() {
        let k = SoftwareSigner::generate(&mut rand::rngs::OsRng);
        let good = der_from_p1363(&k.sign(b"m"), false);
        assert!(p1363_from_der(&good).is_some());
        // Trailing bytes, a wrong outer tag, a length that lies, truncation.
        let mut t = good.clone();
        t.push(0);
        assert!(p1363_from_der(&t).is_none());
        let mut t = good.clone();
        t[0] = 0x31;
        assert!(p1363_from_der(&t).is_none());
        let mut t = good.clone();
        t[1] += 1;
        assert!(p1363_from_der(&t).is_none());
        assert!(p1363_from_der(&good[..good.len() - 1]).is_none());
        assert!(p1363_from_der(&[]).is_none());
        // Raw r‖s is not DER.
        assert!(p1363_from_der(&k.sign(b"m")).is_none());
        // Zero, negative, non-minimal and too-long integers; r ≥ n.
        assert!(p1363_from_der(&[0x30, 0x06, 0x02, 0x01, 0x00, 0x02, 0x01, 0x01]).is_none());
        assert!(p1363_from_der(&[0x30, 0x06, 0x02, 0x01, 0x81, 0x02, 0x01, 0x01]).is_none());
        assert!(p1363_from_der(&[0x30, 0x07, 0x02, 0x02, 0x00, 0x01, 0x02, 0x01, 0x01]).is_none());
        let mut big = vec![0x30, 0x26, 0x02, 0x21, 0x01];
        big.extend([0u8; 32]);
        big.extend([0x02, 0x01, 0x01]);
        assert!(p1363_from_der(&big).is_none());
        let mut n = vec![0x30, 0x26, 0x02, 0x21, 0x00];
        n.extend(N);
        n.extend([0x02, 0x01, 0x01]);
        assert!(p1363_from_der(&n).is_none());
        // Small values are fine (short integers, left-padded).
        let small = p1363_from_der(&[0x30, 0x06, 0x02, 0x01, 0x05, 0x02, 0x01, 0x07]).unwrap();
        assert_eq!(small[31], 5);
        assert_eq!(small[63], 7);
        assert!(small[..31].iter().all(|&b| b == 0));
    }

    /// A key that answers whatever the test says.
    struct Rogue(Vec<u8>, [u8; 65]);

    impl HardwareDeviceKey for Rogue {
        fn public_key(
            &self,
            _: &DeviceKeyId,
        ) -> BoxFuture<'_, Result<Option<[u8; 65]>, DeviceError>> {
            Box::pin(async move { Ok(Some(self.1)) })
        }
        fn generate(&self, _: &DeviceKeyId) -> BoxFuture<'_, Result<[u8; 65], DeviceError>> {
            Box::pin(async move { Ok(self.1) })
        }
        fn sign(&self, _: &DeviceKeyId, _: &[u8]) -> BoxFuture<'_, Result<Vec<u8>, DeviceError>> {
            Box::pin(async move { Ok(self.0.clone()) })
        }
        fn delete(&self, _: &DeviceKeyId) -> BoxFuture<'_, Result<(), DeviceError>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn a_hardware_signature_is_converted_normalised_and_checked() {
        let rt = rt();
        let sim = Arc::new(SimSoftwareKey::in_memory());
        let pk = rt.block_on(sim.generate(&id())).unwrap();
        let key = DeviceKey::hardware(sim.clone(), id(), pk).unwrap();
        assert!(key.is_hardware());
        assert_eq!(key.public_key_sec1(), pk);
        assert!(key.software_bytes().is_none());
        let raw = rt.block_on(key.sign(b"enrol me")).unwrap();
        assert!(verify_p256(&pk, b"enrol me", &raw));

        // A high-S DER answer from the platform comes out low-S.
        let other = SoftwareSigner::generate(&mut rand::rngs::OsRng);
        let opk = other.public_key_sec1();
        let sig = other.sign(b"x");
        let rogue = Arc::new(Rogue(der_from_p1363(&sig, true), opk));
        let key = DeviceKey::hardware(rogue, id(), opk).unwrap();
        let raw = rt.block_on(key.sign(b"x")).unwrap();
        let s: [u8; 32] = raw[32..].try_into().unwrap();
        assert!(s <= HALF_N);
        assert!(verify_p256(&opk, b"x", &raw));

        // Raw r‖s instead of DER, and a signature by another key, are refused.
        let rogue = Arc::new(Rogue(sig.to_vec(), opk));
        let key = DeviceKey::hardware(rogue, id(), opk).unwrap();
        assert!(matches!(
            rt.block_on(key.sign(b"x")),
            Err(DeviceError::Unavailable(_))
        ));
        let rogue = Arc::new(Rogue(der_from_p1363(&sig, false), pk));
        let key = DeviceKey::hardware(rogue, id(), pk).unwrap();
        assert!(matches!(
            rt.block_on(key.sign(b"x")),
            Err(DeviceError::Unavailable(_))
        ));
        // A public key that is not a point.
        assert!(matches!(
            DeviceKey::hardware(sim, id(), [4u8; 65]),
            Err(DeviceError::Unavailable(_))
        ));
    }

    #[test]
    fn the_simulator_key_keeps_its_key_in_its_dir() {
        let rt = rt();
        // No tempfile: the packaged crate (the sew-crates mirror) has no dev-dependencies.
        let dir = TestDir::new("sim-key");
        let sim = SimSoftwareKey::in_dir(dir.path());
        assert_eq!(rt.block_on(sim.public_key(&id())).unwrap(), None);
        let pk = rt.block_on(sim.generate(&id())).unwrap();
        // Another launch reads it back.
        let again = SimSoftwareKey::in_dir(dir.path());
        assert_eq!(rt.block_on(again.public_key(&id())).unwrap(), Some(pk));
        let der = rt.block_on(again.sign(&id(), b"m")).unwrap();
        assert!(verify_p256(&pk, b"m", &p1363_from_der(&der).unwrap()));
        rt.block_on(again.delete(&id())).unwrap();
        rt.block_on(again.delete(&id())).unwrap();
        assert_eq!(
            rt.block_on(SimSoftwareKey::in_dir(dir.path()).public_key(&id()))
                .unwrap(),
            None
        );
        assert!(matches!(
            rt.block_on(again.sign(&id(), b"m")),
            Err(DeviceError::Unavailable(_))
        ));
    }

    /// A fresh directory under the system temp dir, removed on drop.
    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(tag: &str) -> TestDir {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("sew-call-client-{tag}-{pid}-{n}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TestDir(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
