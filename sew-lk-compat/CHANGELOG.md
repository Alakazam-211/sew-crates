# Changelog

## 0.7.2 (2026-11-03)

- First published version.
- `signal`: the `/rtc/v1` join request, `ClientInfo` and ICE trickles.
- `session`: the sans-I/O signalling session of a single-peer-connection voice client (ping and
  round-trip time, the remote roster, leave and refusal handling). `signal` and `session` were
  rewritten from a written behaviour specification.
- `e2ee`: LiveKit's AES-GCM frame format and the production E2EE configuration; `compat`:
  client-sdk-js version comparison and the validate classifier. The frame format and `compat` are
  ported from livekit/client-sdk-js @ 53dd840.
- Public enums and read-only structs are `#[non_exhaustive]`.
- Licence: Apache-2.0, with LiveKit's NOTICE.
