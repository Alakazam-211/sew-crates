//! Video. The engine core never names kessel-video: it moves encoded frames, and [`rtp`] turns
//! them into RTP payloads and back. With `testing` and `video-core`, `FixtureBackend` is a
//! kessel-video backend that replays pre-encoded streams.
//!
//! Receive (built with `video-core`; without it the engine negotiates no video at all): the
//! remote camera and screen tracks of the roster ([`VideoTrack`], `CallEvent::VideoAdded` and
//! friends), each disabled at the media server until the host opens a sink for it
//! ([`crate::Call::video_sink`]); then per track a reorder buffer, the depacketiser, decryption
//! with the publisher's key (LkGcm, no plaintext fallback) and the frames out through
//! [`crate::Call::video_frames`], with keyframe requests rate-limited per track.
//!
//! Publish (built with `video-core`, like receive): the engine takes encoded plaintext frames
//! per simulcast layer ([`publish`]) and does the lease, the signalling, the encryption, the
//! packets, keyframes, dynacast and the rate split ([`alloc`]). The `VideoBridge` (`bridge`)
//! drives any kessel-video frame source (a camera, a screen) and the encoders into it, and
//! hands the in-call self-view its layer.
//!
//! Capture (`video-core`): [`VideoCapture`] over the app's kessel-video backend opens cameras
//! ([`CameraSource`]) and screens ([`ScreenSource`]) for the publish side, lists the screens
//! with PNG thumbnails, runs the OS picker, and runs the pre-join [`Preview`]. One opener holds
//! the camera: the bridge's camera opens through the same `VideoCapture` and takes the camera
//! over from a running preview.

pub(crate) mod alloc;
#[cfg(feature = "video-core")]
mod bridge;
#[cfg(feature = "video-core")]
mod capture;
// Its users: the RTP round trips (test + testing, in Sew's tree: `sew_fixtures`) and
// FixtureBackend (testing + video-core).
#[cfg(all(
    feature = "testing",
    any(all(test, sew_fixtures), feature = "video-core")
))]
mod container;
#[cfg(all(feature = "testing", feature = "video-core"))]
mod fixture;
pub(crate) mod keyframe;
#[cfg(feature = "video-core")]
mod preview;
pub(crate) mod publish;
pub(crate) mod reorder;
pub(crate) mod rtp;
pub(crate) mod size;
pub(crate) mod subscribe;
mod types;

#[cfg(feature = "video-core")]
pub use bridge::{
    CameraRequest, CameraState, SelfViewFrames, SelfViewItem, SourceFeed, SourceNote, VideoBridge,
    choose_codec,
};
#[cfg(feature = "video-core")]
pub use capture::{
    CameraSource, Capture, CaptureEnd, CaptureError, CaptureEvent, CaptureKind, CapturePause,
    ScreenChoice, ScreenPick, ScreenSource, THUMBNAIL_MAX_WIDTH, VideoCapture, screen_constraints,
    thumbnail_png,
};
#[cfg(all(feature = "testing", feature = "video-core"))]
pub use fixture::{FixtureBackend, FixtureError};
#[cfg(feature = "video-core")]
pub use preview::{
    LocalFrame, PREVIEW_BITRATE_BPS, PREVIEW_FPS, PREVIEW_HEIGHT, PREVIEW_QUEUE, PREVIEW_WIDTH,
    Preview, PreviewFrames, PreviewItem, preview_codec,
};
pub use publish::{
    EncodedVideo, LayerRate, LayerSpec, LocalVideo, VideoCaps, VideoControl, VideoError,
    VideoPublication, VideoPublish, VideoSendStats, VideoSender, plan_layers,
};
pub use subscribe::{VideoFrameStream, VideoSink};
pub use types::{
    VideoCodec, VideoFrame, VideoLayerInfo, VideoPause, VideoRecvItem, VideoSinkSpec, VideoSource,
    VideoTrack, VideoTrackView,
};
