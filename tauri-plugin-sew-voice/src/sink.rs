//! The frame sinks: one per `video_sink`, each a channel to one webview element, and the gating
//! that decides which frame goes where.
//!
//! Gating (in Rust, before anything crosses the IPC bridge):
//! - after a sink opens, becomes visible, asks for a keyframe, or its track breaks (a gap, a
//!   decryption failure), the sink sends nothing until the next keyframe, which goes out with
//!   the discontinuity flag;
//! - a broken or removed track sends each visible sink a `reset` message first;
//! - a hidden sink sends nothing;
//! - a frame for a track with no sink is dropped (the engine has the track disabled anyway);
//! - in the background (phones) every webview sink counts as hidden; only native sinks (the
//!   picture in picture) stay enabled, and on return the visible sinks wait for a keyframe.
//!
//! A sink goes either to a webview element (a [`FrameChannel`], the framed bytes) or to a
//! native consumer in Rust (a [`NativeTarget`], the frames themselves: the picture in picture).
//!
//! **Decoded mode** (the fallback for a webview that can't decode the track's codec): the sink
//! owns one of the engine's decoders ([`crate::VoiceEngine::video_decoder`]). The same gating
//! decides which frames reach the decoder (a keyframe after any break, with the decoder reset
//! first; nothing while hidden); its outputs, scaled to fit the sink and capped at 640x360 and
//! 15 fps, go out as I420 (kind 2) or RGBA (kind 3) messages. A decoder error resets it, sends
//! the sink a `reset` message and asks the sender for a keyframe. Frames the decoder has no room
//! for (`Busy`) wait in order in a backlog of [`DECODED_BACKLOG`] and go in as its outputs make
//! room; a keyframe supersedes the deltas waiting before it; an overflow drops them, and the
//! sink waits for a keyframe (asked of the sender), as the native sinks do.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use tokio::sync::Notify;

use crate::engine::EngineError;
use crate::video::{
    DECODED_MIN_INTERVAL_US, DecodeFailure, DecodedFormat, DecoderSpec, EncodedVideoFrame,
    FrameDecoder, FrameHeader, Tile, TrackSettings, VideoCodec, VideoRecvItem, decoded_fit, flag,
    header_dim, is_local_track, kind,
};
use crate::wire::SinkStats;

/// Opens a decoder for a decoded sink (the engine's `video_decoder`, with the sink's wake).
pub(crate) type DecoderOpener =
    Arc<dyn Fn(DecoderSpec) -> Result<Box<dyn FrameDecoder>, EngineError> + Send + Sync>;

/// A decoded sink's decoder and its counters.
pub(crate) struct Decoded {
    format: DecodedFormat,
    open: DecoderOpener,
    /// The decoder and the codec it decodes (`None` until a keyframe of a codec it can open).
    dec: Option<(VideoCodec, Box<dyn FrameDecoder>)>,
    /// The last frame sent's timestamp (the 15 fps cap).
    last_out: Option<u64>,
    /// The next output follows a reset (it goes out with the discontinuity flag).
    fresh: bool,
    /// The native decoder's counters (a decoded sink's stats).
    stats: SinkStats,
    /// Stops the sink's drain task (notified when the sink goes).
    stop: Arc<Notify>,
    /// Frames the decoder had no room for (`Busy`), in order: they go in as its outputs make
    /// room (at most [`DECODED_BACKLOG`]).
    backlog: VecDeque<EncodedVideoFrame>,
}

/// The most frames a decoded sink keeps waiting for room in its decoder; one more drops them
/// all and the sink waits for a keyframe (the same rule as `crate::native`'s backlog).
pub(crate) const DECODED_BACKLOG: usize = 8;

impl Decoded {
    /// A decoded sink's state: `format` out, `dec` already open (a remote track's) or opened by
    /// `open` on the first keyframe (a self-view sink's).
    pub(crate) fn new(
        format: DecodedFormat,
        open: DecoderOpener,
        dec: Option<(VideoCodec, Box<dyn FrameDecoder>)>,
        stop: Arc<Notify>,
    ) -> Decoded {
        Decoded {
            format,
            open,
            dec,
            last_out: None,
            fresh: true,
            stats: SinkStats::default(),
            stop,
            backlog: VecDeque::new(),
        }
    }
}

impl Drop for Decoded {
    fn drop(&mut self) {
        self.stop.notify_one();
    }
}

/// Where one sink's messages go: the webview's Tauri raw `Channel` in the app, a recorder in
/// tests. `send` must not block; it answers false once the channel is gone (the sink is then
/// closed).
pub trait FrameChannel: Send + Sync + 'static {
    /// Deliver one binary message (a [`FrameHeader`] and its payload).
    fn send(&self, message: Vec<u8>) -> bool;
}

#[cfg_attr(not(feature = "video-core"), allow(dead_code))]
/// What a native sink's target answers to one push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pushed {
    /// Taken.
    Sent,
    /// Its queue was full: the frame was dropped, so the sink waits for a keyframe.
    Dropped,
    /// The consumer is gone: the sink closes.
    Closed,
}

