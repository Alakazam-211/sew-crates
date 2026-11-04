//! Ids and small value types. The string names (`r_…` room, 32-hex
//! join id, `p_…` engine identity) are `sew_proto::e2ee`'s types, re-exported under the
//! names this crate's API uses; the slot formula and the labels come from
//! `sew_proto::e2ee::tls`.

use std::fmt;

pub use sew_proto::e2ee::names::{CallId as RoomName, JoinIdHex as JoinId, LkIdentity};
pub use sew_proto::e2ee::{CallScope, ConsentState as Consent};
pub use sew_proto::id::Id;

/// A member (Sew id).
pub type MemberId = Id;
/// A recording (Sew id).
pub type RecordingId = Id;
/// An MLS epoch.
pub type Epoch = u64;
/// The caller's clock in **unix milliseconds**. The core never reads a clock: every input
/// carries `now`, and certificate validity, key-package lifetimes and timers all use it.
pub type Millis = u64;
/// Unix milliseconds (the same clock as [`Millis`]; the alias names wall-clock uses).
pub type UnixMs = u64;

/// The key-ring size every bridge uses: the only accepted value.
pub const KEY_RING_SIZE: u8 = sew_proto::e2ee::tls::KEY_RING_SIZE;

/// A key slot (`0..64`), the SDK's key index.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(u8);

impl Slot {
    /// A slot, if `< 64`.
    pub const fn new(v: u8) -> Option<Slot> {
        if v < KEY_RING_SIZE {
            Some(Slot(v))
        } else {
            None
        }
    }

    /// `slot(e) = (slot_base + e) mod 64`.
    pub fn of(slot_base: u8, epoch: Epoch) -> Slot {
        Slot(sew_proto::e2ee::tls::slot(slot_base, epoch, KEY_RING_SIZE))
    }

    /// The raw index.
    pub const fn get(self) -> u8 {
        self.0
    }

    /// The SDK's `i32` key index.
    pub fn as_i32(self) -> i32 {
        i32::from(self.0)
    }

    /// The next slot round the ring.
    pub fn next(self) -> Slot {
        Slot((self.0 + 1) % KEY_RING_SIZE)
    }
}

impl fmt::Debug for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Slot({})", self.0)
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

macro_rules! counter_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u64);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

counter_id!(
    /// A bridge fence: `Input::Installed { fence }` answers `KeyEvent::Fence { id }`.
    FenceId
);
counter_id!(
    /// A device-signature request: `Input::DeviceSignature { request }` answers
    /// `Output::NeedDeviceSignature { request }`.
    SignId
);
counter_id!(
    /// A key-route request: `Input::Reply(id, …)` answers `Output::Send(id, …)`.
    RequestId
);

/// `"p_" + base32(join_id)` (equal to the media engine's `identity_for`).
pub fn lk_identity_for(join_id: &JoinId) -> LkIdentity {
    join_id.lk_identity()
}

/// Whether `id` is exactly `"p_" + base32(join_id)`.
pub fn identity_matches(join_id: &JoinId, id: &LkIdentity) -> bool {
    &join_id.lk_identity() == id
}

/// The UTF-8 decimal form of an install id, as certificates carry it.
pub fn install_id_bytes(install_id: u64) -> Vec<u8> {
    install_id.to_string().into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_formula_wraps() {
        assert_eq!(Slot::of(60, 0).get(), 60);
        assert_eq!(Slot::of(60, 4).get(), 0);
        assert_eq!(Slot::of(0, 64).get(), 0);
        assert_eq!(Slot::of(63, 1), Slot::new(0).unwrap_or(Slot(0)));
        assert!(Slot::new(64).is_none());
        assert_eq!(Slot(63).next().get(), 0);
    }
}
