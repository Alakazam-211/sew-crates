//! The MLS half of the session: create, Welcome, proposals, commits, activation and the `state`
//! report.

use std::collections::{BTreeMap, BTreeSet};

use openmls::prelude::{
    CredentialWithKey, Extension, Extensions, ExternalSender, GroupId, LeafNodeIndex, Lifetime,
    MlsGroup, MlsGroupCreateConfig, MlsGroupJoinConfig, MlsMessageBodyIn, MlsMessageIn,
    PURE_PLAINTEXT_WIRE_FORMAT_POLICY, ProcessedMessageContent, Proposal, ProposalOrRefType,
    RatchetTreeIn, Sender, SenderExtensionIndex, StagedCommit, StagedWelcome,
};
use openmls_traits::OpenMlsProvider;
use sew_proto::e2ee::tls::{BindingKey, LeafBindingEntry, LeafBindingTableTbs, TlsCodec};
use sew_proto::e2ee::{
    B64, CallKeysInit, InvalidReason, KeyCommitRequest, KeyStateRequest, KeyStateStatus,
    LeafBindingTable, ReportKind, reasons,
};
use sha2::{Digest, Sha256};
use tls_codec::{Deserialize as _, Serialize as _};

use super::{
    ActiveEpoch, CallSession, E2eeStatus, PendingEpoch, Purpose, SessionState, Transition,
};
use crate::bridge::KeyEvent;
use crate::code::SecurityCode;
use crate::config::{LABEL_FRAME_KEY, caps};
use crate::error::{BindingError, SessionError, check_cap};
use crate::identity::{LeafContext, LeafIdentity, VerifiedEntry, check_leaf, verify_table};
use crate::ids::{Epoch, LkIdentity, Millis, Slot};
use crate::keypackage::{CS1, member_capabilities};
use crate::keys::{FrameIkm, ikm_context};
use crate::wire::{ApiFailure, KeyReply, KeyRequest};

pub(super) fn join_config() -> MlsGroupJoinConfig {
    MlsGroupJoinConfig::builder()
        .wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY)
        .max_past_epochs(0)
        .padding_size(0)
        .use_ratchet_tree_extension(false)
        .build()
}

/// Why a Welcome, proposal or commit was refused, and what to send.
struct Refusal {
    reason: InvalidReason,
    report: Option<(ReportKind, Option<u32>)>,
    what: &'static str,
}

impl Refusal {
    fn new(reason: InvalidReason, what: &'static str) -> Refusal {
        Refusal {
            reason,
            report: None,
            what,
        }
    }

    fn with(mut self, kind: ReportKind, leaf: Option<u32>) -> Refusal {
        self.report = Some((kind, leaf));
        self
    }
}

fn binding_refusal(reason: InvalidReason, e: &BindingError) -> Refusal {
    let _ = e;
    Refusal::new(reason, "leaf_binding").with(ReportKind::LeafBindingInvalid, None)
}

