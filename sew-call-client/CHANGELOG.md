# Changelog

## 0.6.0 (2026-11-10, Sew 0.9.0)

### Breaking

- sew-proto 0.9: the re-exported recording types (`Recording`, `ConsentPrompt`,
  `RecordingNotice`, `RecordingRef`, and the new `RecordingTrigger`) are sew-proto 0.9.0's.
  `Recording` and `ConsentPrompt` are now `#[non_exhaustive]` and gain fields (`trigger`;
  `required`, `app_readers`, `agent_readers`): outside sew-proto, build them with
  `Recording::new` and `ConsentPrompt::new` (a struct literal no longer compiles), or read them
  from JSON. sew-proto 0.9.0's own breaks are in its CHANGELOG. `JoinProvider` gains the
  provided method `recording` (a host that implements the trait keeps compiling).
- Building from source: the workspace builds with Rust 1.99.1; the crate's `rust-version`
  stays 1.95. 0.5.1's WebRTC update (dimpl 0.9, upstream str0m fixes) is included.

### Added

- Record and Stop (Sew 0.9.0): `Call::start_recording` and `Call::stop_recording`, the
  person's own acts, sent through the host with the person's session by the new provided
  method `JoinProvider::recording` (`RecordingRequest`, with `RecordingControl::Start` or
  `Stop` as `action`, `path()`, `body()` and `recording_from_json()`; its JSON, `{target, room,
  action, recording_id?, device_id?}`, is the web kit's native lane body). Its default sends nothing and
  answers `recording_unsupported` (`RECORDING_UNSUPPORTED`). Sew's answer is the call's
  recording at once; Sew's refusals come back as `ProviderError::Refused` with Sew's reason. A
  Stop with no active recording is `CallError::NoRecording`; a Record while the join is held
  for a notice is refused `not_in_voice` without asking Sew.
- `CallRecording::trigger` (`RecordingTrigger`, re-exported: `manual`, `auto` or `required`;
  from `auto` against an older Sew) and `CallRecording::required`: the room requires its
  recording, so Sew refuses a Stop while two or more people are in it.

### Fixed

- macOS and iOS: an H.264 session counts as hardware only when kessel-video's encoder name
  names a hardware encoder as well as its implementation saying so. A session opened with
  low-latency rate control could run on VideoToolbox's software encoder while reporting
  hardware; such a session is now refused unless its name carries the hardware encoder's id,
  and the camera publishes VP8 instead. Every H.264 open logs its implementation and encoder
  name at info.

## 0.5.1 (2026-11-10, Sew 0.8.3)

Additions only: nothing in 0.5.0's API changes or goes away.

### Fixed

- Updates the WebRTC dependencies to current upstream releases (dimpl 0.9, upstream str0m
  fixes); recommended for everyone. A host that repeats the `[patch.crates-io]` rows moves the
  str0m rows to `907e050bfa212a23394f4482110acef04cba8fc6` and drops the dimpl row: dimpl 0.9.0
  comes from crates.io.
- A publication no longer drops the media server's keyframe request for a layer whose last
  asked-for keyframe has already gone out (within 300 ms of that ask): such a request comes from
  a subscriber that joined after it, which before waited for the server's next request (up to
  its 0.5–1 s throttle) for its first frame.
- A camera switch (`VideoBridge::switch_camera`, a front/back flip) puts its keyframe on the new
  camera's first frame. Before, it asked the encoders for a keyframe as the new camera came in,
  and a threaded encoder could put it on the old camera's last frame, still waiting in it: the
  new camera's frames then went out as deltas of another picture until the next keyframe.
- A leave frees the app's call slot whatever the camera did. The slot is released the moment
  the call is over (a leave, a kick, a lost admit, an engine error), before `Call::leave`
  answers, so a `CallClient::join` right after it is never `already_in_call`; before, it was
  released only once the engine's parts were dropped, after the answer.
- Dropping a `Call` without a leave leaves now, even while a video sink, a camera pipeline or
  a `CallLeaver` still holds the engine's command queue (before, the call lived on until the
  last of them was gone).
