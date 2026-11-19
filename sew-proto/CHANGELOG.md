# Changelog

## 0.11.0 (2026-11-19)

### Breaking

- **chrono is no longer a dependency, under any feature.** The time types are the crate's own,
  there is no `chrono` feature, and `schemars` is built without its `chrono04` feature. The
  wire forms do not change.
  - `Timestamp` holds Unix milliseconds itself, with the same range (the years -262143 to
    262142, now `Timestamp::MIN` and `Timestamp::MAX`) and the same RFC 3339 input and output.
    `Timestamp::from_datetime` and `Timestamp::as_datetime` are removed, and there is no `From`
    between `Timestamp` and `chrono::DateTime<Utc>`. `from_unix_ms`,
    `from_unix_ms_saturating` and `unix_ms` are `const fn`. Its `Debug` output is
    `Timestamp(2026-09-27T12:00:00.123Z)`.
  - New `time::Date` (a calendar day, `YYYY-MM-DD`, years 0–9999: `from_ymd`, `parse`,
    `year`, `month`, `day`, `succ`) and `time::LocalDateTime` (a wall-clock minute,
    `YYYY-MM-DDTHH:MM`: `new`, `parse`, `date`, `hour`, `minute`), with no chrono conversions
    (no `to_naive_date`, `from_naive_date` or `to_naive_date_time`, and no `From` into
    `NaiveDate` or `NaiveDateTime`).
  - `search::query::DateFilter`'s `on`, `before` and `after`, `DateFilter::allows` and
    `search::query::parse_date` use `time::Date` instead of `chrono::NaiveDate`.
  - `When::local_date_time` returns `Option<time::LocalDateTime>` instead of
    `Option<chrono::NaiveDateTime>`. It reads only the strict `YYYY-MM-DDTHH:MM` (four-digit
    year, two-digit fields), the form `When::validate` documents.
  - Migrating: convert through Unix milliseconds and the calendar fields, with chrono, jiff or
    any other time crate. With chrono:

    ```rust,ignore
    // Timestamp -> DateTime<Utc> (None only outside chrono's range)
    let dt = chrono::DateTime::from_timestamp_millis(ts.unix_ms());
    // DateTime<Utc> -> Timestamp (truncated to milliseconds)
    let ts = sew_proto::Timestamp::from_unix_ms(dt.timestamp_millis());
    // Date -> NaiveDate, and back
    let nd = chrono::NaiveDate::from_ymd_opt(d.year(), d.month(), d.day());
    let d = sew_proto::time::Date::from_ymd(nd.year(), nd.month(), nd.day());
    // LocalDateTime -> NaiveDateTime
    let ndt = nd.and_hms_opt(t.hour(), t.minute(), 0);
    ```

- **`Unknown` has one fixed discriminant.** In every wire unit enum, `Unknown` (the fallback for
  a value this build does not know) is now `registry::UNKNOWN_DISCRIMINANT` (`0xFFFF`), far
  above the known variants, which count up from 0 in wire order. Adding a variant no longer
  moves `Unknown`'s number, so later releases can add variants without changing it. Code that
  casts these enums with `as` sees `Unknown` change from one past the last variant to
  `0xFFFF`; the enums are two bytes wide where they were one. In `mgmt::StatusAlarm`,
  `gateway::events::EventName` and `audit::Action`, the variants that followed the old pinned `Unknown` move down
  by one (`StatusAlarm::KeysMirrorUnmounted` 42 to 41, `KeysMirrorSameDevice` 43 to 42,
  `KeysMirrorUnsynced` 44 to 43; `EventName::DmSeen` 68 to 67,
  `ReadReceiptSettingsUpdated` 69 to 68, `ReadReceiptsPolicyUpdated` 70 to 69; `audit::Action`:
  `NotifyPrefsSet` 212 to 211). The `coded_enum!` enums (`MemberChangeKind`,
  `NotificationReason` and the others below) are covered the same way. The wire
  strings, `ALL` and the storage codes (`as_i16`) do not change. The enums (160):
    - `api::channels`: `ChannelListScope`, `ChannelType`, `MyChannelsKind`, `MyChannelsOrder`,
      `Visibility`.
    - `api::compliance`: `CreatedVia`, `HoldStateFilter`, `PolicyState`, `RunState`,
      `RunTrigger`, `VanishReason`.
    - `api::exports`: `ExportState`, `RequestedVia`.
    - `api::extras`: `PollResultsVisibility`, `PollState`, `ReminderState`, `SavedState`,
      `ScheduledFailReason`, `ScheduledState`.
    - `api::files`: `FileKind`.
    - `api::media`: `LinkPreviewKind`, `SecretSetVia`.
    - `api::meetings`: `MeetingAdmissionOutcome`, `MeetingAttendeeKind`, `MeetingEndReason`,
      `MeetingKind`, `MeetingListScope`, `MeetingLobbyKind`, `MeetingLobbyState`,
      `MeetingRedeemState`, `MeetingState`.
    - `api::members`: `AgentKind`, `InstallRole`, `MemberKind`, `SettableInstallRole`.
    - `api::messages`: `DeletedReason`, `OnBehalfApprovalState`, `ReferenceKind`.
    - `api::notify`: `DigestDetail`, `DigestInterval`, `NotifyLevel`.
    - `api::people`: `DndDay`, `DndSource`, `ManualPresence`, `MentionableKind`,
      `ProfileFieldKind`, `ProfileFieldManagedBy`, `ProfileFieldVisibility`, `ResettableField`,
      `SelfStatus`.
    - `api::read_state`: `ThreadsInboxFilter`.
    - `api::search`: `SearchErrorReason`, `SearchSort`.
    - `api::spaces`: `OverwriteTarget`, `SpaceGovernance`, `SpaceJoinPolicy`, `SpaceLevel`,
      `SpaceListScope`.
    - `audit`: `Action`, `ActorKind`, `AuditResult`, `AuditVisibility`.
    - `calls`: `CallEndReason`, `CallState`, `RingState`.
    - `changes`: `ChannelChangeKind`, `MemberChangeKind`, `NotificationReason`.
    - `e2ee::device`: `DevicePlatform`, `DeviceRevokeReason`.
    - `e2ee::wire`: `ConsentState`, `InvalidReason`, `KeyResyncReason`, `KeyStateStatus`,
      `RekeyReason`, `RemoveReason`, `ReportKind`, `ResetReason`, `TransitionReason`,
      `VerificationCode`.
    - `error`: `Code`, `InvalidStateReason`, `PermissionReason`, `ResyncReason`.
    - `export`: `DeleteKind`, `ExportKind`, `FileMode`, `MemberMode`, `RemoveKind`, `RunEnd`,
      `SegmentState`.
    - `gateway`: `Compression`, `Encoding`.
    - `gateway::client`: `PresenceSetStatus`.
    - `gateway::events`: `EventName`, `PresenceStatus`.
    - `mgmt`: `AuditVerifyMode`, `DeliveryKind`, `DeliveryPlace`, `EraseKeptKind`,
      `EraseKeptReason`, `EraseMode`, `EraseState`, `JobState`, `MailTls`, `MediaPathState`,
      `MgmtChannelNotAddedReason`, `MgmtChannelOpKind`, `MgmtMemberChannelsSkip`,
      `MgmtSpaceAppRole`, `MgmtSpaceNotAddedReason`, `MgmtVoiceChangeKind`,
      `PersonNotificationKind`, `PrincipalsInclude`, `ProviderKind`, `SearchState`,
      `StatusAlarm`.
    - `mgmt::local`: `LocalRole`, `LocalSettableRole`.
    - `policy`: `ArchiveWho`, `ChannelCreateFloor`, `GuestInvite`, `ManagedProfileField`,
      `MeetingsCreate`, `PolicyKey`, `SpacesCreate`.
    - `recording`: `AudioState`, `Capture`, `Consent`, `ConsentSurface`, `ConsentVia`,
      `DeclineMode`, `DownloadWhat`, `GapReason`, `NotRecordedReason`, `Playback`,
      `PromptRoomKind`, `RecordUnavailable`, `RecordingEventKind`, `RecordingRequiredPolicy`,
      `RecordingRule`, `RecordingStartLevel`, `RecordingState`, `RecordingTrigger`,
      `StopReason`, `TranscriptFailureCode`, `TranscriptState`.
    - `search::query`: `FilterName`, `HasValue`, `IsValue`.
    - `voice`: `BudgetSource`, `CallStatsCandidate`, `CallStatsClient`,
      `CallStatsRelayProtocol`, `KeyJoinMode`, `MediaTier`, `VideoCodec`, `VoiceConnection`,
      `VoiceEngineState`, `VoiceLeaveReason`.

