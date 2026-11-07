//! The real engine's video receive behind the plugin's video seam: `sew-call-client`'s
//! receive side (its `video-core`) in the plugin's shapes.
//!
//! - **Frames.** `Call::video_frames()` is mapped item by item onto the plugin's
//!   [`VideoRecvItem`]s, the engine's track ids turned into the tracks' sids. The plugin's
//!   channel is bounded: when the plugin falls behind, the mapping task waits, and the engine's
//!   own queue drops the frame and breaks the track (a reset, then a keyframe), so no frame is
//!   lost silently.
//! - **Tracks.** The `video` list is the engine snapshot's remote cameras and screens, sent
//!   again on every `VideoAdded`, `VideoRemoved` and `VideoChanged`.
//! - **Settings.** The plugin hands each remote track the union of its sinks
//!   ([`TrackSettings`]); the adapter keeps one engine sink per track for it (opened on the
//!   first enabled settings, updated, closed when disabled), so the engine sees what the
//!   plugin's sinks want. Keyframe requests go to that sink.
//! - **Local video.** This device's camera and screen state (`local_video`), folded from the
//!   engine's `LocalVideo` events and the sources' pauses; publishing itself is
//!   `real/publish.rs`.
//! - **Self-view.** While this device publishes, the bridge's self-view (the camera's `h`
//!   layer, the screen's `q` layer: the encoder's own plaintext, no network round trip) comes
//!   into the same channel as [`LOCAL_CAMERA`] / [`LOCAL_SCREEN`] frames (`map_frames`); a
//!   publication's end is a `Removed` of its local track.
//! - **Not yet.** The sinks' decoder counters are kept here but not yet reported by the stats
//!   beacon.

use std::collections::HashMap;
use std::sync::{Mutex, Weak};

use sew_call_client::{
    Call, VideoCodec as EngineCodec, VideoPause, VideoRecvItem as EngineItem, VideoSink,
    VideoSinkSpec,
};
use tokio::sync::mpsc;

use super::{CallShared, lock};
use crate::engine::VideoSnapshot;
use crate::video::{
    EncodedVideoFrame, LOCAL_CAMERA, LOCAL_SCREEN, TrackSettings, VideoCodec, VideoFrames,
    VideoRecvItem, is_local_track,
};
use crate::wire::{LocalVideo, LocalVideoState, SinkStats, VideoPaused, VideoSource, VideoTrack};

/// Items the plugin's frame channel holds.
pub(super) const FRAME_QUEUE: usize = 64;

/// The bridge's self-view channel (`video-core`).
#[cfg(feature = "video-core")]
pub(super) type SelfView = sew_call_client::SelfViewFrames;
#[cfg(feature = "video-core")]
type SelfViewItem = sew_call_client::SelfViewItem;
/// Without `video-core` there is no publish and so no self-view.
#[cfg(not(feature = "video-core"))]
pub(super) type SelfView = mpsc::Receiver<std::convert::Infallible>;
#[cfg(not(feature = "video-core"))]
type SelfViewItem = std::convert::Infallible;

