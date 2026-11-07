//! Video: what the plugin needs from the engine, and the frame sink's framing.
//!
//! **The engine's side.** The engine core hands the plugin encoded **plaintext** frames of the
//! tracks it receives (decrypted and checked in Rust; nothing that failed decryption ever comes
//! out) and of this device's own camera and screen (self-view), as [`VideoRecvItem`]s on one
//! channel per call ([`crate::CallHandle::video_frames`]). The plugin tells the engine what its
//! sinks want of each track ([`TrackSettings`], the union of the track's sinks) and when a sink
//! needs a keyframe. Camera and screen go on and off through [`crate::CallHandle::camera`] and
//! [`crate::CallHandle::screen`]. The engine-wide video calls (capabilities, cameras, the
//! screen list, the pre-join preview) are on [`crate::VoiceEngine`].
//!
//! Every video method of [`crate::VoiceEngine`] and [`crate::CallHandle`] has a default that
//! says the engine has no video (`video_unsupported`, an empty capability answer, no frames),
//! so an engine without video keeps working unchanged.
//!
//! **The webview's side.** Each sink is a Tauri raw `Channel`: one binary message per frame, a
//! 32-byte little-endian [`FrameHeader`] then the payload. No JSON on the hot path.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::wire::DeviceChoice;

/// A video codec, as the wire writes it (`"vp8"`, `"h264"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum VideoCodec {
    /// VP8: one raw VP8 frame per message (the frame tag first, no container).
    Vp8,
    /// H.264: one Annex B access unit per message, with 4-byte start codes.
    H264,
}

impl VideoCodec {
    /// `"vp8"` or `"h264"`.
    pub fn as_str(self) -> &'static str {
        match self {
            VideoCodec::Vp8 => "vp8",
            VideoCodec::H264 => "h264",
        }
    }

    /// From the wire name; `None` for a codec the plugin doesn't know.
    pub fn from_wire(s: &str) -> Option<VideoCodec> {
        match s {
            "vp8" => Some(VideoCodec::Vp8),
            "h264" => Some(VideoCodec::H264),
            _ => None,
        }
    }

    /// The frame header's codec byte: `1` VP8, `2` H.264.
    pub fn header_code(self) -> u8 {
        match self {
            VideoCodec::Vp8 => 1,
            VideoCodec::H264 => 2,
        }
    }
}

/// The self-view sink track of this device's camera (also the pre-join preview's).
pub const LOCAL_CAMERA: &str = "local:camera";
/// The self-view sink track of this device's screen share.
pub const LOCAL_SCREEN: &str = "local:screen";

/// Whether `track` is one of this device's own (`local:camera`, `local:screen`).
pub fn is_local_track(track: &str) -> bool {
    track == LOCAL_CAMERA || track == LOCAL_SCREEN
}

/// One encoded plaintext frame for the sinks of `track`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedVideoFrame {
    /// The remote track's `track_sid`, or [`LOCAL_CAMERA`] / [`LOCAL_SCREEN`].
    pub track: String,
    /// The frame's codec.
    pub codec: VideoCodec,
    /// The frame: a VP8 frame, or an H.264 Annex B access unit with 4-byte start codes.
    pub data: Vec<u8>,
    /// A keyframe (VP8 key frame, H.264 IDR).
    pub keyframe: bool,
    /// The RTP timestamp (90 kHz).
    pub rtp_ts: u32,
    /// Arrival time in µs (capture time for a self-view frame).
    pub arrival_us: u64,
    /// The coded width, from a keyframe's header or SPS; `0` when the frame doesn't say.
    pub width: u32,
    /// The coded height; `0` when the frame doesn't say.
    pub height: u32,
}

