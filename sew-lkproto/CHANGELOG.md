# Changelog

## 0.8.3 (2026-11-10)

- The version follows Sew 0.8.3. No API or behaviour change.

## 0.8.2 (2026-11-06)

- The version follows Sew 0.8.2. No API or behaviour change.

## 0.8.1 (2026-11-06)

- The version follows Sew 0.8.1. No API or behaviour change.

## 0.8.0 (2026-11-05)

- The version follows Sew 0.8.0. The README is revised. No API or behaviour change.

## 0.7.4 (2026-11-05)

- The version follows Sew 0.7.4. The README is revised. No API or behaviour change.

## 0.7.3 (2026-11-04)

- The version follows Sew 0.7.3. No API or behaviour change.

## 0.7.2 (2026-11-03)

- First published version.
- The LiveKit client protocol 17 types (`livekit`, `logger`), generated with prost and pbjson from
  livekit/protocol @ a4f4b5c0c23f167d0c07de1e1d7652125882e73c; the `.proto` sources are in
  `proto/`.
- `version`: the protocol-version gates (`ProtocolVersion`, `CURRENT_PROTOCOL` 17,
  `MIN_CLIENT_PROTOCOL` 13, `negotiate`, the `GATES` table), rewritten from a written behaviour
  specification.
- Licence: Apache-2.0, with LiveKit's NOTICE for the generated code.
