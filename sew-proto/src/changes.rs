//! Change-log kinds and notification reasons.
//!
//! Stored as `smallint`; the numbers are frozen. New kinds are only ever appended.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

coded_enum! {
    /// What changed in a channel's change log (`channel_changes.kind`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum ChannelChangeKind {
        /// A message was posted.
        MessageCreated = "message_created" => 1,
        /// A message was edited.
        MessageUpdated = "message_updated" => 2,
        /// A message was deleted.
        MessageDeleted = "message_deleted" => 3,
        /// A reaction was added or removed.
        ReactionChanged = "reaction_changed" => 4,
        /// A message was pinned or unpinned.
        PinChanged = "pin_changed" => 5,
        /// A thread's summary changed.
        ThreadUpdated = "thread_updated" => 6,
        /// Channel settings changed.
        ChannelUpdated = "channel_updated" => 7,
        /// A member joined.
        MemberJoined = "member_joined" => 8,
        /// A member left.
        MemberLeft = "member_left" => 9,
        /// A message's attachments changed.
        AttachmentChanged = "attachment_changed" => 10,
        /// A poll changed.
        PollUpdated = "poll_updated" => 11,
        /// Voice state changed (registered; unused in v1).
        VoiceStateChanged = "voice_state_changed" => 12,
        /// A channel bookmark changed.
        BookmarkChanged = "bookmark_changed" => 13,
        /// A message's link previews changed.
        PreviewChanged = "preview_changed" => 14,
        /// A meeting row or its attendees changed (`entity_id` = the meeting id).
        MeetingUpdated = "meeting_updated" => 15,
        /// A meeting's link was made, rotated or revoked (`entity_id` = the link
        /// row id). Hosts read the link with `GET /meetings/{id}/link`; the secret is never in
        /// the log.
        MeetingLinkChanged = "meeting_link_changed" => 16,
    }
}

coded_enum! {
    /// What changed in a member's own change log (`member_changes.kind`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum MemberChangeKind {
        /// The member joined a channel.
        ChannelJoined = "channel_joined" => 1,
        /// The member left a channel.
        ChannelLeft = "channel_left" => 2,
        /// The member joined a space.
        SpaceJoined = "space_joined" => 3,
        /// The member left a space.
        SpaceLeft = "space_left" => 4,
        /// A read cursor moved.
        ReadStateMoved = "read_state_moved" => 5,
        /// Preferences changed.
        PrefsChanged = "prefs_changed" => 6,
        /// Effective permissions changed.
        PermsChanged = "perms_changed" => 7,
        /// A notification was written; the entry carries a reason, not an entity.
        Notification = "notification" => 8,
        /// A draft changed.
        DraftChanged = "draft_changed" => 9,
        /// A saved item changed.
        SavedChanged = "saved_changed" => 10,
        /// A reminder fired.
        ReminderFired = "reminder_fired" => 11,
        /// The member's profile changed.
        ProfileChanged = "profile_changed" => 12,
        /// A thread was followed or unfollowed.
        ThreadFollowChanged = "thread_follow_changed" => 13,
        /// Voice mute/deafen flags changed.
        VoiceFlagsChanged = "voice_flags_changed" => 14,
        /// A reminder was created, edited or removed.
        ReminderChanged = "reminder_changed" => 15,
        /// A scheduled message changed.
        ScheduledChanged = "scheduled_changed" => 16,
        /// A thread read cursor moved.
        ThreadReadMoved = "thread_read_moved" => 17,
        /// A DM call rang the member loudly (`entity_id` = the call id, `ref_id` = the
        /// call-record message). Read only by the mgmt person notification feed; the client
        /// change feed never returns it.
        CallRinging = "call_ringing" => 18,
        /// A Space the member is in went private or public (`entity_id` = the Space);
        /// the client reads the Space again.
        SpaceUpdated = "space_updated" => 19,
    }
}

coded_enum! {
    /// Why a member was notified; stored in `member_changes.entity_id` for kind `notification`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum NotificationReason {
        /// A direct `@member` mention.
        Mention = "mention" => 1,
        /// A role mention.
        RoleMention = "role_mention" => 2,
        /// A user-group mention.
        GroupMention = "group_mention" => 3,
        /// `@channel` (or an alias).
        ChannelMention = "channel_mention" => 4,
        /// `@here`.
        HereMention = "here_mention" => 5,
        /// A DM or group DM message.
        Dm = "dm" => 6,
        /// A reply in a followed thread.
        ThreadReply = "thread_reply" => 7,
        /// A keyword match.
        Keyword = "keyword" => 8,
        /// Notify-on-all-messages.
        AllMessages = "all_messages" => 9,
        /// A reminder came due.
        Reminder = "reminder" => 10,
        /// A missed DM call.
        CallMissed = "call_missed" => 11,
        /// A call transcript is ready.
        TranscriptReady = "transcript_ready" => 12,
    }
}