/// What the engine's frame channel carries ([`crate::CallHandle::video_frames`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VideoRecvItem {
    /// A frame for the track's sinks.
    Frame(EncodedVideoFrame),
    /// The track is subscribed (frames may follow once a sink enables it).
    Added {
        /// The track.
        track: String,
    },
    /// The track is gone.
    Removed {
        /// The track.
        track: String,
    },
    /// A gap no repair filled, or a decryption failure: the sinks reset and wait for the next
    /// keyframe (the engine asks the sender for one).
    Broken {
        /// The track.
        track: String,
    },
    /// The next frame the sinks can use is a keyframe (the engine asked the sender for one).
    /// The engine sends it whenever frames restart: a remote unmute, a layer change, recovery
    /// after loss. The sinks drop deltas until that keyframe, which goes out with the
    /// discontinuity flag.
    KeyframeNeeded {
        /// The track.
        track: String,
    },
    /// The camera of a self-view track (`local:camera`) faces this way from its next frame on:
    /// sent before the first frame and before the first frame of another camera (a switch),
    /// so a host mirrors a front camera's self-view frame by frame. The sinks' header carries
    /// it ([`flag::FRONT`], [`flag::BACK`]).
    Facing {
        /// The track.
        track: String,
        /// Which way the camera faces.
        facing: crate::wire::Facing,
    },
}

/// The frame channel of one call (or of the preview): bounded, so a slow plugin drops at the
/// engine rather than buffering video.
pub type VideoFrames = mpsc::Receiver<VideoRecvItem>;

/// A sink's tile size class: `large` (30 fps) or `grid` (15 fps).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tile {
    /// A large tile (the stage, a pinned or spotlit participant).
    Large,
    /// A grid tile.
    #[default]
    Grid,
}

impl Tile {
    /// The frame rate a tile of this class asks for.
    pub fn fps(self) -> u32 {
        match self {
            Tile::Large => 30,
            Tile::Grid => 15,
        }
    }
}

/// What the sinks of one remote track want of it, as the plugin hands it to the engine (the
/// union of the track's sinks). The engine turns it into the SFU's track settings: it
/// debounces the changes, picks the priority (screen above camera, the active speaker above
/// others) and, with the room's `adaptive_stream` off, sends only `disabled`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct TrackSettings {
    /// No sink, or none visible: the SFU sends nothing for the track.
    pub disabled: bool,
    /// The largest visible sink's width in device pixels (`0` while disabled).
    pub width: u32,
    /// The largest visible sink's height in device pixels (`0` while disabled).
    pub height: u32,
    /// 30 when a visible sink is a large tile, else 15 (`0` while disabled).
    pub fps: u32,
}

impl TrackSettings {
    /// No sink wants the track.
    pub const DISABLED: TrackSettings = TrackSettings {
        disabled: true,
        width: 0,
        height: 0,
        fps: 0,
    };
}

/// `local_video`'s reason for a camera turned off by CallKit's or Telecom's video button
/// ([`crate::VoiceControl::set_camera`]).
pub const SYSTEM_UI: &str = "system_ui";

/// `camera {on, device?}` for the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraRequest {
    /// Publish the camera (`true`) or stop it.
    pub on: bool,
    /// Which camera (`None`: the one chosen with `set_video_device`, else the default).
    pub device: Option<DeviceChoice>,
}

/// The app window's native handle, for the OS screen picker's parent (Windows: the `HWND`,
/// as an integer). `None` on other systems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParentWindow(pub isize);

/// `screen {on, source?}` for the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenRequest {
    /// Share a screen (`true`) or stop sharing.
    pub on: bool,
    /// A source id from `screen_sources`; `None`: the OS picker.
    pub source: Option<String>,
    /// The window the OS picker belongs to (Windows needs it: `PickOptions::with_parent_window`).
    pub parent_window: Option<ParentWindow>,
}

/// `preview {on, device?}` for the engine: the camera before or outside a call (one 360p
/// encoder, no lease, nothing sent). Its frames come as [`LOCAL_CAMERA`] items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRequest {
    /// Open (`true`) or close the preview.
    pub on: bool,
    /// Which camera (`None`: the chosen one, else the default).
    pub device: Option<DeviceChoice>,
}

// ───────────────────────────── the decoded-frame fallback ─────────────────────────────

/// The decoded fallback's largest frame: each decoded tile is scaled to fit its sink's size,
/// and never beyond this box.
pub const DECODED_MAX_SIZE: (u32, u32) = (640, 360);
/// The decoded fallback's highest frame rate per tile.
pub const DECODED_MAX_FPS: u32 = 15;
/// How many sinks may be in decoded mode at once. Beyond that `video_sink` rejects
/// `decoded_tiles_full`, and the tile shows its participant's avatar and "video paused (this
/// device)".
pub const MAX_DECODED_SINKS: usize = 4;
/// The shortest gap between two decoded frames of one sink, in µs: one frame time at
/// [`DECODED_MAX_FPS`] less an eighth, for arrival jitter (30 fps in, every other frame out).
pub(crate) const DECODED_MIN_INTERVAL_US: u64 = 1_000_000 / DECODED_MAX_FPS as u64 * 7 / 8;

