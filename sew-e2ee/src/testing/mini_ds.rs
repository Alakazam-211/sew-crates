//! `MiniDs`: an in-process model of the server's delivery service.
//!
//! It holds openmls' **public** group only (like the server), signs external Add/Remove proposals
//! with the test DS key, validates and merges the designated committer's commit, routes Welcomes to
//! joiners with the ratchet tree and a signed `LeafBinding` table, collects `ready`, and activates
//! on all-ready or after `activate_timeout_ms`. It models the key route answers and their
//! `409`/`422` reasons, `group_id` on every message, the explicit `slot`, `removed`,
//! `send_tree`/`rebuilding`, the lowest-eligible-leaf committer with failover, one transition in
//! flight, 200 ms pacing, resets, re-adds and the recorder release routing.
//!
//! Faults model a misbehaving or restarted server for the negative tests. Sew's contract tests
//! keep it honest against the real server.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use openmls::ciphersuite::hash_ref::ProposalRef;
use openmls::prelude::{
    ExternalProposal, GroupId, KeyPackage, KeyPackageIn, LeafNodeIndex, MlsMessageBodyIn,
    MlsMessageIn, ProcessedMessageContent, ProposalOrRefType, ProposalStore, ProtocolVersion,
    PublicGroup, RatchetTreeIn, Sender, SenderExtensionIndex,
};
use openmls_basic_credential::SignatureKeyPair;
use openmls_memory_storage::MemoryStorage;
use openmls_traits::types::SignatureScheme;
use rand::rngs::StdRng;
use rand::{RngCore, SeedableRng};
use sew_mls_crypto::{MlsCrypto, MlsProvider};
use sew_proto::e2ee::tls::{
    BindingKey, LeafBindingEntry, LeafBindingTableTbs, RecorderCertTbs, TlsCodec,
};
use sew_proto::e2ee::{
    B64, CallId, CallKeysInit, CallKeysMessage, CallScope, E2eeJoin, KeyCommitAccepted,
    KeyCommitRequest, KeyInvalidRequest, KeyJoinRequest, KeyPackageRefItem,
    KeyPackageUploadRequest, KeyPackageUploadResponse, KeyReadyRequest, KeyReleaseRequest,
    KeyReportRequest, KeyResetPackageRequest, KeyStateRequest, KeyStateResponse, KeyStateStatus,
    LeafBindingTable, Release, RemoveReason, ResetReason, TransitionReason, reasons,
};
use sha2::{Digest, Sha256};
use tls_codec::{Deserialize as _, Serialize as _};

use super::fixtures::{Install, id};
use crate::config::Params;
use crate::ids::{Id, JoinId, LkIdentity, Millis, Slot, install_id_bytes};
use crate::wire::{ApiFailure, KeyReply, KeyRequest};

/// A Sim client index.
pub type ClientId = usize;

/// MiniDs timings and limits (the server's defaults).
#[derive(Debug, Clone)]
pub struct DsConfig {
    /// Minimum time between an `activate` and the next `proposals`.
    pub pacing_ms: u64,
    /// The designated committer's deadline before failover.
    pub commit_timeout_ms: u64,
    /// `activate` goes out on all-ready or after this long.
    pub activate_timeout_ms: u64,
    /// A reset collects packages this long.
    pub collect_ms: u64,
    /// Changes per batch.
    pub max_batch: usize,
    /// A ref lives this long.
    pub ref_ttl_ms: u64,
    /// `retry_after_ms` in a `rebuilding` answer.
    pub rebuild_retry_ms: u64,
}

impl Default for DsConfig {
    fn default() -> DsConfig {
        DsConfig {
            pacing_ms: 200,
            commit_timeout_ms: 500,
            activate_timeout_ms: 1_000,
            collect_ms: 300,
            max_batch: 32,
            ref_ttl_ms: 3_600_000,
            rebuild_retry_ms: 200,
        }
    }
}

/// Server-side misbehaviour and restarts, for negative tests.
#[derive(Debug, Clone, Default)]
pub struct DsFaults {
    /// Never act on `invalid` from these joins.
    pub never_readd: BTreeSet<JoinId>,
    /// Forward any commit from the committer without validating it (a malicious DS).
    pub accept_any_commit: bool,
    /// Send these clients binding tables with these two joins' entries swapped.
    pub swap_bindings_for: Option<(ClientId, JoinId, JoinId)>,
    /// Send `activate` with a wrong slot to these clients.
    pub wrong_activate_slot_to: BTreeSet<ClientId>,
    /// Do not send `activate` to these clients.
    pub drop_activate_to: BTreeSet<ClientId>,
    /// Corrupt the next table sent to these clients in the named way.
    pub corrupt_table_to: BTreeMap<ClientId, TableCorruption>,
    /// Answer `state` with `send_tree` to this many clients during a rebuild, `rebuilding` to the
    /// rest until the group is rebuilt.
    pub rebuild_send_tree: usize,
    /// Answer `state` of these joins with this status code and reason.
    pub state_answer: BTreeMap<JoinId, (u16, &'static str)>,
    /// Answer `commit` of these joins with this status and reason, once.
    pub commit_answer: BTreeMap<JoinId, VecDeque<(u16, &'static str)>>,
    /// Ask every committer candidate at once (a failover race).
    pub ask_all_candidates: bool,
    /// A second device of a member joins as an Add, not as a swap.
    pub no_swap: bool,
    /// Every `state` answer carries the full table.
    pub always_send_bindings: bool,
}

/// How to corrupt a binding table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableCorruption {
    /// A wrong signature.
    BadSignature,
    /// A missing leaf.
    MissingLeaf,
    /// The local entry names another join.
    WrongLocal,
}

#[derive(Debug, Clone)]
struct RefState {
    member: Id,
    kp: KeyPackage,
    expires: Millis,
    used: bool,
}

#[derive(Debug, Clone)]
struct JoinState {
    client: ClientId,
    member: Id,
    leaf: Option<u32>,
    removed: bool,
    leaf_by_epoch: BTreeMap<(Vec<u8>, u64), u32>,
}

#[derive(Debug, Clone)]
enum Change {
    Add {
        join: JoinId,
        kp: KeyPackage,
    },
    Remove {
        join: JoinId,
        reason: RemoveReason,
    },
    Replace {
        join: JoinId,
        kp: KeyPackage,
    },
    Swap {
        old: JoinId,
        join: JoinId,
        kp: KeyPackage,
    },
    Rekey,
}

#[derive(Debug)]
struct Tr {
    epoch: u64,
    refs: Vec<ProposalRef>,
    adds: Vec<(JoinId, [u8; 32], Vec<u8>)>,
    removes: Vec<(u32, JoinId, Option<RemoveReason>)>,
    candidates: Vec<u32>,
    attempt: usize,
    deadline: Millis,
}

#[derive(Debug)]
struct PendingActivate {
    epoch: u64,
    slot: u8,
    deadline: Millis,
    waiting: BTreeSet<JoinId>,
}

#[derive(Debug)]
struct ResetCollect {
    new_group_id: Vec<u8>,
    slot_base: u8,
    deadline: Option<Millis>,
    packages: Vec<(JoinId, KeyPackage)>,
    creator: Option<JoinId>,
}

#[derive(Debug, Default)]
struct Rebuild {
    asked_tree: usize,
}

/// The in-process delivery service.
pub struct MiniDs {
    /// Timings.
    pub cfg: DsConfig,
    /// Faults.
    pub faults: DsFaults,
    install: Install,
    signer: SignatureKeyPair,
    call_id: CallId,
    scope: CallScope,
    crypto: MlsCrypto,
    storage: MemoryStorage,
    group: Option<PublicGroup>,
    group_id: Vec<u8>,
    slot_base: u8,
    creator: Option<JoinId>,
    joins: BTreeMap<JoinId, JoinState>,
    refs: BTreeMap<[u8; 32], RefState>,
    expected_moves: BTreeMap<JoinId, Id>,
    queue: VecDeque<Change>,
    tr: Option<Tr>,
    pending_activate: Option<PendingActivate>,
    last_activate_at: Millis,
    reset: Option<ResetCollect>,
    rebuild: Option<Rebuild>,
    seen_invalid: BTreeSet<(JoinId, Vec<u8>, u64)>,
    out: VecDeque<(ClientId, CallKeysMessage)>,
    rng: StdRng,
    recording: Option<(Id, [u8; 32])>,
    consent: BTreeMap<Id, bool>,
    /// Every `invalid` received (join, body).
    pub invalids: Vec<(JoinId, KeyInvalidRequest)>,
    /// Every `report` received.
    pub reports: Vec<(JoinId, KeyReportRequest)>,
    /// Releases forwarded to the recorder.
    pub releases: Vec<Release>,
    /// Commits accepted.
    pub commits_accepted: u64,
    last_accepted_from: Option<u64>,
    /// Epochs activated, with the time.
    pub activations: Vec<(u64, Millis)>,
    /// `commit`s received (join, epoch, commit bytes).
    pub commits_received: Vec<(JoinId, u64, Vec<u8>)>,
    /// A log for debugging (ids, epochs and kinds only).
    pub log: Vec<String>,
}

impl fmt::Debug for MiniDs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MiniDs")
            .field("call_id", &self.call_id)
            .field("epoch", &self.epoch())
            .field("joins", &self.joins.len())
            .finish_non_exhaustive()
    }
}

