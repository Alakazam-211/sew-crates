# Changelog

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