/// The pixels the decoded fallback sends: I420 for a webview with WebCodecs (it draws them as a
/// `VideoFrame`), RGBA for one without WebCodecs at all (an empty `decode` probe).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecodedFormat {
    /// Planar 4:2:0: the Y plane, then U, then V, tightly packed (chroma rounds up).
    I420,
    /// 8-bit RGBA, tightly packed.
    Rgba,
}

impl DecodedFormat {
    /// The frame header's kind byte (`2` I420, `3` RGBA).
    pub fn header_kind(self) -> u8 {
        match self {
            DecodedFormat::I420 => kind::I420,
            DecodedFormat::Rgba => kind::RGBA,
        }
    }

    /// The bytes of a tightly packed `width` x `height` frame.
    pub fn frame_len(self, width: u32, height: u32) -> usize {
        let (w, h) = (width as usize, height as usize);
        match self {
            DecodedFormat::I420 => w * h + 2 * w.div_ceil(2) * h.div_ceil(2),
            DecodedFormat::Rgba => w * h * 4,
        }
    }
}

/// What the plugin asks of the engine's decoder for one decoded sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DecoderSpec {
    /// The track's codec.
    pub codec: VideoCodec,
    /// The pixels out.
    pub format: DecodedFormat,
    /// Scale each output to fit this box, keeping the aspect ratio and never scaling up (the
    /// sink's size, capped at [`DECODED_MAX_SIZE`]).
    pub fit: (u32, u32),
}

/// One decoded frame, tightly packed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    /// The pixel layout.
    pub format: DecodedFormat,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The input's `arrival_us`, passed through.
    pub timestamp_us: u64,
    /// The pixels ([`DecodedFormat::frame_len`] bytes).
    pub data: Vec<u8>,
}

/// Why a decoder refused an input or failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DecodeFailure {
    /// A delta came first, or after a reset or an error: dropped; the decoder waits for a
    /// keyframe.
    #[error("need_keyframe")]
    NeedKeyframe,
    /// The decoder's input queue is full: the frame was dropped.
    #[error("busy")]
    Busy,
    /// A broken bitstream, a lost OS session or anything else: the plugin resets the decoder
    /// and asks the sender for a keyframe.
    #[error("{0}")]
    Failed(String),
}

/// Called from any thread when a decoder has output (or an error) queued. Cheap; never blocks.
pub type DecoderWake = Arc<dyn Fn() + Send + Sync>;

/// One remote track's decoder (the engine's: kessel-video's `VideoDecoder`, or a fake). Inputs
/// are the sink's encoded plaintext frames; outputs come back by [`FrameDecoder::poll`] in
/// order, at most one per input.
pub trait FrameDecoder: Send {
    /// Queue one frame. Never blocks.
    fn submit(&mut self, frame: &EncodedVideoFrame) -> Result<(), DecodeFailure>;
    /// The next decoded frame or error, if one is queued.
    fn poll(&mut self) -> Option<Result<DecodedFrame, DecodeFailure>>;
    /// Forget every reference: the next accepted input is a keyframe.
    fn reset(&mut self);
    /// A new output box (the sink was resized).
    fn set_fit(&mut self, fit: (u32, u32));
}

/// The fit box of a decoded sink of `width` x `height` device pixels: the sink's size capped at
/// [`DECODED_MAX_SIZE`] (an unmeasured `0` takes the cap).
pub fn decoded_fit(width: u32, height: u32) -> (u32, u32) {
    let (mw, mh) = DECODED_MAX_SIZE;
    let cap = |v: u32, max: u32| if v == 0 { max } else { v.min(max) };
    (cap(width, mw).max(2), cap(height, mh).max(2))
}

// ───────────────────────────── the sink framing ─────────────────────────────

/// The frame header's length: every sink message starts with these 32 bytes.
pub const HEADER_LEN: usize = 32;
/// The framing version (byte 0).
pub const FRAME_VERSION: u8 = 1;