/// One call's video state on the plugin's side.
#[derive(Default)]
pub(super) struct CallVideo {
    /// One engine sink per remote track the plugin's sinks want.
    sinks: Mutex<HashMap<String, VideoSink>>,
    /// The latest decoder counters of each track's sinks.
    stats: Mutex<HashMap<String, SinkStats>>,
    /// The plugin's frame channel, until the plugin takes it.
    frames: Mutex<Option<VideoFrames>>,
    /// This device's camera and screen (the `local_video` event).
    local: Mutex<LocalVideo>,
    /// The camera is paused by the background (`real/scene.rs`), and the reason of its next
    /// "off" (the background's time limit).
    camera_scene: Mutex<(bool, Option<&'static str>)>,
}

/// `local_video`'s reason for the camera paused, or turned off, by the background.
pub(super) const BACKGROUND: &str = "background";

/// The engine's `LocalVideo` folded into the plugin's `local_video`.
pub(super) fn fold_local(local: &mut LocalVideo, ev: &sew_call_client::LocalVideo) {
    let state = if ev.on {
        LocalVideoState::On
    } else {
        LocalVideoState::Off
    };
    match ev.source {
        sew_call_client::VideoSource::Camera => local.camera = state,
        _ => local.screen = state,
    }
    local.reason = ev.reason.clone();
}

/// The plugin's codec of the engine's.
fn codec(c: EngineCodec) -> Option<VideoCodec> {
    match c {
        EngineCodec::Vp8 => Some(VideoCodec::Vp8),
        EngineCodec::H264 => Some(VideoCodec::H264),
        _ => None,
    }
}

/// The remote cameras and screens of an engine snapshot, as the `video` event lists them. A
/// publisher whose `TrackInfo` names no codec (none of Sew's do that) is listed as VP8 until
/// its first frame; each frame's header carries its own codec.
pub(super) fn tracks(snap: &sew_call_client::CallSnapshot) -> Vec<VideoTrack> {
    let mut out = Vec::new();
    for p in snap.participants.iter().filter(|p| !p.is_self) {
        for (view, source) in [
            (&p.camera, VideoSource::Camera),
            (&p.screen, VideoSource::Screen),
        ] {
            let Some(v) = view else { continue };
            out.push(VideoTrack {
                member_id: p.member_id.clone().unwrap_or_default(),
                is_self: false,
                source,
                track: v.track_sid.clone(),
                codec: v.codec.and_then(codec).unwrap_or(VideoCodec::Vp8),
                width: v.width,
                height: v.height,
                muted: v.muted,
                paused: match v.paused {
                    Some(VideoPause::Congestion) => Some(VideoPaused::Congestion),
                    Some(_) => Some(VideoPaused::Congestion),
                    None => None,
                },
                unsupported: false,
            });
        }
    }
    out
}

/// The engine's track ids → sids, from the snapshot.
fn track_ids(snap: &sew_call_client::CallSnapshot) -> HashMap<u64, String> {
    snap.participants
        .iter()
        .flat_map(|p| [&p.camera, &p.screen])
        .flatten()
        .map(|v| (v.id, v.track_sid.clone()))
        .collect()
}

impl CallVideo {
    /// Starts mapping the call's frames (`rx`, its `Call::video_frames()`) onto a channel the
    /// plugin takes with [`CallVideo::take_frames`].
    /// `self_view` is the bridge's self-view, merged into the same channel.
    pub(super) fn start(
        &self,
        rx: Option<sew_call_client::VideoFrameStream>,
        self_view: Option<SelfView>,
        shared: Weak<CallShared>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let rx = rx?;
        let (tx, plugin_rx) = mpsc::channel(FRAME_QUEUE);
        *lock(&self.frames) = Some(plugin_rx);
        Some(tokio::spawn(map_frames(rx, self_view, tx, shared)))
    }

    /// This device's camera and screen now.
    pub(super) fn local(&self) -> LocalVideo {
        lock(&self.local).clone()
    }

    /// The engine said this device's camera or screen changed: the `local_video` now, when it
    /// changed (a source that ended: the engine's reason is the source's, `user_stopped` …).
    pub(super) fn local_changed(&self, ev: &sew_call_client::LocalVideo) -> Option<LocalVideo> {
        let mut l = lock(&self.local);
        let before = l.clone();
        fold_local(&mut l, ev);
        if ev.source == sew_call_client::VideoSource::Camera {
            let mut sc = lock(&self.camera_scene);
            if ev.on && sc.0 {
                // Published again (a republish, a reconnect) while paused: still paused.
                l.camera = LocalVideoState::Paused;
                l.reason = Some(BACKGROUND.to_owned());
            } else if !ev.on {
                sc.0 = false;
                if let Some(r) = sc.1.take() {
                    l.reason = Some(r.to_owned());
                }
            }
        }
        (*l != before).then(|| l.clone())
    }

    /// A published source paused or came back (`video-core`): the `local_video` now, when it
    /// changed.
    #[cfg(feature = "video-core")]
    pub(super) fn source_changed(
        &self,
        kind: sew_call_client::CaptureKind,
        ev: &sew_call_client::CaptureEvent,
    ) -> Option<LocalVideo> {
        let mut l = lock(&self.local);
        let next = super::capture::local_video_after(kind, ev, &l)?;
        *l = next.clone();
        Some(next)
    }

