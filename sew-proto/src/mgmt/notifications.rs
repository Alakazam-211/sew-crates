//! The person notification feed over mgmt: K2 long-polls it per App user and pushes
//! rings, missed calls and mentions to the person's phone (K2's `push.send`).
//!
//! - `GET /mgmt/v1/people/{external_id}/notifications?after=&limit=&wait=`
//!   ([`PersonNotificationsQuery`] → [`PersonNotificationsPage`]): the person's member-log
//!   entries after `after` that Sew's notify rules let through, as [`PersonNotification`]s
//!   ([`PersonNotificationKind`]: rings, missed calls, mentions, DMs, replies, channels at
//!   `all`, keywords; since flag [`FEATURE_FEED_ALL_MESSAGES`] also `read` markers and
//!   `call_ring_stopped`, which clear pushes; since flag [`FEATURE_VOICE_STARTED_FEED`]
//!   `voice_started` and its clear `voice_ended`; since flag [`FEATURE_VOICE_INVITES`]
//!   `voice_invite` and its clear `voice_invite_stopped`). The cursor is the member-log `seq`: it survives
//!   restarts. A cursor behind the log's floor (pruned) is `410 cursor_expired` with
//!   [`CursorExpiredDetails`].
//! - Persons only: an agent's external id is `404` (agents keep
//!   `GET /mgmt/v1/agents/{member_id}/deliveries`).
//! - `GET /mgmt/v1/people/{external_id}/channels/{id}/notify` and `POST …/notify/set`: the
//!   person's level and mute in one channel or DM, with the person's own route's types
//!   ([`crate::api::notify::ChannelNotifyPrefs`], [`crate::api::notify::SetChannelNotifyRequest`])
//!   and rules (flag [`FEATURE_PERSON_CHANNEL_NOTIFY`]).
//! - No message text unless the install policy `notifications.mgmt_preview` is on; then a
//!   message item carries `preview`, at most [`PERSON_NOTIFICATION_PREVIEW_MAX_CHARS`] chars.
//! - Since flag [`FEATURE_FEED_ATTACHMENT_INFO`], a message item with files carries
//!   `attachment_count` and `attachment_kinds` (never names, sizes, URLs or ids), so a push for
//!   a message with files and no text can say what was sent.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::files::FileKind;
use crate::error::{Validate, ValidationError};
use crate::id::Id;
use crate::time::Timestamp;

/// The feature flag of this feed (`features` in `GET /mgmt/v1/status`).
pub const FEATURE_PERSON_NOTIFICATIONS: &str = "person_notifications";
/// The feature flag of `GET /mgmt/v1/people/{external_id}/notifications/head` (where a new feed
/// consumer starts).
pub const FEATURE_FEED_HEAD: &str = "feed_head";
/// The feature flag of the feed's second version: the kinds `message`, `keyword`, `read` and
/// `call_ring_stopped`, inline replies as `reply_to_me`, `from_external_id` and
/// `thread_root_id` on items, and `head` on pages.
pub const FEATURE_FEED_ALL_MESSAGES: &str = "feed_all_messages";
/// The feature flag of `GET /mgmt/v1/people/{external_id}/channels/{id}/notify` and
/// `POST …/notify/set`: a person's level and mute per channel or DM, read and set over mgmt.
pub const FEATURE_PERSON_CHANNEL_NOTIFY: &str = "person_channel_notify";
/// The feature flag of the voice room start on the person feed: the kinds `voice_started` and
/// `voice_ended`, and the client event `voice_started`.
pub const FEATURE_VOICE_STARTED_FEED: &str = "voice_started_feed";
/// The feature flag of voice room invites: `POST /api/v1/channels/{id}/voice/invite`, its
/// decline and cancel, `GET /api/v1/me/voice/invites`, the client event `voice_invite`,
/// `ReadyChannel.voice_invites` and the feed kinds `voice_invite` and `voice_invite_stopped`.
pub const FEATURE_VOICE_INVITES: &str = "voice_invites";
/// Since 0.11.5: the feature flag of `attachment_count` and `attachment_kinds` on the feed's
/// message items.
pub const FEATURE_FEED_ATTACHMENT_INFO: &str = "feed_attachment_info";
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
    /// What a person notification is about. New kinds are added over time: treat an unknown
    /// kind (`Unknown`) as nothing to push.
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
        /// A reply to the person: in a thread they follow (started, replied in or followed),
        /// or an inline reply to one of their messages.
        ReplyToMe = "reply_to_me",
        /// A message in a channel the person set to `all` (reason `all_messages`).
        Message = "message",
        /// A message with one of the person's keywords.
        Keyword = "keyword",
        /// The person read a channel, DM or thread (on any device) up to `message_id`: clear
        /// the pushes there up to it. With `thread_root_id`, the thread's pushes; without,
        /// the channel's own (not its threads'). Never for the person's own posts.
        Read = "read",
        /// A loud ring the person got (`call_ring`) stopped without being missed: answered,
        /// declined, or the call cancelled. Clear the ring. A missed ring is `call_missed`.
        CallRingStopped = "call_ring_stopped",
        /// A voice room in a channel the person set to `all` came alive: someone joined it
        /// while it was empty and stayed (`from_member_id`). No `message_id`.
        VoiceStarted = "voice_started",
        /// The `voice_started` push of `channel_id` no longer applies (the room went quiet, or
        /// the person joined it): clear it. At most one room session per channel is live, so
        /// the channel is enough.
        VoiceEnded = "voice_ended",
        /// A person in a voice room invites the person in (`from_member_id`, `invite_id`,
        /// `expires_at`). Accepting is joining the room; declining is
        /// `POST /api/v1/channels/{id}/voice/invite/decline` with the person's own session.
        VoiceInvite = "voice_invite",
        /// A loud `voice_invite` stopped (accepted, declined, cancelled, expired, the inviter
        /// left, the room ended or the invite was revoked): clear it (`invite_id`).
        VoiceInviteStopped = "voice_invite_stopped",
    }
}

