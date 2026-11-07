//! The screen ring's consumer glue against a ring written in this process (kessel-frame-ring's
//! own producer), and against rings scribbled on as a hostile extension would.
//!
//! The targeted hostile tests write the ring's words directly. They use the file format of
//! kessel-frame-ring 0.1 (format version 1; `format_is_version_1` fails loudly when the crate
//! moves on): a 256-byte header, then each slot's 64-byte header and its frame, each slot
//! rounded up to 64 bytes.

use std::fs::OpenOptions;
use std::os::unix::fs::FileExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use kessel_frame_ring::{EndReason as RingEnd, Layout, Nv12Frame, Producer, Pushed};
use sew_call_client::kessel_video::{PixelFormat, SourceEvent, VideoFrame};

use super::*;

// The format's offsets (version 1).
const HEADER: u64 = 256;
const SLOT_HEADER: u64 = 64;
const H_STATE: u64 = 56;
const H_LATEST: u64 = 64;
const SL_SEQ: u64 = 0;
const SL_FRAME_NO: u64 = 8;
const SL_WIDTH: u64 = 16;
const SL_HEIGHT: u64 = 20;
const SL_ROTATION: u64 = 24;
const SL_COLOR: u64 = 28;
const SL_LEN: u64 = 40;
const S_RUNNING: u32 = 1;