    /// The background paused (`true`) or resumed the camera: the `local_video` now.
    #[cfg_attr(not(feature = "video-core"), allow(dead_code))]
    pub(super) fn set_camera_paused(&self, paused: bool) -> LocalVideo {
        let mut l = lock(&self.local);
        lock(&self.camera_scene).0 =
            paused && matches!(l.camera, LocalVideoState::On | LocalVideoState::Paused);
        match (paused, l.camera) {
            (true, LocalVideoState::On) => {
                l.camera = LocalVideoState::Paused;
                l.reason = Some(BACKGROUND.to_owned());
            }
            (false, LocalVideoState::Paused) => {
                l.camera = LocalVideoState::On;
                l.reason = None;
            }
            _ => {}
        }
        l.clone()
    }

    /// The camera's next "off" is the background's time limit.
    #[cfg_attr(not(feature = "video-core"), allow(dead_code))]
    pub(super) fn background_off(&self) {
        lock(&self.camera_scene).1 = Some(BACKGROUND);
    }

    pub(super) fn take_frames(&self) -> Option<VideoFrames> {
        lock(&self.frames).take()
    }

    /// The union of `track`'s sinks, from the plugin: the engine's sink for the track follows.
    pub(super) fn set_settings(&self, call: Option<&Call>, track: &str, s: TrackSettings) {
        if is_local_track(track) {
            // Self-view has no remote track: its frames are the publication's own.
            return;
        }
        let mut sinks = lock(&self.sinks);
        if s.disabled {
            // Dropping the engine's sink closes it: with no sink left the track is disabled.
            sinks.remove(track);
            return;
        }
        let spec = VideoSinkSpec::new(s.width, s.height, s.fps >= crate::video::Tile::Large.fps());
        match sinks.get(track) {
            Some(sink) => sink.update(spec),
            None => {
                if let Some(call) = call {
                    sinks.insert(track.to_owned(), call.video_sink(track, spec));
                }
            }
        }
    }

    /// A sink of `track` needs a keyframe.
    pub(super) fn request_keyframe(&self, track: &str) {
        if let Some(sink) = lock(&self.sinks).get(track) {
            sink.request_keyframe();
        }
    }

    pub(super) fn sink_stats(&self, track: &str, stats: SinkStats) {
        lock(&self.stats).insert(track.to_owned(), stats);
    }

    /// The track is gone: its engine sink and counters too.
    pub(super) fn forget(&self, live: &[VideoTrack]) {
        lock(&self.sinks).retain(|t, _| live.iter().any(|v| v.track == *t));
        lock(&self.stats).retain(|t, _| live.iter().any(|v| v.track == *t));
    }