impl PersonNotificationKind {
    /// Whether this kind only clears earlier pushes (`read`, `call_ring_stopped`,
    /// `voice_ended`, `voice_invite_stopped`): DND does not hold these.
    pub const fn clears(self) -> bool {
        matches!(
            self,
            PersonNotificationKind::Read
                | PersonNotificationKind::CallRingStopped
                | PersonNotificationKind::VoiceEnded
                | PersonNotificationKind::VoiceInviteStopped
        )
    }
}

/// One item of the person feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
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
    /// The channel (the DM for calls; the voice channel for the voice kinds).
    pub channel_id: Id,
    /// The message (the call record for calls).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<Id>,
    /// The call, for `call_ring` and `call_missed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<Id>,
    /// Who caused it: the message's author, the call's starter, the voice room session's
    /// starter (`voice_started`, `voice_ended`) or the inviter (`voice_invite`,
    /// `voice_invite_stopped`); the person themself for `read`.
    pub from_member_id: Id,
    /// The external id of `from_member_id` (K2 resolves the name from it); absent for `read`
    /// and when that member has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_external_id: Option<String>,
    /// The thread's root when the message is a thread reply (a tap opens the thread), and on
    /// a `read` of a thread; absent for top-level messages and calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_root_id: Option<Id>,
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
    /// The voice invite, on `voice_invite` and `voice_invite_stopped` (flag `voice_invites`);
    /// an item from an older server has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_id: Option<Id>,
    /// When the invite expires, on `voice_invite` (the push's expiry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    /// Since flag `feed_attachment_info`: how many files the message has, on message items
    /// only; absent (`0`) when it has none, on other kinds, and from an older server.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub attachment_count: u32,
    /// Since flag `feed_attachment_info`: the distinct kinds of the message's files, in the
    /// order they first appear among its files; absent when it has none, on other kinds, and
    /// from an older server. No file names, sizes, URLs or ids are on the feed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_kinds: Vec<FileKind>,
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

impl PersonNotification {
    /// A `PersonNotification` with its required fields: `seq` 0, every list empty and every
    /// optional field unset (set them on the value).
    pub fn new(
        kind: PersonNotificationKind,
        at: Timestamp,
        channel_id: Id,
        from_member_id: Id,
    ) -> Self {
        PersonNotification {
            seq: 0,
            kind,
            at,
            space_id: None,
            channel_id,
            message_id: None,
            call_id: None,
            from_member_id,
            from_external_id: None,
            thread_root_id: None,
            app_id: None,
            participant_app_ids: Vec::new(),
            preview: None,
            invite_id: None,
            expires_at: None,
            attachment_count: 0,
            attachment_kinds: Vec::new(),
        }
    }
}

