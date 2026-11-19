//! Voice room notifications: the room start (`voice_started`) and voice invites
//! (`voice_invite`).
//!
//! - **Room start** (flag [`FEATURE_VOICE_STARTED_FEED`]): a voice room session starts when a
//!   person connects to an empty room and stays a few seconds; it ends when the room has been
//!   empty for a while. The members who get it (channel members at level `all`, not muted,
//!   able to connect, not in voice themselves, within a per-person throttle) get the event
//!   `voice_started` ([`VoiceStartedEvent`], `active: true`) and the person feed item
//!   `voice_started`; `active: false` and the feed's `voice_ended` clear it when the session
//!   ends or the member joins the room.
//! - **Invites** (flag [`FEATURE_VOICE_INVITES`]): a person in a Space voice room invites up to
//!   [`VOICE_INVITE_MEMBERS_MAX`] members (`POST /api/v1/channels/{id}/voice/invite`,
//!   [`VoiceInviteRequest`] → [`VoiceInviteResults`], one result per member). An invite is
//!   accepted by joining the room (`POST /api/v1/channels/{id}/voice/join`, which runs every
//!   join check), declined by the invitee (`…/voice/invite/decline`,
//!   [`VoiceInviteDeclineRequest`]) or cancelled by the inviter (`…/voice/invite/cancel`,
//!   [`VoiceInviteCancelRequest`]); both answer the [`VoiceInvite`]. The event `voice_invite`
//!   ([`VoiceInvite`]) goes to the invitee's and the inviter's sessions only; READY carries the
//!   member's incoming pending invites in their rooms (`ReadyChannel.voice_invites`, and
//!   `ReadyVoiceChannel.voice_invites` for public rooms they have not joined), and
//!   `GET /api/v1/me/voice/invites` ([`MyVoiceInvites`]) both directions.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationError};
use crate::id::Id;
use crate::time::Timestamp;

pub use crate::mgmt::{FEATURE_VOICE_INVITES, FEATURE_VOICE_STARTED_FEED};

/// The most members one invite request names.
pub const VOICE_INVITE_MEMBERS_MAX: usize = 10;

unit_enum! {
    /// Where a voice invite stands. `pending` until it stops; every other value is a stop.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum VoiceInviteState {
        /// Waiting for the invitee.
        Pending = "pending",
        /// The invitee joined the room (on any device).
        Accepted = "accepted",
        /// The invitee declined.
        Declined = "declined",
        /// The inviter cancelled.
        Cancelled = "cancelled",
        /// The inviter left the room (left, was moved out or disconnected).
        InviterLeft = "inviter_left",
        /// Nobody answered before `expires_at`.
        Expired = "expired",
        /// The room ended: archived, closing, its session ended, voice turned off or the media
        /// path down.
        RoomEnded = "room_ended",
        /// The invitee can no longer join the room: no longer able to connect, deactivated,
        /// out of the Space, or erased.
        Revoked = "revoked",
    }
}

impl VoiceInviteState {
    /// Whether the invite is still waiting.
    pub const fn is_pending(self) -> bool {
        matches!(self, VoiceInviteState::Pending)
    }
}

unit_enum! {
    /// One member's result in [`VoiceInviteResults`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum VoiceInviteResultState {
        /// The member has a pending invite (a new one, or the inviter's earlier one, unchanged).
        Pending = "pending",
        /// No invite: `reason` says why.
        Refused = "refused",
    }
}

/// `reason` values of a refused member ([`VoiceInviteResultState::Refused`]) and of a silent
/// invite.
pub mod invite_reason {
    /// Refused: the inviter named themself.
    pub const SELF: &str = "self";
    /// Refused: an agent, an assistant or a service member.
    pub const NOT_A_PERSON: &str = "not_a_person";
    /// Refused: not in the channel's Space (also unknown and deactivated members).
    pub const NOT_SPACE_MEMBER: &str = "not_space_member";
    /// Refused: cannot join the room (no VIEW_CHANNEL or CONNECT, or a private channel the
    /// member is not in).
    pub const CANNOT_CONNECT: &str = "cannot_connect";
    /// Refused: already in the room.
    pub const ALREADY_IN_ROOM: &str = "already_in_room";
    /// Refused: this inviter invited this member to this room too often in the last hour.
    pub const INVITE_LIMIT: &str = "invite_limit";
    /// Silent: the invitee is in voice elsewhere (a room or a DM call).
    pub const BUSY: &str = "busy";
    /// Silent: the invitee muted the channel, or its level is `none`.
    pub const MUTED: &str = "muted";
    /// Silent: the invitee got many invites in the last minutes.
    pub const RATE_LIMITED: &str = "rate_limited";
    /// Silent, in the live event only: the invitee is in Do Not Disturb.
    pub const DND: &str = "dnd";
}