    /// The call is over: every engine sink closes.
    pub(super) fn clear(&self) {
        lock(&self.sinks).clear();
        lock(&self.frames).take();
    }
}

/// The video now, for the plugin.
pub(super) fn snapshot(snap: Option<&sew_call_client::CallSnapshot>) -> VideoSnapshot {
    VideoSnapshot {
        tracks: snap.map(tracks).unwrap_or_default(),
        ..VideoSnapshot::default()
    }
}

/// The next self-view item; never while there is no self-view (or it closed).
async fn next_local(sv: &mut Option<SelfView>) -> SelfViewItem {
    if let Some(rx) = sv.as_mut()
        && let Some(item) = rx.recv().await
    {
        return item;
    }
    *sv = None;
    std::future::pending().await
}

/// A self-view item as the plugin's local-track item (90 kHz from the capture clock: a
/// self-view frame never meets RTP).
#[cfg(feature = "video-core")]
fn local_item(item: SelfViewItem) -> Option<VideoRecvItem> {
    let track = |s: sew_call_client::VideoSource| match s {
        sew_call_client::VideoSource::Camera => LOCAL_CAMERA,
        _ => LOCAL_SCREEN,
    };
    Some(match item {
        SelfViewItem::Frame { source, frame } => VideoRecvItem::Frame(EncodedVideoFrame {
            track: track(source).into(),
            codec: codec(frame.codec)?,
            data: frame.data.to_vec(),
            keyframe: frame.keyframe,
            rtp_ts: (frame.capture_us.wrapping_mul(9) / 100) as u32,
            arrival_us: frame.capture_us,
            width: frame.width,
            height: frame.height,
        }),
        SelfViewItem::Ended { source } => VideoRecvItem::Removed {
            track: track(source).into(),
        },
        _ => return None,
    })
}

#[cfg(not(feature = "video-core"))]
fn local_item(item: SelfViewItem) -> Option<VideoRecvItem> {
    let _ = (LOCAL_CAMERA, LOCAL_SCREEN);
    match item {}
}

/// The facing of the camera that captured a self-view camera frame.
#[cfg(feature = "video-core")]
fn local_facing(item: &SelfViewItem) -> Option<crate::wire::Facing> {
    match item {
        SelfViewItem::Frame {
            source: sew_call_client::VideoSource::Camera,
            frame,
        } => Some(super::capture::facing(frame.facing)),
        _ => None,
    }
}

#[cfg(not(feature = "video-core"))]
fn local_facing(item: &SelfViewItem) -> Option<crate::wire::Facing> {
    match *item {}
}

/// Tracks the self-view camera's facing: the [`VideoRecvItem::Facing`] to send before
/// `item` when its camera faces another way than the last frame's (or it is the first).
pub(super) fn facing_change(
    last: &mut Option<crate::wire::Facing>,
    facing: Option<crate::wire::Facing>,
    item: &VideoRecvItem,
) -> Option<VideoRecvItem> {
    if let VideoRecvItem::Removed { track } = item
        && track == LOCAL_CAMERA
    {
        *last = None;
        return None;
    }
    let f = facing?;
    if *last == Some(f) {
        return None;
    }
    *last = Some(f);
    Some(VideoRecvItem::Facing {
        track: LOCAL_CAMERA.into(),
        facing: f,
    })
}

/// The engine's frame items, and this device's self-view, in the plugin's shapes, until
/// either side closes.
async fn map_frames(
    mut rx: sew_call_client::VideoFrameStream,
    mut self_view: Option<SelfView>,
    tx: mpsc::Sender<VideoRecvItem>,
    shared: Weak<CallShared>,
) {
    let mut ids: HashMap<u64, String> = HashMap::new();
    let mut camera_facing = None;
    loop {
        let item = tokio::select! {
            item = rx.recv() => match item {
                Some(item) => item,
                None => return,
            },
            local = next_local(&mut self_view) => {
                let facing = local_facing(&local);
                if let Some(out) = local_item(local) {
                    if let Some(change) = facing_change(&mut camera_facing, facing, &out)
                        && tx.send(change).await.is_err()
                    {
                        return;
                    }
                    if tx.send(out).await.is_err() {
                        return;
                    }
                }
                continue;
            }
        };
        let id = match &item {
            EngineItem::Frame(f) => f.track,
            EngineItem::Added { track, .. }
            | EngineItem::Removed { track }
            | EngineItem::Broken { track }
            | EngineItem::KeyframeNeeded { track } => *track,
            _ => continue,
        };
        if !ids.contains_key(&id) {
            // A track the map doesn't know yet: the engine's snapshot names it.
            let Some(sh) = shared.upgrade() else { return };
            if let Some(snap) = sh.engine_snapshot() {
                ids.extend(track_ids(&snap));
            }
        }
        let Some(sid) = ids.get(&id).cloned() else {
            continue;
        };
        let out = match item {
            EngineItem::Frame(f) => {
                let Some(c) = codec(f.codec) else { continue };
                VideoRecvItem::Frame(EncodedVideoFrame {
                    track: sid,
                    codec: c,
                    data: f.data.to_vec(),
                    keyframe: f.keyframe,
                    rtp_ts: f.rtp_ts,
                    arrival_us: f.arrival_us,
                    width: f.width.unwrap_or(0),
                    height: f.height.unwrap_or(0),
                })
            }
            EngineItem::Added { .. } => VideoRecvItem::Added { track: sid },
            EngineItem::Removed { .. } => VideoRecvItem::Removed { track: sid },
            EngineItem::Broken { .. } => VideoRecvItem::Broken { track: sid },
            EngineItem::KeyframeNeeded { .. } => VideoRecvItem::KeyframeNeeded { track: sid },
            _ => continue,
        };
        if tx.send(out).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_map_onto_the_engines_sink_spec() {
        // Large tiles are 30 fps: the engine's `large`.
        assert_eq!(crate::video::Tile::Large.fps(), 30);
        assert_eq!(crate::video::Tile::Grid.fps(), 15);
        let v = CallVideo::default();
        // Without a call nothing opens, and a disable of an unknown track is a no-op.
        v.set_settings(
            None,
            "TR_a",
            TrackSettings {
                disabled: false,
                width: 640,
                height: 360,
                fps: 15,
            },
        );
        assert!(lock(&v.sinks).is_empty());
        v.set_settings(None, "TR_a", TrackSettings::DISABLED);
        v.set_settings(None, crate::video::LOCAL_CAMERA, TrackSettings::DISABLED);
        v.request_keyframe("TR_a");
        assert!(v.take_frames().is_none());
    }

    #[test]
    fn engine_events_fold_into_local_video() {
        use sew_call_client::VideoSource as S;
        let v = CallVideo::default();
        let l = v
            .local_changed(&sew_call_client::LocalVideo::new(S::Camera, true, None))
            .expect("changed");
        assert_eq!(l.camera, LocalVideoState::On);
        assert_eq!(l.screen, LocalVideoState::Off);
        let l = v
            .local_changed(&sew_call_client::LocalVideo::new(
                S::Camera,
                false,
                Some("video_disabled".to_owned()),
            ))
            .expect("changed");
        assert_eq!(l.camera, LocalVideoState::Off);
        assert_eq!(l.reason.as_deref(), Some("video_disabled"));
        let l = v
            .local_changed(&sew_call_client::LocalVideo::new(
                S::ScreenShare,
                true,
                None,
            ))
            .expect("changed");
        assert_eq!(l.screen, LocalVideoState::On);
        assert_eq!(v.local(), l);
        // The same again: nothing to tell.
        assert_eq!(
            v.local_changed(&sew_call_client::LocalVideo::new(
                S::ScreenShare,
                true,
                None
            )),
            None
        );
    }

    #[test]
    fn the_background_pause_folds_into_local_video() {
        use sew_call_client::VideoSource as S;
        let v = CallVideo::default();
        let on = sew_call_client::LocalVideo::new(S::Camera, true, None);
        assert!(v.local_changed(&on).is_some());
        let l = v.set_camera_paused(true);
        assert_eq!(l.camera, LocalVideoState::Paused);
        assert_eq!(l.reason.as_deref(), Some(BACKGROUND));
        // Published again while paused (a reconnect): still paused.
        assert_eq!(v.local_changed(&on), None);
        assert_eq!(v.local().camera, LocalVideoState::Paused);
        let l = v.set_camera_paused(false);
        assert_eq!((l.camera, l.reason), (LocalVideoState::On, None));
        // The time limit: off with the background's reason, once.
        v.set_camera_paused(true);
        v.background_off();
        let off = sew_call_client::LocalVideo::new(S::Camera, false, Some("unpublished".into()));
        let l = v.local_changed(&off).expect("changed");
        assert_eq!(l.camera, LocalVideoState::Off);
        assert_eq!(l.reason.as_deref(), Some(BACKGROUND));
        // Then on again by hand: on, not paused.
        let l = v.local_changed(&on).expect("changed");
        assert_eq!((l.camera, l.reason), (LocalVideoState::On, None));
        let l = v.local_changed(&off).expect("changed");
        assert_eq!(l.reason.as_deref(), Some("unpublished"));
        // A pause with the camera off changes nothing.
        assert_eq!(v.set_camera_paused(true).camera, LocalVideoState::Off);
    }

    #[test]
    fn codecs_map() {
        assert_eq!(codec(EngineCodec::Vp8), Some(VideoCodec::Vp8));
        assert_eq!(codec(EngineCodec::H264), Some(VideoCodec::H264));
    }
}
