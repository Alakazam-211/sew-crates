//! The decoded-frame fallback: a sink whose webview can't decode the track's codec gets the
//! engine's decoder (here the fake engine's), with the same gating as an encoded sink, its
//! outputs fitted to the sink and capped at 640x360 and 15 fps, at most four decoded sinks,
//! and a reset plus a keyframe request on a decoder error.

// The helpers (not only #[test] fns) unwrap: a test that cannot set up must fail loudly.
#![allow(clippy::expect_used)]

use std::sync::Arc;

use serde_json::json;
use tauri_plugin_sew_voice::fake::{
    FakeCall, FakeDecoders, FakeEngine, FakeProvider, RecordingFrameChannel, RecordingSink,
};
use tauri_plugin_sew_voice::video::{LOCAL_CAMERA, MAX_DECODED_SINKS, flag, kind};
use tauri_plugin_sew_voice::{
    DecodeFailure, DecodedFormat, DecoderSpec, EncodedVideoFrame, FrameHeader, JoinArgs,
    PreviewArgs, SinkIdArgs, SinkStats, Spawn, Tile, VideoCodec, VideoRecvItem, VideoSinkArgs,
    VideoSinkUpdateArgs, VideoSource, VideoTrack, VoiceService,
};

const ME: &str = "48831765147549699";
const PEER: &str = "48831765147549700";
const CAM: &str = "TR_VCpeercamera";
const SCREEN: &str = "TR_VSpeerscreen";

struct World {
    svc: VoiceService,
    engine: Arc<FakeEngine>,
    decoders: Arc<FakeDecoders>,
}

fn world() -> World {
    let engine = Arc::new(FakeEngine::new());
    engine.enable_video();
    let decoders = engine.enable_fake_decoder();
    let spawn: Spawn = Arc::new(|f| {
        tokio::spawn(f);
    });
    let svc = VoiceService::new(
        engine.clone(),
        Arc::new(FakeProvider::answering(ME)),
        Arc::new(RecordingSink::new()),
        spawn,
    );
    svc.start();
    World {
        svc,
        engine,
        decoders,
    }
}

