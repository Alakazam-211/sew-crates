//! `VirtualSdk`: LiveKit's key-ring semantics, exactly, for tests.
//!
//! Two flavours:
//! - [`Flavour::Js`] (`livekit-client` 2.22.3): installing the **local** identity's key at any
//!   index switches the sender at once; a local `Retire` is ignored by the bridge.
//! - [`Flavour::Rust`] (`livekit` 0.9.3 with Sew's patch to it, which remembers the send slot
//!   for a new local cryptor): `set_key` never switches the sender;
//!   `Activate` sets the key then `set_key_index` on every local cryptor; a new local cryptor
//!   starts at the remembered slot (patched) or at index 0 (unpatched, `patched = false`).
//!
//! Per identity it keeps a ring of 64 slots. A virtual sender emits frames tagged with its send
//! slot; a receiver decrypts a frame only if it holds the same key bytes for that identity at
//! that slot. `Sim` tests assert at the key level with it that every
//! member can decrypt every other member's frames.

use std::collections::BTreeMap;

use super::{Caps, KeyEvent, KeySink, SinkAck, SinkError};
use crate::ids::{FenceId, KEY_RING_SIZE, LkIdentity, Millis, Slot};
use crate::keys::FrameIkm;

/// Which SDK's semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flavour {
    /// `livekit-client` (a local install switches the sender).
    Js,
    /// The LiveKit Rust SDK (`set_key` never switches; `set_key_index` does).
    Rust,
}

/// A frame a virtual sender emitted.
#[derive(Debug)]
pub struct VirtualFrame {
    /// The sender.
    pub identity: LkIdentity,
    /// The key index in the frame trailer.
    pub slot: Slot,
    /// The key the sender used (compared in constant time by receivers).
    pub key: FrameIkm,
    /// When it was sent.
    pub at: Millis,
}

/// A model of one client's SDK key state.
#[derive(Debug)]
pub struct VirtualSdk {
    flavour: Flavour,
    local: LkIdentity,
    rings: BTreeMap<LkIdentity, Vec<Option<FrameIkm>>>,
    send_slot: Option<Slot>,
    remembered: Option<Slot>,
    patched: bool,
    fence_delay_ms: u64,
    fences: Vec<(Millis, FenceId)>,
    now: Millis,
    stuck: bool,
    forced: Option<Slot>,
    log: Vec<String>,
}

impl VirtualSdk {
    /// A virtual SDK for the client whose engine identity is `local`.
    pub fn new(flavour: Flavour, local: LkIdentity) -> VirtualSdk {
        VirtualSdk {
            flavour,
            local,
            rings: BTreeMap::new(),
            send_slot: None,
            remembered: None,
            patched: true,
            fence_delay_ms: 0,
            fences: Vec::new(),
            now: 0,
            stuck: false,
            forced: None,
            log: Vec::new(),
        }
    }

    /// The flavour.
    pub fn flavour(&self) -> Flavour {
        self.flavour
    }

    /// The local identity.
    pub fn local(&self) -> &LkIdentity {
        &self.local
    }

    /// Rust flavour: whether the Sew patch is applied (default true).
    pub fn set_patched(&mut self, patched: bool) {
        self.patched = patched;
    }

    /// Fences resolve this long after they are applied (0: at once).
    pub fn set_fence_delay(&mut self, ms: u64) {
        self.fence_delay_ms = ms;
    }

    /// The caller's clock (for fence delays and frame times).
    pub fn set_now(&mut self, now: Millis) {
        self.now = now;
    }

    /// Fences whose delay has passed; the caller answers each with `Input::Installed`.
    pub fn due_fences(&mut self, now: Millis) -> Vec<FenceId> {
        let mut due = Vec::new();
        self.fences.retain(|(t, f)| {
            if *t <= now {
                due.push(*f);
                false
            } else {
                true
            }
        });
        due
    }

    /// The next fence due time.
    pub fn next_fence_at(&self) -> Option<Millis> {
        self.fences.iter().map(|(t, _)| *t).min()
    }

    /// The local sender's slot (the bridge's self-check reads this).
    pub fn send_slot(&self) -> Option<Slot> {
        self.forced.or(self.send_slot)
    }

    /// Forces the local sender's index: it stays there until the next
    /// switch that is applied.
    pub fn force_send_slot(&mut self, slot: Option<Slot>) {
        self.forced = slot;
    }

    /// Test-only stuck-sender mode: declares `sender_key_index` but never switches.
    #[cfg(feature = "testing")]
    pub fn set_stuck_sender(&mut self, stuck: bool) {
        self.stuck = stuck;
    }

