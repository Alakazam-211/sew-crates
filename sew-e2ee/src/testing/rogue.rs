//! Crafted MLS objects for negative tests: key packages with arbitrary scopes, and a rogue
//! group (a raw openmls client) that can make Welcomes a real member would never make (another
//! external sender, a bad leaf in the tree).

use openmls::prelude::{
    BasicCredential, Credential, CredentialWithKey, Extension, Extensions, ExternalSender, GroupId,
    KeyPackage, KeyPackageIn, Lifetime, MlsGroup, MlsGroupCreateConfig, MlsMessageBodyIn,
    MlsMessageIn, MlsMessageOut, PURE_PLAINTEXT_WIRE_FORMAT_POLICY, ProtocolVersion,
};
use openmls_basic_credential::SignatureKeyPair;
use openmls_traits::OpenMlsProvider;
use openmls_traits::types::SignatureScheme;
use sew_proto::e2ee::tls::{
    DeviceSigInput, IDENTITY_KIND_MEMBER, IDENTITY_VERSION, SewCallIdentity, TlsCodec,
};
use tls_codec::{Deserialize as _, Serialize as _};

use super::fixtures::TestDevice;
use crate::ids::{UnixMs, install_id_bytes};
use crate::keypackage::{CS1, member_capabilities};
use crate::provider::CallProvider;

/// A leaf key and a `SewCallIdentity` credential with arbitrary `scope` bytes, device-signed.
pub fn craft_leaf(device: &TestDevice, scope: &[u8]) -> (SignatureKeyPair, Credential) {
    let signer = match SignatureKeyPair::new(SignatureScheme::ED25519) {
        Ok(s) => s,
        Err(e) => panic!("rogue: leaf key: {e:?}"),
    };
    let ident = device.client.identity();
    let input = DeviceSigInput {
        install_id: install_id_bytes(ident.install_id()),
        member_id: ident.member_id().get(),
        device_id: ident.device_id().get(),
        scope: scope.to_vec(),
        leaf_signature_key: signer.public().to_vec(),
    };
    let msg = input.signed_bytes().unwrap_or_default();
    let sig = device.signer.sign(&msg);
    let identity = SewCallIdentity {
        version: IDENTITY_VERSION,
        kind: IDENTITY_KIND_MEMBER,
        scope: scope.to_vec(),
        device_cert: ident.cert_bytes().to_vec(),
        device_sig: sig.to_vec(),
    };
    let credential: Credential =
        BasicCredential::new(identity.to_bytes().unwrap_or_default()).into();
    (signer, credential)
}

/// A key package for `device` with arbitrary scope bytes (empty, `voice:<id>`, `call:abc`, …).
pub fn craft_key_package(device: &TestDevice, scope: &[u8], now: UnixMs) -> Vec<u8> {
    let (signer, credential) = craft_leaf(device, scope);
    let provider = CallProvider::new();
    let now_s = now / 1000;
    let bundle = KeyPackage::builder()
        .key_package_lifetime(Lifetime::init(now_s - 600, now_s + 6_600))
        .leaf_node_capabilities(member_capabilities())
        .build(
            CS1,
            &provider,
            &signer,
            CredentialWithKey {
                credential,
                signature_key: signer.public().into(),
            },
        );
    match bundle {
        Ok(b) => MlsMessageOut::from(b.key_package().clone())
            .to_bytes()
            .unwrap_or_default(),
        Err(e) => panic!("rogue: key package: {e:?}"),
    }
}

/// A raw openmls group a malicious server could build.
pub struct RogueGroup {
    provider: CallProvider,
    signer: SignatureKeyPair,
    group: MlsGroup,
}

impl std::fmt::Debug for RogueGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RogueGroup")
    }
}

impl RogueGroup {
    /// A one-leaf group `group_id` whose creator is `device` with `scope` bytes and whose
    /// group context names `external_sender` (TLS `ExternalSender` bytes).
    pub fn create(
        device: &TestDevice,
        scope: &[u8],
        external_sender: &[u8],
        group_id: &[u8],
        now: UnixMs,
    ) -> RogueGroup {
        let (signer, credential) = craft_leaf(device, scope);
        let provider = CallProvider::new();
        let sender = match ExternalSender::tls_deserialize_exact(external_sender) {
            Ok(s) => s,
            Err(e) => panic!("rogue: external sender: {e:?}"),
        };
        let ext = match Extensions::single(Extension::ExternalSenders(vec![sender])) {
            Ok(e) => e,
            Err(e) => panic!("rogue: extensions: {e:?}"),
        };
        let now_s = now / 1000;
        let config = MlsGroupCreateConfig::builder()
            .ciphersuite(CS1)
            .wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY)
            .max_past_epochs(0)
            .padding_size(0)
            .use_ratchet_tree_extension(false)
            .capabilities(member_capabilities())
            .lifetime(Lifetime::init(now_s - 600, now_s + 6_600))
            .with_group_context_extensions(ext)
            .build();
        let group = match MlsGroup::new_with_group_id(
            &provider,
            &signer,
            &config,
            GroupId::from_slice(group_id),
            CredentialWithKey {
                credential,
                signature_key: signer.public().into(),
            },
        ) {
            Ok(g) => g,
            Err(e) => panic!("rogue: group: {e:?}"),
        };
        RogueGroup {
            provider,
            signer,
            group,
        }
    }

    /// Adds `kp` (an `MLSMessage(KeyPackage)`) by value and returns `(welcome, ratchet tree,
    /// new epoch, the new leaf)`.
    pub fn add(&mut self, kp: &[u8]) -> (Vec<u8>, Vec<u8>, u64, u32) {
        let msg = match MlsMessageIn::tls_deserialize_exact(kp) {
            Ok(m) => m,
            Err(e) => panic!("rogue: kp decode: {e:?}"),
        };
        let MlsMessageBodyIn::KeyPackage(k) = msg.extract() else {
            panic!("rogue: not a key package");
        };
        let k: KeyPackageIn = k;
        let kp = match k.validate(self.provider.crypto(), ProtocolVersion::Mls10) {
            Ok(k) => k,
            Err(e) => panic!("rogue: kp validate: {e:?}"),
        };
        let sig = kp.leaf_node().signature_key().as_slice().to_vec();
        let bundle = self
            .group
            .commit_builder()
            .propose_adds([kp])
            .force_self_update(true)
            .load_psks(self.provider.storage())
            .ok()
            .and_then(|b| {
                b.create_group_info(true)
                    .use_ratchet_tree_extension(false)
                    .build(
                        self.provider.rand(),
                        self.provider.crypto(),
                        &self.signer,
                        |_| true,
                    )
                    .ok()
            })
            .and_then(|b| b.stage_commit(&self.provider).ok());
        let Some(bundle) = bundle else {
            panic!("rogue: commit");
        };
        let (_commit, welcome, _gi) = bundle.into_messages();
        let welcome = welcome.and_then(|w| w.to_bytes().ok()).unwrap_or_default();
        if let Err(e) = self.group.merge_pending_commit(&self.provider) {
            panic!("rogue: merge: {e:?}");
        }
        let tree = self
            .group
            .export_ratchet_tree()
            .tls_serialize_detached()
            .unwrap_or_default();
        let leaf = self
            .group
            .members()
            .find(|m| m.signature_key == sig)
            .map_or(0, |m| m.index.u32());
        (welcome, tree, self.group.epoch().as_u64(), leaf)
    }

    /// The rogue creator's leaf index.
    pub fn own_leaf(&self) -> u32 {
        self.group.own_leaf_index().u32()
    }
}
