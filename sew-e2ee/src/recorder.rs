//! The recorder key handoff.
//!
//! A consenting device seals its **own** frame IKM of an epoch to the recorder's per-recording
//! X25519 key:
//!
//! ```text
//! info = "sew call recorder release v1" ‖ TLS(ReleaseInfo{ call_id, recording_id, group_id,
//!                                                     epoch, uint32 leaf_index, uint8 slot,
//!                                                     sender_identity })
//! (enc, ct) = HPKE.SealBase(pkR = hpke_pub, info, aad = "", pt = ikm_self(e))   // 32 + 48 bytes
//! ```
//!
//! The recorder rebuilds `info` from its **own** `call_id` and `recording_id` and the
//! release's other fields, so a release for another call, recording, group, epoch, leaf, slot or
//! sender fails AEAD.

use std::fmt;

use openmls_traits::crypto::OpenMlsCrypto;
use openmls_traits::types::{HpkeAeadType, HpkeCiphertext, HpkeConfig, HpkeKdfType, HpkeKemType};
use rand::{CryptoRng, RngCore};
use sew_mls_crypto::MlsCrypto;
use sew_proto::e2ee::Release;
use sew_proto::e2ee::tls::{RecorderCert, RecorderCertTbs, ReleaseInfo, TlsCodec};
use zeroize::Zeroizing;

use crate::config::caps;
use crate::error::ReleaseError;
use crate::identity::cert::verify_recorder_cert_sig;
use crate::ids::{Epoch, LkIdentity, RoomName, Slot, UnixMs, install_id_bytes};
use crate::keys::FrameIkm;

/// DHKEM(X25519), HKDF-SHA256, AES-128-GCM.
pub const RELEASE_HPKE: HpkeConfig = HpkeConfig(
    HpkeKemType::DhKem25519,
    HpkeKdfType::HkdfSha256,
    HpkeAeadType::AesGcm128,
);

/// The longest a recorder certificate may be valid (24 h).
pub const RECORDER_CERT_MAX_S: u64 = 24 * 3600;

/// Why a recorder announcement was refused (no release is ever sent for it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RecorderCertError {
    /// Malformed or over the size cap.
    #[error("recorder certificate: decode")]
    Decode,
    /// The CA signature does not verify under the pin.
    #[error("recorder certificate: signature")]
    Signature,
    /// Another install, call, recording or HPKE key than announced.
    #[error("recorder certificate: binding ({0})")]
    Binding(&'static str),
    /// Outside its validity (or valid for more than 24 h).
    #[error("recorder certificate: validity")]
    Validity,
}

/// Verifies a recorder certificate against the announcement (the certificate's first condition).
pub fn verify_recorder_cert(
    bytes: &[u8],
    ca_public: &[u8; 32],
    install_id: u64,
    call_id: &RoomName,
    recording_id: u64,
    hpke_pub: &[u8; 32],
    now: UnixMs,
) -> Result<RecorderCertTbs, RecorderCertError> {
    if bytes.len() > caps::RECORDER_CERT {
        return Err(RecorderCertError::Decode);
    }
    let cert = RecorderCert::from_bytes(bytes).map_err(|_| RecorderCertError::Decode)?;
    if !verify_recorder_cert_sig(ca_public, &cert) {
        return Err(RecorderCertError::Signature);
    }
    let t = cert.tbs;
    if t.install_id != install_id_bytes(install_id) {
        return Err(RecorderCertError::Binding("install"));
    }
    if t.call_id != call_id.as_str().as_bytes() {
        return Err(RecorderCertError::Binding("call_id"));
    }
    if t.recording_id != recording_id {
        return Err(RecorderCertError::Binding("recording_id"));
    }
    if &t.hpke_pub != hpke_pub {
        return Err(RecorderCertError::Binding("hpke_pub"));
    }
    let now_s = now / 1000;
    if now_s < t.not_before
        || now_s > t.not_after
        || t.not_after.saturating_sub(t.not_before) > RECORDER_CERT_MAX_S
    {
        return Err(RecorderCertError::Validity);
    }
    Ok(t)
}

/// Seals `ikm` for a recorder: `(enc, ct)` (32 and 48 bytes). `None` if HPKE fails.
pub fn seal_release(
    hpke_pub: &[u8; 32],
    info: &ReleaseInfo,
    ikm: &FrameIkm,
) -> Option<(Vec<u8>, Vec<u8>)> {
    let info = info.info().ok()?;
    let ct = MlsCrypto::default()
        .hpke_seal(RELEASE_HPKE, hpke_pub, &info, b"", ikm.expose())
        .ok()?;
    let enc = ct.kem_output.as_slice().to_vec();
    let ct = ct.ciphertext.as_slice().to_vec();
    if enc.len() != caps::RELEASE_ENC || ct.len() != caps::RELEASE_CT {
        return None;
    }
    Some((enc, ct))
}