#[cfg_attr(not(feature = "video-core"), allow(dead_code))]
/// A native sink's consumer side (`crate::native`): frames in Rust, not over the bridge. Every
/// method must not block.
pub(crate) trait NativeTarget: Send + Sync + 'static {
    /// One frame (after the gating), with the discontinuity flag.
    fn push(&self, frame: &EncodedVideoFrame, discontinuity: bool) -> Pushed;
    /// The track broke or went away: drop the decoder state and wait for a keyframe.
    fn reset(&self, track: &str) -> Pushed;
    /// The sink now follows `track` (`None`: nothing to show), for an active-speaker sink.
    fn switched(&self, track: Option<&str>, member_id: Option<&str>) -> Pushed;
    /// The sink's new largest size.
    fn resize(&self, width: u32, height: u32);
}

#[cfg_attr(not(feature = "video-core"), allow(dead_code))]
/// Where one sink's frames go.
#[derive(Clone)]
enum Target {
    /// A webview element: the framed bytes.
    Webview(Arc<dyn FrameChannel>),
    /// A native consumer (the picture in picture).
    Native(Arc<dyn NativeTarget>),
}

/// One open sink.
struct Sink {
    /// The track (empty: a native active-speaker sink with nobody to show).
    track: String,
    target: Target,
    width: u32,
    height: u32,
    visible: bool,
    tile: Tile,
    /// The webview's WebCodecs probe: the codecs it decodes.
    decode: Vec<VideoCodec>,
    /// The next message's sequence.
    seq: u32,
    /// Nothing until a keyframe (then sent with the discontinuity flag).
    waiting: bool,
    /// The webview's counters (`video_sink_update`'s `stats`), the latest.
    stats: Option<SinkStats>,
    /// A native sink that follows the active speaker.
    follow: bool,
    /// Decoded mode's decoder (`None`: encoded mode; always `None` for a native sink).
    decoded: Option<Decoded>,
    /// Its track's camera facing (a self-view), as of the last frame routed to it.
    facing: FacingAt,
    /// A native sink that can't keep up asked for the low layer (`crate::native`'s step-down):
    /// its size counts as at most [`LOW_LAYER`].
    low: bool,
}

impl Sink {
    fn is_native(&self) -> bool {
        matches!(self.target, Target::Native(_))
    }
}

/// A change a sink's owner asks for (`video_sink_update`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SinkChange {
    pub(crate) width: Option<u32>,
    pub(crate) height: Option<u32>,
    pub(crate) visible: Option<bool>,
    pub(crate) tile: Option<Tile>,
}

/// What routing one item asks of the caller.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Routed {
    /// Sinks whose channel is gone (already removed), with their tracks.
    pub(crate) closed: Vec<(u32, String)>,
    /// Tracks that came back: hand the engine their settings again, then ask a keyframe.
    pub(crate) keyframes: Vec<String>,
    /// Tracks to ask a keyframe of without forcing their settings (a native sink dropped a
    /// frame and waits for the next keyframe).
    pub(crate) pli: Vec<String>,
    /// Tracks whose decoded sink lost its decoder's references (an error, a dropped input):
    /// ask a keyframe (nothing else changed).
    pub(crate) decoder_keyframes: Vec<String>,
}

/// Every open sink, and the settings last handed to the engine per remote track.
#[derive(Default)]
pub(crate) struct Sinks {
    next_id: u32,
    sinks: BTreeMap<u32, Sink>,
    /// What the engine was last told of each remote track (to send only changes).
    pushed: HashMap<String, TrackSettings>,
    /// The app is in the background: only native sinks count as visible.
    background: bool,
    /// The self-view tracks' camera facing ([`VideoRecvItem::Facing`]).
    facings: HashMap<String, FacingAt>,
}

/// A self-view track's camera facing: `now` for frames from `since_us` on (the first frame
/// after the change), `was` before (a decoder's late outputs).
#[derive(Debug, Clone, Copy, Default)]
struct FacingAt {
    now: u8,
    was: u8,
    since_us: Option<u64>,
}

impl FacingAt {
    /// The header's facing bits for a frame stamped `ts_us`.
    fn bits(&self, ts_us: u64) -> u8 {
        match self.since_us {
            Some(since) if ts_us < since => self.was,
            _ => self.now,
        }
    }
}

fn facing_bits(f: crate::wire::Facing) -> u8 {
    match f {
        crate::wire::Facing::Front => flag::FRONT,
        crate::wire::Facing::Back => flag::BACK,
        _ => 0,
    }
}

#[cfg_attr(not(feature = "video-core"), allow(dead_code))]
/// A native sink to open.
pub(crate) struct OpenNative {
    /// The track (empty while an active-speaker sink has nobody to show).
    pub(crate) track: String,
    pub(crate) target: Arc<dyn NativeTarget>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) decode: Vec<VideoCodec>,
    pub(crate) follow: bool,
}

pub(crate) struct OpenSink {
    pub(crate) track: String,
    pub(crate) channel: Arc<dyn FrameChannel>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) visible: bool,
    pub(crate) tile: Tile,
    pub(crate) decode: Vec<VideoCodec>,
    /// Decoded mode (`None`: encoded).
    pub(crate) decoded: Option<Decoded>,
}

impl Sinks {
    /// Opens a sink; it waits for a keyframe.
    pub(crate) fn open(&mut self, s: OpenSink) -> u32 {
        self.insert(Sink {
            track: s.track,
            target: Target::Webview(s.channel),
            width: s.width,
            height: s.height,
            visible: s.visible,
            tile: s.tile,
            decode: s.decode,
            seq: 0,
            waiting: true,
            stats: None,
            follow: false,
            decoded: s.decoded,
            facing: FacingAt::default(),
            low: false,
        })
    }