- A camera pipeline's stop waits at most 2 s: a camera whose close blocks on the OS (a
  suspended or lost device) finishes closing on its own thread instead of holding up
  `VideoBridge::set_camera`, `switch_camera` or `close`.
- The engine's leave no longer waits without bound for the call keys to close (at most 3 s,
  logged).
- A device offers to receive only the video codecs it can decode, once its host declares them
  with the new `CallClient::set_video_decode`: VP8 always, H.264 only if the host lists an H.264
  decoder (its kessel-video backend's OS decoders, its webview's WebCodecs probe), and never on
  Linux. Before, every native but Linux offered H.264 whatever it could decode, so the media
  server could send it H.264 it could not show. A host that declares nothing keeps
  `ClientConfig::video_h264` alone, as before.

### Added

- `VideoSendStats::sfu_keyframe_requests_by_layer`: the media server's keyframe requests per
  layer.
- `FixtureBackend::vp8_failures_left` (feature `testing`): how many of `fail_vp8_opens`'s
  failures are still to come.
- `Call::leaver` and `CallLeaver`: leave a call without the `Call` handle, for a host that
  keeps the `Call` behind a lock its camera or screen commands hold while they wait on a
  device. Those commands then answer `ended`.
- `Call::local_id` (and `CallLeaver::local_id`), and `VideoBridge::close_call`: stops only
  that call's video pipelines, so a late teardown of a call that was left never stops the next
  call's camera. A pipeline of an earlier call found by `set_camera`, `switch_camera` or a
  publish is stopped first.
- A camera must give its first frame within 2.5 s of opening, or `set_camera` (and a switch's
  new camera) is refused with the new `VideoError::NoFrames` (`no_frames`): the camera closed,
  the lease released, nothing published. The camera track is published only after its first
  frame (before, after its first `Started` or 3 s).
- The video codec preference, a per-device user setting: `VideoCodecPreference` (`Automatic`,
  the default; `PreferH264`; `PreferVp8`), set with `CallClient::set_video_codec_preference`
  and read back with `CallClient::video_codec_preference` and `Call::video_codec_preference`.
  The engine keeps it for every call until the process ends; persisting it is the host's job.
  `PreferVp8` also stops the device offering to receive H.264, from its next join.
- `effective_publish_codec(preference, detection)` and
  `effective_publish_codec_under_policy(preference, detection, h264_allowed)`: the codec a
  camera or screen publishes in, and `CodecPreferenceReason` (`not_detected`, `platform`,
  `policy`) when Prefer H.264 can't apply. `H264Detection` (`Usable`, `NotDetected`,
  `Unconfirmed`, `NoPlatformCheck`) is the run-time check of the OS's hardware H.264 encoder;
  with `video-core`, `h264_encode_detection` reads it from a kessel-video backend's
  capabilities (a hardware encode entry; no check on Linux, Android or Windows yet; an iOS
  before 17.4 is `Unconfirmed`) and `H264Detection::from_session` from an opened encoder.
  Nothing assumes H.264 from the platform.
- `CallClient::set_video_decode` and `CallClient::video_decode`: the codecs the host can
  decode, from the next join (above). `Call::video_codecs`: the codecs a call's media
  connection offers.
- `VideoTrack::unsupported` and `VideoTrackView::unsupported`: this device can receive none of
  the track's codecs. Before a track's first packet, `VideoTrack::codec` is now the first of the
  publisher's codecs (its primary, then its backups) that this device receives; after it, the
  packets' codec, as before.
- `CapturePause::Suspended`: kessel-video's `Interruption::Suspended` (a MacBook's built-in
  camera with its lid closed, a shut lens cover) is a pause, `Resumed` when the camera comes
  back; before, it was `CapturePause::Other`.

