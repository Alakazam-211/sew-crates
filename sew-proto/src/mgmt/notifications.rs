//! The person notification feed over mgmt: K2 long-polls it per App user and pushes
//! rings, missed calls and mentions to the person's phone (K2's `push.send`).
//!
//! - `GET /mgmt/v1/people/{external_id}/notifications?after=&limit=&wait=`
//!   ([`PersonNotificationsQuery`] → [`PersonNotificationsPage`]): the person's member-log
//!   entries after `after` that Sew's notify rules let through, as [`PersonNotification`]s of
//!   five kinds ([`PersonNotificationKind`]). The cursor is the member-log `seq`: it survives
//!   restarts. A cursor behind the log's floor (pruned) is `410 cursor_expired` with
//!   [`CursorExpiredDetails`].
//! - Persons only: an agent's external id is `404` (agents keep
//!   `GET /mgmt/v1/agents/{member_id}/deliveries`).
//! - No message text unless the install policy `notifications.mgmt_preview` is on; then a
//!   message item carries `preview`, at most [`PERSON_NOTIFICATION_PREVIEW_MAX_CHARS`] chars.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationError};
use crate::id::Id;
use crate::time::Timestamp;

/// The feature flag of this feed (`features` in `GET /mgmt/v1/status`).
pub const FEATURE_PERSON_NOTIFICATIONS: &str = "person_notifications";
/// The longest long-poll, in seconds.
pub const PERSON_NOTIFICATIONS_WAIT_MAX_S: u32 = 30;
/// The default page size.
pub const PERSON_NOTIFICATIONS_LIMIT_DEFAULT: u32 = 100;
/// The largest page size.
pub const PERSON_NOTIFICATIONS_LIMIT_MAX: u32 = 500;
/// The longest `preview`, in chars (cut at a char boundary).
pub const PERSON_NOTIFICATION_PREVIEW_MAX_CHARS: usize = 120;

/// Query of `GET /mgmt/v1/people/{external_id}/notifications`: long-poll after a member seq.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PersonNotificationsQuery {
    /// Return items with `seq > after`. K2 sends the previous page's `next`.
    pub after: u64,
    /// At most this many items (1–500, default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Long-poll up to this many seconds while there is nothing to return (0–30, default 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<u32>,
}

impl PersonNotificationsQuery {
    /// The page size, default 100.
    pub fn limit(&self) -> u32 {
        self.limit.unwrap_or(PERSON_NOTIFICATIONS_LIMIT_DEFAULT)
    }

    /// The long-poll, in seconds (default 0: answer at once).
    pub fn wait_s(&self) -> u32 {
        self.wait.unwrap_or(0)
    }
}

impl Validate for PersonNotificationsQuery {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(l) = self.limit
            && !(1..=PERSON_NOTIFICATIONS_LIMIT_MAX).contains(&l)
        {
            return Err(ValidationError::invalid(
                "limit",
                format!("limit must be 1–{PERSON_NOTIFICATIONS_LIMIT_MAX}"),
            ));
        }
        if let Some(w) = self.wait
            && w > PERSON_NOTIFICATIONS_WAIT_MAX_S
        {
            return Err(ValidationError::invalid(
                "wait",
                format!("wait must be at most {PERSON_NOTIFICATIONS_WAIT_MAX_S} seconds"),
            ));
        }
        Ok(())
    }
}

unit_enum! {
    /// What a person notification is about.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum PersonNotificationKind {
        /// A DM or group-DM call is ringing the person (a loud ring only: a busy, DND or muted
        /// ring is silent and not here). Dropped once the ring is answered, declined or missed.
        CallRing = "call_ring",
        /// A DM call rang the person and nobody answered.
        CallMissed = "call_missed",
        /// A mention: the person, a role or group they are in, `@channel` or `@here`.
        Mention = "mention",
        /// A message in a DM or group DM.
        Dm = "dm",
        /// A reply in a thread the person follows (started, replied in or followed).
        ReplyToMe = "reply_to_me",
    }
}

/// One item of the person feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PersonNotification {
    /// The member-log seq (the cursor).
    pub seq: u64,
    /// What it is about.
    pub kind: PersonNotificationKind,
    /// When it was written.
    pub at: Timestamp,
    /// The Space of a Space channel; absent for DMs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<Id>,
    /// The channel (the DM for calls).
    pub channel_id: Id,
    /// The message (the call record for calls).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<Id>,
    /// The call, for `call_ring` and `call_missed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<Id>,
    /// Who caused it: the message's author, or the call's starter.
    pub from_member_id: Id,
    /// The App the Space is bound to; `null` for an unbound Space and for DMs.
    pub app_id: Option<String>,
    /// Since 0.8.0 (flag `app_participant_links`): the Space's participant Apps (sorted), so
    /// K2 can route the push to an App the person uses; absent when none, and for DMs.
    /// `app_id` stays the governing App.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub participant_app_ids: Vec<String>,
    /// The message text cut to 120 chars, only while `notifications.mgmt_preview` is on and
    /// only on message items (never on calls).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// Response of `GET /mgmt/v1/people/{external_id}/notifications`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PersonNotificationsPage {
    /// The items, oldest first.
    pub items: Vec<PersonNotification>,
    /// The next `after`: the seq of the last log row looked at (returned or skipped). Equal
    /// to `after` while DND holds the feed.
    pub next: u64,
}

/// Details of `410 cursor_expired`: the entries after the cursor were pruned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CursorExpiredDetails {
    /// The oldest cursor the feed still answers: resume with `after = oldest_seq` (anything
    /// at or below it is gone).
    pub oldest_seq: u64,
    /// The log head at the read.
    pub head: u64,
}

/// The `preview` of `body`: at most [`PERSON_NOTIFICATION_PREVIEW_MAX_CHARS`] chars, `None`
/// for an empty body.
pub fn preview_of(body: &str) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    Some(
        body.chars()
            .take(PERSON_NOTIFICATION_PREVIEW_MAX_CHARS)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_cut_at_a_char_boundary() {
        let s = "é".repeat(300);
        let p = preview_of(&s).unwrap();
        assert_eq!(p.chars().count(), 120);
        assert_eq!(p.len(), 240);
        assert_eq!(preview_of("hi").as_deref(), Some("hi"));
        assert_eq!(preview_of(""), None);
    }

    #[test]
    fn query_bounds() {
        let q = |limit, wait| PersonNotificationsQuery {
            after: 0,
            limit,
            wait,
        };
        assert!(q(None, None).validate().is_ok());
        assert!(q(Some(500), Some(30)).validate().is_ok());
        assert!(q(Some(0), None).validate().is_err());
        assert!(q(Some(501), None).validate().is_err());
        assert!(q(None, Some(31)).validate().is_err());
        assert_eq!((q(None, None).limit(), q(None, None).wait_s()), (100, 0));
    }
}