impl CallSession {
    pub(super) fn leaf_ctx(&self, now: Millis) -> LeafContext<'_> {
        LeafContext {
            install_id: self.trust.install_id(),
            ca_public: self.trust.ca_public(),
            crl: self.trust.crl(),
            scope: self.scope,
            now,
            skew_s: self.cfg.clock_skew_s,
        }
    }

    fn refuse(&mut self, now: Millis, r: Refusal) -> SessionError {
        if let Some((kind, leaf)) = r.report {
            self.report(now, kind, leaf, None);
        }
        self.start_readd(now, r.reason);
        SessionError::Refused(r.what)
    }

    fn group_matches(&self, gid: &[u8]) -> bool {
        self.group
            .as_ref()
            .is_some_and(|g| g.group_id().as_slice() == gid)
    }

    // ------------------------------------------------------------------ create

    pub(super) fn create_group(
        &mut self,
        now: Millis,
        init: &CallKeysInit,
    ) -> Result<(), SessionError> {
        let Some(kp_ref) = self.latest_leaf else {
            return Err(SessionError::Refused("no key package"));
        };
        let Some(leaf) = self.leaves.remove(&kp_ref) else {
            return Err(SessionError::Refused("no key package"));
        };
        let sender = ExternalSender::tls_deserialize_exact(self.trust.external_sender())
            .map_err(|_| SessionError::ExternalSenderMismatch)?;
        let ext = Extensions::single(Extension::ExternalSenders(vec![sender]))
            .map_err(|_| SessionError::Mls("extensions"))?;
        let config = MlsGroupCreateConfig::builder()
            .ciphersuite(CS1)
            .wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY)
            .max_past_epochs(0)
            .padding_size(0)
            .use_ratchet_tree_extension(false)
            .capabilities(member_capabilities())
            .lifetime(Lifetime::init(leaf.lifetime.0, leaf.lifetime.1))
            .with_group_context_extensions(ext)
            .build();
        let gid = init.group_id.as_bytes().to_vec();
        let group = MlsGroup::new_with_group_id(
            &self.provider,
            &leaf.signer,
            &config,
            GroupId::from_slice(&gid),
            CredentialWithKey {
                credential: leaf.credential.clone(),
                signature_key: leaf.signer.public().into(),
            },
        )
        .map_err(|_| SessionError::Mls("create group"))?;
        let own = group.own_leaf_index().u32();
        self.group = Some(group);
        self.signer = Some(leaf.signer);
        self.slot_base = init.slot_base;
        self.known_group = Some((gid.clone(), 0));
        self.bindings.clear();
        self.bindings.insert(
            own,
            VerifiedEntry {
                join_id: self.join_id.clone(),
                lk_identity: self.me.clone(),
            },
        );
        self.add_bindings.clear();
        self.last_proposals = None;
        self.own_commit = None;
        self.refresh_leaf_ids(now);
        // The DS builds its PublicGroup from this report.
        self.send_state(Purpose::StateCreate, true);
        if init.activate_on_create {
            let ikm = self.derive(own).ok_or(SessionError::Mls("exporter"))?;
            let code = self.current_code();
            let slot = Slot::of(self.slot_base, 0);
            self.emit_key(KeyEvent::Activate {
                identity: self.me.clone(),
                slot,
                ikm: ikm.duplicate(),
            });
            self.outputs
                .push_back(super::Output::CheckSenderSlot { expected: slot });
            self.sender_check = Some(super::SenderCheck {
                expected: slot,
                attempts: 0,
            });
            self.active = Some(ActiveEpoch {
                group_id: gid,
                epoch: 0,
                slot,
                leaf: own,
                ikm,
                code,
                since: now,
            });
            self.counters.epochs_activated += 1;
            self.state = SessionState::Active {
                epoch: 0,
                transition: Transition::Idle,
            };
            self.set_status(E2eeStatus::Active {
                epoch: 0,
                since: now,
            });
            self.missing_check_at = Some(now + self.cfg.missing_key_check_ms);
            self.recovery = None;
        } else {
            // A reset creator: nothing is emitted for epoch 0; the old group's keys keep
            // running and the new group's first active epoch is 1.
            self.state = SessionState::Active {
                epoch: 0,
                transition: Transition::Idle,
            };
            self.recovery = None;
        }
        self.rebuild_roster();
        Ok(())
    }

    pub(super) fn on_init_dispatch(
        &mut self,
        now: Millis,
        init: &CallKeysInit,
    ) -> Result<(), SessionError> {
        if !init.create {
            return Ok(());
        }
        let gid = init.group_id.as_bytes();
        match &self.reset {
            Some(r) if r.new_group_id == gid => {
                let mut i = init.clone();
                i.slot_base = r.slot_base;
                self.create_group(now, &i)
            }
            Some(_) => {
                self.counters.ignored_stale += 1;
                Ok(())
            }
            None if self.group.is_none() => {
                // Creator failover while joining.
                self.init_group = gid.to_vec();
                self.create_group(now, init)
            }
            None => {
                self.counters.ignored_stale += 1;
                Ok(())
            }
        }
    }

    // ------------------------------------------------------------------ derivation

    /// `ikm(L, e)` for leaf `L` of the current epoch of the held group.
    pub(super) fn derive(&self, leaf: u32) -> Option<FrameIkm> {
        let group = self.group.as_ref()?;
        let ctx = ikm_context(group.group_id().as_slice(), group.epoch().as_u64(), leaf);
        let v = group
            .export_secret(self.provider.crypto(), LABEL_FRAME_KEY, &ctx, 32)
            .ok()?;
        FrameIkm::from_exporter(v)
    }

    fn current_code(&self) -> Option<SecurityCode> {
        let group = self.group.as_ref()?;
        SecurityCode::from_authenticator(group.epoch_authenticator().as_slice())
    }

    /// Installs every remote leaf's key of the held group's current epoch, then a fence. The local
    /// key is kept for `activate` (I1).
    fn install_epoch(&mut self, now: Millis, departed: BTreeSet<LkIdentity>) {
        let Some(group) = self.group.as_ref() else {
            return;
        };
        let gid = group.group_id().as_slice().to_vec();
        let epoch = group.epoch().as_u64();
        let own = group.own_leaf_index().u32();
        let leaves: Vec<u32> = group.members().map(|m| m.index.u32()).collect();
        let slot = Slot::of(self.slot_base, epoch);
        if self.slot_retired_at.get(&slot).is_some_and(|t| *t > now)
            || self.active.as_ref().is_some_and(|a| a.slot == slot)
        {
            self.counters.slot_reused_early += 1;
        }
        let mut own_ikm = None;
        let mut installs = Vec::new();
        for leaf in leaves {
            let Some(ikm) = self.derive(leaf) else {
                continue;
            };
            if leaf == own {
                own_ikm = Some(ikm);
                continue;
            }
            let Some(entry) = self.bindings.get(&leaf) else {
                continue;
            };
            if entry.lk_identity == self.me {
                continue;
            }
            installs.push((entry.lk_identity.clone(), ikm));
        }
        for (identity, ikm) in installs {
            self.emit_key(KeyEvent::Install {
                identity,
                slot,
                ikm,
            });
        }
        let fence = self.next_fence();
        self.emit_key(KeyEvent::Fence { id: fence });
        let Some(ikm) = own_ikm else {
            return;
        };
        let code = self.current_code();
        self.pending = Some(PendingEpoch {
            group_id: gid,
            epoch,
            slot,
            leaf: own,
            ikm,
            code,
            fence,
            departed,
        });
        self.state = SessionState::Active {
            epoch,
            transition: Transition::Installed { epoch, fence },
        };
    }

    // ------------------------------------------------------------------ Welcome

    #[allow(clippy::too_many_arguments)]
    pub(super) fn on_welcome(
        &mut self,
        now: Millis,
        group_id: &[u8],
        epoch: Epoch,
        welcome: &[u8],
        tree: &[u8],
        slot_base: u8,
        slot: u8,
        bindings: &LeafBindingTable,
    ) -> Result<(), SessionError> {
        let expected = match &self.reset {
            Some(r) => r.new_group_id.clone(),
            None => self.init_group.clone(),
        };
        if group_id != expected.as_slice() {
            self.counters.ignored_stale += 1;
            return Ok(());
        }
        self.known_group = Some((group_id.to_vec(), epoch));
        check_cap("welcome", welcome.len(), caps::WELCOME)?;
        check_cap("ratchet_tree", tree.len(), caps::RATCHET_TREE)?;
        let msg = match MlsMessageIn::tls_deserialize_exact(welcome) {
            Ok(m) => m,
            Err(_) => {
                return Err(self.refuse(now, Refusal::new(InvalidReason::BadWelcome, "decode")));
            }
        };
        let MlsMessageBodyIn::Welcome(w) = msg.extract() else {
            return Err(self.refuse(now, Refusal::new(InvalidReason::BadWelcome, "not_welcome")));
        };
        if w.ciphersuite() != CS1 {
            let _ = self.refuse(now, Refusal::new(InvalidReason::BadWelcome, "ciphersuite"));
            return Err(SessionError::UnsupportedCiphersuite(u16::from(
                w.ciphersuite(),
            )));
        }
        let mine = w.secrets().iter().find_map(|s| {
            let r = s.new_member();
            let r: [u8; 32] = r.as_slice().try_into().ok()?;
            self.leaves.contains_key(&r).then_some(r)
        });
        let Some(kp_ref) = mine else {
            // A Welcome for another device of this member (defensive; Target::Sessions).
            self.counters.ignored_stale += 1;
            return Ok(());
        };
        let Ok(tree) = RatchetTreeIn::tls_deserialize_exact(tree) else {
            return Err(self.refuse(now, Refusal::new(InvalidReason::BadWelcome, "tree")));
        };
        let staged =
            StagedWelcome::build_from_welcome(&self.provider, &join_config(), w).and_then(|b| {
                b.with_ratchet_tree(tree)
                    .skip_lifetime_validation()
                    .replace_old_group()
                    .build()
            });
        let staged = match staged {
            Ok(s) => s,
            Err(_) => {
                return Err(self.refuse(now, Refusal::new(InvalidReason::BadWelcome, "stage")));
            }
        };
        let checked = self.check_welcome(now, &staged, group_id, epoch, slot_base, slot, bindings);
        let (leaf_ids, table) = match checked {
            Ok(v) => v,
            Err(r) => return Err(self.refuse(now, r)),
        };
        let group = match staged.into_group(&self.provider) {
            Ok(g) => g,
            Err(_) => {
                return Err(self.refuse(now, Refusal::new(InvalidReason::BadWelcome, "into_group")));
            }
        };
        let Some(leaf) = self.leaves.remove(&kp_ref) else {
            return Err(SessionError::Refused("key package"));
        };
        let previous: BTreeSet<LkIdentity> = self.ring.identities().cloned().collect();
        self.group = Some(group);
        self.signer = Some(leaf.signer);
        self.slot_base = slot_base;
        self.bindings = table;
        self.leaf_ids = leaf_ids;
        self.add_bindings.clear();
        self.last_proposals = None;
        self.own_commit = None;
        self.pending = None;
        self.recovery = None;
        let bound: BTreeSet<LkIdentity> = self
            .bindings
            .values()
            .map(|e| e.lk_identity.clone())
            .collect();
        let departed = previous.difference(&bound).cloned().collect();
        self.install_epoch(now, departed);
        if self.active.is_none() {
            self.set_status(E2eeStatus::Securing {
                since: now,
                show_after_ms: self.params.securing_indicator_after_ms,
            });
        }
        // Any Welcome releases every held epoch.
        self.release_held(now);
        self.rebuild_roster();
        Ok(())
    }

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn check_welcome(
        &self,
        now: Millis,
        staged: &StagedWelcome,
        group_id: &[u8],
        epoch: Epoch,
        slot_base: u8,
        slot: u8,
        bindings: &LeafBindingTable,
    ) -> Result<(BTreeMap<u32, LeafIdentity>, BTreeMap<u32, VerifiedEntry>), Refusal> {
        let bad = |w: &'static str| Refusal::new(InvalidReason::BadWelcome, w);
        let ctx = staged.group_context();
        if ctx.group_id().as_slice() != group_id {
            return Err(bad("group_id"));
        }
        if ctx.epoch().as_u64() != epoch {
            return Err(bad("epoch"));
        }
        if ctx.ciphersuite() != CS1 {
            return Err(bad("ciphersuite"));
        }
        let ext = ctx.extensions();
        let senders_ok = ext.iter().count() == 1
            && ext.external_senders().is_some_and(|s| {
                s.len() == 1
                    && s[0]
                        .tls_serialize_detached()
                        .is_ok_and(|b| b == self.trust.external_sender())
            });
        if !senders_ok {
            return Err(bad("external_senders").with(ReportKind::ExternalSenderMismatch, None));
        }
        if Slot::of(slot_base, epoch).get() != slot {
            return Err(Refusal::new(InvalidReason::StateLost, "slot"));
        }
        let lctx = self.leaf_ctx(now);
        let mut ids = BTreeMap::new();
        let mut members = BTreeSet::new();
        for m in staged.members() {
            let leaf = m.index.u32();
            let id = match check_leaf(&m.credential, &m.signature_key, &lctx) {
                Ok(id) => id,
                Err(e) if e.is_scope() => {
                    return Err(bad("scope").with(ReportKind::ScopeInvalid, Some(leaf)));
                }
                Err(_) => return Err(bad("leaf")),
            };
            if !members.insert(id.member_id) {
                return Err(bad("duplicate_member").with(ReportKind::DuplicateMember, Some(leaf)));
            }
            ids.insert(leaf, id);
        }
        let own = staged.own_leaf_index().u32();
        let table = self
            .check_leaf_table(
                bindings,
                group_id,
                epoch,
                ids.keys().copied().collect(),
                own,
            )
            .map_err(|e| binding_refusal(InvalidReason::BadWelcome, &e))?;
        Ok((ids, table))
    }

    /// The binding-table checks on a full key-type-1 table for the held (or staged) tree.
    fn check_leaf_table(
        &self,
        t: &LeafBindingTable,
        group_id: &[u8],
        epoch: Epoch,
        leaves: BTreeSet<u32>,
        own: u32,
    ) -> Result<BTreeMap<u32, VerifiedEntry>, BindingError> {
        let v = verify_table(t, self.trust.ds_public())?;
        if v.call_id != self.call_id {
            return Err(BindingError::Scope("call_id"));
        }
        if v.group_id != group_id {
            return Err(BindingError::Scope("group_id"));
        }
        if v.epoch != epoch {
            return Err(BindingError::Scope("epoch"));
        }
        if !v.is_leaf_table() {
            return Err(BindingError::Scope("key_type"));
        }
        let keys: BTreeSet<u32> = v.by_leaf.keys().copied().collect();
        if keys != leaves {
            return Err(BindingError::Coverage);
        }
        match v.by_leaf.get(&own) {
            Some(e) if e.join_id == self.join_id && e.lk_identity == self.me => {}
            _ => return Err(BindingError::Local),
        }
        Ok(v.by_leaf)
    }

    // ------------------------------------------------------------------ proposals

    pub(super) fn on_proposals(
        &mut self,
        now: Millis,
        group_id: &[u8],
        epoch: Epoch,
        proposals: &[B64],
        committer: u32,
        bindings: &LeafBindingTable,
    ) -> Result<(), SessionError> {
        if !self.group_matches(group_id) || matches!(self.state, SessionState::Resyncing { .. }) {
            self.counters.ignored_stale += 1;
            return Ok(());
        }
        let mls_epoch = self.mls_epoch().unwrap_or(0);
        if epoch < mls_epoch {
            self.counters.ignored_stale += 1;
            return Ok(());
        }
        if epoch > mls_epoch {
            self.start_readd(now, InvalidReason::MissedEpoch);
            return Err(SessionError::Refused("missed_epoch"));
        }
        if proposals.len() > caps::PROPOSALS_PER_MESSAGE {
            return Err(self.refuse(now, Refusal::new(InvalidReason::BadProposal, "too_many")));
        }
        let mut h = Sha256::new();
        for p in proposals {
            h.update((p.len() as u64).to_be_bytes());
            h.update(p.as_bytes());
        }
        h.update(committer.to_be_bytes());
        let digest = h.finalize().to_vec();
        match &self.last_proposals {
            Some((e, d)) if *e == epoch && *d == digest => {
                self.counters.ignored_stale += 1;
                return Ok(());
            }
            Some((e, _)) if *e == epoch => {
                // A new batch for the same epoch replaces one the DS abandoned.
                self.clear_transition();
            }
            _ => {}
        }
        let table = match verify_table(bindings, self.trust.ds_public()) {
            Ok(t) => t,
            Err(e) => {
                return Err(self.refuse(now, binding_refusal(InvalidReason::BadProposal, &e)));
            }
        };
        if table.call_id != self.call_id
            || table.group_id != group_id
            || table.epoch != epoch
            || !table.is_ref_table()
        {
            let e = BindingError::Scope("table");
            return Err(self.refuse(now, binding_refusal(InvalidReason::BadProposal, &e)));
        }
        let mut adds = BTreeMap::new();
        for p in proposals {
            match self.process_proposal(now, p.as_bytes(), &table.by_ref) {
                Ok(Some((r, e))) => {
                    adds.insert(r, e);
                }
                Ok(None) => {}
                Err(r) => {
                    self.clear_transition();
                    return Err(self.refuse(now, r));
                }
            }
        }
        self.add_bindings.extend(adds);
        self.last_proposals = Some((epoch, digest));
        self.rebuild_roster();
        if self.own_leaf() == Some(committer) {
            self.commit_now(now);
        }
        Ok(())
    }

    /// One external proposal: from `External(0)`, Add or Remove only, an Add's leaf valid and
    /// bound. Returns the Add's `(ref, binding)`.
    fn process_proposal(
        &mut self,
        now: Millis,
        bytes: &[u8],
        refs: &BTreeMap<Vec<u8>, VerifiedEntry>,
    ) -> Result<Option<(Vec<u8>, VerifiedEntry)>, Refusal> {
        let bad = |w: &'static str| Refusal::new(InvalidReason::BadProposal, w);
        if bytes.len() > caps::PROPOSAL {
            return Err(bad("size"));
        }
        let msg = MlsMessageIn::tls_deserialize_exact(bytes).map_err(|_| bad("decode"))?;
        let pm = msg
            .try_into_protocol_message()
            .map_err(|_| bad("wire_format"))?;
        let processed = {
            let Some(group) = self.group.as_mut() else {
                return Err(bad("no_group"));
            };
            group
                .process_message(&self.provider, pm)
                .map_err(|_| bad("process").with(ReportKind::ExternalSenderMismatch, None))?
        };
        if *processed.sender() != Sender::External(SenderExtensionIndex::new(0)) {
            return Err(bad("sender").with(ReportKind::ExternalSenderMismatch, None));
        }
        let ProcessedMessageContent::ProposalMessage(queued) = processed.into_content() else {
            return Err(bad("not_a_proposal"));
        };
        let queued = *queued;
        let mut add = None;
        match queued.proposal() {
            Proposal::Add(a) => {
                let kp = a.key_package();
                let leaf = kp.leaf_node();
                let lctx = self.leaf_ctx(now);
                match check_leaf(leaf.credential(), leaf.signature_key().as_slice(), &lctx) {
                    Ok(_) => {}
                    Err(e) if e.is_scope() => {
                        return Err(bad("add_scope").with(ReportKind::ScopeInvalid, None));
                    }
                    Err(_) => return Err(bad("add_leaf")),
                }
                let lt = kp.life_time();
                let now_s = now / 1000;
                let skew = self.cfg.clock_skew_s;
                if now_s.saturating_add(skew) < lt.not_before()
                    || now_s > lt.not_after().saturating_add(skew)
                {
                    return Err(bad("add_lifetime"));
                }
                let r = kp
                    .hash_ref(self.provider.crypto())
                    .map_err(|_| bad("hash_ref"))?;
                let r = r.as_slice().to_vec();
                let Some(e) = refs.get(&r) else {
                    return Err(bad("add_binding").with(ReportKind::LeafBindingInvalid, None));
                };
                add = Some((r, e.clone()));
            }
            Proposal::Remove(_) => {}
            _ => return Err(bad("type")),
        }
        let Some(group) = self.group.as_mut() else {
            return Err(bad("no_group"));
        };
        group
            .store_pending_proposal(self.provider.storage(), queued)
            .map_err(|_| bad("store"))?;
        Ok(add)
    }

    /// Forgets the transition in progress (pending proposals and any own commit).
    pub(super) fn clear_transition(&mut self) {
        if let Some(g) = self.group.as_mut() {
            let _ = g.clear_pending_commit(self.provider.storage());
            let _ = g.clear_pending_proposals(self.provider.storage());
            self.provider.purge_cleared_proposals(g.group_id());
        }
        self.own_commit = None;
        self.add_bindings.clear();
        self.last_proposals = None;
    }

    pub(super) fn on_commit_request(
        &mut self,
        now: Millis,
        group_id: &[u8],
        epoch: Epoch,
        committer: u32,
    ) -> Result<(), SessionError> {
        if !self.group_matches(group_id) || matches!(self.state, SessionState::Resyncing { .. }) {
            self.counters.ignored_stale += 1;
            return Ok(());
        }
        let mls_epoch = self.mls_epoch().unwrap_or(0);
        if epoch < mls_epoch {
            self.counters.ignored_stale += 1;
            return Ok(());
        }
        if epoch > mls_epoch {
            self.start_readd(now, InvalidReason::MissedEpoch);
            return Err(SessionError::Refused("missed_epoch"));
        }
        if self.own_leaf() == Some(committer) {
            self.commit_now(now);
        }
        Ok(())
    }

    /// The designated committer's commit: every pending proposal by reference, with an
    /// update path (always), a GroupInfo, no ratchet-tree extension.
    pub(super) fn commit_now(&mut self, now: Millis) {
        if self.own_commit.is_some() {
            return;
        }
        let built = {
            let (Some(group), Some(signer)) = (self.group.as_mut(), self.signer.as_ref()) else {
                return;
            };
            let epoch = group.epoch().as_u64();
            let gid = group.group_id().as_slice().to_vec();
            let r = group
                .commit_builder()
                .consume_proposal_store(true)
                .force_self_update(true)
                .load_psks(self.provider.storage())
                .ok()
                .and_then(|b| {
                    b.create_group_info(true)
                        .use_ratchet_tree_extension(false)
                        .build(self.provider.rand(), self.provider.crypto(), signer, |_| {
                            true
                        })
                        .ok()
                })
                .and_then(|b| b.stage_commit(&self.provider).ok());
            r.map(|bundle| (epoch, gid, bundle))
        };
        let Some((epoch, gid, bundle)) = built else {
            self.start_readd(now, InvalidReason::CommitFailed);
            return;
        };
        let (commit, welcome, group_info) = bundle.into_messages();
        let commit = commit.to_bytes().unwrap_or_default();
        let welcome = welcome.and_then(|w| w.to_bytes().ok());
        let group_info = group_info
            .and_then(|g| g.to_bytes().ok())
            .unwrap_or_default();
        self.own_commit = Some(commit.clone());
        self.send(
            Purpose::Commit,
            KeyRequest::Commit(
                self.call_id.clone(),
                KeyCommitRequest {
                    join_id: self.join_id.clone(),
                    group_id: B64::new(gid),
                    epoch,
                    commit: B64::new(commit),
                    welcome: welcome.map(B64::new),
                    group_info: B64::new(group_info),
                },
            ),
        );
        self.state = SessionState::Active {
            epoch,
            transition: Transition::Committing { epoch_from: epoch },
        };
    }

    pub(super) fn on_commit_reply(
        &mut self,
        _now: Millis,
        r: Result<KeyReply, ApiFailure>,
    ) -> Result<(), SessionError> {
        let Err(f) = r else {
            return Ok(());
        };
        // `409 e2ee_commit_superseded`, `e2ee_stale_epoch`, `e2ee_not_committer`,
        // `e2ee_no_transition`, `422 e2ee_commit_invalid`: drop the own commit and wait for the
        // broadcast one; an epoch gap heals through a `state` check.
        let known = [
            reasons::E2EE_COMMIT_SUPERSEDED,
            reasons::E2EE_STALE_EPOCH,
            reasons::E2EE_NOT_COMMITTER,
            reasons::E2EE_NO_TRANSITION,
            reasons::E2EE_COMMIT_INVALID,
        ];
        if known.iter().any(|k| f.is(k)) || f.status >= 400 {
            self.counters.commit_refused += 1;
            if let Some(g) = self.group.as_mut() {
                let _ = g.clear_pending_commit(self.provider.storage());
            }
            self.own_commit = None;
            if let SessionState::Active {
                epoch,
                transition: Transition::Committing { .. },
            } = self.state
            {
                self.state = SessionState::Active {
                    epoch,
                    transition: Transition::Idle,
                };
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ commit

    pub(super) fn on_commit(
        &mut self,
        now: Millis,
        group_id: &[u8],
        epoch_from: Epoch,
        commit: &[u8],
        slot: u8,
        bindings: &LeafBindingTable,
    ) -> Result<(), SessionError> {
        if !self.group_matches(group_id) || matches!(self.state, SessionState::Resyncing { .. }) {
            self.counters.ignored_stale += 1;
            return Ok(());
        }
        let mls_epoch = self.mls_epoch().unwrap_or(0);
        if epoch_from < mls_epoch {
            self.counters.ignored_stale += 1;
            return Ok(());
        }
        if epoch_from > mls_epoch {
            self.start_readd(now, InvalidReason::MissedEpoch);
            return Err(SessionError::Refused("missed_epoch"));
        }
        check_cap("commit", commit.len(), caps::COMMIT)?;
        if Slot::of(self.slot_base, epoch_from + 1).get() != slot {
            return Err(self.refuse(now, Refusal::new(InvalidReason::StateLost, "slot")));
        }
        let table = match verify_table(bindings, self.trust.ds_public()) {
            Ok(t)
                if t.call_id == self.call_id
                    && t.group_id == group_id
                    && t.epoch == epoch_from + 1
                    && t.is_leaf_table() =>
            {
                t
            }
            Ok(_) => {
                let e = BindingError::Scope("table");
                return Err(self.refuse(now, binding_refusal(InvalidReason::BadCommit, &e)));
            }
            Err(e) => {
                return Err(self.refuse(now, binding_refusal(InvalidReason::BadCommit, &e)));
            }
        };
        let own = self.own_commit.as_deref() == Some(commit);
        let (adds, removed) = if own {
            let Some(staged) = self.group.as_ref().and_then(|g| g.pending_commit()) else {
                return Err(self.refuse(now, Refusal::new(InvalidReason::CommitFailed, "own")));
            };
            let changes = commit_changes(staged);
            let merged = self
                .group
                .as_mut()
                .map(|g| g.merge_pending_commit(&self.provider).is_ok())
                .unwrap_or(false);
            if !merged {
                return Err(
                    self.refuse(now, Refusal::new(InvalidReason::CommitFailed, "merge_own"))
                );
            }
            changes
        } else {
            if self.own_commit.is_some() {
                if let Some(g) = self.group.as_mut() {
                    let _ = g.clear_pending_commit(self.provider.storage());
                }
                self.own_commit = None;
            }
            let staged = match self.process_commit(now, commit) {
                Ok(s) => s,
                Err(r) => return Err(self.refuse(now, r)),
            };
            if staged.self_removed() {
                self.teardown(
                    now,
                    super::LeftReason::Removed(sew_proto::e2ee::RemoveReason::Left),
                );
                return Ok(());
            }
            let changes = commit_changes(&staged);
            let merged = self
                .group
                .as_mut()
                .map(|g| g.merge_staged_commit(&self.provider, staged).is_ok())
                .unwrap_or(false);
            if !merged {
                return Err(self.refuse(now, Refusal::new(InvalidReason::CommitFailed, "merge")));
            }
            changes
        };
        self.own_commit = None;
        // What the merge cleared from the proposal queue leaves the store too.
        if let Some(g) = self.group.as_ref() {
            self.provider.purge_cleared_proposals(g.group_id());
        }
        // New leaves: find each added package's leaf in the merged tree by its signature key.
        let mut new_bindings = self.bindings.clone();
        let mut departed_ids = BTreeSet::new();
        for leaf in &removed {
            if let Some(e) = new_bindings.remove(leaf) {
                departed_ids.insert(e.lk_identity);
            }
        }
        let members: Vec<(u32, Vec<u8>)> = self
            .group
            .as_ref()
            .map(|g| {
                g.members()
                    .map(|m| (m.index.u32(), m.signature_key.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let mut added_leaves = BTreeMap::new();
        for (kp_ref, sig_key) in &adds {
            let Some(e) = self.add_bindings.get(kp_ref) else {
                let e = BindingError::Coverage;
                return Err(self.refuse(now, binding_refusal(InvalidReason::BadCommit, &e)));
            };
            let Some((leaf, _)) = members.iter().find(|(_, k)| k == sig_key) else {
                return Err(self.refuse(now, Refusal::new(InvalidReason::BadCommit, "add_leaf")));
            };
            added_leaves.insert(*leaf, e.clone());
            new_bindings.insert(*leaf, e.clone());
        }
        // Cross-check the DS's table for the new leaves.
        let table_ok = table.by_leaf.len() == added_leaves.len()
            && table
                .by_leaf
                .iter()
                .all(|(l, e)| added_leaves.get(l).is_some_and(|a| a == e));
        if !table_ok {
            let e = BindingError::Changed;
            return Err(self.refuse(now, binding_refusal(InvalidReason::BadCommit, &e)));
        }
        let bound: BTreeSet<LkIdentity> = new_bindings
            .values()
            .map(|e| e.lk_identity.clone())
            .collect();
        let departed: BTreeSet<LkIdentity> = departed_ids
            .into_iter()
            .filter(|i| !bound.contains(i))
            .collect();
        self.bindings = new_bindings;
        self.add_bindings.clear();
        self.last_proposals = None;
        self.refresh_leaf_ids(now);
        let new_epoch = epoch_from + 1;
        self.known_group = Some((group_id.to_vec(), new_epoch));
        self.install_epoch(now, departed);
        // The recorder gets the pending epoch before `ready`.
        self.release_pending(now);
        self.rebuild_roster();
        Ok(())
    }

    fn process_commit(&mut self, now: Millis, commit: &[u8]) -> Result<StagedCommit, Refusal> {
        let failed = |w: &'static str| Refusal::new(InvalidReason::CommitFailed, w);
        let bad = |w: &'static str| Refusal::new(InvalidReason::BadCommit, w);
        let msg = MlsMessageIn::tls_deserialize_exact(commit).map_err(|_| failed("decode"))?;
        let pm = msg
            .try_into_protocol_message()
            .map_err(|_| bad("wire_format"))?;
        let processed = {
            let Some(group) = self.group.as_mut() else {
                return Err(failed("no_group"));
            };
            group
                .process_message(&self.provider, pm)
                .map_err(|_| failed("process"))?
        };
        let committer = match processed.sender() {
            Sender::Member(l) => *l,
            _ => return Err(bad("sender")),
        };
        let ProcessedMessageContent::StagedCommitMessage(staged) = processed.into_content() else {
            return Err(bad("not_a_commit"));
        };
        let staged = *staged;
        self.validate_commit(now, &staged, committer)?;
        Ok(staged)
    }

    /// The recorder release, member side.
    fn validate_commit(
        &self,
        now: Millis,
        staged: &StagedCommit,
        committer: LeafNodeIndex,
    ) -> Result<(), Refusal> {
        let bad = |w: &'static str| Refusal::new(InvalidReason::BadCommit, w);
        let Some(group) = self.group.as_ref() else {
            return Err(bad("no_group"));
        };
        // Rule 2: exactly the stored pending proposals, by reference.
        let pending: BTreeSet<Vec<u8>> = group
            .pending_proposals()
            .map(|q| q.proposal_reference_ref().as_slice().to_vec())
            .collect();
        let mut got = BTreeSet::new();
        for q in staged.queued_proposals() {
            if q.proposal_or_ref_type() != ProposalOrRefType::Reference {
                return Err(bad("by_value"));
            }
            got.insert(q.proposal_reference_ref().as_slice().to_vec());
        }
        if got != pending {
            return Err(bad("proposal_set"));
        }
        // Rule 3: the committer is not removed by it.
        let removed: BTreeSet<u32> = staged
            .remove_proposals()
            .map(|r| r.remove_proposal().removed().u32())
            .collect();
        if removed.contains(&committer.u32()) {
            return Err(bad("committer_removed"));
        }
        // Rule 4: an update path.
        let Some(path) = staged.update_path_leaf_node() else {
            return Err(bad("no_path"));
        };
        // Rule 5: the committer's credential is unchanged.
        let Some(old) = group.member(committer) else {
            return Err(bad("committer"));
        };
        if old.tls_serialize_detached().ok() != path.credential().tls_serialize_detached().ok() {
            return Err(bad("credential_changed"));
        }
        // Rules 6 and 8: every credential to verify is valid (the CRL included).
        let lctx = self.leaf_ctx(now);
        if let Err(e) = check_leaf(path.credential(), path.signature_key().as_slice(), &lctx) {
            return Err(if e.is_scope() {
                bad("scope").with(ReportKind::ScopeInvalid, Some(committer.u32()))
            } else {
                bad("committer_leaf")
            });
        }
        // Rule 7: no two leaves share a member or an engine identity afterwards.
        let mut members = BTreeSet::new();
        let mut identities = BTreeSet::new();
        for m in group.members() {
            let leaf = m.index.u32();
            if removed.contains(&leaf) {
                continue;
            }
            let id = match self.leaf_ids.get(&leaf) {
                Some(id) => id.clone(),
                None => {
                    check_leaf(&m.credential, &m.signature_key, &lctx).map_err(|_| bad("leaf"))?
                }
            };
            if self.trust.crl().contains(&id.serial) {
                return Err(bad("revoked"));
            }
            if !members.insert(id.member_id) {
                return Err(bad("duplicate_member").with(ReportKind::DuplicateMember, Some(leaf)));
            }
            if let Some(e) = self.bindings.get(&leaf) {
                identities.insert(e.lk_identity.clone());
            }
        }
        for a in staged.add_proposals() {
            let kp = a.add_proposal().key_package();
            let leaf = kp.leaf_node();
            let id = check_leaf(leaf.credential(), leaf.signature_key().as_slice(), &lctx)
                .map_err(|e| {
                    if e.is_scope() {
                        bad("scope").with(ReportKind::ScopeInvalid, None)
                    } else {
                        bad("add_leaf")
                    }
                })?;
            if !members.insert(id.member_id) {
                return Err(bad("duplicate_member").with(ReportKind::DuplicateMember, None));
            }
            if let Ok(r) = kp.hash_ref(self.provider.crypto())
                && let Some(e) = self.add_bindings.get(r.as_slice())
                && !identities.insert(e.lk_identity.clone())
            {
                return Err(bad("duplicate_identity").with(ReportKind::DuplicateMember, None));
            }
        }
        Ok(())
    }

    /// Re-verifies every leaf of the held group (after a merge or create).
    pub(super) fn refresh_leaf_ids(&mut self, now: Millis) {
        let Some(group) = self.group.as_ref() else {
            return;
        };
        let lctx = self.leaf_ctx(now);
        let mut ids = BTreeMap::new();
        for m in group.members() {
            let leaf = m.index.u32();
            if let Some(id) = self.leaf_ids.get(&leaf)
                && self
                    .leaf_keys
                    .get(&leaf)
                    .is_some_and(|k| k == &m.signature_key)
            {
                ids.insert(leaf, id.clone());
                continue;
            }
            if let Ok(id) = check_leaf(&m.credential, &m.signature_key, &lctx) {
                ids.insert(leaf, id);
            }
        }
        self.leaf_keys = group
            .members()
            .map(|m| (m.index.u32(), m.signature_key))
            .collect();
        self.leaf_ids = ids;
    }

    // ------------------------------------------------------------------ activate

    pub(super) fn on_activate(
        &mut self,
        now: Millis,
        group_id: &[u8],
        epoch: Epoch,
        slot: u8,
    ) -> Result<(), SessionError> {
        if !self.group_matches(group_id) {
            self.counters.ignored_stale += 1;
            return Ok(());
        }
        if self
            .active
            .as_ref()
            .is_some_and(|a| a.group_id == group_id && epoch <= a.epoch)
        {
            self.counters.ignored_stale += 1;
            return Ok(());
        }
        let mls_epoch = self.mls_epoch().unwrap_or(0);
        if epoch > mls_epoch {
            self.start_readd(now, InvalidReason::MissedEpoch);
            return Err(SessionError::Refused("missed_epoch"));
        }
        let Some(p) = &self.pending else {
            self.counters.ignored_stale += 1;
            return Ok(());
        };
        if p.epoch != epoch {
            self.counters.ignored_stale += 1;
            return Ok(());
        }
        if p.slot.get() != slot {
            return Err(self.refuse(now, Refusal::new(InvalidReason::StateLost, "slot")));
        }
        if let SessionState::Active {
            transition: Transition::Installed { .. },
            ..
        } = self.state
        {
            self.counters.activate_before_fence += 1;
        }
        self.activate(now);
        Ok(())
    }

    /// The switch: `Activate{local, slot(e), ikm_self}`, retirements scheduled.
    pub(super) fn activate(&mut self, now: Millis) {
        let Some(p) = self.pending.take() else {
            return;
        };
        self.emit_key(KeyEvent::Activate {
            identity: self.me.clone(),
            slot: p.slot,
            ikm: p.ikm.duplicate(),
        });
        self.outputs
            .push_back(super::Output::CheckSenderSlot { expected: p.slot });
        self.sender_check = Some(super::SenderCheck {
            expected: p.slot,
            attempts: 0,
        });
        let retire_at = now + self.params.retire_after_ms;
        if let Some(old) = self.active.take()
            && old.slot != p.slot
        {
            let holders: Vec<LkIdentity> = self
                .ring
                .identities()
                .filter(|id| self.ring.get(id, old.slot).is_some())
                .cloned()
                .collect();
            for id in holders {
                let forget = p.departed.contains(&id);
                self.retires.push(super::RetireJob {
                    at: retire_at,
                    identity: id,
                    slot: old.slot,
                    forget,
                });
            }
            self.retires.push(super::RetireJob {
                at: retire_at,
                identity: self.me.clone(),
                slot: old.slot,
                forget: false,
            });
            self.slot_retired_at.insert(old.slot, retire_at);
        }
        for id in &p.departed {
            if !self.retires.iter().any(|r| &r.identity == id && r.forget) {
                self.retires.push(super::RetireJob {
                    at: retire_at,
                    identity: id.clone(),
                    slot: p.slot,
                    forget: true,
                });
            }
        }
        let epoch = p.epoch;
        if self
            .reset
            .as_ref()
            .is_some_and(|r| r.new_group_id == p.group_id)
        {
            self.reset = None;
            self.init_group = p.group_id.clone();
        }
        self.active = Some(ActiveEpoch {
            group_id: p.group_id,
            epoch,
            slot: p.slot,
            leaf: p.leaf,
            ikm: p.ikm,
            code: p.code,
            since: now,
        });
        self.counters.epochs_activated += 1;
        self.recovery = None;
        self.state = SessionState::Active {
            epoch,
            transition: Transition::Idle,
        };
        self.set_status(E2eeStatus::Active { epoch, since: now });
        self.missing.clear();
        self.undecryptable.clear();
        self.missing_checked_epoch = None;
        self.missing_check_at = Some(now + self.cfg.missing_key_check_ms);
        self.rebuild_roster();
    }

    // ------------------------------------------------------------------ `state` route

    fn bindings_hash(&self, gid: &[u8], epoch: Epoch) -> [u8; 32] {
        let tbs = LeafBindingTableTbs {
            call_id: self.call_id.as_str().as_bytes().to_vec(),
            group_id: gid.to_vec(),
            epoch,
            entries: self
                .bindings
                .iter()
                .map(|(l, e)| LeafBindingEntry {
                    key: BindingKey::Leaf(*l),
                    join_id: e.join_id.bytes(),
                })
                .collect(),
        };
        Sha256::digest(tbs.to_bytes().unwrap_or_default()).into()
    }

    pub(super) fn send_state(&mut self, purpose: Purpose, with_tree: bool) {
        let body = {
            let (Some(group), Some(signer)) = (self.group.as_ref(), self.signer.as_ref()) else {
                return;
            };
            let gid = group.group_id().as_slice().to_vec();
            let epoch = group.epoch().as_u64();
            let Some(group_info) = group
                .export_group_info(self.provider.crypto(), signer, false)
                .ok()
                .and_then(|m| m.to_bytes().ok())
            else {
                return;
            };
            let tree = if with_tree {
                group.export_ratchet_tree().tls_serialize_detached().ok()
            } else {
                None
            };
            let active_epoch = self
                .active
                .as_ref()
                .filter(|a| a.group_id == gid)
                .map_or(0, |a| a.epoch);
            KeyStateRequest {
                join_id: self.join_id.clone(),
                gateway_session: self.gateway_session.clone(),
                group_id: B64::new(gid.clone()),
                leaf: group.own_leaf_index().u32(),
                epoch,
                active_epoch,
                slot_base: self.slot_base,
                tree_hash: B64::new(group.public_group().group_context().tree_hash().to_vec()),
                bindings_hash: B64::new(self.bindings_hash(&gid, epoch).to_vec()),
                group_info: B64::new(group_info),
                ratchet_tree: tree.map(B64::new),
            }
        };
        self.send(purpose, KeyRequest::State(self.call_id.clone(), body));
    }

    pub(super) fn on_state_reply(
        &mut self,
        now: Millis,
        purpose: Purpose,
        r: Result<KeyReply, ApiFailure>,
    ) -> Result<(), SessionError> {
        match r {
            Ok(KeyReply::State(resp)) => {
                if let Some(t) = &resp.bindings {
                    self.replace_bindings(now, t);
                }
                match resp.status {
                    KeyStateStatus::InSync => {
                        if let Purpose::StatePeer(id) = purpose {
                            self.report_peer(now, &id);
                        }
                    }
                    KeyStateStatus::SendTree => self.send_state(purpose, true),
                    KeyStateStatus::Rebuilding => {
                        self.retry_state_at = Some(now + resp.retry_after_ms.unwrap_or(1_000));
                    }
                    _ => {}
                }
                Ok(())
            }
            Ok(_) => Ok(()),
            Err(f) => {
                if matches!(purpose, Purpose::StateCreate)
                    && f.status == 422
                    && f.is(reasons::E2EE_GROUP_INVALID)
                {
                    self.fail(now, "create_failed");
                    return Ok(());
                }
                if f.is_behind() {
                    self.start_readd(now, InvalidReason::Behind);
                }
                Ok(())
            }
        }
    }

    /// A full table in the `state` reply replaces the client's after the binding-table checks.
    fn replace_bindings(&mut self, now: Millis, t: &LeafBindingTable) {
        let Some(group) = self.group.as_ref() else {
            return;
        };
        let gid = group.group_id().as_slice().to_vec();
        let epoch = group.epoch().as_u64();
        let own = group.own_leaf_index().u32();
        let leaves: BTreeSet<u32> = group.members().map(|m| m.index.u32()).collect();
        match self.check_leaf_table(t, &gid, epoch, leaves, own) {
            Ok(table) => {
                // A live leaf's identity never changes.
                let changed = table.iter().any(|(l, e)| {
                    self.bindings
                        .get(l)
                        .is_some_and(|old| old.lk_identity != e.lk_identity)
                });
                if changed {
                    self.report(now, ReportKind::LeafBindingInvalid, None, None);
                    return;
                }
                self.bindings = table;
                self.rebuild_roster();
            }
            Err(_) => self.report(now, ReportKind::LeafBindingInvalid, None, None),
        }
    }

    pub(super) fn on_reidentified(
        &mut self,
        now: Millis,
        gateway_session: String,
    ) -> Result<(), SessionError> {
        self.gateway_session = gateway_session;
        match self.state {
            SessionState::Resyncing { .. } => {
                self.rebind_on_next_invalid = true;
                Ok(())
            }
            _ if self.group.is_some() => {
                self.send_state(Purpose::StateRebind, false);
                Ok(())
            }
            _ => {
                // Joining with no group: the Welcome may have gone to the dead socket.
                self.rebind_on_next_invalid = true;
                self.start_readd(now, InvalidReason::StateLost);
                Ok(())
            }
        }
    }

    // ------------------------------------------------------------------ SDK signals

    pub(super) fn on_missing_key(
        &mut self,
        now: Millis,
        identity: LkIdentity,
        since: Millis,
    ) -> Result<(), SessionError> {
        self.counters.sdk_missing_key += 1;
        if !self.is_keyed(&identity) {
            return Ok(());
        }
        let e = self.missing.entry(identity).or_insert(since);
        *e = (*e).min(since);
        let base = self.active.as_ref().map_or(now, |a| a.since);
        let at = base.max(since) + self.cfg.missing_key_check_ms;
        self.missing_check_at = Some(self.missing_check_at.map_or(at, |t| t.min(at)));
        self.check_missing(now);
        Ok(())
    }

    pub(super) fn check_missing(&mut self, now: Millis) {
        let Some(a) = &self.active else {
            return;
        };
        if self.missing_checked_epoch == Some(a.epoch) {
            return;
        }
        let gap = self.cfg.missing_key_check_ms;
        let due = self
            .missing
            .values()
            .any(|since| now >= a.since.max(*since) + gap);
        if due {
            self.missing_checked_epoch = Some(a.epoch);
            self.send_state(Purpose::StateMissingKey, false);
        } else if let Some(next) = self.missing.values().map(|s| a.since.max(*s) + gap).min() {
            self.missing_check_at = Some(next);
        }
    }

    pub(super) fn on_undecryptable(
        &mut self,
        now: Millis,
        identity: LkIdentity,
        since: Millis,
        failures: u32,
        reason: super::UndecryptReason,
        missing_index: Option<Slot>,
    ) -> Result<(), SessionError> {
        if !self.is_keyed(&identity) {
            return Ok(());
        }
        let e =
            self.undecryptable
                .entry(identity)
                .or_insert((since, failures, reason, missing_index));
        e.0 = e.0.min(since);
        e.1 = failures;
        e.2 = reason;
        e.3 = missing_index;
        self.check_undecryptable(now);
        Ok(())
    }

    fn undecryptable_start(&self, since: Millis) -> Option<Millis> {
        let a = self.active.as_ref()?;
        Some(since.max(a.since + self.params.retire_after_ms))
    }

    pub(super) fn undecryptable_due(&self) -> Option<Millis> {
        let a = self.active.as_ref()?;
        self.undecryptable
            .iter()
            .filter(|(id, _)| !self.peer_reported.contains(&((*id).clone(), a.epoch)))
            .filter(|(id, _)| self.roster_active(id))
            .filter_map(|(_, (since, ..))| self.undecryptable_start(*since))
            .map(|t| t + self.cfg.peer_undecryptable_after_ms)
            .min()
    }

    pub(super) fn check_undecryptable(&mut self, now: Millis) {
        let Some(epoch) = self.active.as_ref().map(|a| a.epoch) else {
            return;
        };
        let mut ask = Vec::new();
        for (id, (since, ..)) in &self.undecryptable {
            if self.peer_reported.contains(&(id.clone(), epoch)) {
                continue;
            }
            // Only for an `Active` remote leaf (not joining, not leaving).
            if !self.roster_active(id) {
                continue;
            }
            let Some(start) = self.undecryptable_start(*since) else {
                continue;
            };
            if now >= start + self.cfg.peer_undecryptable_after_ms {
                ask.push(id.clone());
            }
        }
        for id in ask {
            self.peer_reported.insert((id.clone(), epoch));
            self.send_state(Purpose::StatePeer(id), false);
        }
    }
}

/// What a staged commit adds (`(key_package_ref, leaf signature key)`) and removes.
/// `(key_package_ref, leaf signature key)` of each Add, and the removed leaves.
type CommitChanges = (Vec<(Vec<u8>, Vec<u8>)>, BTreeSet<u32>);

fn commit_changes(staged: &StagedCommit) -> CommitChanges {
    let crypto = sew_mls_crypto::MlsCrypto::default();
    let adds = staged
        .add_proposals()
        .filter_map(|a| {
            let kp = a.add_proposal().key_package();
            let r = kp.hash_ref(&crypto).ok()?;
            Some((
                r.as_slice().to_vec(),
                kp.leaf_node().signature_key().as_slice().to_vec(),
            ))
        })
        .collect();
    let removed = staged
        .remove_proposals()
        .map(|r| r.remove_proposal().removed().u32())
        .collect();
    (adds, removed)
}

/// A crafted commit for negative tests (feature `testing`).
#[cfg(feature = "testing")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CraftCommit {
    /// The pending proposals by reference plus one Add by value (`spare` key package).
    ByValue,
    /// One pending proposal left out.
    OmitLast,
}

#[cfg(feature = "testing")]
impl CallSession {
    /// Builds a crafted commit on the held group (test hook): returns the `MLSMessage` bytes and
    /// clears it again, so the session does not treat it as its own.
    pub fn craft_commit_for_test(
        &mut self,
        kind: CraftCommit,
        spare: Option<&[u8]>,
    ) -> Option<Vec<u8>> {
        let spare_kp = match spare {
            Some(b) => {
                let msg = MlsMessageIn::tls_deserialize_exact(b).ok()?;
                let MlsMessageBodyIn::KeyPackage(k) = msg.extract() else {
                    return None;
                };
                Some(
                    k.validate(
                        self.provider.crypto(),
                        openmls::prelude::ProtocolVersion::Mls10,
                    )
                    .ok()?,
                )
            }
            None => None,
        };
        let group = self.group.as_mut()?;
        let signer = self.signer.as_ref()?;
        let mut omitted = None;
        if kind == CraftCommit::OmitLast {
            let last = group.pending_proposals().last().cloned()?;
            group
                .remove_pending_proposal(self.provider.storage(), last.proposal_reference_ref())
                .ok()?;
            omitted = Some(last);
        }
        let mut builder = group
            .commit_builder()
            .consume_proposal_store(true)
            .force_self_update(true);
        if let Some(kp) = spare_kp {
            builder = builder.propose_adds([kp]);
        }
        let bundle = builder
            .load_psks(self.provider.storage())
            .ok()?
            .create_group_info(true)
            .use_ratchet_tree_extension(false)
            .build(self.provider.rand(), self.provider.crypto(), signer, |_| {
                true
            })
            .ok()?
            .stage_commit(&self.provider)
            .ok()?;
        let (commit, _w, _gi) = bundle.into_messages();
        let bytes = commit.to_bytes().ok()?;
        let group = self.group.as_mut()?;
        let _ = group.clear_pending_commit(self.provider.storage());
        if let Some(q) = omitted {
            let _ = group.store_pending_proposal(self.provider.storage(), q);
        }
        Some(bytes)
    }
}
