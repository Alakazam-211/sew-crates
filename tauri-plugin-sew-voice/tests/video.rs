//! Video: the commands, the events and the frame sinks' gating, against the fake engine.

// The helpers (not only #[test] fns) unwrap: a test that cannot set up must fail loudly.
#![allow(clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value, json};
use tauri_plugin_sew_voice::fake::{
    FakeCall, FakeEngine, FakeProvider, RecordingFrameChannel, RecordingSink, demo_cameras,
};
use tauri_plugin_sew_voice::video::{LOCAL_CAMERA, flag, kind};
use tauri_plugin_sew_voice::{
    CameraArgs, EncodedVideoFrame, EngineError, FrameHeader, JoinArgs, LocalVideo, LocalVideoState,
    PreviewArgs, ScreenArgs, ScreenCaps, SetVideoDeviceArgs, SinkIdArgs, SinkStats, Spawn, Tile,
    TrackSettings, VideoCaps, VideoCodec, VideoRecvItem, VideoSinkArgs, VideoSinkUpdateArgs,
    VideoSource, VideoTrack, VoiceService,
};

const ME: &str = "48831765147549699";
const PEER: &str = "48831765147549700";
const CAM: &str = "TR_VCpeercamera";
const SCREEN: &str = "TR_VSpeerscreen";

struct World {
    svc: VoiceService,
    engine: Arc<FakeEngine>,
    sink: Arc<RecordingSink>,
}

fn world(video: bool) -> World {
    let engine = Arc::new(FakeEngine::new());
    if video {
        engine.enable_video();
    }
    let sink = Arc::new(RecordingSink::new());
    let spawn: Spawn = Arc::new(|f| {
        tokio::spawn(f);
    });
    let svc = VoiceService::new(
        engine.clone(),
        Arc::new(FakeProvider::answering(ME)),
        sink.clone(),
        spawn,
    );
    svc.start();
    World { svc, engine, sink }
}

fn args() -> JoinArgs {
    JoinArgs {
        target: "k2:acme".into(),
        room: "48831765147549698".into(),
        muted: None,
        deafened: None,
    }
}