fn fail(status: u16, reason: &str) -> ApiFailure {
    ApiFailure::new(status, reason)
}

fn decode_kp(bytes: &[u8], crypto: &MlsCrypto) -> Option<KeyPackage> {
    let msg = MlsMessageIn::tls_deserialize_exact(bytes).ok()?;
    match msg.extract() {
        MlsMessageBodyIn::KeyPackage(k) => {
            let k: KeyPackageIn = k;
            k.validate(crypto, ProtocolVersion::Mls10).ok()
        }
        _ => None,
    }
}

fn kp_ref(kp: &KeyPackage, crypto: &MlsCrypto) -> Option<[u8; 32]> {
    kp.hash_ref(crypto).ok()?.as_slice().try_into().ok()
}

impl MiniDs {
    /// A DS for one call in `scope` of `install`.
    pub fn new(install: Install, scope: CallScope, seed: u64) -> MiniDs {
        let mut rng = StdRng::seed_from_u64(seed ^ 0x5eed_d5d5);
        let mut room = [0u8; 16];
        rng.fill_bytes(&mut room);
        let dsk = install.ds.signing_key();
        let signer = SignatureKeyPair::from_raw(
            SignatureScheme::ED25519,
            dsk.to_bytes().to_vec(),
            dsk.verifying_key().to_bytes().to_vec(),
        );
        MiniDs {
            cfg: DsConfig::default(),
            faults: DsFaults::default(),
            install,
            signer,
            call_id: CallId::from_random(room),
            scope,
            crypto: MlsCrypto::default(),
            storage: MemoryStorage::default(),
            group: None,
            group_id: Vec::new(),
            slot_base: 0,
            creator: None,
            joins: BTreeMap::new(),
            refs: BTreeMap::new(),
            expected_moves: BTreeMap::new(),
            queue: VecDeque::new(),
            tr: None,
            pending_activate: None,
            last_activate_at: 0,
            reset: None,
            rebuild: None,
            seen_invalid: BTreeSet::new(),
            out: VecDeque::new(),
            rng,
            recording: None,
            consent: BTreeMap::new(),
            invalids: Vec::new(),
            reports: Vec::new(),
            releases: Vec::new(),
            commits_accepted: 0,
            last_accepted_from: None,
            activations: Vec::new(),
            commits_received: Vec::new(),
            log: Vec::new(),
        }
    }

    // ------------------------------------------------------------------ observation

    /// The call.
    pub fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// The scope.
    pub fn scope(&self) -> CallScope {
        self.scope
    }

    /// The DS's epoch.
    pub fn epoch(&self) -> Option<u64> {
        self.group
            .as_ref()
            .map(|g| g.group_context().epoch().as_u64())
    }

    /// The group id.
    pub fn group_id(&self) -> &[u8] {
        &self.group_id
    }

    /// The slot base.
    pub fn slot_base(&self) -> u8 {
        self.slot_base
    }

    /// A join's current leaf.
    pub fn leaf_of(&self, join: &JoinId) -> Option<u32> {
        self.joins.get(join).and_then(|j| j.leaf)
    }

    /// Leaves of the DS's group.
    pub fn leaves(&self) -> Vec<u32> {
        self.group
            .as_ref()
            .map(|g| g.members().map(|m| m.index.u32()).collect())
            .unwrap_or_default()
    }

    /// Whether a transition, an activation or a queued change is pending.
    pub fn busy(&self) -> bool {
        self.tr.is_some()
            || self.pending_activate.is_some()
            || !self.queue.is_empty()
            || self.reset.is_some()
    }

    /// Takes the dispatches produced so far, in order.
    pub fn take_deliveries(&mut self) -> Vec<(ClientId, CallKeysMessage)> {
        self.out.drain(..).collect()
    }

    /// The default params every init carries.
    pub fn params(&self) -> sew_proto::e2ee::CallKeysParams {
        Params::default_wire()
    }

    // ------------------------------------------------------------------ dispatch helpers

    fn send_to_join(&mut self, join: &JoinId, msg: CallKeysMessage) {
        if let Some(j) = self.joins.get(join)
            && !j.removed
        {
            self.out.push_back((j.client, msg));
        }
    }

    fn live_joins(&self) -> Vec<JoinId> {
        self.joins
            .iter()
            .filter(|(_, j)| !j.removed && j.leaf.is_some())
            .map(|(k, _)| k.clone())
            .collect()
    }

    fn join_of_leaf(&self, leaf: u32) -> Option<JoinId> {
        self.joins
            .iter()
            .find(|(_, j)| j.leaf == Some(leaf) && !j.removed)
            .map(|(k, _)| k.clone())
    }

    fn sign_table(&self, epoch: u64, entries: Vec<LeafBindingEntry>) -> LeafBindingTable {
        let mut entries = entries;
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        let tbs = LeafBindingTableTbs {
            call_id: self.call_id.as_str().as_bytes().to_vec(),
            group_id: self.group_id.clone(),
            epoch,
            entries,
        };
        self.install.ds.sign_table(&tbs)
    }

    fn full_table_entries(&self) -> Vec<LeafBindingEntry> {
        self.joins
            .iter()
            .filter(|(_, j)| !j.removed)
            .filter_map(|(k, j)| {
                Some(LeafBindingEntry {
                    key: BindingKey::Leaf(j.leaf?),
                    join_id: k.bytes(),
                })
            })
            .collect()
    }