- `choose_codecs` with `CodecPlan` and `PlannedCodec` (`video-core`): what a native sender
  publishes in, under the call's policy and this device's codecs. H.264 counts only when it is
  usable: the backend lists an H.264 encoder run by the hardware, the encoders open
  hardware-only, and each opened session reports the hardware (a session that reports software,
  or can't tell, as on an iOS before 17.4, is refused). Only macOS and iOS publish H.264; Linux
  never does, and Android and Windows publish VP8 only for now. `CodecPlan::reason` says why.
- A backup codec for a publication (the frame API): `VideoPublish::with_backup` with
  `BackupSpec` (VP8 beside an H.264 primary). One `AddTrackRequest` names both codecs, each on
  its own cid, and both send sections go in the same offer; each codec has its own layers,
  SSRCs and keyframe state, and a frame goes to the lane of its codec. The backup reads only
  its own `subscribed_codecs` entry and runs only while some subscriber gets it: the new
  `VideoControl::BackupStart`, `BackupRates`, `BackupKeyframe` and `BackupStop` tell the
  producer (a host that asks for no backup never sees them). Both codecs share one bandwidth
  budget, every lowest layer first; the backup's lowest layer starts without the raise
  hysteresis (the primary's top layer is cut first). The backup is dropped when the SFU keeps
  one codec or the policy no longer allows it (`backup_codec`, dynacast, VP8 listed).
- `VideoBridge` publishes the codec plan: on a device with usable H.264 (and a policy with
  `backup_codec`), H.264 with the VP8 backup's layers (the same rids and sizes as the primary).
  The backup's encoders open only on `BackupStart` and close on `BackupStop`; each captured
  frame goes to its active encoders too. A backup encoder that doesn't open is tried again once
  after a second; failing again, the camera or screen is published again in VP8 alone under
  the same lease (no VP8 encoder: `encoder_lost`). An H.264 encoder that doesn't open on the
  hardware at publish makes it VP8 alone; a hardware H.264 encoder lost mid-call (its reopen
  failing) is published again in VP8 alone, and H.264 isn't tried again in that call.
  `CameraState::On`'s codec is the one published now.
- One capture feeds both codecs: the primary's encoders take each frame as it comes (a native
  buffer for a hardware encoder); the backup's take CPU planes converted once per frame, at its
  largest running layer's size (none while it doesn't run). On a phone the backup stops at
  360p, `Serious` thermal pressure keeps only its lowest layer and `Critical` runs that layer
  at 15 fps. Heat never stops the backup: it keeps its lowest layer whatever the caps, and a
  bitrate cap never takes that layer below its floor.
- `VideoPublication::republish_without_backup`: a producer that can't run its backup, or lost
  its primary codec's encoder, has the track published again in one codec, under the same
  lease.
- `VideoSendStats::frames_by_codec`, `keyframes_by_codec`, `backup_starts`, `backup_stops` and
  `backup_ms`; a backup layer's frames count in `frames_sent` as `source/backup/rid`.
- `FixtureBackend::load_with` with `FixtureOptions` and `FixtureH264` (`testing`): the
  fixture's H.264 encoder as a hardware one, one whose hardware open fails, or one whose session
  reports software or nothing; `encoder_opens`, `encoders_open`, `lose_h264` and
  `fail_vp8_opens`.

### Changed

- A camera or screen publishes H.264 only from a hardware OS encoder. With a policy that lists
  H.264 alone, a device whose only H.264 encoder is software (macOS's software VideoToolbox
  entry, a virtual machine) is now refused with `codec_unavailable`; before, `choose_codec` took
  any H.264 encoder. The bridge opens H.264 encoders hardware-only.
- `FixtureBackend`'s software H.264 encoder refuses a hardware-only open, as an OS's does.

### Deprecated

- `choose_codec`: it keeps its signature and meaning; `VideoBridge` uses `choose_codecs`.

### Dependencies

- kessel-video 0.1.5: a camera that goes away ends its source with `DeviceLost` on macOS and
  iOS (`camera_lost`: unpublished, the call goes on), the camera list says which cameras are
  suspended (`CameraInfo::suspended`), an open with no id passes over a suspended default, and
  a source's drop never waits on the thread that sends its events.

## 0.5.0 (2026-11-06, Sew 0.8.2)

### Breaking