/// Let spawned tasks run until `pred` holds; panics (loudly) if it never does.
async fn until(what: &str, mut pred: impl FnMut() -> bool) {
    for _ in 0..2000 {
        if pred() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("timed out waiting for: {what}");
}

/// Let spawned tasks run for a while (to show that nothing more arrives).
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

/// A joined call with the peer's camera (VP8) and screen (H.264) in the `video` list.
async fn in_call() -> (World, Arc<FakeCall>) {
    let w = world(true);
    w.svc.join(args()).await.expect("join");
    let call = w.engine.last_call().expect("call");
    call.set_video_tracks(vec![
        track(CAM, VideoSource::Camera, VideoCodec::Vp8),
        track(SCREEN, VideoSource::Screen, VideoCodec::H264),
    ]);
    until("the video list", || w.svc.snapshot().video.len() == 2).await;
    (w, call)
}

fn sink_args(track: &str, w: u32, h: u32, visible: bool, tile: Tile) -> VideoSinkArgs {
    VideoSinkArgs {
        track: track.into(),
        width: w,
        height: h,
        visible,
        tile,
        decode: vec!["vp8".into(), "h264".into(), "av1".into()],
    }
}

fn frame(track: &str, codec: VideoCodec, keyframe: bool, n: u8) -> VideoRecvItem {
    VideoRecvItem::Frame(EncodedVideoFrame {
        track: track.into(),
        codec,
        data: vec![n; 40 + usize::from(n)],
        keyframe,
        rtp_ts: 90_000 + u32::from(n) * 3000,
        arrival_us: 1_000_000 + u64::from(n),
        width: if keyframe { 1280 } else { 0 },
        height: if keyframe { 720 } else { 0 },
    })
}

/// Pushes an item and lets the frame task route it.
async fn push(call: &FakeCall, item: VideoRecvItem) {
    assert!(call.push_video(item), "the frame channel took the item");
    settle().await;
}

#[tokio::test]
async fn without_video_the_voice_payloads_are_unchanged() {
    let w = world(false);
    assert_eq!(
        w.svc.video_caps().await.expect("caps"),
        VideoCaps::default()
    );
    assert_eq!(
        serde_json::to_value(VideoCaps::default()).expect("json"),
        json!({
            "camera": false,
            "screen": {"supported": false, "system_picker": false, "list": false, "windows": false},
            "publish_codecs": [], "decode_codecs": [], "receive": false, "self_view": false, "live_switch": false, "backend": "null"
        })
    );
    // An older plugin's answer has neither flag: both read as false.
    let older: VideoCaps = serde_json::from_value(json!({
        "camera": true,
        "screen": {"supported": true, "system_picker": true, "list": true, "windows": true},
        "publish_codecs": ["vp8"], "decode_codecs": [], "receive": true, "backend": "fake"
    }))
    .expect("an older payload");
    assert!(!older.self_view && !older.live_switch);
    assert_eq!(
        w.svc.video_devices().await.expect_err("no video").code,
        "video_unsupported"
    );
    assert_eq!(
        w.svc
            .preview(PreviewArgs {
                on: true,
                device: None
            })
            .await
            .expect_err("no video")
            .code,
        "video_unsupported"
    );
    w.svc.join(args()).await.expect("join");
    let err = w
        .svc
        .camera(CameraArgs {
            on: true,
            device: None,
        })
        .await
        .expect_err("no video");
    assert_eq!(err.code, "video_unsupported");
    settle().await;
    let snap = serde_json::to_value(w.svc.snapshot()).expect("json");
    for k in ["video", "local_video", "policy"] {
        assert!(
            snap.get(k).is_none(),
            "a voice-only state has no {k}: {snap}"
        );
    }
    for name in ["video", "local_video", "video_devices", "policy"] {
        assert!(
            w.sink.of(name).is_empty(),
            "no {name} event in a voice call"
        );
    }
}

#[tokio::test]
async fn video_tracks_local_video_and_policy_follow_the_engine() {
    let (w, call) = in_call().await;
    assert_eq!(
        w.sink.of("video"),
        vec![json!([
            {"member_id": PEER, "is_self": false, "source": "camera", "track": CAM,
             "codec": "vp8", "width": 1280, "height": 720, "muted": false, "paused": null},
            {"member_id": PEER, "is_self": false, "source": "screen", "track": SCREEN,
             "codec": "h264", "width": 1280, "height": 720, "muted": false, "paused": null},
        ])]
    );
    call.set_policy(json!({"video_enabled": true, "adaptive_stream": true}));
    until("policy", || w.sink.of("policy").len() == 1).await;
    assert_eq!(
        w.sink.of("policy"),
        vec![json!({"video_enabled": true, "adaptive_stream": true})]
    );
    w.svc
        .camera(CameraArgs {
            on: true,
            device: Some("cam:usb".into()),
        })
        .await
        .expect("camera on");
    until("camera on", || w.sink.of("local_video").len() == 2).await;
    assert_eq!(
        w.sink.of("local_video"),
        vec![
            json!({"camera": "asking", "screen": "off"}),
            json!({"camera": "on", "screen": "off"})
        ]
    );
    let reqs = call.camera_requests();
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0].on);
    let snap = serde_json::to_value(w.svc.snapshot()).expect("json");
    assert_eq!(snap["video"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        snap["local_video"],
        json!({"camera": "on", "screen": "off"})
    );
    assert_eq!(snap["policy"]["video_enabled"], json!(true));

    // Leave: the list empties, the camera is off, the policy is gone from the state.
    w.svc.leave().await.expect("leave");
    assert_eq!(w.sink.of("video").last(), Some(&json!([])));
    assert_eq!(
        w.sink.of("local_video").last(),
        Some(&json!({"camera": "off", "screen": "off"}))
    );
    let snap = serde_json::to_value(w.svc.snapshot()).expect("json");
    assert!(snap.get("policy").is_none() && snap.get("video").is_none());
}