/// A fresh directory for one test's ring, removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "sew-screen-ring-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }

    fn ring(&self) -> PathBuf {
        ring_path(&self.0)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A 64 × 64 box, 3 slots, no rate limit.
fn small() -> Layout {
    let mut l = Layout::new(64, 64);
    l.max_fps = 0;
    l
}

fn slot_stride(l: &Layout) -> u64 {
    (SLOT_HEADER + l.slot_bytes() as u64).div_ceil(64) * 64
}

fn slot_at(l: &Layout, slot: u64) -> u64 {
    HEADER + slot * slot_stride(l)
}

/// Writes `v` at `at` in the ring's file, as another process would.
fn poke(path: &Path, at: u64, v: &[u8]) {
    let f = OpenOptions::new().write(true).open(path).unwrap();
    f.write_all_at(v, at).unwrap();
}

fn poke32(path: &Path, at: u64, v: u32) {
    poke(path, at, &v.to_le_bytes());
}

fn poke64(path: &Path, at: u64, v: u64) {
    poke(path, at, &v.to_le_bytes());
}

fn peek64(path: &Path, at: u64) -> u64 {
    let f = OpenOptions::new().read(true).open(path).unwrap();
    let mut b = [0u8; 8];
    f.read_exact_at(&mut b, at).unwrap();
    u64::from_le_bytes(b)
}

/// A `w × h` NV12 frame whose Y plane is all `n` (tight).
fn nv12(w: u32, h: u32, n: u8) -> (Vec<u8>, Vec<u8>) {
    let (w, h) = (w as usize, h as usize);
    (vec![n; w * h], vec![128; 2 * w.div_ceil(2) * h.div_ceil(2)])
}

fn push(p: &mut Producer, w: u32, h: u32, n: u8, rotation: u32) -> Pushed {
    let (y, uv) = nv12(w, h, n);
    let mut f = Nv12Frame::new(&y, w as usize, &uv, 2 * (w as usize).div_ceil(2), w, h);
    f.rotation = rotation;
    p.push(&f).unwrap()
}

fn written(p: &mut Producer, w: u32, h: u32, n: u8) {
    let r = push(p, w, h, n, 0);
    assert!(matches!(r, Pushed::Written { .. }), "{r:?}");
}

/// The frames and events a share's source has now.
fn drain(feed: &mut SourceFeed) -> (Vec<VideoFrame>, Vec<SourceEvent>) {
    let mut frames = Vec::new();
    while let Some(f) = feed.frames.try_recv() {
        frames.push(f);
    }
    let mut events = Vec::new();
    while let Some(e) = feed.frames.try_event() {
        events.push(e);
    }
    (frames, events)
}

fn y0(f: &VideoFrame) -> u8 {
    f.planes().expect("CPU planes").data[0]
}

/// The first step that starts a share.
fn started(pump: &mut RingPump) -> SourceFeed {
    for _ in 0..20 {
        match pump.step(Instant::now()) {
            PumpStep::Start(feed) => return feed,
            PumpStep::Idle => std::thread::sleep(Duration::from_millis(2)),
            other => panic!("expected a start, got {other:?}"),
        }
    }
    panic!("no share started");
}

/// The ring, a producer attached and running (the consumer has looked once, so it counts
/// as present).
fn ring(dir: &Dir) -> (RingPump, Producer) {
    let mut pump = RingPump::create(dir.ring(), small()).unwrap();
    assert!(matches!(pump.step(Instant::now()), PumpStep::Idle));
    let p = Producer::attach(dir.ring()).unwrap();
    p.start();
    (pump, p)
}

#[test]
fn format_is_version_1() {
    assert_eq!(
        kessel_frame_ring::VERSION,
        1,
        "the hostile tests' offsets are version 1's"
    );
    let dir = Dir::new("version");
    let pump = RingPump::create(dir.ring(), small()).unwrap();
    let len = std::fs::metadata(dir.ring()).unwrap().len();
    assert_eq!(len, slot_at(&pump.layout(), 3), "3 slots after the header");
}

#[test]
fn a_share_starts_flows_and_ends_when_the_extension_finishes() {
    let dir = Dir::new("flow");
    let (mut pump, mut p) = ring(&dir);
    written(&mut p, 64, 48, 1);
    let mut feed = started(&mut pump);
    assert_eq!((feed.width, feed.height), (64, 48));
    assert!(pump.is_sharing());
    for n in 2..=6u8 {
        std::thread::sleep(Duration::from_millis(6));
        written(&mut p, 64, 48, n);
        assert!(matches!(pump.step(Instant::now()), PumpStep::Frame));
    }
    let (frames, events) = drain(&mut feed);
    // A queue of 2: the newest two, in order, as tight NV12 of the frame's size.
    assert!(!frames.is_empty(), "frames reached the source");
    let ys: Vec<u8> = frames.iter().map(y0).collect();
    assert!(ys.windows(2).all(|w| w[0] < w[1]), "in order: {ys:?}");
    assert_eq!(*ys.last().unwrap(), 6);
    for f in &frames {
        assert_eq!((f.width, f.height, f.format()), (64, 48, PixelFormat::Nv12));
    }
    assert!(
        events.iter().any(|e| matches!(
            e,
            SourceEvent::Started {
                width: 64,
                height: 48,
                ..
            }
        )),
        "{events:?}"
    );
    // The extension finishes (the person stopped the broadcast): the share ends.
    p.finish(RingEnd::UserStopped);
    let PumpStep::Ended(why) = pump.step(Instant::now()) else {
        panic!("the share did not end");
    };
    assert_eq!(why, ShareEnd::UserStopped);
    assert!(!pump.is_sharing());
    let (_, events) = drain(&mut feed);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SourceEvent::Ended(EndReason::UserStopped))),
        "{events:?}"
    );
    // A fresh ring is armed: the next broadcast is a new share.
    assert_eq!(pump.stats().rings, 2);
    assert!(matches!(pump.step(Instant::now()), PumpStep::Idle));
    let mut p2 = Producer::attach(dir.ring()).unwrap();
    assert!(!p2.stop_requested());
    p2.start();
    written(&mut p2, 32, 32, 9);
    let mut feed2 = started(&mut pump);
    assert_eq!((feed2.width, feed2.height), (32, 32));
    let (frames, _) = drain(&mut feed2);
    assert_eq!(frames.iter().map(y0).collect::<Vec<_>>(), vec![9]);
    assert_eq!(pump.stats().shares, 2);
}

