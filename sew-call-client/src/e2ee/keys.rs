//! The key state the media path reads ([`MediaKeys`]) and the `KeySink` that fills it.
//!
//! `sew-e2ee`'s `CallSession` hands keys out only as `KeyEvent`s. This bridge
//! applies them, in order, to per-identity 64-slot rings of derived frame keys:
//!
//! - `Install` puts a **remote** sender's key at a slot (never the local identity: I1);
//! - `Activate` sets the local identity's key and switches the sender to that slot;
//! - `Retire` overwrites a slot with a key derived from fresh random bytes;
//! - `Forget` drops an identity, `Clear` drops every remote key, `Fence` is in effect at once.
//!
//! The raw IKMs never stay here: each is derived into a `FrameCrypto::Key` at once and the
//! `FrameIkm` (zeroized on drop) is dropped. Nothing in this module logs a key or prints one:
//! every `Debug` is counts and identities only.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use sew_e2ee::{Caps, KeyEvent, KeySink, LkIdentity, SinkAck, SinkError, Slot};

use super::frame::FrameCrypto;
use crate::ENGINE_VERSION;

pub(crate) const RING: usize = sew_e2ee::ids::KEY_RING_SIZE as usize;

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// The caps this engine declares (only a bridge builds `Caps`): it moves its sender to any
/// slot (`sender_key_index`), SDK family `rust`.
pub fn engine_caps() -> Caps {
    sew_e2ee::__bridge::caps(true, "rust", ENGINE_VERSION, None)
}

/// What the media path reported for the key session to pass on (the `report` route and the receiver
/// check).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MediaReport {
    /// The engine misbehaved (an unencrypted track, a downgraded publication).
    Attack {
        /// What.
        kind: sew_e2ee::EngineAttack,
        /// The publisher involved, if any.
        identity: Option<LkIdentity>,
    },
    /// A subscribed sender's frames stopped decrypting.
    Undecryptable {
        /// The sender.
        identity: LkIdentity,
        /// Failing since (unix ms).
        since: u64,
        /// Failures counted.
        failures: u32,
        /// `invalid_key` or `missing_key`.
        reason: sew_e2ee::UndecryptReason,
        /// The key index the frames carried.
        slot: Option<Slot>,
    },
    /// The sender decrypts again.
    DecryptOk {
        /// The sender.
        identity: LkIdentity,
    },
}

/// Counters of the media path (no identities, no keys).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MediaCounters {
    /// Frames sealed and handed to the transport.
    pub sealed: u64,
    /// Outgoing frames dropped: no active local key.
    pub dropped_no_key: u64,
    /// Outgoing frames dropped: the publication is not (or no longer) verified encrypted.
    pub dropped_unverified_publication: u64,
    /// Outgoing frames dropped: the cipher failed (or the frame was empty).
    pub dropped_seal_failed: u64,
    /// Received frames opened and passed on.
    pub opened: u64,
    /// Received frames dropped: the sender is not a verified, subscribed, encrypted publisher.
    pub dropped_unverified_sender: u64,
    /// Received frames that did not decrypt (never played): a tag that did not verify (a wrong
    /// or retired key, a forged or corrupted frame), a malformed trailer, or a key index still
    /// empty once the sender's frames have failed for [`super::KEY_PENDING_GRACE_MS`].
    pub decrypt_failures: u64,
    /// Received frames dropped (never played) because the sender's key at the frame's index was
    /// not installed yet, within [`super::KEY_PENDING_GRACE_MS`] of the sender's first such
    /// frame: frames that crossed a join or a key switch (the old epoch's, which a member that
    /// just joined never holds, or the new one's a moment before its install). Not a decrypt
    /// failure.
    pub frames_before_key: u64,
}

/// How the local publication stands (the "checked path" of the publish check).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PublishState {
    /// No publication, or the server has not confirmed it encrypted yet: frames are dropped.
    Unverified,
    /// The server's `TrackPublishedResponse` confirmed `encryption = GCM`.
    Verified,
    /// The engine downgraded the publication: refused for the rest of the call.
    Refused,
}