    /// The table a client gets, after the swap and corruption faults.
    fn table_for(
        &mut self,
        client: ClientId,
        epoch: u64,
        entries: Vec<LeafBindingEntry>,
    ) -> LeafBindingTable {
        let mut entries = entries;
        if let Some((c, a, b)) = &self.faults.swap_bindings_for
            && *c == client
        {
            let (ab, bb) = (a.bytes(), b.bytes());
            for e in &mut entries {
                if e.join_id == ab {
                    e.join_id = bb;
                } else if e.join_id == bb {
                    e.join_id = ab;
                }
            }
        }
        match self.faults.corrupt_table_to.remove(&client) {
            Some(TableCorruption::MissingLeaf) => {
                entries.pop();
                self.sign_table(epoch, entries)
            }
            Some(TableCorruption::WrongLocal) => {
                let mut other = [0u8; 16];
                self.rng.fill_bytes(&mut other);
                if let Some(local) = self.joins.iter().find(|(_, j)| j.client == client) {
                    let lb = local.0.bytes();
                    for e in &mut entries {
                        if e.join_id == lb {
                            e.join_id = other;
                        }
                    }
                }
                self.sign_table(epoch, entries)
            }
            Some(TableCorruption::BadSignature) => {
                let mut t = self.sign_table(epoch, entries);
                let mut sig = t.sig.as_bytes().to_vec();
                if let Some(b) = sig.first_mut() {
                    *b ^= 1;
                }
                t.sig = B64::new(sig);
                t
            }
            None => self.sign_table(epoch, entries),
        }
    }

    fn table_hash(&self, epoch: u64) -> [u8; 32] {
        let mut entries = self.full_table_entries();
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        let tbs = LeafBindingTableTbs {
            call_id: self.call_id.as_str().as_bytes().to_vec(),
            group_id: self.group_id.clone(),
            epoch,
            entries,
        };
        Sha256::digest(tbs.to_bytes().unwrap_or_default()).into()
    }

    fn mint_join(&mut self) -> JoinId {
        let mut b = [0u8; 16];
        self.rng.fill_bytes(&mut b);
        JoinId::from_bytes(b)
    }

    fn init_for(&self, join: &JoinId, create: bool, activate_on_create: bool) -> CallKeysInit {
        CallKeysInit {
            call_id: self.call_id.clone(),
            join_id: join.clone(),
            lk_identity: join.lk_identity(),
            scope: self.scope,
            group_id: B64::new(self.group_id.clone()),
            epoch: self.epoch().unwrap_or(0),
            create,
            activate_on_create,
            slot_base: self.slot_base,
            external_sender: B64::new(self.install.external_sender()),
            params: self.params(),
        }
    }

    // ------------------------------------------------------------------ upload and joins

    /// KeyPackage upload: stores single-use refs (TTL `ref_ttl_ms`).
    pub fn upload(
        &mut self,
        now: Millis,
        member: Id,
        req: &KeyPackageUploadRequest,
    ) -> Result<KeyPackageUploadResponse, ApiFailure> {
        let mut items = Vec::new();
        for kp in &req.key_packages {
            let Some(k) = decode_kp(kp.as_bytes(), &self.crypto) else {
                return Err(fail(422, reasons::E2EE_KEY_PACKAGE_INVALID));
            };
            let Some(r) = kp_ref(&k, &self.crypto) else {
                return Err(fail(422, reasons::E2EE_KEY_PACKAGE_INVALID));
            };
            let expires = now + self.cfg.ref_ttl_ms;
            self.refs.insert(
                r,
                RefState {
                    member,
                    kp: k,
                    expires,
                    used: false,
                },
            );
            items.push(KeyPackageRefItem {
                key_package_ref: B64::new(r.to_vec()),
                expires_at_ms: expires,
            });
        }
        Ok(KeyPackageUploadResponse { items })
    }

    fn claim(&mut self, now: Millis, member: Id, r: &B64) -> Result<KeyPackage, ApiFailure> {
        let Ok(r) = <[u8; 32]>::try_from(r.as_bytes()) else {
            return Err(fail(422, reasons::E2EE_KEY_PACKAGE_REF_UNKNOWN));
        };
        let Some(st) = self.refs.get_mut(&r) else {
            return Err(fail(422, reasons::E2EE_KEY_PACKAGE_REF_UNKNOWN));
        };
        if st.member != member || st.expires <= now {
            return Err(fail(422, reasons::E2EE_KEY_PACKAGE_REF_UNKNOWN));
        }
        if st.used {
            return Err(fail(409, reasons::E2EE_KEY_PACKAGE_REF_USED));
        }
        st.used = true;
        Ok(st.kp.clone())
    }

    fn admit(
        &mut self,
        now: Millis,
        client: ClientId,
        member: Id,
        join: JoinId,
        kp: KeyPackage,
    ) -> CallKeysInit {
        self.joins.insert(
            join.clone(),
            JoinState {
                client,
                member,
                leaf: None,
                removed: false,
                leaf_by_epoch: BTreeMap::new(),
            },
        );
        if self.group.is_none() && self.creator.is_none() && self.reset.is_none() {
            let mut gid = [0u8; 16];
            self.rng.fill_bytes(&mut gid);
            self.group_id = gid.to_vec();
            self.slot_base = 0;
            self.creator = Some(join.clone());
            self.log.push(format!("create by {join}"));
            return self.init_for(&join, true, true);
        }
        let old = self
            .joins
            .iter()
            .find(|(k, j)| **k != join && j.member == member && !j.removed && j.leaf.is_some())
            .map(|(k, _)| k.clone());
        match old {
            Some(old) if !self.faults.no_swap => self.queue.push_back(Change::Swap {
                old,
                join: join.clone(),
                kp,
            }),
            _ => self.queue.push_back(Change::Add {
                join: join.clone(),
                kp,
            }),
        }
        self.maybe_start(now);
        self.init_for(&join, false, false)
    }

    /// The `e2ee` object of a voice join.
    pub fn join(
        &mut self,
        now: Millis,
        client: ClientId,
        member: Id,
        e: &E2eeJoin,
    ) -> Result<CallKeysInit, ApiFailure> {
        if !e.caps.sender_key_index {
            let mut f = fail(422, reasons::E2EE_UNSUPPORTED_CLIENT);
            f.detail = Some("sender_key_index".to_owned());
            return Err(f);
        }
        let kp = self.claim(now, member, &e.key_package_ref)?;
        let join = self.mint_join();
        Ok(self.admit(now, client, member, join, kp))
    }

    /// A move: Sew mints the join id first (`voice_server_updated{reason: moved, join_id}`).
    pub fn expect_move(&mut self, member: Id) -> JoinId {
        let j = self.mint_join();
        self.expected_moves.insert(j.clone(), member);
        j
    }

    /// The key join after a move.
    pub fn key_join(
        &mut self,
        now: Millis,
        client: ClientId,
        member: Id,
        req: &KeyJoinRequest,
    ) -> Result<CallKeysInit, ApiFailure> {
        if self.expected_moves.get(&req.join_id) != Some(&member) {
            return Err(fail(404, reasons::KEY_JOIN_REQUIRES_LIVE_ADMIT));
        }
        let kp = self.claim(now, member, &req.key_package_ref)?;
        self.expected_moves.remove(&req.join_id);
        Ok(self.admit(now, client, member, req.join_id.clone(), kp))
    }