#[test]
fn a_turned_frame_publishes_upright() {
    let dir = Dir::new("turned");
    let (mut pump, mut p) = ring(&dir);
    // 64 × 32, a quarter turn short of upright: the share is 32 × 64.
    assert!(matches!(
        push(&mut p, 64, 32, 7, 90),
        Pushed::Written { .. }
    ));
    let mut feed = started(&mut pump);
    assert_eq!((feed.width, feed.height), (32, 64));
    let (frames, _) = drain(&mut feed);
    assert_eq!(frames.len(), 1);
    assert_eq!((frames[0].width, frames[0].height), (32, 64));
}

#[test]
fn an_extension_that_never_says_it_runs_goes_idle_after_two_seconds() {
    let dir = Dir::new("idle");
    let mut pump = RingPump::create(dir.ring(), small()).unwrap();
    let t0 = Instant::now();
    assert!(matches!(pump.step(t0), PumpStep::Idle));
    let mut p = Producer::attach(dir.ring()).unwrap();
    // Frames without `start`: the state stays waiting.
    written(&mut p, 32, 32, 1);
    let PumpStep::Start(mut feed) = pump.step(t0) else {
        panic!("no start");
    };
    assert!(matches!(
        pump.step(t0 + Duration::from_secs(1)),
        PumpStep::Idle
    ));
    let PumpStep::Ended(why) = pump.step(t0 + IDLE_END + Duration::from_millis(1)) else {
        panic!("the quiet share did not end");
    };
    assert_eq!(why, ShareEnd::Idle);
    // The extension is asked to stop (its old ring), the source ended.
    assert!(p.stop_requested());
    let (_, events) = drain(&mut feed);
    assert!(events.iter().any(|e| matches!(e, SourceEvent::Ended(_))));
}

#[test]
fn a_running_quiet_extension_keeps_its_share_and_the_frame_is_sent_again() {
    let dir = Dir::new("static");
    let (mut pump, mut p) = ring(&dir);
    written(&mut p, 32, 32, 5);
    let t0 = Instant::now();
    let PumpStep::Start(mut feed) = pump.step(t0) else {
        panic!("no start");
    };
    // A static screen: no new frame for a long while, the extension still running.
    let mut repeats = 0;
    for s in 1..=5u64 {
        match pump.step(t0 + Duration::from_secs(s)) {
            PumpStep::Frame => repeats += 1,
            PumpStep::Idle => {}
            other => panic!("the share of a running extension ended: {other:?}"),
        }
        std::thread::sleep(Duration::from_millis(6));
    }
    assert!(repeats >= 4, "{repeats} repeats");
    assert!(pump.is_sharing());
    let (frames, _) = drain(&mut feed);
    assert!(frames.iter().all(|f| y0(f) == 5));
    let ts: Vec<u64> = frames.iter().map(|f| f.capture_us).collect();
    assert!(
        ts.windows(2).all(|w| w[0] < w[1]),
        "capture times go up: {ts:?}"
    );
}

#[test]
fn the_app_stopping_the_share_asks_the_extension_to_stop() {
    let dir = Dir::new("appstop");
    let (mut pump, mut p) = ring(&dir);
    written(&mut p, 32, 32, 1);
    let feed = started(&mut pump);
    // `screen {on: false}`: the bridge drops the source.
    drop(feed);
    let PumpStep::Ended(why) = pump.step(Instant::now()) else {
        panic!("the share did not end");
    };
    assert_eq!(why, ShareEnd::AppStopped);
    assert!(p.stop_requested(), "the extension finishes its broadcast");
}

#[test]
fn a_refused_publication_stops_the_extension_and_rearms() {
    let dir = Dir::new("refused");
    let (mut pump, mut p) = ring(&dir);
    written(&mut p, 32, 32, 1);
    drop(started(&mut pump));
    pump.refused();
    assert!(!pump.is_sharing());
    assert!(p.stop_requested());
    assert_eq!(pump.stats().rings, 2);
    assert!(
        Producer::attach(dir.ring()).is_ok(),
        "a fresh ring at the path"
    );
}

