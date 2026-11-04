//! `CallSession`: the per-call state machine.
//!
//! Sans-IO: it takes inputs (server messages, replies, bridge acknowledgements, device
//! signatures, time) and emits outputs (key routes to POST, key events for the bridge, status).
//! It never does IO, never reads a clock, never spawns and never blocks. Every method is
//! synchronous and `&mut self`; there are no threads and no `Send` requirement.
//!
//! Invariants (asserted by tests and the session fuzzer):
//! - **I1** the local identity's key appears only in `Activate`, never in `Install`;
//! - **I2** `Activate{e}` only after `Ready{e}` was sent, or by the fallback;
//! - **I3** `Ready{e}` only after `Installed{fence}` for every `Install` of epoch `e`;
//! - **I4** a `Retire` never targets the slot that is current or pending for that identity;
//! - **I5** no output contains key bytes except `Key(Install | Activate | Retire)`;
//! - **I6** for a given `(group, epoch, identity)` the IKM is the same on every member.

mod buffer;
mod io;
mod mls;
mod recorder;
mod recover;
mod roster;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use openmls::prelude::MlsGroup;
use openmls_basic_credential::SignatureKeyPair;
use sew_proto::e2ee::{CallKeysInit, CallKeysMessage, KeyResyncReason, VerificationCode};

use crate::bridge::{KeyEvent, KeyRing};
use crate::code::SecurityCode;
use crate::config::{E2eeConfig, Params};
use crate::error::SessionError;
use crate::identity::{DeviceIdentity, LeafIdentity, VerifiedEntry};
use crate::ids::{
    Epoch, FenceId, JoinId, LkIdentity, Millis, RequestId, RoomName, SignId, Slot, identity_matches,
};
use crate::keypackage::{E2eeClient, LeafDraft, LeafSecret, PreparedKp};
use crate::keys::FrameIkm;
use crate::provider::CallProvider;
use crate::roster::{MediaLabel, RosterEntry};
use crate::trust::TrustAnchors;
use crate::wire::{ApiFailure, KeyReply, KeyRequest};

pub use buffer::{UnknownCallBuffer, call_id_of};
pub use io::{
    E2eeStatus, EngineAttack, Input, LeftReason, Output, SessionState, Transition, UndecryptReason,
};
#[cfg(feature = "testing")]
pub use mls::CraftCommit;

use recorder::RecorderState;
use recover::SignPurpose;
use roster::RosterWatch;

/// Counts and ids only, for the app's metrics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counters {
    /// Epochs activated.
    pub epochs_activated: u64,
    /// `Install` events emitted.
    pub installs: u64,
    /// `Retire` events emitted.
    pub retires: u64,
    /// Local activations by the fallback timer.
    pub activate_fallback: u64,
    /// Activations whose fence had not returned yet.
    pub activate_before_fence: u64,
    /// Re-adds by reason.
    pub recoveries: BTreeMap<&'static str, u64>,
    /// Group resets seen.
    pub resets: u64,
    /// Messages for another group or an old epoch, ignored.
    pub ignored_stale: u64,
    /// Installs into a slot whose previous key was active less than `retire_after` ago.
    pub slot_reused_early: u64,
    /// Unencrypted publications refused by the bridge.
    pub unencrypted_track_refused: u64,
    /// Sender self-checks that ended stuck.
    pub sender_index_stuck: u64,
    /// Recorder releases sent.
    pub releases_sent: u64,
    /// `SdkMissingKey` inputs.
    pub sdk_missing_key: u64,
    /// `commit` `409`/`422` answers (superseded, stale, not committer, no transition, invalid).
    pub commit_refused: u64,
    /// `report`s sent.
    pub reports_sent: u64,
    /// `report`s suppressed by the client limits.
    pub reports_limited: u64,
    /// Last commit processing time (ms of the caller's clock; 0 in tests with a fixed clock).
    pub last_commit_ms: u64,
}

/// The epoch whose keys senders use now.
#[derive(Debug)]
struct ActiveEpoch {
    group_id: Vec<u8>,
    epoch: Epoch,
    slot: Slot,
    leaf: u32,
    ikm: FrameIkm,
    code: Option<SecurityCode>,
    since: Millis,
}

/// The installed, not yet active epoch.
#[derive(Debug)]
struct PendingEpoch {
    group_id: Vec<u8>,
    epoch: Epoch,
    slot: Slot,
    leaf: u32,
    ikm: FrameIkm,
    code: Option<SecurityCode>,
    fence: FenceId,
    /// Identities that left in this transition (retired and forgotten after the grace).
    departed: BTreeSet<LkIdentity>,
}