async fn until(what: &str, mut pred: impl FnMut() -> bool) {
    for _ in 0..2000 {
        if pred() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("timed out waiting for: {what}");
}

async fn settle() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

fn track(sid: &str, source: VideoSource, codec: VideoCodec) -> VideoTrack {
    VideoTrack {
        member_id: PEER.into(),
        is_self: false,
        source,
        track: sid.into(),
        codec,
        width: 1280,
        height: 720,
        muted: false,
        paused: None,
        unsupported: false,
    }
}

/// A joined call with the peer's camera (VP8) and screen (H.264).
async fn in_call() -> (World, Arc<FakeCall>) {
    let w = world();
    w.svc
        .join(JoinArgs {
            target: "k2:acme".into(),
            room: "48831765147549698".into(),
            muted: None,
            deafened: None,
        })
        .await
        .expect("join");
    let call = w.engine.last_call().expect("call");
    call.set_video_tracks(vec![
        track(CAM, VideoSource::Camera, VideoCodec::Vp8),
        track(SCREEN, VideoSource::Screen, VideoCodec::H264),
    ]);
    until("the video list", || w.svc.snapshot().video.len() == 2).await;
    (w, call)
}

/// A sink on `track` whose webview decodes only `decode`.
fn sink_args(track: &str, w: u32, h: u32, decode: &[&str]) -> VideoSinkArgs {
    VideoSinkArgs {
        track: track.into(),
        width: w,
        height: h,
        visible: true,
        tile: Tile::Large,
        decode: decode.iter().map(|&c| c.to_owned()).collect(),
    }
}

/// Frame `n` of `track`, arriving at `at_ms` (a keyframe says 1280x720).
fn frame(track: &str, codec: VideoCodec, keyframe: bool, n: u8, at_ms: u64) -> VideoRecvItem {
    VideoRecvItem::Frame(EncodedVideoFrame {
        track: track.into(),
        codec,
        data: vec![n; 40 + usize::from(n)],
        keyframe,
        rtp_ts: 90 * u32::try_from(at_ms).expect("small"),
        arrival_us: at_ms * 1000,
        width: if keyframe { 1280 } else { 0 },
        height: if keyframe { 720 } else { 0 },
    })
}

async fn push(call: &FakeCall, item: VideoRecvItem) {
    assert!(call.push_video(item), "the frame channel took the item");
    settle().await;
}

/// The I420 bytes of a `w` x `h` frame.
fn i420(w: usize, h: usize) -> usize {
    w * h + 2 * w.div_ceil(2) * h.div_ceil(2)
}

#[tokio::test]
async fn a_codec_the_webview_cant_decode_comes_as_i420_fitted_and_at_most_15_fps() {
    let (w, call) = in_call().await;
    let ch = Arc::new(RecordingFrameChannel::new());
    // The webview decodes H.264 only; the camera is VP8.
    let ans = w
        .svc
        .video_sink(sink_args(CAM, 1280, 720, &["h264"]), ch.clone())
        .await
        .expect("sink");
    assert_eq!(
        serde_json::to_value(&ans).expect("json"),
        json!({"sink_id": ans.sink_id, "codec": null, "mode": "decoded"})
    );
    assert_eq!(
        w.decoders.opened(),
        vec![DecoderSpec {
            codec: VideoCodec::Vp8,
            format: DecodedFormat::I420,
            fit: (640, 360),
        }],
        "the sink's size, capped at 640x360"
    );
    // Like an encoded sink, it asked for a keyframe and drops what comes before it.
    assert_eq!(call.keyframe_requests(), vec![CAM.to_owned()]);
    push(&call, frame(CAM, VideoCodec::Vp8, false, 1, 1000)).await;
    assert!(ch.messages().is_empty(), "nothing before the keyframe");
    assert!(
        w.decoders.submitted().is_empty(),
        "the delta never reached the decoder"
    );

    push(&call, frame(CAM, VideoCodec::Vp8, true, 2, 1033)).await;
    until("the first decoded frame", || ch.messages().len() == 1).await;
    let (h, payload) = &ch.frames()[0];
    assert_eq!(
        *h,
        FrameHeader {
            kind: kind::I420,
            codec: 0,
            flags: flag::DISCONTINUITY,
            sink_id: ans.sink_id,
            timestamp_us: 1_033_000,
            width: 640,
            height: 360,
            seq: 0,
            len: u32::try_from(i420(640, 360)).expect("len"),
        }
    );
    assert!(payload.iter().all(|&b| b == 2), "the decoder's pixels");

    // 30 fps in: every frame is decoded (they are references), every other one is sent.
    for (n, at) in [(3u8, 1066u64), (4, 1100), (5, 1133), (6, 1166)] {
        push(&call, frame(CAM, VideoCodec::Vp8, false, n, at)).await;
    }
    until("three frames", || ch.messages().len() == 3).await;
    settle().await;
    let f = ch.frames();
    assert_eq!(f.len(), 3, "15 fps out");
    assert_eq!(
        f.iter()
            .map(|(h, p)| (h.timestamp_us, h.flags, h.seq, p[0]))
            .collect::<Vec<_>>(),
        vec![
            (1_033_000, 2, 0, 2),
            (1_100_000, 0, 1, 4),
            (1_166_000, 0, 2, 6)
        ]
    );
    assert_eq!(
        w.decoders.submitted(),
        vec![(2, true), (3, false), (4, false), (5, false), (6, false)]
    );
    // Another track's frames never reach this decoder.
    push(&call, frame(SCREEN, VideoCodec::H264, true, 9, 1200)).await;
    assert_eq!(w.decoders.submitted().len(), 5);
}

#[tokio::test]
async fn without_webcodecs_the_frames_are_rgba_and_resizes_refit() {
    let (w, call) = in_call().await;
    let ch = Arc::new(RecordingFrameChannel::new());
    let ans = w
        .svc
        .video_sink(sink_args(SCREEN, 320, 240, &[]), ch.clone())
        .await
        .expect("sink");
    assert_eq!(ans.mode, tauri_plugin_sew_voice::SinkMode::Decoded);
    assert_eq!(
        w.decoders.opened(),
        vec![DecoderSpec {
            codec: VideoCodec::H264,
            format: DecodedFormat::Rgba,
            fit: (320, 240),
        }]
    );
    push(&call, frame(SCREEN, VideoCodec::H264, true, 1, 1000)).await;
    until("an RGBA frame", || ch.messages().len() == 1).await;
    let (h, payload) = &ch.frames()[0];
    assert_eq!(
        (h.kind, h.codec, h.width, h.height),
        (kind::RGBA, 0, 320, 180)
    );
    assert_eq!(payload.len(), 320 * 180 * 4);

    // A larger tile: the decoder's box follows, still capped.
    w.svc
        .video_sink_update(VideoSinkUpdateArgs {
            sink_id: ans.sink_id,
            width: Some(1920),
            height: Some(1080),
            visible: None,
            tile: None,
            stats: None,
        })
        .await
        .expect("update");
    assert_eq!(w.decoders.fits(), vec![(640, 360)]);
    push(&call, frame(SCREEN, VideoCodec::H264, false, 2, 1100)).await;
    until("a refitted frame", || ch.messages().len() == 2).await;
    let (h, payload) = &ch.frames()[1];
    assert_eq!((h.width, h.height), (640, 360));
    assert_eq!(payload.len(), 640 * 360 * 4);
}

#[tokio::test]
async fn a_decoder_error_resets_the_sink_and_asks_for_a_keyframe() {
    let (w, call) = in_call().await;
    let ch = Arc::new(RecordingFrameChannel::new());
    let id = w
        .svc
        .video_sink(sink_args(CAM, 640, 360, &[]), ch.clone())
        .await
        .expect("sink")
        .sink_id;
    push(&call, frame(CAM, VideoCodec::Vp8, true, 1, 1000)).await;
    until("a frame", || ch.messages().len() == 1).await;
    let resets = w.decoders.resets();

    // The decoder fails on a delta's output: a reset message, the decoder reset, a PLI.
    w.decoders
        .fail_next_output(DecodeFailure::Failed("corrupt: bad partition".into()));
    push(&call, frame(CAM, VideoCodec::Vp8, false, 2, 1100)).await;
    until("the reset", || ch.messages().len() == 2).await;
    let (h, payload) = &ch.frames()[1];
    assert_eq!((h.kind, h.sink_id, h.len, h.seq), (kind::RESET, id, 0, 1));
    assert!(payload.is_empty());
    assert_eq!(w.decoders.resets(), resets + 1);
    assert_eq!(
        call.keyframe_requests(),
        vec![CAM.to_owned(), CAM.to_owned()],
        "the open's, then the error's"
    );
    // Deltas wait for the keyframe, which restarts the decoder.
    push(&call, frame(CAM, VideoCodec::Vp8, false, 3, 1200)).await;
    push(&call, frame(CAM, VideoCodec::Vp8, true, 4, 1300)).await;
    until("the keyframe's frame", || ch.messages().len() == 3).await;
    let (h, payload) = &ch.frames()[2];
    assert_eq!((h.kind, h.flags), (kind::RGBA, flag::DISCONTINUITY));
    assert_eq!(payload[0], 4);
    assert!(
        !w.decoders.submitted().contains(&(3, false)),
        "the delta after the error never reached the decoder"
    );

    // A refused input (the decoder's queue is full) waits in the sink's backlog and goes in
    // once the decoder has room: nothing is lost, no keyframe is asked, the order is kept.
    w.decoders.fail_next_submit(DecodeFailure::Busy);
    push(&call, frame(CAM, VideoCodec::Vp8, false, 5, 1400)).await;
    push(&call, frame(CAM, VideoCodec::Vp8, false, 6, 1500)).await;
    until("the waiting frames", || ch.messages().len() == 5).await;
    assert_eq!(
        call.keyframe_requests().len(),
        2,
        "no keyframe asked for a Busy input"
    );
    let f = ch.frames();
    assert_eq!((f[3].1[0], f[4].1[0]), (5, 6), "in order");
    assert_eq!(f[3].0.flags & flag::DISCONTINUITY, 0, "no break");

    // A broken track: the reset message (as for an encoded sink), then the decoder starts over
    // from the next keyframe.
    push(&call, VideoRecvItem::Broken { track: CAM.into() }).await;
    push(&call, frame(CAM, VideoCodec::Vp8, true, 7, 1600)).await;
    until("after the break", || ch.messages().len() == 7).await;
    let f = ch.frames();
    assert_eq!(f[5].0.kind, kind::RESET);
    assert_eq!((f[6].0.flags, f[6].1[0]), (flag::DISCONTINUITY, 7));
}

#[tokio::test]
async fn at_most_four_decoded_sinks() {
    let (w, call) = in_call().await;
    let mut ids = Vec::new();
    for _ in 0..MAX_DECODED_SINKS {
        let ch = Arc::new(RecordingFrameChannel::new());
        let a = w
            .svc
            .video_sink(sink_args(CAM, 320, 180, &["h264"]), ch)
            .await
            .expect("decoded sink");
        ids.push(a.sink_id);
    }
    let ch = Arc::new(RecordingFrameChannel::new());
    let err = w
        .svc
        .video_sink(sink_args(CAM, 320, 180, &["h264"]), ch.clone())
        .await
        .expect_err("a fifth");
    assert_eq!(err.code, "decoded_tiles_full");
    assert_eq!(err.detail, json!({"max": 4}));
    assert_eq!(
        w.decoders.opened().len(),
        MAX_DECODED_SINKS,
        "no decoder for it"
    );
    // An encoded sink is never refused for it.
    w.svc
        .video_sink(sink_args(CAM, 320, 180, &["vp8"]), ch.clone())
        .await
        .expect("an encoded sink");
    // A decoded sink closes: the slot is free again.
    w.svc
        .video_sink_close(SinkIdArgs { sink_id: ids[0] })
        .await
        .expect("close");
    w.svc
        .video_sink(sink_args(CAM, 320, 180, &["h264"]), ch)
        .await
        .expect("a free slot");
    drop(call);
}

#[tokio::test]
async fn the_engine_must_decode_the_codec() {
    let w = world();
    // The engine decodes VP8 only.
    let d = Arc::clone(&w.decoders);
    w.engine.set_video_decoder(
        vec![VideoCodec::Vp8],
        Arc::new(move |_spec, _wake| Err(tauri_plugin_sew_voice::EngineError::failed("nope"))),
    );
    w.svc
        .join(JoinArgs {
            target: "k2:acme".into(),
            room: "48831765147549698".into(),
            muted: None,
            deafened: None,
        })
        .await
        .expect("join");
    let call = w.engine.last_call().expect("call");
    call.set_video_tracks(vec![
        track(CAM, VideoSource::Camera, VideoCodec::Vp8),
        track(SCREEN, VideoSource::Screen, VideoCodec::H264),
    ]);
    until("the video list", || w.svc.snapshot().video.len() == 2).await;
    let ch = Arc::new(RecordingFrameChannel::new());
    let err = w
        .svc
        .video_sink(sink_args(SCREEN, 320, 180, &["vp8"]), ch.clone())
        .await
        .expect_err("no H.264 decoder");
    assert_eq!(
        (err.code.as_str(), err.detail.clone()),
        ("decoded_unsupported", json!({"codec": "h264"}))
    );
    // Listed, but the decoder doesn't open: refused the same way.
    let err = w
        .svc
        .video_sink(sink_args(CAM, 320, 180, &["h264"]), ch)
        .await
        .expect_err("the decoder failed to open");
    assert_eq!(
        (err.code.as_str(), err.detail),
        ("decoded_unsupported", json!({"codec": "vp8"}))
    );
    assert!(d.opened().is_empty());
}

#[tokio::test]
async fn outputs_that_come_later_are_sent_when_the_decoder_wakes() {
    let (w, call) = in_call().await;
    w.decoders.hold();
    let ch = Arc::new(RecordingFrameChannel::new());
    w.svc
        .video_sink(sink_args(CAM, 640, 360, &["h264"]), ch.clone())
        .await
        .expect("sink");
    push(&call, frame(CAM, VideoCodec::Vp8, true, 1, 1000)).await;
    push(&call, frame(CAM, VideoCodec::Vp8, false, 2, 1100)).await;
    assert!(ch.messages().is_empty(), "the decoder holds its outputs");
    // The decoder's thread queues them and wakes the sink: its drain task sends them.
    w.decoders.release();
    until("both frames", || ch.messages().len() == 2).await;
    assert_eq!(
        ch.frames().iter().map(|(_, p)| p[0]).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[tokio::test]
async fn a_hidden_decoded_sink_decodes_nothing_and_counts_its_own_stats() {
    let (w, call) = in_call().await;
    let ch = Arc::new(RecordingFrameChannel::new());
    let mut a = sink_args(CAM, 640, 360, &["h264"]);
    a.visible = false;
    let id = w.svc.video_sink(a, ch.clone()).await.expect("sink").sink_id;
    push(&call, frame(CAM, VideoCodec::Vp8, true, 1, 1000)).await;
    assert!(w.decoders.submitted().is_empty(), "hidden: nothing decoded");
    w.svc
        .video_sink_update(VideoSinkUpdateArgs {
            sink_id: id,
            width: None,
            height: None,
            visible: Some(true),
            tile: None,
            stats: None,
        })
        .await
        .expect("shown");
    push(&call, frame(CAM, VideoCodec::Vp8, false, 2, 1100)).await;
    push(&call, frame(CAM, VideoCodec::Vp8, true, 3, 1200)).await;
    push(&call, frame(CAM, VideoCodec::Vp8, false, 4, 1300)).await;
    until("two frames", || ch.messages().len() == 2).await;
    // The webview's counters of a decoded sink are not what the engine gets: the native
    // decoder's are (2 sent; the delta before the keyframe never reached it).
    w.svc
        .video_sink_update(VideoSinkUpdateArgs {
            sink_id: id,
            width: None,
            height: None,
            visible: None,
            tile: None,
            stats: Some(SinkStats {
                decoded: 99,
                dropped: 99,
                freezes: 9,
                freeze_ms: 999,
            }),
        })
        .await
        .expect("stats");
    assert_eq!(
        call.video_stats(),
        vec![(
            CAM.to_owned(),
            SinkStats {
                decoded: 2,
                dropped: 0,
                freezes: 0,
                freeze_ms: 0
            }
        )]
    );
}

#[tokio::test]
async fn a_self_view_sink_without_webcodecs_decodes_the_preview() {
    let w = world();
    w.svc
        .preview(PreviewArgs {
            on: true,
            device: None,
        })
        .await
        .expect("preview");
    let ch = Arc::new(RecordingFrameChannel::new());
    let ans = w
        .svc
        .video_sink(sink_args(LOCAL_CAMERA, 320, 180, &[]), ch.clone())
        .await
        .expect("self-view sink");
    assert_eq!(ans.mode, tauri_plugin_sew_voice::SinkMode::Decoded);
    assert!(
        w.decoders.opened().is_empty(),
        "opened with the first keyframe's codec"
    );
    let tx = w.engine.preview_sender().expect("preview running");
    tx.try_send(frame(LOCAL_CAMERA, VideoCodec::H264, true, 1, 1000))
        .expect("send");
    until("the self-view frame", || ch.messages().len() == 1).await;
    let (h, _) = &ch.frames()[0];
    assert_eq!(
        (h.kind, h.flags, h.width, h.height),
        (kind::RGBA, flag::DISCONTINUITY | flag::SELF_VIEW, 320, 180)
    );
    assert_eq!(
        w.decoders.opened(),
        vec![DecoderSpec {
            codec: VideoCodec::H264,
            format: DecodedFormat::Rgba,
            fit: (320, 180),
        }]
    );
}