#[test]
fn an_extension_that_dies_ends_the_share() {
    let dir = Dir::new("gone");
    let mut pump = RingPump::create(dir.ring(), small()).unwrap();
    assert!(matches!(pump.step(Instant::now()), PumpStep::Idle));
    let mut p = Producer::attach(dir.ring()).unwrap();
    p.start();
    written(&mut p, 32, 32, 1);
    let _feed = started(&mut pump);
    // Killed: its lock is gone without an end. (A dropped producer says `Closed`, so the
    // state is written back to running as a killed process would have left it.)
    drop(p);
    poke32(&dir.ring(), H_STATE, S_RUNNING);
    let PumpStep::Ended(why) = pump.step(Instant::now()) else {
        panic!("the share did not end");
    };
    assert_eq!(why, ShareEnd::ExtensionGone);
}

#[test]
fn a_ring_that_shrinks_ends_the_share_and_a_fresh_one_is_armed() {
    let dir = Dir::new("shrink");
    let (mut pump, mut p) = ring(&dir);
    written(&mut p, 32, 32, 1);
    let _feed = started(&mut pump);
    // The extension truncates the file under the mapping.
    let f = OpenOptions::new().write(true).open(dir.ring()).unwrap();
    f.set_len(100).unwrap();
    let PumpStep::Ended(why) = pump.step(Instant::now()) else {
        panic!("the share did not end");
    };
    assert_eq!(why, ShareEnd::RingLost);
    let len = std::fs::metadata(dir.ring()).unwrap().len();
    assert_eq!(len, slot_at(&small(), 3), "a whole ring again");
    // The producer's mapping is past the old file's end: never touched again.
    std::mem::forget(p);
}

#[test]
fn a_ring_that_disappears_is_made_again() {
    let dir = Dir::new("gone-file");
    let mut pump = RingPump::create(dir.ring(), small()).unwrap();
    let t0 = Instant::now();
    assert!(matches!(pump.step(t0), PumpStep::Idle));
    std::fs::remove_file(dir.ring()).unwrap();
    assert!(Producer::attach(dir.ring()).is_err());
    assert!(matches!(
        pump.step(t0 + Duration::from_secs(2)),
        PumpStep::Idle
    ));
    assert_eq!(pump.stats().rings, 2);
    let mut p = Producer::attach(dir.ring()).unwrap();
    p.start();
    written(&mut p, 32, 32, 3);
    let _ = started(&mut pump);
    // Replaced by another file: made again too.
    drop(p);
    drop(pump);
    let mut pump = RingPump::create(dir.ring(), small()).unwrap();
    assert!(matches!(pump.step(t0), PumpStep::Idle));
    std::fs::remove_file(dir.ring()).unwrap();
    std::fs::write(dir.ring(), b"not a ring").unwrap();
    assert!(matches!(
        pump.step(t0 + Duration::from_secs(2)),
        PumpStep::Idle
    ));
    assert!(Producer::attach(dir.ring()).is_ok());
}

#[test]
fn a_dropped_pump_stops_the_extension_and_removes_the_ring() {
    let dir = Dir::new("drop");
    let (pump, p) = ring(&dir);
    drop(pump);
    assert!(p.stop_requested());
    assert!(!dir.ring().exists());
}

/// A written slot, then `bad` applied to it: no frame comes out and nothing panics.
fn bad_slot(name: &str, bad: impl Fn(&Path, u64)) {
    let dir = Dir::new(name);
    let (mut pump, mut p) = ring(&dir);
    written(&mut p, 32, 32, 1);
    let mut feed = started(&mut pump);
    std::thread::sleep(Duration::from_millis(6));
    written(&mut p, 32, 32, 2);
    let latest = peek64(&dir.ring(), H_LATEST);
    let slot = latest & 0xff;
    bad(&dir.ring(), slot_at(&small(), slot));
    for _ in 0..5 {
        match pump.step(Instant::now()) {
            PumpStep::Idle | PumpStep::Frame | PumpStep::Ended(_) => {}
            other => panic!("{name}: {other:?}"),
        }
    }
    let (frames, _) = drain(&mut feed);
    assert!(
        frames
            .iter()
            .all(|f| y0(f) == 1 && (f.width, f.height) == (32, 32)),
        "{name}: a bad slot came out"
    );
    drop(p);
}