/// A retirement due at `at`.
#[derive(Debug, Clone)]
struct RetireJob {
    at: Millis,
    identity: LkIdentity,
    slot: Slot,
    forget: bool,
}

/// What a request in flight was for.
#[derive(Debug, Clone)]
enum Purpose {
    /// `state` after create (with tree).
    StateCreate,
    /// `state` after a re-identify (`with_tree` once `send_tree` asked).
    StateRebind,
    /// `state` for the missing-key check.
    StateMissingKey,
    /// `state` before a `peer_undecryptable` report about this identity.
    StatePeer(LkIdentity),
    /// `state` after a `resync{stale_slot}` whose slot disagreed.
    StateSlot,
    /// `commit`.
    Commit,
    /// `ready`.
    Ready,
    /// `invalid`.
    Invalid,
    /// `key_package` (a reset).
    ResetPackage,
    /// `release` for `(recording, group, epoch)`.
    Release(u64, Vec<u8>, Epoch),
    /// `rekey`.
    Rekey,
    /// `report`.
    Report,
}

/// The sender self-check in progress.
#[derive(Debug, Clone, Copy)]
struct SenderCheck {
    expected: Slot,
    attempts: u32,
}

/// A group reset in progress.
#[derive(Debug, Clone)]
struct ResetState {
    new_group_id: Vec<u8>,
    slot_base: u8,
}

/// Recovery in progress (re-add or reset), for the watchdog.
#[derive(Debug, Clone)]
struct Recovery {
    reason: &'static str,
    since: Millis,
    tries: u32,
}

/// One device's key state for one call.
pub struct CallSession {
    cfg: E2eeConfig,
    params: Params,
    trust: TrustAnchors,
    device: DeviceIdentity,
    call_id: RoomName,
    join_id: JoinId,
    me: LkIdentity,
    scope: sew_proto::e2ee::CallScope,
    gateway_session: String,
    provider: CallProvider,
    group: Option<MlsGroup>,
    signer: Option<SignatureKeyPair>,
    leaves: BTreeMap<[u8; 32], LeafSecret>,
    own_commit: Option<Vec<u8>>,
    init_group: Vec<u8>,
    known_group: Option<(Vec<u8>, Epoch)>,
    slot_base: u8,
    bindings: BTreeMap<u32, VerifiedEntry>,
    add_bindings: BTreeMap<Vec<u8>, VerifiedEntry>,
    leaf_ids: BTreeMap<u32, LeafIdentity>,
    last_proposals: Option<(Epoch, Vec<u8>)>,
    active: Option<ActiveEpoch>,
    pending: Option<PendingEpoch>,
    ring: KeyRing,
    retires: Vec<RetireJob>,
    slot_retired_at: BTreeMap<Slot, Millis>,
    latest_leaf: Option<[u8; 32]>,
    leaf_keys: BTreeMap<u32, Vec<u8>>,
    rebind_on_next_invalid: bool,
    state: SessionState,
    status: Option<E2eeStatus>,
    reset: Option<ResetState>,
    recovery: Option<Recovery>,
    sign_waits: BTreeMap<SignId, (LeafDraft, SignPurpose)>,
    next_req: u64,
    next_fence: u64,
    next_sign: u64,
    inflight: BTreeMap<RequestId, Purpose>,
    retry_state_at: Option<Millis>,
    missing_check_at: Option<Millis>,
    missing: BTreeMap<LkIdentity, Millis>,
    missing_checked_epoch: Option<Epoch>,
    undecryptable: BTreeMap<LkIdentity, (Millis, u32, UndecryptReason, Option<Slot>)>,
    peer_reported: BTreeSet<(LkIdentity, Epoch)>,
    sender_check: Option<SenderCheck>,
    rec: RecorderState,
    watch: RosterWatch,
    roster: Vec<RosterEntry>,
    outputs: VecDeque<Output>,
    counters: Counters,
    verification_code: VerificationCode,
    #[cfg(feature = "testing")]
    zeroized_probe: bool,
}

impl fmt::Debug for CallSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallSession")
            .field("call_id", &self.call_id)
            .field("join_id", &self.join_id)
            .field("state", &self.state)
            .field("active_epoch", &self.active.as_ref().map(|a| a.epoch))
            .finish_non_exhaustive()
    }
}

impl CallSession {
    // ------------------------------------------------------------------ construction

