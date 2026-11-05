# Changelog

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
