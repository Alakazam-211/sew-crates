# Changelog

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
