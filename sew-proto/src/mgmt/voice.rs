//! Voice over mgmt (Sew 0.7.0): what K2's daemon reads and sets
//! about voice rooms without a member session.
//!
//! - Flag [`FEATURE_MGMT_VOICE_STATES`]: `GET /mgmt/v1/spaces/{space_id}/voice/states`
//!   ([`MgmtVoiceStates`]) is every voice state in one Space's voice channels now, with a cursor;
//!   `GET /mgmt/v1/spaces/{space_id}/voice/changes?after=&wait=&limit=`
//!   ([`MgmtVoiceChangesQuery`] → [`MgmtVoiceChanges`]) long-polls the changes after it. Voice
//!   state is live state, so the feed is in memory only (a ring of [`VOICE_CHANGES_RING`]
//!   changes per Space): after a Sew restart, or once the ring has dropped the changes after the
//!   cursor, the feed answers `410 cursor_expired` with [`MgmtVoiceCursorExpiredDetails`] and
//!   K2 reads `voice/states` again. `external_id` is Sew's (`k2:owner`, `k2:connect:<uuid>`);
//!   mapping it to a K2 login is K2's job. Any Space (mgmt is trusted).
//! - Flag [`FEATURE_VOICE_CHANNEL_CLOSING`]: `POST /mgmt/v1/channels/{channel_id}/closing`
//!   ([`MgmtChannelClosingRequest`] → [`MgmtChannelClosingResponse`]) closes a voice room: an
//!   empty room is archived at once; a live one refuses joins and moves (`409 conflict`
//!   `channel_closing`) and is archived when its last person leaves.
//! - Flag [`FEATURE_VOICE_MEDIA_PATH`]: `POST /mgmt/v1/voice/media-path`
//!   ([`MgmtMediaPathRequest`] → [`MgmtMediaPathResponse`]) is K2 saying the box's media path
//!   (the TURN lane) is down or up. While it is down, joins and moves answer
//!   `503 voice_unavailable` with `details.engine = "media_path_down"` and every live call ends
//!   (leave reason `media_path_down`). In memory: Sew starts `up`. `status.voice.media_path` is
//!   [`VoiceMediaPathStatus`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationError};
use crate::external_id::ExternalId;
use crate::id::Id;
use crate::time::Timestamp;
use crate::voice::{VoiceConnection, VoiceLeaveReason};

/// `voice/states` and the `voice/changes` feed over mgmt.
pub const FEATURE_MGMT_VOICE_STATES: &str = "mgmt_voice_states";
/// `POST /mgmt/v1/channels/{channel_id}/closing` and `Channel.closing`.
pub const FEATURE_VOICE_CHANNEL_CLOSING: &str = "voice_channel_closing";
/// `POST /mgmt/v1/voice/media-path` and `status.voice.media_path`. K2 calls the route
/// only when this flag is listed.
pub const FEATURE_VOICE_MEDIA_PATH: &str = "voice_media_path";

/// The longest `voice/changes` long-poll, in seconds.
pub const VOICE_CHANGES_WAIT_MAX_S: u32 = 30;
/// The default `voice/changes` page size.
pub const VOICE_CHANGES_LIMIT_DEFAULT: u32 = 100;
/// The largest `voice/changes` page size.
pub const VOICE_CHANGES_LIMIT_MAX: u32 = 500;
/// The changes each Space's feed keeps in memory.
pub const VOICE_CHANGES_RING: usize = 4_096;
/// The longest media-path `reason`, in chars.
pub const MEDIA_PATH_REASON_MAX_CHARS: usize = 64;

/// One member's voice state in a Space's voice channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtVoiceState {
    /// The voice channel.
    pub channel_id: Id,
    /// The member.
    pub member_id: Id,
    /// The member's external id (`k2:owner`, `k2:connect:<uuid>`, …).
    pub external_id: ExternalId,
    /// `connecting` or `connected`.
    pub connection: VoiceConnection,
    /// Muted by the member.
    pub self_mute: bool,
    /// Deafened by the member.
    pub self_deaf: bool,
    /// Muted by a moderator.
    pub server_mute: bool,
    /// Deafened by a moderator.
    pub server_deaf: bool,
    /// When the join succeeded (a move resets it).
    pub joined_at: Timestamp,
}

/// Response of `GET /mgmt/v1/spaces/{space_id}/voice/states`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtVoiceStates {
    /// Every voice state in the Space's voice channels now, by channel then member.
    pub states: Vec<MgmtVoiceState>,
    /// The Space's feed cursor (opaque): `voice/changes?after=<cursor>` has every change after
    /// this read (a change already in `states` may come again).
    pub cursor: String,
}

/// Query of `GET /mgmt/v1/spaces/{space_id}/voice/changes`: long-poll after a cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtVoiceChangesQuery {
    /// The cursor of `voice/states` or of the previous page's `next`.
    pub after: String,
    /// Long-poll up to this many seconds while there is nothing to return (0–30, default 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<u32>,
    /// At most this many changes (1–500, default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl MgmtVoiceChangesQuery {
    /// The page size, default 100.
    pub fn limit(&self) -> u32 {
        self.limit.unwrap_or(VOICE_CHANGES_LIMIT_DEFAULT)
    }

    /// The long-poll, in seconds (default 0: answer at once).
    pub fn wait_s(&self) -> u32 {
        self.wait.unwrap_or(0)
    }
}

