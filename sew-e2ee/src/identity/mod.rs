//! Device identity, certificates, the call credential and the binding table.

pub mod binding;
pub mod cert;
pub mod credential;
pub mod signer;

use sew_proto::e2ee::tls::enroll_proof_input;
use sew_proto::e2ee::{B64, DeviceCertificateResponse, DeviceEnrollRequest, DevicePlatform};

use crate::error::IdentityError;
use crate::ids::{MemberId, UnixMs};
use crate::store::{DeviceStore, StoredDevice};
use crate::trust::TrustAnchors;

pub use binding::{VerifiedEntry, VerifiedTable, verify_table};
pub use cert::{CertContext, VerifiedCert, verify_device_cert};
pub use credential::{LeafCheckError, LeafContext, LeafIdentity, check_leaf};
pub use signer::{DeviceSigner, SoftwareSigner};

/// Bytes the device key must sign (raw `r‖s` answer).
#[derive(Clone, PartialEq, Eq)]
pub struct SignRequest {
    /// The message.
    pub message: Vec<u8>,
}

impl std::fmt::Debug for SignRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SignRequest({} bytes)", self.message.len())
    }
}

/// The device's own verified identity: its certificate and public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    stored: StoredDevice,
    cert: VerifiedCert,
}

/// Certificate clock skew.
pub const CLOCK_SKEW_S: u64 = 300;

fn own_cert(
    stored: &StoredDevice,
    trust: &TrustAnchors,
    me: MemberId,
    now: UnixMs,
) -> Result<VerifiedCert, IdentityError> {
    let ctx = CertContext {
        install_id: trust.install_id(),
        ca_public: trust.ca_public(),
        skew_s: CLOCK_SKEW_S,
        now,
    };
    let cert = verify_device_cert(&stored.cert, &ctx)?;
    if cert.member_id != me {
        return Err(IdentityError::WrongMember);
    }
    if cert.device_pubkey() != stored.pubkey.as_slice() {
        return Err(IdentityError::WrongKey);
    }
    if trust.crl().contains(&cert.serial()) {
        return Err(IdentityError::BadCert("revoked"));
    }
    Ok(cert)
}

impl DeviceIdentity {
    /// Loads and verifies the stored device of this install (own-certificate checks:
    /// member = the signed-in member, key = the device's own). `Ok(None)`: not enrolled.
    pub fn load(
        store: &impl DeviceStore,
        trust: &TrustAnchors,
        me: MemberId,
        now: UnixMs,
    ) -> Result<Option<DeviceIdentity>, IdentityError> {
        let Some(stored) = store.load(trust.install_id()) else {
            return Ok(None);
        };
        let cert = own_cert(&stored, trust, me, now)?;
        Ok(Some(DeviceIdentity { stored, cert }))
    }

    /// From a stored device (already verified by [`Enrollment::accept`]).
    pub fn from_stored(
        stored: StoredDevice,
        trust: &TrustAnchors,
        me: MemberId,
        now: UnixMs,
    ) -> Result<DeviceIdentity, IdentityError> {
        let cert = own_cert(&stored, trust, me, now)?;
        Ok(DeviceIdentity { stored, cert })
    }

    /// The member.
    pub fn member_id(&self) -> MemberId {
        self.stored.member_id
    }

    /// The device id.
    pub fn device_id(&self) -> crate::ids::Id {
        self.stored.device_id
    }

    /// The install.
    pub fn install_id(&self) -> u64 {
        self.stored.install_id
    }

    /// The certificate's TLS bytes.
    pub fn cert_bytes(&self) -> &[u8] {
        &self.stored.cert
    }

    /// The verified certificate.
    pub fn cert(&self) -> &VerifiedCert {
        &self.cert
    }

    /// The device public key.
    pub fn pubkey(&self) -> &[u8; 65] {
        &self.stored.pubkey
    }

    /// The stored state.
    pub fn stored(&self) -> &StoredDevice {
        &self.stored
    }
}

/// An enrollment in progress (`POST /api/v1/devices`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollDraft {
    pubkey: [u8; 65],
    platform: DevicePlatform,
    label: String,
}

/// What to do after an enrollment or renewal failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrollAction {
    /// Retry later with backoff (no user action).
    Retry,
    /// The store was cleared: make a new key and enroll again.
    ReEnroll,
}

/// Enrollment and certificate renewal. The core builds bodies and verifies answers;
/// the app or the driver sends them.
#[derive(Debug, Clone, Copy)]
pub struct Enrollment;

impl Enrollment {
    /// Starts an enrollment: the proof message is `"sew call device enroll v1" ‖ u64
    /// install_id ‖ u64 member_id ‖ pubkey`, signed by the device key (raw `r‖s`).
    pub fn begin(
        pubkey: &[u8; 65],
        platform: DevicePlatform,
        label: &str,
        install_id: u64,
        me: MemberId,
    ) -> (EnrollDraft, SignRequest) {
        let mut label = label.to_owned();
        while label.len() > sew_proto::e2ee::DEVICE_LABEL_MAX_BYTES {
            label.pop();
        }
        (
            EnrollDraft {
                pubkey: *pubkey,
                platform,
                label,
            },
            SignRequest {
                message: enroll_proof_input(install_id, me.get(), pubkey),
            },
        )
    }

    /// The enrollment body.
    pub fn request(draft: EnrollDraft, proof: [u8; 64]) -> DeviceEnrollRequest {
        DeviceEnrollRequest {
            pubkey: B64::new(draft.pubkey.to_vec()),
            platform: draft.platform,
            label: draft.label,
            proof: B64::new(proof.to_vec()),
        }
    }

    /// Verifies an enrollment or renewal answer (the certificate and the own-certificate checks)
    /// and returns the state to store.
    pub fn accept(
        resp: DeviceCertificateResponse,
        trust: &TrustAnchors,
        me: MemberId,
        pubkey: &[u8; 65],
        now: UnixMs,
    ) -> Result<StoredDevice, IdentityError> {
        let stored = StoredDevice {
            install_id: trust.install_id(),
            member_id: me,
            device_id: resp.device_id,
            pubkey: *pubkey,
            cert: resp.cert.as_bytes().to_vec(),
        };
        let cert = own_cert(&stored, trust, me, now)?;
        if cert.device_id != resp.device_id || cert.serial() != resp.serial.get() {
            return Err(IdentityError::BadCert("response_mismatch"));
        }
        Ok(stored)
    }

    /// Whether the certificate has less than 7 days left (silent renewal).
    pub fn needs_renewal(dev: &DeviceIdentity, now: UnixMs) -> bool {
        let left = dev.cert().not_after_s().saturating_sub(now / 1000);
        left < crate::config::E2eeConfig::default().cert_renew_before_s
    }

    /// The renewal body (`{}`).
    pub fn renew_body() -> serde_json::Value {
        serde_json::json!({})
    }

    /// What to do with an enrollment or renewal failure: `409 device_key_in_use` (another member's
    /// key) and `422 device_revoked` clear the store and re-enroll with a new key; anything else
    /// retries with backoff.
    pub fn on_failure(
        store: &mut impl DeviceStore,
        install_id: u64,
        status: u16,
        reason: Option<&str>,
    ) -> EnrollAction {
        use sew_proto::e2ee::reasons;
        match (status, reason) {
            (409, Some(r)) if r == reasons::DEVICE_KEY_IN_USE => {
                store.clear(install_id);
                EnrollAction::ReEnroll
            }
            (422, Some(r)) if r == reasons::DEVICE_REVOKED => {
                store.clear(install_id);
                EnrollAction::ReEnroll
            }
            _ => EnrollAction::Retry,
        }
    }
}