- **Every wire unit enum is `#[non_exhaustive]`.** The 160 enums listed above (every
  `unit_enum!` and `coded_enum!`) are `#[non_exhaustive]`, so a later release can add a value
  without a breaking change. A `match` on one outside this crate needs a wildcard arm (`_`),
  usually the same arm as `Unknown`. The wire forms, `ALL` and the storage codes do not change
  (the discriminant moves are listed above). Each already had its `serde(other)` `Unknown`.

- **The transcript failure reason moves inside the recording summary.**
  `SummaryTranscript.failure` (`Option<TranscriptFailure>`, skipped when `None`) carries why a
  transcript is `failed` or `partial`: the record message's `recording.transcript.failure`,
  e.g. `{"code": "transcriber_error", "message": "The transcription service could not process
  the audio."}`. `RecordingSummary`, `SummaryAudio` and `SummaryTranscript` are
  `#[non_exhaustive]`, with `RecordingSummary::new` (recording, starter, start time, audio,
  transcript), `SummaryAudio::new` (state, mixed, keep days) and `SummaryTranscript::new`
  (state); set the other fields on the value.
- **Deprecated:** `Message.transcript_failure` and `MgmtRecordingSummary.transcript_failure`
  (`#[deprecated]`). The server still writes both, with the same value as
  `recording.transcript.failure`; they are kept for at least one more release, then removed in a
  later breaking release.

- **`PolicyKey` is `#[non_exhaustive]` and names `dm.read_receipts`.** It gains
  `PolicyKey::DmReadReceipts` (`dm.read_receipts`), so the policy is listed, validated and
  refused (`policy_denied`) like every other key; a later key is an additive change.
  `policy::DM_READ_RECEIPTS` stays, as that variant's wire string. `PolicyDigest` gains
  `dm_read_receipts` (old payloads read `true`), so READY's `ReadyInstall.policy_digest` and
  `install_updated` carry the install policy.

- `mgmt::KeysMirrorSyncResponse` is `#[non_exhaustive]`, with `KeysMirrorSyncResponse::new`
  (mirror kind, ok); a later field is an additive change.

