//! Key packages: made offline when the channel view opens
//! or a ring arrives, signed by the device key, uploaded for a single-use
//! `key_package_ref`, then claimed by the voice join or the key join (a move).

use std::fmt;

use openmls::prelude::{
    BasicCredential, Capabilities, Ciphersuite, Credential, CredentialType, CredentialWithKey,
    ExtensionType, KeyPackage, Lifetime, MlsMessageOut, ProtocolVersion,
};
use openmls_traits::OpenMlsProvider;
use openmls_traits::types::SignatureScheme;
use sew_mls_crypto::SignatureKeyPair;
use sew_proto::e2ee::{
    B64, CallKeysInit, CallScope, E2eeJoin, KeyJoinRequest, KeyPackageRefItem,
    KeyPackageUploadRequest,
};

use crate::bridge::Caps;
use crate::config::E2eeConfig;
use crate::error::{IdentityError, JoinError, SessionError};
use crate::identity::cert::verify_p256;
use crate::identity::credential::{device_sig_message, identity_bytes};
use crate::identity::{DeviceIdentity, SignRequest};
use crate::ids::{JoinId, UnixMs};
use crate::provider::CallProvider;
use crate::session::CallSession;
use crate::trust::TrustAnchors;

/// The call ciphersuite.
pub const CS1: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// The capabilities every member leaf advertises: ciphersuite 1 and Basic credentials. Default
/// extension types such as `external_senders` are **not** listed (RFC 9420 §7.2; Sew's
/// choice where the server's checks and the RFC disagree: other implementations refuse a leaf
/// that lists one).
pub fn member_capabilities() -> Capabilities {
    Capabilities::builder()
        .versions(vec![ProtocolVersion::Mls10])
        .ciphersuites(vec![CS1])
        .extensions(Vec::<ExtensionType>::new())
        .credentials(vec![CredentialType::Basic])
        .build()
}

/// A leaf's MLS signing key and credential (in memory only, zeroized on drop by
/// `sew_mls_crypto::SignatureKeyPair`).
pub(crate) struct LeafSecret {
    pub(crate) signer: SignatureKeyPair,
    pub(crate) credential: Credential,
    pub(crate) lifetime: (u64, u64),
    pub(crate) kp_ref: [u8; 32],
}

impl fmt::Debug for LeafSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LeafSecret(***)")
    }
}

/// A fresh leaf key waiting for the device signature over its credential binding.
pub(crate) struct LeafDraft {
    signer: SignatureKeyPair,
    scope: CallScope,
    message: Vec<u8>,
}

impl fmt::Debug for LeafDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LeafDraft(***)")
    }
}

impl LeafDraft {
    /// A fresh Ed25519 leaf key and the bytes the device must sign.
    pub(crate) fn new(
        device: &DeviceIdentity,
        scope: CallScope,
    ) -> Result<LeafDraft, IdentityError> {
        let signer = SignatureKeyPair::new(SignatureScheme::ED25519)
            .map_err(|_| IdentityError::Mls("leaf key"))?;
        let message = device_sig_message(
            device.install_id(),
            device.member_id(),
            device.device_id(),
            scope,
            signer.public(),
        );
        Ok(LeafDraft {
            signer,
            scope,
            message,
        })
    }

    pub(crate) fn message(&self) -> &[u8] {
        &self.message
    }

    /// Verifies the device signature and builds the KeyPackage in `provider` (its private keys
    /// stay there for the Welcome). Returns the leaf secret and the `MLSMessage(KeyPackage)`
    /// bytes.
    pub(crate) fn finish(
        self,
        device: &DeviceIdentity,
        provider: &CallProvider,
        device_sig: &[u8; 64],
        now: UnixMs,
        cfg: &E2eeConfig,
    ) -> Result<(LeafSecret, Vec<u8>), IdentityError> {
        if !verify_p256(device.pubkey(), &self.message, device_sig) {
            return Err(IdentityError::BadSignature);
        }
        let identity = identity_bytes(self.scope, device.cert_bytes(), device_sig);
        let credential: Credential = BasicCredential::new(identity).into();
        let now_s = now / 1000;
        let lifetime = (
            now_s.saturating_sub(cfg.key_package_lifetime_back_s),
            now_s.saturating_add(cfg.key_package_lifetime_fwd_s),
        );
        let bundle = KeyPackage::builder()
            .key_package_lifetime(Lifetime::init(lifetime.0, lifetime.1))
            .leaf_node_capabilities(member_capabilities())
            .build(
                CS1,
                provider,
                &self.signer,
                CredentialWithKey {
                    credential: credential.clone(),
                    signature_key: self.signer.public().into(),
                },
            )
            .map_err(|_| IdentityError::Mls("key package"))?;
        let kp = bundle.key_package().clone();
        let r = kp
            .hash_ref(provider.crypto())
            .map_err(|_| IdentityError::Mls("hash_ref"))?;
        let kp_ref: [u8; 32] = r
            .as_slice()
            .try_into()
            .map_err(|_| IdentityError::Mls("hash_ref length"))?;
        let bytes = MlsMessageOut::from(kp)
            .to_bytes()
            .map_err(|_| IdentityError::Mls("encode key package"))?;
        Ok((
            LeafSecret {
                signer: self.signer,
                credential,
                lifetime,
                kp_ref,
            },
            bytes,
        ))
    }
}