#[test]
fn hostile_slots_are_skipped() {
    // A truncated frame: the length says less than the size needs.
    bad_slot("truncated", |r, s| poke64(r, s + SL_LEN, 100));
    // A length beyond the slot.
    bad_slot("long", |r, s| poke64(r, s + SL_LEN, u64::MAX));
    // Huge dimensions.
    bad_slot("huge", |r, s| {
        poke32(r, s + SL_WIDTH, u32::MAX);
        poke32(r, s + SL_HEIGHT, u32::MAX);
    });
    bad_slot("wide", |r, s| poke32(r, s + SL_WIDTH, 4097));
    bad_slot("zero", |r, s| poke32(r, s + SL_HEIGHT, 0));
    // A rotation and a colour no frame has.
    bad_slot("rotation", |r, s| poke32(r, s + SL_ROTATION, 45));
    bad_slot("color", |r, s| poke32(r, s + SL_COLOR, 99));
    // A torn sequence: odd (being written) for good.
    bad_slot("torn", |r, s| {
        let seq = peek64(r, s + SL_SEQ);
        poke64(r, s + SL_SEQ, seq | 1);
    });
    // A frame number that doesn't match the newest pointer.
    bad_slot("frame-no", |r, s| poke64(r, s + SL_FRAME_NO, 0));
    // The newest pointer at a slot past the layout's.
    bad_slot("slot", |r, _| {
        let latest = peek64(r, H_LATEST);
        poke64(r, H_LATEST, (latest & !0xff) | 0xfe);
    });
}

/// xorshift64*: the seeded rounds' randomness.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// Edge values for a scribbled word.
fn word(rng: &mut Rng) -> u64 {
    match rng.below(10) {
        0 => 0,
        1 => 1,
        2 => u64::MAX,
        3 => u64::from(u32::MAX),
        4 => 4097,
        5 => 64,
        6 => 32,
        7 => rng.next() | 1,
        _ => rng.next(),
    }
}

