//! `KeyRing`: a shadow of what the SDK holds.
//!
//! Per remote identity it keeps at most three live keys: `previous` (within the grace),
//! `current` (the slot every sender uses now) and `pending` (installed, not active yet), plus the
//! local identity's send slot and key. It is the pull API for Sew's test client key source
//! (`current`, `get`) and for the JS provider overrides (`getKeys()` returns live keys only,
//! `getLatestManuallySetKeyIndex()` returns [`KeyRing::local_slot`]).
//!
//! Roles follow the local activation: every sender of one call switches to the same slot at
//! the same epoch, so when the local identity activates slot `s`, every remote key at `s`
//! becomes `current` and the old `current` becomes `previous`.

use std::collections::BTreeMap;

use super::KeyEvent;
use crate::ids::{LkIdentity, Slot};
use crate::keys::FrameIkm;

/// The role of one installed key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyRole {
    /// Superseded, kept until its retirement (the grace window).
    Previous,
    /// The slot senders use now.
    Current,
    /// Installed for the next epoch, not active yet.
    Pending,
}

#[derive(Debug, Default)]
struct Remote {
    keys: BTreeMap<Slot, (FrameIkm, KeyRole)>,
}

/// The shadow of an SDK's key state.
#[derive(Debug, Default)]
pub struct KeyRing {
    remotes: BTreeMap<LkIdentity, Remote>,
    local: Option<(LkIdentity, Slot, FrameIkm)>,
    local_previous: Option<Slot>,
    reconnect_window: bool,
}

impl KeyRing {
    /// An empty ring.
    pub fn new() -> KeyRing {
        KeyRing::default()
    }

    /// Applies one event (the same events the bridge gets, in order).
    pub fn apply(&mut self, ev: &KeyEvent) {
        match ev {
            KeyEvent::Install {
                identity,
                slot,
                ikm,
            } => {
                let current = self.local.as_ref().map(|(_, s, _)| *s);
                let role = if Some(*slot) == current {
                    KeyRole::Current
                } else {
                    KeyRole::Pending
                };
                let r = self.remotes.entry(identity.clone()).or_default();
                r.keys.insert(*slot, (ikm.duplicate(), role));
            }
            KeyEvent::Activate {
                identity,
                slot,
                ikm,
            } => {
                let old = self.local.as_ref().map(|(_, s, _)| *s);
                if old != Some(*slot) {
                    self.local_previous = old;
                }
                self.local = Some((identity.clone(), *slot, ikm.duplicate()));
                for r in self.remotes.values_mut() {
                    for (s, (_, role)) in r.keys.iter_mut() {
                        *role = if *s == *slot {
                            KeyRole::Current
                        } else if *role == KeyRole::Current {
                            KeyRole::Previous
                        } else {
                            *role
                        };
                    }
                }
            }
            KeyEvent::Retire { identity, slot, .. } => {
                if self.local.as_ref().is_some_and(|(id, _, _)| id == identity) {
                    if self.local_previous == Some(*slot) {
                        self.local_previous = None;
                    }
                    return;
                }
                if let Some(r) = self.remotes.get_mut(identity) {
                    r.keys.remove(slot);
                }
            }
            KeyEvent::Forget { identity } => {
                self.remotes.remove(identity);
            }
            KeyEvent::Fence { .. } => {}
            KeyEvent::Clear => {
                self.remotes.clear();
            }
        }
    }

    /// The local send slot (`getLatestManuallySetKeyIndex()`).
    pub fn local_slot(&self) -> Option<Slot> {
        self.local.as_ref().map(|(_, s, _)| *s)
    }

    /// The local identity, once activated.
    pub fn local_identity(&self) -> Option<&LkIdentity> {
        self.local.as_ref().map(|(id, _, _)| id)
    }

    /// The local identity's current key.
    pub fn local_key(&self) -> Option<(Slot, &FrameIkm)> {
        self.local.as_ref().map(|(_, s, k)| (*s, k))
    }

    /// The slot and key `id` sends on now (the local identity included).
    pub fn current(&self, id: &LkIdentity) -> Option<(Slot, &FrameIkm)> {
        if let Some((lid, s, k)) = &self.local
            && lid == id
        {
            return Some((*s, k));
        }
        self.remotes.get(id).and_then(|r| {
            r.keys
                .iter()
                .find(|(_, (_, role))| *role == KeyRole::Current)
                .map(|(s, (k, _))| (*s, k))
        })
    }

    /// The key of `id` at `slot`, if live.
    pub fn get(&self, id: &LkIdentity, slot: Slot) -> Option<&FrameIkm> {
        if let Some((lid, s, k)) = &self.local
            && lid == id
            && *s == slot
        {
            return Some(k);
        }
        self.remotes
            .get(id)
            .and_then(|r| r.keys.get(&slot))
            .map(|(k, _)| k)
    }

    /// The role of `id`'s key at `slot`.
    pub fn role(&self, id: &LkIdentity, slot: Slot) -> Option<KeyRole> {
        self.remotes
            .get(id)
            .and_then(|r| r.keys.get(&slot))
            .map(|(_, role)| *role)
    }

    /// Every live key: remote keys of every role, then the local current key.
    pub fn live(&self) -> impl Iterator<Item = (&LkIdentity, Slot, &FrameIkm)> {
        self.remotes
            .iter()
            .flat_map(|(id, r)| r.keys.iter().map(move |(s, (k, _))| (id, *s, k)))
            .chain(self.local.iter().map(|(id, s, k)| (id, *s, k)))
    }

    /// What `getKeys()` returns: the current and pending keys of every
    /// live remote identity and the local current key; `previous` keys too only inside a
    /// reconnect window ([`KeyRing::set_reconnect_window`]).
    pub fn repost_keys(&self) -> Vec<(&LkIdentity, Slot, &FrameIkm)> {
        let mut out: Vec<(&LkIdentity, Slot, &FrameIkm)> = self
            .remotes
            .iter()
            .flat_map(|(id, r)| {
                r.keys
                    .iter()
                    .filter(|(_, (_, role))| *role != KeyRole::Previous || self.reconnect_window)
                    .map(move |(s, (k, _))| (id, *s, k))
            })
            .collect();
        if let Some((id, s, k)) = &self.local {
            out.push((id, *s, k));
        }
        out
    }

    /// Opens or closes the reconnect window (`Reconnecting` → `SignalConnected`, JS bridge).
    pub fn set_reconnect_window(&mut self, open: bool) {
        self.reconnect_window = open;
    }

    /// Remote identities with at least one key.
    pub fn identities(&self) -> impl Iterator<Item = &LkIdentity> {
        self.remotes.keys()
    }

    /// Whether `id` holds any key (local or remote).
    pub fn holds(&self, id: &LkIdentity) -> bool {
        self.remotes.get(id).is_some_and(|r| !r.keys.is_empty())
            || self.local.as_ref().is_some_and(|(lid, _, _)| lid == id)
    }

    /// Live remote keys (for the `getKeys()` budget: ≤ 3 per live identity + 1).
    pub fn remote_key_count(&self) -> usize {
        self.remotes.values().map(|r| r.keys.len()).sum()
    }
}