pub(crate) struct Ring<K> {
    pub(crate) slots: Vec<Option<K>>,
}

impl<K> Ring<K> {
    fn new() -> Ring<K> {
        Ring {
            slots: (0..RING).map(|_| None).collect(),
        }
    }

    fn set(&mut self, slot: Slot, k: K) {
        if let Some(s) = self.slots.get_mut(usize::from(slot.get())) {
            *s = Some(k);
        }
    }

    pub(crate) fn get(&self, slot: u8) -> Option<&K> {
        self.slots.get(usize::from(slot)).and_then(Option::as_ref)
    }

    fn live(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }
}

pub(crate) struct Failing {
    pub(crate) since: u64,
    pub(crate) failures: u32,
    pub(crate) reason: sew_e2ee::UndecryptReason,
    pub(crate) slot: Option<Slot>,
    pub(crate) reported: bool,
}

pub(crate) struct State<K> {
    pub(crate) local: Option<(LkIdentity, Slot, K)>,
    pub(crate) remotes: BTreeMap<LkIdentity, Ring<K>>,
    /// Identities bound to a leaf of the verified MLS tree (the DS-signed leaf binding table).
    pub(crate) verified: BTreeSet<LkIdentity>,
    /// Publishers whose audio track passed the subscribe check.
    pub(crate) subscribed: BTreeSet<LkIdentity>,
    pub(crate) publish: PublishState,
    pub(crate) counters: MediaCounters,
    pub(crate) failing: BTreeMap<LkIdentity, Failing>,
    pub(crate) reports: VecDeque<MediaReport>,
    pub(crate) send_count: BTreeMap<u32, u32>,
}

/// The shared key state of one call: the `KeySink` the key session applies events to, and what
/// [`super::SendGate`], [`super::RecvGate`] and the guards read. Cheap to clone (one `Arc`).
pub struct MediaKeys<C: FrameCrypto> {
    pub(crate) crypto: Arc<C>,
    pub(crate) state: Arc<Mutex<State<C::Key>>>,
}

impl<C: FrameCrypto> Clone for MediaKeys<C> {
    fn clone(&self) -> Self {
        MediaKeys {
            crypto: self.crypto.clone(),
            state: self.state.clone(),
        }
    }
}

impl<C: FrameCrypto> fmt::Debug for MediaKeys<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = lock(&self.state);
        f.debug_struct("MediaKeys")
            .field("local_slot", &s.local.as_ref().map(|(_, slot, _)| *slot))
            .field("remotes", &s.remotes.len())
            .field("verified", &s.verified.len())
            .field("publish", &s.publish)
            .field("counters", &s.counters)
            .finish_non_exhaustive()
    }
}

impl<C: FrameCrypto> MediaKeys<C> {
    /// Empty state over `crypto` (sew-lk-compat's LkGcm in the engine).
    pub fn new(crypto: C) -> MediaKeys<C> {
        MediaKeys {
            crypto: Arc::new(crypto),
            state: Arc::new(Mutex::new(State {
                local: None,
                remotes: BTreeMap::new(),
                verified: BTreeSet::new(),
                subscribed: BTreeSet::new(),
                publish: PublishState::Unverified,
                counters: MediaCounters::default(),
                failing: BTreeMap::new(),
                reports: VecDeque::new(),
                send_count: BTreeMap::new(),
            })),
        }
    }

    /// The frame cipher.
    pub fn crypto(&self) -> &C {
        &self.crypto
    }

    /// The local send slot, once a key is active (`connected` needs it).
    pub fn local_slot(&self) -> Option<Slot> {
        lock(&self.state).local.as_ref().map(|(_, s, _)| *s)
    }