#[tokio::test]
async fn camera_refusals_reject_with_the_code_and_say_why() {
    let (w, call) = in_call().await;
    call.fail_next_camera(EngineError::failed("video_slots_full"));
    let err = w
        .svc
        .camera(CameraArgs {
            on: true,
            device: None,
        })
        .await
        .expect_err("slots full");
    assert_eq!(err.code, "video_slots_full");
    until("local_video", || w.sink.of("local_video").len() == 2).await;
    assert_eq!(
        w.sink.of("local_video").last(),
        Some(&json!({"camera": "off", "screen": "off", "reason": "video_slots_full"}))
    );
    // No call: not_in_call.
    let w2 = world(true);
    assert_eq!(
        w2.svc
            .camera(CameraArgs {
                on: true,
                device: None
            })
            .await
            .expect_err("no call")
            .code,
        "not_in_call"
    );
}

#[tokio::test]
async fn screen_needs_capture_support() {
    let (w, call) = in_call().await;
    w.svc
        .screen(
            ScreenArgs {
                on: true,
                source: Some("display:1".into()),
            },
            None,
        )
        .await
        .expect("screen on");
    let reqs = call.screen_requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].source.as_deref(), Some("display:1"));
    assert_eq!(reqs[0].parent_window, None);
    // A device without screen capture (Linux without the portal).
    let mut caps = w.svc.video_caps().await.expect("caps");
    caps.screen = ScreenCaps::default();
    w.engine.set_video_caps(caps);
    let err = w
        .svc
        .screen(
            ScreenArgs {
                on: true,
                source: None,
            },
            None,
        )
        .await
        .expect_err("unsupported");
    assert_eq!(err.code, "screen_unsupported");
    assert_eq!(call.screen_requests().len(), 1, "the engine wasn't asked");
}

#[tokio::test]
async fn video_devices_and_their_events() {
    let w = world(true);
    assert_eq!(
        w.engine.video_device_subscribers(),
        0,
        "nothing before a use"
    );
    let list = w.svc.video_devices().await.expect("devices");
    assert_eq!(list, demo_cameras());
    assert_eq!(
        serde_json::to_value(&list).expect("json")["cameras"][0],
        json!({"id": "cam:builtin", "name": "Built-in camera", "facing": "front", "default": true})
    );
    assert_eq!(
        serde_json::to_value(&list).expect("json")["permission"],
        json!({"camera": "granted", "screen": "prompt"})
    );
    assert_eq!(w.engine.video_device_subscribers(), 1);
    w.svc
        .set_video_device(SetVideoDeviceArgs {
            camera: "cam:usb".into(),
        })
        .await
        .expect("set");
    until("video_devices", || w.sink.of("video_devices").len() == 1).await;
    assert_eq!(w.sink.of("video_devices")[0]["camera"], json!("cam:usb"));
    let err = w
        .svc
        .set_video_device(SetVideoDeviceArgs {
            camera: "cam:nope".into(),
        })
        .await
        .expect_err("unknown");
    assert_eq!(err.code, "camera_not_found");
    w.svc
        .set_video_device(SetVideoDeviceArgs {
            camera: "default".into(),
        })
        .await
        .expect("default");
    until("default", || w.sink.of("video_devices").len() == 2).await;
    assert_eq!(w.sink.of("video_devices")[1]["camera"], Value::Null);
}

