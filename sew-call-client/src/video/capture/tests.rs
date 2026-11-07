//! The capture side and the preview on kessel-video's fake backend, behind a probe that counts
//! the open sources (the camera light) and records what the OS picker was given.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use kessel_video::fake::{FakeBackend, FakeConfig, FakePermission, FakePick};
use kessel_video::{
    DecoderConfig, EncoderConfig, Planes, ScreenCapability, VideoDecoder, VideoEncoder,
};

use super::*;
use crate::video::preview::{PREVIEW_HEIGHT, PreviewFrames, PreviewItem};
use crate::video::types::VideoCodec;

/// Delegates to a [`FakeBackend`]; counts open sources; records the picker's options; lets a
/// test change the capabilities (screen capture off, no codecs).
struct Probe {
    inner: Arc<FakeBackend>,
    open: Arc<AtomicUsize>,
    opened: AtomicUsize,
    picks: Mutex<Vec<PickOptions>>,
    caps: fn(&mut Capabilities),
}

impl Probe {
    fn new(cfg: FakeConfig, caps: fn(&mut Capabilities)) -> Arc<Probe> {
        Arc::new(Probe {
            inner: FakeBackend::new(cfg),
            open: Arc::new(AtomicUsize::new(0)),
            opened: AtomicUsize::new(0),
            picks: Mutex::new(Vec::new()),
            caps,
        })
    }

    fn open(&self) -> usize {
        self.open.load(Ordering::Acquire)
    }

    fn counted(&self, s: Box<dyn FrameSource>) -> Box<dyn FrameSource> {
        self.open.fetch_add(1, Ordering::AcqRel);
        self.opened.fetch_add(1, Ordering::AcqRel);
        Box::new(Counted {
            inner: s,
            open: Arc::clone(&self.open),
        })
    }
}

fn same(_: &mut Capabilities) {}

/// The fake's default script with `f`'s changes (`FakeConfig` is non-exhaustive).
fn fake(f: impl FnOnce(&mut FakeConfig)) -> FakeConfig {
    let mut c = FakeConfig::default();
    f(&mut c);
    c
}

struct Counted {
    inner: Box<dyn FrameSource>,
    open: Arc<AtomicUsize>,
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::AcqRel);
    }
}

impl FrameSource for Counted {
    fn try_recv(&mut self) -> Option<kessel_video::VideoFrame> {
        self.inner.try_recv()
    }
    fn try_event(&mut self) -> Option<SourceEvent> {
        self.inner.try_event()
    }
    fn set_constraints(&mut self, c: CaptureConstraints) -> Result<(), VideoError> {
        self.inner.set_constraints(c)
    }
    fn set_paused(&mut self, paused: bool) {
        self.inner.set_paused(paused);
    }
    fn stats(&self) -> SourceStats {
        self.inner.stats()
    }
}

