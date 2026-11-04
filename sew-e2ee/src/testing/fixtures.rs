//! The standard fixtures: install `acme` with a test CA and DS key
//! (pinned), members with enrolled `SoftwareSigner` devices, and a call scope.
//!
//! Keys come from fixed seeds so vectors are reproducible; the device keys come from a seeded
//! RNG per fixture.

use ed25519_dalek::{Signer as _, SigningKey};
use rand::SeedableRng;
use rand::rngs::StdRng;
use sew_proto::e2ee::tls::{
    DeviceCertTbs, LeafBindingTableTbs, RecorderCert, RecorderCertTbs, SewDeviceCert, TlsCodec,
    platform,
};
use sew_proto::e2ee::{
    B64, CallPublicKey, CallScope, DeviceCertificateResponse, KeyPackageRefItem, LeafBindingTable,
    ReadyCallKeys, VerificationCode,
};
use sew_proto::time::Timestamp;

use crate::config::Params;
use crate::identity::{DeviceIdentity, DeviceSigner, Enrollment, SoftwareSigner};
use crate::ids::{Id, UnixMs, install_id_bytes};
use crate::keypackage::{E2eeClient, PreparedKp};
use crate::store::{DeviceStore, MemoryDeviceStore, StoredDevice};
use crate::trust::{TrustAnchors, external_sender_bytes};

/// The fixed install id of the Std fixture.
pub const INSTALL_ID: u64 = 48_831_765_147_549_707;
/// `#lounge`'s channel id.
pub const LOUNGE: u64 = 48_831_765_147_549_698;
/// `#stage`'s channel id (another call's scope).
pub const STAGE: u64 = 48_831_765_147_549_699;
/// Device certificates live 30 days.
pub const CERT_DAYS: u64 = 30;
/// The fixture's default clock: 2026-10-01T00:00:00Z in unix ms.
pub const T0: UnixMs = 1_790_812_800_000;

/// A Sew id from a raw value (fixtures only).
pub fn id(v: u64) -> Id {
    Id::new(v).unwrap_or(Id::new_const(1))
}

/// The test CA (`sew-call-ca`).
#[derive(Debug, Clone)]
pub struct TestCa {
    key: SigningKey,
}

impl TestCa {
    /// From a seed.
    pub fn from_seed(seed: [u8; 32]) -> TestCa {
        TestCa {
            key: SigningKey::from_bytes(&seed),
        }
    }

