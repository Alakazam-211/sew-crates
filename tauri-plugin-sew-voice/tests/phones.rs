//! Phones, against the fake engine: the app's scene and the video backend's background
//! signal (either one pauses the sinks, both must say foreground to resume), the listener's
//! `video_changed`, the camera permission and the device orientation.

// The helpers (not only #[test] fns) unwrap: a test that cannot set up must fail loudly.
#![allow(clippy::expect_used)]

use std::sync::Arc;

use serde_json::json;
use tauri_plugin_sew_voice::engine::EngineError;
use tauri_plugin_sew_voice::fake::{
    FakeCall, FakeEngine, FakeProvider, RecordingFrameChannel, RecordingListener, RecordingSink,
};
use tauri_plugin_sew_voice::video::{flag, kind};
use tauri_plugin_sew_voice::{
    AppScene, CameraArgs, CameraRequest, DeviceChoice, EncodedVideoFrame, JoinArgs, JoinOptions,
    LocalVideo, LocalVideoState, PermissionState, PreviewArgs, Spawn, Tile, TrackSettings,
    VideoCodec, VideoRecvItem, VideoSinkArgs, VideoSource, VideoTrack, VoiceControl, VoiceService,
};

const ME: &str = "48831765147549699";
const PEER: &str = "48831765147549700";
const CAM: &str = "TR_VCpeercamera";

struct World {
    svc: VoiceService,
    control: VoiceControl,
    engine: Arc<FakeEngine>,
    listener: Arc<RecordingListener>,
}

fn world() -> World {
    let engine = Arc::new(FakeEngine::new());
    engine.enable_video();
    let listener = Arc::new(RecordingListener::new());
    let spawn: Spawn = Arc::new(|f| {
        tokio::spawn(f);
    });
    let svc = VoiceService::with_listener(
        engine.clone(),
        Arc::new(FakeProvider::answering(ME)),
        Arc::new(RecordingSink::new()),
        listener.clone(),
        spawn,
    );
    svc.start();
    World {
        control: VoiceControl::new(svc.clone()),
        svc,
        engine,
        listener,
    }
}