    #[cfg_attr(not(feature = "video-core"), allow(dead_code))]
    /// Opens a native sink (always visible, a large tile: 30 fps at its size); it waits for a
    /// keyframe.
    pub(crate) fn open_native(&mut self, s: OpenNative) -> u32 {
        self.insert(Sink {
            track: s.track,
            target: Target::Native(s.target),
            width: s.width,
            height: s.height,
            visible: true,
            tile: Tile::Large,
            decode: s.decode,
            seq: 0,
            waiting: true,
            stats: None,
            follow: s.follow,
            decoded: None,
            facing: FacingAt::default(),
            low: false,
        })
    }

    fn insert(&mut self, sink: Sink) -> u32 {
        // Ids start at 1 and are never reused while the plugin runs.
        self.next_id = self.next_id.wrapping_add(1).max(1);
        while self.sinks.contains_key(&self.next_id) {
            self.next_id = self.next_id.wrapping_add(1).max(1);
        }
        let id = self.next_id;
        self.sinks.insert(id, sink);
        id
    }

    /// How many sinks are in decoded mode.
    pub(crate) fn decoded_count(&self) -> usize {
        self.sinks.values().filter(|s| s.decoded.is_some()).count()
    }

    #[cfg_attr(not(feature = "video-core"), allow(dead_code))]
    /// The track of sink `id`.
    pub(crate) fn track_of(&self, id: u32) -> Option<String> {
        self.sinks.get(&id).map(|s| s.track.clone())
    }

    #[cfg_attr(not(feature = "video-core"), allow(dead_code))]
    /// A native sink's new size; answers its track.
    pub(crate) fn resize_native(&mut self, id: u32, width: u32, height: u32) -> Option<String> {
        let s = self.sinks.get_mut(&id).filter(|s| s.is_native())?;
        s.width = width;
        s.height = height;
        if let Target::Native(t) = &s.target {
            t.resize(width, height);
        }
        Some(s.track.clone())
    }

    #[cfg_attr(not(feature = "video-core"), allow(dead_code))]
    /// A native sink steps down to the low layer (`true`) or back up; answers its track when
    /// that changed anything.
    pub(crate) fn set_native_low(&mut self, id: u32, low: bool) -> Option<String> {
        let s = self.sinks.get_mut(&id).filter(|s| s.is_native())?;
        if s.low == low {
            return None;
        }
        s.low = low;
        Some(s.track.clone())
    }