#[tokio::test]
async fn a_sink_waits_for_a_keyframe_then_gets_every_frame() {
    let (w, call) = in_call().await;
    let ch = Arc::new(RecordingFrameChannel::new());
    let ans = w
        .svc
        .video_sink(sink_args(CAM, 640, 360, true, Tile::Grid), ch.clone())
        .await
        .expect("sink");
    assert_eq!(
        serde_json::to_value(&ans).expect("json"),
        json!({"sink_id": ans.sink_id, "codec": "vp8", "mode": "encoded"})
    );
    assert_eq!(
        call.video_settings(),
        vec![(
            CAM.to_owned(),
            TrackSettings {
                disabled: false,
                width: 640,
                height: 360,
                fps: 15
            }
        )]
    );
    assert_eq!(call.keyframe_requests(), vec![CAM.to_owned()]);

    // A delta before the first keyframe is dropped.
    push(&call, frame(CAM, VideoCodec::Vp8, false, 1)).await;
    assert!(ch.messages().is_empty(), "nothing before the keyframe");
    push(&call, frame(CAM, VideoCodec::Vp8, true, 2)).await;
    push(&call, frame(CAM, VideoCodec::Vp8, false, 3)).await;
    // Another track's frame doesn't reach this sink.
    push(&call, frame(SCREEN, VideoCodec::H264, true, 4)).await;
    until("two frames", || ch.messages().len() == 2).await;
    let f = ch.frames();
    let (h, payload) = &f[0];
    assert_eq!(
        *h,
        FrameHeader {
            kind: kind::ENCODED,
            codec: 1,
            flags: flag::KEYFRAME | flag::DISCONTINUITY,
            sink_id: ans.sink_id,
            timestamp_us: 1_000_002,
            width: 1280,
            height: 720,
            seq: 0,
            len: 42,
        }
    );
    assert_eq!(payload, &vec![2u8; 42]);
    let (h, payload) = &f[1];
    assert_eq!((h.flags, h.seq, h.width, h.height), (0, 1, 0, 0));
    assert_eq!(payload, &vec![3u8; 43]);
}

#[tokio::test]
async fn a_broken_track_resets_the_sink_until_the_next_keyframe() {
    let (w, call) = in_call().await;
    let ch = Arc::new(RecordingFrameChannel::new());
    let id = w
        .svc
        .video_sink(sink_args(CAM, 640, 360, true, Tile::Grid), ch.clone())
        .await
        .expect("sink")
        .sink_id;
    push(&call, frame(CAM, VideoCodec::Vp8, true, 1)).await;
    push(&call, VideoRecvItem::Broken { track: CAM.into() }).await;
    push(&call, frame(CAM, VideoCodec::Vp8, false, 2)).await;
    push(&call, frame(CAM, VideoCodec::Vp8, true, 3)).await;
    until("three messages", || ch.messages().len() == 3).await;
    let f = ch.frames();
    assert_eq!(
        f[1].0.kind,
        kind::RESET,
        "the reset comes before anything else"
    );
    assert_eq!((f[1].0.len, f[1].0.sink_id), (0, id));
    assert!(f[1].1.is_empty());
    assert_eq!(f[2].0.flags, flag::KEYFRAME | flag::DISCONTINUITY);
    assert_eq!(f[2].1, vec![3u8; 43]);
    assert_eq!(
        f.iter().map(|(h, _)| h.seq).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "every message counts"
    );

    // KeyframeNeeded: no reset, but the next delta is dropped.
    push(&call, VideoRecvItem::KeyframeNeeded { track: CAM.into() }).await;
    push(&call, frame(CAM, VideoCodec::Vp8, false, 4)).await;
    push(&call, frame(CAM, VideoCodec::Vp8, true, 5)).await;
    until("the keyframe", || ch.messages().len() == 4).await;
    assert_eq!(ch.frames()[3].0.flags, flag::KEYFRAME | flag::DISCONTINUITY);

    // request_keyframe: the same, and the engine is asked.
    w.svc
        .request_keyframe(SinkIdArgs { sink_id: id })
        .await
        .expect("keyframe");
    assert_eq!(
        call.keyframe_requests(),
        vec![CAM.to_owned(), CAM.to_owned()]
    );
    push(&call, frame(CAM, VideoCodec::Vp8, false, 6)).await;
    settle().await;
    assert_eq!(
        ch.messages().len(),
        4,
        "the delta after a request is dropped"
    );
}

