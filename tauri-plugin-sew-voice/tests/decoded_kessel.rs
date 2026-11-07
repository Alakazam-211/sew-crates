//! The decoded fallback on kessel-video's decoders (`video-core`): the plugin's adapter
//! (`kessel::open_decoder`) over kessel-video's fake backend, fed with real VP8 from the
//! fixture backend (the pre-encoded simulcast streams) and the fake's own H.264, alone and
//! behind the plugin's sinks (threaded decoders, their wakes, the 15 fps cap, a corrupt
//! keyframe's reset and keyframe request).
#![cfg(feature = "video-core")]
// The helpers (not only #[test] fns) unwrap: a test that cannot set up must fail loudly.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use sew_call_client::FixtureBackend;
use sew_call_client::kessel_video::fake::{FakeBackend, FakeConfig};
use sew_call_client::kessel_video::{
    CodecConfig, EncodeOptions, EncodedFrame, EncoderConfig, PixelFormat, VideoBackend, VideoFrame,
};
use tauri_plugin_sew_voice::fake::{
    FakeEngine, FakeProvider, RecordingFrameChannel, RecordingSink,
};
use tauri_plugin_sew_voice::kessel::{decode_codecs, open_decoder};
use tauri_plugin_sew_voice::video::kind;
use tauri_plugin_sew_voice::{
    DecodeFailure, DecodedFormat, DecodedFrame, DecoderSpec, EncodedVideoFrame, FrameDecoder,
    JoinArgs, Spawn, Tile, VideoCodec, VideoRecvItem, VideoSinkArgs, VideoSource, VideoTrack,
    VoiceService,
};

const CAM: &str = "TR_VCpeercamera";

fn fixtures() -> Arc<FixtureBackend> {
    FixtureBackend::load(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../sew-media-testclient/fixtures"
    ))
    .expect("the fixture streams")
}

/// `n` frames from `backend`'s encoder for `config`, one per submit, 33 ms apart from 1 s.
fn encode(backend: &dyn VideoBackend, config: EncoderConfig, n: usize) -> Vec<EncodedFrame> {
    let mut enc = backend
        .encoder(config, sew_call_client::kessel_video::no_wake())
        .expect("encoder");
    let mut out = Vec::new();
    for i in 0..n {
        let ts = 1_000_000 + i as u64 * 33_333;
        let frame = VideoFrame::filled(
            PixelFormat::I420,
            config.width,
            config.height,
            [90, 120, 140],
            ts,
        )
        .expect("frame");
        let opts = if i == 0 {
            EncodeOptions::keyframe()
        } else {
            EncodeOptions::default()
        };
        enc.submit(&frame, opts).expect("submit");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(r) = enc.poll() {
                out.push(r.expect("encoded"));
                break;
            }
            assert!(Instant::now() < deadline, "the encoder never answered");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    out
}

fn input(f: &EncodedFrame, codec: VideoCodec) -> EncodedVideoFrame {
    EncodedVideoFrame {
        track: CAM.into(),
        codec,
        data: f.data.to_vec(),
        keyframe: f.keyframe,
        rtp_ts: 0,
        arrival_us: f.capture_us,
        width: f.width,
        height: f.height,
    }
}

/// Every output of `dec` within `wait`, until `want` came.
fn outputs(
    dec: &mut dyn FrameDecoder,
    want: usize,
    wait: Duration,
) -> Vec<Result<DecodedFrame, DecodeFailure>> {
    let deadline = Instant::now() + wait;
    let mut out = Vec::new();
    while out.len() < want && Instant::now() < deadline {
        match dec.poll() {
            Some(r) => out.push(r),
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    out
}

fn counting_wake() -> (Arc<AtomicUsize>, tauri_plugin_sew_voice::DecoderWake) {
    let n = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&n);
    (
        n,
        Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }),
    )
}

#[test]
fn the_fake_backend_decodes_vp8_and_its_own_h264() {
    let fake = FakeBackend::new(FakeConfig::default());
    let codecs = decode_codecs(fake.as_ref());
    assert!(
        codecs.contains(&VideoCodec::Vp8) && codecs.contains(&VideoCodec::H264),
        "{codecs:?}"
    );
}

