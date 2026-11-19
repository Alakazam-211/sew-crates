//! Read receipts in DMs and group DMs: each member's "seen up to" position, shown to the other
//! members.
//!
//! A member's seen position in a DM only moves forward: it is the highest message id of every
//! channel ack the member made there (`POST /api/v1/channels/{id}/ack` and the bulk ack). Mark
//! unread moves the member's own read cursor back, never the seen position. Space channels have
//! none.
//!
//! Who sees what: a DM member sees another member's position when the install policy
//! `dm.read_receipts` is on and both of them share their read receipts (the member setting
//! `share_read_receipts`, on by default). It is reciprocal: a member who turns sharing off is
//! neither shown nor shown anyone's. Nothing is kept while it is off.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationError};
use crate::id::Id;
use crate::time::Timestamp;

/// Feature flag: read receipts in DMs (the seen positions, the member setting, the policy
/// `dm.read_receipts` and the events `dm_seen`, `read_receipt_settings_updated` and
/// `read_receipts_policy_updated`).
pub const FEATURE_DM_READ_RECEIPTS: &str = "dm_read_receipts";

/// `details.reason` values of this module's errors.
pub mod reasons {
    /// `422 invalid_state`: read receipts exist only in DMs and group DMs.
    pub const NOT_A_DM: &str = "not_a_dm";
}

/// One member's seen position in a DM: the body of `dm_seen`, an item of
/// [`DmReadReceipts`] and of `ReadyChannel.dm_seen`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct DmSeen {
    /// The DM or group DM.
    pub channel_id: Id,
    /// The member whose position this is (never the receiver).
    pub member_id: Id,
    /// Every message with an id at or below this one was seen (a comparator: the message
    /// itself may since have been deleted). Always set in READY and
    /// `GET /api/v1/channels/{id}/read_receipts`; absent only in a `dm_seen` event that
    /// removes the member's position (they stopped sharing their read receipts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_id: Option<Id>,
    /// When the position last moved; absent with `seen_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_at: Option<Timestamp>,
}

impl DmSeen {
    /// A position: `member_id` has seen `channel_id` up to `seen_id`, as of `seen_at`.
    pub fn new(channel_id: Id, member_id: Id, seen_id: Id, seen_at: Timestamp) -> Self {
        DmSeen {
            channel_id,
            member_id,
            seen_id: Some(seen_id),
            seen_at: Some(seen_at),
        }
    }

    /// The removal of `member_id`'s position in `channel_id` (a `dm_seen` event without
    /// `seen_id`).
    pub fn removed(channel_id: Id, member_id: Id) -> Self {
        DmSeen {
            channel_id,
            member_id,
            seen_id: None,
            seen_at: None,
        }
    }
}

/// Response of `GET /api/v1/channels/{id}/read_receipts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct DmReadReceipts {
    /// The DM.
    pub channel_id: Id,
    /// Read receipts apply to the caller here: the policy is on, the caller shares and is a
    /// member of the DM. When `false`, `items` is empty.
    pub active: bool,
    /// The other members' positions, ascending member id (members who do not share, or have
    /// seen nothing since sharing, are left out).
    pub items: Vec<DmSeen>,
}

impl DmReadReceipts {
    /// The answer for `channel_id`.
    pub fn new(channel_id: Id, active: bool, items: Vec<DmSeen>) -> Self {
        DmReadReceipts {
            channel_id,
            active,
            items,
        }
    }
}

/// The caller's read-receipt settings: `GET /api/v1/me/read_receipts`, the answer of
/// `POST /api/v1/me/read_receipts/set`, `ReadyInstall.read_receipts` and the event
/// `read_receipt_settings_updated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct ReadReceiptSettings {
    /// The member shares their seen positions (default `true`). Off, their positions are
    /// neither kept nor shown, and they see nobody's.
    pub share_read_receipts: bool,
    /// The install policy `dm.read_receipts` (default `true`). Off, nobody's are kept or shown.
    pub install_enabled: bool,
}

impl ReadReceiptSettings {
    /// The settings.
    pub fn new(share_read_receipts: bool, install_enabled: bool) -> Self {
        ReadReceiptSettings {
            share_read_receipts,
            install_enabled,
        }
    }

    /// Read receipts apply to this member: the policy is on and they share.
    pub fn active(&self) -> bool {
        self.share_read_receipts && self.install_enabled
    }
}

/// Body of `POST /api/v1/me/read_receipts/set`. Returns [`ReadReceiptSettings`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SetReadReceiptsRequest {
    /// Share read receipts. `false` also deletes the member's stored positions.
    pub share_read_receipts: bool,
}

impl SetReadReceiptsRequest {
    /// The request.
    pub fn new(share_read_receipts: bool) -> Self {
        SetReadReceiptsRequest {
            share_read_receipts,
        }
    }
}

impl Validate for SetReadReceiptsRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}

/// Body of `read_receipts_policy_updated`, sent to every session when the install policy
/// `dm.read_receipts` changes. Off: drop every seen position. On: none are kept yet; they
/// arrive as `dm_seen` when members read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct ReadReceiptsPolicyUpdated {
    /// The policy's new value.
    pub enabled: bool,
}

impl ReadReceiptsPolicyUpdated {
    /// The event body.
    pub fn new(enabled: bool) -> Self {
        ReadReceiptsPolicyUpdated { enabled }
    }
}