- **The structs that grow are `#[non_exhaustive]`.** A later field on any of these is an
  additive change. Outside this crate they can no longer be built with a struct expression or
  matched without `..`: build one with its constructor and set the other (public) fields on
  the value, e.g. `let mut s = Space::new(id, slug, "Ops", owner, now); s.private = true;`.
  Each `new` takes the required fields in the order given and leaves every optional field
  unset, every list empty, every count 0 and every flag `false` (each type's doc says which).
  The wire forms do not change.
  - READY and sync: `gateway::server::Ready::new` (session id, resume URL, member, install,
    threads inbox; protocol `PROTOCOL_VERSION`), `ReadySpace::new` (id, slug, name,
    versions), `api::read_state::ReadState::new` (channel id), `ThreadReadState::new` (root
    id), `MeSync::new` (install id, sync epoch, member id, threads inbox).
    `policy::PolicyDigest` implements `Default` (the digest of `Policies::default()`);
    `Policies::digest` still builds one from the install's policies.
  - `mgmt::Status` parts: `StatusIdentity::new` (provider), `StatusSearchRebuild::new`
    (reason, start time), `StatusKeysMirror::new` (kind), `StatusGateway::new` (identify,
    resume counters), `e2ee::StatusVoiceE2eeCall::new` (call id, leaves by state),
    `voice::VoiceStatus::new` (engine), `VoiceEngineStatus::new` (state),
    `VoiceMediaStatus::new` (media tier, budget source); `StatusDb`, `StatusJobs`,
    `StatusBackup`, `StatusDrill`, `StatusSearch`, `StatusSearchDocs`, `StatusKeys`,
    `StatusGatewayIdentify`, `StatusGatewayResume`, `StatusGatewayReauth`, `StatusMail`,
    `StatusDigest`, `StatusFiles`, `StatusErasures`, `e2ee::StatusVoiceE2ee`,
    `StatusVoiceE2eeLeaves`, `api::exports::StatusExports`,
    `api::compliance::StatusRetention` and `recording::StatusRecording` through `Default`.
    `Status::new` is unchanged.
  - Mgmt: `DeliveryMessage::new` (id, channel id, author id, author display name, author
    kind, addr, body, created at), `MgmtPrincipal::new` (member id, external id, kind, install
    role, username, display name, created at), `PersonNotification::new` (kind, at, channel
    id, from member id), `PersonNotificationsPage` and `MgmtEraseCounts` through `Default`,
    `MgmtChannelCreateRequest::new` (space id, slug, name), `MgmtChannelCreateResponse::new`
    (channel id, space id, slug, name; type `text`), `MgmtMemberChannelsRequest::new` (level),
    `MgmtMemberChannelsItem::new` (external id, level).
  - Client API: `api::spaces::Space::new` (id, slug, name, owner id, created at),
    `api::messages::Attachment::new` (id, filename, content type, url),
    `api::meetings::Meeting::new` (id, channel id, space id, organiser, kind, state, starts
    at, ends at, time zone, opens at, hard end at, ICS UID, created at, updated at).
  - Voice and calls: `voice::VoiceState::new` (member id, channel id, connection, joined at),
    `VoiceJoinResponse::new` (url, token, expires at, room, state), `CallStatsSample::new`
    (client), `calls::Call::new` (id, channel id, message id, started by, started at, state).
  - Call keys: `e2ee::E2eeCaps::new` (kdf, SDK, SDK version), `CallKeysParams::new` (kdf,
    exporter label, release info label), `CallKeysInit::new` (call id, join id, LiveKit
    identity, scope, group id, external sender, params), `ReadyCallKeys::new` (external
    sender, DS public key, CA, verification code, params).
  - Export records (every `export::*Record`): `InstallRecord::new` (install id, name, created
    at), `ProfileFieldRecord::new` (id, key, label, kind, managed by, visibility, created at,
    updated at), `StatusRecord::new` (set at), `AvatarRecord::new` (file, avatar id, source),
    `MemberRecord::new` (id, external id, kind, install role, username, display name, created
    at), `PresenceSettingsRecord::new` (member id, manual, updated at), `IconRecord::new`
    (file, link id, icon id), `RoleRecord::new` (id, space id, name, perms, created at,
    updated at), `SpaceMemberRecord::new` (space id, member id, joined at),
    `UserGroupRecord::new` (id, handle, name, created at, updated at), `OverwriteRecord::new`
    (target kind, target id, allow, deny), `CategoryRecord::new` (id, space id, name, created
    at), `ChannelMemberRecord::new` and `ChannelPastMemberRecord::new` (channel id, member id,
    joined or left at), `SpaceBanRecord::new` (space id, member id, banned at),
    `TimeoutCarryRecord::new` (space id, member id, timeout until), `EmojiRecord::new` (id,
    space id, name, created at, updated at), `ReferenceRecord::new` (kind, message id, channel
    id), `DeletedRecord::new` and `RemovedRecord::new` (at, kind), `PinRecord::new` (by, at),
    `ReactionRecord::new` (emoji), `AttachmentRecord::new` (id, uploader id, content type,
    kind, created at), `PollOptionRecord::new` (id), `PollRecord::new` (results visibility,
    created at), `MessageRecord::new` (id, channel id, author id, created at),
    `RevisionRecord::new` (id, message id, channel id, created at, replaced at),
    `BookmarkRecord::new` (id, channel id, created by, created at, updated at),
    `ThreadFollowRecord::new` (member id, root id, channel id, state, reason, followed at),
    `ReadStateRecord::new` (member id, channel id, updated at), `ThreadReadRecord::new`
    (member id, root id, channel id, last read id, updated at), `MentionRecord::new` (member
    id, channel id, message id, created at), `SavedItemRecord::new` (id, member id, message
    id, channel id, state, created at, updated at, state changed at), `ReminderRecord::new`
    (id, member id, at, state, created at, updated at), `DraftRecord::new` (id, member id,
    channel id, body, created at, updated at), `ScheduledRecord::new` (id, member id, channel
    id, send at, state, created at, updated at), `NotifyPrefsRecord::new` (member id),
    `NotifySettingsRecord::new` (digest interval, updated at), `NotifyChannelPrefRecord::new`
    (channel id, updated at), `NotifySpacePrefRecord::new` (space id, level, updated at),
    `AuditRecord::new` (event JSON), `RecordingParticipantRecord::new` (member id, consent,
    first at, updated at), `RecordingRunRecord::new` (started at), `RecordingTrackRecord::new`
    (member id), `RecordingGapRecord::new` (reason), `RecordingSegmentRecord::new` (id, member
    id, join id, state, created at), `TranscriptTurnRecord::new` (message id, speaker id,
    spoken at, pieces); `MentionsRecord` and `RecordingMixRecord` through `Default`.
    `SpaceRecord`, `ChannelRecord`, `DmSeenRecord`, `ReadReceiptPrefRecord` and
    `RecordingRecord` were already `#[non_exhaustive]` with constructors.
  - `voice::ReadyVoiceChannel` (READY's `voice_channels`): `ReadyVoiceChannel::new` (id, Space
    id, slug, name, position, my perms; no category, user limit 0). It gains `voice_invites`
    (below).

### Added

- **The person notification feed's second version** (`mgmt::notifications`, flag
  `feed_all_messages`, `mgmt::FEATURE_FEED_ALL_MESSAGES` in `mgmt::FEATURES`). With the enums
  and structs above `#[non_exhaustive]`, these are additions:
  - `PersonNotificationKind` gains `Message` (`message`, a channel at `all`), `Keyword`
    (`keyword`), `Read` (`read`: the person read a channel, DM or thread up to `message_id`)
    and `CallRingStopped` (`call_ring_stopped`: a loud ring answered, declined or cancelled),
    numbered 5 to 8 after the five 0.10 kinds. `PersonNotificationKind::clears` says which
    kinds only clear pushes.
  - `PersonNotification` gains `from_external_id` and `thread_root_id` (`Option`, absent when
    unset; `PersonNotification::new` leaves them unset), and `PersonNotificationsPage` gains
    `head` (the log head at the read) and `PersonNotificationsPage::new(items, next, head)`.
  - `changes::MemberChangeKind` gains `ReadMarker` (`read_marker`, code 20),
    `ThreadReadMarker` (`thread_read_marker`, 21) and `CallRingStopped` (`call_ring_stopped`,
    22), log entries only the person feed reads (the client change feed never returns them),
    and `changes::NotificationReason` gains `InlineReply` (`inline_reply`, 13): an inline reply
    to one of the member's messages (people only; it notifies like a role mention and ranks
    after `dm`).