#[test]
fn fixture_vp8_decodes_to_fitted_i420_and_rgba() {
    let fx = fixtures();
    let frames = encode(
        fx.as_ref(),
        EncoderConfig::new(CodecConfig::Vp8, 640, 360, 30, 600_000),
        20,
    );
    assert!(frames[0].keyframe, "the fixture starts on a keyframe");
    let fake = FakeBackend::new(FakeConfig::default());
    for (format, fit, size) in [
        (DecodedFormat::I420, (640, 360), (640, 360)),
        (DecodedFormat::I420, (320, 240), (320, 180)),
        (DecodedFormat::Rgba, (200, 200), (200, 112)),
    ] {
        let (wakes, wake) = counting_wake();
        let mut dec = open_decoder(
            fake.as_ref(),
            DecoderSpec {
                codec: VideoCodec::Vp8,
                format,
                fit,
            },
            wake,
        )
        .expect("a VP8 decoder");
        // A delta first is refused (or dropped with NeedKeyframe on the way out).
        let early = dec.submit(&input(&frames[1], VideoCodec::Vp8));
        let mut got: Vec<DecodedFrame> = Vec::new();
        if early.is_ok() {
            let r = outputs(dec.as_mut(), 1, Duration::from_secs(5));
            assert!(
                matches!(r.as_slice(), [Err(DecodeFailure::NeedKeyframe)]),
                "{r:?}"
            );
        } else {
            assert_eq!(early, Err(DecodeFailure::NeedKeyframe));
        }
        for f in &frames {
            // The decoder's input queue is small: wait for room.
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match dec.submit(&input(f, VideoCodec::Vp8)) {
                    Ok(()) => break,
                    Err(DecodeFailure::Busy) if Instant::now() < deadline => {
                        got.extend(
                            outputs(dec.as_mut(), 1, Duration::from_millis(20))
                                .into_iter()
                                .map(|r| r.expect("decoded")),
                        );
                    }
                    Err(e) => panic!("submit: {e}"),
                }
            }
            got.extend(
                outputs(dec.as_mut(), 1, Duration::from_secs(5))
                    .into_iter()
                    .map(|r| r.expect("decoded")),
            );
        }
        // The output queue keeps 2 (oldest dropped): most frames come out when polled as they go.
        assert!(
            got.len() >= frames.len() - 2,
            "{} of {}",
            got.len(),
            frames.len()
        );
        for f in &got {
            assert_eq!((f.format, f.width, f.height), (format, size.0, size.1));
            assert_eq!(f.data.len(), format.frame_len(f.width, f.height));
        }
        assert!(
            got.windows(2)
                .all(|w| w[0].timestamp_us < w[1].timestamp_us),
            "the arrival times pass through in order"
        );
        assert!(
            got.iter().any(|f| f.data.iter().any(|&b| b != f.data[0])),
            "real pictures, not one colour"
        );
        assert!(
            wakes.load(Ordering::SeqCst) > 0,
            "the decoder woke the sink"
        );
    }
}

#[test]
fn the_fakes_h264_decodes_and_a_corrupt_frame_fails() {
    let fake = FakeBackend::new(FakeConfig::default());
    let frames = encode(fake.as_ref(), EncoderConfig::h264(640, 360, 30, 600_000), 5);
    let (_, wake) = counting_wake();
    let mut dec = open_decoder(
        fake.as_ref(),
        DecoderSpec {
            codec: VideoCodec::H264,
            format: DecodedFormat::I420,
            fit: (320, 180),
        },
        wake,
    )
    .expect("an H.264 decoder");
    dec.submit(&input(&frames[0], VideoCodec::H264))
        .expect("keyframe");
    let r = outputs(dec.as_mut(), 1, Duration::from_secs(5));
    let f = r[0].as_ref().expect("decoded");
    assert_eq!(
        (f.width, f.height, f.format),
        (320, 180, DecodedFormat::I420)
    );

    // Garbage marked as a keyframe: the decoder fails (on submit or on the way out).
    let mut bad = input(&frames[0], VideoCodec::H264);
    bad.data = vec![0, 0, 0, 1, 0x65, 0xff, 0xff, 0xff];
    let failed = match dec.submit(&bad) {
        Err(DecodeFailure::Failed(_)) => true,
        Err(e) => panic!("submit: {e}"),
        Ok(()) => matches!(
            outputs(dec.as_mut(), 1, Duration::from_secs(5)).as_slice(),
            [Err(DecodeFailure::Failed(_))]
        ),
    };
    assert!(failed, "a corrupt keyframe is a decoder failure");
    // A reset takes the decoder back to keyframe-first.
    dec.reset();
    dec.submit(&input(&frames[0], VideoCodec::H264))
        .expect("keyframe after the reset");
    assert!(outputs(dec.as_mut(), 1, Duration::from_secs(5))[0].is_ok());
}