/// The `E2eeClient`: one device's identity and trust, for preparing joins.
#[derive(Debug)]
pub struct E2eeClient {
    pub(crate) identity: DeviceIdentity,
    pub(crate) trust: TrustAnchors,
    pub(crate) cfg: E2eeConfig,
}

impl E2eeClient {
    /// A client for an enrolled device.
    pub fn new(identity: DeviceIdentity, trust: TrustAnchors) -> E2eeClient {
        E2eeClient {
            identity,
            trust,
            cfg: E2eeConfig::default(),
        }
    }

    /// With local configuration (tests).
    pub fn with_config(mut self, cfg: E2eeConfig) -> E2eeClient {
        self.cfg = cfg;
        self
    }

    /// Replaces the local configuration.
    pub fn set_config(&mut self, cfg: E2eeConfig) {
        self.cfg = cfg;
    }

    /// Offline (~1 ms native): a fresh leaf key and the device-signature request. Call when the
    /// channel view opens or a ring arrives. `Scope` has no empty form.
    pub fn prepare_key_package(
        &mut self,
        scope: CallScope,
        now: UnixMs,
    ) -> Result<(PendingKp, SignRequest), IdentityError> {
        let draft = LeafDraft::new(&self.identity, scope)?;
        let req = SignRequest {
            message: draft.message().to_vec(),
        };
        Ok((
            PendingKp {
                draft,
                identity: self.identity.clone(),
                cfg: self.cfg.clone(),
                created: now,
            },
            req,
        ))
    }

    /// The trust anchors (a `revoked` dispatch extends the CRL; a fresh READY refreshes them).
    pub fn trust_mut(&mut self) -> &mut TrustAnchors {
        &mut self.trust
    }

    /// The trust anchors.
    pub fn trust(&self) -> &TrustAnchors {
        &self.trust
    }

    /// The device identity.
    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    /// The local configuration.
    pub fn config(&self) -> &E2eeConfig {
        &self.cfg
    }
}

/// A key package waiting for its device signature. Not `Debug`-printable beyond its type.
pub struct PendingKp {
    draft: LeafDraft,
    identity: DeviceIdentity,
    cfg: E2eeConfig,
    created: UnixMs,
}

impl fmt::Debug for PendingKp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PendingKp(***)")
    }
}

impl PendingKp {
    /// Verifies the signature and builds the KeyPackage.
    pub fn finish(self, device_sig: [u8; 64]) -> Result<PreparedKp, IdentityError> {
        let provider = CallProvider::new();
        let scope = self.draft.scope;
        let (leaf, bytes) = self.draft.finish(
            &self.identity,
            &provider,
            &device_sig,
            self.created,
            &self.cfg,
        )?;
        Ok(PreparedKp {
            provider,
            leaf,
            bytes,
            scope,
            created: self.created,
            uploaded: None,
            cfg: self.cfg,
        })
    }
}

/// A signed key package (single use; moved into the session).
pub struct PreparedKp {
    pub(crate) provider: CallProvider,
    pub(crate) leaf: LeafSecret,
    pub(crate) bytes: Vec<u8>,
    pub(crate) scope: CallScope,
    created: UnixMs,
    uploaded: Option<(Vec<u8>, u64)>,
    cfg: E2eeConfig,
}

impl fmt::Debug for PreparedKp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedKp")
            .field("scope", &self.scope)
            .field("uploaded", &self.uploaded.is_some())
            .finish_non_exhaustive()
    }
}