- **Voice room notifications**: a voice room's start and voice invites (flags
  `voice_started_feed` and `voice_invites`, `mgmt::FEATURE_VOICE_STARTED_FEED` and
  `mgmt::FEATURE_VOICE_INVITES` in `mgmt::FEATURES`, also re-exported from the new module
  `voice_notify`). All additions:
  - `voice_notify::VoiceStartedEvent` (the event `voice_started`: `channel_id`, `space_id`,
    `started_by`, `active`, `at`), `VoiceInviteRequest` (`member_ids`, 1–10 distinct,
    `VOICE_INVITE_MEMBERS_MAX`), `VoiceInviteResults` of `VoiceInviteResult` (`member_id`,
    `state` `pending` or `refused` as `VoiceInviteResultState`, `invite_id`, `expires_at`,
    `silent`, `reason`), `VoiceInviteDeclineRequest` and `VoiceInviteCancelRequest`
    (`invite_id`), `VoiceInvite` (the event `voice_invite`, the decline and cancel answers:
    `invite_id`, `channel_id`, `space_id`, `invited_by`, `member_id`, `state`, `reason`,
    `silent`, `recorded`, `expires_at`, `created_at`), `VoiceInviteState` (`pending`,
    `accepted`, `declined`, `cancelled`, `inviter_left`, `expired`, `room_ended`, `revoked`)
    and `MyVoiceInvites` (`incoming`, `outgoing`). The reason strings are in
    `voice_notify::invite_reason` and `voice_notify::reasons` (`invite_not_pending`).
  - `routes::CHANNEL_VOICE_INVITE`, `CHANNEL_VOICE_INVITE_DECLINE`,
    `CHANNEL_VOICE_INVITE_CANCEL` and `ME_VOICE_INVITES`.
  - `gateway::events::EventName` gains `VoiceStarted` (`voice_started`, 70) and `VoiceInvite`
    (`voice_invite`, 71), with `Event::VoiceStarted` and `Event::VoiceInvite`;
    `gateway::server::ReadyChannel` and `voice::ReadyVoiceChannel` gain `voice_invites` (the
    member's incoming pending invites into the room, for a channel they are in and for a
    public room they have not joined; absent when empty).
  - `PersonNotificationKind` gains `VoiceStarted` (`voice_started`), `VoiceEnded`
    (`voice_ended`, clears), `VoiceInvite` (`voice_invite`) and `VoiceInviteStopped`
    (`voice_invite_stopped`, clears), numbered 9 to 12; `PersonNotification` gains `invite_id`
    and `expires_at` (`Option`, absent when unset; an older server's item reads both as
    `None`).
  - `changes::MemberChangeKind` gains `VoiceStarted` (`voice_started`, code 23),
    `VoiceEnded` (`voice_ended`, 24), `VoiceInvited` (`voice_invited`, 25) and
    `VoiceInviteStopped` (`voice_invite_stopped`, 26): log entries only the person feed reads
    (the client change feed never returns them).
  - `audit::Action` gains `VoiceInvite` (`voice.invite`) and `VoiceInviteStop`
    (`voice.invite_stop`), both Space class.

## 0.10.6 (2026-11-19)

### Added

- **Read receipts in DMs and group DMs.** A DM member's seen position (the highest message id
  of their channel acks there; it only moves forward, and mark unread never lowers it) is shown
  to the other members when the install policy `dm.read_receipts` is on and both sides share
  their read receipts. The new module `api::read_receipts`:
  - `DmSeen {channel_id, member_id, seen_id?, seen_at?}` (`#[non_exhaustive]`; `DmSeen::new`,
    `DmSeen::removed`): the body of the event `dm_seen`, sent to the DM's other members, never
    the reader's own sessions, e.g. `{"channel_id": "…", "member_id": "…", "seen_id": "…",
    "seen_at": "2026-11-18T12:01:00.123Z"}`. A copy without `seen_id` and `seen_at` removes the
    member's position (they stopped sharing).
  - `DmReadReceipts {channel_id, active, items}` (`#[non_exhaustive]`) for
    `routes::CHANNEL_READ_RECEIPTS` (`GET /api/v1/channels/{id}/read_receipts`); a space
    channel answers `422 invalid_state` with `reasons::NOT_A_DM` (`not_a_dm`).
  - `ReadReceiptSettings {share_read_receipts, install_enabled}` (`#[non_exhaustive]`;
    `active()`) for `routes::ME_READ_RECEIPTS` (`GET /api/v1/me/read_receipts`) and
    `routes::ME_READ_RECEIPTS_SET` (`POST /api/v1/me/read_receipts/set` with
    `SetReadReceiptsRequest {share_read_receipts}`), and the event
    `read_receipt_settings_updated` to the member's own sessions. Sharing is on by default and
    reciprocal: off, the member's positions are neither kept nor shown, and they see nobody's.
  - `ReadReceiptsPolicyUpdated {enabled}`, the event `read_receipts_policy_updated` to every
    session when the policy changes.
  - `FEATURE_DM_READ_RECEIPTS` (`dm_read_receipts`) in `mgmt::FEATURES`.
- **A person's notification level and mute per channel or DM over mgmt.**
  `routes::MGMT_PERSON_CHANNEL_NOTIFY` (`GET /mgmt/v1/people/{external_id}/channels/{id}/notify`)
  and `routes::MGMT_PERSON_CHANNEL_NOTIFY_SET` (`POST …/notify/set`) answer and take the person's
  own route's types, `api::notify::ChannelNotifyPrefs` and `SetChannelNotifyRequest`, with the
  same rules. `audit::Action::NotifyPrefsSet` (`notify.prefs_set`, install class; additive:
  `Action::Unknown` keeps its 0.10.5 discriminant, 211, and the new action takes 212) and
  `mgmt::FEATURE_PERSON_CHANNEL_NOTIFY` (`person_channel_notify`) in `mgmt::FEATURES`.
- `routes::MGMT_PERSON_NOTIFICATIONS_HEAD` (`GET /mgmt/v1/people/{external_id}/notifications/head`)
  and `mgmt::PersonNotificationsHead {head}` (`#[non_exhaustive]`; `PersonNotificationsHead::new`):
  the person's member-log head, where a new feed consumer starts (`after = head`);
  `mgmt::FEATURE_FEED_HEAD` (`feed_head`) in `mgmt::FEATURES`.
- `EventName::DmSeen`, `ReadReceiptSettingsUpdated` and `ReadReceiptsPolicyUpdated` (68–70)
  and their `Event` variants. Additive: both enums are `#[non_exhaustive]`, and
  `EventName::Unknown` keeps its 0.10.5 discriminant (67).
