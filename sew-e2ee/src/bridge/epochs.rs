//! The per-epoch view of the key contract that Sew's media test harness clients consume:
//! `MlsKeySource`'s converter.
//!
//! The harness's events are per epoch: `Install(KeyEpoch{epoch, slot, keys})` with every **remote**
//! sender's key (never the local identity's), `Activate{epoch, local_ikm}` carrying the local
//! key, `Retire{epoch, slot}` for every remote sender, `Reset{slot_base}` at a new group, and the
//! client's `ready(epoch)` after the install. [`EpochAdapter`] converts the session's
//! per-identity [`KeyEvent`]s into these. The `Send + Sync` wrapper and the harness's trait impl
//! live with the harness, which depends on this crate.

use std::collections::{BTreeMap, VecDeque};

use super::KeyEvent;
use crate::ids::{Epoch, LkIdentity, Slot};
use crate::keys::FrameIkm;

/// One per-epoch key step.
#[derive(Debug)]
pub enum EpochEvent {
    /// Every remote sender's key of `epoch`, at `slot`; answer `ready(epoch)` once installed.
    Install {
        /// The epoch.
        epoch: Epoch,
        /// `(slot_base + epoch) mod 64`.
        slot: Slot,
        /// Remote keys only.
        keys: Vec<(LkIdentity, FrameIkm)>,
    },
    /// The local key of `epoch`: set it and switch the sender, in one step.
    Activate {
        /// The epoch.
        epoch: Epoch,
        /// The local key.
        local_ikm: FrameIkm,
    },
    /// Overwrite `slot` (of `epoch`) of every remote sender.
    Retire {
        /// The epoch.
        epoch: Epoch,
        /// Its slot.
        slot: Slot,
    },
    /// A new epoch series whose slots start at `slot_base`.
    Reset {
        /// The new base.
        slot_base: u8,
    },
}

/// Converts per-identity [`KeyEvent`]s into [`EpochEvent`]s.
#[derive(Debug)]
pub struct EpochAdapter {
    local: LkIdentity,
    slot_base: Option<u8>,
    installs: Vec<(LkIdentity, Slot, FrameIkm)>,
    install_epoch: Option<Epoch>,
    epoch_of_slot: BTreeMap<Slot, Epoch>,
    retired: BTreeMap<Slot, Epoch>,
    installed: Option<Epoch>,
    out: VecDeque<EpochEvent>,
}

impl EpochAdapter {
    /// An adapter for the client whose identity is `local`.
    pub fn new(local: LkIdentity) -> EpochAdapter {
        EpochAdapter {
            local,
            slot_base: None,
            installs: Vec::new(),
            install_epoch: None,
            epoch_of_slot: BTreeMap::new(),
            retired: BTreeMap::new(),
            installed: None,
            out: VecDeque::new(),
        }
    }

    /// One session event, tagged with the epoch it belongs to (the session's MLS epoch for an
    /// install or fence, its active epoch for an activate) and the group's slot base.
    pub fn push(&mut self, ev: &KeyEvent, epoch: Epoch, slot_base: u8) {
        if self.slot_base != Some(slot_base)
            && matches!(ev, KeyEvent::Install { .. } | KeyEvent::Activate { .. })
        {
            if self.slot_base.is_some() {
                self.out.push_back(EpochEvent::Reset { slot_base });
                self.installed = None;
            }
            self.slot_base = Some(slot_base);
        }
        match ev {
            KeyEvent::Install {
                identity,
                slot,
                ikm,
            } => {
                if identity == &self.local {
                    return;
                }
                self.install_epoch = Some(epoch);
                self.installs
                    .push((identity.clone(), *slot, ikm.duplicate()));
            }
            KeyEvent::Fence { .. } => {
                let installs = std::mem::take(&mut self.installs);
                let epoch = self.install_epoch.take().unwrap_or(epoch);
                let slot = installs
                    .first()
                    .map(|(_, s, _)| *s)
                    .unwrap_or_else(|| Slot::of(slot_base, epoch));
                self.epoch_of_slot.insert(slot, epoch);
                self.installed = Some(epoch);
                self.out.push_back(EpochEvent::Install {
                    epoch,
                    slot,
                    keys: installs.into_iter().map(|(i, _, k)| (i, k)).collect(),
                });
            }
            KeyEvent::Activate { slot, ikm, .. } => {
                if self.installed != Some(epoch) {
                    // A creator's epoch 0 has no remote sender: an empty install first.
                    self.installed = Some(epoch);
                    self.out.push_back(EpochEvent::Install {
                        epoch,
                        slot: *slot,
                        keys: Vec::new(),
                    });
                }
                self.epoch_of_slot.insert(*slot, epoch);
                self.out.push_back(EpochEvent::Activate {
                    epoch,
                    local_ikm: ikm.duplicate(),
                });
            }
            KeyEvent::Retire { identity, slot, .. } => {
                if identity == &self.local {
                    return;
                }
                let e = self.epoch_of_slot.get(slot).copied().unwrap_or(epoch);
                if self.retired.get(slot) != Some(&e) {
                    self.retired.insert(*slot, e);
                    self.out.push_back(EpochEvent::Retire {
                        epoch: e,
                        slot: *slot,
                    });
                }
            }
            KeyEvent::Forget { .. } | KeyEvent::Clear => {}
        }
    }

    /// The next converted event.
    pub fn poll(&mut self) -> Option<EpochEvent> {
        self.out.pop_front()
    }
}