#[tokio::test]
async fn hidden_sinks_get_nothing_and_settings_follow_the_union() {
    let (w, call) = in_call().await;
    let grid = Arc::new(RecordingFrameChannel::new());
    let large = Arc::new(RecordingFrameChannel::new());
    let g = w
        .svc
        .video_sink(sink_args(CAM, 320, 180, true, Tile::Grid), grid.clone())
        .await
        .expect("grid")
        .sink_id;
    let l = w
        .svc
        .video_sink(sink_args(CAM, 1280, 720, false, Tile::Large), large.clone())
        .await
        .expect("large")
        .sink_id;
    // The hidden large sink changes nothing.
    assert_eq!(call.video_settings().len(), 1);
    assert_eq!(
        call.keyframe_requests().len(),
        1,
        "no keyframe for a hidden sink"
    );
    push(&call, frame(CAM, VideoCodec::Vp8, true, 1)).await;
    until("grid frame", || grid.messages().len() == 1).await;
    assert!(large.messages().is_empty(), "a hidden sink gets nothing");

    // The large one shows: 1280x720 at 30, and it waits for a keyframe.
    w.svc
        .video_sink_update(VideoSinkUpdateArgs {
            sink_id: l,
            width: None,
            height: None,
            visible: Some(true),
            tile: None,
            stats: None,
        })
        .await
        .expect("show");
    assert_eq!(
        call.video_settings().last(),
        Some(&(
            CAM.to_owned(),
            TrackSettings {
                disabled: false,
                width: 1280,
                height: 720,
                fps: 30
            }
        ))
    );
    assert_eq!(call.keyframe_requests().len(), 2);
    push(&call, frame(CAM, VideoCodec::Vp8, false, 2)).await;
    until("grid delta", || grid.messages().len() == 2).await;
    assert!(
        large.messages().is_empty(),
        "the shown sink waits for a keyframe"
    );
    push(&call, frame(CAM, VideoCodec::Vp8, true, 3)).await;
    until("large keyframe", || large.messages().len() == 1).await;
    assert_eq!(
        large.frames()[0].0.flags,
        flag::KEYFRAME | flag::DISCONTINUITY
    );
    assert_eq!(
        grid.frames()[2].0.flags,
        flag::KEYFRAME,
        "no reset for the grid"
    );

    // The same settings again: nothing sent.
    let n = call.video_settings().len();
    w.svc
        .video_sink_update(VideoSinkUpdateArgs {
            sink_id: g,
            width: Some(320),
            height: None,
            visible: None,
            tile: None,
            stats: Some(SinkStats {
                decoded: 10,
                dropped: 1,
                freezes: 0,
                freeze_ms: 0,
            }),
        })
        .await
        .expect("stats");
    assert_eq!(
        call.video_settings().len(),
        n,
        "unchanged settings aren't resent"
    );
    w.svc
        .video_sink_update(VideoSinkUpdateArgs {
            sink_id: l,
            width: None,
            height: None,
            visible: None,
            tile: None,
            stats: Some(SinkStats {
                decoded: 5,
                dropped: 0,
                freezes: 1,
                freeze_ms: 120,
            }),
        })
        .await
        .expect("stats");
    assert_eq!(
        call.video_stats().last(),
        Some(&(
            CAM.to_owned(),
            SinkStats {
                decoded: 15,
                dropped: 1,
                freezes: 1,
                freeze_ms: 120
            }
        )),
        "the track's sinks, summed"
    );

    // Both closed: disabled again.
    w.svc
        .video_sink_close(SinkIdArgs { sink_id: l })
        .await
        .expect("close");
    assert_eq!(
        call.video_settings().last().map(|(_, s)| *s),
        Some(TrackSettings {
            disabled: false,
            width: 320,
            height: 180,
            fps: 15
        })
    );
    w.svc
        .video_sink_close(SinkIdArgs { sink_id: g })
        .await
        .expect("close");
    assert_eq!(
        call.video_settings().last().map(|(_, s)| *s),
        Some(TrackSettings::DISABLED)
    );
    w.svc
        .video_sink_close(SinkIdArgs { sink_id: g })
        .await
        .expect("closing twice is fine");
    let before = grid.messages().len();
    push(&call, frame(CAM, VideoCodec::Vp8, true, 4)).await;
    assert_eq!(grid.messages().len(), before, "a closed sink gets nothing");
    let err = w
        .svc
        .request_keyframe(SinkIdArgs { sink_id: g })
        .await
        .expect_err("closed");
    assert_eq!(err.code, "unknown_sink");
}