- `ReadyChannel.dm_seen` (the other members' positions in a DM, skipped when empty) and
  `ReadyInstall.read_receipts` (`Option<ReadReceiptSettings>`, skipped when `None`; absent
  from an older server). Both structs are `#[non_exhaustive]`. `Ready`, `ReadState` and
  `MeSync` are not, so they gain nothing: an agent without a socket reads the positions from
  the GET.
- The policy `dm.read_receipts` (default `true`; old payloads read `true`):
  `Policies.dm_read_receipts` and `PolicyPatch.dm_read_receipts` (both `#[non_exhaustive]`).
  `PolicyKey` is not `#[non_exhaustive]`, so it gains no variant in this release; the key is
  `policy::DM_READ_RECEIPTS`. `PolicyDigest` is unchanged (not `#[non_exhaustive]`); clients
  read the policy from `ReadyInstall.read_receipts.install_enabled`.
- **Export minor 14.** Full exports carry `dm_seen.ndjson` (`export::DmSeenRecord`) and
  `read_receipt_prefs.ndjson` (`export::ReadReceiptPrefRecord`), both `#[non_exhaustive]`
  with constructors. `EXPORT_FORMAT_MINOR` is 14.

## 0.10.5 (2026-11-18)

### Added

- **Keyring mirror placement and sync alarms.** `mgmt::StatusAlarm` gains
  `KeysMirrorUnmounted` (`keys_mirror_unmounted`: the `dir` mirror path is missing or not
  mounted), `KeysMirrorSameDevice` (`keys_mirror_same_device`: it is on the keyring's own
  filesystem) and `KeysMirrorUnsynced` (`keys_mirror_unsynced`: no full mirror reconcile has
  succeeded since the server started). Additive: the enum is `#[non_exhaustive]`.
  `StatusAlarm::Unknown` keeps its 0.10.4 discriminant (41) and the new variants take 42 to
  44, so every existing variant's number (an `as` cast) is unchanged.
- **Why a transcript could not be made.** `recording::TranscriptFailure { code, message }`
  (`#[non_exhaustive]`, built with `TranscriptFailure::new(code)`) and the stored enum
  `recording::TranscriptFailureCode` (`#[non_exhaustive]`; `audio_gone` 1,
  `unsupported_language` 2, `transcriber_error` 3, `audio_not_captured` 4, and `Unknown`;
  `TranscriptFailureCode::message` gives each code's plain words). The server sets it when a
  transcript ends `failed` or `partial` and it knows why:
  `Message.transcript_failure` on the record message, beside `recording`, and
  `MgmtRecordingSummary.transcript_failure`, each skipped when `None`, e.g.
  `"transcript_failure": {"code": "transcriber_error", "message": "The transcription service
  could not process the audio."}`. Additive: `Message` and `MgmtRecordingSummary` are
  `#[non_exhaustive]`. It is not inside `RecordingSummary.transcript`, because
  `SummaryTranscript` and `RecordingSummary` are not `#[non_exhaustive]` and a new field there
  would break a struct literal. `DeliveryMessage` (not `#[non_exhaustive]`) does not carry it.
- **A full mirror reconcile on demand.** `routes::MGMT_KEYS_MIRROR_SYNC`
  (`POST /mgmt/v1/keys/mirror-sync`) answering `mgmt::KeysMirrorSyncResponse` (`kind`, `ok`,
  `pushed`, `failed`, `repaired`, `deleted_shredded`, `unknown`, and `error`, `placement`,
  `last_full_reconcile_ok_at` when set).

## 0.10.4 (2026-11-17)

- The version follows Sew 0.10.4. No API or behaviour change. The package's metadata and
  README follow the published-crate standard (issues, MSRV, features, a compiling example that
  runs as a doc test).

## 0.10.3 (2026-11-16)

- The version follows Sew 0.10.3. No API or behaviour change.

## 0.10.2 (2026-11-16)

- The version follows Sew 0.10.2 (there is no 0.10.1). No API or behaviour change.

## 0.10.0 (2026-11-15)

### Added

- **Each person's Record and Stop, from the server.** `recording::controls::decide`
  (`ControlsInput`, `ActiveInput`): the one rule table for whether Sew would take a person's
  Record and Stop now, pure and without IO. `recording::RecordingControls { can_record,
  can_stop, rev, record_unavailable }` (`#[non_exhaustive]`), with the hint
  `RecordUnavailable` (`recording_capacity`, `recorder_unavailable`; it never changes
  `can_record`). The event `recording_controls_updated` (`EventName::RecordingControlsUpdated`,
  `Event::RecordingControlsUpdated`, body `RecordingControlsUpdated`, to the person's own
  sessions only); `Recording.my_controls` (self only, skipped when `None`);
  `ReadyInstall.recording_controls` (skipped when `false`); `CallReady.recording_controls`
  (written as `null` while the member has no connected voice entry; absent from older
  servers). Of two values, the larger `rev` is the newer. `mgmt::FEATURE_RECORDING_CONTROLS`
  (`recording_controls`) in `FEATURES`.
- `vectors/recording-controls-vectors.json`: `decide` over the full cross product of its
  facts, for other implementations to check themselves against.
- **One channel member in or out over mgmt.** `routes::MGMT_CHANNEL_MEMBERS_ADD` and
  `MGMT_CHANNEL_MEMBERS_REMOVE` (`/mgmt/v1/channels/{channel_id}/members/add`, `…/remove`):
  `mgmt::MgmtChannelMemberRequest {app_id?, external_id}` →
  `MgmtChannelMemberResponse {channel_id, space_id, external_id, member_id, changed}`.
  `mgmt::FEATURE_CHANNEL_MEMBER_ADD_REMOVE` (`channel_member_add_remove`) in `FEATURES`.
- **Members who may leave a governed channel.** `Channel.members_can_leave` (additive:
  `Channel` is `#[non_exhaustive]`) and `ReadyChannel.members_can_leave` (skipped when `false`); `MgmtChannelCreateRequest.members_can_leave`
  (skipped when `false`), `MgmtChannelCreateResponse.members_can_leave` (always written; absent
  reads as `false`); `members_can_leave` on `MgmtChannelOp::Ensure` and the new op
  `MgmtChannelOp::Update {channel_id, members_can_leave?}` (`MgmtChannelOpKind::Update`,
  `update`). `api::channels::FEATURE_CHANNEL_MEMBERS_CAN_LEAVE` (`channel_members_can_leave`)
  in `FEATURES`.
- **Export minor 13.** `export::ChannelRecord.members_can_leave` (skipped when `false`; an
  older archive without it reads as `false`). `EXPORT_FORMAT_MINOR` is 13.
- **Typing over HTTP.** `routes::CHANNEL_TYPING` (`POST /api/v1/channels/{id}/typing`) with
  `api::channels::ChannelTypingRequest {thread_id?}`; it answers `204`.
  `api::channels::FEATURE_HTTP_TYPING` (`http_typing`) in `FEATURES`.
- **Unread count.** `ReadState.unread_count` (always written; absent from an older server reads
  as `0`): unread timeline messages, capped at `api::read_state::UNREAD_COUNT_CAP` (99, meaning
  99 or more). `api::read_state::FEATURE_READ_STATE_UNREAD_COUNT` (`read_state_unread_count`)
  in `FEATURES`.
- **Level-only member-channels sync.** `MgmtMemberChannelsRequest.channel_ids` and
  `MgmtMemberChannelsItem.channel_ids` are `Option<Vec<Id>>`: absent or `null` changes only the
  level; `[]` still empties the person's private channels.
  `mgmt::FEATURE_MEMBER_CHANNELS_LEVEL_ONLY` (`member_channels_level_only`) in `FEATURES`.
- **Agents and box people in DMs.** The policy `agents.dm_with_box_people`
  (`Policies.agents_dm_with_box_people`, default `true`, old payloads read `true`;
  `PolicyPatch.agents_dm_with_box_people`; `PolicyKey::AgentsDmWithBoxPeople`, the last
  variant): off refuses opening a DM or group DM that puts an agent with a box person
  (`k2:owner`, `k2:connect:`). `policy::FEATURE_AGENTS_DM_WITH_BOX_PEOPLE`
  (`agents_dm_with_box_people`) in `FEATURES`.
- **A DM call's starter cancels it.** `routes::CHANNEL_CALL_CANCEL`
  (`POST /api/v1/channels/{id}/call/cancel`) with `calls::CallCancelRequest { call_id }`, only
  by the call's starter and only before anyone else joined (otherwise `422 invalid_state`
  with `calls::reasons::NOT_CANCELLABLE`, `not_cancellable`). The call ends with
  `CallEndReason::Cancelled` (`cancelled`, storage code 9); its pending rings become
  `RingState::Cancelled` (`cancelled`), not `missed`, and stop with `call_ring` reason
  `ring_reason::CANCELLED` (`cancelled`). `calls::FEATURE_CALL_CANCEL` (`call_cancel`) in
  `FEATURES`. On the wire this is additive: an older client reads the new values as `Unknown`.

### Changed

- `ReadyChannel.has_guests` is left out of READY when `false` (it has always read as `false`
  when absent), so READY stays within its size bound with `ReadState.unread_count` added.

### Breaking

- `CallEndReason` and `RingState` have a new variant, `Cancelled`, so a `match` on either
  written for 0.9.x no longer compiles. Add a `Cancelled` arm (or a wildcard). Both are now
  `#[non_exhaustive]` (see below).
- `PolicyKey` has a new variant, `AgentsDmWithBoxPeople`, and is not `#[non_exhaustive]`, so an
  exhaustive `match` on it written for 0.9.x no longer compiles. The new variant comes just
  before `Unknown`, whose discriminant moves from 44 to 45; no other variant's moves.
  (`Policies.agents_dm_with_box_people` is additive: `Policies` is already `#[non_exhaustive]`.)
- New public fields on structs that are not `#[non_exhaustive]`, so a struct literal written
  for 0.9.x no longer compiles: `ReadState.unread_count`, `MgmtChannelCreateRequest.members_can_leave`,
  `MgmtChannelCreateResponse.members_can_leave` and `members_can_leave` on
  `MgmtChannelOp::Ensure`. `MgmtChannelOp` and `MgmtChannelOpKind` have a new variant (`Update`),
  so an exhaustive `match` on either no longer compiles. `MgmtMemberChannelsRequest.channel_ids` and
  `MgmtMemberChannelsItem.channel_ids` changed from `Vec<Id>` to `Option<Vec<Id>>` (wrap a list
  in `Some`). Every 0.9.x payload still reads as before.
- **`gateway::events::EventName`, `gateway::events::Event`, `gateway::server::ReadyInstall`,
  `gateway::server::CallReady`, `calls::CallEndReason` and `calls::RingState` are now
  `#[non_exhaustive]`.** This is a one-time change so that later events, call end reasons,
  ring states and READY fields are additive and do not need a breaking release. Outside the
  crate, a `match` on `EventName`, `Event`, `CallEndReason` or `RingState` needs a `_` arm (a
  value this build does not know already decodes as `Unknown`; a `_` arm also covers values
  added later). A struct
  literal of `ReadyInstall` or `CallReady` no longer compiles: build them with
  `ReadyInstall::new(id, name, policy_digest)` or `CallReady::new(session_id, user,
  install_id)` (which sets `v` to `PROTOCOL_VERSION`) and set the other public fields;
  destructuring them needs `..`. The wire format does not change.

### Fixed (server behaviour)

- Posting in a public Space channel now joins the poster. Before, a Space member who had not
  joined a public channel could not post there (`403`); now a top-level post or a thread reply by a member who
  may join it (not an assistant or a guest, the channel not archived) joins them in the post's
  transaction, as `POST /channels/{id}/join` does (`channel_member_added`, the change-log
  entries, the read cursor at the new message), so they get `@channel`, `@here` and mention
  rows from then on. Private channels, DMs and meeting channels are unchanged.

## 0.9.3 (2026-11-12)

- The version follows Sew 0.9.3. No API or behaviour change.

### Correction to 0.9.2's notes

- 0.9.2's notes say a payload of 0.9.0 reads as before. One does not: `UpdateChannelRequest`
  with a present `"name": null` read as `None` in 0.9.0, which keeps the channel's name, and
  reads as `Some("")` from 0.9.2 on, which clears a group DM's name (as 0.9.2's Group DM names
  entry says) and which the server refuses on a space channel (`400 validation_failed`, field
  `name`). An absent `name` still reads as `None`. Every other 0.9.0 payload reads as before,
  and no public field or type changes shape.

