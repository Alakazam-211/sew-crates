# Changelog

## 0.11.5 (2026-11-25)

- The version follows Sew 0.11.5. No API or behaviour change.

## 0.11.4 (2026-11-22)

- The version follows Sew 0.11.4. No API or behaviour change.

## 0.11.3 (2026-11-21)

- The version follows Sew 0.11.3. No API or behaviour change.

## 0.11.2 (2026-11-20)

- The version follows Sew 0.11.2. No API or behaviour change.

## 0.11.1 (2026-11-20)

- The version follows Sew 0.11.1. No API or behaviour change.

## 0.11.0 (2026-11-19)

### Breaking

- **The generated `google.protobuf.Timestamp` fields change type**, from
  `Option<pbjson_types::Timestamp>` to `Option<sew_lkproto::Timestamp>` (the new
  `sew_lkproto::Timestamp`). The fields (14):
  - `MetricsBatch.normalized_timestamp`
  - `MetricSample.normalized_timestamp`
  - `EventMetric.normalized_start_timestamp` and `EventMetric.normalized_end_timestamp`
  - `MetricsRecordingHeader.start_time` and `MetricsRecordingHeader.room_start_time`
  - `RtpDrift.start_time` and `RtpDrift.end_time`
  - `RtpStats.start_time`, `RtpStats.end_time`, `RtpStats.last_pli`, `RtpStats.last_fir`,
    `RtpStats.last_key_frame` and `RtpStats.last_layer_lock_pli`

  The protobuf and JSON encoding is unchanged: `Timestamp` has the same fields (`seconds`,
  `nanos`), the same protobuf message (`seconds` field 1, `nanos` field 2) and the same JSON
  (an RFC 3339 string in UTC with a `+00:00` offset and as many fraction digits as the value
  needs: none, 3, 6 or 9). Code that names `pbjson_types::Timestamp` for these fields uses
  `sew_lkproto::Timestamp` instead. The crate no longer depends on `pbjson-types` (nor,
  through it, on `chrono`).

## 0.10.6 (2026-11-19)

- The version follows Sew 0.10.6. No API or behaviour change.

## 0.10.5 (2026-11-18)

- The version follows Sew 0.10.5. No API or behaviour change.

## 0.10.4 (2026-11-17)

- The version follows Sew 0.10.4. No API or behaviour change. The package's metadata and
  README follow the published-crate standard (issues, MSRV, features, a compiling example that
  runs as a doc test).

## 0.10.3 (2026-11-16)

- The version follows Sew 0.10.3. No API or behaviour change.

## 0.10.2 (2026-11-16)

- The version follows Sew 0.10.2 (there is no 0.10.1). No API or behaviour change.

## 0.10.0 (2026-11-15)

- The version follows Sew 0.10.0. No API or behaviour change.

## 0.9.3 (2026-11-12)

- The version follows Sew 0.9.3. No API or behaviour change.

## 0.9.2 (2026-11-11)

- The version follows Sew 0.9.2. No API or behaviour change.

## 0.9.0 (2026-11-10)

- The version follows Sew 0.9.0. No API or behaviour change.

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