    pub(crate) fn start(
        client: &E2eeClient,
        kp: PreparedKp,
        init: CallKeysInit,
        gateway_session: &str,
        now: Millis,
    ) -> Result<CallSession, SessionError> {
        let params = Params::from_wire(&init.params)?;
        if !identity_matches(&init.join_id, &init.lk_identity) {
            return Err(SessionError::IdentityMismatch);
        }
        if init.external_sender.as_bytes() != client.trust.external_sender() {
            return Err(SessionError::ExternalSenderMismatch);
        }
        if init.scope != kp.scope {
            return Err(SessionError::Refused("scope"));
        }
        let PreparedKp { provider, leaf, .. } = kp;
        let first_leaf = leaf.kp_ref;
        let mut leaves = BTreeMap::new();
        leaves.insert(leaf.kp_ref, leaf);
        let verification_code = client.trust.verification_code();
        let mut s = CallSession {
            cfg: client.cfg.clone(),
            params,
            trust: client.trust.clone(),
            device: client.identity.clone(),
            call_id: init.call_id.clone(),
            join_id: init.join_id.clone(),
            me: init.lk_identity.clone(),
            scope: init.scope,
            gateway_session: gateway_session.to_owned(),
            provider,
            group: None,
            signer: None,
            leaves,
            own_commit: None,
            init_group: init.group_id.as_bytes().to_vec(),
            known_group: Some((init.group_id.as_bytes().to_vec(), init.epoch)),
            slot_base: init.slot_base,
            bindings: BTreeMap::new(),
            add_bindings: BTreeMap::new(),
            leaf_ids: BTreeMap::new(),
            last_proposals: None,
            active: None,
            pending: None,
            ring: KeyRing::new(),
            retires: Vec::new(),
            slot_retired_at: BTreeMap::new(),
            latest_leaf: None,
            leaf_keys: BTreeMap::new(),
            rebind_on_next_invalid: false,
            state: SessionState::Joining,
            status: None,
            reset: None,
            recovery: None,
            sign_waits: BTreeMap::new(),
            next_req: 1,
            next_fence: 1,
            next_sign: 1,
            inflight: BTreeMap::new(),
            retry_state_at: None,
            missing_check_at: None,
            missing: BTreeMap::new(),
            missing_checked_epoch: None,
            undecryptable: BTreeMap::new(),
            peer_reported: BTreeSet::new(),
            sender_check: None,
            rec: RecorderState::default(),
            watch: RosterWatch::default(),
            roster: Vec::new(),
            outputs: VecDeque::new(),
            counters: Counters::default(),
            verification_code,
            #[cfg(feature = "testing")]
            zeroized_probe: false,
        };
        s.latest_leaf = Some(first_leaf);
        s.set_status(E2eeStatus::Joining { since: now });
        if init.create {
            s.create_group(now, &init)?;
        } else {
            s.set_status(E2eeStatus::Securing {
                since: now,
                show_after_ms: s.params.securing_indicator_after_ms,
            });
            // A Welcome that never comes (dropped from the unknown-call buffer, or sent to a
            // dead socket) heals through a re-add after the watchdog period.
            s.recovery = Some(Recovery {
                reason: "joining",
                since: now,
                tries: 0,
            });
        }
        Ok(s)
    }

    // ------------------------------------------------------------------ the API

    /// Applies one input. Never panics. A returned `Err` has already queued any recovery
    /// output; state changes only on success of the MLS step it guards.
    pub fn handle(&mut self, now: Millis, input: Input) -> Result<(), SessionError> {
        if matches!(self.state, SessionState::Left { .. }) {
            return Err(SessionError::Left);
        }
        let r = match input {
            Input::Server(m) => self.on_server(now, m),
            Input::Reply(id, r) => self.on_reply(now, id, r),
            Input::Installed { fence } => self.on_installed(now, fence),
            Input::DeviceSignature { request, sig } => self.on_signature(now, request, sig),
            Input::GatewayResumed => Ok(()),
            Input::GatewayReidentified { gateway_session } => {
                self.on_reidentified(now, gateway_session)
            }
            Input::Ready(r) => self.on_ready(now, &r),
            Input::Recording {
                recording_id,
                my_consent,
            } => self.on_recording(now, recording_id, my_consent),
            Input::SdkMissingKey { identity, since } => self.on_missing_key(now, identity, since),
            Input::SdkUndecryptable {
                identity,
                since,
                failures,
                reason,
                missing_index,
            } => self.on_undecryptable(now, identity, since, failures, reason, missing_index),
            Input::SdkDecryptOk { identity } => {
                self.missing.remove(&identity);
                self.undecryptable.remove(&identity);
                Ok(())
            }
            Input::SenderSlotObserved { observed } => self.on_sender_slot(now, observed),
            Input::EngineAttack { kind, identity } => self.on_engine_attack(now, kind, identity),
            Input::MediaParticipants(ids) => self.on_media_participants(now, ids),
            Input::VoiceMembers(members) => self.on_voice_members(now, members),
        };
        self.after_input(now);
        r
    }