    /// Rust flavour: a new local track is published (a new sender cryptor).
    pub fn publish_new_local_track(&mut self) {
        if self.flavour == Flavour::Rust {
            self.send_slot = Some(if self.patched {
                self.remembered.unwrap_or(Slot::of(0, 0))
            } else {
                Slot::of(0, 0)
            });
        }
    }

    /// Rust flavour: the bridge re-asserts the send slot after a reconnect or a new
    /// track.
    pub fn reassert(&mut self, slot: Slot) {
        if !self.stuck {
            self.send_slot = Some(slot);
            self.forced = None;
        }
    }

    fn set_key(&mut self, id: &LkIdentity, slot: Slot, ikm: FrameIkm) {
        let ring = self
            .rings
            .entry(id.clone())
            .or_insert_with(|| (0..KEY_RING_SIZE).map(|_| None).collect());
        if let Some(s) = ring.get_mut(usize::from(slot.get())) {
            *s = Some(ikm);
        }
    }

    /// The key held for `id` at `slot`.
    pub fn key(&self, id: &LkIdentity, slot: Slot) -> Option<&FrameIkm> {
        self.rings
            .get(id)
            .and_then(|r| r.get(usize::from(slot.get())))
            .and_then(Option::as_ref)
    }

    /// A frame from the local sender, if it has a key at its send slot (no key → the
    /// frame is dropped, never sent in plaintext).
    pub fn send_frame(&self, at: Millis) -> Option<VirtualFrame> {
        let slot = self.send_slot()?;
        let key = self.key(&self.local, slot)?;
        Some(VirtualFrame {
            identity: self.local.clone(),
            slot,
            key: key.duplicate(),
            at,
        })
    }

    /// Whether this receiver decrypts `f`.
    pub fn can_decrypt(&self, f: &VirtualFrame) -> bool {
        self.key(&f.identity, f.slot)
            .is_some_and(|k| k.ct_eq(&f.key))
    }

    /// Identities with a ring.
    pub fn identities(&self) -> impl Iterator<Item = &LkIdentity> {
        self.rings.keys()
    }

    /// The event log (kind, identity, slot), for fidelity traces.
    pub fn log(&self) -> &[String] {
        &self.log
    }
}

impl KeySink for VirtualSdk {
    fn apply(&mut self, ev: KeyEvent) -> Result<SinkAck, SinkError> {
        self.log.push(format!(
            "{} {} {}",
            ev.kind(),
            ev.identity().map(|i| i.as_str()).unwrap_or("-"),
            ev.slot().map(|s| s.get().to_string()).unwrap_or_default()
        ));
        match ev {
            KeyEvent::Install {
                identity,
                slot,
                ikm,
            } => {
                let local = identity == self.local;
                self.set_key(&identity, slot, ikm);
                if local && self.flavour == Flavour::Js && !self.stuck {
                    // A local install switches the sender.
                    self.send_slot = Some(slot);
                    self.forced = None;
                }
            }
            KeyEvent::Activate {
                identity,
                slot,
                ikm,
            } => {
                self.set_key(&identity, slot, ikm);
                self.remembered = Some(slot);
                if !self.stuck {
                    self.send_slot = Some(slot);
                    self.forced = None;
                }
            }
            KeyEvent::Retire {
                identity,
                slot,
                filler,
            } => {
                if identity == self.local && self.flavour == Flavour::Js {
                    // The JS bridge never touches the local identity on retire.
                    return Ok(SinkAck::Now);
                }
                if identity == self.local && self.send_slot() == Some(slot) {
                    return Ok(SinkAck::Now);
                }
                self.set_key(&identity, slot, filler);
            }
            KeyEvent::Forget { identity } => {
                if identity != self.local {
                    self.rings.remove(&identity);
                }
            }
            KeyEvent::Fence { id } => {
                if self.fence_delay_ms == 0 {
                    return Ok(SinkAck::Now);
                }
                self.fences.push((self.now + self.fence_delay_ms, id));
                return Ok(SinkAck::Later(id));
            }
            KeyEvent::Clear => {
                let local = self.local.clone();
                let ids: Vec<LkIdentity> = self
                    .rings
                    .keys()
                    .filter(|i| **i != local)
                    .cloned()
                    .collect();
                for id in ids {
                    for s in 0..KEY_RING_SIZE {
                        if let Some(slot) = Slot::new(s) {
                            self.set_key(&id, slot, FrameIkm::random_os());
                        }
                    }
                }
            }
        }
        Ok(SinkAck::Now)
    }

    fn caps(&self) -> Caps {
        let sdk = match self.flavour {
            Flavour::Js => "js",
            Flavour::Rust => "rust",
        };
        Caps::from_bridge(true, sdk, "virtual", None)
    }
}
