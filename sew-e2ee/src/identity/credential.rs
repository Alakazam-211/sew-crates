//! The call credential: the MLS leaf's Basic credential is `TLS(SewCallIdentity)` version 2.
//!
//! ```text
//! device_sig = ECDSA-P256-SHA256(device key,
//!     "sew call leaf v2" ‖ TLS({ install_id<V>, member_id, device_id, scope<V>, leaf_signature_key<V> }))
//! ```
//!
//! Every member validates every leaf (Welcome trees, Add proposals, commit update paths): the
//! credential is Basic and decodes, version 2 and kind 1; its scope is non-empty and equals the
//! call's; the certificate verifies; the serial is not revoked; `device_sig` verifies over the
//! leaf's actual signature key. These repeat the server's checks on the client.

use std::collections::BTreeSet;

use openmls::prelude::{Credential, CredentialType};
use sew_proto::e2ee::CallScope;
use sew_proto::e2ee::tls::{
    DeviceSigInput, IDENTITY_KIND_MEMBER, IDENTITY_VERSION, SewCallIdentity, TlsCodec,
};

use super::cert::{CertContext, check_device_cert, verify_device_sig};
use crate::config::caps;
use crate::ids::{Id, UnixMs, install_id_bytes};

/// What a valid leaf credential says (member and device come from its certificate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafIdentity {
    /// The member.
    pub member_id: Id,
    /// The device.
    pub device_id: Id,
    /// The certificate serial.
    pub serial: u64,
    /// The platform code (1–5).
    pub platform: u8,
    /// The device label.
    pub label: String,
    /// The certificate's `not_after` (unix seconds).
    pub cert_not_after_s: u64,
}

/// Why a leaf was refused. Carries no credential bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LeafCheckError {
    /// Not Basic, not `SewCallIdentity` v2, or `kind ≠ 1`.
    #[error("credential: {0}")]
    Credential(&'static str),
    /// Empty, malformed or another call's scope (`report` kind `scope_invalid`).
    #[error("scope: {0}")]
    Scope(&'static str),
    /// The certificate failed.
    #[error("device certificate: {0}")]
    Cert(&'static str),
    /// The serial is revoked.
    #[error("revoked")]
    Revoked,
    /// `device_sig` does not verify over the leaf key.
    #[error("device signature")]
    DeviceSig,
}

impl LeafCheckError {
    /// Whether the scope was the problem (the client also reports `scope_invalid`).
    pub fn is_scope(&self) -> bool {
        matches!(self, LeafCheckError::Scope(_))
    }
}

/// The trust inputs of a leaf check.
#[derive(Debug, Clone, Copy)]
pub struct LeafContext<'a> {
    /// This install.
    pub install_id: u64,
    /// The pinned CA key.
    pub ca_public: &'a [u8; 32],
    /// Revoked serials.
    pub crl: &'a BTreeSet<u64>,
    /// The call's scope (`CallKeysInit.scope`).
    pub scope: CallScope,
    /// Now (unix ms).
    pub now: UnixMs,
    /// Certificate skew (seconds).
    pub skew_s: u64,
}

/// Checks one leaf (credential plus its signature key).
pub fn check_leaf(
    credential: &Credential,
    leaf_signature_key: &[u8],
    ctx: &LeafContext<'_>,
) -> Result<LeafIdentity, LeafCheckError> {
    if credential.credential_type() != CredentialType::Basic {
        return Err(LeafCheckError::Credential("not_basic"));
    }
    let raw = credential.serialized_content();
    if raw.len() > caps::CALL_IDENTITY {
        return Err(LeafCheckError::Credential("size"));
    }
    let identity =
        SewCallIdentity::from_bytes(raw).map_err(|_| LeafCheckError::Credential("decode"))?;
    check_identity(&identity, leaf_signature_key, ctx)
}

/// [`check_leaf`] on decoded identity bytes.
pub fn check_identity(
    identity: &SewCallIdentity,
    leaf_signature_key: &[u8],
    ctx: &LeafContext<'_>,
) -> Result<LeafIdentity, LeafCheckError> {
    if identity.version != IDENTITY_VERSION {
        return Err(LeafCheckError::Credential("version"));
    }
    if identity.kind != IDENTITY_KIND_MEMBER {
        return Err(LeafCheckError::Credential("kind"));
    }
    if identity.scope.is_empty() {
        return Err(LeafCheckError::Scope("empty"));
    }
    let scope = CallScope::parse(&identity.scope).ok_or(LeafCheckError::Scope("malformed"))?;
    if scope != ctx.scope {
        return Err(LeafCheckError::Scope("mismatch"));
    }
    if identity.device_cert.len() > caps::DEVICE_CERT {
        return Err(LeafCheckError::Cert("size"));
    }
    let cert = sew_proto::e2ee::tls::SewDeviceCert::from_bytes(&identity.device_cert)
        .map_err(|_| LeafCheckError::Cert("decode"))?;
    let cctx = CertContext {
        install_id: ctx.install_id,
        ca_public: ctx.ca_public,
        skew_s: ctx.skew_s,
        now: ctx.now,
    };
    let verified = check_device_cert(cert, &cctx).map_err(|e| match e {
        crate::error::IdentityError::BadCert(r) => LeafCheckError::Cert(r),
        _ => LeafCheckError::Cert("cert"),
    })?;
    if ctx.crl.contains(&verified.serial()) {
        return Err(LeafCheckError::Revoked);
    }
    let tbs = &verified.cert.tbs;
    let input = DeviceSigInput {
        install_id: tbs.install_id.clone(),
        member_id: tbs.member_id,
        device_id: tbs.device_id,
        scope: identity.scope.clone(),
        leaf_signature_key: leaf_signature_key.to_vec(),
    };
    if !verify_device_sig(&tbs.device_pubkey, &input, &identity.device_sig) {
        return Err(LeafCheckError::DeviceSig);
    }
    Ok(LeafIdentity {
        member_id: verified.member_id,
        device_id: verified.device_id,
        serial: tbs.serial,
        platform: tbs.platform,
        label: String::from_utf8_lossy(&tbs.label).into_owned(),
        cert_not_after_s: tbs.not_after,
    })
}

/// The bytes the device key signs for a leaf (`"sew call leaf v2" ‖ TLS(DeviceSigInput)`).
pub fn device_sig_message(
    install_id: u64,
    member_id: Id,
    device_id: Id,
    scope: CallScope,
    leaf_signature_key: &[u8],
) -> Vec<u8> {
    DeviceSigInput {
        install_id: install_id_bytes(install_id),
        member_id: member_id.get(),
        device_id: device_id.get(),
        scope: scope.to_bytes(),
        leaf_signature_key: leaf_signature_key.to_vec(),
    }
    .signed_bytes()
    .unwrap_or_default()
}

/// `TLS(SewCallIdentity)` v2, kind 1, for a scope, a certificate and a device signature.
pub fn identity_bytes(scope: CallScope, device_cert: &[u8], device_sig: &[u8; 64]) -> Vec<u8> {
    SewCallIdentity {
        version: IDENTITY_VERSION,
        kind: IDENTITY_KIND_MEMBER,
        scope: scope.to_bytes(),
        device_cert: device_cert.to_vec(),
        device_sig: device_sig.to_vec(),
    }
    .to_bytes()
    .unwrap_or_default()
}