#[tokio::test]
async fn a_gone_webview_closes_its_sink() {
    let (w, call) = in_call().await;
    let ch = Arc::new(RecordingFrameChannel::new());
    let id = w
        .svc
        .video_sink(sink_args(CAM, 640, 360, true, Tile::Grid), ch.clone())
        .await
        .expect("sink")
        .sink_id;
    ch.close();
    push(&call, frame(CAM, VideoCodec::Vp8, true, 1)).await;
    until("disabled", || {
        call.video_settings().last().map(|(_, s)| *s) == Some(TrackSettings::DISABLED)
    })
    .await;
    assert_eq!(
        w.svc
            .video_sink_update(VideoSinkUpdateArgs {
                sink_id: id,
                width: Some(1),
                height: None,
                visible: None,
                tile: None,
                stats: None,
            })
            .await
            .expect_err("gone")
            .code,
        "unknown_sink"
    );
}

#[tokio::test]
async fn sink_refusals() {
    let w = world(true);
    let ch = Arc::new(RecordingFrameChannel::new());
    assert_eq!(
        w.svc
            .video_sink(sink_args(CAM, 1, 1, true, Tile::Grid), ch.clone())
            .await
            .expect_err("no call")
            .code,
        "not_in_call"
    );
    let (w, _call) = in_call().await;
    let err = w
        .svc
        .video_sink(sink_args("TR_nope", 1, 1, true, Tile::Grid), ch.clone())
        .await
        .expect_err("unknown");
    assert_eq!(err.code, "unknown_track");
    assert_eq!(err.detail, json!({"track": "TR_nope"}));
    // A webview that decodes VP8 only, on the H.264 screen: the fake engine decodes no H.264.
    let mut a = sink_args(SCREEN, 1, 1, true, Tile::Large);
    a.decode = vec!["vp8".into()];
    let err = w.svc.video_sink(a, ch.clone()).await.expect_err("decoded");
    assert_eq!(err.code, "decoded_unsupported");
    assert_eq!(err.detail, json!({"codec": "h264"}));
}

#[tokio::test]
async fn a_track_that_comes_back_is_enabled_again() {
    let (w, call) = in_call().await;
    let ch = Arc::new(RecordingFrameChannel::new());
    w.svc
        .video_sink(sink_args(CAM, 640, 360, true, Tile::Grid), ch.clone())
        .await
        .expect("sink");
    push(&call, frame(CAM, VideoCodec::Vp8, true, 1)).await;
    push(&call, VideoRecvItem::Removed { track: CAM.into() }).await;
    push(&call, VideoRecvItem::Added { track: CAM.into() }).await;
    until("re-enabled", || call.video_settings().len() == 2).await;
    assert_eq!(call.video_settings()[0], call.video_settings()[1]);
    assert_eq!(call.keyframe_requests().len(), 2);
    let f = ch.frames();
    assert_eq!(f.last().map(|(h, _)| h.kind), Some(kind::RESET));
}

#[tokio::test]
async fn self_view_and_the_preview() {
    let w = world(true);
    let ans = w
        .svc
        .preview(PreviewArgs {
            on: true,
            device: Some("cam:usb".into()),
        })
        .await
        .expect("preview");
    assert_eq!(
        serde_json::to_value(&ans).expect("json"),
        json!({"sink_track": "local:camera"})
    );
    let ch = Arc::new(RecordingFrameChannel::new());
    let s = w
        .svc
        .video_sink(
            sink_args(LOCAL_CAMERA, 320, 180, true, Tile::Grid),
            ch.clone(),
        )
        .await
        .expect("self-view sink, no call needed");
    assert_eq!(s.codec, None);
    assert_eq!(
        w.engine.preview_keyframes(),
        1,
        "the preview's encoder is asked"
    );
    let tx = w.engine.preview_sender().expect("preview running");
    tx.try_send(frame(LOCAL_CAMERA, VideoCodec::Vp8, true, 1))
        .expect("send");
    until("self-view frame", || ch.messages().len() == 1).await;
    assert_eq!(
        ch.frames()[0].0.flags,
        flag::KEYFRAME | flag::DISCONTINUITY | flag::SELF_VIEW
    );
    w.svc
        .preview(PreviewArgs {
            on: false,
            device: None,
        })
        .await
        .expect("preview off");
    assert!(w.engine.preview_sender().is_none());
    assert_eq!(
        w.engine.previews().iter().map(|p| p.on).collect::<Vec<_>>(),
        vec![true, false]
    );

    // In a call the self-view comes on the call's channel; local tracks send no settings.
    w.svc.join(args()).await.expect("join");
    let call = w.engine.last_call().expect("call");
    push(&call, frame(LOCAL_CAMERA, VideoCodec::Vp8, false, 2)).await;
    push(&call, frame(LOCAL_CAMERA, VideoCodec::Vp8, false, 3)).await;
    until("self-view in a call", || ch.messages().len() == 3).await;
    assert_eq!(ch.frames()[2].0.flags, flag::SELF_VIEW);
    assert!(
        call.video_settings().is_empty(),
        "no track settings for self-view"
    );
}

