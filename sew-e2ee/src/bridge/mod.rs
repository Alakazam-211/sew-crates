//! The key-provider bridge contract.
//!
//! Keys leave the core only as [`KeyEvent`]s. A bridge (`@sew/e2ee`'s `JsBridge` over
//! WASM, Sew's LiveKit Rust SDK bridge (not published), or the test [`virtual_sdk::VirtualSdk`])
//! applies them to its SDK in order and answers each [`KeyEvent::Fence`] with
//! `Input::Installed { fence }` once every earlier event is in effect.
//!
//! The SDK configuration is identical on every bridge ([`SDK_CONFIG`], checked by
//! `bridge_config_constants`): per-participant keys, ratchet window 0, failure tolerance −1,
//! ring 64, 128-bit keys, HKDF with salt `"LKFrameEncryptionKey"`, empty SIF trailer, no data
//! packets.

pub mod caps;
pub mod epochs;
pub mod keyring;
pub mod virtual_sdk;

use crate::ids::{FenceId, LkIdentity, Slot};
use crate::keys::FrameIkm;

pub use caps::Caps;
pub use keyring::KeyRing;

/// One key instruction for a bridge. Not `Clone`: IKMs are moved, never copied
/// implicitly.
#[derive(Debug)]
pub enum KeyEvent {
    /// Install a **remote** sender's key at `slot` (I1: never the local identity).
    Install {
        /// The sender's engine identity.
        identity: LkIdentity,
        /// The slot.
        slot: Slot,
        /// The key.
        ikm: FrameIkm,
    },
    /// The local identity's key: install it **and** switch the sender to `slot`.
    Activate {
        /// The local identity.
        identity: LkIdentity,
        /// The slot.
        slot: Slot,
        /// The key.
        ikm: FrameIkm,
    },
    /// Overwrite `slot` of `identity` with random bytes (10 s after the next activation). The JS
    /// bridge ignores it for the local identity.
    Retire {
        /// The identity.
        identity: LkIdentity,
        /// The slot.
        slot: Slot,
        /// Fresh random bytes (never derived).
        filler: FrameIkm,
    },
    /// The identity left the key group; no further keys (its slots are retired first).
    Forget {
        /// The identity.
        identity: LkIdentity,
    },
    /// Answer `Input::Installed { fence: id }` when every earlier event is in effect.
    Fence {
        /// The fence.
        id: FenceId,
    },
    /// Leaving: overwrite every remote slot with random bytes.
    Clear,
}

impl KeyEvent {
    /// A short name for logs and telemetry.
    pub fn kind(&self) -> &'static str {
        match self {
            KeyEvent::Install { .. } => "install",
            KeyEvent::Activate { .. } => "activate",
            KeyEvent::Retire { .. } => "retire",
            KeyEvent::Forget { .. } => "forget",
            KeyEvent::Fence { .. } => "fence",
            KeyEvent::Clear => "clear",
        }
    }

    /// The identity it is about, if any.
    pub fn identity(&self) -> Option<&LkIdentity> {
        match self {
            KeyEvent::Install { identity, .. }
            | KeyEvent::Activate { identity, .. }
            | KeyEvent::Retire { identity, .. }
            | KeyEvent::Forget { identity } => Some(identity),
            KeyEvent::Fence { .. } | KeyEvent::Clear => None,
        }
    }

    /// The slot it is about, if any.
    pub fn slot(&self) -> Option<Slot> {
        match self {
            KeyEvent::Install { slot, .. }
            | KeyEvent::Activate { slot, .. }
            | KeyEvent::Retire { slot, .. } => Some(*slot),
            _ => None,
        }
    }

    /// An explicit copy (zeroizing copies of the key bytes). Used by drivers that feed one
    /// event to both a [`KeyRing`] shadow and a bridge, and by tests.
    pub fn duplicate(&self) -> KeyEvent {
        match self {
            KeyEvent::Install {
                identity,
                slot,
                ikm,
            } => KeyEvent::Install {
                identity: identity.clone(),
                slot: *slot,
                ikm: ikm.duplicate(),
            },
            KeyEvent::Activate {
                identity,
                slot,
                ikm,
            } => KeyEvent::Activate {
                identity: identity.clone(),
                slot: *slot,
                ikm: ikm.duplicate(),
            },
            KeyEvent::Retire {
                identity,
                slot,
                filler,
            } => KeyEvent::Retire {
                identity: identity.clone(),
                slot: *slot,
                filler: filler.duplicate(),
            },
            KeyEvent::Forget { identity } => KeyEvent::Forget {
                identity: identity.clone(),
            },
            KeyEvent::Fence { id } => KeyEvent::Fence { id: *id },
            KeyEvent::Clear => KeyEvent::Clear,
        }
    }
}

/// How a bridge acknowledged an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkAck {
    /// In effect now (a fence is answered at once).
    Now,
    /// A fence that resolves later; the bridge feeds `Input::Installed { fence }` then.
    Later(FenceId),
}

/// Why a bridge refused an event. Carries no key bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SinkError {
    /// The SDK refused a key (`set_key` returned false). Fails loudly.
    #[error("the SDK refused the key for slot {slot}")]
    SdkRefused {
        /// The slot.
        slot: u8,
    },
    /// The sender stayed on another slot after one re-apply.
    #[error("sender key index stuck: expected {expected}, observed {observed}")]
    SenderIndexStuck {
        /// The slot it should be on.
        expected: u8,
        /// The slot the SDK reports.
        observed: u8,
    },
    /// The engine downgraded a publication's encryption.
    #[error("the engine downgraded a publication's encryption")]
    EncryptionDowngraded,
    /// `libwebrtc` debug/trace logging is on without the explicit opt-in.
    #[error("libwebrtc debug logging would log keys; refused without the explicit opt-in")]
    KeyLoggingRisk,
    /// The bridge is not armed (JS) or the room is gone.
    #[error("bridge not ready: {0}")]
    NotReady(&'static str),
}

/// A bridge: applies [`KeyEvent`]s to an SDK, in order.
pub trait KeySink {
    /// Applies one event.
    fn apply(&mut self, ev: KeyEvent) -> Result<SinkAck, SinkError>;
    /// What this bridge's SDK can do (the only source of [`Caps`]).
    fn caps(&self) -> Caps;
}

/// The SDK configuration every bridge uses (`bridge_config_constants`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdkConfig {
    /// Per-participant keys (`sharedKey: false`).
    pub shared_key: bool,
    /// Ratchet window.
    pub ratchet_window_size: i32,
    /// Failure tolerance.
    pub failure_tolerance: i32,
    /// Key ring size.
    pub key_ring_size: u8,
    /// Key size in bits.
    pub key_size_bits: u16,
    /// The KDF.
    pub kdf: &'static str,
    /// The HKDF salt.
    pub salt: &'static [u8],
    /// The SIF trailer (always empty).
    pub sif_trailer: &'static [u8],
    /// Data packets are never published.
    pub publish_data: bool,
}

/// The one configuration.
pub const SDK_CONFIG: SdkConfig = SdkConfig {
    shared_key: false,
    ratchet_window_size: 0,
    failure_tolerance: -1,
    key_ring_size: crate::ids::KEY_RING_SIZE,
    key_size_bits: 128,
    kdf: crate::config::KDF,
    salt: crate::config::LK_SALT,
    sif_trailer: b"",
    publish_data: false,
};
