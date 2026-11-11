# Changelog

## 0.6.1 (2026-11-11, Sew 0.9.2)

Additions only. On sew-call-client 0.6.1 and sew-proto 0.9.2 (additive: group DM names,
system messages, DMs by latest activity).

### Added

- Which recording buttons to show: the command `set_recording_rights {room, my_perms, dm?,
  recording_enabled?, recording_dm_enabled?, record_capped?, agent?}` (`RecordingRights`,
  `VoiceControl::set_recording_rights`, `VoiceService::set_recording_rights`;
  `RECORDING_RIGHTS_COMMANDS`, permission `sew-voice:allow-set-recording-rights`, in the set
  `sew-voice:recording`) takes the host's word on the person's rights in the call's room. From
  it and the call (its state and room, who is in it, the recording and who started it) the
  plugin works out whether Sew would take a Record or a Stop now, as Sew checks them: the new
  event `sew-voice://recording_controls` `{can_record, can_stop}` (`RecordingControls`,
  `PluginEvent::RecordingControls`, `wire::RECORDING_EVENTS`) when either changes, the `state`
  command's `recording_controls` (absent while both are `false`), and the `recording` event's
  `can_record` and `can_stop` (`RecordingInfo`'s new fields; each written only when `true` and
  read as `false` when absent, so older payloads still parse). They are worked out again on
  every change of the rights, the state, the room, the participants (a required room's
  headcount) and the recording, and are `false` until the host sends the rights. The rights
  apply while the call is in their `room` and are forgotten when the call ends.
- The fake's `FakeCall::remove_remote`.

## 0.6.0 (2026-11-10, Sew 0.9.0)

Additions, and two breaks (below): `RecordingInfo` gains fields and is now non-exhaustive, and
the plugin is on sew-call-client 0.6 and sew-proto 0.9.

### Breaking

- The `recording` event (and the `state` command's `recording`) carries `trigger`
  (`"manual"`, `"auto"` or `"required"`, `wire::trigger`) and `required` (the room requires its
  recording: Sew refuses Stop `recording_required` while two or more people are in it). Both are
  always written; a payload without them reads as `"manual"` and `false`. `RecordingInfo` gains
  the two fields and is `#[non_exhaustive]`: code outside the crate that built one with a
  struct literal reads it from JSON instead.
- sew-call-client 0.6 and sew-proto 0.9: the types the plugin re-exports from them are theirs
  (see their CHANGELOGs).

### Added

- Record and Stop (Sew 0.9.0): the commands `start_recording` and `stop_recording` (no
  arguments; `null`), outside `sew-voice:default`, with their own permissions
  `sew-voice:allow-start-recording` and `sew-voice:allow-stop-recording` and the set
  `sew-voice:recording` (`RECORDING_COMMANDS`). Each is the person's own act, sent by the host's
  provider (sew-call-client's new `JoinProvider::recording`, `RecordingRequest` and
  `RecordingControl`, re-exported with `RECORDING_UNSUPPORTED`) with the person's session; the
  `recording` event follows Sew's answer. They reject `not_in_call`, `not_in_voice` (the join
  is held for a notice; `code::NOT_IN_VOICE`), `no_recording` (a Stop with none),
  `recording_unsupported` (`code::RECORDING_UNSUPPORTED`: the provider sends none), or Sew's
  refusal as the provider gave it (`recording_active`, `recording_required`, `cap_ceiling`,
  …). `VoiceControl::start_recording` and `stop_recording`, `VoiceService::start_recording`
  and `stop_recording`, and `CallHandle::start_recording` and `stop_recording` (provided:
  `recording_unsupported`). The fake records a Start's member alone and ends it on a Stop
  (`fake::FakeCall::controls`, `fail_next_control`). The contract fixture's
  `recording.controls` has the payloads.

## 0.5.1 (2026-11-10, Sew 0.8.3)