    /// Forgets every ref (a `sew` restart).
    pub fn forget_refs(&mut self) {
        self.refs.clear();
    }

    // ------------------------------------------------------------------ key routes

    /// One key route from `join` (`state` to `report`).
    pub fn handle(
        &mut self,
        now: Millis,
        join: &JoinId,
        req: &KeyRequest,
    ) -> Result<KeyReply, ApiFailure> {
        if !self.joins.contains_key(join) {
            return Err(fail(404, "not_found"));
        }
        match req {
            KeyRequest::State(_, b) => self.k3(now, join, b).map(KeyReply::State),
            KeyRequest::Commit(_, b) => self.k4(now, join, b).map(KeyReply::CommitAccepted),
            KeyRequest::Ready(_, b) => self.k5(now, join, b).map(|()| KeyReply::Accepted),
            KeyRequest::Invalid(_, b) => self.k6(now, join, b).map(|()| KeyReply::Accepted),
            KeyRequest::ResetPackage(_, b) => self.k7(now, join, b).map(|()| KeyReply::Accepted),
            KeyRequest::Release(_, b) => self.k8(join, b).map(|()| KeyReply::Accepted),
            KeyRequest::Rekey(_, _) => {
                self.queue.push_back(Change::Rekey);
                self.maybe_start(now);
                Ok(KeyReply::Accepted)
            }
            KeyRequest::Report(_, b) => {
                self.reports.push((join.clone(), b.clone()));
                Ok(KeyReply::Accepted)
            }
        }
    }

    fn rebuild_from(&mut self, tree: &[u8], group_info: &[u8]) -> bool {
        let Ok(tree_in) = RatchetTreeIn::tls_deserialize_exact(tree) else {
            return false;
        };
        let Ok(msg) = MlsMessageIn::tls_deserialize_exact(group_info) else {
            return false;
        };
        let MlsMessageBodyIn::GroupInfo(vgi) = msg.extract() else {
            return false;
        };
        let storage = MemoryStorage::default();
        match PublicGroup::from_external(&self.crypto, &storage, tree_in, vgi, ProposalStore::new())
        {
            Ok((g, _)) => {
                self.storage = storage;
                self.group_id = g.group_id().as_slice().to_vec();
                self.group = Some(g);
                true
            }
            Err(_) => false,
        }
    }

    fn k3(
        &mut self,
        now: Millis,
        join: &JoinId,
        b: &KeyStateRequest,
    ) -> Result<KeyStateResponse, ApiFailure> {
        if let Some((status, reason)) = self.faults.state_answer.get(join).copied() {
            return Err(fail(status, reason));
        }
        let in_sync = KeyStateResponse {
            status: KeyStateStatus::InSync,
            retry_after_ms: None,
            bindings: None,
        };
        // The creator's first report (or a reset creator's).
        let creating = self.group.is_none()
            && b.group_id.as_bytes() == self.group_id.as_slice()
            && (self.creator.as_ref() == Some(join)
                || self
                    .reset
                    .as_ref()
                    .is_some_and(|r| r.creator.as_ref() == Some(join)));
        if creating {
            let Some(tree) = &b.ratchet_tree else {
                return Ok(KeyStateResponse {
                    status: KeyStateStatus::SendTree,
                    retry_after_ms: None,
                    bindings: None,
                });
            };
            if !self.rebuild_from(tree.as_bytes(), b.group_info.as_bytes()) {
                return Err(fail(422, reasons::E2EE_GROUP_INVALID));
            }
            let gid = self.group_id.clone();
            if let Some(j) = self.joins.get_mut(join) {
                j.leaf = Some(b.leaf);
                j.leaf_by_epoch.insert((gid, 0), b.leaf);
            }
            self.creator = None;
            if let Some(r) = self.reset.take() {
                for (j, kp) in r.packages {
                    if &j != join {
                        self.queue.push_back(Change::Add { join: j, kp });
                    }
                }
            }
            self.last_activate_at = now;
            self.maybe_start(now);
            return Ok(in_sync);
        }
        // A restarted DS rebuilding from the members' reports.
        if self.rebuild.is_some() {
            if let Some(tree) = &b.ratchet_tree
                && self.rebuild_from(tree.as_bytes(), b.group_info.as_bytes())
            {
                self.rebuild = None;
                return Ok(in_sync);
            }
            let asked = self.rebuild.as_ref().map_or(0, |r| r.asked_tree);
            if asked < self.faults.rebuild_send_tree {
                if let Some(r) = self.rebuild.as_mut() {
                    r.asked_tree += 1;
                }
                return Ok(KeyStateResponse {
                    status: KeyStateStatus::SendTree,
                    retry_after_ms: None,
                    bindings: None,
                });
            }
            return Ok(KeyStateResponse {
                status: KeyStateStatus::Rebuilding,
                retry_after_ms: Some(self.cfg.rebuild_retry_ms),
                bindings: None,
            });
        }
        let Some(epoch) = self.epoch() else {
            return Err(fail(409, reasons::E2EE_NOT_IN_GROUP));
        };
        if b.group_id.as_bytes() != self.group_id.as_slice() {
            return Err(fail(409, reasons::E2EE_GROUP_MISMATCH));
        }
        if self.leaf_of(join).is_none() {
            return Err(fail(409, reasons::E2EE_NOT_IN_GROUP));
        }
        if b.epoch < epoch {
            return Err(fail(409, reasons::E2EE_EPOCH_BEHIND));
        }
        let mut resp = in_sync;
        if self.faults.always_send_bindings
            || b.bindings_hash.as_bytes() != self.table_hash(epoch).as_slice()
        {
            let client = self.joins.get(join).map_or(0, |j| j.client);
            let entries = self.full_table_entries();
            resp.bindings = Some(self.table_for(client, epoch, entries));
        }
        Ok(resp)
    }