    /// The public key.
    pub fn public(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    /// `kid` (hex of the first 8 bytes of SHA-256 of the key).
    pub fn kid(&self) -> String {
        use sha2::Digest as _;
        let d = sha2::Sha256::digest(self.public());
        d[..8].iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Signs a device certificate.
    pub fn sign_device(&self, tbs: DeviceCertTbs) -> SewDeviceCert {
        let mut cert = SewDeviceCert {
            tbs,
            ca_sig: Vec::new(),
        };
        let msg = cert.signed_bytes().unwrap_or_default();
        cert.ca_sig = self.key.sign(&msg).to_bytes().to_vec();
        cert
    }

    /// Signs a recorder certificate.
    pub fn sign_recorder(&self, tbs: RecorderCertTbs) -> RecorderCert {
        let mut cert = RecorderCert {
            tbs,
            ca_sig: Vec::new(),
        };
        let msg = cert.signed_bytes().unwrap_or_default();
        cert.ca_sig = self.key.sign(&msg).to_bytes().to_vec();
        cert
    }
}

/// The test delivery-service key (`sew-call-ds`).
#[derive(Debug, Clone)]
pub struct TestDs {
    key: SigningKey,
}

impl TestDs {
    /// From a seed.
    pub fn from_seed(seed: [u8; 32]) -> TestDs {
        TestDs {
            key: SigningKey::from_bytes(&seed),
        }
    }

    /// The public key.
    pub fn public(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    /// The signing key (MiniDs signs external proposals with it).
    pub fn signing_key(&self) -> &SigningKey {
        &self.key
    }

    /// Signs a binding table and returns its JSON form.
    pub fn sign_table(&self, tbs: &LeafBindingTableTbs) -> LeafBindingTable {
        let msg = tbs.signed_bytes().unwrap_or_default();
        let sig = self.key.sign(&msg).to_bytes().to_vec();
        match LeafBindingTable::from_tbs(tbs, sig) {
            Ok(t) => t,
            Err(e) => panic!("fixture: a malformed binding table: {e}"),
        }
    }

    /// Signs arbitrary bytes (crafted tables in tests).
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.key.sign(msg).to_bytes()
    }
}

/// An install: CA, DS, READY's `call_keys`.
#[derive(Debug, Clone)]
pub struct Install {
    /// The install id.
    pub install_id: u64,
    /// The CA.
    pub ca: TestCa,
    /// The DS key.
    pub ds: TestDs,
    /// The CRL.
    pub crl: Vec<u64>,
    /// The verification-code policy.
    pub verification_code: VerificationCode,
    next_serial: u64,
}

impl Install {
    /// The Std install `acme` (fixed keys).
    pub fn acme() -> Install {
        Install {
            install_id: INSTALL_ID,
            ca: TestCa::from_seed([0x11; 32]),
            ds: TestDs::from_seed([0x22; 32]),
            crl: Vec::new(),
            verification_code: VerificationCode::Shown,
            next_serial: 48_831_765_147_550_100,
        }
    }

    /// The `external_sender` bytes.
    pub fn external_sender(&self) -> Vec<u8> {
        external_sender_bytes(&self.ds.public(), self.install_id)
    }

    /// READY's `call_keys`.
    pub fn ready(&self) -> ReadyCallKeys {
        ReadyCallKeys {
            external_sender: B64::new(self.external_sender()),
            ds_public_key: B64::new(self.ds.public().to_vec()),
            ca: CallPublicKey {
                kid: self.ca.kid(),
                public_key: B64::new(self.ca.public().to_vec()),
            },
            crl: self.crl.iter().map(|s| id(*s)).collect(),
            crl_partial: false,
            verification_code: self.verification_code,
            params: Params::default_wire(),
        }
    }

    /// A fresh serial.
    pub fn serial(&mut self) -> u64 {
        self.next_serial += 1;
        self.next_serial
    }

    /// Issues a device certificate (what the server's enrollment would answer).
    pub fn issue(
        &mut self,
        member: Id,
        device: Id,
        pubkey: &[u8; 65],
        label: &str,
        now: UnixMs,
    ) -> DeviceCertificateResponse {
        let serial = self.serial();
        let nb = now / 1000;
        let tbs = DeviceCertTbs {
            version: 1,
            install_id: install_id_bytes(self.install_id),
            member_id: member.get(),
            device_id: device.get(),
            device_pubkey: pubkey.to_vec(),
            platform: platform::DESKTOP,
            label: label.as_bytes().to_vec(),
            serial,
            not_before: nb,
            not_after: nb + CERT_DAYS * 86_400,
        };
        let cert = self.ca.sign_device(tbs);
        DeviceCertificateResponse {
            device_id: device,
            serial: id(serial),
            cert: B64::new(cert.to_bytes().unwrap_or_default()),
            not_after: Timestamp::from_unix_ms_saturating((nb + CERT_DAYS * 86_400) * 1000),
        }
    }

    /// The scope of `#lounge`.
    pub fn lounge() -> CallScope {
        CallScope::call(id(LOUNGE))
    }

    /// The scope of `#stage`.
    pub fn stage() -> CallScope {
        CallScope::call(id(STAGE))
    }

    /// Enrolls a device for `member` with a seeded software key; returns the ready client.
    pub fn enroll(&mut self, member: u64, device: u64, seed: u64, now: UnixMs) -> TestDevice {
        let mut rng = StdRng::seed_from_u64(seed);
        let signer = SoftwareSigner::generate(&mut rng);
        let pubkey = signer.public_key_sec1();
        let mut store = MemoryDeviceStore::new();
        let trust = match TrustAnchors::from_ready(&self.ready(), self.install_id, &mut store) {
            Ok(t) => t,
            Err(e) => panic!("fixture: READY refused: {e}"),
        };
        let resp = self.issue(id(member), id(device), &pubkey, "Test device", now);
        let stored = match Enrollment::accept(resp, &trust, id(member), &pubkey, now) {
            Ok(s) => s,
            Err(e) => panic!("fixture: enrollment refused: {e}"),
        };
        if let Err(e) = store.save(&stored) {
            panic!("fixture: store: {e}");
        }
        let identity = match DeviceIdentity::load(&store, &trust, id(member), now) {
            Ok(Some(i)) => i,
            Ok(None) => panic!("fixture: no device stored"),
            Err(e) => panic!("fixture: device refused: {e}"),
        };
        TestDevice {
            signer,
            store,
            stored,
            client: E2eeClient::new(identity, trust),
        }
    }
}

/// An enrolled device with a software key.
#[derive(Debug)]
pub struct TestDevice {
    /// The device key.
    pub signer: SoftwareSigner,
    /// Its store.
    pub store: MemoryDeviceStore,
    /// The stored device.
    pub stored: StoredDevice,
    /// The client.
    pub client: E2eeClient,
}

impl TestDevice {
    /// Prepares, signs and "uploads" a package (the ref is its own `hash_ref`, valid 1 h).
    pub fn prepare(&mut self, scope: CallScope, now: UnixMs) -> PreparedKp {
        let (pending, req) = match self.client.prepare_key_package(scope, now) {
            Ok(v) => v,
            Err(e) => panic!("fixture: prepare: {e}"),
        };
        let sig = self.signer.sign(&req.message);
        let mut kp = match pending.finish(sig) {
            Ok(k) => k,
            Err(e) => panic!("fixture: finish: {e}"),
        };
        let item = KeyPackageRefItem {
            key_package_ref: B64::new(kp.key_package_ref().to_vec()),
            expires_at_ms: now + 3_600_000,
        };
        if let Err(e) = kp.set_ref(&item) {
            panic!("fixture: set_ref: {e}");
        }
        kp
    }

    /// The member.
    pub fn member_id(&self) -> Id {
        self.stored.member_id
    }
}
