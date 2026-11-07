//! The engine's video types: remote video tracks as the host sees them, the receive side's
//! items ([`Call::video_frames`](crate::Call::video_frames)) and the sinks a host opens to ask
//! for a track ([`Call::video_sink`](crate::Call::video_sink)).
//!
//! The engine hands out **encoded plaintext** frames: decrypted and verified, never decoded. A
//! host decodes them itself (the webview's WebCodecs) or with kessel-video's decoders.

use bytes::Bytes;
use serde::Serialize;

/// A video codec a remote track can carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum VideoCodec {
    /// VP8 (RFC 6386 frames).
    Vp8,
    /// H.264: one Annex B access unit per frame, with 4-byte start codes.
    H264,
}

impl VideoCodec {
    /// The codec's name in Sew's media policy and in `TrackInfo` mime types (`vp8`, `h264`).
    pub fn as_str(self) -> &'static str {
        match self {
            VideoCodec::Vp8 => "vp8",
            VideoCodec::H264 => "h264",
        }
    }

    /// The codec of a mime type such as `video/VP8` or `video/H264` (any case).
    pub fn from_mime(mime: &str) -> Option<VideoCodec> {
        let m = mime.to_ascii_lowercase();
        let name = m.strip_prefix("video/").unwrap_or(&m);
        match name {
            "vp8" => Some(VideoCodec::Vp8),
            "h264" => Some(VideoCodec::H264),
            _ => None,
        }
    }
}

/// What a remote video track shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum VideoSource {
    /// A camera.
    Camera,
    /// A screen share.
    ScreenShare,
}

/// Why the media server paused a track's video for this member.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum VideoPause {
    /// This member's downlink is congested: the server pauses video (never audio) until the
    /// bandwidth estimate allows it again.
    Congestion,
}

/// One simulcast layer a publisher sends, as its `TrackInfo` lists it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct VideoLayerInfo {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bitrate in bits per second (0 when the publisher did not say).
    pub bitrate: u32,
}

/// A remote video track (camera or screen) of a participant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct VideoTrack {
    /// The engine's stable id for the track in this call (the `track` of its
    /// [`VideoRecvItem`]s).
    pub id: u64,
    /// The publisher's engine identity (`p_…`).
    pub identity: String,
    /// The publisher's Sew member, once the keys' roster or the call socket named it.
    pub member_id: Option<String>,
    /// Camera or screen.
    pub source: VideoSource,
    /// The engine's track sid (`TR_…`).
    pub track_sid: String,
    /// The codec: the negotiated payload type of the track's packets once they arrive, else
    /// what the publisher's `TrackInfo` says. `None` when neither says yet (a publisher that
    /// does not name its codec in `TrackInfo` until its first packet).
    pub codec: Option<VideoCodec>,
    /// The publisher's size, from its `TrackInfo` (0 when it did not say).
    pub width: u32,
    /// The publisher's size, from its `TrackInfo` (0 when it did not say).
    pub height: u32,
    /// The publisher muted the track (camera off).
    pub muted: bool,
    /// The server paused the track for this member (`None`: not paused).
    pub paused: Option<VideoPause>,
    /// The simulcast layers the publisher sends, lowest first.
    pub layers: Vec<VideoLayerInfo>,
}

/// One participant's camera or screen in the snapshot ([`crate::Participant::camera`],
/// [`crate::Participant::screen`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct VideoTrackView {
    /// The engine's id for the track ([`VideoTrack::id`]).
    pub id: u64,
    /// The track sid.
    pub track_sid: String,
    /// The codec, when known.
    pub codec: Option<VideoCodec>,
    /// The publisher's size (0 when unknown).
    pub width: u32,
    /// The publisher's size (0 when unknown).
    pub height: u32,
    /// The publisher muted it.
    pub muted: bool,
    /// The server paused it for this member.
    pub paused: Option<VideoPause>,
}

impl VideoTrack {
    /// The snapshot's view of the track.
    pub fn view(&self) -> VideoTrackView {
        VideoTrackView {
            id: self.id,
            track_sid: self.track_sid.clone(),
            codec: self.codec,
            width: self.width,
            height: self.height,
            muted: self.muted,
            paused: self.paused,
        }
    }
}

/// One decrypted, verified encoded frame of a remote video track.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct VideoFrame {
    /// The track ([`VideoTrack::id`]).
    pub track: u64,
    /// The codec of this frame.
    pub codec: VideoCodec,
    /// The plaintext frame: a VP8 frame, or an H.264 access unit with 4-byte start codes.
    pub data: Bytes,
    /// A keyframe (VP8's P bit clear; H.264 with an IDR slice).
    pub keyframe: bool,
    /// The RTP timestamp (90 kHz).
    pub rtp_ts: u32,
    /// When its last packet arrived, in microseconds since the call started.
    pub arrival_us: u64,
    /// The coded width, from a VP8 keyframe's header or the H.264 SPS (`None` on other frames).
    pub width: Option<u32>,
    /// The coded height, likewise.
    pub height: Option<u32>,
}

/// What the receive side hands a video host, in order ([`crate::Call::video_frames`]).
///
/// Frames of a track flow only while at least one of its sinks is open
/// ([`crate::Call::video_sink`]); with none, the engine asks the server to disable the track
/// and nothing is forwarded.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum VideoRecvItem {
    /// A track's frames start (its first sink opened): frames follow, from a keyframe.
    Added {
        /// The track.
        track: u64,
        /// Its codec.
        codec: VideoCodec,
    },
    /// A track's frames stop (its last sink closed, the publisher unpublished or left).
    Removed {
        /// The track.
        track: u64,
    },
    /// A decrypted frame.
    Frame(VideoFrame),
    /// Frames were lost or failed to decrypt: the decoder must reset and wait for a keyframe
    /// (the engine already asked the publisher for one).
    Broken {
        /// The track.
        track: u64,
    },
    /// The engine asked the publisher for a keyframe (a sink opened, a loss, a decrypt failure
    /// or the host's [`VideoSink::request_keyframe`](crate::VideoSink::request_keyframe)).
    KeyframeNeeded {
        /// The track.
        track: u64,
    },
}

/// What a sink shows of a track; the engine asks the server for the union of a track's sinks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct VideoSinkSpec {
    /// The tile's size in device pixels (the layer whose size fits it is asked for).
    pub width: u32,
    /// The tile's size in device pixels.
    pub height: u32,
    /// On screen. A track whose sinks are all hidden is disabled at the server.
    pub visible: bool,
    /// A large tile (30 fps) rather than a grid tile (15 fps).
    pub large: bool,
}

impl VideoSinkSpec {
    /// A visible sink of `width` × `height` device pixels; `large` for a large tile.
    pub fn new(width: u32, height: u32, large: bool) -> VideoSinkSpec {
        VideoSinkSpec {
            width,
            height,
            visible: true,
            large,
        }
    }

    /// The same sink, on or off screen.
    pub fn visible(mut self, visible: bool) -> VideoSinkSpec {
        self.visible = visible;
        self
    }
}