/// An opened release.
#[derive(Debug)]
pub struct Released {
    /// The sender's engine identity.
    pub identity: LkIdentity,
    /// The sender's leaf index in `epoch`.
    pub leaf: u32,
    /// The epoch.
    pub epoch: Epoch,
    /// `slot(epoch)`.
    pub slot: Slot,
    /// The sender's frame IKM.
    pub ikm: FrameIkm,
}

/// The recorder's side: opens relayed releases with the per-recording key.
pub struct ReleaseOpener {
    sk: Zeroizing<[u8; 32]>,
    call_id: RoomName,
    recording_id: u64,
}

impl fmt::Debug for ReleaseOpener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReleaseOpener")
            .field("call_id", &self.call_id)
            .field("recording_id", &self.recording_id)
            .finish_non_exhaustive()
    }
}

impl ReleaseOpener {
    /// An opener for one recording of one call.
    pub fn new(call_id: RoomName, recording_id: u64, hpke_sk: Zeroizing<[u8; 32]>) -> Self {
        ReleaseOpener {
            sk: hpke_sk,
            call_id,
            recording_id,
        }
    }

    /// Opens `r` (the server's relayed `Release`). The `info` is rebuilt from the opener's own
    /// `call_id` and `recording_id` and the release's `group_id`, `epoch`, `leaf`, `slot` and
    /// `sender_identity`; sizes are checked first.
    pub fn open(&self, r: &Release) -> Result<Released, ReleaseError> {
        if r.enc.len() != caps::RELEASE_ENC || r.ct.len() != caps::RELEASE_CT {
            return Err(ReleaseError::Sizes);
        }
        let slot = Slot::new(r.slot).ok_or(ReleaseError::Malformed("slot"))?;
        let info = ReleaseInfo {
            call_id: self.call_id.as_str().as_bytes().to_vec(),
            recording_id: self.recording_id,
            group_id: r.group_id.as_bytes().to_vec(),
            epoch: r.epoch,
            leaf_index: r.leaf,
            slot: r.slot,
            sender_identity: r.sender_identity.as_str().as_bytes().to_vec(),
        };
        let info = info.info().map_err(|_| ReleaseError::Malformed("info"))?;
        let input = HpkeCiphertext {
            kem_output: r.enc.as_bytes().to_vec().into(),
            ciphertext: r.ct.as_bytes().to_vec().into(),
        };
        let pt = MlsCrypto::default()
            .hpke_open(RELEASE_HPKE, &input, self.sk.as_slice(), &info, b"")
            .map_err(|_| ReleaseError::Aead)?;
        let pt = Zeroizing::new(pt);
        let ikm = FrameIkm::from_bytes(
            <[u8; 32]>::try_from(pt.as_slice()).map_err(|_| ReleaseError::Aead)?,
        );
        Ok(Released {
            identity: r.sender_identity.clone(),
            leaf: r.leaf,
            epoch: r.epoch,
            slot,
            ikm,
        })
    }

    /// The call.
    pub fn call_id(&self) -> &RoomName {
        &self.call_id
    }

    /// The recording.
    pub fn recording_id(&self) -> u64 {
        self.recording_id
    }
}

/// The recorder's per-recording X25519 key pair. The public key goes into the
/// `RecorderCert` through the server.
pub struct RecorderKeyPair {
    sk: Zeroizing<[u8; 32]>,
    pk: [u8; 32],
    call_id: RoomName,
    recording_id: u64,
}

impl fmt::Debug for RecorderKeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecorderKeyPair")
            .field("call_id", &self.call_id)
            .field("recording_id", &self.recording_id)
            .finish_non_exhaustive()
    }
}

impl RecorderKeyPair {
    /// A fresh key pair (HPKE `DeriveKeyPair` from 32 random bytes).
    pub fn generate<R: RngCore + CryptoRng>(
        call_id: RoomName,
        recording_id: u64,
        rng: &mut R,
    ) -> RecorderKeyPair {
        loop {
            let mut ikm = Zeroizing::new([0u8; 32]);
            rng.fill_bytes(ikm.as_mut());
            if let Ok(kp) = MlsCrypto::default().derive_hpke_keypair(RELEASE_HPKE, ikm.as_slice())
                && kp.public.len() == 32
                && kp.private.len() == 32
            {
                let mut sk = Zeroizing::new([0u8; 32]);
                sk.copy_from_slice(&kp.private);
                let mut pk = [0u8; 32];
                pk.copy_from_slice(&kp.public);
                return RecorderKeyPair {
                    sk,
                    pk,
                    call_id,
                    recording_id,
                };
            }
        }
    }

    /// The public key (`RecorderCert.hpke_pub`).
    pub fn public_key(&self) -> [u8; 32] {
        self.pk
    }

    /// Test hook: the secret key (tests search logs for it).
    #[cfg(feature = "testing")]
    pub fn secret_for_test(&self) -> [u8; 32] {
        *self.sk
    }

    /// The opener for this recording.
    pub fn into_opener(self) -> ReleaseOpener {
        ReleaseOpener::new(self.call_id, self.recording_id, self.sk)
    }
}