Additions only: nothing in 0.5.0's API changes or goes away.

### Fixed

- Updates the WebRTC dependencies to current upstream releases (dimpl 0.9, upstream str0m
  fixes); recommended for everyone. A host that repeats the `[patch.crates-io]` rows moves the
  str0m rows to `907e050bfa212a23394f4482110acef04cba8fc6` and drops the dimpl row: dimpl 0.9.0
  comes from crates.io.
- After a leave, a join is never refused `already_in_call` by the plugin's own engine. The
  leave reaches the engine through the call's `CallLeaver`, so a camera or screen command
  waiting on a device (a lid-closed MacBook camera, a camera switch whose new camera is slow to
  open) no longer holds it up; before, the leave waited for that command, and the next joins
  were refused until it ended (minutes, or a webview reload). The leave waits at most
  `real::LEAVE_WAIT` for the engine's answer and never for such a command (it is logged when
  it still holds the call past `real::TAKE_WAIT`).
- The real engine's end of a call stops only that call's camera and screen pipelines
  (`VideoBridge::close_call`): a late teardown never stops the next call's camera.
- The real engine offers to receive only the video codecs this device can decode: before each
  join it declares its video backend's OS decoders and the webview's `decode` (below) to the
  engine (`CallClient::set_video_decode`). VP8 is always offered; H.264 only when one of them
  decodes it, and never on Linux. Before, every desktop but Linux offered H.264 whatever the
  webview could decode. Without `decode`, nothing is assumed about the webview.
- The `video` event's `unsupported` is now set: this device can receive none of the track's
  codecs (it was always `false`). Before a track's first frame its `codec` is the first of the
  publisher's codecs this device receives.

### Added

- `join {…, decode?}`: the video codecs the webview decodes (`["vp8", "h264"]`, from its
  WebCodecs probe), for the engine's receive codecs; `JoinOptions::decode` and
  `JoinOptions::with_decode` for a native join; `VoiceEngine::join_decoding` (a provided
  method; the default ignores `decode` and joins).
- The video codec setting, a per-device user setting: the command `set_video_codec_preference
  {preference}` (`"automatic"`, the default; `"prefer_h264"`; `"prefer_vp8"`), with its own
  permission `sew-voice:allow-set-video-codec-preference`, in the `sew-voice:video` set
  (`VIDEO_CODEC_COMMANDS`). It answers the readback, which `video_caps` now also carries as
  `codec_preference` (left out by an engine without video publish): `{setting, effective,
  reason?}`, the codec a camera or screen turned on now publishes in (`"h264"` with a VP8
  backup, or `"vp8"`) and, only when Prefer H.264 can't apply, why (`"not_detected"`: no usable
  hardware H.264 encoder found at run time; `"platform"`: no run-time check on Linux, Android or
  Windows yet, or an iOS before 17.4; `"policy"`: the live call's media policy doesn't allow
  H.264). `"prefer_vp8"` also stops the device
  receiving H.264 from its next join. The engine keeps the setting until the process ends:
  persisting it is the host's job. `VoiceControl::set_video_codec_preference` and
  `video_codec_preference`, `VoiceService::set_video_codec_preference` and
  `video_codec_preference`, `VoiceEngine::set_video_codec_preference` and
  `video_codec_preference` (provided: `video_unsupported`, and no readback), the wire types
  `SetVideoCodecPreferenceArgs` and `CodecPreferenceInfo`, and sew-call-client's
  `VideoCodecPreference` and `CodecPreferenceReason`, re-exported.
- `VoiceEngine::release_calls` (a provided method; the default does nothing): leaves and
  releases a call the plugin lost hold of. `leave` with no live call calls it, and so does a
  join the engine refuses `already_in_call` (then joins once more), so a host recovers without
  a reload. `fake::FakeEngine::strand_call` and `releases` test it.
- `camera {on}` is refused with `no_frames` (`video_code::NO_FRAMES`) when the camera opened
  but gave no frame within 2.5 s; nothing is published until the first frame.