#[test]
fn seeded_scribbles_never_panic_and_only_whole_frames_come_out() {
    let iters: u64 = std::env::var("SEW_RING_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3_000);
    let seed: u64 = std::env::var("SEW_RING_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x5e3_51ce);
    eprintln!("screen ring scribbles: {iters} rounds, seed {seed:#x}");
    let mut rng = Rng(seed | 1);
    let dir = Dir::new("scribble");
    let layout = small();
    let file_len = slot_at(&layout, 3);
    let mut pump = RingPump::create(dir.ring(), layout).unwrap();
    let t0 = Instant::now();
    let mut clock = t0;
    let mut feeds: Vec<SourceFeed> = Vec::new();
    let mut producer: Option<Producer> = None;
    let mut out = 0u64;
    for round in 0..iters {
        clock += Duration::from_millis(rng.below(400));
        // A producer, now and then (a fresh ring after an end needs a new one).
        if producer.is_none() || rng.below(20) == 0 {
            producer = Producer::attach(dir.ring()).ok();
            if let Some(p) = &producer
                && rng.below(2) == 0
            {
                p.start();
            }
        }
        if let Some(p) = producer.as_mut()
            && rng.below(2) == 0
        {
            let (w, h) = (16 + 2 * rng.below(25) as u32, 16 + 2 * rng.below(25) as u32);
            let (y, uv) = nv12(w, h, (round % 251) as u8);
            let mut f = Nv12Frame::new(&y, w as usize, &uv, 2 * (w as usize).div_ceil(2), w, h);
            f.rotation = [0, 90, 180, 270][rng.below(4) as usize];
            // The stand-in extension reads words this test scribbles on too: its producer
            // treats the file as untrusted, so a scribble never panics it.
            let _ = p.push(&f);
        }
        // Scribbles: the header's words, a slot's header, or anywhere.
        for _ in 0..rng.below(4) {
            let at = match rng.below(3) {
                0 => rng.below(HEADER / 4) * 4,
                1 => slot_at(&layout, rng.below(3)) + rng.below(SLOT_HEADER / 4) * 4,
                _ => rng.below(file_len),
            };
            let w = word(&mut rng);
            if let Ok(f) = OpenOptions::new().write(true).open(dir.ring()) {
                let bytes = if rng.below(2) == 0 {
                    w.to_le_bytes().to_vec()
                } else {
                    (w as u32).to_le_bytes().to_vec()
                };
                let _ = f.write_all_at(&bytes, at.min(file_len.saturating_sub(8)));
            }
        }
        // Rarely: a shrink, or the file removed.
        match rng.below(200) {
            0 => {
                if let Ok(f) = OpenOptions::new().write(true).open(dir.ring()) {
                    let _ = f.set_len(rng.below(file_len));
                }
                // This process's producer maps that file: past its end it would fault, so it
                // is never touched again (as a killed extension).
                if let Some(p) = producer.take() {
                    std::mem::forget(p);
                }
            }
            1 => {
                let _ = std::fs::remove_file(dir.ring());
            }
            _ => {}
        }
        match pump.step(clock) {
            PumpStep::Start(feed) => {
                if rng.below(10) == 0 {
                    pump.refused();
                } else {
                    feeds.push(feed);
                }
            }
            PumpStep::Lost(why) => panic!("round {round}: the ring was lost: {why}"),
            _ => {}
        }
        for feed in &mut feeds {
            let (frames, _) = drain(feed);
            for f in frames {
                out += 1;
                let (bw, bh) = (layout.max_width, layout.max_height);
                assert!(
                    (f.width <= bw && f.height <= bh) || (f.width <= bh && f.height <= bw),
                    "round {round}: a {}x{} frame",
                    f.width,
                    f.height
                );
                let planes = f.planes().expect("CPU planes");
                assert!(planes.validate(f.width, f.height).is_ok(), "round {round}");
            }
        }
        feeds.retain(|_| rng.below(50) != 0);
    }
    let st = pump.stats();
    eprintln!("screen ring scribbles: {out} frames out, {st:?}");
    assert!(st.shares > 0, "no share ever started: {st:?}");
}

#[test]
fn the_layout_follows_the_policy_and_caps_at_720p() {
    let l = layout_for(None);
    assert_eq!((l.max_width, l.max_height, l.max_fps), (1280, 720, 15));
    use sew_proto::voice::{MediaPolicy, MediaTier, TrackHint, VideoCodec};
    let hint = |max_width, max_height, max_fps| TrackHint {
        max_width,
        max_height,
        max_fps,
        layers: Vec::new(),
    };
    let mut p = MediaPolicy {
        tier: MediaTier::Normal,
        video_enabled: true,
        e2ee: "required".to_owned(),
        audio_codecs: vec!["opus".to_owned()],
        codecs: vec![VideoCodec::Vp8],
        backup_codec: false,
        vp9: None,
        camera: hint(1280, 720, 30),
        screen: hint(1920, 1080, 60),
        dynacast: true,
        adaptive_stream: true,
        camera_allowed: true,
        screen_allowed: true,
        relay_only: false,
    };
    let l = layout_for(Some(&p));
    assert_eq!((l.max_width, l.max_height, l.max_fps), (1280, 720, 30));
    p.screen.max_width = 853;
    p.screen.max_height = 480;
    p.screen.max_fps = 5;
    let l = layout_for(Some(&p));
    assert_eq!((l.max_width, l.max_height, l.max_fps), (852, 480, 5));
    assert!(l.validate().is_ok());
}

#[test]
fn timestamps_never_go_backwards_and_are_not_believed_far_off() {
    let dir = Dir::new("clock");
    let mut pump = RingPump::create(dir.ring(), small()).unwrap();
    let now = kessel_frame_ring::monotonic_ns();
    let a = pump.capture_us(now, false);
    // A timestamp from the future, then one from long ago, then garbage.
    let b = pump.capture_us(now + 10_000_000_000, false);
    let c = pump.capture_us(1, false);
    let d = pump.capture_us(u64::MAX, false);
    assert!(a < b && b < c && c < d, "{a} {b} {c} {d}");
    assert!(d <= sew_call_client::kessel_video::now_us() + 4);
}