    /// Runs every timer due at `now`.
    pub fn handle_timeout(&mut self, now: Millis) {
        if matches!(self.state, SessionState::Left { .. }) {
            return;
        }
        self.run_timers(now);
        self.after_input(now);
    }

    /// The next time `handle_timeout` must run.
    pub fn poll_timeout(&self) -> Option<Millis> {
        if matches!(self.state, SessionState::Left { .. }) {
            return None;
        }
        let mut t: Option<Millis> = None;
        let mut take = |v: Option<Millis>| {
            if let Some(v) = v {
                t = Some(t.map_or(v, |c: Millis| c.min(v)));
            }
        };
        take(self.retires.iter().map(|r| r.at).min());
        if let SessionState::Active {
            transition: Transition::Ready { since, .. },
            ..
        } = &self.state
        {
            take(Some(since + self.params.client_activate_fallback_ms));
        }
        if let Some(r) = &self.recovery {
            take(Some(r.since + self.cfg.recovery_watchdog_ms));
        }
        take(self.retry_state_at);
        take(self.missing_check_at);
        take(self.watch.next_due(self.cfg.roster_grace_ms));
        take(self.undecryptable_due());
        t
    }

    /// The next output, in order.
    pub fn poll_output(&mut self) -> Option<Output> {
        self.outputs.pop_front()
    }

    /// Teardown: `Key(Clear)`, every secret zeroized, state `Left{Left}`. There is no key
    /// message; the voice leave has the server remove the leaf.
    pub fn leave(&mut self, now: Millis) {
        if matches!(self.state, SessionState::Left { .. }) {
            return;
        }
        self.teardown(now, LeftReason::Left);
    }

    /// Entries in this call's MLS store (no contents; tests and telemetry: a long call's
    /// store must not grow with its epochs).
    pub fn stored_entries(&self) -> usize {
        self.provider.stored_entries()
    }

    /// Sizes of what the session holds (telemetry, no contents: a long soak tells a
    /// growing structure from allocator noise): `(name, entries)` pairs, the MLS store's bytes
    /// as `store_bytes`.
    pub fn footprint(&self) -> Vec<(&'static str, usize)> {
        vec![
            ("store_entries", self.provider.stored_entries()),
            ("store_bytes", self.provider.stored_bytes()),
            ("leaves", self.leaves.len()),
            ("bindings", self.bindings.len()),
            ("add_bindings", self.add_bindings.len()),
            ("leaf_ids", self.leaf_ids.len()),
            ("leaf_keys", self.leaf_keys.len()),
            ("retires", self.retires.len()),
            ("slot_retired_at", self.slot_retired_at.len()),
            ("sign_waits", self.sign_waits.len()),
            ("inflight", self.inflight.len()),
            ("missing", self.missing.len()),
            ("undecryptable", self.undecryptable.len()),
            ("peer_reported", self.peer_reported.len()),
            ("roster", self.roster.len()),
            ("outputs", self.outputs.len()),
        ]
    }

    /// The MLS store's entries by storage label (telemetry).
    pub fn stored_labels(&self) -> BTreeMap<String, usize> {
        self.provider.stored_labels()
    }

    /// The per-call state.
    pub fn state(&self) -> SessionState {
        self.state.clone()
    }

    /// The active epoch.
    pub fn active_epoch(&self) -> Option<Epoch> {
        self.active.as_ref().map(|a| a.epoch)
    }

    /// The local send slot.
    pub fn local_slot(&self) -> Option<Slot> {
        self.active.as_ref().map(|a| a.slot)
    }

    /// The MLS epoch of the group held now.
    pub fn mls_epoch(&self) -> Option<Epoch> {
        self.group.as_ref().map(|g| g.epoch().as_u64())
    }

    /// The slot base of the group held now (`slot(e) = (slot_base + e) mod 64`).
    pub fn slot_base(&self) -> u8 {
        self.slot_base
    }

