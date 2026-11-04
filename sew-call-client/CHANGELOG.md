# Changelog

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