## 0.9.2 (2026-11-11)

Additive: no public field or type changes shape, and a payload of 0.9.0 reads as before.

### Added

- **Group DM names.** `UpdateChannelRequest.name` on a group DM sets its name (1–100
  characters once trimmed); `null`, `""` or blanks clear it (a present `null` reads as
  `Some("")`, so a Rust client clears with an empty string). Any other field on a DM, and any
  change to a 1:1 DM, still answers `422 dm_immutable`. `Channel.name` is set on a named group
  DM and absent otherwise.
- **System messages.** `Message.system: Option<MessageKind>` (absent on ordinary messages and
  tombstones); `MessageKind` (`#[non_exhaustive]`, tagged by `kind`) has `GroupDmRenamed { name }`
  (`group_dm_renamed`; no `name` when it was cleared) and the forward-compat `Unknown`. A system
  message's `body` is empty. `api::messages::reasons::SYSTEM_MESSAGE_IMMUTABLE`
  (`system_message_immutable`): an edit of one answers `422 invalid_state` with it.
- **DMs by latest activity.** `GET /api/v1/me/channels` reads `MyChannelsPageQuery`
  (`#[non_exhaustive]`; `MyChannelsQuery`'s fields plus `order` and `before`, `From` the old
  query) and answers `MyChannelsPage` (`#[non_exhaustive]`; `items`, `has_more` and `next`). The
  enum `MyChannelsOrder` (`id`, the default; `activity`). `order=activity` (with `kind=dm`) lists
  the DMs newest first by `last_message_id` (a call's record message counts; a DM with no message
  by its own id), `limit` 1–200 (`MY_CHANNELS_ACTIVITY_LIMIT_MAX`, default 50), paged by the
  opaque `before` cursor the previous page's `next` gives; each page continues strictly below
  it, and a DM that gets new activity while a client pages moves above the cursor, so clients
  move it to the top from their live events. The `order=id` answer is unchanged (no `next`).
- **Feature flags** `group_dm_names` and `dm_order_activity`, now in `FEATURES`
  (`api::channels::FEATURE_GROUP_DM_NAMES`, `api::channels::FEATURE_DM_ORDER_ACTIVITY`), so a
  client knows the server has them.
- `UpdateChannelRequest`'s validation lets an empty `name` through (the server refuses it on a
  space channel, `400 validation_failed`, field `name`, as before).

### Correction to 0.9.0's notes

- 0.9.0 also broke exhaustive matches on `PolicyKey`: `PolicyKey` is not `#[non_exhaustive]`,
  so a `match` on it written for 0.8.x no longer compiles once `PolicyKey::RecordingRequired`
  exists. The new variant sits after `RecordingRequireConsentToJoin`, so the implicit
  discriminants of every later variant (from `MeetingsEnabled` on) moved by one; code that casts
  a `PolicyKey` to an integer sees different values. 0.9.0's Breaking list named the variant
  but said only `PassCap` and `StopReason` matches stop compiling.

## 0.9.0 (2026-11-10)