    /// The group held now.
    pub fn group_id(&self) -> Option<Vec<u8>> {
        self.group
            .as_ref()
            .map(|g| g.group_id().as_slice().to_vec())
    }

    /// The own leaf index in the group held now.
    pub fn own_leaf(&self) -> Option<u32> {
        self.group.as_ref().map(|g| g.own_leaf_index().u32())
    }

    /// The roster, from the verified MLS tree only.
    pub fn roster(&self) -> &[RosterEntry] {
        &self.roster
    }

    /// Whether `id` is in the key group now (a media participant with no leaf gets no key).
    pub fn is_keyed(&self, id: &LkIdentity) -> bool {
        self.bindings.values().any(|e| &e.lk_identity == id)
    }

    /// How the app labels a media participant.
    pub fn media_label(&self, id: &LkIdentity) -> MediaLabel {
        if let Some((leaf, _)) = self.bindings.iter().find(|(_, e)| &e.lk_identity == id) {
            return MediaLabel::Member(*leaf);
        }
        if self.rec.identity() == Some(id) {
            return MediaLabel::Recorder;
        }
        MediaLabel::NotInEncryptedSession
    }

    /// The 30-digit code of the active epoch; `None` when the policy hides it.
    pub fn security_code(&self) -> Option<SecurityCode> {
        if self.verification_code == VerificationCode::Hidden {
            return None;
        }
        self.active.as_ref().and_then(|a| a.code.clone())
    }

    /// Ids and counts only.
    pub fn telemetry(&self) -> &Counters {
        &self.counters
    }

    /// The last status emitted.
    pub fn status(&self) -> Option<&E2eeStatus> {
        self.status.as_ref()
    }

    /// The local engine identity.
    pub fn lk_identity(&self) -> &LkIdentity {
        &self.me
    }

    /// The Sew-minted join id.
    pub fn join_id(&self) -> &JoinId {
        &self.join_id
    }

    /// The call.
    pub fn call_id(&self) -> &RoomName {
        &self.call_id
    }

    /// The shadow of the keys this session emitted (what the bridge should hold).
    pub fn key_ring(&self) -> &KeyRing {
        &self.ring
    }

    /// The bound engine identity of a leaf.
    pub fn identity_of_leaf(&self, leaf: u32) -> Option<&LkIdentity> {
        self.bindings.get(&leaf).map(|e| &e.lk_identity)
    }

    /// Test hook: whether teardown zeroized the MLS state (a teardown test).
    #[cfg(feature = "testing")]
    pub fn zeroized_for_test(&self) -> bool {
        self.zeroized_probe
    }

    /// Test hook: the epoch authenticator of the held group's epoch (secret-adjacent; tests
    /// search logs for it).
    #[cfg(feature = "testing")]
    pub fn epoch_authenticator_for_test(&self) -> Option<Vec<u8>> {
        self.group
            .as_ref()
            .map(|g| g.epoch_authenticator().as_slice().to_vec())
    }

    /// Test hook: the identities with a live key in the ring.
    #[cfg(feature = "testing")]
    pub fn ring_identities_for_test(&self) -> Vec<LkIdentity> {
        self.ring.identities().cloned().collect()
    }

    // ------------------------------------------------------------------ plumbing

    fn after_input(&mut self, now: Millis) {
        self.watch_roster(now);
    }

    fn set_status(&mut self, s: E2eeStatus) {
        if self.status.as_ref() != Some(&s) {
            self.status = Some(s.clone());
            self.outputs.push_back(Output::Status(s));
        }
    }

    fn emit_key(&mut self, ev: KeyEvent) {
        match &ev {
            KeyEvent::Install { .. } => self.counters.installs += 1,
            KeyEvent::Retire { .. } => self.counters.retires += 1,
            _ => {}
        }
        self.ring.apply(&ev);
        self.outputs.push_back(Output::Key(ev));
    }

    fn next_fence(&mut self) -> FenceId {
        let f = FenceId(self.next_fence);
        self.next_fence += 1;
        f
    }

    fn send(&mut self, purpose: Purpose, req: KeyRequest) -> RequestId {
        let id = RequestId(self.next_req);
        self.next_req += 1;
        self.inflight.insert(id, purpose);
        self.outputs.push_back(Output::Send(id, req));
        id
    }

