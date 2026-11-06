# Changelog

## 0.4.1 (2026-11-06, Sew 0.8.1)

Additive. Needs sew-call-client 0.4.1 and sew-proto 0.8.1.

- `Builder::crypto_provider(Arc<CryptoProvider>)`: the rustls crypto provider of the engine's
  TLS. Without it, the process default (`CryptoProvider::install_default`), else ring through
  the new default feature `builtin-ring-provider` (sew-call-client's); with that feature off
  and neither, a join fails with `engine_stopped`, its detail naming `no_crypto_provider`.
- The phone audio backend. On iOS and Android (`sew-call-client/mobile`, with `audio`) a
  call's audio is kessel-audio's OS voice processing (VoiceProcessingIO; AAudio's
  voice-communication preset) with the OS's echo cancellation and noise suppression on;
  desktop keeps cpal. `real::system_audio()` is sew-call-client's `platform_audio_config`.
  kessel-audio's `[patch.crates-io]` row moves to K2 rev
  aad16f76b8768ae090d3060033d473e87c736be4.
- `VoiceControl`'s `session_activated`, `session_deactivated`, `suspend`, `resume` and
  `route_changed` now reach kessel-audio (they did nothing before): the real engine keeps one
  `AudioSession` for the app, so an event before the call applies when its audio starts, and
  on iOS the microphone and speaker open only after `session_activated`. The plugin still never
  configures `AVAudioSession` or the Android audio mode.
- Device keys on sign-out. `VoiceControl::forget_server(install_id)` deletes the devices of one
  Sew install (a hardware key through the host's `HardwareDeviceKey::delete`) and its pins,
  answering the removed `DeviceKeyId`s; refused with `in_call` during a call, `device_store`
  when the store fails. `VoiceControl::known_device_keys()` lists every key the store holds.
  The same as `VoiceService` methods, `VoiceEngine::forget_server` / `known_device_keys`
  (defaults: no devices), and two webview commands, `forget_server {install_id}` and
  `known_device_keys`, answering `[{install_id, member_id, device_scope, account}]`
  (`DeviceKeyInfo`, `ForgetServerArgs`). They are not in `sew-voice:default`: allow them with
  `sew-voice:device-keys` or `sew-voice:allow-forget-server` / `allow-known-device-keys`.
  `DEVICE_KEY_COMMANDS` lists them; `COMMANDS` stays the eight. New error codes `in_call` and
  `device_store`. The fake engine has `set_device_keys`, `fail_next_forget` and `forgets`.

## 0.4.0 (2026-11-05, Sew 0.8.0)

- Needs sew-call-client 0.4 and sew-proto 0.8. The plugin re-exports sew-proto's
  `ConsentAsk` and `ConsentChoice`, so the new sew-proto is a breaking change here. The
  plugin's own API is unchanged.

## 0.3.0 (2026-11-05, Sew 0.7.4)

- Needs sew-call-client 0.3.0 (breaking there: `VaultConfig.platform`, and the vault's
  `ensure_device` / `enroll` return `device::DeviceKey`). The plugin's own API change is additive.

- The native side for phones (additive):
  - `VoiceListener` (`on_state`, `on_participants`, `on_self_mute`, `on_recording`,
    `on_error`, `on_devices`; empty defaults) and `Builder::listener`: called after each
    `sew-voice://` event, in order, under the plugin's lock, with the same payload.
  - `SelfMute` (`muted`, `deafened`, `self_muted`, `server_muted`, `speak_denied`).
  - `VoiceControl`, managed as Tauri state: `join`, `leave` (safe to repeat), `set_muted` and
    `set_deafened` (`not_in_call` without a call, as the webview's `mute`), `consent`,
    `withdraw_consent`, `snapshot`, `network_changed`, and `session_activated`, `session_deactivated`, `suspend`, `resume`
    and `route_changed` (to kessel-audio unchanged).
  - `CallHandle::network_changed` and `VoiceEngine::audio_session` (`AudioSessionEvent`),
    with defaults that do nothing; `VoiceService::with_listener`, `VoiceService::network_changed`
    and `VoiceService::audio_session`; `fake::RecordingListener`.
  - The real engine's call (`CallClientEngine`) forwards `network_changed` to sew-call-client's
    `Call::network_changed`. Its `audio_session` still does nothing: kessel-audio's phone
    session hooks are not in the pinned kessel-audio yet.
  - `ConsentAsk` and `ConsentChoice` re-exported from sew-proto (`ConsentPrompt::title_en`,
    `text_en`, `choices_en`: the web kit's consent words).
- A held join's `state` event (`consent_required`) now comes before its `recording` event with
  the notice (it came after), for a fresh join and a re-join alike.
- iOS and Android: no device enumeration between calls (`CallClientEngine::with_mls` sets no
  lister there; `real::system_devices` is desktop-only), and `keyring-store` refuses to build.
- `tests/sew-native-voice-listener.json`: the listener's exact calls per scenario, checked by
  `tests/listener.rs`.
- `Builder::hardware_device_key(key)`: the device key in the Secure Enclave or StrongBox/TEE
  through `HardwareDeviceKey` (re-exported, with `DevicePlatform`). No software fallback: a key
  error fails the join with `e2ee_refused`; it replaces `Builder::device_key_store`.
- `Builder::device_platform(platform)`: the platform the device enrols as. Default from
  `target_os` (`ios`, `android`, otherwise `desktop`; it was always `desktop`).
- Feature `sim-software-key` (off; iOS simulator and x86 Android emulator builds only):
  `SimSoftwareKey`, a software stand-in for the hardware key.
- Fix (through sew-call-client): a call Sew ends with `media_path_down` (or another reason the
  key protocol cannot name) ends with that reason, not `disconnected`.
- `ended.reason` `credential_expired` (`wire::reason::CREDENTIAL_EXPIRED`, in the contract
  fixture's fallbacks): the call credential expired (Sew refused a renewal `expired`, or no
  renewal got through before it ran out). A renewal Sew refuses `signed_out` ends
  `signin_required`, also when Sew's call socket closed first.

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