    fn k4(
        &mut self,
        now: Millis,
        join: &JoinId,
        b: &KeyCommitRequest,
    ) -> Result<KeyCommitAccepted, ApiFailure> {
        self.commits_received
            .push((join.clone(), b.epoch, b.commit.as_bytes().to_vec()));
        if let Some(q) = self.faults.commit_answer.get_mut(join)
            && let Some((status, reason)) = q.pop_front()
        {
            return Err(fail(status, reason));
        }
        if self.tr.is_none() && self.last_accepted_from == Some(b.epoch) {
            return Err(fail(409, reasons::E2EE_COMMIT_SUPERSEDED));
        }
        let Some(tr) = &self.tr else {
            return Err(fail(409, reasons::E2EE_NO_TRANSITION));
        };
        let Some(epoch) = self.epoch() else {
            return Err(fail(409, reasons::E2EE_NO_TRANSITION));
        };
        if b.epoch != epoch
            || b.epoch != tr.epoch
            || b.group_id.as_bytes() != self.group_id.as_slice()
        {
            return Err(fail(409, reasons::E2EE_STALE_EPOCH));
        }
        let leaf = self.leaf_of(join);
        let asked: Vec<u32> = if self.faults.ask_all_candidates {
            tr.candidates.clone()
        } else {
            tr.candidates.iter().take(tr.attempt + 1).copied().collect()
        };
        if !leaf.is_some_and(|l| asked.contains(&l)) {
            return Err(fail(409, reasons::E2EE_NOT_COMMITTER));
        }
        let Some(leaf) = leaf else {
            return Err(fail(409, reasons::E2EE_NOT_COMMITTER));
        };
        let commit = b.commit.as_bytes().to_vec();
        let staged = match self.stage_commit(&commit, leaf) {
            Ok(s) => s,
            Err(check) if self.faults.accept_any_commit => {
                self.log.push(format!("forwarding a bad commit ({check})"));
                let new_epoch = epoch + 1;
                let slot = Slot::of(self.slot_base, new_epoch).get();
                let call_id = self.call_id.clone();
                let gid = self.group_id.clone();
                let empty = self.sign_table(new_epoch, Vec::new());
                for j in self.live_joins() {
                    self.send_to_join(
                        &j,
                        CallKeysMessage::Commit {
                            call_id: call_id.clone(),
                            group_id: B64::new(gid.clone()),
                            epoch_from: epoch,
                            commit: B64::new(commit.clone()),
                            slot,
                            bindings: empty.clone(),
                        },
                    );
                }
                return Ok(KeyCommitAccepted {
                    accepted: true,
                    epoch: new_epoch,
                    slot,
                });
            }
            Err(check) => {
                let mut f = fail(422, reasons::E2EE_COMMIT_INVALID);
                f.detail = Some(check.to_owned());
                return Err(f);
            }
        };
        if b.welcome.is_none() && self.tr.as_ref().is_some_and(|t| !t.adds.is_empty()) {
            let mut f = fail(422, reasons::E2EE_COMMIT_INVALID);
            f.detail = Some("welcome_mismatch".to_owned());
            return Err(f);
        }
        let Some(group) = self.group.as_mut() else {
            return Err(fail(409, reasons::E2EE_NO_TRANSITION));
        };
        if group.merge_commit(&self.storage, staged).is_err() {
            return Err(fail(422, reasons::E2EE_COMMIT_INVALID));
        }
        // As the server's DS does after a merge.
        crate::provider::purge_cleared_proposals(&self.storage, group.group_id());
        let Some(tr) = self.tr.take() else {
            return Err(fail(409, reasons::E2EE_NO_TRANSITION));
        };
        self.commits_accepted += 1;
        self.last_accepted_from = Some(epoch);
        let new_epoch = epoch + 1;
        let slot = Slot::of(self.slot_base, new_epoch).get();
        let gid = self.group_id.clone();
        // Leaves after the merge.
        let members: Vec<(u32, Vec<u8>)> = self
            .group
            .as_ref()
            .map(|g| {
                g.members()
                    .map(|m| (m.index.u32(), m.signature_key))
                    .collect()
            })
            .unwrap_or_default();
        let mut joiners = Vec::new();
        let mut removed_joins = Vec::new();
        for (l, j, reason) in &tr.removes {
            if let Some(js) = self.joins.get_mut(j)
                && js.leaf == Some(*l)
            {
                js.leaf = None;
                if let Some(r) = reason {
                    js.removed = true;
                    removed_joins.push((j.clone(), js.client, *r));
                }
            }
        }
        let mut new_entries = Vec::new();
        for (j, _r, sig) in &tr.adds {
            if let Some((leaf, _)) = members.iter().find(|(_, k)| k == sig)
                && let Some(js) = self.joins.get_mut(j)
            {
                js.leaf = Some(*leaf);
                joiners.push(j.clone());
                new_entries.push(LeafBindingEntry {
                    key: BindingKey::Leaf(*leaf),
                    join_id: j.bytes(),
                });
            }
        }
        for (k, js) in self.joins.iter_mut() {
            let _ = k;
            if let Some(l) = js.leaf {
                js.leaf_by_epoch.insert((gid.clone(), new_epoch), l);
            }
        }
        let call_id = self.call_id.clone();
        let tree = self
            .group
            .as_ref()
            .and_then(|g| g.export_ratchet_tree().tls_serialize_detached().ok())
            .unwrap_or_default();
        // The commit to every remaining member (not to the joiners: they get Welcomes).
        for j in self.live_joins() {
            if joiners.contains(&j) {
                continue;
            }
            let client = self.joins.get(&j).map_or(0, |s| s.client);
            let table = self.table_for(client, new_epoch, new_entries.clone());
            self.send_to_join(
                &j,
                CallKeysMessage::Commit {
                    call_id: call_id.clone(),
                    group_id: B64::new(gid.clone()),
                    epoch_from: epoch,
                    commit: B64::new(commit.clone()),
                    slot,
                    bindings: table,
                },
            );
        }
        if let Some(w) = &b.welcome {
            for j in &joiners {
                let client = self.joins.get(j).map_or(0, |s| s.client);
                let entries = self.full_table_entries();
                let table = self.table_for(client, new_epoch, entries);
                self.send_to_join(
                    j,
                    CallKeysMessage::Welcome {
                        call_id: call_id.clone(),
                        group_id: B64::new(gid.clone()),
                        join_id: j.clone(),
                        epoch: new_epoch,
                        welcome: w.clone(),
                        ratchet_tree: B64::new(tree.clone()),
                        slot_base: self.slot_base,
                        slot,
                        bindings: table,
                    },
                );
            }
        }
        for (j, client, reason) in removed_joins {
            self.out.push_back((
                client,
                CallKeysMessage::Removed {
                    call_id: call_id.clone(),
                    group_id: B64::new(gid.clone()),
                    join_id: j,
                    reason,
                },
            ));
        }
        self.pending_activate = Some(PendingActivate {
            epoch: new_epoch,
            slot,
            deadline: now + self.cfg.activate_timeout_ms,
            waiting: self.live_joins().into_iter().collect(),
        });
        self.log
            .push(format!("commit e{epoch}->e{new_epoch} by leaf {leaf}"));
        Ok(KeyCommitAccepted {
            accepted: true,
            epoch: new_epoch,
            slot,
        })
    }