- A camera lost while sending (kessel-video's `DeviceLost`: a Continuity Camera gone) sends
  the `errors` event `camera_lost`, besides `local_video {camera: "off", reason:
  "camera_lost"}`; the track is unpublished and the lease released, and the call goes on.
- `local_video {camera: "paused", reason: "suspended"}` (`video_reason::SUSPENDED`): the
  camera says it is suspended (a MacBook's lid closed), `on` again when it comes back. It is
  no error.
- `camera {on}` without a device, with none chosen, prefers the camera that last gave frames
  (by its stable id) while it is listed and not suspended; else it opens without an id, which
  passes over a suspended default (kessel-video 0.1.5).

### Dependencies

- Needs sew-call-client with `CallLeaver`, `Call::local_id`, `VideoBridge::close_call`,
  `VideoError::NoFrames` and `CapturePause::Suspended`, `CallClient::set_video_decode`, the
  video codec preference (`VideoCodecPreference`, `effective_publish_codec`,
  `h264_encode_detection`) and `VideoTrackView::unsupported`, and kessel-video 0.1.5.

## 0.5.0 (2026-11-06, Sew 0.8.2)

### Breaking

- `StateSnapshot` has new public fields (`video`, `local_video`, `policy`, `has_video`; below),
  so a struct literal written for 0.4.1 no longer compiles. `StateSnapshot` is now
  `#[non_exhaustive]`: build one with the new `StateSnapshot::new(state)` and set its fields,
  so later fields are not breaking.
- `PluginEvent` has new variants (`Security`, `Roster`, `Quality`, `Incoming`, `Video`,
  `LocalVideo`, `VideoDevices`, `Policy`), so an exhaustive `match` written for 0.4.1 no longer
  compiles. `PluginEvent` is now `#[non_exhaustive]`: a `match` outside the crate needs a `_`
  arm, and later events are not breaking.
- Needs sew-call-client 0.5.0 (breaking there: `PublishRefused`'s discriminants) and
  sew-proto 0.8.2.
- kessel-audio comes from crates.io: drop its row from your `[patch.crates-io]` (the str0m and
  dimpl fork rows stay; the str0m row moves to `7faf99d17f34292b5c9f72e67f3d0332cd67c122`).

### Added

- Camera flip fix: switching the camera live (`set_video_device`, front to back on a phone)
  no longer leaves the publication paused with `camera_busy`. The engine closes the old camera
  before it opens the new one, since a phone runs one camera at a time. The new camera starts
  at a keyframe, and a new camera that fails to open brings the old one back.
- Self-view facing: the sink header's `flags` bit 3 (`video::flag::FRONT`, 8) marks a
  self-view frame from a front camera (mirror it) and bit 4 (`video::flag::BACK`, 16) one from
  a back camera; neither means unknown or external. The flag is set frame by frame, so a flip
  lands on the new camera's first frame. `VideoRecvItem::Facing { track, facing }` comes
  before a self-view's first frame and on each camera change.
- `VoiceControl::set_camera(false)` (CallKit's or Telecom's video button) says `local_video`
  `{camera: "off", reason: "system_ui"}`; the webview's own `camera {off}` still says
  `unpublished`, and the background's time limit `background`. `CallHandle::camera_off_because`
  (default: `camera` off) carries the reason. A source that ends on its own reaches
  `local_video` with its reason (`camera_lost`, `user_stopped`, `source_closed`, …).

Video. Additive for the webview and for hosts that implement `VoiceEngine` / `CallHandle`
(every new method has a default meaning "no video"); `PluginEvent` and `CallEvent` gain
variants, and `StateSnapshot` gains fields.

- Eleven commands, behind the new permission set `sew-voice:video` (not in
  `sew-voice:default`): `video_caps`, `video_devices`, `set_video_device`, `camera`,
  `screen_sources`, `screen` (on Windows the OS picker's parent is the calling window),
  `preview`, `video_sink`, `video_sink_update`, `video_sink_close`, `request_keyframe`
  (`VIDEO_COMMANDS`). Four events: `sew-voice://video`, `local_video`, `video_devices`, `policy`.
  The `state` command adds `video`, `local_video` and `policy` while they say something; voice
  payloads are unchanged. `video_caps` answers everything off (backend `null`) without video;
  permissions are `granted`, `denied`, `prompt` or `unsupported`; sink stats are cumulative.
- Frame sinks: one Tauri raw `Channel` per video element, a 32-byte little-endian header
  (`video::FrameHeader`) then the encoded frame; gated in Rust (nothing until a keyframe after
  an open, a reveal, a request or a broken track; a `reset` message on a broken track; nothing to
  a hidden sink; the union of a track's sinks handed to the engine as `TrackSettings`).
- The decoded fallback (`mode: "decoded"`): a sink whose webview can't decode the track's codec
  gets the engine's own decoder (`VoiceEngine::video_decoder`, for a codec `video_caps` lists in
  `decode_codecs`; the `FrameDecoder` seam with `DecoderSpec`, `DecodedFormat`, `DecodedFrame`,
  `DecodeFailure`, `DecoderWake`). Its outputs go out as I420 (kind 2), or RGBA (kind 3) when
  the webview has no WebCodecs (an empty `decode`), fitted to the sink and capped at 640x360 and
  15 fps; a self-view sink of such a webview is decoded too. A decoder error resets it, sends a
  `reset` message and asks for a keyframe. At most 4 decoded sinks (`MAX_DECODED_SINKS`); a fifth
  is refused with the new code `decoded_tiles_full` (`detail.max`). A decoded sink's stats are
  the decoder's. The fake engine decodes with `FakeEngine::enable_fake_decoder` (`FakeDecoders`)
  or any opener (`FakeEngine::set_video_decoder`); `decoded_unsupported` now means the engine
  can't decode the codec either.
- The engine side (module `video`): `CallHandle::video_frames` (the call's
  `VideoRecvItem` channel), `video_snapshot`, `set_video_settings`, `request_keyframe`,
  `video_sink_stats`, `camera`, `screen`; `VoiceEngine::video_caps`, `video_devices`,
  `set_video_device`, `video_device_events`, `screen_sources`, `preview`, `preview_keyframe`;
  `CallEvent::Video`, `LocalVideo`, `Policy`. The fake engine has video
  (`FakeEngine::enable_video`, `FakeCall::push_video` and friends, `RecordingFrameChannel`).
- The real engine (`real::CallClientEngine`) receives video with the new feature
  `video-core` (off by default; `video` adds kessel-video's system backends): `video_frames`
  maps the engine's `Call::video_frames()` item by item (track ids become track sids),
  `video_snapshot` and `CallEvent::Video` are the engine's remote cameras and screens (sent
  again on every `VideoAdded`, `VideoRemoved`, `VideoChanged`), `set_video_settings` keeps one
  engine sink per track for the union of the plugin's sinks (closed when disabled),
  `request_keyframe` goes to it, `video_sink_stats` are kept (not yet in the stats beacon).
  `video_caps` gains `receive` (`true` with `video-core`; `false`, as before, without).
  The camera and screen publish (`video-core`): `camera` and `screen` go through
  sew-call-client's `VideoBridge` over the engine's video backend (the lease, the policy, the
  encoders); the engine's `LocalVideo` and `MediaPolicy` become `local_video` and `policy`, and
  `video_caps` lists the backend's camera, screen and `publish_codecs`.
  The decoded fallback decodes with the engine's video backend (below: kessel-video's system
  backend with `video`, made on first use, or the host's through
  `CallClientEngine::with_video_backend`): `video_caps.decode_codecs` lists what it decodes.
  The adapter is `kessel::open_decoder` (and `kessel::decode_codecs`), with `video-core`.
- Video on phones. New feature `mobile` (sew-call-client's `mobile`). With `video-core` the
  real engine has a video backend: kessel-video's `system` with `video` (on iOS AVFoundation
  and VideoToolbox, on Android Camera2 and MediaCodec), built on first use, or the host's
  (`Builder::video_backend`, `CallClientEngine::with_video_backend`). On Android
  `Builder::android_video_context(AndroidContext)` passes the app's JNI handles
  (`CallClientEngine::with_android_video_context`). `video_devices` lists its cameras and
  permissions, `set_video_device` keeps the choice (the camera `camera {on}` opens when it names
  none). It is the engine's one backend: the camera and screen publish, the native sinks and the
  decoded fallback all use it. kessel-video is 0.1.1 from crates.io (the iOS and Android
  backends, `apple_pixel_buffer`); its `[patch.crates-io]` row is gone.
- Camera permission: commands `camera_permission` and `request_camera_permission` (in
  `sew-voice:video`, now thirteen commands), `VoiceControl::camera_permission` and
  `request_camera_permission`, `VoiceEngine::camera_permission` and
  `request_camera_permission` (default `unsupported`).
- The app's scene: `VoiceControl::app_scene(AppScene::Background | Foreground)`, separate from
  `suspend`/`resume` (audio interruptions only). Background while the app or the video backend
  (`VoiceEngine::video_background_events`, kessel-video's `BackendEvent::Background`) says so,
  foreground when both do. In the background every webview sink is hidden (its track disabled
  at the media server) and only native sinks stay on; back in front the visible sinks restart
  at a keyframe, asked of each track. `VoiceEngine::app_scene` (the real engine tells the
  Android backend) and `CallHandle::app_scene` (the combined signal: the hook for pausing the
  camera publish, later). `VoiceControl::device_orientation(degrees)` for Android
  (`VoiceEngine::device_orientation`).
- Speaker with video: `VoiceListener::video_changed(has_video)` when the call gains or loses
  video (this device's camera or screen on or paused, or an unmuted remote one), right after the
  `video` or `local_video` event; `VoiceControl::has_video()` and the `state` command's
  `has_video` (present only while true). The plugin still never sets the audio route.
- Native sinks (module `native`, `video-core`): `VoiceControl::open_native_sink(target,
  max_size, consumer)` with `NativeSinkTarget::Track(sid)` or `ActiveSpeaker` (follows the
  speaker, at least 1.5 s each, with a keyframe request and a `Switched` event on every
  switch), the media server's layer for `max_size` at 30 fps, and the one sink that stays on in
  the background. H.264 tracks come as access units (`NativePayload::H264`;
  `h264_annex_b_to_avcc`, `h264_parameter_sets` for `CMSampleBuffer`s), VP8 tracks decoded by
  the video backend (`NativePayload::Decoded`, a `CVPixelBuffer` on Apple). `NativeSink`:
  `track`, `set_max_size`, `request_keyframe`, `close` (and on drop), `stats`.
  `VoiceEngine::native_video_backend`. On macOS and iOS (with `video`)
  `NativePayload::apple_pixel_buffer()` answers a decoded frame's `CVPixelBufferRef`, borrowed
  while the frame lives (`CVBufferRetain` it to keep it).
- Capture (`video-core`, over the engine's one video backend): `video_devices` events on
  camera hot-plug and on every `set_video_device`, `screen_sources` (thumbnails as base64 PNGs
  at most 320 px wide), and `preview`: the camera outside a call with one 360p VP8 encoder, its
  frames to `local:camera` sinks with the self-view flag, `preview_keyframe`, and the camera
  closed at once on `{on: false}`. `CallClientEngine::video_capture()` is the engine's
  `VideoCapture`, the one the camera publish opens through: one opener holds the camera, so
  `camera {on}` takes it over from a running preview (the preview's channel ends; the
  self-view goes on from the call's own keyframe), and `preview {on}` while the camera is
  published opens no second one (the call's self-view feeds `local:camera`).
- `set_video_device` during a call switches the published camera at once (the same encoders,
  from a keyframe); without a call it is kept for the next `camera {on}` and `preview`.
- Screen publish: `screen {on, source?}` opens the listed source or the OS picker (over the
  calling window on Windows) at most 1080p15 (lower when `policy.screen` says so), with the
  screen content hint, and publishes it under `self_stream`. A hidden window is
  `local_video {screen: "paused", reason: "hidden"}` (frames stop, the track stays); back,
  every layer starts on a keyframe; stopped from the OS (`user_stopped`), its window closed
  (`source_closed`) or the permission revoked (`screen_denied`) unpublishes it, releases the
  lease and sends `local_video {screen: "off", reason}`. A camera another app took is `paused`
  with `camera_busy`; one that went away is `off` with `camera_lost`.
- `video_caps` gains `self_view` (the `local:camera` / `local:screen` sinks are fed in a call
  as well as by the preview) and `live_switch` (`set_video_device` switches a published
  camera), both `true` in the real engine with a video backend; missing in an older payload,
  both read as `false`.
- In-call self-view: while the camera or a screen is published, its `h` (camera) or `q`
  (screen) layer's plaintext output goes to the `local:camera` / `local:screen` sinks with the
  self-view flag (no second encoder, nothing sent); their keyframe requests go to that layer.
- New codes `screen_not_found` and `capture_failed` (`detail.error`), and `wire::video_reason`
  for `local_video.reason` (`hidden`, `background`, `thermal`, `user_stopped`, `source_closed`,
  `video_disabled`).
- iPhone screen share. New feature `ios-screen-ring` (turned on by `mobile`; iOS only, with
  `video-core`), on kessel-frame-ring 0.1.1 from crates.io. `VoiceControl::screen_share_ring(
  app_group_container)` arms a ring at `<container>/sew-screen.ring` for the call; the app's
  broadcast upload extension writes the screen's NV12 frames into it, and the plugin reads the
  newest about every 10 ms and publishes it as the call's screen (the same publish path as a
  desktop screen, at most 720p). The share ends when the extension finishes or dies, stays
  quiet 2 s without running, or the ring is lost; a quiet running extension keeps its share
  (the last frame again each second). Every frame is checked before use (the extension is
  another process). New listener method `VoiceListener::screen_share_changed(active)`, after
  the `local_video` event that turned the screen on or off (any platform). New
  `CallHandle::screen_share_ring` (default `screen_unsupported`). Module `screen_ring`
  (`RingPump`, `drive`, `ScreenShareSink`) for hosts with their own publish path.
  docs/native-voice-sdk.md §3b gives the extension's contract and Android's MediaProjection
  steps (the app's consent, its `mediaProjection` foreground service, then `screen {on}`).
- Native sinks keep up: the sink's thread runs at the user-interactive QoS class on Apple
  (`pthread_set_qos_class_self_np`; the VP8 decoder's thread starts from it); VP8 frames the
  decoder has no room for (`Busy`) wait in order in a backlog of 8 (`native::DECODE_BACKLOG`)
  and go in as outputs make room, a keyframe supersedes the deltas waiting before it, and an
  overflow drops them with a keyframe asked and the deltas after it skipped (they were dropped
  silently before); a discontinuity resets the decoder instead of reopening it.
  `NativeSink::stats()` answers `NativeSinkStats` (frames in and out, fps, decode time from
  submit to output and its two parts, to the decoder's wake and from the wake to the sink's
  thread, inputs in the decoder, conversion and consumer times, the queue, drops by reason,
  keyframes asked, frames handed over as CPU planes, the step-down), logged every 10 s while
  the sink is open. When OS-buffer conversion fails the sink logs it once and hands over NV12
  planes. The step-down: more than 3 drops within 2 s ask the media server for the 180p layer
  (`native::LOW_LAYER`); 10 s with no drop and a decode under a third of the frame interval go
  back up (the wait doubles after a quick relapse, up to 80 s).
- Camera publish on phones:
  - **No prompt.** The plugin never asks for the camera on its own: `camera {on}`,
    `preview {on}`, `VoiceControl::set_camera` and a join's camera need the permission granted,
    else they reject with the new codes `camera_permission_not_determined` or
    `camera_permission_denied`, opening nothing (`video_code`). `request_camera_permission`
    stays the one call that prompts, on the app's camera button.
  - **Background.** The real engine's `CallHandle::app_scene` pauses the camera publish in the
    background: capture and encoding stop and the track is muted at the media server, keeping
    the publication and the lease; `local_video` says `{camera: "paused", reason:
    "background"}` (`video_changed` stays true). Back in front it unmutes and every encoder
    starts at a keyframe; after 5 minutes in the background the camera turns off with
    `{camera: "off", reason: "background"}`. A camera turned on in the background pauses at
    once.
  - **CallKit and Telecom.** `VoiceControl::join_with(args, JoinOptions)` (`JoinOptions::
    with_camera()`, `.camera_device(id)`): the camera turns on as soon as the call is connected
    and the app is in front (never from the lock screen); its refusals come back as
    `local_video {camera: "off", reason: <code>}` and the call goes on.
    `VoiceControl::set_camera(on, device)` for the system UI's video button.
  - **Never self in the picture in picture.** An active-speaker native sink skips this device's
    tracks and our own member's; `Track("local:camera" | "local:screen")` or an `is_self` track
    is refused with `unknown_track` (`detail.self_view: true`).
  - Docs: orientation (Android `device_orientation`; iOS turns frames itself, iOS 17+), the
    camera never touching the call's audio session.
- kessel-video 0.1.3 (its Apple frameworks link on their own, also from a static library; the
  README lists them for apps built with `-ignore_auto_link`).

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
- Call details, for a call UI that shows more than the frozen events: the call's key state
  and security code (`sew-voice://security`, `SecurityInfo`, from sew-call-client's
  `CallSecurity`), every media participant with its engine identity and label (`member`,
  `recorder` or `not_in_session`; `sew-voice://roster`, `RosterEntry`), the connection quality
  of each stats window (`excellent`, `good`, `poor`, `lost`, `unknown` with the window's loss,
  concealment, jitter and round trip; `lost` while reconnecting; `sew-voice://quality`,
  `QualityInfo`), and an incoming DM call (`sew-voice://incoming`, `IncomingCall`). Three
  commands: `call_details` (all four, for a window opened mid-call; `CallDetails`), `ring
  {call_id, target, room, started_by?, silent?, reason?}` and `ring_ended {call_id}`; a `join`
  with the ring's `target` and `room` accepts it and clears it. They are not in
  `sew-voice:default`: allow them with the new set `sew-voice:call-details` (or
  `allow-call-details`, `allow-ring`, `allow-ring-ended`). `DETAIL_COMMANDS` and
  `DETAIL_EVENTS` list them; `COMMANDS` and `EVENT_NAMES` are unchanged, and so are the frozen
  events' payloads.
- `VoiceListener::on_incoming` (a ring, then `None`); `VoiceControl::ring`, `ring_ended`,
  `incoming` and `details`; `VoiceService::details`, `ring`, `ring_ended`, `incoming`.
- An engine reports them through `CallHandle::security`, `roster` and `quality` (defaults:
  none, none, `unknown`) and `CallEvent::Security`, `Roster` and `Quality`; the fake call has
  `set_security`, `set_roster` and `set_quality`. `tests/native-voice-details.json` has the
  exact payloads.

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
    `text_en`, `choices_en`: the Sew App kit's consent words).
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