impl VideoBackend for Probe {
    fn capabilities(&self) -> Capabilities {
        let mut c = self.inner.capabilities();
        (self.caps)(&mut c);
        c
    }
    fn now_us(&self) -> u64 {
        self.inner.now_us()
    }
    fn cameras(&self) -> Vec<CameraInfo> {
        self.inner.cameras()
    }
    fn camera_permission(&self) -> Permission {
        self.inner.camera_permission()
    }
    fn request_camera_permission(&self, done: kessel_video::Done<Permission>) {
        self.inner.request_camera_permission(done);
    }
    fn open_camera(
        &self,
        id: Option<&str>,
        c: CaptureConstraints,
        wake: Wake,
    ) -> Result<Box<dyn FrameSource>, VideoError> {
        let s = self.inner.open_camera(id, c, wake)?;
        Ok(self.counted(s))
    }
    fn screen_permission(&self) -> Permission {
        self.inner.screen_permission()
    }
    fn request_screen_permission(&self, done: kessel_video::Done<Permission>) {
        self.inner.request_screen_permission(done);
    }
    fn screen_sources(&self) -> Result<Vec<ScreenSourceInfo>, VideoError> {
        self.inner.screen_sources()
    }
    fn pick_screen(
        &self,
        opts: PickOptions,
        done: kessel_video::Done<Result<ScreenSelection, VideoError>>,
    ) {
        self.picks.lock().unwrap().push(opts);
        self.inner.pick_screen(opts, done);
    }
    fn open_screen(
        &self,
        source: &ScreenSelection,
        c: CaptureConstraints,
        wake: Wake,
    ) -> Result<Box<dyn FrameSource>, VideoError> {
        let s = self.inner.open_screen(source, c, wake)?;
        Ok(self.counted(s))
    }
    fn encoder(
        &self,
        config: EncoderConfig,
        wake: Wake,
    ) -> Result<Box<dyn VideoEncoder>, VideoError> {
        self.inner.encoder(config, wake)
    }
    fn decoder(
        &self,
        config: DecoderConfig,
        wake: Wake,
    ) -> Result<Box<dyn VideoDecoder>, VideoError> {
        self.inner.decoder(config, wake)
    }
    fn convert(
        &self,
        frame: &kessel_video::VideoFrame,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Result<kessel_video::VideoFrame, VideoError> {
        self.inner.convert(frame, width, height, format)
    }
    fn subscribe(&self) -> EventRx {
        self.inner.subscribe()
    }
    fn shutdown(&self) {
        self.inner.shutdown();
    }
}

fn capture(p: &Arc<Probe>) -> VideoCapture {
    let b: Arc<dyn VideoBackend> = Arc::clone(p) as Arc<dyn VideoBackend>;
    VideoCapture::new(b)
}

const WAIT: Duration = Duration::from_secs(10);

async fn next_frame(c: &mut Capture) -> kessel_video::VideoFrame {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Some(f) = c.try_frame() {
                return f;
            }
            let _ = tokio::time::timeout(Duration::from_millis(100), c.ready()).await;
        }
    })
    .await
    .expect("a frame within 10 s")
}

async fn next_event(c: &mut Capture) -> CaptureEvent {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Some(e) = c.try_event() {
                return e;
            }
            let _ = tokio::time::timeout(Duration::from_millis(50), c.ready()).await;
        }
    })
    .await
    .expect("an event within 10 s")
}