/// `details.reason` values of the invite routes' errors.
pub mod reasons {
    /// `422 invalid_state`: a decline or cancel of an invite that already stopped.
    pub const INVITE_NOT_PENDING: &str = "invite_not_pending";
    /// `409 conflict`: the inviter is not in this voice room.
    pub const NOT_IN_VOICE: &str = crate::voice::reasons::NOT_IN_VOICE;
}

/// `POST /api/v1/channels/{id}/voice/invite` body: invite members into the room the caller is
/// in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VoiceInviteRequest {
    /// The members to invite: 1–10, distinct.
    pub member_ids: Vec<Id>,
}

impl VoiceInviteRequest {
    /// A request inviting `member_ids`.
    pub fn new(member_ids: Vec<Id>) -> Self {
        VoiceInviteRequest { member_ids }
    }
}

impl Validate for VoiceInviteRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        let n = self.member_ids.len();
        if !(1..=VOICE_INVITE_MEMBERS_MAX).contains(&n) {
            return Err(ValidationError::invalid(
                "member_ids",
                format!("member_ids must name 1–{VOICE_INVITE_MEMBERS_MAX} members"),
            ));
        }
        let mut ids = self.member_ids.clone();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() != n {
            return Err(ValidationError::invalid(
                "member_ids",
                "member_ids must be distinct",
            ));
        }
        Ok(())
    }
}

/// One member's result of an invite request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct VoiceInviteResult {
    /// The member named.
    pub member_id: Id,
    /// `pending` (invited) or `refused`.
    pub state: VoiceInviteResultState,
    /// The invite, when `pending`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_id: Option<Id>,
    /// When the invite expires, when `pending`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    /// A pending invite the invitee is not pushed (absent when `false`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub silent: bool,
    /// Why it was refused, or why it is silent ([`invite_reason`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl VoiceInviteResult {
    /// A pending invite of `member_id`.
    pub fn pending(member_id: Id, invite_id: Id, expires_at: Timestamp) -> Self {
        VoiceInviteResult {
            member_id,
            state: VoiceInviteResultState::Pending,
            invite_id: Some(invite_id),
            expires_at: Some(expires_at),
            silent: false,
            reason: None,
        }
    }

    /// A refusal of `member_id` with its reason.
    pub fn refused(member_id: Id, reason: &str) -> Self {
        VoiceInviteResult {
            member_id,
            state: VoiceInviteResultState::Refused,
            invite_id: None,
            expires_at: None,
            silent: false,
            reason: Some(reason.to_owned()),
        }
    }
}

/// Response of `POST /api/v1/channels/{id}/voice/invite`: one result per member, in request
/// order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct VoiceInviteResults {
    /// The results.
    pub invites: Vec<VoiceInviteResult>,
}

impl VoiceInviteResults {
    /// The results.
    pub fn new(invites: Vec<VoiceInviteResult>) -> Self {
        VoiceInviteResults { invites }
    }
}

/// `POST /api/v1/channels/{id}/voice/invite/decline` body: the invitee declines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VoiceInviteDeclineRequest {
    /// The invite.
    pub invite_id: Id,
}

impl VoiceInviteDeclineRequest {
    /// Declines `invite_id`.
    pub fn new(invite_id: Id) -> Self {
        VoiceInviteDeclineRequest { invite_id }
    }
}

impl Validate for VoiceInviteDeclineRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}

/// `POST /api/v1/channels/{id}/voice/invite/cancel` body: the inviter withdraws.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct VoiceInviteCancelRequest {
    /// The invite.
    pub invite_id: Id,
}

impl VoiceInviteCancelRequest {
    /// Cancels `invite_id`.
    pub fn new(invite_id: Id) -> Self {
        VoiceInviteCancelRequest { invite_id }
    }
}

impl Validate for VoiceInviteCancelRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}

/// A voice invite: the body of `voice_invite` (to the invitee's and the inviter's sessions,
/// never the rest of the room), the answer of decline and cancel, an item of
/// `ReadyChannel.voice_invites` and of [`MyVoiceInvites`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct VoiceInvite {
    /// The invite.
    pub invite_id: Id,
    /// The voice channel.
    pub channel_id: Id,
    /// Its Space.
    pub space_id: Id,
    /// The inviter.
    pub invited_by: Id,
    /// The invitee.
    pub member_id: Id,
    /// Where it stands.
    pub state: VoiceInviteState,
    /// On a silent pending invite, why it is silent (`busy`, `muted`, `rate_limited`, `dnd`);
    /// on a stopped one, the stop (the same word as `state`). Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The invitee is not pushed (the invitee's copy only; absent when `false`): clients stay
    /// quiet and still show it in the room UI.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub silent: bool,
    /// The room has a live recording or records every call (the invitee's copy only; absent
    /// when `false`). Joining still meets the recording notice.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recorded: bool,
    /// When it expires.
    pub expires_at: Timestamp,
    /// When it was made.
    pub created_at: Timestamp,
}