    fn stage_commit(
        &self,
        commit: &[u8],
        committer: u32,
    ) -> Result<openmls::prelude::StagedCommit, &'static str> {
        let group = self.group.as_ref().ok_or("no_group")?;
        let msg = MlsMessageIn::tls_deserialize_exact(commit).map_err(|_| "decode")?;
        let pm = match msg.extract() {
            MlsMessageBodyIn::PublicMessage(pm) => pm,
            MlsMessageBodyIn::PrivateMessage(_) => return Err("wire_format"),
            _ => return Err("decode"),
        };
        match pm.sender() {
            Sender::Member(l) if l.u32() == committer => {}
            _ => return Err("sender"),
        }
        let processed = group
            .process_message(&self.crypto, openmls::prelude::ProtocolMessage::from(pm))
            .map_err(|_| "mls")?;
        let ProcessedMessageContent::StagedCommitMessage(staged) = processed.into_content() else {
            return Err("decode");
        };
        let staged = *staged;
        let want: BTreeSet<Vec<u8>> = self
            .tr
            .as_ref()
            .map(|t| t.refs.iter().map(|r| r.as_slice().to_vec()).collect())
            .unwrap_or_default();
        let mut got = BTreeSet::new();
        for q in staged.queued_proposals() {
            if q.proposal_or_ref_type() != ProposalOrRefType::Reference {
                return Err("by_value");
            }
            got.insert(q.proposal_reference_ref().as_slice().to_vec());
        }
        if want != got {
            return Err("proposal_set");
        }
        if staged.update_path_leaf_node().is_none() {
            return Err("no_path");
        }
        Ok(staged)
    }

    fn k5(&mut self, now: Millis, join: &JoinId, b: &KeyReadyRequest) -> Result<(), ApiFailure> {
        let epoch = self.epoch().unwrap_or(0);
        if b.epoch > epoch {
            return Err(fail(422, reasons::E2EE_EPOCH_AHEAD));
        }
        let all = if let Some(p) = self.pending_activate.as_mut()
            && p.epoch == b.epoch
        {
            p.waiting.remove(join);
            p.waiting.is_empty()
        } else {
            false
        };
        if all {
            self.activate(now);
        }
        Ok(())
    }

    fn k6(&mut self, now: Millis, join: &JoinId, b: &KeyInvalidRequest) -> Result<(), ApiFailure> {
        self.invalids.push((join.clone(), b.clone()));
        if self.faults.never_readd.contains(join) {
            return Ok(());
        }
        let key = (join.clone(), b.group_id.as_bytes().to_vec(), b.epoch);
        if !self.seen_invalid.insert(key) {
            return Ok(());
        }
        let Some(kp) = decode_kp(b.key_package.as_bytes(), &self.crypto) else {
            return Err(fail(422, reasons::E2EE_KEY_PACKAGE_INVALID));
        };
        if self.group.is_some() && b.group_id.as_bytes() == self.group_id.as_slice() {
            self.queue.push_back(Change::Replace {
                join: join.clone(),
                kp,
            });
        } else if let Some(r) = self.reset.as_mut()
            && b.group_id.as_bytes() == r.new_group_id.as_slice()
        {
            r.packages.push((join.clone(), kp));
        } else {
            self.queue.push_back(Change::Replace {
                join: join.clone(),
                kp,
            });
        }
        self.maybe_start(now);
        Ok(())
    }

    fn k7(
        &mut self,
        now: Millis,
        join: &JoinId,
        b: &KeyResetPackageRequest,
    ) -> Result<(), ApiFailure> {
        let Some(r) = self.reset.as_mut() else {
            return Err(fail(409, reasons::E2EE_NO_RESET));
        };
        if r.new_group_id != b.new_group_id.as_bytes() || r.deadline.is_none() {
            return Err(fail(409, reasons::E2EE_NO_RESET));
        }
        let Some(kp) = decode_kp(b.key_package.as_bytes(), &self.crypto) else {
            return Err(fail(422, reasons::E2EE_KEY_PACKAGE_INVALID));
        };
        r.packages.push((join.clone(), kp));
        let _ = now;
        Ok(())
    }

    fn k8(&mut self, join: &JoinId, b: &KeyReleaseRequest) -> Result<(), ApiFailure> {
        let Some((rid, _)) = self.recording else {
            return Err(fail(409, reasons::E2EE_RECORDING_MISMATCH));
        };
        if rid != b.recording_id {
            return Err(fail(409, reasons::E2EE_RECORDING_MISMATCH));
        }
        let Some(js) = self.joins.get(join) else {
            return Err(fail(404, "not_found"));
        };
        if !self.consent.get(&js.member).copied().unwrap_or(false) {
            return Err(fail(403, reasons::E2EE_CONSENT_NOT_GRANTED));
        }
        if b.sender_identity != join.lk_identity().as_str() {
            return Err(fail(422, reasons::E2EE_SENDER_IDENTITY_MISMATCH));
        }
        let key = (b.group_id.as_bytes().to_vec(), b.epoch);
        if js.leaf_by_epoch.get(&key) != Some(&b.leaf) {
            return Err(fail(422, reasons::E2EE_RELEASE_LEAF_MISMATCH));
        }
        if Slot::of(self.slot_base, b.epoch).get() != b.slot
            || b.enc.len() != 32
            || b.ct.len() != 48
        {
            return Err(fail(422, reasons::E2EE_RELEASE_MALFORMED));
        }
        self.releases.push(Release {
            call_id: self.call_id.clone(),
            recording_id: b.recording_id,
            group_id: b.group_id.clone(),
            epoch: b.epoch,
            leaf: b.leaf,
            slot: b.slot,
            sender_identity: join.lk_identity(),
            enc: b.enc.clone(),
            ct: b.ct.clone(),
        });
        Ok(())
    }

    // ------------------------------------------------------------------ transitions

    /// Queues the removal of `join` (a kick, a leave).
    pub fn remove(&mut self, now: Millis, join: &JoinId, reason: RemoveReason) {
        self.queue.push_back(Change::Remove {
            join: join.clone(),
            reason,
        });
        self.maybe_start(now);
    }

    /// Queues a rekey (an empty batch).
    pub fn rekey(&mut self, now: Millis) {
        self.queue.push_back(Change::Rekey);
        self.maybe_start(now);
    }

    fn maybe_start(&mut self, now: Millis) {
        if self.group.is_none()
            || self.tr.is_some()
            || self.pending_activate.is_some()
            || self.reset.is_some()
            || self.rebuild.is_some()
            || self.queue.is_empty()
            || now < self.last_activate_at + self.cfg.pacing_ms
        {
            return;
        }
        let Some(epoch) = self.epoch() else {
            return;
        };
        let gid = self.group_id.clone();
        let mut refs = Vec::new();
        let mut proposals = Vec::new();
        let mut adds = Vec::new();
        let mut removes = Vec::new();
        let mut entries = Vec::new();
        let mut reason = TransitionReason::Rekey;
        let mut changes = 0;
        while changes < self.cfg.max_batch {
            let Some(c) = self.queue.pop_front() else {
                break;
            };
            changes += 1;
            match c {
                Change::Rekey => {}
                Change::Remove { join, reason: why } => {
                    reason = TransitionReason::Leave;
                    let Some(leaf) = self.leaf_of(&join) else {
                        if let Some(js) = self.joins.get_mut(&join) {
                            js.removed = true;
                        }
                        continue;
                    };
                    if let Some((m, r)) = self.propose_remove(leaf) {
                        proposals.push(m);
                        refs.push(r);
                        removes.push((leaf, join, Some(why)));
                    }
                }
                Change::Swap { old, join, kp } => {
                    reason = TransitionReason::Swap;
                    if let Some(leaf) = self.leaf_of(&old)
                        && let Some((m, r)) = self.propose_remove(leaf)
                    {
                        proposals.push(m);
                        refs.push(r);
                        removes.push((leaf, old, Some(RemoveReason::Replaced)));
                    }
                    let Some(kr) = kp_ref(&kp, &self.crypto) else {
                        continue;
                    };
                    let sig = kp.leaf_node().signature_key().as_slice().to_vec();
                    if let Some((m, r)) = self.propose_add(kp) {
                        proposals.push(m);
                        refs.push(r);
                        entries.push(LeafBindingEntry {
                            key: BindingKey::KeyPackageRef(kr.to_vec()),
                            join_id: join.bytes(),
                        });
                        adds.push((join, kr, sig));
                    }
                }
                Change::Add { join, kp } | Change::Replace { join, kp } => {
                    reason = TransitionReason::Join;
                    if self.joins.get(&join).is_none_or(|j| j.removed) {
                        continue;
                    }
                    if let Some(old) = self.leaf_of(&join) {
                        reason = TransitionReason::Readd;
                        if let Some((m, r)) = self.propose_remove(old) {
                            proposals.push(m);
                            refs.push(r);
                            removes.push((old, join.clone(), None));
                        }
                    }
                    let Some(kr) = kp_ref(&kp, &self.crypto) else {
                        continue;
                    };
                    let sig = kp.leaf_node().signature_key().as_slice().to_vec();
                    if let Some((m, r)) = self.propose_add(kp) {
                        proposals.push(m);
                        refs.push(r);
                        entries.push(LeafBindingEntry {
                            key: BindingKey::KeyPackageRef(kr.to_vec()),
                            join_id: join.bytes(),
                        });
                        adds.push((join, kr, sig));
                    }
                }
            }
        }
        let removing: BTreeSet<u32> = removes.iter().map(|(l, _, _)| *l).collect();
        let candidates: Vec<u32> = self
            .leaves()
            .into_iter()
            .filter(|l| !removing.contains(l))
            .collect();
        if candidates.is_empty() {
            return;
        }
        let committer = candidates[0];
        let call_id = self.call_id.clone();
        for j in self.live_joins() {
            let client = self.joins.get(&j).map_or(0, |s| s.client);
            let table = self.table_for(client, epoch, entries.clone());
            self.send_to_join(
                &j,
                CallKeysMessage::Proposals {
                    call_id: call_id.clone(),
                    group_id: B64::new(gid.clone()),
                    epoch,
                    proposals: proposals.iter().map(|p| B64::new(p.clone())).collect(),
                    committer,
                    reason,
                    bindings: table,
                },
            );
        }
        if self.faults.ask_all_candidates {
            for c in candidates.iter().skip(1) {
                for j in self.live_joins() {
                    self.send_to_join(
                        &j,
                        CallKeysMessage::CommitRequest {
                            call_id: call_id.clone(),
                            group_id: B64::new(gid.clone()),
                            epoch,
                            committer: *c,
                            attempt: 2,
                        },
                    );
                }
            }
        }
        self.log.push(format!(
            "proposals e{epoch}: {} adds, {} removes, committer {committer}",
            adds.len(),
            removes.len()
        ));
        self.tr = Some(Tr {
            epoch,
            refs,
            adds,
            removes,
            candidates,
            attempt: 0,
            deadline: now + self.cfg.commit_timeout_ms,
        });
    }

    fn run_own(&mut self, message: Vec<u8>) -> Option<(Vec<u8>, ProposalRef)> {
        let group = self.group.as_mut()?;
        let msg = MlsMessageIn::tls_deserialize_exact(&message).ok()?;
        let pm = msg.try_into_protocol_message().ok()?;
        let processed = group.process_message(&self.crypto, pm).ok()?;
        let ProcessedMessageContent::ProposalMessage(q) = processed.into_content() else {
            return None;
        };
        let r = q.proposal_reference_ref().clone();
        group.add_proposal(&self.storage, *q).ok()?;
        Some((message, r))
    }

    fn propose_add(&mut self, kp: KeyPackage) -> Option<(Vec<u8>, ProposalRef)> {
        let ctx = self.group.as_ref()?.group_context();
        let m = ExternalProposal::new_add::<MlsProvider>(
            kp,
            ctx.group_id().clone(),
            ctx.epoch(),
            &self.signer,
            SenderExtensionIndex::new(0),
        )
        .ok()?
        .to_bytes()
        .ok()?;
        self.run_own(m)
    }

    fn propose_remove(&mut self, leaf: u32) -> Option<(Vec<u8>, ProposalRef)> {
        let ctx = self.group.as_ref()?.group_context();
        let m = ExternalProposal::new_remove::<MlsProvider>(
            LeafNodeIndex::new(leaf),
            ctx.group_id().clone(),
            ctx.epoch(),
            &self.signer,
            SenderExtensionIndex::new(0),
        )
        .ok()?
        .to_bytes()
        .ok()?;
        self.run_own(m)
    }

    fn clear_proposals(&mut self) {
        if let Some(tr) = self.tr.take()
            && let Some(g) = self.group.as_mut()
        {
            for r in &tr.refs {
                let _ = g.remove_proposal(&self.storage, r);
            }
        }
    }

    fn activate(&mut self, now: Millis) {
        let Some(p) = self.pending_activate.take() else {
            return;
        };
        let call_id = self.call_id.clone();
        let gid = self.group_id.clone();
        for j in self.live_joins() {
            let client = self.joins.get(&j).map_or(0, |s| s.client);
            if self.faults.drop_activate_to.contains(&client) {
                continue;
            }
            let slot = if self.faults.wrong_activate_slot_to.contains(&client) {
                (p.slot + 7) % 64
            } else {
                p.slot
            };
            self.send_to_join(
                &j,
                CallKeysMessage::Activate {
                    call_id: call_id.clone(),
                    group_id: B64::new(gid.clone()),
                    epoch: p.epoch,
                    slot,
                },
            );
        }
        self.activations.push((p.epoch, now));
        self.last_activate_at = now;
        self.log.push(format!("activate e{}", p.epoch));
    }

    // ------------------------------------------------------------------ resets, restarts

    /// Forces a group reset: a new group whose ring continues at the active
    /// slot.
    pub fn force_reset(&mut self, now: Millis, reason: ResetReason) {
        self.clear_proposals();
        self.pending_activate = None;
        self.queue.clear();
        let mut gid = [0u8; 16];
        self.rng.fill_bytes(&mut gid);
        let epoch = self.epoch().unwrap_or(0);
        let slot_base = Slot::of(self.slot_base, epoch).get();
        let call_id = self.call_id.clone();
        for j in self.live_joins() {
            self.send_to_join(
                &j,
                CallKeysMessage::Reset {
                    call_id: call_id.clone(),
                    new_group_id: B64::new(gid.to_vec()),
                    reason,
                    slot_base,
                    collect_ms: self.cfg.collect_ms,
                },
            );
        }
        self.reset = Some(ResetCollect {
            new_group_id: gid.to_vec(),
            slot_base,
            deadline: Some(now + self.cfg.collect_ms),
            packages: Vec::new(),
            creator: None,
        });
        self.log.push(format!("reset to slot_base {slot_base}"));
    }

    fn finish_collect(&mut self, now: Millis) {
        let Some(r) = self.reset.as_mut() else {
            return;
        };
        r.deadline = None;
        let Some((creator, _)) = r.packages.first().cloned() else {
            return;
        };
        r.creator = Some(creator.clone());
        let gid = r.new_group_id.clone();
        let slot_base = r.slot_base;
        // The old group is gone; every join's leaf is re-made in the new group.
        self.group = None;
        self.storage = MemoryStorage::default();
        self.group_id = gid;
        self.slot_base = slot_base;
        for js in self.joins.values_mut() {
            js.leaf = None;
        }
        let init = self.init_for(&creator, true, false);
        self.send_to_join(&creator, CallKeysMessage::Init(init));
        let _ = now;
    }

    /// Drops the DS's state (a `sew` restart): members re-identify and `state` rebuilds.
    pub fn drop_state(&mut self) {
        self.group = None;
        self.storage = MemoryStorage::default();
        self.tr = None;
        self.pending_activate = None;
        self.rebuild = Some(Rebuild::default());
    }

    // ------------------------------------------------------------------ recorder

    /// Starts a recording with the recorder's key and identity; announces it to every member.
    pub fn start_recording(
        &mut self,
        now: Millis,
        recording_id: Id,
        hpke_pub: [u8; 32],
        recorder_identity: LkIdentity,
    ) {
        let cert = self.recorder_cert(now, recording_id, hpke_pub);
        self.recording = Some((recording_id, hpke_pub));
        let call_id = self.call_id.clone();
        for j in self.live_joins() {
            self.send_to_join(
                &j,
                CallKeysMessage::Recorder {
                    call_id: call_id.clone(),
                    recording_id,
                    recorder_cert: Some(B64::new(cert.clone())),
                    hpke_pub: Some(B64::new(hpke_pub.to_vec())),
                    lk_identity: Some(recorder_identity.clone()),
                    removed: false,
                },
            );
        }
    }

    /// Re-announces the recorder to one join (a late joiner gets it after its Welcome).
    pub fn announce_recorder_to(
        &mut self,
        now: Millis,
        join: &JoinId,
        recorder_identity: LkIdentity,
    ) {
        let Some((rid, hpke)) = self.recording else {
            return;
        };
        let cert = self.recorder_cert(now, rid, hpke);
        let call_id = self.call_id.clone();
        self.send_to_join(
            join,
            CallKeysMessage::Recorder {
                call_id,
                recording_id: rid,
                recorder_cert: Some(B64::new(cert)),
                hpke_pub: Some(B64::new(hpke.to_vec())),
                lk_identity: Some(recorder_identity),
                removed: false,
            },
        );
    }

    /// A recorder certificate for this call signed by the test CA.
    pub fn recorder_cert(&self, now: Millis, recording_id: Id, hpke_pub: [u8; 32]) -> Vec<u8> {
        let tbs = RecorderCertTbs {
            version: 1,
            install_id: install_id_bytes(self.install.install_id),
            call_id: self.call_id.as_str().as_bytes().to_vec(),
            recording_id: recording_id.get(),
            hpke_pub,
            not_before: now / 1000 - 60,
            not_after: now / 1000 + 3_600,
        };
        self.install
            .ca
            .sign_recorder(tbs)
            .to_bytes()
            .unwrap_or_default()
    }

    /// Stops the recording (`recorder{removed: true}`) and rekeys.
    pub fn stop_recording(&mut self, now: Millis) {
        let Some((rid, _)) = self.recording.take() else {
            return;
        };
        let call_id = self.call_id.clone();
        for j in self.live_joins() {
            self.send_to_join(
                &j,
                CallKeysMessage::Recorder {
                    call_id: call_id.clone(),
                    recording_id: rid,
                    recorder_cert: None,
                    hpke_pub: None,
                    lk_identity: None,
                    removed: true,
                },
            );
        }
        self.rekey(now);
    }

    /// A member's consent; a withdrawal rekeys.
    pub fn set_consent(&mut self, now: Millis, member: Id, granted: bool) {
        let was = self.consent.insert(member, granted).unwrap_or(false);
        if was && !granted {
            self.rekey(now);
        }
    }

    /// Sends a raw dispatch to one join (crafted messages in tests).
    pub fn send_raw(&mut self, join: &JoinId, msg: CallKeysMessage) {
        self.send_to_join(join, msg);
    }

    /// Signs a crafted table for this call and group (tests).
    pub fn sign_entries(&self, epoch: u64, entries: Vec<LeafBindingEntry>) -> LeafBindingTable {
        self.sign_table(epoch, entries)
    }

    /// The DS signer (crafted external proposals in tests).
    pub fn ds_signer(&self) -> &SignatureKeyPair {
        &self.signer
    }

    /// The public group (crafted proposals in tests).
    pub fn public_group(&self) -> Option<&PublicGroup> {
        self.group.as_ref()
    }

    /// The id of a member's join, if admitted.
    pub fn join_of_member(&self, member: Id) -> Option<JoinId> {
        self.joins
            .iter()
            .filter(|(_, j)| j.member == member && !j.removed)
            .map(|(k, _)| k.clone())
            .next_back()
    }

    /// The client of a join.
    pub fn client_of(&self, join: &JoinId) -> Option<ClientId> {
        self.joins.get(join).map(|j| j.client)
    }

    // ------------------------------------------------------------------ timers

    /// The next time `handle_timeout` must run.
    pub fn poll_timeout(&self) -> Option<Millis> {
        let mut t: Vec<Millis> = Vec::new();
        if let Some(tr) = &self.tr {
            t.push(tr.deadline);
        }
        if let Some(p) = &self.pending_activate {
            t.push(p.deadline);
        }
        if let Some(r) = &self.reset
            && let Some(d) = r.deadline
        {
            t.push(d);
        }
        if self.group.is_some()
            && self.tr.is_none()
            && self.pending_activate.is_none()
            && !self.queue.is_empty()
        {
            t.push(self.last_activate_at + self.cfg.pacing_ms);
        }
        t.into_iter().min()
    }

    /// Runs the timers due at `now`.
    pub fn handle_timeout(&mut self, now: Millis) {
        if let Some(d) = self.reset.as_ref().and_then(|r| r.deadline)
            && now >= d
        {
            self.finish_collect(now);
        }
        if self
            .pending_activate
            .as_ref()
            .is_some_and(|p| now >= p.deadline)
        {
            self.activate(now);
        }
        let failover = self.tr.as_ref().is_some_and(|t| now >= t.deadline);
        if failover {
            let next = self.tr.as_mut().and_then(|t| {
                t.attempt += 1;
                t.deadline = now + self.cfg.commit_timeout_ms;
                (t.attempt < 3)
                    .then(|| t.candidates.get(t.attempt).copied())
                    .flatten()
            });
            match next {
                Some(c) => {
                    let epoch = self.epoch().unwrap_or(0);
                    let attempt = self.tr.as_ref().map_or(2, |t| t.attempt as u32 + 1);
                    let call_id = self.call_id.clone();
                    let gid = self.group_id.clone();
                    for j in self.live_joins() {
                        self.send_to_join(
                            &j,
                            CallKeysMessage::CommitRequest {
                                call_id: call_id.clone(),
                                group_id: B64::new(gid.clone()),
                                epoch,
                                committer: c,
                                attempt,
                            },
                        );
                    }
                }
                None => self.force_reset(now, ResetReason::CommitterTimeout),
            }
        }
        self.maybe_start(now);
    }

    /// The join id of a leaf.
    pub fn join_at_leaf(&self, leaf: u32) -> Option<JoinId> {
        self.join_of_leaf(leaf)
    }

    /// The scope's channel id.
    pub fn channel(&self) -> Id {
        self.scope.channel_id()
    }

    /// The install.
    pub fn install(&self) -> &Install {
        &self.install
    }

    /// A fresh 16-byte group id (crafted messages).
    pub fn random_group_id(&mut self) -> Vec<u8> {
        let mut gid = [0u8; 16];
        self.rng.fill_bytes(&mut gid);
        gid.to_vec()
    }

    /// The member a join belongs to.
    pub fn member_of(&self, join: &JoinId) -> Option<Id> {
        self.joins.get(join).map(|j| j.member)
    }

    /// `GroupId` of the current group (crafted proposals).
    pub fn openmls_group_id(&self) -> GroupId {
        GroupId::from_slice(&self.group_id)
    }

    /// The test install's id helper.
    pub fn id(v: u64) -> Id {
        id(v)
    }
}
