# Changelog

## 0.2.0 (2026-11-04, Sew 0.7.3)

- Recording consent (needs Sew 0.7.3's call socket for mid-call updates). A
  native member is recorded only after Sew says they granted, and no answer is never consent:
  - The engine feeds the call keys (`KeySession::recording`, sew-e2ee's `Input::Recording`)
    from Sew's word only: the join answer's and a move's `recording`, the call socket's own
    `recording_updated` and its READY's `recording`, and Sew's answer to a consent. The keys
    release this device's key to the recorder only while it says `granted`, and rekey at once
    on a withdrawal. Every new key session (a move, a re-join) gets the state again.
  - A join refused `409 recording_consent_required` no longer fails: `CallClient::join`
    returns the call held at `CallState::ConsentRequired { recording_id, notice_version }`
    (no media, no keys, nothing captured), for as long as the host takes. `Call::consent`
    joins again with the answer (`JoinRequest::recording_notice`); `Call::leave` gives up. A
    re-join refused the same way holds the same way.
  - `Call::recording()` and `CallEvent::Recording` (`CallRecording`: the recording, this
    person's `RecordingConsent`, the notice while pending, who is recorded);
    `CallSnapshot::recording`; `Participant::recorder` marks the recorder.
  - `Call::consent(granted, notice_version)` and `Call::withdraw_consent()` ("Stop recording
    me") in a call go through the host: `JoinProvider::consent(ConsentAnswer)`, sent with the
    person's own session (`ConsentAnswer::path`, `body`, `recording_from_json`). Its default
    answers `consent_unsupported`. `CallError::NoRecording` when there is nothing to answer.
  - `ProviderError::ConsentRequired(Box<ConsentRequired>)` and
    `ProviderError::from_sew_error(status, retry_after_ms, body)`, which reads Sew's error body
    (`401`, the `409` with its prompt, `429`/`5xx`, refusals by `details.reason`).
  - Breaking: `JoinRequest` has `recording_notice` (written to its JSON only when set; struct
    literals need `recording_notice: None`). Sew's recording types are re-exported
    (`ConsentPrompt`, `Recording`, `RecordingNotice`, `RecordingRef`).
  - The call socket passes `recording_updated` on (`SocketEvent::RecordingUpdated`) and reads
    the call socket READY's `recording` (`CallReadyInfo::recording`).

- The call-stats beacon: when the join answer carries Sew's `stats` terms (the
  `calls.stats_beacon` policy), the engine sends one `CallStatsSample` per window
  (`interval_ms`, 15 s) and one when the host leaves, to `POST /api/v1/me/voice/stats` with the
  call credential. `client: "native"` and the `audio_*` fields (played, concealed, packets
  expected and lost, the worst stream's jitter, the longest gap) are sent only when the terms
  say `audio_fields: true`; otherwise the sample has the browsers' shape (`client: "other"`).
  Values are clamped to Sew's caps; Sew's `call_stats_off` stops the beacon for the call. The
  `stats` module documents how each value is derived.
- `Call::stats()` (`CallStatsReport`: the terms and the latest 256 `StatsWindow`s, each with the
  numbers measured, the sample sent and Sew's answer as a `BeaconOutcome`) and
  `CallEvent::Stats` per window; the windows are measured whether or not the beacon is on.
- Played and concealed audio come from kessel-audio's counters when the call runs through
  `AudioBridge`; a host with its own audio pipeline reports them with
  `Call::set_playout_stats` (`PlayoutStats`, `PlayoutTotals`), else the engine counts the frames
  and loss marks it handed over (`PlayoutSource`).

- A refused call-credential renewal is read by Sew's `error.details.reason` (it was read from a
  top-level `details` that Sew never sends, so every `401` ended the call `credential_lost`):
  `signed_out` now ends it `signin_required`, as documented.

## 0.1.0 (2026-11-03)

- First published version.
- `CallClient` and `Call`: one Sew voice call at a time, joined through the host's `JoinProvider`,
  with resume, full reconnect within a 20 s window and re-join.
- Transport on str0m with TURN over UDP, TCP and TLS, and a relay-only mode.
- Mandatory end-to-end encryption: MLS call keys over the call credential and the call socket, the
  publication and subscription checks, and the device key store (a 0600 file by default, the
  host's own store, or the OS credential store with `keyring-store`).
- Call states, end reasons (Sew's voice leave reason passed through as `EndReason::Removed`),
  participants with `speak_denied`, speaking levels and errors, as events.
- The `kessel-audio` bridge (`audio`, `audio-core`), built against kessel-audio 0.1.0 at
  Alakazam-211/k2 @ bc78b69a6640a533d11edf1be2feaa615416cda8.
- Public enums and read-only structs are `#[non_exhaustive]`; `CallErrorEvent::new` builds an
  error event.
- Licence: MIT OR Apache-2.0.
- `CallEvent::Moved(CallMoved)` and `CallSnapshot::moved`: a moderator move (Sew's
  `voice_server_updated`) and its destination (`channel_id`, `call_id`, `media_room`;
  `CallMoved::room()` is the room a host joins). A re-join after the move asks the provider for
  the destination room.
- `device::StoredDevice` is re-exported from sew-e2ee.