**The first crates.io release since 0.8.1.** 0.8.2 and 0.8.3 were mirror-only, so coming from
crates.io's 0.8.1 this release also carries 0.8.2's breaks and 0.8.3's change (their entries
below).

**Recording in Apps.** The wire is additive throughout: every new field is optional or has a
default, and a payload of 0.8.x reads as before.

### Breaking

- **New enum variants:** `PassCap::VoiceRecord` (`voice:record`, bit 32; `PassCap::ALL` has six
  entries and `CapSet` reads bit 32), `StopReason::MgmtStopped` (`mgmt_stopped`, code 10),
  `Action::RecordingSettings` (`recording.settings`) and `Action::RecordingTranscriptRead`
  (`recording.transcript_read`), `PolicyKey::RecordingRequired` (`recording.required`) and
  `StatusAlarm::RecordingRequiredFailed` (`recording_required_failed`). A `match` on
  `PassCap` or `StopReason` written for 0.8.x no longer compiles. `Action`'s new variants sit
  before older ones, so the implicit discriminants of later variants move.
- **`StopReason` and `PassCap` are now `#[non_exhaustive]`:** a `match` outside the crate needs
  a `_` arm, and later variants are not breaking.
- **New public fields on structs** (a struct literal written for 0.8.x no longer compiles):
  - `Channel.recording_required` and `Channel.recording_rule`, and the same two on
    `ReadyChannel`; `UpdateChannelRequest.recording_required`;
  - `Recording.trigger`;
  - `ConsentPrompt.required`, `ConsentPrompt.app_readers` and `ConsentPrompt.agent_readers`;
  - `Policies.recording_required` and `PolicyPatch.recording_required`;
  - `MgmtAppSettingsRequest.recording`, `MgmtAppSettings.recording`, and
    `recording_start_level` and `recording_required` on `MgmtSpaceSettingsRequest` and
    `MgmtSpaceSettings`;
  - the export records: `SpaceRecord.recording_start_level` and `recording_required`,
    `ChannelRecord.recording_required`, `RecordingRecord.trigger`.
- **These structs are now `#[non_exhaustive]`,** so later fields are not breaking. Outside the
  crate, build them with their constructor (or `Default`) and set the public fields; a struct
  literal or a `..base` update no longer compiles. Each constructor sets every other field to
  its unset value (`None`, empty, `false`, 0); its doc says which.
  - `Channel::new(id, kind, visibility, created_at)`;
  - `ReadyChannel::new(id, kind, visibility, my_perms)`;
  - `Recording::new(id, channel_id, started_by, started_at, message_id)`;
  - `ConsentPrompt::new(recording_id, room)`;
  - `Member::new(id, kind, install_role, username, display_name, created_at)`;
  - `Message::new(id, channel_id, author_id, seq, created_at)`;
  - `MediaPolicy::new(tier, camera, screen)`;
  - `Status::new(version, schema_version, install_id, provider, db, jobs)`;
  - `MgmtAppSettings::new(app_id)` and `MgmtSpaceSettings::new(space_id, join_policy)`;
  - the export records `SpaceRecord::new(id, slug, name, owner_id, created_at)`,
    `ChannelRecord::new(id, kind, visibility, created_at)` and
    `RecordingRecord::new(id, channel_id, message_id, started_by, started_at, state,
    audio_state, transcript_state)`;
  - with `Default`: `UpdateChannelRequest`, `MgmtAppSettingsRequest`,
    `MgmtSpaceSettingsRequest`, `Policies` and `PolicyPatch`.
- `EXPORT_FORMAT_MINOR` is 12.

### Added

- **Enums** (all `#[non_exhaustive]`): `RecordingTrigger` (`manual`, `auto`, `required`; codes
  1–3; `Default` is `manual`), `RecordingRule` (`none`, `auto`, `required`; `Default` is
  `none`), `RecordingStartLevel` (`none`, `admin`, `moderator`, `member`),
  `RecordingRequiredPolicy` (`off`, `multi_person`) and `RecordingEventKind` (`started`,
  `stopped`, `transcript_ready`, `audio_expired`, `deleted`; codes 1–5).
- **The flags** `app_recording`, `voice_record_cap`, `recording_required` and
  `mgmt_recording_reads`, now in `FEATURES` (`FEATURE_APP_RECORDING`,
  `FEATURE_VOICE_RECORD_CAP`, `FEATURE_RECORDING_REQUIRED`, `FEATURE_MGMT_RECORDING_READS`).
- **The client wire:**
  - `Channel.recording_rule`: the rule a room records under, every source applied (`auto`,
    `required`; omitted for `none`);
  - `Channel.recording_required`: a voice channel's own "record every call with more than one
    person" (omitted while it inherits), settable through `UpdateChannelRequest`;
  - `Recording.trigger`: what started a recording (always sent; a payload without it reads as
    `manual`);
  - `ConsentPrompt.required`, `app_readers` and `agent_readers`: each left out of the JSON
    when unset, and `notice_version` hashes it only when set, so a prompt that sets none keeps
    the version 0.8.x gave it. `ConsentPrompt::lines_en` / `text_en` say "This room records
    every call with more than one person." (in place of the auto-record line for a room with
    no recording yet), "<Apps> can read the transcript." and "Agents in this channel can read
    the transcript." after who reads it;
  - the reason `recording_required` (`recording::reasons::RECORDING_REQUIRED`): a client stop
    of a required recording while two or more people are in the room.
- **The policy** `recording.required` (`off` by default; a stored policy set without it reads
  `off`). `Policies` JSON now always carries the key.
- **Mgmt:**
  - an App's recording settings (`MgmtAppRecordingRequest`, `MgmtAppRecording`) and a Space's
    overrides (`null` inherits);
  - the routes `MGMT_CHANNEL_RECORDING`, `MGMT_RECORDING_STOP`, `MGMT_RECORDING`,
    `MGMT_RECORDING_TRANSCRIPT`, `MGMT_SPACE_RECORDINGS`, `MGMT_APP_RECORDING_CHANGES`, and the
    audio download twins `MGMT_COMPLIANCE_RECORDING_DOWNLOADS`,
    `MGMT_COMPLIANCE_RECORDING_DOWNLOAD_SEGMENTS` and `MGMT_COMPLIANCE_RECORDING_FILE`;
  - their types (`MgmtChannelRecordingRequest`, `MgmtChannelRecording`,
    `MgmtRecordingStopRequest`, `MgmtRecordingQuery`, `MgmtRecordingsQuery`,
    `MgmtRecordingsPage`, `MgmtRecordingSummary`, `MgmtRecordingParticipant`,
    `MgmtTranscriptQuery`, `MgmtTranscriptPage`, `MgmtTranscriptTurn`,
    `MgmtRecordingChangesQuery`, `MgmtRecordingChanges`, `MgmtRecordingChange`), all
    `#[non_exhaustive]` with `new` or `Default`, and their limits;
  - the reasons `recording_server_read_off` and `sew_actor_admin_required`
    (`mgmt::reasons`): the stop, a read without `app_id` and the audio downloads need a
    `Sew-Actor` naming the install's Owner or an Admin who is a person; the stop names no App.