/// Response of `GET /mgmt/v1/people/{external_id}/notifications`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct PersonNotificationsPage {
    /// The items, oldest first.
    pub items: Vec<PersonNotification>,
    /// The next `after`: the seq of the last log row looked at (returned or skipped). Equal
    /// to `after` while DND holds the feed and nothing before the first held item clears.
    pub next: u64,
    /// The person's member-log head at the read (`next == head`: caught up). Since flag
    /// `feed_all_messages`; a page from an older server has none and reads `0` (unknown).
    #[serde(default)]
    pub head: u64,
}

impl PersonNotificationsPage {
    /// A page (`PersonNotificationsPage::default()` is the empty page at 0).
    pub fn new(items: Vec<PersonNotification>, next: u64, head: u64) -> Self {
        PersonNotificationsPage { items, next, head }
    }
}

/// Response of `GET /mgmt/v1/people/{external_id}/notifications/head`: the person's member-log
/// head now, so a new consumer can start the feed there (`after = head`) and push only what
/// comes next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct PersonNotificationsHead {
    /// The member-log head seq (`0` for an empty log).
    pub head: u64,
}

impl PersonNotificationsHead {
    /// A head answer.
    pub fn new(head: u64) -> Self {
        PersonNotificationsHead { head }
    }
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
// The package has no clippy.toml (Sew's allows unwrap in tests); the module says it itself.
#[allow(clippy::unwrap_used)]
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
    fn kind_discriminants_are_fixed() {
        use PersonNotificationKind as K;
        // `Unknown` has the fixed discriminant; the known kinds count up from 0 in wire order.
        assert_eq!(K::Unknown as isize, crate::registry::UNKNOWN_DISCRIMINANT);
        assert_eq!(
            [K::CallRing, K::CallMissed, K::Mention, K::Dm, K::ReplyToMe].map(|k| k as isize),
            [0, 1, 2, 3, 4]
        );
        assert_eq!(
            [K::Message, K::Keyword, K::Read, K::CallRingStopped].map(|k| k as isize),
            [5, 6, 7, 8]
        );
        assert_eq!(
            [
                K::VoiceStarted,
                K::VoiceEnded,
                K::VoiceInvite,
                K::VoiceInviteStopped
            ]
            .map(|k| k as isize),
            [9, 10, 11, 12]
        );
        assert!(K::Read.clears() && K::CallRingStopped.clears());
        assert!(K::VoiceEnded.clears() && K::VoiceInviteStopped.clears());
        assert!(!K::Mention.clears() && !K::Unknown.clears());
        assert!(!K::VoiceStarted.clears() && !K::VoiceInvite.clears());
        let k: K = serde_json::from_str("\"voice_invite\"").unwrap();
        assert_eq!(k, K::VoiceInvite);
        let k: K = serde_json::from_str("\"voice_missed\"").unwrap();
        assert_eq!(k, K::Unknown);
    }