/// The header's `kind` byte.
pub mod kind {
    /// An encoded frame (decode it with WebCodecs).
    pub const ENCODED: u8 = 1;
    /// A decoded I420 frame (the decoded fallback).
    pub const I420: u8 = 2;
    /// A decoded RGBA frame (the decoded fallback without WebCodecs).
    pub const RGBA: u8 = 3;
    /// Reset the decoder and wait for a keyframe; no payload.
    pub const RESET: u8 = 4;
}

/// The header's `flags` bits.
pub mod flag {
    /// The frame is a keyframe.
    pub const KEYFRAME: u8 = 1;
    /// Reset the decoder before this frame (the first frame after a gap, an open or a request).
    pub const DISCONTINUITY: u8 = 2;
    /// The frame is this device's own (self-view: mirror it with CSS).
    pub const SELF_VIEW: u8 = 4;
    /// A self-view frame from a front (selfie) camera: mirror it.
    pub const FRONT: u8 = 8;
    /// A self-view frame from a back camera: don't mirror it. Neither `FRONT` nor `BACK`: the
    /// camera's facing is unknown or external (or the frame isn't a camera's self-view).
    pub const BACK: u8 = 16;
}

/// The 32-byte header of one sink message (little-endian).
///
/// | Offset | Type | Field |
/// |---|---|---|
/// | 0 | u8 | version, `1` |
/// | 1 | u8 | kind ([`kind`]) |
/// | 2 | u8 | codec: `1` VP8, `2` H.264, `0` for decoded |
/// | 3 | u8 | flags ([`flag`]) |
/// | 4 | u32 | sink_id |
/// | 8 | u64 | timestamp_us |
/// | 16 | u16 | width (`0` = unchanged) |
/// | 18 | u16 | height (`0` = unchanged) |
/// | 20 | u32 | the sink's frame sequence |
/// | 24 | u32 | payload length |
/// | 28 | u32 | reserved, 0 |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// [`kind`].
    pub kind: u8,
    /// [`VideoCodec::header_code`], or `0`.
    pub codec: u8,
    /// [`flag`] bits.
    pub flags: u8,
    /// The sink.
    pub sink_id: u32,
    /// Arrival (or capture, for self-view) in µs.
    pub timestamp_us: u64,
    /// Width, `0` = unchanged.
    pub width: u16,
    /// Height, `0` = unchanged.
    pub height: u16,
    /// The sink's message sequence (every message, resets included).
    pub seq: u32,
    /// The payload's length.
    pub len: u32,
}

impl FrameHeader {
    /// The 32 bytes.
    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0] = FRAME_VERSION;
        b[1] = self.kind;
        b[2] = self.codec;
        b[3] = self.flags;
        b[4..8].copy_from_slice(&self.sink_id.to_le_bytes());
        b[8..16].copy_from_slice(&self.timestamp_us.to_le_bytes());
        b[16..18].copy_from_slice(&self.width.to_le_bytes());
        b[18..20].copy_from_slice(&self.height.to_le_bytes());
        b[20..24].copy_from_slice(&self.seq.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b
    }

    /// Reads a message's header; `None` when it is short, another version, or its length
    /// doesn't match the message.
    pub fn parse(msg: &[u8]) -> Option<FrameHeader> {
        let b: &[u8; HEADER_LEN] = msg.get(..HEADER_LEN)?.try_into().ok()?;
        if b[0] != FRAME_VERSION {
            return None;
        }
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let mut ts = [0u8; 8];
        ts.copy_from_slice(&b[8..16]);
        let h = FrameHeader {
            kind: b[1],
            codec: b[2],
            flags: b[3],
            sink_id: u32_at(4),
            timestamp_us: u64::from_le_bytes(ts),
            width: u16_at(16),
            height: u16_at(18),
            seq: u32_at(20),
            len: u32_at(24),
        };
        (msg.len() - HEADER_LEN == h.len as usize).then_some(h)
    }

    /// The whole message: this header (with `len` set from `payload`) then `payload`.
    pub fn message(mut self, payload: &[u8]) -> Vec<u8> {
        self.len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.extend_from_slice(&self.to_bytes());
        out.extend_from_slice(payload);
        out
    }
}

/// A frame size as the header writes it (larger than 65535 is clamped).
pub(crate) fn header_dim(v: u32) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}