async fn wait_until(what: &str, mut pred: impl FnMut() -> bool) {
    let t = tokio::time::Instant::now();
    while !pred() {
        assert!(t.elapsed() < WAIT, "{what}: not within 10 s");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ───────────────────────────── cameras ─────────────────────────────

#[tokio::test]
async fn a_camera_asks_the_permission_once_then_opens_and_closes_on_drop() {
    let cfg = fake(|c| {
        c.camera_permission = FakePermission::Prompt {
            answer: Permission::Granted,
            after: Duration::from_millis(20),
        };
    });
    let p = Probe::new(cfg, same);
    let cap = capture(&p);
    assert_eq!(
        cap.camera_permission(),
        Permission::NotDetermined,
        "never asked yet"
    );
    assert_eq!(p.open(), 0, "nothing opens before open_camera");
    let mut cam = cap
        .open_camera(None, CaptureConstraints::new(640, 360, 30))
        .await
        .expect("granted at the prompt");
    assert_eq!(cap.camera_permission(), Permission::Granted);
    assert_eq!(p.open(), 1);
    assert_eq!(cam.kind(), CaptureKind::Camera);
    assert_eq!(cam.content_hint(), ContentHint::Camera);
    assert_eq!(cam.device(), None);
    assert_eq!(cam.constraints(), CaptureConstraints::new(640, 360, 30));
    let f = next_frame(&mut cam).await;
    assert!(
        (1..=1280).contains(&f.width) && (1..=720).contains(&f.height),
        "{}x{}",
        f.width,
        f.height
    );
    let mut c2 = cam.constraints();
    c2.fps = Some(15);
    cam.set_constraints(c2).expect("new constraints");
    assert_eq!(cam.constraints().fps, Some(15));
    drop(cam);
    assert_eq!(p.open(), 0, "dropping the source closes the camera at once");
}

#[tokio::test]
async fn camera_refusals_have_the_plugins_codes() {
    for (perm, what) in [
        (FakePermission::Denied, "denied"),
        (FakePermission::Restricted, "restricted"),
        (
            FakePermission::Prompt {
                answer: Permission::Denied,
                after: Duration::from_millis(5),
            },
            "denied at the prompt",
        ),
    ] {
        let p = Probe::new(
            fake(|c| {
                c.camera_permission = perm;
            }),
            same,
        );
        let e = capture(&p)
            .open_camera(None, CaptureConstraints::default())
            .await
            .unwrap_err();
        assert_eq!(e, CaptureError::CameraDenied, "{what}");
        assert_eq!(e.code(), "camera_denied");
        assert_eq!(
            p.opened.load(Ordering::Acquire),
            0,
            "{what}: no device opened"
        );
    }
    let p = Probe::new(FakeConfig::default(), same);
    let e = capture(&p)
        .open_camera(Some("no-such-camera"), CaptureConstraints::default())
        .await
        .unwrap_err();
    assert_eq!(e.code(), "camera_not_found");
    let e = VideoCapture::null()
        .open_camera(None, CaptureConstraints::default())
        .await
        .unwrap_err();
    assert_eq!(e.code(), "video_unsupported");
}

#[test]
fn every_error_has_its_code() {
    let all = [
        (CaptureError::VideoUnsupported, "video_unsupported"),
        (CaptureError::CameraDenied, "camera_denied"),
        (CaptureError::CameraBusy, "camera_busy"),
        (CaptureError::CameraNotFound, "camera_not_found"),
        (CaptureError::CameraLost, "camera_lost"),
        (CaptureError::CodecUnavailable, "codec_unavailable"),
        (CaptureError::ScreenUnsupported, "screen_unsupported"),
        (CaptureError::ScreenDenied, "screen_denied"),
        (CaptureError::PickerCancelled, "picker_cancelled"),
        (CaptureError::ScreenNotFound, "screen_not_found"),
        (CaptureError::EncoderLost, "encoder_lost"),
        (CaptureError::Failed(VideoError::Busy), "capture_failed"),
    ];
    for (e, code) in all {
        assert_eq!(e.code(), code);
    }
    assert_eq!(
        camera_error(VideoError::DeviceBusy),
        CaptureError::CameraBusy
    );
    assert_eq!(
        camera_error(VideoError::DeviceLost),
        CaptureError::CameraLost
    );
    assert_eq!(
        pick_error(VideoError::NotFound),
        CaptureError::PickerCancelled
    );
    assert_eq!(
        pick_error(VideoError::NoPermission),
        CaptureError::ScreenDenied
    );
    assert_eq!(
        screen_error(VideoError::NotFound),
        CaptureError::ScreenNotFound
    );
    assert_eq!(
        screen_error(VideoError::Unsupported("no portal".into())),
        CaptureError::ScreenUnsupported
    );
    let inv = VideoError::Invalid("no parent window".into());
    assert_eq!(pick_error(inv.clone()), CaptureError::Failed(inv));
}

// ───────────────────────────── screens ─────────────────────────────

fn no_screen(c: &mut Capabilities) {
    // Linux without the xdg portal or libpipewire.
    c.screen = ScreenCapability::default();
}

fn picker_only(c: &mut Capabilities) {
    c.screen.list = false;
    c.screen.exclude_own_windows = false;
}

#[tokio::test]
async fn without_screen_capture_everything_screen_is_unsupported() {
    for cap in [
        capture(&Probe::new(FakeConfig::default(), no_screen)),
        VideoCapture::null(),
    ] {
        assert_eq!(
            cap.screen_sources().await.unwrap_err().code(),
            "screen_unsupported"
        );
        let e = cap
            .open_screen(
                ScreenPick::System {
                    parent_window: None,
                },
                screen_constraints(1920, 1080, 15),
            )
            .await
            .unwrap_err();
        assert_eq!(e.code(), "screen_unsupported");
        let e = cap
            .open_screen(
                ScreenPick::Listed("fake:display:0".into()),
                screen_constraints(1920, 1080, 15),
            )
            .await
            .unwrap_err();
        assert_eq!(e.code(), "screen_unsupported");
    }
}

#[tokio::test]
async fn the_os_picker_gets_the_parent_window_and_opens_the_choice_as_a_screen() {
    let p = Probe::new(FakeConfig::default(), picker_only);
    let cap = capture(&p);
    assert!(
        cap.screen_sources().await.unwrap().is_empty(),
        "picker only: no list"
    );
    let s = cap
        .open_screen(
            ScreenPick::System {
                parent_window: Some(0x0001_0a2c),
            },
            screen_constraints(1920, 1080, 15),
        )
        .await
        .expect("the fake picker picks its display");
    let picks = p.picks.lock().unwrap().clone();
    assert_eq!(picks.len(), 1);
    assert_eq!(
        picks[0].parent_window,
        Some(0x0001_0a2c),
        "Windows needs the HWND"
    );
    assert!(picks[0].displays && picks[0].windows);
    assert_eq!(s.kind(), CaptureKind::Screen);
    assert_eq!(s.content_hint(), ContentHint::Screen);
    assert_eq!(s.device(), None, "the picker's choice has no listed id");
    assert!(s.constraints().cursor);
    assert!(
        !s.constraints().exclude_own_windows,
        "cleared where the OS can't leave the app's windows out"
    );
    assert_eq!(p.open(), 1);
    drop(s);
    assert_eq!(p.open(), 0);

    // No parent window: none is passed (macOS and Linux ignore it).
    cap.open_screen(
        ScreenPick::System {
            parent_window: None,
        },
        screen_constraints(1920, 1080, 15),
    )
    .await
    .expect("open");
    assert_eq!(p.picks.lock().unwrap()[1].parent_window, None);

    // Listed ids don't exist where only the picker does.
    let e = cap
        .open_screen(
            ScreenPick::Listed("fake:display:0".into()),
            screen_constraints(1920, 1080, 15),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code(), "screen_not_found");
}

#[tokio::test]
async fn picker_and_permission_refusals() {
    let p = Probe::new(
        fake(|c| {
            c.pick = FakePick::Cancel;
        }),
        same,
    );
    let e = capture(&p)
        .open_screen(
            ScreenPick::System {
                parent_window: None,
            },
            screen_constraints(1920, 1080, 15),
        )
        .await
        .unwrap_err();
    assert_eq!(e, CaptureError::PickerCancelled);
    assert_eq!(e.code(), "picker_cancelled");
    assert_eq!(p.open(), 0);

    let p = Probe::new(
        fake(|c| {
            c.screen_permission = FakePermission::Denied;
        }),
        same,
    );
    let e = capture(&p)
        .open_screen(
            ScreenPick::System {
                parent_window: None,
            },
            screen_constraints(1920, 1080, 15),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code(), "screen_denied");
    assert!(
        p.picks.lock().unwrap().is_empty(),
        "no picker without the permission"
    );

    let p = Probe::new(
        fake(|c| {
            c.screen_permission = FakePermission::Prompt {
                answer: Permission::Granted,
                after: Duration::from_millis(5),
            };
        }),
        same,
    );
    let s = capture(&p)
        .open_screen(
            ScreenPick::Listed("fake:window:1".into()),
            screen_constraints(1920, 1080, 15),
        )
        .await
        .expect("granted at the prompt");
    assert_eq!(s.device(), Some("fake:window:1"));
    assert!(
        s.constraints().exclude_own_windows,
        "kept where the OS honours it"
    );
    let e = capture(&p)
        .open_screen(
            ScreenPick::Listed("fake:window:gone".into()),
            screen_constraints(1920, 1080, 15),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code(), "screen_not_found");
}

#[tokio::test]
async fn screen_events_end_and_hidden_come_in_order() {
    let cfg = fake(|c| {
        c.source_events = vec![
            (
                Duration::from_millis(40),
                SourceEvent::Interrupted(Interruption::Hidden),
            ),
            (Duration::from_millis(80), SourceEvent::Resumed),
            (
                Duration::from_millis(120),
                SourceEvent::Ended(EndReason::UserStopped),
            ),
        ];
    });
    let p = Probe::new(cfg, same);
    let mut s = capture(&p)
        .open_screen(
            ScreenPick::System {
                parent_window: None,
            },
            screen_constraints(1920, 1080, 15),
        )
        .await
        .expect("open");
    let mut seen = Vec::new();
    while !matches!(seen.last(), Some(CaptureEvent::Ended(_))) {
        let e = next_event(&mut s).await;
        if let CaptureEvent::Paused(_) = e {
            assert_eq!(s.paused(), Some(CapturePause::Hidden));
        }
        if !matches!(e, CaptureEvent::Started { .. }) {
            seen.push(e);
        }
    }
    assert_eq!(
        seen,
        vec![
            CaptureEvent::Paused(CapturePause::Hidden),
            CaptureEvent::Resumed,
            CaptureEvent::Ended(CaptureEnd::UserStopped),
        ]
    );
    assert_eq!(s.ended(), Some(CaptureEnd::UserStopped));
    assert_eq!(s.paused(), None);
    assert!(s.try_frame().is_none(), "no frames once ended");

    // The other ends map one to one.
    for (r, want) in [
        (EndReason::SourceClosed, CaptureEnd::SourceClosed),
        (EndReason::Revoked, CaptureEnd::Revoked),
        (EndReason::DeviceLost, CaptureEnd::DeviceLost),
        (EndReason::Closed, CaptureEnd::Closed),
    ] {
        assert_eq!(
            map_event(SourceEvent::Ended(r)),
            Some(CaptureEvent::Ended(want))
        );
    }
    assert_eq!(
        map_event(SourceEvent::Interrupted(Interruption::Thermal)),
        Some(CaptureEvent::Paused(CapturePause::Thermal))
    );
}

// ───────────────────────────── thumbnails ─────────────────────────────

/// An RGBA (or BGRA) picture: red left half, blue right half, `pad` bytes after each row.
fn picture(format: PixelFormat, w: u32, h: u32, pad: u32) -> Planes {
    let stride = w * 4 + pad;
    let mut data = vec![0u8; (stride * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let at = (y * stride + x * 4) as usize;
            let (r, g, b) = if x < w / 2 { (255, 0, 0) } else { (0, 0, 255) };
            let px = match format {
                PixelFormat::Bgra => [b, g, r, 255],
                _ => [r, g, b, 255],
            };
            data[at..at + 4].copy_from_slice(&px);
        }
        // Padding that must never be read as pixels.
        for i in 0..pad {
            data[(y * stride + w * 4 + i) as usize] = 0x77;
        }
    }
    Planes::new(format, w, h, data, [stride, 0, 0], [0, 0, 0]).expect("planes")
}

fn decode(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    assert_eq!(&png_bytes[..8], b"\x89PNG\r\n\x1a\n", "a PNG");
    let w = u32::from_be_bytes(png_bytes[16..20].try_into().unwrap());
    let h = u32::from_be_bytes(png_bytes[20..24].try_into().unwrap());
    assert_eq!((png_bytes[24], png_bytes[25]), (8, 6), "8-bit RGBA");
    let mut reader = png::Decoder::new(std::io::Cursor::new(png_bytes))
        .read_info()
        .expect("info");
    let mut buf = vec![0u8; (w * h * 4) as usize];
    reader.next_frame(&mut buf).expect("pixels");
    (w, h, buf)
}

fn px(buf: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * w + x) * 4) as usize;
    buf[at..at + 4].try_into().unwrap()
}