    /// The local engine identity, once activated.
    pub fn local_identity(&self) -> Option<LkIdentity> {
        lock(&self.state)
            .local
            .as_ref()
            .map(|(id, _, _)| id.clone())
    }

    /// Whether `id` holds a key at `slot` (tests, the roster report).
    pub fn holds(&self, id: &LkIdentity, slot: Slot) -> bool {
        let s = lock(&self.state);
        if let Some((lid, ls, _)) = &s.local
            && lid == id
        {
            return *ls == slot;
        }
        s.remotes
            .get(id)
            .is_some_and(|r| r.get(slot.get()).is_some())
    }

    /// Remote identities with at least one key.
    pub fn remote_identities(&self) -> Vec<LkIdentity> {
        lock(&self.state)
            .remotes
            .iter()
            .filter(|(_, r)| r.live() > 0)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Replaces the verified identity set (the key session, after every roster change).
    pub fn set_verified(&self, ids: BTreeSet<LkIdentity>) {
        let mut s = lock(&self.state);
        // A publisher that left the verified tree loses its subscription check at once.
        s.subscribed.retain(|id| ids.contains(id));
        s.verified = ids;
    }

    /// Whether `id` is bound to a leaf of the verified tree.
    pub fn is_verified(&self, id: &LkIdentity) -> bool {
        lock(&self.state).verified.contains(id)
    }

    /// The publication state.
    pub fn publish_state(&self) -> PublishState {
        lock(&self.state).publish
    }

    /// The counters.
    pub fn counters(&self) -> MediaCounters {
        lock(&self.state).counters
    }

    /// Takes what the media path reported since the last call.
    pub fn take_reports(&self) -> Vec<MediaReport> {
        lock(&self.state).reports.drain(..).collect()
    }

    pub(crate) fn report(&self, r: MediaReport) {
        let mut s = lock(&self.state);
        if s.reports.len() >= 256 {
            s.reports.pop_front();
        }
        s.reports.push_back(r);
    }
}

impl<C: FrameCrypto> KeySink for MediaKeys<C> {
    fn apply(&mut self, ev: KeyEvent) -> Result<SinkAck, SinkError> {
        let mut s = lock(&self.state);
        match ev {
            KeyEvent::Install {
                identity,
                slot,
                ikm,
            } => {
                if s.local.as_ref().is_some_and(|(id, _, _)| *id == identity) {
                    // I1: the local identity's key only ever comes through `Activate`.
                    return Err(SinkError::NotReady("install for the local identity"));
                }
                let k = self.crypto.derive(ikm.expose());
                s.remotes
                    .entry(identity)
                    .or_insert_with(Ring::new)
                    .set(slot, k);
            }
            KeyEvent::Activate {
                identity,
                slot,
                ikm,
            } => {
                let k = self.crypto.derive(ikm.expose());
                s.remotes.remove(&identity);
                s.local = Some((identity, slot, k));
            }
            KeyEvent::Retire {
                identity,
                slot,
                filler,
            } => {
                let is_local = s.local.as_ref().is_some_and(|(id, _, _)| *id == identity);
                if is_local {
                    // The local key is replaced only by the next `Activate`.
                    return Ok(SinkAck::Now);
                }
                // This bridge can delete a key outright (dropping it zeroizes), so the filler is
                // not needed: the slot is emptied and a frame on it is `missing_key`.
                drop(filler);
                if let Some(r) = s.remotes.get_mut(&identity)
                    && let Some(k) = r.slots.get_mut(usize::from(slot.get()))
                {
                    *k = None;
                }
            }
            KeyEvent::Forget { identity } => {
                s.remotes.remove(&identity);
                s.subscribed.remove(&identity);
                s.failing.remove(&identity);
            }
            KeyEvent::Fence { .. } => {}
            KeyEvent::Clear => {
                s.remotes.clear();
                s.subscribed.clear();
                s.failing.clear();
            }
        }
        Ok(SinkAck::Now)
    }

    fn caps(&self) -> Caps {
        engine_caps()
    }
}
