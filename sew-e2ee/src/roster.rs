//! The roster: built from the **verified MLS tree only**. The engine's
//! participant list is used by the app for media state only.

use crate::ids::{Id, LkIdentity};

/// A leaf's state in the roster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RosterState {
    /// Named by a pending Add.
    Joining,
    /// A member of the active epoch.
    Active,
    /// Named by a pending Remove, until `activate` (so a member being removed is never
    /// counted as active in between).
    Leaving,
}

/// One roster entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterEntry {
    /// The leaf index (`None` for a pending Add, which has no leaf yet).
    pub leaf_index: Option<u32>,
    /// The engine identity the DS bound to the leaf.
    pub lk_identity: LkIdentity,
    /// The member (from the certificate).
    pub member_id: Id,
    /// The device (from the certificate).
    pub device_id: Id,
    /// Platform code (1 web, 2 desktop, 3 ios, 4 android, 5 other).
    pub platform: u8,
    /// The device label ("Chrome on macOS").
    pub label: String,
    /// Joining, active or leaving.
    pub state: RosterState,
}

/// How the app labels a media participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaLabel {
    /// A leaf of the key group.
    Member(u32),
    /// The recorder ("Bobbin"), named in a valid `recorder` dispatch.
    Recorder,
    /// "Not in the encrypted session": no leaf, no key.
    NotInEncryptedSession,
}