#[tokio::test]
async fn thumbnails_are_pngs_at_most_320_wide() {
    let mut display = ScreenSourceInfo::new("d0", ScreenKind::Display, "Display", 1920, 1080);
    display.thumbnail = Some(picture(PixelFormat::Rgba, 400, 225, 0));
    (display.thumbnail_width, display.thumbnail_height) = (400, 225);
    let mut window = ScreenSourceInfo::new("w1", ScreenKind::Window, "Notes", 800, 600);
    window.app = Some("Notes App".into());
    // BGRA with padded rows: the size is the backend's, never the padding.
    window.thumbnail = Some(picture(PixelFormat::Bgra, 160, 120, 64));
    (window.thumbnail_width, window.thumbnail_height) = (160, 120);
    let bare = ScreenSourceInfo::new("w2", ScreenKind::Window, "No preview", 640, 480);
    // Planes without their size: no thumbnail (no guess from the stride).
    let mut no_size = ScreenSourceInfo::new("w4", ScreenKind::Window, "No size", 640, 480);
    no_size.thumbnail = Some(picture(PixelFormat::Rgba, 160, 120, 0));
    // A size the planes don't hold: no thumbnail.
    let mut short = ScreenSourceInfo::new("w5", ScreenKind::Window, "Too short", 640, 480);
    short.thumbnail = Some(picture(PixelFormat::Rgba, 160, 120, 0));
    (short.thumbnail_width, short.thumbnail_height) = (160, 240);
    let mut yuv = ScreenSourceInfo::new("w3", ScreenKind::Window, "YUV", 64, 64);
    yuv.thumbnail = Some(
        kessel_video::VideoFrame::filled(PixelFormat::I420, 64, 64, [16, 128, 128], 0)
            .unwrap()
            .planes()
            .unwrap()
            .clone(),
    );
    (yuv.thumbnail_width, yuv.thumbnail_height) = (64, 64);
    let p = Probe::new(
        fake(|c| {
            c.screens = vec![display, window, bare, yuv, no_size, short];
        }),
        same,
    );
    let list = capture(&p).screen_sources().await.expect("listed");
    assert_eq!(list.len(), 6);
    assert_eq!(
        (list[0].id.as_str(), list[0].kind),
        ("d0", ScreenKind::Display)
    );
    assert_eq!((list[0].width, list[0].height), (1920, 1080));

    let (w, h, buf) = decode(list[0].thumbnail_png.as_deref().expect("a thumbnail"));
    assert_eq!((w, h), (320, 180), "scaled down to 320 wide, aspect kept");
    assert_eq!(px(&buf, w, 10, 90), [255, 0, 0, 255]);
    assert_eq!(px(&buf, w, 310, 90), [0, 0, 255, 255]);

    assert_eq!(list[1].app.as_deref(), Some("Notes App"));
    let (w, h, buf) = decode(list[1].thumbnail_png.as_deref().expect("a thumbnail"));
    assert_eq!((w, h), (160, 120), "small enough: kept as is");
    assert_eq!(
        px(&buf, w, 5, 60),
        [255, 0, 0, 255],
        "BGRA turned into RGBA"
    );
    assert_eq!(
        px(&buf, w, 155, 60),
        [0, 0, 255, 255],
        "no padding read as pixels"
    );

    assert!(list[2].thumbnail_png.is_none(), "none given");
    assert!(list[3].thumbnail_png.is_none(), "not RGBA or BGRA");
    assert!(list[4].thumbnail_png.is_none(), "no size given");
    assert!(
        list[5].thumbnail_png.is_none(),
        "a size the planes don't hold"
    );
    for s in &list {
        if let Some(t) = &s.thumbnail_png {
            assert!(decode(t).0 <= THUMBNAIL_MAX_WIDTH);
        }
    }
}

