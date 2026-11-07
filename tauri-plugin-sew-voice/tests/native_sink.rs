//! Native sinks (feature `video-core`), against the fake engine, kessel-video's fake backend
//! (its pure-Rust VP8 decoder) and Sew's pre-encoded fixture streams: H.264 access units pass
//! through unchanged, VP8 is decoded, an active-speaker sink follows the speaker with a
//! keyframe request on each switch, and it is the one sink that stays on in the background.
#![cfg(feature = "video-core")]
// The helpers (not only #[test] fns) unwrap: a test that cannot set up must fail loudly.
#![allow(clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kessel_video::fake::{FakeBackend, FakeConfig};
use tauri_plugin_sew_voice::engine::CallEvent;
use tauri_plugin_sew_voice::fake::{
    FakeCall, FakeEngine, FakeProvider, RecordingFrameChannel, RecordingSink,
};
use tauri_plugin_sew_voice::native::{
    NativePayload, NativeSinkTarget, NativeVideoEvent, h264_nal_units, h264_parameter_sets,
};
use tauri_plugin_sew_voice::{
    AppScene, EncodedVideoFrame, JoinArgs, Spawn, Speaking, Tile, TrackSettings, VideoCodec,
    VideoRecvItem, VideoSinkArgs, VideoSource, VideoTrack, VoiceControl, VoiceService,
};

const ME: &str = "48831765147549699";
const ANN: &str = "48831765147549700";
const BOB: &str = "48831765147549701";
const ANN_CAM: &str = "TR_VCanncamera";
const BOB_CAM: &str = "TR_VCbobcamera";
const BOB_SCREEN: &str = "TR_VSbobscreen";

const PIP: (u32, u32) = (640, 360);
const PIP_SETTINGS: TrackSettings = TrackSettings {
    disabled: false,
    width: 640,
    height: 360,
    fps: 30,
};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/../sew-media-testclient/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("the fixture {path}: {e}"))
}

/// The fixture's H.264 access units (each slice closes one; AUDs dropped), with 4-byte start
/// codes, and whether each is an IDR.
fn h264_units() -> Vec<(bool, Vec<u8>)> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    let mut idr = false;
    for nal in h264_nal_units(&fixture("h264_sim_q.h264")) {
        let t = nal[0] & 0x1f;
        if t == 9 {
            continue;
        }
        cur.extend_from_slice(&[0, 0, 0, 1]);
        cur.extend_from_slice(nal);
        idr |= t == 5;
        if t == 1 || t == 5 {
            out.push((idr, std::mem::take(&mut cur)));
            idr = false;
        }
    }
    out
}

/// The fixture's VP8 frames (IVF) and whether each is a keyframe (the P bit clear).
fn vp8_frames() -> Vec<(bool, Vec<u8>)> {
    vp8_frames_of("vp8_sim_q.ivf")
}

/// One IVF fixture's VP8 frames.
fn vp8_frames_of(name: &str) -> Vec<(bool, Vec<u8>)> {
    let b = fixture(name);
    assert_eq!(&b[..4], b"DKIF", "an IVF file");
    let mut pos = usize::from(u16::from_le_bytes([b[6], b[7]]));
    let mut out = Vec::new();
    while pos + 12 <= b.len() {
        let size = u32::from_le_bytes([b[pos], b[pos + 1], b[pos + 2], b[pos + 3]]) as usize;
        pos += 12;
        let f = b[pos..pos + size].to_vec();
        out.push((f[0] & 1 == 0, f));
        pos += size;
    }
    out
}

fn encoded(track: &str, codec: VideoCodec, keyframe: bool, data: Vec<u8>, n: u32) -> VideoRecvItem {
    VideoRecvItem::Frame(EncodedVideoFrame {
        track: track.into(),
        codec,
        data,
        keyframe,
        rtp_ts: n * 3000,
        arrival_us: 1_000_000 + u64::from(n) * 33_333,
        width: if keyframe { 320 } else { 0 },
        height: if keyframe { 180 } else { 0 },
    })
}

