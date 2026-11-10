//! Audit events and their canonical JSON.
//!
//! Audit rows never hold content: `details` carries ids, names, old and new policy values and
//! permission bits only. Rows are chained with `hash = SHA-256(prev_hash ‖ canonical_json)`.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::id::Id;
use crate::time::Timestamp;

unit_enum! {
    /// What kind of principal acted.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum ActorKind {
        /// A person member.
        Person = "person",
        /// An agent member.
        Agent = "agent",
        /// The mgmt service (K2's daemon or the `sew` CLI).
        Service = "service",
        /// Sew itself (jobs).
        System = "system",
    }
}

unit_enum! {
    /// How an audited action ended.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum AuditResult {
        /// It succeeded.
        Ok = "ok",
        /// A permission or policy check refused it.
        Denied = "denied",
        /// It failed.
        Error = "error",
    }
}

unit_enum! {
    /// An audited action, `<entity>.<verb>` in `lower_snake`. New actions are added over time.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    #[non_exhaustive]
    pub enum Action {
        /// A session was created.
        SessionCreate = "session.create",
        /// A session was renewed.
        SessionRenew = "session.renew",
        /// A session was revoked.
        SessionRevoke = "session.revoke",
        /// A pass with a valid signature was rejected.
        PassReject = "pass.reject",
        /// A standalone login failed.
        LoginFail = "login.fail",
        /// A principal was provisioned.
        MemberProvision = "member.provision",
        /// A principal's profile was updated.
        MemberUpdate = "member.update",
        /// A principal was deprovisioned.
        MemberDeprovision = "member.deprovision",
        /// A principal's account was erased: deprovisioned, scrubbed and its external id
        /// mapping cut (`details {erasure_id, mode, sessions_revoked, upgraded?, cascade_of?}`).
        MemberErase = "member.erase",
        /// An erasure has nothing left pending (`details {erasure_id, erased, kept}`: counts and
        /// kinds only).
        MemberEraseComplete = "member.erase_complete",
        /// A dry run of an erase (`details {member_id, mode, erased, kept}`: counts only).
        MemberErasePreview = "member.erase_preview",
        /// A restored database brought an erased member back; it was erased again at start.
        MemberEraseReplayed = "member.erase_replayed",
        /// A local member got a K2 identity, keeping its member id; details `{from, to}`.
        PrincipalRelink = "principal.relink",
        /// The install's identity mode switched from standalone to K2; details `{from, to}`.
        IdentitySwitch = "identity.switch",
        /// The install owner was set.
        OwnerSet = "owner.set",
        /// An assistant was pinned.
        AgentPin = "agent.pin",
        /// An assistant was unpinned.
        AgentUnpin = "agent.unpin",
        /// An act-as call with no domain action of its own.
        ApiCall = "api.call",
        /// A space was created.
        SpaceCreate = "space.create",
        /// A space was updated.
        SpaceUpdate = "space.update",
        /// A space was unbound from its App and became a read-only orphan; details
        /// `{app_id}`.
        SpaceUnbind = "space.unbind",
        /// An orphaned space was released from its old App; details `{orphaned_from}`.
        SpaceRelease = "space.release",
        /// A space's governance switched; details `{app_id, from, to,
        /// managed_role_ids}` (`via: unbind` when unbinding the Space's App set it back to `sew`).
        SpaceGovernance = "space.governance",
        /// A category was created.
        CategoryCreate = "category.create",
        /// A category was updated.
        CategoryUpdate = "category.update",
        /// A category was deleted.
        CategoryDelete = "category.delete",
        /// A channel was created.
        ChannelCreate = "channel.create",
        /// A channel was updated.
        ChannelUpdate = "channel.update",
        /// A channel was archived.
        ChannelArchive = "channel.archive",
        /// A channel was unarchived.
        ChannelUnarchive = "channel.unarchive",
        /// A channel's overwrites were re-synced with its category.
        ChannelSync = "channel.sync",
        /// A role was created.
        RoleCreate = "role.create",
        /// A role was updated.
        RoleUpdate = "role.update",
        /// A role was deleted.
        RoleDelete = "role.delete",
        /// A role was assigned.
        RoleAssign = "role.assign",
        /// A role was unassigned.
        RoleUnassign = "role.unassign",
        /// An overwrite was set.
        OverwriteSet = "overwrite.set",
        /// An overwrite was deleted.
        OverwriteDelete = "overwrite.delete",
        /// Org policies were changed.
        PolicySet = "policy.set",
        /// A member was timed out.
        MemberTimeout = "member.timeout",
        /// A timeout was lifted.
        MemberUntimeout = "member.untimeout",
        /// A member was removed from a space.
        SpaceMemberRemove = "space_member.remove",
        /// A member was banned from a space (the reason is never in details).
        SpaceMemberBan = "space_member.ban",
        /// A space ban was lifted (by a moderator, or by the system on promotion or takeover).
        SpaceMemberUnban = "space_member.unban",
        /// Slow mode was set on a channel.
        ChannelSlowModeSet = "channel.slow_mode_set",
        /// A moderator deleted a message.
        MessageModeratorDelete = "message.moderator_delete",
        /// One batch of a ban's recent-message wipe (one row per batch of up to
        /// 1,000 ids in one channel, instead of one `message.moderator_delete` per message).
        MessageBanWipe = "message.ban_wipe",
        /// A moderator deleted a whole thread.
        ThreadDelete = "thread.delete",
        /// A moderator removed another member's reaction.
        ReactionModeratorRemove = "reaction.moderator_remove",
        /// A retention policy was set.
        RetentionSet = "retention.set",
        /// A retention preview was requested.
        RetentionPreview = "retention.preview",
        /// A pending retention policy was cancelled or replaced.
        RetentionCancel = "retention.cancel",
        /// A pending retention policy took effect.
        RetentionActivate = "retention.activate",
        /// A retention run was requested by hand.
        RetentionRunRequested = "retention.run_requested",
        /// A retention run finished.
        RetentionRun = "retention.run",
        /// A legal hold was created.
        LegalHoldCreate = "legal_hold.create",
        /// A legal hold was updated.
        LegalHoldUpdate = "legal_hold.update",
        /// A legal hold was released.
        LegalHoldRelease = "legal_hold.release",
        /// An export was started.
        ExportStart = "export.start",
        /// An eDiscovery export was run.
        EdiscoveryExport = "ediscovery.export",
        /// A backup was started.
        BackupStart = "backup.start",
        /// Retired (an import that fails leaves no install to audit into); never written.
        ImportStart = "import.start",
        /// The master key was rotated.
        KeyRotateMaster = "key.rotate_master",
        /// A month key was shredded.
        KeyShred = "key.shred",
        /// The audit log was read.
        AuditRead = "audit.read",
        /// A compliance read happened.
        ComplianceRead = "compliance.read",
        /// A job exhausted its attempts or failed permanently.
        JobDead = "job.dead",
        /// An admin requeued a dead job.
        JobRetry = "job.retry",
        /// An admin cancelled a pending job.
        JobCancel = "job.cancel",
        /// The audit hash chain failed verification.
        AuditVerifyFailed = "audit.verify_failed",
        /// A standalone password was set through a one-time setup link.
        PasswordSet = "password.set",
        /// Failed service-token checks on the mgmt socket, coalesced per peer uid per 10 s.
        MgmtAuthFail = "mgmt.auth_fail",
        /// A non-empty agent deliveries read.
        AgentDeliveriesRead = "agent.deliveries_read",
        /// A one-time password setup link was issued; never the token or URL.
        PasswordSetupLink = "password.setup_link",
        /// A member's install role was set (over the client API or the local role route; both use
        /// the same implementation).
        MemberInstallRoleSet = "member.install_role_set",
        /// A backup bundle was made.
        InstallBackup = "install.backup",
        /// A backup bundle was restored.
        InstallRestore = "install.restore",
        /// A restore drill ran.
        InstallDrill = "install.drill",
        /// The binary was upgraded or rolled back.
        InstallUpgrade = "install.upgrade",
        /// The media engine was installed or reconfigured.
        MediaInstall = "media.install",
        /// Voice was enabled.
        MediaEnable = "media.enable",
        /// Voice was disabled.
        MediaDisable = "media.disable",
        /// The media engine was uninstalled.
        MediaUninstall = "media.uninstall",
        /// Media keys were rotated; kids only, never key bytes.
        MediaKeysRotate = "media.keys_rotate",
        /// The TURN certificate was replaced.
        MediaTurnCert = "media.turn_cert",
        /// The media engine was upgraded or rolled back.
        MediaUpgrade = "media.upgrade",
        /// The recorder and transcriber were installed.
        RecordingInstall = "recording.install",
        /// The recorder or transcriber was upgraded.
        RecordingUpgrade = "recording.upgrade",
        /// The transcriber's model was changed.
        RecordingSetModel = "recording.set_model",
        /// The master key was exported for a box move.
        KeysMasterExported = "keys.master_exported",
        /// The keyring was exported for a box move.
        KeysKeyringExported = "keys.keyring_exported",
        /// A keyring export was imported.
        KeysKeyringImported = "keys.keyring_imported",
        /// A month key was rolled over early.
        KeyRollover = "key.rollover",
        /// Keyring entries were re-wrapped under a new master key.
        KeyRewrap = "key.rewrap",
        /// The local keyring was rebuilt from its mirror.
        KeyRestoreFromMirror = "key.restore_from_mirror",
        /// The keyring mirror went stale.
        KeyMirrorStale = "key.mirror_stale",
        /// A key file for a shredded kid was found and refused.
        KeyResurrectedRefused = "key.resurrected_refused",
        /// Space ownership moved.
        SpaceTransfer = "space.transfer",
        /// Members were added to a space.
        SpaceMemberAdd = "space_member.add",
        /// K2's member-channels sync changed a person's private channels or level in an
        /// App-governed Space; details `{app_id, external_id, added, removed, level,
        /// previous_level, owner_added, queued_job, k2_actor}` (channel ids).
        SpaceMemberSync = "space_member.sync",
        /// A member was added to a channel.
        ChannelMemberAdd = "channel_member.add",
        /// A member was removed from a channel.
        ChannelMemberRemove = "channel_member.remove",
        /// A space admin joined a private channel through the policy join.
        ChannelAdminJoin = "channel.admin_join",
        /// A space's roles were reordered.
        RoleReorder = "role.reorder",
        /// A user group was created.
        UserGroupCreate = "user_group.create",
        /// A user group was changed.
        UserGroupUpdate = "user_group.update",
        /// A user group was deleted.
        UserGroupDelete = "user_group.delete",
        /// Members were added to a user group.
        UserGroupMembersAdd = "user_group.members_add",
        /// Members were removed from a user group.
        UserGroupMembersRemove = "user_group.members_remove",
        /// A custom profile field was created.
        ProfileFieldCreate = "profile_field.create",
        /// A custom profile field was changed.
        ProfileFieldUpdate = "profile_field.update",
        /// A custom profile field was deleted.
        ProfileFieldDelete = "profile_field.delete",
        /// Custom profile fields were reordered.
        ProfileFieldReorder = "profile_field.reorder",
        /// An admin reset another member's avatar or status.
        MemberProfileReset = "member.profile_reset",
        /// K2 pushed an avatar through mgmt.
        MemberAvatarPush = "member.avatar_push",
        /// A moderator removed another member's attachments.
        AttachmentModeratorRemove = "attachment.moderator_remove",
        /// A custom emoji was created.
        EmojiCreate = "emoji.create",
        /// A custom emoji was renamed or its aliases changed.
        EmojiUpdate = "emoji.update",
        /// A custom emoji was deleted.
        EmojiDelete = "emoji.delete",
        /// A space icon was set.
        SpaceIconSet = "space.icon_set",
        /// A space icon was cleared.
        SpaceIconClear = "space.icon_clear",
        /// A moderator turned off another member's link previews.
        MessagePreviewsSuppress = "message.previews_suppress",
        /// The person an assistant posted for approved or declined the post.
        MessageOnBehalfApproval = "message.on_behalf_approval",
        /// An install secret was set; details carry the name and fingerprint only.
        SecretSet = "secret.set",
        /// An install secret was cleared.
        SecretClear = "secret.clear",
        /// A channel bookmark was created.
        BookmarkCreate = "bookmark.create",
        /// A channel bookmark was edited.
        BookmarkUpdate = "bookmark.update",
        /// A channel bookmark was deleted.
        BookmarkDelete = "bookmark.delete",
        /// A channel's bookmarks were reordered.
        BookmarkReorder = "bookmark.reorder",
        /// A moderator closed another member's poll.
        PollModeratorClose = "poll.moderator_close",
        /// An agent's scheduled message was sent.
        ScheduledMessageSend = "scheduled_message.send",
        /// An admin asked for a search index rebuild over mgmt.
        SearchRebuildRequested = "search.rebuild_requested",
        /// A search index rebuild finished and swapped in (system actor).
        SearchRebuild = "search.rebuild",
        /// Retention compacted the search index.
        SearchCompact = "search.compact",
        /// A member became a guest or a guest became a member.
        GuestConvert = "guest.convert",
        /// A guest left a space because its last channel there was gone.
        GuestSpaceRemove = "guest.space_remove",
        /// `guests.enabled` turned off: every guest session revoked.
        GuestSuspendAll = "guest.suspend_all",
        /// `guests.enabled` turned back on.
        GuestRestoreAll = "guest.restore_all",
        /// A test mail was sent over mgmt; details `{ok, smtp_code}`, never the address.
        MailTest = "mail.test",
        /// A digest unsubscribe link turned a member's digest off (system actor).
        NotifyDigestUnsubscribe = "notify.digest_unsubscribe",
        /// The compliance flag was granted or revoked.
        MemberComplianceSet = "member.compliance_set",
        /// An eDiscovery archive was written.
        EdiscoveryComplete = "ediscovery.complete",
        /// An eDiscovery export failed.
        EdiscoveryFail = "ediscovery.fail",
        /// An eDiscovery export was cancelled.
        EdiscoveryCancel = "ediscovery.cancel",
        /// An eDiscovery archive was downloaded, or a range of it.
        EdiscoveryDownload = "ediscovery.download",
        /// An eDiscovery archive was deleted by a caller.
        EdiscoveryDelete = "ediscovery.delete",
        /// An eDiscovery archive expired.
        EdiscoveryExpire = "ediscovery.expire",
        /// A full export archive was written.
        ExportComplete = "export.complete",
        /// A full export failed.
        ExportFail = "export.fail",
        /// A full export was cancelled.
        ExportCancel = "export.cancel",
        /// A full export archive was downloaded.
        ExportDownload = "export.download",
        /// A full export archive was deleted by a caller.
        ExportDelete = "export.delete",
        /// A full export archive expired.
        ExportExpire = "export.expire",
        /// An import finished (the first row of a moved install's new tail).
        ImportComplete = "import.complete",
        /// An import into a standalone install mapped K2 identities (`details {people,
        /// deactivated, spaces_reowned, governance_reset}`).
        ImportIdentityMap = "import.identity_map",
        /// A client asked for an audit chain check.
        AuditVerifyRequested = "audit.verify_requested",
        /// A member joined voice (`details {channel_id, via}`).
        VoiceJoin = "voice.join",
        /// A member left voice (`details {channel_id, reason, duration_ms}`).
        VoiceLeave = "voice.leave",
        /// A moderator server-muted or unmuted a member.
        VoiceServerMute = "voice.server_mute",
        /// A moderator server-deafened or undeafened a member.
        VoiceServerDeafen = "voice.server_deafen",
        /// A moderator moved a member to another voice channel.
        VoiceMove = "voice.move",
        /// A moderator disconnected a member from voice.
        VoiceDisconnect = "voice.disconnect",
        /// Voice was installed, reconfigured, enabled or disabled from the CLI.
        VoiceConfig = "voice.config",
        /// The media token key was rotated from the CLI.
        VoiceKeysRotate = "voice.keys_rotate",
        /// K2 reported the media path down or up (`details {state,
        /// media_path_reason, ended_calls}`).
        VoiceMediaPath = "voice.media_path",
        /// The call DS and CA keys were generated (`details {ds_kid, ca_kid}`).
        CallE2eeKeysCreated = "call.e2ee_keys_created",
        /// A forced removal from a call's key group completed.
        CallE2eeMemberRemoved = "call.e2ee_member_removed",
        /// A call's key group was reset.
        CallE2eeGroupReset = "call.e2ee_group_reset",
        /// A call's key group was rebuilt after a restart.
        CallE2eeRebuilt = "call.e2ee_rebuilt",
        /// A recorder certificate was issued.
        CallE2eeRecorderCertified = "call.e2ee_recorder_certified",
        /// A client reported a roster mismatch.
        CallE2eeRosterReport = "call.e2ee_roster_report",
        /// A device was removed and refused for a stuck key slot.
        CallE2eeSenderStuck = "call.e2ee_sender_stuck",
        /// A client's bridge reported a misbehaving engine.
        CallE2eeEngineAnomaly = "call.e2ee_engine_anomaly",
        /// A device key was enrolled (`details {device_id, serial, platform}`).
        DeviceEnroll = "device.enroll",
        /// A device certificate was renewed.
        DeviceRenew = "device.renew",
        /// A device was revoked (`details {device_id, serials, reason}`).
        DeviceRevoke = "device.revoke",
        /// A DM call started (actor = the starter; `details {call_id,
        /// channel_id, rung_count}`).
        CallStart = "call.start",
        /// A DM call ended (actor `system`; `details {call_id, channel_id,
        /// duration_ms, joined_count, end_reason}`).
        CallEnd = "call.end",
        /// A recording started (`details {recording_id, channel_id,
        /// call_record_id?, auto, audio_keep_days, mixed}`).
        RecordingStart = "recording.start",
        /// A recording stopped (`details {recording_id, duration_ms, stop_reason,
        /// recorded_members}`).
        RecordingStop = "recording.stop",
        /// A consent answer (`details {recording_id, consent, via, surface, notice_version}`).
        RecordingConsent = "recording.consent",
        /// Audio keep days changed (`details {recording_id, old_days, new_days}`).
        RecordingAudioRetention = "recording.audio_retention",
        /// Audio purged (`details {recording_id, segments, bytes, held_segments}`).
        RecordingAudioPurge = "recording.audio_purge",
        /// The recorder rejoined (`details {recording_id, run, reason}`).
        RecordingRecorderRejoin = "recording.recorder_rejoin",
        /// The recorder refused engine misbehaviour (`details {recording_id, kind}`).
        RecordingEngineAnomaly = "recording.engine_anomaly",
        /// The recorder refused a binding table (`details {recording_id, kind}`).
        RecordingBindingAnomaly = "recording.binding_anomaly",
        /// A segment of an ungranted identity was discarded (`details {recording_id, run,
        /// seq}`; never the identity).
        RecordingConsentViolation = "recording.consent_violation",
        /// A transcript became final (`details {recording_id, turns, gaps_ms,
        /// failed_segments, model, backend}`).
        TranscriptFinal = "transcript.final",
        /// A compliance download grant (`details {recording_id, channel_id, download_id, what,
        /// parts, segments, reason_given}`).
        RecordingDownload = "recording.download",
        /// A room's or an App's recording settings changed over mgmt (`details {app_id?,
        /// space_id?, channel_id?, fields}`: the names of the fields set, never values that
        /// identify a person).
        RecordingSettings = "recording.settings",
        /// An App's server read a transcript over mgmt (`details {recording_id, app_id,
        /// turns}`; the actor is the App's service and K2's `Sew-Actor`).
        RecordingTranscriptRead = "recording.transcript_read",
        /// A meeting was made (`details {meeting_id, channel_id, kind,
        /// starts_at, ends_at, member_attendees, email_attendees}`: counts and ids only, never a
        /// title, name or email).
        MeetingCreate = "meeting.create",
        /// A meeting changed (`details {meeting_id, fields, ics_sequence}`).
        MeetingUpdate = "meeting.update",
        /// A scheduled meeting was cancelled (`details {meeting_id, ics_sequence}`).
        MeetingCancel = "meeting.cancel",
        /// A meeting ended (actor `system` for the end job, the host for End for all;
        /// `details {meeting_id, end_reason, duration_ms}`).
        MeetingEnd = "meeting.end",
        /// A host locked or unlocked a meeting (`details {meeting_id, locked}`).
        MeetingLock = "meeting.lock",
        /// A host removed someone from a meeting (`details {meeting_id, member_id,
        /// kind, in_call, sessions_revoked, devices_revoked}`): ids and counts only, never a
        /// name, an email or an IP.
        MeetingRemove = "meeting.remove",
        /// A meeting link was made (at create and in a rotate). No secret.
        MeetingLinkCreate = "meeting_link.create",
        /// A host read a meeting's link. No secret.
        MeetingLinkRead = "meeting_link.read",
        /// A host rotated a meeting's link. No secret.
        MeetingLinkRotate = "meeting_link.rotate",
        /// A host revoked a meeting's link, or the meeting closed. No secret.
        MeetingLinkRevoke = "meeting_link.revoke",
        /// A meeting link was redeemed with its valid secret: ids and the outcome,
        /// never a name, a nonce, a secret or an IP.
        MeetingRedeem = "meeting.redeem",
        /// Someone knocked on a meeting's lobby (a redeem's new entry, or a
        /// signed-in member with the link): ids only, never a name, a nonce, a secret or an IP.
        LobbyKnock = "lobby.knock",
        /// A host let a lobby entry in (`details {meeting_id, entry_id, member_id,
        /// all}`).
        LobbyAdmit = "lobby.admit",
        /// A host turned a lobby entry away (`details {meeting_id, entry_id,
        /// member_id, blocked}`).
        LobbyDeny = "lobby.deny",
        /// An App's quotas changed over mgmt (`details {app_id, before, after}`, each the
        /// set quotas).
        AppQuotasSet = "app.quotas_set",
        /// K2 set an App's settings (`details {app_id, fields, hidden_from_home,
        /// display_name, origin}`, the values after the write).
        AppSettingsSet = "app.settings_set",
        /// K2 set or cleared the install's home client (`details {app_id, previous}`).
        HomeClientSet = "home_client.set",
        /// A Space was made private or public again (`details {private}`).
        SpaceVisibility = "space.visibility",
        /// Sew 0.8.0: K2 linked a participant App to a Space (`details {app_id,
        /// governing_app_id, k2_actor}`).
        SpaceAppLink = "space.app_link",
        /// Sew 0.8.0: K2 removed a participant App's link (`details {app_id,
        /// governing_app_id, k2_actor}`).
        SpaceAppUnlink = "space.app_unlink",
    }
}