// ───────────────────────────── the preview ─────────────────────────────

async fn next_item(rx: &mut PreviewFrames) -> Option<PreviewItem> {
    tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("an item within 10 s")
}

/// A VP8 key frame's size from its header (RFC 6386 §9.1).
fn vp8_key_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 10 || data[0] & 1 != 0 || data[3..6] != [0x9d, 0x01, 0x2a] {
        return None;
    }
    let w = u32::from(u16::from_le_bytes([data[6], data[7]]) & 0x3fff);
    let h = u32::from(u16::from_le_bytes([data[8], data[9]]) & 0x3fff);
    Some((w, h))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_preview_encodes_vp8_at_360p_keyframes_on_request_and_closes_the_camera() {
    let p = Probe::new(FakeConfig::default(), same);
    let cap = capture(&p);
    assert_eq!(preview_codec_of(&cap), Some(VideoCodec::Vp8));
    assert_eq!(p.open(), 0, "the camera opens only on preview");
    let (preview, mut rx) = cap.preview(None).await.expect("preview");
    assert_eq!(preview.codec(), VideoCodec::Vp8);
    assert!(preview.is_running());
    assert_eq!(p.open(), 1);

    let Some(PreviewItem::Frame(first)) = next_item(&mut rx).await else {
        panic!("a frame first");
    };
    assert!(first.keyframe, "the first frame is a keyframe");
    assert_eq!(first.codec, VideoCodec::Vp8);
    assert_eq!(
        first.height, PREVIEW_HEIGHT,
        "fits 640x360 with the camera's aspect"
    );
    assert!(first.width <= 640 && first.width % 2 == 0);
    assert_eq!(
        vp8_key_size(&first.data),
        Some((first.width, first.height)),
        "a VP8 key frame"
    );

    // Deltas follow; a request brings another keyframe.
    let mut deltas = 0;
    while deltas < 3 {
        match next_item(&mut rx).await {
            Some(PreviewItem::Frame(f)) => deltas += usize::from(!f.keyframe),
            other => panic!("frames only while running, not {other:?}"),
        }
    }
    preview.request_keyframe();
    let got_key = tokio::time::timeout(WAIT, async {
        loop {
            match rx.recv().await {
                Some(PreviewItem::Frame(f)) if f.keyframe => return f,
                Some(PreviewItem::Frame(_)) => {}
                other => panic!("frames only while running, not {other:?}"),
            }
        }
    })
    .await
    .expect("a keyframe after the request");
    assert!(vp8_key_size(&got_key.data).is_some());

    preview.stop();
    assert!(!preview.is_running());
    assert_eq!(p.open(), 0, "stopping closes the camera at once");
    // The channel ends.
    tokio::time::timeout(WAIT, async { while rx.recv().await.is_some() {} })
        .await
        .expect("the channel closes");
    drop(preview);

    // Dropping a running preview closes the camera too.
    let (preview, _rx) = cap.preview(Some("fake:camera:0")).await.expect("preview");
    assert_eq!(preview.device(), Some("fake:camera:0"));
    assert_eq!(p.open(), 1);
    drop(preview);
    assert_eq!(p.open(), 0);
}