    fn sign(&mut self, draft: LeafDraft, purpose: SignPurpose) {
        let id = SignId(self.next_sign);
        self.next_sign += 1;
        let message = draft.message().to_vec();
        self.sign_waits.insert(id, (draft, purpose));
        self.outputs.push_back(Output::NeedDeviceSignature {
            request: id,
            message,
        });
    }

    fn teardown(&mut self, now: Millis, reason: LeftReason) {
        self.emit_key(KeyEvent::Clear);
        self.group = None;
        self.signer = None;
        self.leaves.clear();
        self.sign_waits.clear();
        self.active = None;
        self.pending = None;
        self.retires.clear();
        self.own_commit = None;
        self.provider.wipe();
        #[cfg(feature = "testing")]
        {
            self.zeroized_probe = self.provider.stored_entries() == 0;
        }
        self.state = SessionState::Left { reason };
        self.set_status(E2eeStatus::Left { reason });
        let _ = now;
    }

    // ------------------------------------------------------------------ dispatch

    fn on_server(&mut self, now: Millis, m: CallKeysMessage) -> Result<(), SessionError> {
        match m {
            CallKeysMessage::Init(init) => {
                if init.call_id != self.call_id || init.join_id != self.join_id {
                    self.counters.ignored_stale += 1;
                    return Ok(());
                }
                self.on_init_dispatch(now, &init)
            }
            CallKeysMessage::Proposals {
                call_id,
                group_id,
                epoch,
                proposals,
                committer,
                reason: _,
                bindings,
            } => {
                if call_id != self.call_id {
                    self.counters.ignored_stale += 1;
                    return Ok(());
                }
                self.on_proposals(
                    now,
                    group_id.as_bytes(),
                    epoch,
                    &proposals,
                    committer,
                    &bindings,
                )
            }
            CallKeysMessage::CommitRequest {
                call_id,
                group_id,
                epoch,
                committer,
                attempt: _,
            } => {
                if call_id != self.call_id {
                    self.counters.ignored_stale += 1;
                    return Ok(());
                }
                self.on_commit_request(now, group_id.as_bytes(), epoch, committer)
            }
            CallKeysMessage::Commit {
                call_id,
                group_id,
                epoch_from,
                commit,
                slot,
                bindings,
            } => {
                if call_id != self.call_id {
                    self.counters.ignored_stale += 1;
                    return Ok(());
                }
                self.on_commit(
                    now,
                    group_id.as_bytes(),
                    epoch_from,
                    commit.as_bytes(),
                    slot,
                    &bindings,
                )
            }
            CallKeysMessage::Welcome {
                call_id,
                group_id,
                join_id,
                epoch,
                welcome,
                ratchet_tree,
                slot_base,
                slot,
                bindings,
            } => {
                if call_id != self.call_id || join_id != self.join_id {
                    self.counters.ignored_stale += 1;
                    return Ok(());
                }
                self.on_welcome(
                    now,
                    group_id.as_bytes(),
                    epoch,
                    welcome.as_bytes(),
                    ratchet_tree.as_bytes(),
                    slot_base,
                    slot,
                    &bindings,
                )
            }
            CallKeysMessage::Activate {
                call_id,
                group_id,
                epoch,
                slot,
            } => {
                if call_id != self.call_id {
                    self.counters.ignored_stale += 1;
                    return Ok(());
                }
                self.on_activate(now, group_id.as_bytes(), epoch, slot)
            }
            CallKeysMessage::Reset {
                call_id,
                new_group_id,
                reason: _,
                slot_base,
                collect_ms: _,
            } => {
                if call_id != self.call_id {
                    self.counters.ignored_stale += 1;
                    return Ok(());
                }
                self.on_reset(now, new_group_id.as_bytes(), slot_base)
            }
            CallKeysMessage::Recorder {
                call_id,
                recording_id,
                recorder_cert,
                hpke_pub,
                lk_identity,
                removed,
            } => {
                if call_id != self.call_id {
                    self.counters.ignored_stale += 1;
                    return Ok(());
                }
                self.on_recorder(
                    now,
                    recording_id,
                    recorder_cert.as_ref().map(|b| b.as_bytes()),
                    hpke_pub.as_ref().map(|b| b.as_bytes()),
                    lk_identity,
                    removed,
                );
                Ok(())
            }
            CallKeysMessage::Revoked { serials } => {
                let s: Vec<u64> = serials.iter().map(|i| i.get()).collect();
                self.trust.revoke(&s);
                Ok(())
            }
            CallKeysMessage::Removed {
                call_id,
                group_id: _,
                join_id,
                reason,
            } => {
                if call_id != self.call_id || join_id != self.join_id {
                    self.counters.ignored_stale += 1;
                    return Ok(());
                }
                self.teardown(now, LeftReason::Removed(reason));
                Ok(())
            }
            CallKeysMessage::Resync {
                call_id,
                join_id,
                reason,
                expected_slot,
            } => {
                if call_id != self.call_id || join_id != self.join_id {
                    self.counters.ignored_stale += 1;
                    return Ok(());
                }
                match reason {
                    KeyResyncReason::StaleSlot => self.on_stale_slot(now, expected_slot),
                    KeyResyncReason::Behind
                    | KeyResyncReason::GroupMismatch
                    | KeyResyncReason::Lagging => {
                        self.start_readd(now, sew_proto::e2ee::InvalidReason::Behind);
                        Ok(())
                    }
                    KeyResyncReason::Unknown => Ok(()),
                }
            }
            CallKeysMessage::Unknown => Ok(()),
        }
    }