unit_enum! {
    /// Who may read an audit row. Every [`Action`] has one
    /// ([`Action::visibility`]); a test fails if one is missing.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum AuditVisibility {
        /// Rows of one space: `VIEW_AUDIT_LOG` there, the install owner, compliance, service.
        Space = "space",
        /// Install-level rows: the install owner, compliance, service.
        Install = "install",
        /// Legal holds, retention, eDiscovery, compliance reads, flag changes, audit reads:
        /// compliance-flag holders and the mgmt service only. An owner without the flag cannot
        /// learn that a hold or an investigation exists.
        Sealed = "sealed",
    }
}

impl Action {
    /// The action's read class, independent of any row. `Space`-class
    /// actions are readable at space level only when the row carries a `space_id`; see
    /// [`Action::row_visibility`].
    pub const fn visibility(self) -> AuditVisibility {
        use Action as A;
        match self {
            // Sealed: holds, retention, eDiscovery, compliance reads, flag changes, audit reads.
            A::LegalHoldCreate
            | A::LegalHoldUpdate
            | A::LegalHoldRelease
            | A::RetentionSet
            | A::RetentionPreview
            | A::RetentionCancel
            | A::RetentionActivate
            | A::RetentionRunRequested
            | A::RetentionRun
            | A::ComplianceRead
            | A::EdiscoveryExport
            | A::EdiscoveryComplete
            | A::EdiscoveryFail
            | A::EdiscoveryCancel
            | A::EdiscoveryDownload
            | A::EdiscoveryDelete
            | A::EdiscoveryExpire
            | A::MemberComplianceSet
            | A::AuditRead
            | A::RecordingDownload
            | A::RecordingTranscriptRead => AuditVisibility::Sealed,
            // Install: exports, imports, keys, install ops, policies, members, owner, jobs,
            // audit verification, mgmt, auth.
            A::ExportStart
            | A::ExportComplete
            | A::ExportFail
            | A::ExportCancel
            | A::ExportDownload
            | A::ExportDelete
            | A::ExportExpire
            | A::ImportStart
            | A::ImportComplete
            | A::ImportIdentityMap
            | A::KeyRotateMaster
            | A::KeyShred
            | A::KeyRollover
            | A::KeyRewrap
            | A::KeyRestoreFromMirror
            | A::KeyMirrorStale
            | A::KeyResurrectedRefused
            | A::KeysMasterExported
            | A::KeysKeyringExported
            | A::KeysKeyringImported
            | A::InstallBackup
            | A::InstallRestore
            | A::InstallDrill
            | A::InstallUpgrade
            | A::MediaInstall
            | A::MediaEnable
            | A::MediaDisable
            | A::MediaUninstall
            | A::MediaKeysRotate
            | A::MediaTurnCert
            | A::MediaUpgrade
            | A::RecordingInstall
            | A::RecordingUpgrade
            | A::RecordingSetModel
            | A::RecordingEngineAnomaly
            | A::RecordingBindingAnomaly
            | A::RecordingConsentViolation
            | A::BackupStart
            | A::PolicySet
            | A::MemberProvision
            | A::MemberUpdate
            | A::MemberDeprovision
            | A::MemberErase
            | A::MemberEraseComplete
            | A::MemberErasePreview
            | A::MemberEraseReplayed
            | A::PrincipalRelink
            | A::IdentitySwitch
            | A::MemberInstallRoleSet
            | A::MemberProfileReset
            | A::MemberAvatarPush
            | A::OwnerSet
            | A::JobDead
            | A::JobRetry
            | A::JobCancel
            | A::AuditVerifyFailed
            | A::AuditVerifyRequested
            | A::MgmtAuthFail
            | A::SessionCreate
            | A::SessionRenew
            | A::SessionRevoke
            | A::PassReject
            | A::LoginFail
            | A::PasswordSet
            | A::PasswordSetupLink
            | A::AgentPin
            | A::AgentUnpin
            | A::AgentDeliveriesRead
            | A::SecretSet
            | A::SecretClear
            | A::ProfileFieldCreate
            | A::ProfileFieldUpdate
            | A::ProfileFieldDelete
            | A::ProfileFieldReorder
            | A::SearchRebuildRequested
            | A::SearchRebuild
            | A::SearchCompact
            | A::GuestConvert
            | A::GuestSuspendAll
            | A::GuestRestoreAll
            | A::MailTest
            | A::NotifyDigestUnsubscribe
            | A::VoiceConfig
            | A::VoiceKeysRotate
            | A::VoiceMediaPath
            | A::CallE2eeKeysCreated
            | A::CallE2eeMemberRemoved
            | A::CallE2eeGroupReset
            | A::CallE2eeRebuilt
            | A::CallE2eeRecorderCertified
            | A::CallE2eeRosterReport
            | A::CallE2eeSenderStuck
            | A::CallE2eeEngineAnomaly
            | A::DeviceEnroll
            | A::DeviceRenew
            | A::DeviceRevoke
            | A::AppQuotasSet
            | A::AppSettingsSet
            | A::HomeClientSet => AuditVisibility::Install,
            // Space: everything else (readable at space level when the row names a space).
            A::ApiCall
            | A::GuestSpaceRemove
            | A::SpaceCreate
            | A::SpaceUpdate
            | A::SpaceVisibility
            | A::SpaceUnbind
            | A::SpaceAppLink
            | A::SpaceAppUnlink
            | A::SpaceRelease
            | A::SpaceGovernance
            | A::CategoryCreate
            | A::CategoryUpdate
            | A::CategoryDelete
            | A::ChannelCreate
            | A::ChannelUpdate
            | A::ChannelArchive
            | A::ChannelUnarchive
            | A::ChannelSync
            | A::RoleCreate
            | A::RoleUpdate
            | A::RoleDelete
            | A::RoleAssign
            | A::RoleUnassign
            | A::OverwriteSet
            | A::OverwriteDelete
            | A::MemberTimeout
            | A::MemberUntimeout
            | A::SpaceMemberRemove
            | A::SpaceMemberBan
            | A::SpaceMemberUnban
            | A::ChannelSlowModeSet
            | A::MessageModeratorDelete
            | A::MessageBanWipe
            | A::ThreadDelete
            | A::ReactionModeratorRemove
            | A::SpaceTransfer
            | A::SpaceMemberAdd
            | A::SpaceMemberSync
            | A::ChannelMemberAdd
            | A::ChannelMemberRemove
            | A::ChannelAdminJoin
            | A::RoleReorder
            | A::UserGroupCreate
            | A::UserGroupUpdate
            | A::UserGroupDelete
            | A::UserGroupMembersAdd
            | A::UserGroupMembersRemove
            | A::AttachmentModeratorRemove
            | A::EmojiCreate
            | A::EmojiUpdate
            | A::EmojiDelete
            | A::SpaceIconSet
            | A::SpaceIconClear
            | A::MessagePreviewsSuppress
            | A::MessageOnBehalfApproval
            | A::BookmarkCreate
            | A::BookmarkUpdate
            | A::BookmarkDelete
            | A::BookmarkReorder
            | A::PollModeratorClose
            | A::ScheduledMessageSend
            | A::VoiceJoin
            | A::VoiceLeave
            | A::VoiceServerMute
            | A::VoiceServerDeafen
            | A::VoiceMove
            | A::VoiceDisconnect
            | A::CallStart
            | A::CallEnd
            | A::RecordingStart
            | A::RecordingStop
            | A::RecordingConsent
            | A::RecordingAudioRetention
            | A::RecordingAudioPurge
            | A::RecordingRecorderRejoin
            | A::RecordingSettings
            | A::TranscriptFinal
            | A::MeetingCreate
            | A::MeetingUpdate
            | A::MeetingCancel
            | A::MeetingEnd
            | A::MeetingLock
            | A::MeetingRemove
            | A::MeetingLinkCreate
            | A::MeetingLinkRead
            | A::MeetingLinkRotate
            | A::MeetingLinkRevoke
            | A::MeetingRedeem
            | A::LobbyKnock
            | A::LobbyAdmit
            | A::LobbyDeny => AuditVisibility::Space,
            // A newer peer's action: only the fullest readers see it.
            A::Unknown => AuditVisibility::Sealed,
        }
    }