#[tokio::test]
async fn a_new_call_starts_with_every_track_disabled() {
    let (w, call) = in_call().await;
    let ch = Arc::new(RecordingFrameChannel::new());
    let id = w
        .svc
        .video_sink(sink_args(CAM, 640, 360, true, Tile::Grid), ch.clone())
        .await
        .expect("sink")
        .sink_id;
    assert_eq!(call.video_settings().len(), 1);
    w.svc.leave().await.expect("leave");
    w.svc.join(args()).await.expect("join again");
    let call2 = w.engine.last_call().expect("call");
    call2.set_video_tracks(vec![track(CAM, VideoSource::Camera, VideoCodec::Vp8)]);
    until("the video list", || w.svc.snapshot().video.len() == 1).await;
    // The old sink is still open (the webview closes it): a change resends its settings to
    // the new call's engine.
    w.svc
        .video_sink_update(VideoSinkUpdateArgs {
            sink_id: id,
            width: Some(641),
            height: None,
            visible: None,
            tile: None,
            stats: None,
        })
        .await
        .expect("update");
    assert_eq!(call2.video_settings().len(), 1);
    assert_eq!(call.video_settings().len(), 1, "the old call isn't told");
    // The old call's frame task stopped with the leave: its frames go nowhere.
    let _ = call.push_video(frame(CAM, VideoCodec::Vp8, true, 9));
    settle().await;
    assert!(ch.messages().is_empty(), "nothing from the old call");
}

#[tokio::test]
async fn local_video_reasons_serialize() {
    let lv = LocalVideo {
        camera: LocalVideoState::Paused,
        screen: LocalVideoState::Off,
        reason: Some("background".into()),
    };
    assert_eq!(
        serde_json::to_value(&lv).expect("json"),
        json!({"camera": "paused", "screen": "off", "reason": "background"})
    );
    assert!(LocalVideo::default().is_idle());
    assert!(!lv.is_idle());
}

#[tokio::test]
async fn a_remote_unmute_restarts_at_a_keyframe() {
    let (w, call) = in_call().await;
    let ch = Arc::new(RecordingFrameChannel::new());
    w.svc
        .video_sink(sink_args(CAM, 640, 360, true, Tile::Grid), ch.clone())
        .await
        .expect("sink");
    push(&call, frame(CAM, VideoCodec::Vp8, true, 1)).await;
    let mut muted = track(CAM, VideoSource::Camera, VideoCodec::Vp8);
    muted.muted = true;
    call.set_video_tracks(vec![
        muted.clone(),
        track(SCREEN, VideoSource::Screen, VideoCodec::H264),
    ]);
    until("muted", || {
        w.svc.snapshot().video.first().is_some_and(|t| t.muted)
    })
    .await;
    let before = call.keyframe_requests().len();
    muted.muted = false;
    call.set_video_tracks(vec![
        muted,
        track(SCREEN, VideoSource::Screen, VideoCodec::H264),
    ]);
    until("unmuted keyframe request", || {
        call.keyframe_requests().len() == before + 1
    })
    .await;
    push(&call, frame(CAM, VideoCodec::Vp8, false, 2)).await;
    push(&call, frame(CAM, VideoCodec::Vp8, true, 3)).await;
    until("the keyframe", || ch.messages().len() == 2).await;
    assert_eq!(ch.frames()[1].0.flags, flag::KEYFRAME | flag::DISCONTINUITY);
}