impl VoiceInvite {
    /// A pending, loud, unrecorded invite.
    pub fn new(
        invite_id: Id,
        channel_id: Id,
        space_id: Id,
        invited_by: Id,
        member_id: Id,
        expires_at: Timestamp,
        created_at: Timestamp,
    ) -> Self {
        VoiceInvite {
            invite_id,
            channel_id,
            space_id,
            invited_by,
            member_id,
            state: VoiceInviteState::Pending,
            reason: None,
            silent: false,
            recorded: false,
            expires_at,
            created_at,
        }
    }
}

/// Response of `GET /api/v1/me/voice/invites`: the caller's pending invites, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct MyVoiceInvites {
    /// Invites to the caller.
    pub incoming: Vec<VoiceInvite>,
    /// Invites the caller sent.
    pub outgoing: Vec<VoiceInvite>,
}

impl MyVoiceInvites {
    /// Both lists.
    pub fn new(incoming: Vec<VoiceInvite>, outgoing: Vec<VoiceInvite>) -> Self {
        MyVoiceInvites { incoming, outgoing }
    }
}

/// Body of `voice_started`: a voice room the receiver gets pushes for came alive
/// (`active: true`), or that push no longer applies (`active: false`: the room went quiet or
/// the receiver joined it). Clients pop up only on this event, never on `voice_state_updated`
/// alone, and stay quiet in Do Not Disturb and for the channel in view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct VoiceStartedEvent {
    /// The voice channel.
    pub channel_id: Id,
    /// Its Space.
    pub space_id: Id,
    /// Who started the room session.
    pub started_by: Id,
    /// `true`: the room started; `false`: clear it.
    pub active: bool,
    /// When the session started (`active: true`) or the push stopped applying.
    pub at: Timestamp,
}

impl VoiceStartedEvent {
    /// The event.
    pub fn new(channel_id: Id, space_id: Id, started_by: Id, active: bool, at: Timestamp) -> Self {
        VoiceStartedEvent {
            channel_id,
            space_id,
            started_by,
            active,
            at,
        }
    }
}

#[cfg(test)]
// The package has no clippy.toml (Sew's allows unwrap in tests); the module says it itself.
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn id(n: i64) -> Id {
        Id::from_i64(n).unwrap()
    }

    #[test]
    fn request_bounds() {
        assert!(VoiceInviteRequest::new(vec![id(1)]).validate().is_ok());
        assert!(
            VoiceInviteRequest::new((1..=10).map(id).collect())
                .validate()
                .is_ok()
        );
        assert!(VoiceInviteRequest::new(vec![]).validate().is_err());
        assert!(
            VoiceInviteRequest::new((1..=11).map(id).collect())
                .validate()
                .is_err()
        );
        assert!(
            VoiceInviteRequest::new(vec![id(1), id(1)])
                .validate()
                .is_err()
        );
        let r: Result<VoiceInviteRequest, _> =
            serde_json::from_str(r#"{"member_ids": ["1"], "silent": true}"#);
        assert!(r.is_err(), "unknown fields are refused");
    }

    #[test]
    fn invite_copies() {
        let t = Timestamp::from_unix_ms_saturating(0);
        let mut i = VoiceInvite::new(id(1), id(2), id(3), id(4), id(5), t, t);
        let v = serde_json::to_value(&i).unwrap();
        assert_eq!(v["state"], "pending");
        for absent in ["reason", "silent", "recorded"] {
            assert!(v.get(absent).is_none(), "{v}");
        }
        i.silent = true;
        i.recorded = true;
        i.reason = Some(invite_reason::BUSY.into());
        let v = serde_json::to_value(&i).unwrap();
        assert_eq!(
            (v["silent"].clone(), v["recorded"].clone()),
            (true.into(), true.into())
        );
        let back: VoiceInvite = serde_json::from_value(v).unwrap();
        assert_eq!(back, i);
        assert!(VoiceInviteState::Pending.is_pending());
        assert!(!VoiceInviteState::InviterLeft.is_pending());
        assert_eq!(VoiceInviteState::InviterLeft.as_str(), "inviter_left");
        let s: VoiceInviteState = serde_json::from_str("\"withdrawn\"").unwrap();
        assert_eq!(s, VoiceInviteState::Unknown);
    }

    #[test]
    fn result_shapes() {
        let t = Timestamp::from_unix_ms_saturating(0);
        let p = serde_json::to_value(VoiceInviteResult::pending(id(1), id(2), t)).unwrap();
        assert_eq!(p["state"], "pending");
        assert!(
            p.get("reason").is_none() && p.get("silent").is_none(),
            "{p}"
        );
        let r = serde_json::to_value(VoiceInviteResult::refused(
            id(1),
            invite_reason::CANNOT_CONNECT,
        ))
        .unwrap();
        assert_eq!(r["state"], "refused");
        assert_eq!(r["reason"], "cannot_connect");
        assert!(
            r.get("invite_id").is_none() && r.get("expires_at").is_none(),
            "{r}"
        );
    }
}