    /// The class of a stored row: a `Space`-class action without a `space_id` is `Install`
    /// ("every action without a `space_id`" is install-level).
    pub const fn row_visibility(self, has_space: bool) -> AuditVisibility {
        match self.visibility() {
            AuditVisibility::Space if !has_space => AuditVisibility::Install,
            v => v,
        }
    }

    /// Every action of `class`.
    pub fn of_class(class: AuditVisibility) -> Vec<Action> {
        Action::ALL
            .iter()
            .copied()
            .filter(|a| a.visibility() == class)
            .collect()
    }
}

/// How an act-as call reached Sew: through K2, for an agent, in a K2 session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActorVia {
    /// The delegating system; `"k2"` in v1.
    pub system: String,
    /// The K2 workspace uuid of the agent.
    pub agent: String,
    /// The K2 session id.
    pub k2_session: String,
}

/// Who performed an audited action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Actor {
    /// The acting member, when a member acted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_id: Option<Id>,
    /// What kind of principal acted.
    pub kind: ActorKind,
    /// Set on act-as calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<ActorVia>,
    /// For an assistant, the member id of its person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<Id>,
    /// The mgmt caller's `SO_PEERCRED` uid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_uid: Option<u32>,
    /// The client IP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
}

/// What an audited action touched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AuditTarget {
    /// The entity kind, `lower_snake` (for example `channel`).
    pub kind: String,
    /// The entity id.
    pub id: Id,
}

