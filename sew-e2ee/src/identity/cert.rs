//! Device and recorder certificates and the signature primitives.
//! The formats are `sew_proto::e2ee::tls`; this module adds the checks.

use ed25519_dalek::{Signature as EdSignature, VerifyingKey};
use p256::ecdsa::signature::Verifier as _;
use sew_proto::e2ee::tls::{
    DeviceSigInput, LeafBindingTableTbs, RecorderCert, SewDeviceCert, TlsCodec,
};

use crate::config::caps;
use crate::error::{IdentityError, check_cap};
use crate::ids::{Id, UnixMs, install_id_bytes};

/// Verifies an Ed25519 signature (strict: no small-order keys, canonical `S`).
pub fn verify_ed25519(public_key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let Ok(pk) = <[u8; 32]>::try_from(public_key) else {
        return false;
    };
    let Ok(sig) = <[u8; 64]>::try_from(signature) else {
        return false;
    };
    let Ok(vk) = VerifyingKey::from_bytes(&pk) else {
        return false;
    };
    vk.verify_strict(message, &EdSignature::from_bytes(&sig))
        .is_ok()
}

/// Verifies an ECDSA-P256-SHA256 signature (raw `r‖s`) under a 65-byte SEC1 key.
pub fn verify_p256(public_key_sec1: &[u8], message: &[u8], signature: &[u8]) -> bool {
    if public_key_sec1.len() != 65 || public_key_sec1[0] != 0x04 || signature.len() != 64 {
        return false;
    }
    let Ok(vk) = p256::ecdsa::VerifyingKey::from_sec1_bytes(public_key_sec1) else {
        return false;
    };
    let Ok(sig) = p256::ecdsa::Signature::from_slice(signature) else {
        return false;
    };
    vk.verify(message, &sig).is_ok()
}

/// Whether `pubkey` is a valid uncompressed SEC1 P-256 point.
pub fn valid_p256_point(pubkey: &[u8]) -> bool {
    pubkey.len() == 65
        && pubkey[0] == 0x04
        && p256::ecdsa::VerifyingKey::from_sec1_bytes(pubkey).is_ok()
}

/// `device_sig` over `"sew call leaf v2" ‖ TLS(input)`.
pub fn verify_device_sig(device_pubkey: &[u8], input: &DeviceSigInput, signature: &[u8]) -> bool {
    match input.signed_bytes() {
        Ok(msg) => verify_p256(device_pubkey, &msg, signature),
        Err(_) => false,
    }
}

/// The CA signature of a device certificate.
pub fn verify_device_cert_sig(ca_public: &[u8; 32], cert: &SewDeviceCert) -> bool {
    match cert.signed_bytes() {
        Ok(msg) => verify_ed25519(ca_public, &msg, &cert.ca_sig),
        Err(_) => false,
    }
}

/// The CA signature of a recorder certificate.
pub fn verify_recorder_cert_sig(ca_public: &[u8; 32], cert: &RecorderCert) -> bool {
    match cert.signed_bytes() {
        Ok(msg) => verify_ed25519(ca_public, &msg, &cert.ca_sig),
        Err(_) => false,
    }
}

/// The DS signature of a binding table.
pub fn verify_bindings_sig(ds_public: &[u8; 32], tbs: &LeafBindingTableTbs, sig: &[u8]) -> bool {
    match tbs.signed_bytes() {
        Ok(msg) => verify_ed25519(ds_public, &msg, sig),
        Err(_) => false,
    }
}

/// A device certificate that passed [`verify_device_cert`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCert {
    /// The decoded certificate.
    pub cert: SewDeviceCert,
    /// The member.
    pub member_id: Id,
    /// The device.
    pub device_id: Id,
}

impl VerifiedCert {
    /// The certificate's serial.
    pub fn serial(&self) -> u64 {
        self.cert.tbs.serial
    }

    /// `not_after` in unix seconds.
    pub fn not_after_s(&self) -> u64 {
        self.cert.tbs.not_after
    }

    /// The device public key (65 bytes).
    pub fn device_pubkey(&self) -> &[u8] {
        &self.cert.tbs.device_pubkey
    }

    /// The TLS bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.cert.to_bytes().unwrap_or_default()
    }
}

/// What a certificate check needs.
#[derive(Debug, Clone, Copy)]
pub struct CertContext<'a> {
    /// This install.
    pub install_id: u64,
    /// The pinned CA key.
    pub ca_public: &'a [u8; 32],
    /// Clock skew allowed on both ends (seconds).
    pub skew_s: u64,
    /// Now (unix ms).
    pub now: UnixMs,
}

/// `DeviceCert::verify`: version 1; the CA signature under the pinned key; the install;
/// `not_before − skew ≤ now ≤ not_after + skew`; valid ids; a valid SEC1 point. Revocation is
/// the caller's (it holds the CRL).
pub fn verify_device_cert(
    bytes: &[u8],
    ctx: &CertContext<'_>,
) -> Result<VerifiedCert, IdentityError> {
    check_cap("device_cert", bytes.len(), caps::DEVICE_CERT)
        .map_err(|_| IdentityError::BadCert("size"))?;
    let cert = SewDeviceCert::from_bytes(bytes).map_err(|_| IdentityError::BadCert("decode"))?;
    check_device_cert(cert, ctx)
}

/// [`verify_device_cert`] on a decoded certificate.
pub fn check_device_cert(
    cert: SewDeviceCert,
    ctx: &CertContext<'_>,
) -> Result<VerifiedCert, IdentityError> {
    if cert.tbs.version != 1 {
        return Err(IdentityError::BadCert("version"));
    }
    if !verify_device_cert_sig(ctx.ca_public, &cert) {
        return Err(IdentityError::BadCert("ca_sig"));
    }
    if cert.tbs.install_id != install_id_bytes(ctx.install_id) {
        return Err(IdentityError::BadCert("install"));
    }
    let now_s = ctx.now / 1000;
    if now_s.saturating_add(ctx.skew_s) < cert.tbs.not_before
        || now_s > cert.tbs.not_after.saturating_add(ctx.skew_s)
    {
        return Err(IdentityError::BadCert("validity"));
    }
    let member_id = Id::new(cert.tbs.member_id).ok_or(IdentityError::BadCert("member_id"))?;
    let device_id = Id::new(cert.tbs.device_id).ok_or(IdentityError::BadCert("device_id"))?;
    if !valid_p256_point(&cert.tbs.device_pubkey) {
        return Err(IdentityError::BadCert("device_pubkey"));
    }
    Ok(VerifiedCert {
        cert,
        member_id,
        device_id,
    })
}