- `PublishRefused` has a new variant, `NoLease` (a video publication without the lease Sew
  granted for it), declared before `Downgraded`, so the implicit discriminants of `Downgraded`
  and `Refused` changed: code that casts the enum to an integer gets other numbers. The enum
  was already `#[non_exhaustive]`, so a `match` is unaffected.

### Dependencies

- Needs sew-proto, sew-e2ee, sew-lkproto and sew-lk-compat 0.8.2.
- kessel-audio now comes from crates.io (`kessel-audio = "0.1"`; 0.1.0 has the phone
  backends): drop its row from your `[patch.crates-io]`. The only rows left are the str0m and
  dimpl forks. kessel-video stays at 0.1.3 from crates.io.
- The str0m fork moves to `7faf99d17f34292b5c9f72e67f3d0332cd67c122`, which fixes the
  bandwidth estimate at the start of a call (video layers no longer start low for a few
  seconds after joining). A build without the fork's rows gets str0m from crates.io, without
  the fix until str0m releases it (README, "Known issue").

### Added

- Camera flip: `VideoBridge::switch_camera` closes the old camera before it opens the new one
  (a phone runs one camera at a time; on iOS the old session was interrupted with
  `camera_busy` and nothing resumed it). The new camera starts at a keyframe, a pause of the
  released camera is reported over, and a new camera that fails to open brings the old one
  back. Neither ends the publication with `camera_lost`.
- `LocalFrame::facing` (the facing of the camera that captured a self-view or preview frame)
  and `VideoCapture::camera_facing(device)`.
- Video capture, with `video-core` (which now also turns on kessel-video's pure-Rust VP8, so
  every backend can encode VP8): `VideoCapture` over the app's kessel-video backend.
  - `open_camera(device, constraints)` and `open_screen(ScreenPick, constraints)` answer a
    running `CameraSource` / `ScreenSource` (a `Capture`: `try_frame`, `try_event` as
    `CaptureEvent` (`Started`, `Paused(CapturePause)`, `Resumed`, `Ended(CaptureEnd)`, `Error`),
    `ready` and `wake` for one task over frames and encoder outputs, `constraints` and
    `set_constraints`, `set_paused`, `content_hint`: `ContentHint::Screen` for screens).
    Dropping one stops the device. The OS permission is asked only when not yet determined.
  - Screens: `ScreenPick::Listed(id)` or `ScreenPick::System { parent_window }` (the OS picker;
    Windows needs the app window's handle); `screen_constraints` (cursor on, the app's own
    windows left out where the OS allows it); `screen_sources()` lists displays and windows
    with their thumbnails as 8-bit RGBA PNGs at most 320 px wide (`ScreenChoice`,
    `thumbnail_png`; the new `png` dependency, pure Rust), each at the size the backend gives
    (kessel-video 0.1.3's `thumbnail_width` and `thumbnail_height`; none without it).
  - `CaptureError` refusals carry the plugin's codes (`camera_denied`, `camera_busy`,
    `camera_not_found`, `screen_unsupported`, `screen_denied`, `picker_cancelled`,
    `screen_not_found`, `codec_unavailable`, `capture_failed`, ...).
  - `preview(device)`: the camera outside a call with one encoder at up to 640x360 (VP8, else
    H.264), no lease and nothing sent; its encoded plaintext frames (`LocalFrame`) come on a
    `PreviewFrames` channel (`PreviewItem::Frame`, `Ended`, `Failed`); `request_keyframe`;
    `stop` or drop closes the camera at once.
- The capture feeds the publish. `VideoBridge` opens its camera through a `VideoCapture`
  (`VideoBridge::with_capture`, `VideoBridge::capture`; `VideoBridge::new(backend)` makes one):
  the same constraints, permission and codes as the preview, and one opener holds the camera
  (`VideoCapture::open_camera` stops a running preview of the same capture first;
  `stop_preview`, `preview_running`).
- `VideoBridge::publish_capture(call, source, Capture, temporal_layers)` publishes a camera or
  screen opened through `VideoCapture` (a screen with the screen content hint, under
  `self_stream`); `Capture` is a kessel-video `FrameSource` too.
