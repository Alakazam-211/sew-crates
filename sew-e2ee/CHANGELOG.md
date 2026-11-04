# Changelog

## 0.7.3 (2026-11-04)

- The version follows Sew 0.7.3. No API or behaviour change.

## 0.7.2 (2026-11-03)

- First published version.
- The client side of Sew's end-to-end encrypted calls: device identity and certificates, trust
  anchors and pins, key packages, the per-call MLS session (`CallSession`, openmls 0.9.0 with
  `sew-mls-crypto`), per-sender frame keys in a 64-slot ring with a two-phase key switch,
  recovery, the recorder key handoff and recording-chunk opening, and the key-event contract for
  SDK bridges.
- Sans-I/O; builds for Linux, macOS and wasm32.
- Feature `testing`: an in-process delivery-service model and a simulated SDK, for tests only.
- Licence: MIT OR Apache-2.0.