impl Validate for MgmtVoiceChangesQuery {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.after.is_empty() || self.after.len() > 64 {
            return Err(ValidationError::invalid(
                "after",
                "after is the cursor of voice/states or of the previous page",
            ));
        }
        if let Some(l) = self.limit
            && !(1..=VOICE_CHANGES_LIMIT_MAX).contains(&l)
        {
            return Err(ValidationError::invalid(
                "limit",
                format!("limit must be 1–{VOICE_CHANGES_LIMIT_MAX}"),
            ));
        }
        if let Some(w) = self.wait
            && w > VOICE_CHANGES_WAIT_MAX_S
        {
            return Err(ValidationError::invalid(
                "wait",
                format!("wait must be at most {VOICE_CHANGES_WAIT_MAX_S} seconds"),
            ));
        }
        Ok(())
    }
}

unit_enum! {
    /// What a voice change is.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum MgmtVoiceChangeKind {
        /// A member joined a voice channel (or came in by a move; `state` is set).
        Joined = "joined",
        /// A member's state changed in the same channel (connected, mute, deafen; `state`).
        Updated = "updated",
        /// A member left a voice channel (`reason`: `left`, `moved`, `channel_archived`,
        /// `media_path_down`, …).
        Left = "left",
        /// The room is closing: no new joins; archived when its last person leaves.
        ChannelClosing = "channel_closing",
        /// The voice channel was archived.
        ChannelArchived = "channel_archived",
    }
}

/// One change of a Space's voice feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtVoiceChange {
    /// The change's place in the Space's feed (increasing; gaps are never skipped changes).
    pub seq: u64,
    /// What it is.
    pub kind: MgmtVoiceChangeKind,
    /// The voice channel.
    pub channel_id: Id,
    /// The member (`joined`, `updated`, `left`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_id: Option<Id>,
    /// The member's external id (`joined`, `updated`, `left`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<ExternalId>,
    /// The state now (`joined`, `updated`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<MgmtVoiceState>,
    /// Why the member left (`left`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<VoiceLeaveReason>,
}

/// Response of `GET /mgmt/v1/spaces/{space_id}/voice/changes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtVoiceChanges {
    /// The changes, oldest first.
    pub changes: Vec<MgmtVoiceChange>,
    /// The next `after`.
    pub next: String,
}

/// Details of the feed's `410 cursor_expired`: the changes after the cursor are gone (a Sew
/// restart, or the ring dropped them). Read `voice/states` again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtVoiceCursorExpiredDetails {
    /// The feed's cursor now.
    pub head: String,
}

/// Body of `POST /mgmt/v1/channels/{channel_id}/closing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtChannelClosingRequest {
    /// `true` closes the room; `false` reopens a room that is closing (not yet archived).
    pub closing: bool,
}

impl Validate for MgmtChannelClosingRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}

/// Response of `POST /mgmt/v1/channels/{channel_id}/closing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtChannelClosingResponse {
    /// The room is closing (or was closed and is archived).
    pub closing: bool,
    /// The room is archived (it was empty, or already archived).
    pub archived: bool,
}

unit_enum! {
    /// The media path's state.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum MediaPathState {
        /// Calls work (Sew's state after every start).
        Up = "up",
        /// Voice is unavailable: joins and moves refused, live calls ended.
        Down = "down",
    }
}

/// Body of `POST /mgmt/v1/voice/media-path` (idempotent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtMediaPathRequest {
    /// `down` or `up`.
    pub state: MediaPathState,
    /// K2's words for why (at most 64 chars, e.g. `voice_lane_down`); kept with `down` and
    /// shown as `details.media_path_reason` and `status.voice.media_path.media_path_reason`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Validate for MgmtMediaPathRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if !matches!(self.state, MediaPathState::Up | MediaPathState::Down) {
            return Err(ValidationError::invalid("state", "state is up or down"));
        }
        if let Some(r) = &self.reason
            && (r.chars().count() > MEDIA_PATH_REASON_MAX_CHARS || r.chars().any(char::is_control))
        {
            return Err(ValidationError::invalid(
                "reason",
                format!(
                    "reason is at most {MEDIA_PATH_REASON_MAX_CHARS} chars with no control chars"
                ),
            ));
        }
        Ok(())
    }
}

/// Response of `POST /mgmt/v1/voice/media-path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtMediaPathResponse {
    /// The state now.
    pub state: MediaPathState,
    /// Since when (a repeated call keeps it).
    pub since: Timestamp,
    /// The live calls this call ended (voice channels and DM calls; 0 for `up` and for a
    /// repeated `down`).
    pub ended_calls: u64,
}

/// `status.voice.media_path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VoiceMediaPathStatus {
    /// `up` or `down`.
    pub state: MediaPathState,
    /// Since when (the start of this Sew process while it has always been `up`).
    pub since: Timestamp,
    /// K2's reason, while `down`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_path_reason: Option<String>,
}
