# Changelog

## 0.2.0 (2026-11-04, Sew 0.7.3)

- Needs sew-call-client 0.2.0. Breaking: `JoinRequest` has `recording_notice` (below).

- Recording consent: nobody is recorded without saying yes, and no answer is never a yes.
  - Commands `consent {granted, notice_version}` and `withdraw_consent` ("Stop recording me"),
    with their permissions (`allow-consent`, `allow-withdraw-consent`, both in
    `sew-voice:default`).
  - The `sew-voice://recording` event (`RecordingInfo`: the recording, `my_consent`, the notice
    as Sew's `consent_prompt` while pending, the participants' answers, `recorder_present`),
    `null` once none is active; the `state` command's `recording`.
  - A join into a recorded room is held: `join` answers with an empty `call_id` and the state
    is `consent_required {recording_id, notice_version}` (no microphone is opened) until
    `consent` or `leave`; `call_id` follows once Sew lets the join in.
  - The recording leave reasons `recording_declined`, `recording_withdrawn` and
    `recording_consent_timeout` (`wire::reason::RECORDING`), and the command errors
    `no_recording`, `recording_consent_required` (the notice changed; `detail` has the new
    one) and `consent_unsupported`.
  - In Rust: `CallState::ConsentRequired`, `PluginEvent::Recording`, `StateSnapshot::recording`,
    the engine's `CallEvent::Recording` and `CallSnapshot::recording`, and
    `CallHandle::consent` / `withdraw_consent` (defaults answer `consent_unsupported`, so other
    engines keep compiling). `fake::FakeProvider::refuse_next`, `fake::FakeCall::set_recording`,
    `consents`, `fail_next_consent` and `fake::consent_required` drive it without Sew.
  - `JoinRequest` has `recording_notice` (sew-call-client's; the provider forwards it).
  - The contract fixture (`tests/native-voice-contract.json`) has the `recording` payloads, the
    held `state_snapshot` and `ended.recording_reasons`.

## 0.1.0 (2026-11-03)

- First published version.
- A Tauri 2 plugin with six commands (`join`, `leave`, `mute`, `devices`, `set_device`, `state`),
  five events (`sew-voice://state`, `participants`, `speaking`, `devices`, `errors`) and their
  permission files.
- The real engine over `sew-call-client` (`real::CallClientEngine`) and an in-memory fake engine
  (`fake`).
- The host's `JoinProvider` keeps the sign-in in Rust; `Builder::device_key_store` takes the
  host's own device key store.
- The JSON contract is pinned in `tests/native-voice-contract.json`.
- `EngineError` and the engine's `CallEvent` are `#[non_exhaustive]`.
- Licence: MIT OR Apache-2.0.
- The `state` event and the `state` command carry the call's `target` and `room` (from
  `connecting` until `ended`; absent while `idle`). A moderator move sends a `state` event with
  the destination `room`. Additive on the wire: both fields are written only when present.
  In Rust, `PluginEvent::State` holds a `StateEvent` (the `CallState` plus `target` and
  `room`), `StateSnapshot` has `target` and `room`, the engine's `CallEvent` has `Moved {room}`
  and `CallHandle` has `moved_room()` (default `None`).
- `set_device` takes `"default"` (`DEFAULT_DEVICE`) for the OS default, followed when it
  changes. `VoiceEngine::set_device` takes `Option<DeviceChoice>` (`Default` or `Id`).
- The default engine lists the audio devices on first use (the first `devices`, `set_device`,
  `join` or `device_events`), not at app launch, and polls every 2 s only between calls while
  something is subscribed. `VoiceService::start` no longer subscribes at once.
- `StoredDevice` is re-exported, so a host's `DeviceKeyStore` needs no sew-e2ee dependency.