- `VideoBridge::switch_camera(call, device)` switches the published camera (nothing when it is
  off); `camera_state`, `camera_device`.
- Source events in every pipeline (cameras and screens, `publish_source` feeds too):
  `Paused` stops the frames and keeps the publication, `Resumed` asks every layer for a
  keyframe, `Ended` unpublishes and releases the lease with the reason as the `LocalVideo`
  reason (`CaptureEnd::reason`: `user_stopped`, `source_closed`, `camera_lost`,
  `camera_denied`, `screen_denied`). `VideoBridge::events()` carries them (`SourceNote`).
- The in-call self-view: `VideoBridge::self_view()` (`SelfViewFrames` of `SelfViewItem::Frame
  { source, frame: LocalFrame }` and `Ended`): the plaintext output of the camera's `h` layer
  (else its lowest at 360p or more) and the screen's `q` layer (else its lowest), no second
  encoder; `self_view_keyframe(source)`. While a self-view is open the source never pauses
  (except while `pause_camera` holds the camera).
- `Call::set_video_muted(source, muted)`: mutes or unmutes a published camera or screen at the
  SFU (`MuteTrackRequest`) while keeping the publication and its lease; sent again after a
  republish or a reconnect.
- `VideoBridge::pause_camera(paused)`: the camera's pipeline stops capturing and encoding while
  paused (a phone app in the background) and, on the return, every encoder makes a keyframe
  before its first frame. A device switch keeps the pause.
- On iOS and Android the camera captures at most 720p (1280 x 720 either way round), whatever
  the policy's camera hint allows.
- `ClientConfig::video_h264` (`with_video_h264`, `default_video_h264`): the media connection
  offers H.264 beside VP8 only where this device can show it. Off by default on Linux (the
  engine has no H.264 decoder there), so the media server never forwards a Linux client an
  H.264 track it can't show; on elsewhere. A host that decodes H.264 itself turns it on.