    #[test]
    fn page_from_an_older_server_reads() {
        // A server without flag `feed_all_messages` sends no `head`, and items without
        // `from_external_id` or `thread_root_id`.
        let old = r#"{"items": [{"seq": 815, "kind": "mention", "at": "2026-09-27T12:00:01.500Z",
            "channel_id": "48831765147549710", "message_id": "48831765147549800",
            "from_member_id": "48831765147549703", "app_id": null}], "next": 818}"#;
        let page: PersonNotificationsPage = serde_json::from_str(old).unwrap();
        assert_eq!((page.next, page.head), (818, 0));
        assert_eq!(page.items.len(), 1);
        let item = &page.items[0];
        assert_eq!(item.kind, PersonNotificationKind::Mention);
        assert_eq!((&item.from_external_id, item.thread_root_id), (&None, None));
        let empty: PersonNotificationsPage =
            serde_json::from_str(r#"{"items": [], "next": 77}"#).unwrap();
        assert_eq!(empty, PersonNotificationsPage::new(Vec::new(), 77, 0));
    }

    #[test]
    fn item_wire_shape() {
        let mut i = PersonNotification::new(
            PersonNotificationKind::Read,
            Timestamp::from_unix_ms_saturating(0),
            Id::from_i64(2).unwrap(),
            Id::from_i64(3).unwrap(),
        );
        i.seq = 9;
        let v = serde_json::to_value(&i).unwrap();
        for absent in ["from_external_id", "thread_root_id", "preview", "space_id"] {
            assert!(v.get(absent).is_none(), "{v}");
        }
        assert!(v["app_id"].is_null(), "{v}");
        i.thread_root_id = Id::from_i64(5);
        i.from_external_id = Some("local:bob".into());
        let v = serde_json::to_value(&i).unwrap();
        assert_eq!(v["thread_root_id"], "5");
        assert_eq!(v["from_external_id"], "local:bob");
        let back: PersonNotification = serde_json::from_value(v).unwrap();
        assert_eq!(back, i);
    }

    #[test]
    fn voice_items_wire_shape() {
        // An invite item carries `invite_id` and `expires_at`; an item from an older server
        // (no such keys) reads with both unset.
        let mut i = PersonNotification::new(
            PersonNotificationKind::VoiceInvite,
            Timestamp::from_unix_ms_saturating(1_000),
            Id::from_i64(2).unwrap(),
            Id::from_i64(3).unwrap(),
        );
        i.seq = 903;
        i.space_id = Id::from_i64(7);
        i.invite_id = Id::from_i64(9);
        i.expires_at = Some(Timestamp::from_unix_ms_saturating(301_000));
        let v = serde_json::to_value(&i).unwrap();
        assert_eq!(v["kind"], "voice_invite");
        assert_eq!(v["invite_id"], "9");
        assert_eq!(v["expires_at"], "1970-01-01T00:05:01.000Z");
        for absent in ["message_id", "call_id", "thread_root_id", "preview"] {
            assert!(v.get(absent).is_none(), "{v}");
        }
        let back: PersonNotification = serde_json::from_value(v).unwrap();
        assert_eq!(back, i);
        let old = r#"{"seq": 5, "kind": "mention", "at": "2026-09-27T12:00:01.500Z",
            "channel_id": "2", "from_member_id": "3", "app_id": null}"#;
        let old: PersonNotification = serde_json::from_str(old).unwrap();
        assert_eq!((old.invite_id, old.expires_at), (None, None));
        let started = PersonNotification::new(
            PersonNotificationKind::VoiceStarted,
            Timestamp::from_unix_ms_saturating(0),
            Id::from_i64(2).unwrap(),
            Id::from_i64(3).unwrap(),
        );
        let v = serde_json::to_value(&started).unwrap();
        for absent in ["invite_id", "expires_at", "message_id"] {
            assert!(v.get(absent).is_none(), "{v}");
        }
    }

    #[test]
    fn attachment_info_wire_shape() {
        // A message item with files carries the count and the distinct kinds; without files
        // (and from an older server) neither key is there.
        let mut i = PersonNotification::new(
            PersonNotificationKind::Dm,
            Timestamp::from_unix_ms_saturating(0),
            Id::from_i64(2).unwrap(),
            Id::from_i64(3).unwrap(),
        );
        let v = serde_json::to_value(&i).unwrap();
        for absent in ["attachment_count", "attachment_kinds"] {
            assert!(v.get(absent).is_none(), "{v}");
        }
        i.attachment_count = 3;
        i.attachment_kinds = vec![FileKind::Image, FileKind::Document];
        let v = serde_json::to_value(&i).unwrap();
        assert_eq!(v["attachment_count"], 3);
        assert_eq!(
            v["attachment_kinds"],
            serde_json::json!(["image", "document"])
        );
        let back: PersonNotification = serde_json::from_value(v).unwrap();
        assert_eq!(back, i);
        let old = r#"{"seq": 5, "kind": "dm", "at": "2026-09-27T12:00:01.500Z",
            "channel_id": "2", "from_member_id": "3", "app_id": null}"#;
        let old: PersonNotification = serde_json::from_str(old).unwrap();
        assert_eq!(old.attachment_count, 0);
        assert!(old.attachment_kinds.is_empty());
        // A kind a later server adds reads as `Unknown`.
        let new = r#"{"seq": 5, "kind": "dm", "at": "2026-09-27T12:00:01.500Z",
            "channel_id": "2", "from_member_id": "3", "app_id": null,
            "attachment_count": 2, "attachment_kinds": ["model", "image"]}"#;
        let new: PersonNotification = serde_json::from_str(new).unwrap();
        assert_eq!(
            new.attachment_kinds,
            vec![FileKind::Unknown, FileKind::Image]
        );
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