    /// The active-speaker sinks now on another track than `track` (`""`: nobody to show).
    pub(crate) fn followers_off(&self, track: &str) -> Vec<u32> {
        self.sinks
            .iter()
            .filter(|(_, s)| s.follow && s.track != track)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Moves sink `id` to `track` (an active-speaker switch): it waits for a keyframe there and
    /// its consumer hears of the switch. Answers the old track; `None` when nothing changed
    /// (or the consumer is gone, and the sink with it: then `closed` says so).
    pub(crate) fn retarget(
        &mut self,
        id: u32,
        track: &str,
        member_id: Option<&str>,
        closed: &mut Vec<(u32, String)>,
    ) -> Option<String> {
        let s = self.sinks.get_mut(&id)?;
        if s.track == track {
            return None;
        }
        let old = std::mem::replace(&mut s.track, track.to_owned());
        s.waiting = true;
        if let Target::Native(t) = &s.target {
            let to = (!track.is_empty()).then_some(track);
            if t.switched(to, member_id) == Pushed::Closed {
                self.sinks.remove(&id);
                closed.push((id, track.to_owned()));
            }
        }
        Some(old)
    }

    /// The app went to the background (`true`) or came back. Answers every track with a sink
    /// (their settings may change) and, on return, the tracks whose visible sinks now wait for
    /// a keyframe. `None` when nothing changed.
    pub(crate) fn set_background(
        &mut self,
        background: bool,
    ) -> Option<(Vec<String>, Vec<String>)> {
        if self.background == background {
            return None;
        }
        self.background = background;
        let mut tracks: Vec<String> = self
            .sinks
            .values()
            .filter(|s| !s.track.is_empty())
            .map(|s| s.track.clone())
            .collect();
        tracks.sort();
        tracks.dedup();
        let mut keyframes = Vec::new();
        if !background {
            for s in self
                .sinks
                .values_mut()
                .filter(|s| s.visible && !s.is_native())
            {
                s.waiting = true;
                if !keyframes.contains(&s.track) {
                    keyframes.push(s.track.clone());
                }
            }
            keyframes.sort();
        }
        Some((tracks, keyframes))
    }

    /// Is the app in the background (as the plugin decided)?
    pub(crate) fn is_background(&self) -> bool {
        self.background
    }

    /// Whether `s` gets frames now: visible, and in the background only a native sink.
    fn shown(&self, s: &Sink) -> bool {
        s.visible && (!self.background || s.is_native())
    }

    /// Applies a change; answers the sink's track and whether it just became visible (it then
    /// waits for a keyframe).
    pub(crate) fn update(
        &mut self,
        id: u32,
        change: SinkChange,
        stats: Option<SinkStats>,
    ) -> Option<(String, bool)> {
        let background = self.background;
        // A native sink is not the webview's to change.
        let s = self.sinks.get_mut(&id).filter(|s| !s.is_native())?;
        if let Some(w) = change.width {
            s.width = w;
        }
        if let Some(h) = change.height {
            s.height = h;
        }
        if let Some(t) = change.tile {
            s.tile = t;
        }
        let shown = change.visible == Some(true) && !s.visible;
        if let Some(v) = change.visible {
            s.visible = v;
        }
        if shown {
            s.waiting = true;
        }
        if stats.is_some() {
            s.stats = stats;
        }
        if (change.width.is_some() || change.height.is_some())
            && let Some((_, dec)) = s.decoded.as_mut().and_then(|d| d.dec.as_mut())
        {
            dec.set_fit(decoded_fit(s.width, s.height));
        }
        // In the background a revealed sink waits for the return (which asks the keyframe).
        Some((s.track.clone(), shown && !background))
    }

    /// Closes a sink (`native`: a native sink, else a webview one); answers its track.
    pub(crate) fn close(&mut self, id: u32, native: bool) -> Option<String> {
        if self.sinks.get(&id)?.is_native() != native {
            return None;
        }
        self.sinks.remove(&id).map(|s| s.track)
    }

    /// The sink waits for the next keyframe (`native`: a native sink, else a webview one);
    /// answers its track.
    pub(crate) fn want_keyframe(&mut self, id: u32, native: bool) -> Option<String> {
        let s = self
            .sinks
            .get_mut(&id)
            .filter(|s| s.is_native() == native)?;
        s.waiting = true;
        Some(s.track.clone())
    }

    /// The union of `track`'s sinks.
    pub(crate) fn settings(&self, track: &str) -> TrackSettings {
        let mut out = TrackSettings::DISABLED;
        if track.is_empty() {
            return out;
        }
        for s in self
            .sinks
            .values()
            .filter(|s| s.track == track && self.shown(s))
        {
            out.disabled = false;
            let (w, h) = if s.low {
                low_size(s.width, s.height)
            } else {
                (s.width, s.height)
            };
            out.width = out.width.max(w);
            out.height = out.height.max(h);
            out.fps = out.fps.max(s.tile.fps());
        }
        out
    }

    /// The settings to hand the engine for `track` when they changed since the last time
    /// (never for a local track: the engine's own encoder feeds those).
    pub(crate) fn changed(&mut self, track: &str) -> Option<TrackSettings> {
        if is_local_track(track) || track.is_empty() {
            return None;
        }
        let now = self.settings(track);
        match self.pushed.get(track) {
            Some(was) if *was == now => None,
            // A track the engine was never told of starts disabled there.
            None if now == TrackSettings::DISABLED => None,
            _ => {
                self.pushed.insert(track.to_owned(), now);
                Some(now)
            }
        }
    }

    /// The settings to hand the engine for `track` now, whatever it was told before (a track
    /// that came back, a new call).
    pub(crate) fn force(&mut self, track: &str) -> Option<TrackSettings> {
        if is_local_track(track) {
            return None;
        }
        self.pushed.remove(track);
        self.changed(track)
    }

    /// The engine forgot its tracks (a new call): forget what it was told.
    pub(crate) fn forget_pushed(&mut self) {
        self.pushed.clear();
    }

    /// The latest webview counters of `track`'s sinks, summed.
    pub(crate) fn stats(&self, track: &str) -> SinkStats {
        let mut out = SinkStats::default();
        // A decoded sink's counters are the native decoder's; an encoded sink's the webview's.
        for st in self
            .sinks
            .values()
            .filter(|s| s.track == track)
            .filter_map(|s| s.decoded.as_ref().map(|d| d.stats).or(s.stats))
        {
            out.decoded += st.decoded;
            out.dropped += st.dropped;
            out.freezes += st.freezes;
            out.freeze_ms += st.freeze_ms;
        }
        out
    }

    /// Routes one engine item to the sinks.
    pub(crate) fn route(&mut self, item: VideoRecvItem) -> Routed {
        let mut r = Routed::default();
        match item {
            VideoRecvItem::Frame(f) => self.frame(&f, &mut r),
            VideoRecvItem::Broken { track } | VideoRecvItem::Removed { track } => {
                let background = self.background;
                for (id, s) in self.sinks.iter_mut().filter(|(_, s)| s.track == track) {
                    s.waiting = true;
                    let ok = match &s.target {
                        Target::Native(t) => t.reset(&track) != Pushed::Closed,
                        Target::Webview(_) if s.visible && !background => {
                            send(s, reset_header(*id), &[])
                        }
                        Target::Webview(_) => true,
                    };
                    if !ok {
                        r.closed.push((*id, s.track.clone()));
                    }
                }
            }
            VideoRecvItem::KeyframeNeeded { track } => {
                for s in self.sinks.values_mut().filter(|s| s.track == track) {
                    s.waiting = true;
                }
            }
            VideoRecvItem::Facing { track, facing } => {
                let at = self.facings.entry(track).or_default();
                let bits = facing_bits(facing);
                if bits != at.now || at.since_us.is_none() {
                    *at = FacingAt {
                        now: bits,
                        was: at.now,
                        // Set by the next frame of the track.
                        since_us: None,
                    };
                }
            }
            VideoRecvItem::Added { track } => {
                // A track with sinks already (it came back): they need a keyframe.
                let mut any = false;
                for s in self.sinks.values_mut().filter(|s| s.track == track) {
                    s.waiting = true;
                    any = true;
                }
                if any {
                    r.keyframes.push(track);
                }
            }
        }
        for (id, _) in &r.closed {
            self.sinks.remove(id);
        }
        r
    }

    /// Sends a decoded sink what its decoder has ready (its drain task, on the decoder's
    /// wake). `None` when the sink is gone or not decoded.
    pub(crate) fn drain_decoded(&mut self, id: u32) -> Option<Routed> {
        let background = self.background;
        let s = self.sinks.get_mut(&id)?;
        let mut d = s.decoded.take()?;
        let mut r = Routed::default();
        // In the background a webview sink is hidden: its outputs are dropped.
        let shown = s.visible && !background;
        drain(id, s, &mut d, &mut r, shown);
        s.decoded = Some(d);
        for (id, _) in &r.closed {
            self.sinks.remove(id);
        }
        Some(r)
    }

    /// A frame for every sink of its track that can take it (a track with no sink: dropped).
    fn frame(&mut self, f: &EncodedVideoFrame, r: &mut Routed) {
        let background = self.background;
        let facing = match self.facings.get_mut(&f.track) {
            Some(at) if is_local_track(&f.track) => {
                at.since_us.get_or_insert(f.arrival_us);
                *at
            }
            _ => FacingAt::default(),
        };
        for (id, s) in self.sinks.iter_mut().filter(|(_, s)| s.track == f.track) {
            s.facing = facing;
            let shown = s.visible && (!background || s.is_native());
            if !shown || (s.waiting && !f.keyframe) {
                continue;
            }
            if let Some(mut d) = s.decoded.take() {
                decode(*id, s, &mut d, f, r);
                s.decoded = Some(d);
                continue;
            }
            // The webview can't decode this codec (a self-view sink whose camera changed
            // codec; a remote one is refused at open or decoded): the sink gets nothing.
            if !s.decode.contains(&f.codec) {
                continue;
            }
            if let Target::Native(t) = &s.target {
                match t.push(f, s.waiting) {
                    Pushed::Sent => s.waiting = false,
                    Pushed::Dropped => {
                        s.waiting = true;
                        if !r.pli.contains(&s.track) {
                            r.pli.push(s.track.clone());
                        }
                    }
                    Pushed::Closed => r.closed.push((*id, s.track.clone())),
                }
                continue;
            }
            let mut flags = 0;
            if f.keyframe {
                flags |= flag::KEYFRAME;
            }
            if s.waiting {
                flags |= flag::DISCONTINUITY;
            }
            if is_local_track(&f.track) {
                flags |= flag::SELF_VIEW | facing.bits(f.arrival_us);
            }
            let h = FrameHeader {
                kind: kind::ENCODED,
                codec: f.codec.header_code(),
                flags,
                sink_id: *id,
                timestamp_us: f.arrival_us,
                width: header_dim(f.width),
                height: header_dim(f.height),
                seq: 0,
                len: 0,
            };
            if send(s, h, &f.data) {
                s.waiting = false;
            } else {
                r.closed.push((*id, s.track.clone()));
            }
        }
    }
}

/// The box a stepped-down native sink asks the media server for: the 180p layer.
pub(crate) const LOW_LAYER: (u32, u32) = (320, 180);

/// A size stepped down to fit [`LOW_LAYER`] (aspect kept, even, at least 2).
fn low_size(width: u32, height: u32) -> (u32, u32) {
    let (lw, lh) = LOW_LAYER;
    if width <= lw && height <= lh {
        return (width, height);
    }
    let s = (f64::from(lw) / f64::from(width.max(1))).min(f64::from(lh) / f64::from(height.max(1)));
    let even = |v: u32| ((f64::from(v) * s / 2.0).round() as u32 * 2).max(2);
    (even(width), even(height))
}

/// A frame into a decoded sink's decoder (the gating already passed it), then whatever the
/// decoder has ready out to the sink.
fn decode(id: u32, s: &mut Sink, d: &mut Decoded, f: &EncodedVideoFrame, r: &mut Routed) {
    // No decoder yet, or the track changed codec: open one, on a keyframe only.
    if d.dec.as_ref().map(|(c, _)| *c) != Some(f.codec) {
        d.dec = None;
        if !f.keyframe {
            s.waiting = true;
            d.stats.dropped += 1;
            return;
        }
        match (d.open)(DecoderSpec {
            codec: f.codec,
            format: d.format,
            fit: decoded_fit(s.width, s.height),
        }) {
            Ok(dec) => d.dec = Some((f.codec, dec)),
            Err(e) => {
                // The engine can't decode this codec: the sink stays black.
                tracing::debug!(sink = id, codec = f.codec.as_str(), error = %e, "no decoder");
                d.stats.dropped += 1;
                return;
            }
        }
        s.waiting = true;
    }
    let Some((_, dec)) = d.dec.as_mut() else {
        return;
    };
    if s.waiting {
        // The first frame after a break: the decoder starts over from this keyframe.
        dec.reset();
        d.backlog.clear();
        d.fresh = true;
        d.last_out = None;
        s.waiting = false;
    }
    if !d.backlog.is_empty() {
        if f.keyframe {
            // Nothing before a keyframe is needed to show it: the waiting deltas go.
            d.stats.dropped += d.backlog.len() as u64;
            d.backlog.clear();
            d.fresh = true;
            d.backlog.push_back(f.clone());
        } else if d.backlog.len() >= DECODED_BACKLOG {
            // Too far behind: the waiting frames and this one go; a keyframe starts over.
            let n = d.backlog.len() as u64 + 1;
            backlog_lost(s, d, r, n);
            return;
        } else {
            d.backlog.push_back(f.clone());
        }
    } else {
        submit(id, s, d, r, f);
    }
    // The gating passed the frame: the sink is shown.
    drain(id, s, d, r, true);
}

/// One frame into the decoder: `Busy` puts it at the back of the backlog. `false` when it
/// did not go in.
fn submit(id: u32, s: &mut Sink, d: &mut Decoded, r: &mut Routed, f: &EncodedVideoFrame) -> bool {
    let Some((_, dec)) = d.dec.as_mut() else {
        return false;
    };
    match dec.submit(f) {
        Ok(()) => true,
        Err(DecodeFailure::Busy) => {
            d.backlog.push_back(f.clone());
            false
        }
        Err(DecodeFailure::NeedKeyframe) => {
            // A dropped input leaves the decoder without its reference.
            s.waiting = true;
            d.backlog.clear();
            d.stats.dropped += 1;
            r.decoder_keyframes.push(s.track.clone());
            false
        }
        Err(DecodeFailure::Failed(why)) => {
            tracing::debug!(sink = id, %why, "decoder refused a frame");
            decoder_failed(id, s, d, r, true);
            false
        }
    }
}

/// The backlog into the decoder, in order, while it has room; `true` when any went in.
fn flush_backlog(id: u32, s: &mut Sink, d: &mut Decoded, r: &mut Routed) -> bool {
    let mut any = false;
    while !s.waiting {
        let Some(f) = d.backlog.pop_front() else {
            break;
        };
        if submit(id, s, d, r, &f) {
            any = true;
            continue;
        }
        // `submit` put a Busy frame at the back of what was left: back to the front.
        if let Some(again) = d.backlog.pop_back() {
            d.backlog.push_front(again);
        }
        break;
    }
    if s.waiting {
        d.backlog.clear();
    }
    any
}

/// `n` frames lost before the decoder (the backlog overflowed): the sink waits for a keyframe,
/// asked of the sender.
fn backlog_lost(s: &mut Sink, d: &mut Decoded, r: &mut Routed, n: u64) {
    tracing::debug!(track = %s.track, n, "a decoded sink's backlog overflowed");
    d.backlog.clear();
    d.stats.dropped += n;
    s.waiting = true;
    if !r.decoder_keyframes.contains(&s.track) {
        r.decoder_keyframes.push(s.track.clone());
    }
}

/// A decoder error: reset it, tell a shown sink (`reset`), and ask the sender for a keyframe.
fn decoder_failed(id: u32, s: &mut Sink, d: &mut Decoded, r: &mut Routed, shown: bool) {
    s.waiting = true;
    d.stats.dropped += 1;
    if let Some((_, dec)) = d.dec.as_mut() {
        dec.reset();
    }
    if shown && !send(s, reset_header(id), &[]) {
        r.closed.push((id, s.track.clone()));
    }
    r.decoder_keyframes.push(s.track.clone());
}

/// Every output a decoded sink's decoder has ready: sent (at most 15 per second), or dropped
/// while the sink is hidden (`shown` false: not visible, or in the background) or waits for a
/// keyframe.
fn drain(id: u32, s: &mut Sink, d: &mut Decoded, r: &mut Routed, shown: bool) {
    loop {
        let Some(out) = d.dec.as_mut().and_then(|(_, dec)| dec.poll()) else {
            // The outputs made room: the waiting frames go in, and their outputs follow.
            if flush_backlog(id, s, d, r) {
                continue;
            }
            return;
        };
        match out {
            Ok(frame) => {
                if !shown || s.waiting {
                    // Hidden, or from before a break: nobody wants it.
                    continue;
                }
                if frame.format != d.format
                    || frame.data.len() != frame.format.frame_len(frame.width, frame.height)
                {
                    d.stats.dropped += 1;
                    continue;
                }
                if let Some(last) = d.last_out
                    && frame.timestamp_us >= last
                    && frame.timestamp_us - last < DECODED_MIN_INTERVAL_US
                {
                    // Over the 15 fps cap: decoded (it is a reference) but not sent.
                    continue;
                }
                let mut flags = 0;
                if d.fresh {
                    flags |= flag::DISCONTINUITY;
                }
                if is_local_track(&s.track) {
                    flags |= flag::SELF_VIEW | s.facing.bits(frame.timestamp_us);
                }
                let h = FrameHeader {
                    kind: frame.format.header_kind(),
                    codec: 0,
                    flags,
                    sink_id: id,
                    timestamp_us: frame.timestamp_us,
                    width: header_dim(frame.width),
                    height: header_dim(frame.height),
                    seq: 0,
                    len: 0,
                };
                if send(s, h, &frame.data) {
                    d.fresh = false;
                    d.last_out = Some(frame.timestamp_us);
                    d.stats.decoded += 1;
                } else {
                    r.closed.push((id, s.track.clone()));
                    return;
                }
            }
            Err(DecodeFailure::NeedKeyframe) => {
                s.waiting = true;
                d.stats.dropped += 1;
                r.decoder_keyframes.push(s.track.clone());
            }
            Err(DecodeFailure::Busy) => {
                // Nothing more ready now (no input is lost: the backlog keeps them).
                flush_backlog(id, s, d, r);
                return;
            }
            Err(DecodeFailure::Failed(why)) => {
                tracing::debug!(sink = id, %why, "decoder failed");
                decoder_failed(id, s, d, r, shown);
            }
        }
    }
}

fn reset_header(sink_id: u32) -> FrameHeader {
    FrameHeader {
        kind: kind::RESET,
        codec: 0,
        flags: 0,
        sink_id,
        timestamp_us: 0,
        width: 0,
        height: 0,
        seq: 0,
        len: 0,
    }
}

/// Sends one message on the sink, with its next sequence number.
fn send(s: &mut Sink, mut h: FrameHeader, payload: &[u8]) -> bool {
    let Target::Webview(channel) = &s.target else {
        return true;
    };
    h.seq = s.seq;
    s.seq = s.seq.wrapping_add(1);
    channel.send(h.message(payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::DecodedFrame;

    struct Nowhere;

    impl NativeTarget for Nowhere {
        fn push(&self, _frame: &EncodedVideoFrame, _discontinuity: bool) -> Pushed {
            Pushed::Sent
        }
        fn reset(&self, _track: &str) -> Pushed {
            Pushed::Sent
        }
        fn switched(&self, _track: Option<&str>, _member_id: Option<&str>) -> Pushed {
            Pushed::Sent
        }
        fn resize(&self, _width: u32, _height: u32) {}
    }

    /// The scripted decoder's inputs (timestamp, keyframe) and the outputs it may give now.
    type Script = Arc<std::sync::Mutex<(VecDeque<(u64, bool)>, usize)>>;

    /// A decoder with room for `room` inputs; each `poll` answers the oldest as a 2x2 I420
    /// frame (its timestamp the input's) while `outputs` allows.
    struct Scripted {
        room: usize,
        queue: Script,
    }

    impl FrameDecoder for Scripted {
        fn submit(&mut self, f: &EncodedVideoFrame) -> Result<(), DecodeFailure> {
            let mut q = self.queue.lock().unwrap();
            if q.0.len() >= self.room {
                return Err(DecodeFailure::Busy);
            }
            q.0.push_back((f.arrival_us, f.keyframe));
            Ok(())
        }
        fn poll(&mut self) -> Option<Result<DecodedFrame, DecodeFailure>> {
            let mut q = self.queue.lock().unwrap();
            if q.1 == 0 {
                return None;
            }
            let (ts, _) = q.0.pop_front()?;
            q.1 -= 1;
            Some(Ok(DecodedFrame {
                format: DecodedFormat::I420,
                width: 2,
                height: 2,
                timestamp_us: ts,
                data: vec![0; DecodedFormat::I420.frame_len(2, 2)],
            }))
        }
        fn reset(&mut self) {
            self.queue.lock().unwrap().0.clear();
        }
        fn set_fit(&mut self, _fit: (u32, u32)) {}
    }

    struct Messages(std::sync::Mutex<Vec<Vec<u8>>>);

    impl FrameChannel for Messages {
        fn send(&self, message: Vec<u8>) -> bool {
            self.0.lock().unwrap().push(message);
            true
        }
    }

    fn vp8(n: u64, keyframe: bool) -> EncodedVideoFrame {
        EncodedVideoFrame {
            track: "TR_VCdec".into(),
            codec: VideoCodec::Vp8,
            data: vec![n as u8],
            keyframe,
            rtp_ts: n as u32 * 3000,
            // 100 ms apart: under the 15 fps cap every output goes out.
            arrival_us: n * 100_000,
            width: 0,
            height: 0,
        }
    }

    /// The timestamps of the frames the sink sent (decoded kinds), and of resets (`None`).
    fn sent(m: &Messages) -> Vec<u64> {
        m.0.lock()
            .unwrap()
            .iter()
            .filter(|b| b[1] == kind::I420)
            .map(|b| u64::from_le_bytes(b[8..16].try_into().unwrap()) / 100_000)
            .collect()
    }

    /// The self-view header carries the facing of the camera that captured each frame: front
    /// up to a flip, back from the new camera's first frame (its keyframe) on; a remote track
    /// carries neither bit.
    #[test]
    fn a_self_view_frame_carries_its_cameras_facing_and_a_flip_changes_it_on_the_first_frame() {
        use crate::video::LOCAL_CAMERA;
        use crate::wire::Facing;
        let msgs = Arc::new(Messages(std::sync::Mutex::new(Vec::new())));
        let mut sinks = Sinks::default();
        for track in [LOCAL_CAMERA, "TR_VCremote"] {
            sinks.open(OpenSink {
                track: track.into(),
                channel: msgs.clone(),
                width: 640,
                height: 360,
                visible: true,
                tile: Tile::Large,
                decode: vec![VideoCodec::Vp8],
                decoded: None,
            });
        }
        let frame = |track: &str, n: u64, keyframe: bool| {
            VideoRecvItem::Frame(EncodedVideoFrame {
                track: track.into(),
                ..vp8(n, keyframe)
            })
        };
        let facing = |facing| VideoRecvItem::Facing {
            track: LOCAL_CAMERA.into(),
            facing,
        };
        sinks.route(facing(Facing::Front));
        for n in 1..=3 {
            sinks.route(frame(LOCAL_CAMERA, n, n == 1));
        }
        sinks.route(frame("TR_VCremote", 4, true));
        sinks.route(facing(Facing::Back));
        for n in 5..=7 {
            sinks.route(frame(LOCAL_CAMERA, n, n == 5));
        }
        // The same facing again changes nothing.
        sinks.route(facing(Facing::Back));
        sinks.route(frame(LOCAL_CAMERA, 8, false));
        let flags: Vec<(u8, u8)> = msgs
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|b| b[1] == kind::ENCODED)
            .map(|b| (b[32], b[3] & (flag::FRONT | flag::BACK | flag::SELF_VIEW)))
            .collect();
        let sv = flag::SELF_VIEW;
        assert_eq!(
            flags.iter().map(|f| f.1).collect::<Vec<_>>(),
            vec![
                sv | flag::FRONT,
                sv | flag::FRONT,
                sv | flag::FRONT,
                0,
                sv | flag::BACK,
                sv | flag::BACK,
                sv | flag::BACK,
                sv | flag::BACK,
            ],
            "{flags:?}"
        );
    }

    #[test]
    fn a_decoded_sink_keeps_busy_frames_in_order_and_skips_to_a_keyframe_on_overflow() {
        let queue = Arc::new(std::sync::Mutex::new((VecDeque::new(), 0usize)));
        let q2 = Arc::clone(&queue);
        let open: DecoderOpener = Arc::new(move |_| {
            Ok(Box::new(Scripted {
                room: 2,
                queue: Arc::clone(&q2),
            }) as Box<dyn FrameDecoder>)
        });
        let msgs = Arc::new(Messages(std::sync::Mutex::new(Vec::new())));
        let mut sinks = Sinks::default();
        let id = sinks.open(OpenSink {
            track: "TR_VCdec".into(),
            channel: msgs.clone(),
            width: 640,
            height: 360,
            visible: true,
            tile: Tile::Large,
            decode: Vec::new(),
            decoded: Some(Decoded::new(
                DecodedFormat::I420,
                open,
                None,
                Arc::new(Notify::new()),
            )),
        });
        let route =
            |sinks: &mut Sinks, n: u64, key: bool| sinks.route(VideoRecvItem::Frame(vp8(n, key)));
        let outputs = |n: usize| queue.lock().unwrap().1 += n;

        // The decoder holds 2 and gives nothing yet: 3..=6 wait in the backlog, nothing lost.
        for n in 1..=6 {
            let r = route(&mut sinks, n, n == 1);
            assert!(r.decoder_keyframes.is_empty(), "frame {n}: {r:?}");
        }
        assert!(sent(&msgs).is_empty());
        // Outputs make room: every frame comes out, in order.
        outputs(6);
        let r = sinks.drain_decoded(id).unwrap();
        assert!(r.decoder_keyframes.is_empty(), "{r:?}");
        assert_eq!(sent(&msgs), vec![1, 2, 3, 4, 5, 6]);

        // A keyframe supersedes the deltas waiting before it.
        for n in 7..=10 {
            route(&mut sinks, n, false);
        }
        route(&mut sinks, 11, true);
        outputs(3);
        sinks.drain_decoded(id).unwrap();
        assert_eq!(
            sent(&msgs)[6..],
            [7, 8, 11],
            "9 and 10 superseded by keyframe 11"
        );

        // An overflow: the waiting frames go, a keyframe is asked, deltas wait for it.
        for n in 12..=13 {
            route(&mut sinks, n, false);
        }
        let mut asked = false;
        for n in 14..=14 + DECODED_BACKLOG as u64 {
            let r = route(&mut sinks, n, false);
            asked |= r.decoder_keyframes == vec!["TR_VCdec".to_owned()];
        }
        assert!(asked, "the overflow asks a keyframe");
        let r = route(&mut sinks, 30, false);
        assert!(r.decoder_keyframes.is_empty());
        outputs(10);
        sinks.drain_decoded(id).unwrap();
        let before = sent(&msgs).len();
        route(&mut sinks, 31, false);
        sinks.drain_decoded(id).unwrap();
        assert_eq!(sent(&msgs).len(), before, "no delta before the keyframe");
        route(&mut sinks, 32, true);
        outputs(1);
        sinks.drain_decoded(id).unwrap();
        assert_eq!(sent(&msgs).last(), Some(&32), "back from the keyframe");
        assert!(sinks.stats("TR_VCdec").dropped > 0);
    }

    #[test]
    fn a_stepped_down_native_sink_asks_for_the_low_layer_until_it_steps_up() {
        assert_eq!(low_size(640, 360), (320, 180));
        assert_eq!(low_size(1280, 720), (320, 180));
        assert_eq!(low_size(360, 640), (102, 180), "portrait keeps its aspect");
        assert_eq!(low_size(160, 90), (160, 90), "already small");

        let mut sinks = Sinks::default();
        let id = sinks.open_native(OpenNative {
            track: "TR_VCann".into(),
            target: Arc::new(Nowhere),
            width: 640,
            height: 360,
            decode: vec![VideoCodec::Vp8],
            follow: false,
        });
        let full = TrackSettings {
            disabled: false,
            width: 640,
            height: 360,
            fps: 30,
        };
        assert_eq!(sinks.changed("TR_VCann"), Some(full));
        assert_eq!(sinks.set_native_low(id, true).as_deref(), Some("TR_VCann"));
        assert_eq!(sinks.set_native_low(id, true), None, "no change twice");
        assert_eq!(
            sinks.changed("TR_VCann"),
            Some(TrackSettings {
                width: 320,
                height: 180,
                ..full
            })
        );
        assert!(sinks.set_native_low(id, false).is_some());
        assert_eq!(sinks.changed("TR_VCann"), Some(full));
    }
}