    fn on_reply(
        &mut self,
        now: Millis,
        id: RequestId,
        r: Result<KeyReply, ApiFailure>,
    ) -> Result<(), SessionError> {
        let Some(purpose) = self.inflight.remove(&id) else {
            return Ok(());
        };
        match purpose {
            Purpose::StateCreate
            | Purpose::StateRebind
            | Purpose::StateMissingKey
            | Purpose::StatePeer(_)
            | Purpose::StateSlot => self.on_state_reply(now, purpose, r),
            Purpose::Commit => self.on_commit_reply(now, r),
            Purpose::Ready => {
                if let Err(f) = r
                    && f.status == 422
                    && f.is(sew_proto::e2ee::reasons::E2EE_EPOCH_AHEAD)
                {
                    self.start_readd(now, sew_proto::e2ee::InvalidReason::StateLost);
                }
                Ok(())
            }
            Purpose::ResetPackage => {
                if let Err(f) = r
                    && f.status == 409
                    && f.is(sew_proto::e2ee::reasons::E2EE_NO_RESET)
                {
                    self.start_readd(now, sew_proto::e2ee::InvalidReason::Behind);
                }
                Ok(())
            }
            Purpose::Release(rec, gid, epoch) => {
                self.on_release_reply(rec, gid, epoch, r);
                Ok(())
            }
            Purpose::Invalid | Purpose::Rekey | Purpose::Report => Ok(()),
        }
    }

    fn on_installed(&mut self, now: Millis, fence: FenceId) -> Result<(), SessionError> {
        let Some(p) = &self.pending else {
            return Ok(());
        };
        if p.fence != fence {
            return Ok(());
        }
        let (gid, epoch) = (p.group_id.clone(), p.epoch);
        if let SessionState::Active {
            transition: Transition::Installed { .. },
            ..
        } = self.state
        {
            self.send(
                Purpose::Ready,
                KeyRequest::Ready(
                    self.call_id.clone(),
                    sew_proto::e2ee::KeyReadyRequest {
                        join_id: self.join_id.clone(),
                        group_id: sew_proto::e2ee::B64::new(gid),
                        epoch,
                    },
                ),
            );
            self.state = SessionState::Active {
                epoch,
                transition: Transition::Ready { epoch, since: now },
            };
        }
        Ok(())
    }

    fn on_ready(
        &mut self,
        now: Millis,
        r: &sew_proto::e2ee::ReadyCallKeys,
    ) -> Result<(), SessionError> {
        match self.trust.refresh(r) {
            Ok(()) => {
                self.verification_code = r.verification_code;
                Ok(())
            }
            Err(crate::error::TrustError::ServerIdentityChanged { what }) => {
                let kind = if what == "ca" {
                    sew_proto::e2ee::ReportKind::CaChanged
                } else {
                    sew_proto::e2ee::ReportKind::ExternalSenderMismatch
                };
                self.report(now, kind, None, None);
                self.set_status(E2eeStatus::ServerIdentityChanged);
                Err(SessionError::Refused("server identity changed"))
            }
            Err(_) => Err(SessionError::Refused("ready")),
        }
    }