fn args() -> JoinArgs {
    JoinArgs {
        target: "k2:acme".into(),
        room: "48831765147549698".into(),
        muted: None,
        deafened: None,
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

fn camera(muted: bool) -> VideoTrack {
    VideoTrack {
        member_id: PEER.into(),
        is_self: false,
        source: VideoSource::Camera,
        track: CAM.into(),
        codec: VideoCodec::Vp8,
        width: 1280,
        height: 720,
        muted,
        paused: None,
        unsupported: false,
    }
}

fn frame(keyframe: bool, n: u8) -> VideoRecvItem {
    VideoRecvItem::Frame(EncodedVideoFrame {
        track: CAM.into(),
        codec: VideoCodec::Vp8,
        data: vec![n; 40],
        keyframe,
        rtp_ts: u32::from(n) * 3000,
        arrival_us: u64::from(n),
        width: 0,
        height: 0,
    })
}

async fn push(call: &FakeCall, item: VideoRecvItem) {
    assert!(call.push_video(item), "the frame channel took the item");
    settle().await;
}

/// A call with the peer's camera and one visible webview sink on it, past its first keyframe.
async fn watching() -> (World, Arc<FakeCall>, Arc<RecordingFrameChannel>) {
    let w = world();
    w.svc.join(args()).await.expect("join");
    let call = w.engine.last_call().expect("call");
    call.set_video_tracks(vec![camera(false)]);
    until("the video list", || w.svc.snapshot().video.len() == 1).await;
    let ch = Arc::new(RecordingFrameChannel::new());
    w.svc
        .video_sink(
            VideoSinkArgs {
                track: CAM.into(),
                width: 640,
                height: 360,
                visible: true,
                tile: Tile::Grid,
                decode: vec!["vp8".into()],
            },
            ch.clone(),
        )
        .await
        .expect("sink");
    push(&call, frame(true, 1)).await;
    until("the first frame", || ch.messages().len() == 1).await;
    (w, call, ch)
}

const GRID_360: TrackSettings = TrackSettings {
    disabled: false,
    width: 640,
    height: 360,
    fps: 15,
};

#[tokio::test]
async fn the_background_hides_webview_sinks_and_the_foreground_restarts_them_at_a_keyframe() {
    let (w, call, ch) = watching().await;
    assert_eq!(call.video_settings(), vec![(CAM.to_owned(), GRID_360)]);
    assert_eq!(call.keyframe_requests(), vec![CAM.to_owned()]);

    w.control.app_scene(AppScene::Background);
    assert!(w.svc.is_background());
    assert_eq!(
        w.engine.app_scenes(),
        vec![AppScene::Background],
        "the engine hears it"
    );
    assert_eq!(
        call.video_settings().last(),
        Some(&(CAM.to_owned(), TrackSettings::DISABLED)),
        "the track is turned off at the media server"
    );
    assert_eq!(
        call.app_scenes(),
        vec![AppScene::Background],
        "the call's hook"
    );
    push(&call, frame(true, 2)).await;
    push(&call, frame(false, 3)).await;
    assert_eq!(
        ch.messages().len(),
        1,
        "nothing reaches a webview sink in the background"
    );
    // A repeat changes nothing.
    w.control.app_scene(AppScene::Background);
    assert_eq!(call.video_settings().len(), 2);

    w.control.app_scene(AppScene::Foreground);
    assert!(!w.svc.is_background());
    assert_eq!(
        call.video_settings().last(),
        Some(&(CAM.to_owned(), GRID_360))
    );
    assert_eq!(
        call.keyframe_requests(),
        vec![CAM.to_owned(), CAM.to_owned()],
        "a keyframe is asked on return"
    );
    assert_eq!(
        call.app_scenes(),
        vec![AppScene::Background, AppScene::Foreground]
    );
    push(&call, frame(false, 4)).await;
    assert_eq!(
        ch.messages().len(),
        1,
        "a delta before the keyframe is dropped"
    );
    push(&call, frame(true, 5)).await;
    until("the keyframe", || ch.messages().len() == 2).await;
    let f = ch.frames();
    assert_eq!(f[1].0.kind, kind::ENCODED);
    assert_eq!(f[1].0.flags, flag::KEYFRAME | flag::DISCONTINUITY);
    assert_eq!(f[1].1, vec![5u8; 40]);
}

#[tokio::test]
async fn either_signal_pauses_and_both_must_say_foreground_to_resume() {
    let (w, call, _ch) = watching().await;
    // The backend's subscription starts with the first sink.
    until("the backend subscription", || {
        w.engine.background_subscribers() == 1
    })
    .await;

    // The backend alone pauses.
    w.engine.backend_background(true);
    until("background", || w.svc.is_background()).await;
    assert_eq!(
        call.video_settings().last(),
        Some(&(CAM.to_owned(), TrackSettings::DISABLED))
    );
    // The app says background too, then foreground: the backend still says background.
    w.control.app_scene(AppScene::Background);
    w.control.app_scene(AppScene::Foreground);
    settle().await;
    assert!(w.svc.is_background(), "the backend still says background");
    assert_eq!(
        call.video_settings().len(),
        2,
        "no change while one says background"
    );
    // The backend comes back: both say foreground.
    w.engine.backend_background(false);
    until("foreground", || !w.svc.is_background()).await;
    assert_eq!(
        call.video_settings().last(),
        Some(&(CAM.to_owned(), GRID_360))
    );

    // The app alone pauses; the backend's foreground doesn't resume it.
    w.control.app_scene(AppScene::Background);
    w.engine.backend_background(false);
    settle().await;
    assert!(w.svc.is_background());
    w.control.app_scene(AppScene::Foreground);
    assert!(!w.svc.is_background());
    assert_eq!(
        call.app_scenes(),
        vec![
            AppScene::Background,
            AppScene::Foreground,
            AppScene::Background,
            AppScene::Foreground
        ],
        "the call hears only the combined changes"
    );
}

#[tokio::test]
async fn a_sink_revealed_in_the_background_waits_for_the_return() {
    let (w, call, ch) = watching().await;
    w.control.app_scene(AppScene::Background);
    let asked = call.keyframe_requests().len();
    // A second sink opens while backgrounded: no settings, no keyframe yet.
    let ch2 = Arc::new(RecordingFrameChannel::new());
    w.svc
        .video_sink(
            VideoSinkArgs {
                track: CAM.into(),
                width: 1280,
                height: 720,
                visible: true,
                tile: Tile::Large,
                decode: vec!["vp8".into()],
            },
            ch2.clone(),
        )
        .await
        .expect("sink");
    assert_eq!(
        call.video_settings().last(),
        Some(&(CAM.to_owned(), TrackSettings::DISABLED)),
        "still off in the background"
    );
    push(&call, frame(true, 2)).await;
    assert!(ch2.messages().is_empty() && ch.messages().len() == 1);
    w.control.app_scene(AppScene::Foreground);
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
    assert_eq!(
        call.keyframe_requests().len(),
        asked + 1,
        "one keyframe for the track"
    );
}

#[tokio::test]
async fn video_changed_follows_local_and_remote_video() {
    let w = world();
    w.svc.join(args()).await.expect("join");
    let call = w.engine.last_call().expect("call");
    until("connected", || w.svc.snapshot().state.is_live()).await;
    w.listener.clear();
    assert!(!w.control.has_video());

    // A muted camera is no video; unmuted it is.
    call.set_video_tracks(vec![camera(true)]);
    until("the video list", || w.svc.snapshot().video.len() == 1).await;
    assert!(w.listener.calls().is_empty(), "a muted camera is not video");
    call.set_video_tracks(vec![camera(false)]);
    until("video_changed(true)", || {
        w.listener.calls() == vec![json!({"video_changed": true})]
    })
    .await;
    assert!(w.control.has_video());
    assert_eq!(
        serde_json::to_value(w.control.snapshot()).expect("json")["has_video"],
        json!(true)
    );

    // Our own camera keeps it on after the remote one goes.
    call.set_local_video(LocalVideo {
        camera: LocalVideoState::On,
        ..LocalVideo::default()
    });
    call.set_video_tracks(Vec::new());
    until("the empty list", || w.svc.snapshot().video.is_empty()).await;
    settle().await;
    assert_eq!(w.listener.calls().len(), 1, "still video: our camera");
    // Paused (background) is still video; off is not.
    call.set_local_video(LocalVideo {
        camera: LocalVideoState::Paused,
        reason: Some("background".into()),
        ..LocalVideo::default()
    });
    settle().await;
    assert_eq!(w.listener.calls().len(), 1);
    call.set_local_video(LocalVideo::default());
    until("video_changed(false)", || w.listener.calls().len() == 2).await;
    assert_eq!(w.listener.calls()[1], json!({"video_changed": false}));
    assert!(!w.control.has_video());
    let snap = serde_json::to_value(w.control.snapshot()).expect("json");
    assert!(
        snap.get("has_video").is_none(),
        "absent while false: voice payloads unchanged"
    );

    // The call ends with video: false.
    call.set_video_tracks(vec![camera(false)]);
    until("video again", || w.control.has_video()).await;
    w.control.leave().await.expect("leave");
    assert!(!w.control.has_video());
    let calls = w.listener.calls();
    let changes: Vec<_> = calls
        .iter()
        .filter(|c| c.get("video_changed").is_some())
        .collect();
    assert_eq!(
        changes,
        vec![
            &json!({"video_changed": true}),
            &json!({"video_changed": false}),
            &json!({"video_changed": true}),
            &json!({"video_changed": false})
        ]
    );
}

#[tokio::test]
async fn camera_permission_flows() {
    let w = world();
    assert_eq!(
        w.control.camera_permission().await,
        PermissionState::Unsupported,
        "an engine without a camera"
    );
    w.engine
        .set_camera_permission(PermissionState::Prompt, PermissionState::Granted);
    assert_eq!(w.control.camera_permission().await, PermissionState::Prompt);
    assert_eq!(w.engine.permission_requests(), 0, "reading never prompts");
    assert_eq!(
        w.control.request_camera_permission().await,
        PermissionState::Granted
    );
    assert_eq!(
        w.svc.request_camera_permission().await.expect("request"),
        PermissionState::Granted,
        "asked again: the state at once"
    );
    assert_eq!(w.engine.permission_requests(), 2);
    w.engine
        .set_camera_permission(PermissionState::Prompt, PermissionState::Denied);
    assert_eq!(
        w.svc.request_camera_permission().await.expect("request"),
        PermissionState::Denied
    );
    assert_eq!(
        w.svc.camera_permission().await.expect("permission"),
        PermissionState::Denied
    );
}

#[tokio::test]
async fn camera_on_never_prompts_for_the_permission() {
    let w = world();
    w.svc.join(args()).await.expect("join");
    let call = w.engine.last_call().expect("call");
    until("connected", || w.svc.snapshot().state.is_live()).await;

    // Never asked: refused, no prompt, nothing opened.
    w.engine
        .set_camera_permission(PermissionState::Prompt, PermissionState::Granted);
    let e = w
        .svc
        .camera(CameraArgs {
            on: true,
            device: None,
        })
        .await
        .expect_err("not determined");
    assert_eq!(e.code, "camera_permission_not_determined");
    let e = w
        .control
        .set_camera(true, None)
        .await
        .expect_err("not determined");
    assert_eq!(e.code, "camera_permission_not_determined");
    let e = w
        .svc
        .preview(PreviewArgs {
            on: true,
            device: None,
        })
        .await
        .expect_err("the preview neither");
    assert_eq!(e.code, "camera_permission_not_determined");
    assert_eq!(w.engine.permission_requests(), 0, "no prompt");
    assert!(call.camera_requests().is_empty(), "no camera opened");

    // Denied: refused the same way.
    w.engine
        .set_camera_permission(PermissionState::Denied, PermissionState::Denied);
    let e = w.control.set_camera(true, None).await.expect_err("denied");
    assert_eq!(e.code, "camera_permission_denied");
    assert_eq!(w.engine.permission_requests(), 0);

    // The app's camera button asks first; then the camera goes on.
    w.engine
        .set_camera_permission(PermissionState::Prompt, PermissionState::Granted);
    assert_eq!(
        w.control.request_camera_permission().await,
        PermissionState::Granted
    );
    assert_eq!(w.engine.permission_requests(), 1, "only on the app's call");
    w.control
        .set_camera(true, Some("fake:camera:back"))
        .await
        .expect("camera on");
    assert_eq!(
        call.camera_requests(),
        vec![CameraRequest {
            on: true,
            device: Some(DeviceChoice::Id("fake:camera:back".into())),
        }]
    );
    until("video_changed(true)", || w.control.has_video()).await;
    // Off needs no permission.
    w.engine
        .set_camera_permission(PermissionState::Denied, PermissionState::Denied);
    w.control.set_camera(false, None).await.expect("camera off");
    assert_eq!(call.camera_requests().len(), 2);
    until("video_changed(false)", || !w.control.has_video()).await;
}

#[tokio::test]
async fn a_join_with_the_camera_publishes_once_connected() {
    let w = world();
    w.engine.set_auto_connect(false);
    w.control
        .join_with(args(), JoinOptions::with_camera().camera_device("default"))
        .await
        .expect("join");
    let call = w.engine.last_call().expect("call");
    settle().await;
    assert!(
        call.camera_requests().is_empty(),
        "not before the call is connected"
    );
    call.connect();
    until("the camera request", || call.camera_requests().len() == 1).await;
    assert_eq!(
        call.camera_requests(),
        vec![CameraRequest {
            on: true,
            device: Some(DeviceChoice::Default),
        }]
    );
    until("local_video on", || {
        w.svc.snapshot().local_video.camera == LocalVideoState::On
    })
    .await;
    assert!(w.listener.calls().contains(&json!({"video_changed": true})));
    // A reconnect doesn't start it again.
    call.reconnecting(1);
    call.connect();
    settle().await;
    assert_eq!(call.camera_requests().len(), 1);
}

#[tokio::test]
async fn a_join_with_the_camera_answered_in_the_background_starts_it_in_front() {
    let w = world();
    // Answered from the lock screen.
    w.control.app_scene(AppScene::Background);
    w.control
        .join_with(args(), JoinOptions::with_camera())
        .await
        .expect("join");
    let call = w.engine.last_call().expect("call");
    until("connected", || w.svc.snapshot().state.is_live()).await;
    settle().await;
    assert!(
        call.camera_requests().is_empty(),
        "the camera never starts from the lock screen"
    );
    w.control.app_scene(AppScene::Foreground);
    until("the camera request", || call.camera_requests().len() == 1).await;
    assert_eq!(
        call.camera_requests(),
        vec![CameraRequest {
            on: true,
            device: None,
        }]
    );
    // Once only.
    w.control.app_scene(AppScene::Background);
    w.control.app_scene(AppScene::Foreground);
    settle().await;
    assert_eq!(call.camera_requests().len(), 1);
}

#[tokio::test]
async fn a_join_cameras_refusal_is_a_local_video_error_and_the_call_goes_on() {
    // The room's slots are full.
    let w = world();
    w.engine.set_auto_connect(false);
    w.control
        .join_with(args(), JoinOptions::with_camera())
        .await
        .expect("join");
    let call = w.engine.last_call().expect("call");
    call.fail_next_camera(EngineError::failed("video_slots_full"));
    call.connect();
    until("the refusal", || {
        w.svc.snapshot().local_video.reason.as_deref() == Some("video_slots_full")
    })
    .await;
    let snap = w.svc.snapshot();
    assert_eq!(snap.local_video.camera, LocalVideoState::Off);
    assert_eq!(
        snap.state,
        tauri_plugin_sew_voice::CallState::Connected,
        "the call goes on"
    );
    assert!(!w.control.has_video());

    // No permission: refused without a prompt, the call goes on.
    let w = world();
    w.engine
        .set_camera_permission(PermissionState::Prompt, PermissionState::Granted);
    w.control
        .join_with(args(), JoinOptions::with_camera())
        .await
        .expect("join");
    let call = w.engine.last_call().expect("call");
    until("the refusal", || {
        w.svc.snapshot().local_video.reason.as_deref() == Some("camera_permission_not_determined")
    })
    .await;
    assert!(call.camera_requests().is_empty());
    assert_eq!(w.engine.permission_requests(), 0);
    assert!(w.svc.snapshot().state.is_live());

    // A camera off before the call is up drops the join's camera.
    let w = world();
    w.engine.set_auto_connect(false);
    w.control
        .join_with(args(), JoinOptions::with_camera())
        .await
        .expect("join");
    let call = w.engine.last_call().expect("call");
    w.control.set_camera(false, None).await.expect("off");
    call.connect();
    until("connected", || w.svc.snapshot().state.is_live()).await;
    settle().await;
    assert_eq!(
        call.camera_requests(),
        vec![CameraRequest {
            on: false,
            device: None,
        }],
        "only the off"
    );
}

#[tokio::test]
async fn orientation_and_scene_reach_the_engine_without_a_call() {
    let w = world();
    w.control.device_orientation(90);
    w.control.device_orientation(270);
    assert_eq!(w.engine.device_orientations(), vec![90, 270]);
    w.control.app_scene(AppScene::Background);
    w.control.app_scene(AppScene::Foreground);
    assert_eq!(
        w.engine.app_scenes(),
        vec![AppScene::Background, AppScene::Foreground]
    );
    assert!(!w.svc.is_background());
    // Audio interruptions are not scenes.
    w.control.suspend();
    w.control.resume();
    assert!(!w.svc.is_background());
    assert_eq!(w.engine.app_scenes().len(), 2);
}

fn screen_changes(w: &World) -> Vec<serde_json::Value> {
    w.listener
        .calls()
        .into_iter()
        .filter(|c| c.get("screen_share_changed").is_some())
        .collect()
}

#[tokio::test]
async fn screen_share_changed_follows_the_local_screen() {
    let w = world();
    // No call: the ring can't be armed.
    let e = w
        .control
        .screen_share_ring("/nonexistent/group")
        .await
        .expect_err("not in a call");
    assert_eq!(e.code, "not_in_call");
    w.svc.join(args()).await.expect("join");
    let call = w.engine.last_call().expect("call");
    until("connected", || w.svc.snapshot().state.is_live()).await;
    // An engine without the ring (the fake, or a build without it): screen_unsupported.
    let e = w
        .control
        .screen_share_ring("/nonexistent/group")
        .await
        .expect_err("no ring here");
    assert_eq!(e.code, "screen_unsupported");
    w.listener.clear();
    // The camera is not a screen share.
    call.set_local_video(LocalVideo {
        camera: LocalVideoState::On,
        ..LocalVideo::default()
    });
    settle().await;
    assert!(screen_changes(&w).is_empty());
    // Asking is not sharing yet; on is; paused still is; off is not.
    for (state, n) in [
        (LocalVideoState::Asking, 0),
        (LocalVideoState::On, 1),
        (LocalVideoState::Paused, 1),
        (LocalVideoState::Off, 2),
        (LocalVideoState::On, 3),
    ] {
        call.set_local_video(LocalVideo {
            camera: LocalVideoState::On,
            screen: state,
            reason: None,
        });
        settle().await;
        assert_eq!(screen_changes(&w).len(), n, "after {state:?}");
    }
    assert_eq!(
        screen_changes(&w),
        vec![
            json!({"screen_share_changed": true}),
            json!({"screen_share_changed": false}),
            json!({"screen_share_changed": true}),
        ]
    );
    // The call ends while sharing: false.
    w.control.leave().await.expect("leave");
    until("the share's end", || screen_changes(&w).len() == 4).await;
    assert_eq!(
        screen_changes(&w)[3],
        json!({"screen_share_changed": false})
    );
}