/// One opener holds the camera: a second preview replaces the first, and a camera opened for
/// the publish (through a clone of the same capture, as the bridge's is) stops the preview
/// before it opens; never two open at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_opener_holds_the_camera_from_preview_to_publish() {
    let p = Probe::new(FakeConfig::default(), same);
    let cap = capture(&p);
    let (first, mut rx1) = cap.preview(None).await.expect("preview");
    assert_eq!(p.open(), 1);
    let (second, mut rx2) = cap.preview(None).await.expect("a second preview");
    assert!(!first.is_running(), "the first preview stopped");
    assert_eq!(p.open(), 1, "never two cameras");
    tokio::time::timeout(WAIT, async { while rx1.recv().await.is_some() {} })
        .await
        .expect("the first channel ends");
    assert!(cap.preview_running());
    let publish = cap.clone();
    let mut cam = publish
        .open_camera(None, CaptureConstraints::new(640, 360, 30))
        .await
        .expect("the camera for the publish");
    assert!(!second.is_running(), "the preview handed the camera over");
    assert!(!cap.preview_running());
    assert_eq!(p.open(), 1);
    assert_eq!(p.opened.load(Ordering::Acquire), 3);
    tokio::time::timeout(WAIT, async { while rx2.recv().await.is_some() {} })
        .await
        .expect("the preview's channel ends");
    let _ = next_frame(&mut cam).await;
    drop(cam);
    assert_eq!(p.open(), 0, "the camera closes when the publish stops");
}