    fn run_timers(&mut self, now: Millis) {
        // Retirements.
        let mut due: Vec<RetireJob> = Vec::new();
        self.retires.retain(|r| {
            if r.at <= now {
                due.push(r.clone());
                false
            } else {
                true
            }
        });
        due.sort_by_key(|r| r.at);
        for r in due {
            self.retire_now(&r);
        }
        // Activate fallback.
        if let SessionState::Active {
            transition: Transition::Ready { since, epoch },
            ..
        } = self.state
            && now >= since + self.params.client_activate_fallback_ms
        {
            if self.pending.as_ref().is_some_and(|p| p.epoch == epoch) {
                self.counters.activate_fallback += 1;
                self.activate(now);
            } else {
                self.state = SessionState::Active {
                    epoch,
                    transition: Transition::Idle,
                };
            }
        }
        // Recovery watchdog.
        self.watchdog(now);
        // `state` retry after `rebuilding`.
        if let Some(at) = self.retry_state_at
            && now >= at
        {
            self.retry_state_at = None;
            self.send_state(Purpose::StateRebind, false);
        }
        // Missing-key check.
        if let Some(at) = self.missing_check_at
            && now >= at
        {
            self.missing_check_at = None;
            self.check_missing(now);
        }
        self.check_undecryptable(now);
    }

    fn retire_now(&mut self, r: &RetireJob) {
        // I4: never the slot that is current or pending for that identity.
        let current = if r.identity == self.me {
            self.active.as_ref().map(|a| a.slot)
        } else {
            self.ring.current(&r.identity).map(|(s, _)| s)
        };
        let pending = self.pending.as_ref().map(|p| p.slot);
        if current != Some(r.slot) && pending != Some(r.slot) {
            let filler = FrameIkm::random_os();
            self.emit_key(KeyEvent::Retire {
                identity: r.identity.clone(),
                slot: r.slot,
                filler,
            });
        }
        if r.forget && r.identity != self.me && !self.is_keyed(&r.identity) {
            self.emit_key(KeyEvent::Forget {
                identity: r.identity.clone(),
            });
        }
    }

    fn on_stale_slot(&mut self, now: Millis, expected: Option<u8>) -> Result<(), SessionError> {
        let Some(a) = &self.active else {
            return Ok(());
        };
        if expected != Some(a.slot.get()) {
            // A state disagreement: ask the DS (`state`), never a re-add here.
            self.send_state(Purpose::StateSlot, false);
            return Ok(());
        }
        let slot = a.slot;
        self.reapply_local();
        self.sender_check = Some(SenderCheck {
            expected: slot,
            attempts: 1,
        });
        self.outputs
            .push_back(Output::CheckSenderSlot { expected: slot });
        let _ = now;
        Ok(())
    }

    fn reapply_local(&mut self) {
        if let Some(a) = &self.active {
            let ev = KeyEvent::Activate {
                identity: self.me.clone(),
                slot: a.slot,
                ikm: a.ikm.duplicate(),
            };
            self.emit_key(ev);
        }
    }

    fn on_sender_slot(&mut self, now: Millis, observed: Slot) -> Result<(), SessionError> {
        let Some(c) = self.sender_check else {
            return Ok(());
        };
        if observed == c.expected {
            self.sender_check = None;
            return Ok(());
        }
        if c.attempts == 0 {
            self.reapply_local();
            self.sender_check = Some(SenderCheck {
                expected: c.expected,
                attempts: 1,
            });
            self.outputs.push_back(Output::CheckSenderSlot {
                expected: c.expected,
            });
            return Ok(());
        }
        self.sender_check = None;
        self.counters.sender_index_stuck += 1;
        let leaf = self.own_leaf();
        self.report(
            now,
            sew_proto::e2ee::ReportKind::SenderIndexStuck,
            leaf,
            Some(sew_proto::e2ee::ReportDetails {
                expected_slot: Some(c.expected.get()),
                observed_slot: Some(observed.get()),
                attempts: Some(2),
                ..Default::default()
            }),
        );
        self.set_status(E2eeStatus::Failed {
            reason: "sender_index_stuck",
        });
        Ok(())
    }

    fn on_engine_attack(
        &mut self,
        now: Millis,
        kind: EngineAttack,
        _identity: Option<LkIdentity>,
    ) -> Result<(), SessionError> {
        let k = match kind {
            EngineAttack::UnencryptedTrack => {
                self.counters.unencrypted_track_refused += 1;
                sew_proto::e2ee::ReportKind::EngineUnencryptedTrack
            }
            EngineAttack::SifTrailer => sew_proto::e2ee::ReportKind::EngineSifTrailer,
            EngineAttack::EncryptionDowngrade => {
                sew_proto::e2ee::ReportKind::EngineEncryptionDowngrade
            }
        };
        self.report(now, k, None, None);
        Ok(())
    }
}
