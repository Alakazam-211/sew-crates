# Changelog

## 0.4.0 (2026-11-05, Sew 0.8.0)

- Needs sew-proto 0.8 (and sew-e2ee, sew-lkproto and sew-lk-compat 0.8). The crate re-exports
  sew-proto's recording types (`ConsentPrompt`, `Recording`, `RecordingNotice`,
  `RecordingRef`), so the new sew-proto is a breaking change here. sew-proto 0.8 adds the
  voice leave reason `app_unlinked` (an App's link to the Space was removed); a call that
  ends with it reports the server's reason as the other leave reasons do.
- Each new media session (the first connection and every full reconnect) logs its first RTP
  send sequence at info (`first_seq`, target `sew_call_client`), so a log shows that a new
  connection starts a fresh sequence.
- No other API change.

## 0.3.0 (2026-11-05, Sew 0.7.4)

- The hardware device key (phones). `device::HardwareDeviceKey` (async `public_key`,
  `generate`, `sign`, `delete`) puts the device's private key in the Secure Enclave or
  StrongBox/TEE: `ClientConfig::with_hardware_device_key`, `StoreChoice::Hardware`
  (`KeyStorage::Hardware`). The host's `sign` returns the platform's DER signature; the engine
  converts it to `r‖s`, normalises low-S and checks it under the key's public key
  (`device::p1363_from_der`). The public record stays in the data directory, with no key file
  and no `key_storage_file` warning. No software fallback: a key error is
  `DeviceError::Unavailable` and fails the key setup (`e2ee_refused`), or ends the key session
  mid-call (`KeyEnd::Device`). A key whose `public_key` answers `None` enrols a new device.
  Signatures are made with the key session's lock released.
- The enrolment platform from `target_os` (`ios`, `android`, otherwise `desktop`;
  `device::default_platform`) instead of always `desktop`: `ClientConfig::device_platform` /
  `with_device_platform`, `MlsKeys::with_platform`, `DeviceVault::platform`.
- Feature `sim-software-key` (simulator and x86 emulator builds only; any other target refuses
  to compile with it): `device::SimSoftwareKey`, a software key behind `HardwareDeviceKey`.
- `DeviceVault::open_async` (the open plus a hardware key's `pins_reset` probe) and
  `DeviceVault::device_key`. `DeviceVault::signer` answers `None` with a hardware key.
- Breaking: `VaultConfig` has `platform` (struct literals need it), and
  `DeviceVault::ensure_device` / `enroll` return a `device::DeviceKey` (software or hardware;
  `sign` is async) instead of sew-e2ee's `SoftwareSigner`.
- Phones, additive:
  - `rust-crypto` feature: the call PC's DTLS/SRTP provider is pure Rust (dimpl's RustCrypto
    backend for DTLS 1.2, RustCrypto AES-CTR/HMAC-SHA1 and AES-GCM for the three SRTP
    profiles, a self-signed P-256 certificate from rcgen on ring), and the E2EE frame cipher
    too (sew-lk-compat's `rust-crypto`: LkGcm on RustCrypto). `mobile` turns it on.
    `CRYPTO_BACKEND` says which provider a build uses. On iOS and Android aws-lc-rs is never
    built, and the crate refuses to compile there without `mobile` (or `rust-crypto`). TLS
    stays on rustls with ring.
  - The TURN credential HMAC-SHA1 is the `hmac` and `sha1` crates on every target (was
    aws-lc-rs); the crate no longer depends on aws-lc-rs directly.
  - `android_init(env, context)` (Android): passes the JNI env and Context to
    rustls-platform-verifier; call it before the first join. `jni` is re-exported.
  - `Call::network_changed()`: the host's path monitor says the network moved; the engine
    resumes now on a fresh transport (a new TURN allocation over a new socket on the new
    network), swapped into the kept PC, then restarts ICE; the call socket reconnects and
    resumes too. A failed open or a restart that doesn't connect within 5 s falls back to the
    full reconnect. `CallCounters::network_changes`. `KeySession::network_changed` (default:
    nothing) and `CallSocket::reconnect`, `MlsSession::network_changed`.
  - Test only: `TestingOverrides::source_ip` (the transport's source address).
- Fix: after a full reconnect (a new PC) the other members could stop hearing this device
  for the rest of the call. The microphone's RTP sequence number ran on across PCs, so once
  it had passed 65535 the new session's first packet carried rollover counter 1, which the
  media server's new SRTP context cannot know (it starts a new SSRC at 0): every packet failed
  SRTP authentication there (str0m logged "First SeqNo has non-zero ROC"). Each new PC now
  starts at a fresh random sequence number below 2^15 (rollover 0); a resume keeps its PC and
  its count. The receive side was already fresh per PC.
- Testing (feature `testing`): `TestingOverrides::first_seq` sets the first sequence number
  of each new media session. `TestingOverrides` gained a field: struct literals need
  `..TestingOverrides::default()`.
- A remote frame whose key index holds no key yet is no longer a decrypt failure: around a join
  the others still send under the old epoch until `activate`, and a new epoch's frame can beat
  its install. Such frames are dropped and counted in `CallCounters::frames_before_key` (and
  `MediaCounters::frames_before_key`, `RecvRefused::KeyPending`, `CryptoError::KeyPending`) for
  `KEY_PENDING_GRACE_MS` (2 s) of the sender's failing streak; a key still missing after that,
  a tag that does not verify and a malformed frame stay in `decrypt_failures` (and the beacon's).
  A frame of a sender no longer verified is now `refused_frames`, not a decrypt failure.
- `CallStatsReport::before_first_window` (`BeforeFirstWindow`): the counts from before the call
  first connected, which no window covers, so each counter is that bucket plus the windows.
  `StatsWindow` has `frames_before_key` and `refused_frames` (the host's view; the sample sent to
  Sew is unchanged).
- Fix: a call Sew ends for a reason the key protocol has no code for (`media_path_down`, the
  recording reasons, …) ended with the key group's `disconnected` when its removal came
  first. The key group's removal is now `E2eeEvent::KeysRemoved` (new; `E2eeEvent` is
  non-exhaustive): media stops at once, and the call ends with the reason the call socket
  brings within 1.5 s (its self copy of `voice_state_updated` or its `4011` close), else with
  the key group's. After the engine's `Disconnect`, the key group's reason replaces `kicked` /
  `room_closed` as the fallback.
- Fix: the engine drops a removed participant's TURN allocation as it sends the `Disconnect`
  leave, so the client could see its media go first, drop the signalling link and lose the
  leave: the call then re-joined, and ended `network` (or with the re-join's refusal) instead
  of `kicked` / `room_closed` / Sew's reason. After media is lost, the old signalling link is
  now read until it closes (at most 1.5 s) and a re-join waits for it; its `Disconnect` leave
  (a removal or a closed room) ends the call as before. Reconnects are not delayed.
- Call-credential renewal: each renewal is scheduled at half the life the token has left (the
  join's, each renewal's, each re-join's), and logged at `info` when it goes through. A renewal
  that fails on the network, with a `5xx` or a `429` is retried after 1 s, doubling to 8 s and
  never later than half the time the credential has left (inside the 30 s Sew keeps the old
  secret), until the credential runs out; it was retried every 10 s, past the expiry and
  forever. New `EndReason::CredentialExpired` (`credential_expired`): Sew refused the renewal
  `expired`, or none got through before the credential ran out (it ended `credential_lost`, or
  not at all). When Sew's call socket ends the join while a renewal is in flight, the engine
  waits for the renewal's answer: a `signed_out` refusal ends the call `signin_required` (it
  ended with the socket's `session_revoked`, a revocation the person did not make); otherwise
  Sew's leave reason stands.

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