- **Export minor 12:** the Space's recording overrides, a voice channel's
  `recording_required`, and `trigger: required` on a recording a required room started.

## 0.8.3 (2026-11-10)

**0.8.3 is mirror-only, as 0.8.2 was (crates.io stays at 0.8.1); the next crates.io release is
0.9.0.** It adds nothing to 0.8.2's API: one field's documentation and the example policy.

- `MediaPolicy.backup_codec` can now be `true` (no shape change): a native sender with OS
  H.264 may publish H.264 with a VP8 backup that the engine turns on for subscribers without
  H.264. Browsers still publish one codec. It is `true` only while video is on and `codecs`
  lists both `vp8` and `h264`; the example policy says `true`.

## 0.8.2 (2026-11-06)

**0.8.2 is mirror-only; it breaks 0.8.1 (crates.io stays at 0.8.1); the next crates.io release is
0.9.0.**

### Breaking

- New variants on enums that were exhaustive: `Action` (`MemberErase`, `MemberEraseComplete`,
  `MemberErasePreview`, `MemberEraseReplayed`), `ExternalId::Erased`, `DeleteKind::Erased`,
  `RemoveKind::Erased`, `VanishReason::Erased` and `StatusAlarm::EraseStuck`. A `match` written
  for 0.8.1 no longer compiles.
- `Action`'s new variants sit before older ones, so the implicit discriminants of the later
  variants move: code that casts an `Action` to an integer gets other numbers. The wire is
  unchanged (the string names, with the `Unknown` fallback).
- New public fields on structs (`Member.erased`, `Message.deleted_reason`,
  `MemberRecord.erased_at` and `erase_mode`, `Status.erasures`): a struct literal written for
  0.8.1 no longer compiles. The JSON is additive (each new field is omitted when unset).
- `Action`, `ExternalId`, `DeleteKind`, `RemoveKind`, `VanishReason` and `StatusAlarm` are now
  `#[non_exhaustive]`, so later variants are not breaking: a `match` outside the crate needs a
  `_` arm. The JSON and the schemas are unchanged.

### Added

- Account erase (flag `principal_erase`, `FEATURE_PRINCIPAL_ERASE`): `MgmtEraseRequest`,
  `MgmtEraseResult`, `MgmtEraseAccepted`, `MgmtErasuresQuery`, `MgmtErasurePage`,
  `MgmtEraseCounts`, `MgmtEraseKept`, `EraseMode` (with `EraseMode::default_for`),
  `EraseState`, `EraseKeptKind` and `EraseKeptReason` (`POST /mgmt/v1/principals/erase`,
  `GET /mgmt/v1/principals/erase/{job_id}`, `GET /mgmt/v1/principals/erasures`;
  `routes::MGMT_PRINCIPALS_ERASE`, `MGMT_PRINCIPALS_ERASE_JOB`, `MGMT_PRINCIPALS_ERASURES`,
  `MGMT_ERASURES_LIMIT_DEFAULT` and `MGMT_ERASURES_LIMIT_MAX`).
- `ExternalId::Erased` (`erased:<member_id>`, the tombstone id of an erased member), refused
  by provision, passes and every mgmt input.
- Client API: `Member.erased` and `Message.deleted_reason` (`DeletedReason::Erased`), both
  omitted otherwise; `VanishReason::Erased` on `content_vanished`.
- Reason `erase_mode_downgrade` (`mgmt::reasons`); `service_member` is reused.
- Audit actions `member.erase`, `member.erase_complete`, `member.erase_preview` and
  `member.erase_replayed`.
- `status.erasures` (`StatusErasures`) and the alarm `erase_stuck` (`StatusAlarm::EraseStuck`).
- Export format minor 11 (`EXPORT_FORMAT_MINOR`): `MemberRecord.erased_at` and `erase_mode`,
  `DeleteKind::Erased` and `RemoveKind::Erased`.

## 0.8.1 (2026-11-06)

- The version follows Sew 0.8.1. No API or behaviour change.

## 0.8.0 (2026-11-05)

- Participant links (flag `app_participant_links`, `FEATURE_APP_PARTICIPANT_LINKS`): `Space`
  and `ReadySpace` gain `participant_app_ids` (the Apps that also show the Space; an App
  session sees only its own id), and `PersonNotification` gains `participant_app_ids`. All
  omitted when empty; `app_id` stays the governing App.
- Export format minor 10 (`EXPORT_FORMAT_MINOR`): `SpaceRecord.participant_app_ids`.
- Mgmt: `MgmtSpaceAppLinkRequest` and `MgmtSpaceAppsChange` (`POST
  /mgmt/v1/spaces/{space_id}/apps/link` and `…/apps/unlink`, `routes::MGMT_SPACE_APPS_LINK` and
  `MGMT_SPACE_APPS_UNLINK`), `MgmtAppSpacesQuery`, `MgmtAppSpaces` and `MgmtAppSpace` (`GET
  /mgmt/v1/apps/{app_id}/spaces`, `routes::MGMT_APP_SPACES`, `MGMT_APP_SPACES_LIMIT_DEFAULT`
  and `MGMT_APP_SPACES_LIMIT_MAX`). `MgmtSpaceAppRole::Participant` is now written.
- Reasons `app_not_governing`, `app_is_governing`, `participant_apps_max` and
  `home_client_cannot_link` (`mgmt::reasons`).
- Audit actions `space.app_link` and `space.app_unlink` (`Action::SpaceAppLink`,
  `Action::SpaceAppUnlink`).
- `VoiceLeaveReason::AppUnlinked` (`app_unlinked`, code 29).

## 0.7.4 (2026-11-05)

- `ConsentPrompt::title_en`, `feature_en`, `lines_en`, `text_en` and `choices_en`, with
  `recording::ConsentAsk` (`Join`, `InCall`) and `recording::ConsentChoice`: the Sew App kit's
  consent modal in English, word for word, for native hosts. No wire change.

## 0.7.3 (2026-11-04)

- `CallReady.recording`: the call channel's active recording as the member sees it, or `null`
  (always written by Sew 0.7.3; absent from an older Sew). The call socket's event set now
  includes the member's own `recording_updated`.

## 0.7.2 (2026-11-03)

- First published version.
- The wire types of Sew's client and management APIs: ids, chat addresses, errors and their codes,
  HTTP request and response bodies and route paths, gateway frames and events, change-log kinds,
  permission bits, organisation policies and audit events.
- Includes Sew 0.7.2's wire: App settings `origins`, the optional `origin` on voice and call
  joins and `GatewayTicketRequest`, the `origin_not_listed` error, the `app_origins` feature flag
  and `status.gateway.reauth`.
- A JSON Schema for every type in `schema/`.
- Dependencies: serde, serde_json, schemars and chrono. No I/O apart from the clock.
- Licence: MIT OR Apache-2.0.