fn preview_codec_of(cap: &VideoCapture) -> Option<VideoCodec> {
    crate::video::preview::preview_codec(&cap.capabilities())
}

fn no_codecs(c: &mut Capabilities) {
    c.codecs.clear();
}

#[tokio::test]
async fn the_preview_refuses_before_opening_anything() {
    let p = Probe::new(FakeConfig::default(), no_codecs);
    let e = capture(&p).preview(None).await.unwrap_err();
    assert_eq!(e.code(), "codec_unavailable");
    assert_eq!(
        p.opened.load(Ordering::Acquire),
        0,
        "no camera opened for nothing"
    );

    let p = Probe::new(
        fake(|c| {
            c.camera_permission = FakePermission::Denied;
        }),
        same,
    );
    assert_eq!(
        capture(&p).preview(None).await.unwrap_err().code(),
        "camera_denied"
    );
    assert_eq!(p.opened.load(Ordering::Acquire), 0);
    assert_eq!(
        VideoCapture::null().preview(None).await.unwrap_err().code(),
        "video_unsupported"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_camera_that_ends_ends_the_preview() {
    let p = Probe::new(
        fake(|c| {
            c.source_events = vec![(
                Duration::from_millis(300),
                SourceEvent::Ended(EndReason::DeviceLost),
            )];
        }),
        same,
    );
    let (preview, mut rx) = capture(&p).preview(None).await.expect("preview");
    let ended = tokio::time::timeout(WAIT, async {
        loop {
            match rx.recv().await {
                Some(PreviewItem::Ended(e)) => return Some(e),
                Some(_) => {}
                None => return None,
            }
        }
    })
    .await
    .expect("within 10 s");
    assert_eq!(ended, Some(CaptureEnd::DeviceLost));
    wait_until("the camera closed", || p.open() == 0).await;
    wait_until("the preview stopped", || !preview.is_running()).await;
    assert!(
        next_item(&mut rx).await.is_none(),
        "the channel ends after Ended"
    );
}