/// Lets the runtime and the decoder threads work until `pred` holds.
async fn until(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !pred() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn behind_the_sinks_fixture_vp8_comes_out_as_i420_at_15_fps() {
    let fake: Arc<dyn VideoBackend> = FakeBackend::new(FakeConfig::default());
    let engine = Arc::new(FakeEngine::new());
    engine.enable_video();
    let backend = Arc::clone(&fake);
    engine.set_video_decoder(
        decode_codecs(fake.as_ref()),
        Arc::new(move |spec, wake| open_decoder(backend.as_ref(), spec, wake)),
    );
    let spawn: Spawn = Arc::new(|f| {
        tokio::spawn(f);
    });
    let svc = VoiceService::new(
        engine.clone(),
        Arc::new(FakeProvider::answering("48831765147549699")),
        Arc::new(RecordingSink::new()),
        spawn,
    );
    svc.start();
    svc.join(JoinArgs {
        target: "k2:acme".into(),
        room: "48831765147549698".into(),
        muted: None,
        deafened: None,
    })
    .await
    .expect("join");
    let call = engine.last_call().expect("call");
    call.set_video_tracks(vec![VideoTrack {
        member_id: "48831765147549700".into(),
        is_self: false,
        source: VideoSource::Camera,
        track: CAM.into(),
        codec: VideoCodec::Vp8,
        width: 1280,
        height: 720,
        muted: false,
        paused: None,
        unsupported: false,
    }]);
    until("the video list", || svc.snapshot().video.len() == 1).await;

    let ch = Arc::new(RecordingFrameChannel::new());
    let ans = svc
        .video_sink(
            VideoSinkArgs {
                track: CAM.into(),
                width: 1280,
                height: 720,
                visible: true,
                tile: Tile::Large,
                // A webview without VP8 in WebCodecs.
                decode: vec!["h264".into()],
            },
            ch.clone(),
        )
        .await
        .expect("a decoded sink");
    assert_eq!(ans.mode, tauri_plugin_sew_voice::SinkMode::Decoded);

    // The q layer (320x180, small enough for a debug build's decoder) at 30 fps for one
    // second, paced so the decoder keeps up.
    let fx = fixtures();
    let frames = encode(
        fx.as_ref(),
        EncoderConfig::new(CodecConfig::Vp8, 320, 180, 30, 300_000),
        30,
    );
    for f in &frames {
        assert!(call.push_video(VideoRecvItem::Frame(input(f, VideoCodec::Vp8))));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    until("the decoded frames", || ch.messages().len() >= 12).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let got = ch.frames();
    // 30 frames in over 1 s of arrival time: about half go out (15 fps).
    assert!(
        (12..=16).contains(&got.len()),
        "{} decoded frames sent for 30 in",
        got.len()
    );
    for (h, payload) in &got {
        // Never scaled up: the 320x180 stream in a 1280x720 tile (fit 640x360).
        assert_eq!(
            (h.kind, h.codec, h.width, h.height),
            (kind::I420, 0, 320, 180)
        );
        assert_eq!(payload.len(), 320 * 180 * 3 / 2);
    }
    assert!(
        got.windows(2)
            .all(|w| w[1].0.timestamp_us - w[0].0.timestamp_us >= 58_000),
        "at most 15 fps"
    );
    let asked = call.keyframe_requests().len();

    // A corrupt keyframe: a reset message, and a keyframe is asked of the sender.
    let mut bad = input(&frames[0], VideoCodec::Vp8);
    bad.data.truncate(12);
    bad.arrival_us = 3_000_000;
    let before = ch.messages().len();
    assert!(call.push_video(VideoRecvItem::Frame(bad)));
    until("the reset", || {
        ch.frames()[before..]
            .iter()
            .any(|(h, _)| h.kind == kind::RESET)
    })
    .await;
    until("the keyframe request", || {
        call.keyframe_requests().len() > asked
    })
    .await;
}