fn track(member: &str, sid: &str, source: VideoSource, codec: VideoCodec) -> VideoTrack {
    VideoTrack {
        member_id: member.into(),
        is_self: false,
        source,
        track: sid.into(),
        codec,
        width: 320,
        height: 180,
        muted: false,
        paused: None,
        unsupported: false,
    }
}

/// Every event a native consumer got.
#[derive(Default)]
struct Seen(Mutex<Vec<NativeVideoEvent>>);

impl Seen {
    fn all(&self) -> Vec<NativeVideoEvent> {
        self.0.lock().expect("seen").clone()
    }

    fn frames(&self) -> Vec<tauri_plugin_sew_voice::native::NativeVideoFrame> {
        self.all()
            .into_iter()
            .filter_map(|e| match e {
                NativeVideoEvent::Frame(f) => Some(f),
                _ => None,
            })
            .collect()
    }
}

fn consumer(seen: &Arc<Seen>) -> Arc<dyn tauri_plugin_sew_voice::native::NativeVideoConsumer> {
    let seen = Arc::clone(seen);
    Arc::new(move |e: NativeVideoEvent| seen.0.lock().expect("seen").push(e))
}

/// Waits (real time: the sink has its own thread) until `pred` holds; fails loudly after 10 s.
async fn until(what: &str, mut pred: impl FnMut() -> bool) {
    for _ in 0..2000 {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for: {what}");
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

async fn push(call: &FakeCall, item: VideoRecvItem) {
    assert!(call.push_video(item), "the frame channel took the item");
    tokio::task::yield_now().await;
}

struct World {
    svc: VoiceService,
    control: VoiceControl,
    engine: Arc<FakeEngine>,
}

fn world(backend: bool) -> World {
    let engine = Arc::new(FakeEngine::new());
    engine.enable_video();
    if backend {
        engine.set_native_video_backend(FakeBackend::new(FakeConfig::default()));
    }
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
        control: VoiceControl::new(svc.clone()),
        svc,
        engine,
    }
}

async fn in_call(w: &World, tracks: Vec<VideoTrack>) -> Arc<FakeCall> {
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
    let n = tracks.len();
    call.set_video_tracks(tracks);
    until("the video list", || w.svc.snapshot().video.len() == n).await;
    call
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h264_access_units_pass_through_unchanged_and_stay_on_in_the_background() {
    let w = world(false);
    let call = in_call(
        &w,
        vec![
            track(ANN, ANN_CAM, VideoSource::Camera, VideoCodec::H264),
            track(BOB, BOB_SCREEN, VideoSource::Screen, VideoCodec::H264),
        ],
    )
    .await;
    // A webview tile of Ann's camera, and the picture in picture of Bob's screen.
    let ch = Arc::new(RecordingFrameChannel::new());
    w.svc
        .video_sink(
            VideoSinkArgs {
                track: ANN_CAM.into(),
                width: 320,
                height: 180,
                visible: true,
                tile: Tile::Grid,
                decode: vec!["h264".into()],
            },
            ch.clone(),
        )
        .await
        .expect("webview sink");
    let seen = Arc::new(Seen::default());
    let pip = w
        .control
        .open_native_sink(
            NativeSinkTarget::Track(BOB_SCREEN.into()),
            PIP,
            consumer(&seen),
        )
        .expect("native sink");
    assert_eq!(pip.track().as_deref(), Some(BOB_SCREEN));
    assert!(
        call.video_settings()
            .contains(&(BOB_SCREEN.to_owned(), PIP_SETTINGS)),
        "the 360p layer at 30 fps: {:?}",
        call.video_settings()
    );
    assert!(call.keyframe_requests().contains(&BOB_SCREEN.to_owned()));

    let units = h264_units();
    assert!(units[0].0, "the fixture starts at an IDR");
    // A delta first is dropped: the sink waits for a keyframe.
    push(
        &call,
        encoded(BOB_SCREEN, VideoCodec::H264, false, units[1].1.clone(), 0),
    )
    .await;
    for (i, (idr, au)) in units.iter().take(8).enumerate() {
        push(
            &call,
            encoded(BOB_SCREEN, VideoCodec::H264, *idr, au.clone(), i as u32 + 1),
        )
        .await;
    }
    until("eight access units", || seen.frames().len() == 8).await;
    let f = seen.frames();
    for (i, frame) in f.iter().enumerate() {
        let NativePayload::H264 { access_unit, .. } = &frame.payload else {
            panic!("an H.264 frame is not decoded");
        };
        assert_eq!(
            access_unit, &units[i].1,
            "access unit {i} is byte for byte the input"
        );
        assert_eq!(frame.track, BOB_SCREEN);
        assert_eq!(frame.keyframe, units[i].0);
        assert_eq!(frame.discontinuity, i == 0, "only the first after the open");
    }
    let NativePayload::H264 {
        access_unit,
        width,
        height,
    } = &f[0].payload
    else {
        panic!("H.264");
    };
    assert_eq!((*width, *height), (320, 180));
    let ps = h264_parameter_sets(access_unit);
    assert_eq!(
        (ps.sps.len(), ps.pps.len()),
        (1, 1),
        "an IDR carries its SPS and PPS"
    );

    // The background: Ann's tile goes off, the picture in picture stays on and keeps getting
    // frames.
    let before = call.video_settings().len();
    w.control.app_scene(AppScene::Background);
    let after = call.video_settings()[before..].to_vec();
    assert_eq!(after, vec![(ANN_CAM.to_owned(), TrackSettings::DISABLED)]);
    let shown = ch.messages().len();
    for (i, (idr, au)) in units.iter().skip(8).take(4).enumerate() {
        push(
            &call,
            encoded(BOB_SCREEN, VideoCodec::H264, *idr, au.clone(), i as u32 + 9),
        )
        .await;
        push(
            &call,
            encoded(ANN_CAM, VideoCodec::H264, *idr, au.clone(), i as u32 + 9),
        )
        .await;
    }
    until("four more in the background", || seen.frames().len() == 12).await;
    assert_eq!(ch.messages().len(), shown, "the webview tile got nothing");

    // A broken track resets the consumer; the next keyframe comes with discontinuity.
    push(
        &call,
        VideoRecvItem::Broken {
            track: BOB_SCREEN.into(),
        },
    )
    .await;
    push(
        &call,
        encoded(BOB_SCREEN, VideoCodec::H264, false, units[13].1.clone(), 14),
    )
    .await;
    push(
        &call,
        encoded(BOB_SCREEN, VideoCodec::H264, true, units[0].1.clone(), 15),
    )
    .await;
    until("the keyframe after the reset", || seen.frames().len() == 13).await;
    let all = seen.all();
    assert!(
        all.iter()
            .any(|e| matches!(e, NativeVideoEvent::Reset { track } if track == BOB_SCREEN))
    );
    let last = seen.frames().pop().expect("a frame");
    assert!(last.keyframe && last.discontinuity);

    // Closing it turns Bob's screen off at the media server.
    drop(pip);
    assert_eq!(
        call.video_settings().last(),
        Some(&(BOB_SCREEN.to_owned(), TrackSettings::DISABLED))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vp8_is_decoded_through_the_backend_at_most_the_sinks_size() {
    let w = world(true);
    let call = in_call(
        &w,
        vec![track(ANN, ANN_CAM, VideoSource::Camera, VideoCodec::Vp8)],
    )
    .await;
    let seen = Arc::new(Seen::default());
    let pip = w
        .control
        .open_native_sink(
            NativeSinkTarget::Track(ANN_CAM.into()),
            PIP,
            consumer(&seen),
        )
        .expect("native sink");
    let frames = vp8_frames();
    assert!(frames[0].0, "the fixture starts at a keyframe");
    for (i, (key, data)) in frames.iter().take(10).enumerate() {
        push(
            &call,
            encoded(ANN_CAM, VideoCodec::Vp8, *key, data.clone(), i as u32),
        )
        .await;
        settle().await;
    }
    until("ten decoded frames", || seen.frames().len() == 10).await;
    for (i, f) in seen.frames().iter().enumerate() {
        let NativePayload::Decoded(frame) = &f.payload else {
            panic!("VP8 is decoded");
        };
        assert_eq!(
            (frame.width, frame.height),
            (320, 180),
            "frame {i}: the coded size fits"
        );
        assert_eq!(f.discontinuity, i == 0);
        assert_eq!(
            f.timestamp_us,
            1_000_000 + i as u64 * 33_333,
            "in order, with their time"
        );
    }

    // A smaller box: the decoder scales down from the next frame.
    pip.set_max_size(160, 90);
    assert_eq!(
        call.video_settings().last(),
        Some(&(
            ANN_CAM.to_owned(),
            TrackSettings {
                disabled: false,
                width: 160,
                height: 90,
                fps: 30
            }
        ))
    );
    settle().await;
    for (i, (key, data)) in frames.iter().enumerate().skip(10).take(5) {
        push(
            &call,
            encoded(ANN_CAM, VideoCodec::Vp8, *key, data.clone(), i as u32),
        )
        .await;
        settle().await;
    }
    until("five more", || seen.frames().len() == 15).await;
    let NativePayload::Decoded(last) = &seen.frames()[14].payload else {
        panic!("VP8 is decoded");
    };
    assert_eq!((last.width, last.height), (160, 90));

    // A broken frame: the consumer is reset and a keyframe is asked.
    let asked = call.keyframe_requests().len();
    push(
        &call,
        encoded(ANN_CAM, VideoCodec::Vp8, false, vec![0xff; 64], 99),
    )
    .await;
    until("the keyframe request", || {
        call.keyframe_requests().len() > asked
    })
    .await;
    assert!(
        seen.all()
            .iter()
            .any(|e| matches!(e, NativeVideoEvent::Reset { track } if track == ANN_CAM))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_active_speaker_sink_follows_the_speaker_and_asks_a_keyframe_on_each_switch() {
    let w = world(false);
    let call = in_call(
        &w,
        vec![
            track(ANN, ANN_CAM, VideoSource::Camera, VideoCodec::H264),
            track(BOB, BOB_SCREEN, VideoSource::Screen, VideoCodec::H264),
            track(BOB, BOB_CAM, VideoSource::Camera, VideoCodec::H264),
        ],
    )
    .await;
    let seen = Arc::new(Seen::default());
    let pip = w
        .control
        .open_native_sink(NativeSinkTarget::ActiveSpeaker, PIP, consumer(&seen))
        .expect("native sink");
    // Before anyone speaks: the first remote camera.
    assert_eq!(pip.track().as_deref(), Some(ANN_CAM));
    until("the first switch event", || !seen.all().is_empty()).await;
    assert!(matches!(
        &seen.all()[0],
        NativeVideoEvent::Switched { track: Some(t), member_id: Some(m) } if t == ANN_CAM && m == ANN
    ));
    assert_eq!(
        call.video_settings(),
        vec![(ANN_CAM.to_owned(), PIP_SETTINGS)]
    );

    // Bob speaks: his camera (not his screen), Ann's goes off, a keyframe is asked of Bob's.
    let asked = call.keyframe_requests().len();
    call.emit(CallEvent::Speaking(vec![
        Speaking {
            member_id: ME.into(),
            level: 0.9,
        },
        Speaking {
            member_id: BOB.into(),
            level: 0.5,
        },
        Speaking {
            member_id: ANN.into(),
            level: 0.05,
        },
    ]));
    until("the switch to Bob", || {
        pip.track().as_deref() == Some(BOB_CAM)
    })
    .await;
    let s = call.video_settings();
    assert!(
        s.contains(&(ANN_CAM.to_owned(), TrackSettings::DISABLED)),
        "{s:?}"
    );
    assert!(s.contains(&(BOB_CAM.to_owned(), PIP_SETTINGS)), "{s:?}");
    assert_eq!(call.keyframe_requests()[asked..], [BOB_CAM.to_owned()]);
    until("the switch event", || {
        seen.all().iter().any(|e| {
            matches!(e, NativeVideoEvent::Switched { track: Some(t), member_id: Some(m) } if t == BOB_CAM && m == BOB)
        })
    })
    .await;

    // Frames of the old track no longer reach it; the new one starts at a keyframe.
    let units = h264_units();
    push(
        &call,
        encoded(ANN_CAM, VideoCodec::H264, true, units[0].1.clone(), 1),
    )
    .await;
    push(
        &call,
        encoded(BOB_CAM, VideoCodec::H264, false, units[1].1.clone(), 2),
    )
    .await;
    push(
        &call,
        encoded(BOB_CAM, VideoCodec::H264, true, units[0].1.clone(), 3),
    )
    .await;
    until("Bob's keyframe", || seen.frames().len() == 1).await;
    settle().await;
    let f = seen.frames();
    assert_eq!(f.len(), 1, "nothing of Ann's, no delta before the keyframe");
    assert_eq!(
        (f[0].track.as_str(), f[0].keyframe, f[0].discontinuity),
        (BOB_CAM, true, true)
    );

    // Ann answers at once: Bob holds the stage for a moment.
    call.emit(CallEvent::Speaking(vec![Speaking {
        member_id: ANN.into(),
        level: 0.8,
    }]));
    settle().await;
    assert_eq!(pip.track().as_deref(), Some(BOB_CAM), "the speaker holds");
    tokio::time::sleep(Duration::from_millis(1600)).await;
    call.emit(CallEvent::Speaking(vec![Speaking {
        member_id: ANN.into(),
        level: 0.8,
    }]));
    until("the switch back to Ann", || {
        pip.track().as_deref() == Some(ANN_CAM)
    })
    .await;
    assert_eq!(
        call.keyframe_requests().last().map(String::as_str),
        Some(ANN_CAM)
    );

    // Ann's camera goes: the sink moves to another camera. Then nobody has video.
    call.set_video_tracks(vec![track(
        BOB,
        BOB_SCREEN,
        VideoSource::Screen,
        VideoCodec::H264,
    )]);
    until("the switch to Bob's screen", || {
        pip.track().as_deref() == Some(BOB_SCREEN)
    })
    .await;
    call.set_video_tracks(Vec::new());
    until("nothing to show", || pip.track().is_none()).await;
    until("the empty switch event", || {
        matches!(
            seen.all().last(),
            Some(NativeVideoEvent::Switched {
                track: None,
                member_id: None
            })
        )
    })
    .await;
    assert_eq!(
        call.video_settings().last(),
        Some(&(BOB_SCREEN.to_owned(), TrackSettings::DISABLED))
    );
}

#[tokio::test]
async fn native_sinks_are_refused_without_a_call_a_track_or_a_decoder() {
    let w = world(false);
    let seen = Arc::new(Seen::default());
    let e = w
        .control
        .open_native_sink(NativeSinkTarget::ActiveSpeaker, PIP, consumer(&seen))
        .expect_err("no call");
    assert_eq!(e.code, "not_in_call");
    let _call = in_call(
        &w,
        vec![track(ANN, ANN_CAM, VideoSource::Camera, VideoCodec::Vp8)],
    )
    .await;
    let e = w
        .control
        .open_native_sink(
            NativeSinkTarget::Track("TR_nope".into()),
            PIP,
            consumer(&seen),
        )
        .expect_err("unknown track");
    assert_eq!(e.code, "unknown_track");
    let e = w
        .control
        .open_native_sink(
            NativeSinkTarget::Track(ANN_CAM.into()),
            PIP,
            consumer(&seen),
        )
        .expect_err("VP8 without a backend");
    assert_eq!(e.code, "decoded_unsupported");
    // An active-speaker sink opens (it shows H.264 speakers only).
    let pip = w
        .control
        .open_native_sink(NativeSinkTarget::ActiveSpeaker, PIP, consumer(&seen))
        .expect("active speaker");
    assert_eq!(pip.track().as_deref(), Some(ANN_CAM));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_active_speaker_sink_never_shows_this_devices_own_video() {
    const MINE: &str = "TR_VCmycamera";
    const MY_OTHER_DEVICE: &str = "TR_VCmyothercamera";
    let w = world(false);
    // An engine that lists this device's camera (`is_self`) and our member's other device:
    // neither is ever the picture in picture.
    let mut mine = track(ME, MINE, VideoSource::Camera, VideoCodec::H264);
    mine.is_self = true;
    let other = track(ME, MY_OTHER_DEVICE, VideoSource::Camera, VideoCodec::H264);
    let call = in_call(
        &w,
        vec![
            mine.clone(),
            other.clone(),
            track(ANN, ANN_CAM, VideoSource::Camera, VideoCodec::H264),
        ],
    )
    .await;
    let seen = Arc::new(Seen::default());
    let pip = w
        .control
        .open_native_sink(NativeSinkTarget::ActiveSpeaker, PIP, consumer(&seen))
        .expect("native sink");
    assert_eq!(
        pip.track().as_deref(),
        Some(ANN_CAM),
        "the first remote camera"
    );

    // We speak loudest: still Ann.
    call.emit(CallEvent::Speaking(vec![Speaking {
        member_id: ME.into(),
        level: 0.9,
    }]));
    settle().await;
    assert_eq!(pip.track().as_deref(), Some(ANN_CAM));

    // Ann's camera goes: nothing, not ours.
    call.set_video_tracks(vec![mine, other]);
    until("the shorter list", || w.svc.snapshot().video.len() == 2).await;
    until("the sink lets Ann go", || pip.track().is_none()).await;
    // Our self-view frames never reach it.
    let before = seen.frames().len();
    for (n, t) in ["local:camera", MINE, MY_OTHER_DEVICE]
        .into_iter()
        .enumerate()
    {
        let n = u32::try_from(n).expect("small");
        let au = vec![0, 0, 0, 1, 0x65, 0x88];
        push(&call, encoded(t, VideoCodec::H264, true, au, n)).await;
    }
    settle().await;
    assert_eq!(seen.frames().len(), before, "no frame of ours");
    let s = call.video_settings();
    assert!(
        !s.iter()
            .any(|(t, st)| (t == MINE || t == MY_OTHER_DEVICE) && !st.disabled),
        "our own tracks are never enabled for it: {s:?}"
    );

    // Asked for by name, self-view is refused too.
    for t in ["local:camera", "local:screen", MINE] {
        let e = w
            .control
            .open_native_sink(NativeSinkTarget::Track(t.into()), PIP, consumer(&seen))
            .expect_err("self-view is never a native sink");
        assert_eq!(e.code, "unknown_track", "{t}");
        assert_eq!(e.detail["self_view"], serde_json::json!(true), "{t}");
    }
}

/// The real VP8 decoder (kessel-video's pure-Rust one, through the fake backend) at 640x360:
/// a 30 fps stream paced in real time comes out whole, at 30 fps, with no drop and no keyframe
/// asked; then, as fast as the sink takes it (at most 3 frames in flight, so the decoder is
/// never handed more than it has room for), the stream comes out at over 30 fps. Prints the
/// sink's decode time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::print_stderr)]
async fn vp8_at_640x360_keeps_up_with_30_fps_through_the_real_decoder() {
    const PACED: usize = 150;
    const BURST: usize = 150;
    let w = world(true);
    let mut ann = track(ANN, ANN_CAM, VideoSource::Camera, VideoCodec::Vp8);
    (ann.width, ann.height) = (640, 360);
    let call = in_call(&w, vec![ann]).await;
    // When each decoded frame reached the consumer, and its size.
    type Out = Mutex<Vec<(Instant, (u32, u32))>>;
    let out: Arc<Out> = Arc::default();
    let consumer: Arc<dyn tauri_plugin_sew_voice::native::NativeVideoConsumer> = {
        let out = Arc::clone(&out);
        Arc::new(move |e: NativeVideoEvent| {
            if let NativeVideoEvent::Frame(f) = e
                && let NativePayload::Decoded(frame) = &f.payload
            {
                out.lock()
                    .expect("out")
                    .push((Instant::now(), (frame.width, frame.height)));
            }
        })
    };
    let pip = w
        .control
        .open_native_sink(NativeSinkTarget::Track(ANN_CAM.into()), PIP, consumer)
        .expect("native sink");
    let frames = vp8_frames_of("vp8_sim_h.ivf");
    assert!(frames.len() >= PACED + BURST, "{} frames", frames.len());
    assert!(frames[0].0, "the fixture starts at a keyframe");
    let item = |i: usize| {
        let (key, data) = &frames[i];
        let n = i as u32;
        VideoRecvItem::Frame(EncodedVideoFrame {
            track: ANN_CAM.into(),
            codec: VideoCodec::Vp8,
            data: data.clone(),
            keyframe: *key,
            rtp_ts: n * 3000,
            arrival_us: 1_000_000 + u64::from(n) * 33_333,
            width: if *key { 640 } else { 0 },
            height: if *key { 360 } else { 0 },
        })
    };
    let asked = call.keyframe_requests().len();

    // Paced: one frame every 33.3 ms.
    let start = tokio::time::Instant::now();
    for i in 0..PACED {
        tokio::time::sleep_until(start + Duration::from_micros(i as u64 * 33_333)).await;
        push(&call, item(i)).await;
    }
    until("every paced frame decoded", || {
        out.lock().expect("out").len() == PACED
    })
    .await;
    let paced = out.lock().expect("out").clone();
    assert!(
        paced.iter().all(|(_, size)| *size == (640, 360)),
        "640x360 out"
    );
    let span = paced[PACED - 1].0 - paced[0].0;
    let paced_fps = (PACED - 1) as f64 / span.as_secs_f64();
    let st = pip.stats();
    eprintln!(
        "native sink, VP8 640x360 paced at 30 fps: {paced_fps:.2} fps out; decode {} us avg, {} \
         us last; consumer {} us avg; {st:?}",
        st.decode_us_avg, st.decode_us, st.consumer_us_avg
    );
    assert_eq!(st.frames_out, PACED as u64, "{st:?}");
    assert_eq!(
        (
            st.dropped_queue_full,
            st.dropped_busy,
            st.dropped_decoder_output,
            st.skipped_to_keyframe,
            st.decode_errors,
        ),
        (0, 0, 0, 0, 0),
        "no frame lost: {st:?}"
    );
    assert!(
        paced_fps >= 29.0,
        "the paced stream comes out at its own 30 fps: {paced_fps:.2}"
    );
    assert!(
        Duration::from_micros(u64::from(st.decode_us_avg)) < Duration::from_micros(33_333),
        "a decode takes less than a frame interval: {st:?}"
    );

    // As fast as it goes.
    let burst_start = Instant::now();
    for i in PACED..PACED + BURST {
        // Every frame pushed so far reached the sink, and fewer than 3 are in its decoder.
        until("room in the decoder", || {
            let s = pip.stats();
            s.frames_in == i as u64 && s.queue_depth == 0 && s.decoder_in_flight < 3
        })
        .await;
        push(&call, item(i)).await;
    }
    until("every burst frame decoded", || {
        out.lock().expect("out").len() == PACED + BURST
    })
    .await;
    let burst_fps = BURST as f64 / burst_start.elapsed().as_secs_f64();
    let st = pip.stats();
    eprintln!(
        "native sink, VP8 640x360 as fast as it goes: {burst_fps:.1} fps; decode {} us avg, {} \
         us last; {st:?}",
        st.decode_us_avg, st.decode_us
    );
    assert_eq!(
        (
            st.dropped_queue_full,
            st.dropped_busy,
            st.dropped_decoder_output
        ),
        (0, 0, 0),
        "{st:?}"
    );
    assert!(
        burst_fps >= 30.0,
        "the sink delivers at least 30 fps: {burst_fps:.1}"
    );
    assert_eq!(
        call.keyframe_requests().len(),
        asked,
        "no keyframe asked while it kept up"
    );
    assert!(!st.low_layer && st.step_downs == 0, "{st:?}");
}
