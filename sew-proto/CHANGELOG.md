# Changelog

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
  `recording::ConsentAsk` (`Join`, `InCall`) and `recording::ConsentChoice`: the web kit's
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