- `VideoPublication::published_mime`: the codec the SFU confirmed in its `TrackPublished`
  (the track's mime type), so a host can prove which codec it publishes in.
- `plan_layers` follows the capture's orientation: a hint layer's height is its short side, so
  a portrait camera (720 x 1280) gets 180 x 320, 360 x 640 and 720 x 1280 rather than 100 x 180,
  202 x 360 and 404 x 720; the split's bitrate floor goes by the short side too.
- `local_video` says why a camera or screen went off: `Call::unpublish_video_because(source,
  release, reason)` and `CameraRequest::with_reason` (an off without one still says
  `unpublished`); a device switch that publishes again says `camera_switch`; a TrackPublished
  nobody waited for, `publisher_gone`.
- `VideoPublication::end(reason)`: a host's own producer that can't go on unpublishes its
  track and releases the lease, with `reason` as `local_video`'s. The bridge's pipeline ends a
  publication whose encoders are all lost and none reopens with `encoder_lost` (a source that
  ends unpublishes with its `CaptureEnd` reason, above). The next `camera {on}` starts again.

## 0.4.1 (2026-11-06, Sew 0.8.1)

Additive.

- Camera publishing (video stays off by default; the core needs no feature). The frame API:
  `Call::set_video_lease` (Sew's `self_video` / `self_stream` through `POST
  /api/v1/me/voice/state`; refusals as `VideoError` codes `video_slots_full`, `media_busy`
  with `Retry-After`, `media_capacity`, `video_disabled`), `Call::publish_video`
  (`VideoPublish`: the codec and the layers that opened; the `AddTrackRequest` carries the
  layers and `simulcast_codecs` naming the codec, through the publish check, which refuses
  video without its lease as `no_lease`; then a sendonly simulcast `m=video`), the
  `VideoPublication` handle (`push` each layer's `EncodedVideo`, follow `VideoControl`:
  keyframes, `Rates`, `Republish`, `Ended`), `Call::unpublish_video`, `Call::set_video_caps`,
  `Call::media_policy`, `Call::video_stats` and `CallEvent::LocalVideo`. The engine
  normalises and checks H.264, encrypts per layer with LkGcm, packetises (VP8 with the T field
  always written, H.264 mode 1), writes a Dependency Descriptor on every packet and answers
  NACKs over RTX; it passes the SFU's keyframe requests on (one per 300 ms per layer), follows
  dynacast (`SubscribedQualityUpdate`), splits str0m's bandwidth estimate over the layers with
  hysteresis (cuts at once, a raise after 3 s with 15% to spare, at most one per 2 s), applies
  a policy update's caps in place or republishes for a new layer set (the lease kept), and
  unpublishes when video turns off. A PC gets the video codecs, the DD extension and str0m's
  BWE only when the join's policy has video on and a camera or screen allowed.
- `VideoBridge` (feature `video-core`): the camera on through a kessel-video backend: policy,
  permission, lease, camera, `choose_codec` (VP8 first, else H.264), one encoder per
  `plan_layers` layer opened from the top down, the publication, and a pipeline thread per
  publication; device switch and camera off.
- `e2ee`: `Leases`, `check_publish`, `PublishGuard::prepare_with_leases`,
  `PublishRefused::NoLease`, `SendGate::seal_video`, `FrameCrypto::seal_video` and
  `KeySession::encrypt_video` (both default to refusing every frame), `VideoFrameCodec`,
  `E2eeEvent::MediaPolicy`; the call socket passes `media_policy_updated` on.

- Needs sew-proto, sew-e2ee, sew-lkproto and sew-lk-compat 0.8.1 (no API change there).
- The TLS crypto provider is the host's. Every TLS connection (signalling, TURN over TLS, the
  client API, the key routes) uses one rustls provider: the one passed with the new
  `ClientConfig::with_crypto_provider` (field `crypto_provider`), else the process default
  (`CryptoProvider::get_default`), else ring behind the new default feature
  `builtin-ring-provider`. Without that feature and with neither, TLS setup is refused with
  `no_crypto_provider` (`CallError::Setup`) instead of picking a provider. A build with
  `default-features = false` that relied on ring and installs no provider adds
  `builtin-ring-provider`. The choice is logged at info once per change: `tls provider: <name>
  (caller | process default | built-in)`. `sew_call_client::rustls` re-exports rustls. ring
  stays a rustls feature; DTLS and SRTP are unchanged.
- The phone audio backend. kessel-audio moves to K2's public repository at
  aad16f76b8768ae090d3060033d473e87c736be4 (its phone backends); repeat the new
  `[patch.crates-io]` row. Feature `mobile` now also turns on kessel-audio's `os-voice` when an
  audio feature is on. `platform_audio_config(input, output)` (feature `audio`) is the call's
  kessel-audio config for this platform: on iOS and Android with `mobile`,
  `Backend::OsVoiceProcessing` (iOS's VoiceProcessingIO unit, Android's AAudio with the
  voice-communication preset) with the OS's echo cancellation, noise suppression and gain
  control on; on desktop the system devices through cpal, as before. `AUDIO_BACKEND` says which
  (`"os-voice"` or `"system"`).
- The phone's audio session. `AudioSession` (one per app) and `SessionEvent` (`Activated`,
  `Deactivated`, `Suspend`, `Resume`, `RouteChanged`): `AudioBridge::start_with_session` hands
  the app's reports to the call's kessel-audio (`session_activated` / `session_deactivated`,
  `suspend` / `resume`, `route_changed`). On iOS the bridge starts kessel-audio deferred
  (`start_when_session_active`): nothing opens until the session is active. An early
  `Activated` or `Suspend` applies when a call's audio starts; a repeated event is passed on
  once; a call's interruption ends with its bridge. The engine never configures the OS's audio
  session. On desktop the events change nothing. `AudioBridge::start` is unchanged (a session
  of its own, `AudioBridge::session`).
- Device keys on sign-out. `CallClient::forget_server(install_id)` resets every device of one
  Sew install that the data directory holds a record of (the key deleted from its store, a
  hardware key through `HardwareDeviceKey::delete`; the record and the pins cleared) and
  removes the install's files; it answers the removed `DeviceKeyId`s, and is refused with the
  new `DeviceError::InCall` while a call is live or joining. `CallClient::known_device_keys()`
  lists every key the data directory holds a record of. Also as `device::forget_install` and
  `device::known_device_keys`.
- New features `video-core` and `video`, off by default: `kessel-video` (K2's cameras, screen
  capture and video codecs, re-exported as `sew_call_client::kessel_video`), kessel-video 0.1.1
  from crates.io (no `[patch.crates-io]` row). With them calls receive video (below); with both
  off, the crate builds and behaves as before. kessel-video 0.1.1 has its OS backends, the iOS
  and Android ones included, `apple_pixel_buffer`, and its pure-Rust VP8 encoder and decoder,
  which its `system` feature, and so `video`, turns on.
- With `testing` and `video-core`: `FixtureBackend`, a kessel-video backend whose encoders
  replay pre-encoded VP8 (three temporal layers) and H.264 simulcast streams; kessel-video's
  fake backend has the VP8 encoder and decoder too.
- Video receive, with `video-core` (or `video`); a build without it negotiates no video and
  behaves as before, its offer byte for byte.
  - The PC also offers VP8 and H.264 (Constrained Baseline 42e01f and Constrained High 640c1f,
    packetization mode 1), each with RTX, NACK, PLI and FIR, and the Dependency Descriptor; the
    video receive sections the media server asks for are added.
  - Remote camera and screen tracks: `VideoTrack`, `CallEvent::VideoAdded`, `VideoChanged`
    (muted, size, layers, codec, `paused: Some(VideoPause::Congestion)` from the server's stream
    states) and `VideoRemoved`; `Participant::camera` and `Participant::screen`
    (`VideoTrackView`).
  - Every video track is disabled at the media server (`UpdateTrackSettings { disabled: true }`)
    until the host opens a sink: `Call::video_sink(track_sid, VideoSinkSpec)` returns a
    `VideoSink` (update, request a keyframe, close or drop). The settings follow the union of a
    track's visible sinks (the largest size; 30 fps for a large tile, 15 for a grid tile;
    screens above cameras, the active speaker above the others), debounced by 250 ms.
  - Per track: a reorder buffer (the larger of 150 ms and twice the RTT), the VP8 and H.264
    depacketisers, decryption with the publisher's key (LkGcm's VP8 and H.264 cases; no
    plaintext fallback, a failure counted and reported as for audio), and
    `Call::video_frames()`: `VideoRecvItem::Frame` (encoded plaintext, keyframe flag, RTP time,
    arrival, the keyframe's coded size), `Added`, `Removed`, `Broken` and `KeyframeNeeded`.
    A loss or a decrypt failure breaks the track: frames wait for a keyframe and a PLI goes out
    (at most one per 300 ms; a FIR after 1 s without a keyframe; `video_stalled` after 5 s more,
    then a PLI a second).
  - `KeySession::decrypt_video`, `FrameCrypto::open_video` and `RecvGate::open_video` (each
    refusing by default); the subscribe check admits encrypted video tracks of verified
    publishers. New `CallCounters` fields for video.
  - With `testing`: `TestingOverrides::drop_video_packets`.
- Internally, the VP8 (RFC 7741) and H.264 (RFC 6184 packetization mode 1) packetisers and
  depacketisers, the H.264 access-unit normaliser and the Dependency Descriptor writer the
  video path will use.
- The call's key state and security code. `CallSecurity` (`state`: `preparing`, `joining`,
  `active`, `securing`, `recovering`, `device_not_enrolled`, `server_identity_changed`,
  `unsupported`, `failed` or `left`; `epoch`, `since_ms`, `show_after_ms`, `reason`, and
  `code`, the active epoch's security code as six groups of five digits, written only while an
  epoch is active and Sew's policy shows it) is the new `CallEvent::Security` and
  `CallSnapshot::security`, sent whenever it changes (every new epoch). `E2eeEvent::Security`
  carries it from a key provider; the built-in MLS keys send it.

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
