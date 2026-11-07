//! The decoded fallback's decoders from kessel-video (the plugin's `video-core`): a
//! [`FrameDecoder`] over any kessel-video backend's `VideoDecoder`. With `video`, the real
//! engine opens them on kessel-video's system backend (VP8 through the pure-Rust
//! `vp8-decoder` everywhere, H.264 through VideoToolbox on macOS); a host or a test may pass
//! its own backend (`real::CallClientEngine::with_video_backend`, the engine's one backend).

use sew_call_client::kessel_video::{
    self as kv, CodecDirection, DecodeError, DecoderConfig, EncodedInput, FrameBuffer, PixelFormat,
    VideoBackend,
};

use crate::engine::EngineError;
use crate::video::{
    DecodeFailure, DecodedFormat, DecodedFrame, DecoderSpec, DecoderWake, EncodedVideoFrame,
    FrameDecoder, VideoCodec,
};
use crate::wire::{ErrorInfo, video_code};

/// kessel-video's codec of the plugin's.
fn kv_codec(c: VideoCodec) -> kv::VideoCodec {
    match c {
        VideoCodec::Vp8 => kv::VideoCodec::Vp8,
        VideoCodec::H264 => kv::VideoCodec::H264,
    }
}

/// The plugin's codec of kessel-video's (`None`: one the plugin doesn't carry).
fn codec_of(c: kv::VideoCodec) -> Option<VideoCodec> {
    match c {
        kv::VideoCodec::Vp8 => Some(VideoCodec::Vp8),
        kv::VideoCodec::H264 => Some(VideoCodec::H264),
        _ => None,
    }
}

fn pixel_format(f: DecodedFormat) -> PixelFormat {
    match f {
        DecodedFormat::I420 => PixelFormat::I420,
        DecodedFormat::Rgba => PixelFormat::Rgba,
    }
}

/// The codecs `backend` decodes now (its capabilities' decode entries), sorted.
pub fn decode_codecs(backend: &dyn VideoBackend) -> Vec<VideoCodec> {
    let mut out: Vec<VideoCodec> = backend
        .capabilities()
        .codecs
        .iter()
        .filter(|c| c.direction == CodecDirection::Decode)
        .filter_map(|c| codec_of(c.codec))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// A decoder of `backend` for one decoded sink: `spec.codec` in, `spec.format` out, fitted to
/// `spec.fit`; `wake` is called whenever an output is queued. A codec the backend can't decode
/// is `decoded_unsupported`.
pub fn open_decoder(
    backend: &dyn VideoBackend,
    spec: DecoderSpec,
    wake: DecoderWake,
) -> Result<Box<dyn FrameDecoder>, EngineError> {
    let mut config = DecoderConfig::new(kv_codec(spec.codec));
    config.output = pixel_format(spec.format);
    config.fit = Some(spec.fit);
    let dec = backend.decoder(config, wake).map_err(|e| {
        EngineError::Failed(ErrorInfo::with_detail(
            video_code::DECODED_UNSUPPORTED,
            serde_json::json!({ "codec": spec.codec, "why": e.to_string() }),
        ))
    })?;
    Ok(Box::new(KesselDecoder {
        dec,
        codec: kv_codec(spec.codec),
        format: spec.format,
    }))
}

/// A kessel-video decoder behind the plugin's [`FrameDecoder`].
struct KesselDecoder {
    dec: Box<dyn kv::VideoDecoder>,
    codec: kv::VideoCodec,
    format: DecodedFormat,
}

fn failure(e: DecodeError) -> DecodeFailure {
    match e {
        DecodeError::NeedKeyframe => DecodeFailure::NeedKeyframe,
        DecodeError::Busy => DecodeFailure::Busy,
        e => DecodeFailure::Failed(e.to_string()),
    }
}

impl FrameDecoder for KesselDecoder {
    fn submit(&mut self, frame: &EncodedVideoFrame) -> Result<(), DecodeFailure> {
        self.dec
            .submit(EncodedInput::new(
                self.codec,
                frame.data.clone(),
                frame.keyframe,
                frame.arrival_us,
            ))
            .map_err(failure)
    }

    fn poll(&mut self) -> Option<Result<DecodedFrame, DecodeFailure>> {
        let out = self.dec.poll()?;
        Some(out.map_err(failure).and_then(|f| self.pixels(f)))
    }

    fn reset(&mut self) {
        self.dec.reset();
    }

    fn set_fit(&mut self, fit: (u32, u32)) {
        self.dec.set_fit(Some(fit));
    }
}

impl KesselDecoder {
    /// A decoded frame's pixels, tightly packed in the sink's format.
    fn pixels(&self, f: kv::VideoFrame) -> Result<DecodedFrame, DecodeFailure> {
        let want = pixel_format(self.format);
        let planes = match &f.buffer {
            FrameBuffer::Planes(p) if p.format == want => p.to_tight(f.width, f.height),
            FrameBuffer::Planes(_) => kv::convert(&f, f.width, f.height, want)
                .ok()
                .and_then(|c| c.planes().map(|p| p.to_tight(c.width, c.height)))
                .ok_or_else(|| DecodeFailure::Failed("a decoded frame in another format".into()))?,
            FrameBuffer::Native(n) => {
                n.0.to_planes(want)
                    .map_err(|e| DecodeFailure::Failed(e.to_string()))?
                    .to_tight(f.width, f.height)
            }
            _ => return Err(DecodeFailure::Failed("an unknown frame buffer".into())),
        };
        Ok(DecodedFrame {
            format: self.format,
            width: f.width,
            height: f.height,
            timestamp_us: f.capture_us,
            data: planes.data.to_vec(),
        })
    }
}