/// One row of the append-only audit log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AuditEvent {
    /// The event id.
    pub id: Id,
    /// When it happened.
    pub at: Timestamp,
    /// Who acted.
    pub actor: Actor,
    /// What was done.
    pub action: Action,
    /// What it touched; absent when there is no target (for example a failed login).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<AuditTarget>,
    /// The space involved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<Id>,
    /// The channel involved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<Id>,
    /// How it ended.
    pub result: AuditResult,
    /// Ids, names, old and new values, permission bits. Never content.
    #[serde(default)]
    pub details: Map<String, Value>,
}

/// Why canonical JSON could not be produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalError {
    /// A float appeared; canonical JSON holds integers only.
    Float,
    /// Serialization failed.
    Serialize(String),
}

impl fmt::Display for CanonicalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CanonicalError::Float => {
                f.write_str("canonical JSON holds integers only; found a float")
            }
            CanonicalError::Serialize(e) => write!(f, "canonical JSON serialization failed: {e}"),
        }
    }
}

impl std::error::Error for CanonicalError {}

impl AuditEvent {
    /// The canonical bytes hashed into the audit chain: object keys sorted by UTF-8 bytes at
    /// every level, no whitespace, serde_json string escaping, integers only.
    pub fn canonical_json(&self) -> Result<Vec<u8>, CanonicalError> {
        let value =
            serde_json::to_value(self).map_err(|e| CanonicalError::Serialize(e.to_string()))?;
        canonical_json(&value)
    }
}

/// Writes any JSON value in the canonical form used by the audit chain.
pub fn canonical_json(value: &Value) -> Result<Vec<u8>, CanonicalError> {
    let mut out = Vec::new();
    write_canonical(value, &mut out)?;
    Ok(out)
}

fn write_string(s: &str, out: &mut Vec<u8>) -> Result<(), CanonicalError> {
    let quoted = serde_json::to_string(s).map_err(|e| CanonicalError::Serialize(e.to_string()))?;
    out.extend_from_slice(quoted.as_bytes());
    Ok(())
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) -> Result<(), CanonicalError> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                out.extend_from_slice(u.to_string().as_bytes());
            } else if let Some(i) = n.as_i64() {
                out.extend_from_slice(i.to_string().as_bytes());
            } else {
                return Err(CanonicalError::Float);
            }
        }
        Value::String(s) => write_string(s, out)?,
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push(b'{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_string(key, out)?;
                out.push(b':');
                if let Some(v) = map.get(key) {
                    write_canonical(v, out)?;
                }
            }
            out.push(b'}');
        }
    }
    Ok(())
}