impl PreparedKp {
    /// The KeyPackage upload body (one package per upload here).
    pub fn upload_body(&self) -> KeyPackageUploadRequest {
        KeyPackageUploadRequest {
            key_packages: vec![B64::new(self.bytes.clone())],
        }
    }

    /// The package's own `hash_ref` (RFC 9420 §5.2).
    pub fn key_package_ref(&self) -> [u8; 32] {
        self.leaf.kp_ref
    }

    /// The `MLSMessage(KeyPackage)` bytes.
    pub fn key_package_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The scope.
    pub fn scope(&self) -> CallScope {
        self.scope
    }

    /// Takes the upload's answer for this package; refuses a ref other than the package's
    /// `hash_ref`.
    pub fn set_ref(&mut self, r: &KeyPackageRefItem) -> Result<(), IdentityError> {
        if r.key_package_ref.as_bytes() != self.leaf.kp_ref.as_slice() {
            return Err(IdentityError::RefMismatch);
        }
        self.uploaded = Some((r.key_package_ref.as_bytes().to_vec(), r.expires_at_ms));
        Ok(())
    }

    /// Over 60 min old, or past the server TTL: prepare again.
    pub fn is_stale(&self, now: UnixMs) -> bool {
        let age_ms = now.saturating_sub(self.created);
        if age_ms > self.cfg.prepared_kp_max_age_s.saturating_mul(1000) {
            return true;
        }
        self.uploaded.as_ref().is_some_and(|(_, exp)| now >= *exp)
    }

    fn uploaded_ref(&self) -> Result<B64, IdentityError> {
        self.uploaded
            .as_ref()
            .map(|(r, _)| B64::new(r.clone()))
            .ok_or(IdentityError::NoRef)
    }

    /// The `e2ee` object of the voice join: `{key_package_ref, gateway_session, caps}`.
    pub fn join_field(
        &self,
        caps: &Caps,
        gateway_session: &str,
    ) -> Result<E2eeJoin, IdentityError> {
        Ok(E2eeJoin {
            key_package_ref: self.uploaded_ref()?,
            gateway_session: gateway_session.to_owned(),
            caps: caps.to_wire(),
        })
    }

    /// The key join body for a move, with the server-minted `join_id` from `voice_server_updated`.
    pub fn key_join_request(
        &self,
        join_id: JoinId,
        caps: &Caps,
        gateway_session: &str,
    ) -> Result<KeyJoinRequest, IdentityError> {
        Ok(KeyJoinRequest {
            join_id,
            gateway_session: gateway_session.to_owned(),
            key_package_ref: self.uploaded_ref()?,
            caps: caps.to_wire(),
        })
    }

    /// Starts the session from the server's answer (`CallKeysInit`: `join_id`, `lk_identity`,
    /// `scope`, group, params). Checks `lk_identity == "p_" + base32(join_id)`.
    pub fn into_session(
        self,
        client: &E2eeClient,
        init: CallKeysInit,
        gateway_session: &str,
        now: UnixMs,
    ) -> Result<CallSession, SessionError> {
        CallSession::start(client, self, init, gateway_session, now)
    }
}

/// What to do when the voice join or the key join refused the join: `Ok(())` means prepare and
/// upload a fresh package and retry **once** (`422 e2ee_key_package_ref_unknown`, `409
/// e2ee_key_package_ref_used`); a second ref failure is `JoinError::KeyPackage`; `422
/// e2ee_unsupported_client` is `JoinError::Unsupported{unsupported}`.
pub fn on_join_failure(
    f: &crate::wire::ApiFailure,
    already_retried: bool,
) -> Result<(), JoinError> {
    use sew_proto::e2ee::reasons;
    let ref_failure = (f.status == 422 && f.is(reasons::E2EE_KEY_PACKAGE_REF_UNKNOWN))
        || (f.status == 409 && f.is(reasons::E2EE_KEY_PACKAGE_REF_USED));
    if ref_failure {
        return if already_retried {
            Err(JoinError::KeyPackage)
        } else {
            Ok(())
        };
    }
    if f.status == 422 && f.is(reasons::E2EE_UNSUPPORTED_CLIENT) {
        return Err(JoinError::Unsupported {
            unsupported: f.detail.clone().unwrap_or_default(),
        });
    }
    Err(JoinError::Session(SessionError::Refused("join refused")))
}
