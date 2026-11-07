//! The real engine's capture side through the service, on kessel-video's fake backend: the
//! capabilities, the cameras and the permission words, the screen list, and the pre-join
//! preview into a self-view sink (no call, no lease).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tauri_plugin_sew_voice::fake::{FakeProvider, RecordingFrameChannel, RecordingSink};
use tauri_plugin_sew_voice::real::{AudioConfig, CallClientEngine};
use tauri_plugin_sew_voice::sew_call_client::kessel_video::fake::{
    FakeBackend, FakeConfig, FakePermission,
};
use tauri_plugin_sew_voice::sew_call_client::kessel_video::{self, VideoBackend};
use tauri_plugin_sew_voice::sew_call_client::{self, kessel_audio};
use tauri_plugin_sew_voice::video::{LOCAL_CAMERA, flag, kind};
use tauri_plugin_sew_voice::{
    PreviewArgs, ScreenArgs, SetVideoDeviceArgs, Spawn, Tile, VideoSinkArgs, VoiceService,
};

const ME: &str = "48831765147549699";

fn service(backend: Arc<dyn VideoBackend>) -> VoiceService {
    let dir = std::env::temp_dir().join("sew-voice-video-capture");
    let client =
        sew_call_client::CallClient::with_mls(sew_call_client::ClientConfig::new("sew.test", dir));
    let audio: AudioConfig = Arc::new(|_, _| kessel_audio::EngineConfig::default());
    let engine = CallClientEngine::new(client, audio).with_video_backend(backend);
    let spawn: Spawn = Arc::new(|f| {
        tokio::spawn(f);
    });
    let svc = VoiceService::new(
        Arc::new(engine),
        Arc::new(FakeProvider::answering(ME)),
        Arc::new(RecordingSink::new()),
        spawn,
    );
    svc.start();
    svc
}

fn fake(f: impl FnOnce(&mut FakeConfig)) -> Arc<dyn VideoBackend> {
    let mut c = FakeConfig::default();
    f(&mut c);
    FakeBackend::new(c)
}

async fn until(what: &str, mut pred: impl FnMut() -> bool) {
    let t = std::time::Instant::now();
    while !pred() {
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caps_devices_and_screens_come_from_the_backend() {
    let svc = service(fake(|c| {
        c.camera_permission = FakePermission::Prompt {
            answer: kessel_video::Permission::Granted,
            after: Duration::from_millis(1),
        };
    }));
    let caps = serde_json::to_value(svc.video_caps().await.expect("caps")).expect("json");
    assert_eq!(
        caps,
        json!({
            "camera": true,
            "screen": {"supported": true, "system_picker": true, "list": true, "windows": true},
            "publish_codecs": ["vp8", "h264"],
            "decode_codecs": ["vp8", "h264"],
            "receive": true,
            "self_view": true,
            "live_switch": true,
            "backend": "fake",
        })
    );
    let d = serde_json::to_value(svc.video_devices().await.expect("devices")).expect("json");
    assert_eq!(
        d["permission"],
        json!({"camera": "prompt", "screen": "granted"})
    );
    assert_eq!(d["cameras"][0]["id"], json!("fake:camera:0"));
    let e = svc
        .set_video_device(SetVideoDeviceArgs {
            camera: "cam:none".into(),
        })
        .await
        .expect_err("unknown camera");
    assert_eq!(e.code, "camera_not_found");
    svc.set_video_device(SetVideoDeviceArgs {
        camera: "fake:camera:0".into(),
    })
    .await
    .expect("known camera");
    assert_eq!(
        svc.video_devices()
            .await
            .expect("devices")
            .camera
            .as_deref(),
        Some("fake:camera:0")
    );
    let screens = serde_json::to_value(svc.screen_sources().await.expect("screens")).expect("json");
    assert_eq!(
        screens,
        json!([
            {"id": "fake:display:0", "kind": "display", "title": "Fake Display", "width": 1920, "height": 1080},
            {"id": "fake:window:1", "kind": "window", "title": "Fake Window", "app": "Fake App", "width": 800, "height": 600},
        ])
    );
    // Screen capture exists: `screen` then needs a call.
    let e = svc
        .screen(
            ScreenArgs {
                on: true,
                source: None,
            },
            None,
        )
        .await
        .expect_err("no call");
    assert_eq!(e.code, "not_in_call");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_screen_capture_screen_is_unsupported() {
    // kessel-video's null backend reports no screen capture, as Linux does without the
    // xdg portal or libpipewire.
    let svc = service(kessel_video::null());
    let caps = svc.video_caps().await.expect("caps");
    assert!(!caps.screen.supported && !caps.camera);
    let d = serde_json::to_value(svc.video_devices().await.expect("devices")).expect("json");
    assert_eq!(
        d["permission"],
        json!({"camera": "unsupported", "screen": "unsupported"})
    );
    let e = svc
        .screen(
            ScreenArgs {
                on: true,
                source: None,
            },
            None,
        )
        .await
        .expect_err("unsupported");
    assert_eq!(e.code, "screen_unsupported");
    assert_eq!(
        svc.screen_sources().await.expect_err("unsupported").code,
        "screen_unsupported"
    );
    let e = svc
        .preview(PreviewArgs {
            on: true,
            device: None,
        })
        .await
        .expect_err("no camera");
    assert_eq!(e.code, "video_unsupported");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_preview_feeds_a_self_view_sink_and_closes_the_camera() {
    let svc = service(fake(|_| {}));
    let ans = svc
        .preview(PreviewArgs {
            on: true,
            device: None,
        })
        .await
        .expect("preview");
    assert_eq!(
        serde_json::to_value(&ans).expect("json"),
        json!({"sink_track": "local:camera"})
    );
    let ch = Arc::new(RecordingFrameChannel::new());
    svc.video_sink(
        VideoSinkArgs {
            track: LOCAL_CAMERA.into(),
            width: 320,
            height: 180,
            visible: true,
            tile: Tile::Grid,
            decode: vec!["vp8".into()],
        },
        ch.clone(),
    )
    .await
    .expect("a self-view sink, no call needed");
    until("self-view frames", || ch.messages().len() >= 5).await;
    let frames = ch.frames();
    let (first, data) = &frames[0];
    assert_eq!(first.kind, kind::ENCODED);
    assert_eq!(first.codec, 1, "VP8");
    assert_eq!(
        first.flags,
        flag::KEYFRAME | flag::DISCONTINUITY | flag::SELF_VIEW | flag::FRONT,
        "the first is a keyframe, flagged self-view from a front camera"
    );
    assert_eq!(first.height, 360);
    assert_eq!(&data[3..6], &[0x9d, 0x01, 0x2a], "a VP8 key frame");
    for (h, _) in &frames[1..] {
        assert_ne!(h.flags & flag::SELF_VIEW, 0, "every frame is self-view");
        assert_eq!(h.flags & (flag::FRONT | flag::BACK), flag::FRONT, "front");
        assert_eq!(h.kind, kind::ENCODED);
    }
    assert!(
        frames[1..]
            .iter()
            .any(|(h, _)| h.flags & flag::KEYFRAME == 0),
        "deltas follow"
    );

    svc.preview(PreviewArgs {
        on: false,
        device: None,
    })
    .await
    .expect("preview off");
    // Nothing more comes once the camera is closed.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let n = ch.messages().len();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(ch.messages().len(), n, "no frames after preview off");
}
