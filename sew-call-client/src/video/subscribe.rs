//! The receive side of video, inside the engine: the roster's video tracks, their sinks and
//! settings, and per track the pipeline RTP → reorder → depacketise → decrypt → frame.
//!
//! [`Subscriptions`] holds no I/O. The engine feeds it (the roster, RTP, sink commands, time)
//! and carries out what it asks in [`Out`]: signal requests (`UpdateTrackSettings`),
//! keyframe requests on the PC, items for [`crate::Call::video_frames`] and call events.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use sew_lkproto::livekit::{
    ParticipantInfo, SignalRequest, StreamState, StreamStateInfo, TrackInfo, TrackSource,
    TrackType, UpdateTrackSettings, signal_request,
};
use tokio::sync::mpsc;

use super::keyframe::{KeyframeAsk, KeyframeLimiter};
use super::reorder::{Pushed, Reorder};
use super::rtp::h264::H264Depacketizer;
use super::rtp::vp8::Vp8Depacketizer;
use super::rtp::{Frame, RtpIn};
use super::types::{
    VideoCodec, VideoFrame, VideoLayerInfo, VideoPause, VideoRecvItem, VideoSinkSpec, VideoSource,
    VideoTrack, VideoTrackView,
};
use crate::call::{CallCounters, CallEvent, Cmd};
use crate::e2ee::{CryptoError, KeySession};

/// Sink changes reach the server at most this often (the first sink of a track and the last
/// one closing go at once).
pub(crate) const SETTINGS_DEBOUNCE: Duration = Duration::from_millis(250);

/// The receive side's queue (one consumer), [`crate::Call::video_frames`].
#[derive(Debug)]
pub struct VideoFrameStream(pub(crate) mpsc::Receiver<VideoRecvItem>);

impl VideoFrameStream {
    /// The next item; `None` once the call ended.
    pub async fn recv(&mut self) -> Option<VideoRecvItem> {
        self.0.recv().await
    }

    /// The next item if one is waiting.
    pub fn try_recv(&mut self) -> Option<VideoRecvItem> {
        self.0.try_recv().ok()
    }
}

/// A sink command, from [`VideoSink`] to the engine.
#[derive(Debug)]
pub(crate) enum SinkCmd {
    Open {
        id: u64,
        track_sid: String,
        spec: VideoSinkSpec,
    },
    Update {
        id: u64,
        spec: VideoSinkSpec,
    },
    Keyframe {
        id: u64,
    },
    Close {
        id: u64,
    },
}

/// One open sink of a remote video track ([`crate::Call::video_sink`]): while it is open (and
/// visible) the track's frames flow; dropping it closes it.
///
/// This is the engine's own sink API, enough to drive the server's settings; the Tauri
/// plugin's sinks are built on it.
#[derive(Debug)]
pub struct VideoSink {
    pub(crate) id: u64,
    pub(crate) cmd: mpsc::UnboundedSender<Cmd>,
}

impl VideoSink {
    /// A new size, visibility or tile kind (debounced by 250 ms toward the server).
    pub fn update(&self, spec: VideoSinkSpec) {
        let _ = self
            .cmd
            .send(Cmd::Video(SinkCmd::Update { id: self.id, spec }));
    }

    /// The host's decoder needs a keyframe (a decode error): a PLI goes out, rate-limited.
    pub fn request_keyframe(&self) {
        let _ = self.cmd.send(Cmd::Video(SinkCmd::Keyframe { id: self.id }));
    }

    /// Closes the sink (as dropping it does).
    pub fn close(self) {}
}

impl Drop for VideoSink {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Video(SinkCmd::Close { id: self.id }));
    }
}

/// What [`Subscriptions`] asks the engine to do.
#[derive(Debug, Default)]
pub(crate) struct Out {
    pub(crate) requests: Vec<SignalRequest>,
    /// `(ssrc, fir)`: a keyframe request on the PC.
    pub(crate) keyframes: Vec<(u32, bool)>,
    pub(crate) items: Vec<VideoRecvItem>,
    pub(crate) events: Vec<CallEvent>,
    /// Tracks that stalled (`video_stalled`): their sids.
    pub(crate) stalled: Vec<String>,
}

/// The settings one track asks of the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) disabled: bool,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: u32,
    pub(crate) priority: u32,
}

impl Settings {
    const DISABLED: Settings = Settings {
        disabled: true,
        width: 0,
        height: 0,
        fps: 0,
        priority: 0,
    };
}

/// The settings for a track's sinks: no visible sink → disabled; otherwise the largest
/// visible sink's size, 30 fps for a large tile and 15 for a grid tile, and a priority with
/// screens above cameras and the active speaker above the others.
pub(crate) fn settings_for<'a>(
    sinks: impl Iterator<Item = &'a VideoSinkSpec>,
    source: VideoSource,
    speaking: bool,
) -> Settings {
    let mut s = Settings::DISABLED;
    for k in sinks.filter(|k| k.visible) {
        s.disabled = false;
        s.width = s.width.max(k.width);
        s.height = s.height.max(k.height);
        s.fps = s.fps.max(if k.large { 30 } else { 15 });
    }
    if !s.disabled {
        let base = match source {
            VideoSource::ScreenShare => 2,
            VideoSource::Camera => 0,
        };
        s.priority = base + u32::from(speaking);
    }
    s
}

/// One received packet, held in the reorder buffer.
#[derive(Debug)]
struct Pkt {
    seq: u64,
    timestamp: u32,
    marker: bool,
    payload: Arc<[u8]>,
    at: Instant,
}

#[derive(Debug)]
enum Depack {
    Vp8(Vp8Depacketizer),
    H264(H264Depacketizer),
}

impl Depack {
    fn new(codec: VideoCodec) -> Depack {
        match codec {
            VideoCodec::Vp8 => Depack::Vp8(Vp8Depacketizer::new()),
            VideoCodec::H264 => Depack::H264(H264Depacketizer::new()),
        }
    }

    fn codec(&self) -> VideoCodec {
        match self {
            Depack::Vp8(_) => VideoCodec::Vp8,
            Depack::H264(_) => VideoCodec::H264,
        }
    }

    fn push(&mut self, p: RtpIn<'_>) -> Option<Frame> {
        match self {
            Depack::Vp8(d) => d.push(p),
            Depack::H264(d) => d.push(p),
        }
    }

    fn dropped(&self) -> u64 {
        match self {
            Depack::Vp8(d) => d.dropped(),
            Depack::H264(d) => d.dropped(),
        }
    }
}

/// One remote video track.
#[derive(Debug)]
struct Track {
    info: VideoTrack,
    participant_sid: String,
    ssrc: Option<u32>,
    reorder: Reorder<Pkt>,
    depack: Option<Depack>,
    /// Frames flow to the host (a sink is open and `Added` went out).
    flowing: bool,
    /// Frames are dropped until a keyframe (a loss, a decrypt failure, a new sink).
    need_key: bool,
    kf: KeyframeLimiter,
    sent: Option<Settings>,
    settings_due: Option<Instant>,
}

impl Track {
    fn new(info: VideoTrack, participant_sid: String) -> Track {
        Track {
            info,
            participant_sid,
            ssrc: None,
            reorder: Reorder::new(),
            depack: None,
            flowing: false,
            need_key: true,
            kf: KeyframeLimiter::new(),
            sent: None,
            settings_due: None,
        }
    }

    /// Asks for a keyframe (and tells the host, once per wait).
    fn want_key(&mut self, out: &mut Out) {
        if !self.kf.waiting() && self.flowing {
            out.items.push(VideoRecvItem::KeyframeNeeded {
                track: self.info.id,
            });
        }
        self.kf.request();
    }

    /// Frames were lost or failed: the host resets, frames wait for a keyframe.
    fn broken(&mut self, out: &mut Out) {
        if !self.need_key && self.flowing {
            out.items.push(VideoRecvItem::Broken {
                track: self.info.id,
            });
        }
        self.need_key = true;
        self.want_key(out);
    }
}

fn source_of(t: &TrackInfo) -> Option<VideoSource> {
    if t.r#type != TrackType::Video as i32 {
        return None;
    }
    Some(if t.source == TrackSource::ScreenShare as i32 {
        VideoSource::ScreenShare
    } else {
        VideoSource::Camera
    })
}

/// The codec a publisher's `TrackInfo` names (its mime type, else its first codec's).
pub(crate) fn codec_of(t: &TrackInfo) -> Option<VideoCodec> {
    VideoCodec::from_mime(&t.mime_type).or_else(|| {
        t.codecs
            .iter()
            .find_map(|c| VideoCodec::from_mime(&c.mime_type))
    })
}

/// The publisher's layers: its codec's (`TrackInfo.codecs`), else the older `layers` field.
fn layers_of(t: &TrackInfo) -> Vec<VideoLayerInfo> {
    let layers = match t.codecs.iter().find(|c| !c.layers.is_empty()) {
        Some(c) => &c.layers,
        #[allow(deprecated)]
        None => &t.layers,
    };
    let mut v: Vec<VideoLayerInfo> = layers
        .iter()
        .map(|l| VideoLayerInfo {
            width: l.width,
            height: l.height,
            bitrate: l.bitrate,
        })
        .collect();
    v.sort_by_key(|l| (l.height, l.width));
    v
}

fn request(m: signal_request::Message) -> SignalRequest {
    SignalRequest { message: Some(m) }
}

fn settings_request(sid: &str, s: Settings) -> SignalRequest {
    request(signal_request::Message::TrackSetting(UpdateTrackSettings {
        track_sids: vec![sid.to_owned()],
        disabled: s.disabled,
        width: s.width,
        height: s.height,
        fps: s.fps,
        priority: s.priority,
        ..UpdateTrackSettings::default()
    }))
}

/// The receive side's state.
#[derive(Debug)]
pub(crate) struct Subscriptions {
    epoch: Instant,
    tracks: BTreeMap<String, Track>,
    next_id: u64,
    /// Sink id → (track sid, spec).
    sinks: HashMap<u64, (String, VideoSinkSpec)>,
    /// Participant sids speaking now (the server's speaker list).
    speaking: HashSet<String>,
}

impl Subscriptions {
    pub(crate) fn new(epoch: Instant) -> Subscriptions {
        Subscriptions {
            epoch,
            tracks: BTreeMap::new(),
            next_id: 1,
            sinks: HashMap::new(),
            speaking: HashSet::new(),
        }
    }

    /// A participant's camera and screen for the snapshot.
    pub(crate) fn views(&self, identity: &str) -> (Option<VideoTrackView>, Option<VideoTrackView>) {
        let mut camera = None;
        let mut screen = None;
        for t in self.tracks.values().filter(|t| t.info.identity == identity) {
            match t.info.source {
                VideoSource::Camera => camera = camera.or(Some(t.info.view())),
                VideoSource::ScreenShare => screen = screen.or(Some(t.info.view())),
            }
        }
        (camera, screen)
    }

    /// The roster changed: new video tracks are announced (and disabled at the server until a
    /// sink opens), changed ones re-announced, gone ones removed.
    pub(crate) fn roster(
        &mut self,
        now: Instant,
        remotes: &BTreeMap<String, ParticipantInfo>,
        members: &HashMap<String, String>,
        out: &mut Out,
    ) {
        let mut live = HashSet::new();
        for p in remotes.values() {
            for t in &p.tracks {
                let Some(source) = source_of(t) else {
                    continue;
                };
                live.insert(t.sid.clone());
                let member_id = members.get(&p.identity).cloned();
                match self.tracks.get_mut(&t.sid) {
                    None => {
                        let id = self.next_id;
                        self.next_id += 1;
                        let info = VideoTrack {
                            id,
                            identity: p.identity.clone(),
                            member_id,
                            source,
                            track_sid: t.sid.clone(),
                            codec: codec_of(t),
                            width: t.width,
                            height: t.height,
                            muted: t.muted,
                            paused: None,
                            layers: layers_of(t),
                        };
                        out.events.push(CallEvent::VideoAdded(info.clone()));
                        // No video flows to a member who shows no tile.
                        out.requests
                            .push(settings_request(&t.sid, Settings::DISABLED));
                        let mut track = Track::new(info, p.sid.clone());
                        track.sent = Some(Settings::DISABLED);
                        self.tracks.insert(t.sid.clone(), track);
                        // Sinks opened before the roster named the track.
                        if self.sinks.values().any(|(s, _)| *s == t.sid) {
                            self.sinks_changed(now, &t.sid, out);
                        }
                    }
                    Some(track) => {
                        let mut info = track.info.clone();
                        info.member_id = member_id.or(info.member_id);
                        info.muted = t.muted;
                        info.width = t.width;
                        info.height = t.height;
                        info.layers = layers_of(t);
                        if info.codec.is_none() {
                            info.codec = codec_of(t);
                        }
                        track.participant_sid = p.sid.clone();
                        if info != track.info {
                            track.info = info.clone();
                            out.events.push(CallEvent::VideoChanged(info));
                        }
                    }
                }
            }
        }
        let gone: Vec<String> = self
            .tracks
            .keys()
            .filter(|k| !live.contains(*k))
            .cloned()
            .collect();
        for sid in gone {
            if let Some(t) = self.tracks.remove(&sid) {
                if t.flowing {
                    out.items.push(VideoRecvItem::Removed { track: t.info.id });
                }
                out.events.push(CallEvent::VideoRemoved {
                    id: t.info.id,
                    track_sid: sid,
                });
            }
        }
    }

    /// The server's speaker list (participant sids): priorities follow it.
    pub(crate) fn speakers(&mut self, now: Instant, speaking: HashSet<String>, out: &mut Out) {
        if speaking == self.speaking {
            return;
        }
        self.speaking = speaking;
        let sids: Vec<String> = self
            .tracks
            .iter()
            .filter(|(_, t)| t.enabled())
            .map(|(s, _)| s.clone())
            .collect();
        for sid in sids {
            self.schedule(now, &sid, false, out);
        }
    }

    /// The server's stream states: a paused track is shown paused (congestion).
    pub(crate) fn stream_states(&mut self, states: &[StreamStateInfo], out: &mut Out) {
        for s in states {
            let Some(t) = self.tracks.get_mut(&s.track_sid) else {
                continue;
            };
            let paused = (s.state == StreamState::Paused as i32).then_some(VideoPause::Congestion);
            if t.info.paused != paused {
                t.info.paused = paused;
                out.events.push(CallEvent::VideoChanged(t.info.clone()));
            }
        }
    }

    /// A sink command from the host.
    pub(crate) fn sink(&mut self, now: Instant, cmd: SinkCmd, out: &mut Out) {
        match cmd {
            SinkCmd::Open {
                id,
                track_sid,
                spec,
            } => {
                self.sinks.insert(id, (track_sid.clone(), spec));
                self.sinks_changed(now, &track_sid, out);
            }
            SinkCmd::Update { id, spec } => {
                let Some((sid, s)) = self.sinks.get_mut(&id) else {
                    return;
                };
                *s = spec;
                let sid = sid.clone();
                self.sinks_changed(now, &sid, out);
            }
            SinkCmd::Keyframe { id } => {
                let Some((sid, _)) = self.sinks.get(&id) else {
                    return;
                };
                if let Some(t) = self.tracks.get_mut(sid)
                    && t.flowing
                {
                    t.want_key(out);
                }
            }
            SinkCmd::Close { id } => {
                if let Some((sid, _)) = self.sinks.remove(&id) {
                    self.sinks_changed(now, &sid, out);
                }
            }
        }
    }

    fn sinks_of<'a>(&'a self, sid: &'a str) -> impl Iterator<Item = &'a VideoSinkSpec> + 'a {
        self.sinks
            .values()
            .filter(move |(s, _)| s == sid)
            .map(|(_, spec)| spec)
    }

    /// A track's sinks changed: frames start (the first visible sink) or stop (none left), and
    /// its settings follow.
    fn sinks_changed(&mut self, now: Instant, sid: &str, out: &mut Out) {
        let want = self.sinks_of(sid).any(|k| k.visible);
        let Some(t) = self.tracks.get_mut(sid) else {
            return;
        };
        match (t.flowing, want) {
            (false, true) => {
                t.flowing = true;
                t.need_key = true;
                out.items.push(VideoRecvItem::Added {
                    track: t.info.id,
                    codec: t
                        .depack
                        .as_ref()
                        .map_or(t.info.codec.unwrap_or(VideoCodec::Vp8), Depack::codec),
                });
                t.want_key(out);
                self.schedule(now, sid, true, out);
            }
            (true, false) => {
                t.flowing = false;
                t.kf.cancel();
                t.reorder.reset();
                t.depack = None;
                out.items.push(VideoRecvItem::Removed { track: t.info.id });
                self.schedule(now, sid, true, out);
            }
            _ => self.schedule(now, sid, false, out),
        }
    }

    /// Sends the track's settings now (`at_once`) or after the debounce.
    fn schedule(&mut self, now: Instant, sid: &str, at_once: bool, out: &mut Out) {
        if at_once {
            if let Some(t) = self.tracks.get_mut(sid) {
                t.settings_due = None;
            }
            self.send_settings(sid, out);
        } else if let Some(t) = self.tracks.get_mut(sid) {
            t.settings_due.get_or_insert(now + SETTINGS_DEBOUNCE);
        }
    }

    fn send_settings(&mut self, sid: &str, out: &mut Out) {
        let Some(t) = self.tracks.get(sid) else {
            return;
        };
        let speaking = self.speaking.contains(&t.participant_sid);
        let s = settings_for(self.sinks_of(sid), t.info.source, speaking);
        if let Some(t) = self.tracks.get_mut(sid)
            && t.sent != Some(s)
        {
            t.sent = Some(s);
            out.requests.push(settings_request(sid, s));
        }
    }

    /// Sends every track's settings again (after a reconnect the server may hold none).
    pub(crate) fn resend_settings(&mut self, out: &mut Out) {
        let sids: Vec<String> = self.tracks.keys().cloned().collect();
        for sid in sids {
            if let Some(t) = self.tracks.get_mut(&sid) {
                t.sent = None;
                t.settings_due = None;
            }
            self.send_settings(&sid, out);
        }
    }

    /// The round-trip time: each reorder window is the larger of 150 ms and twice it.
    pub(crate) fn set_rtt(&mut self, rtt: Duration) {
        for t in self.tracks.values_mut() {
            t.reorder.set_rtt(rtt);
        }
    }

    /// A new PC (a full reconnect): the streams start over; flowing tracks wait for a keyframe.
    pub(crate) fn media_reset(&mut self, out: &mut Out) {
        for t in self.tracks.values_mut() {
            t.ssrc = None;
            t.reorder.reset();
            t.depack = None;
            t.kf.cancel();
            if t.flowing {
                t.broken(out);
            }
        }
    }

    /// The media resumed (a resume from background or a network change): flowing tracks ask
    /// for a keyframe.
    pub(crate) fn resumed(&mut self, out: &mut Out) {
        for t in self.tracks.values_mut().filter(|t| t.flowing) {
            t.want_key(out);
        }
    }

    /// One RTP packet of `track_sid` (from `identity`, allowed by the keys).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn on_rtp(
        &mut self,
        now: Instant,
        track_sid: &str,
        identity: &str,
        r: &crate::pc::RtpRx,
        codec: VideoCodec,
        keys: &dyn KeySession,
        counters: &mut CallCounters,
        out: &mut Out,
    ) {
        let Some(t) = self.tracks.get_mut(track_sid) else {
            counters.video_packets_unwanted += 1;
            return;
        };
        if t.info.codec != Some(codec) {
            // The negotiated payload type says what the publisher sends.
            t.info.codec = Some(codec);
            out.events.push(CallEvent::VideoChanged(t.info.clone()));
        }
        if !t.flowing {
            // No sink: nothing is forwarded (the server was asked to disable the track).
            counters.video_packets_unwanted += 1;
            return;
        }
        if t.ssrc != Some(r.ssrc) || t.depack.as_ref().map(Depack::codec) != Some(codec) {
            // A new stream (a layer switch keeps the SSRC; a new publication or PC does not).
            if t.ssrc.is_some() {
                t.reorder.reset();
            }
            t.ssrc = Some(r.ssrc);
            t.depack = Some(Depack::new(codec));
        }
        let mut ready = Vec::new();
        let skipped = t.reorder.skipped;
        let pushed = t.reorder.push(
            r.seq,
            now,
            Pkt {
                seq: r.seq,
                timestamp: r.timestamp,
                marker: r.marker,
                payload: r.payload.clone(),
                at: now,
            },
            &mut ready,
        );
        if pushed == Pushed::Late {
            counters.video_packets_late += 1;
        }
        Self::gap_skipped(t, skipped, counters, out);
        let epoch = self.epoch;
        for p in ready {
            Self::depacketise(epoch, t, p, identity, keys, counters, out);
        }
    }

    /// The reorder buffer skipped packets (`skipped` was its count before): a gap the
    /// retransmissions did not fill. Whatever it held (part of a frame, or whole frames, which
    /// VP8's depacketiser cannot see when the gap ends on a frame boundary), the decoder's
    /// references are gone: the track breaks and waits for a keyframe.
    fn gap_skipped(t: &mut Track, skipped: u64, counters: &mut CallCounters, out: &mut Out) {
        if t.reorder.skipped > skipped {
            counters.video_packets_lost += t.reorder.skipped - skipped;
            t.broken(out);
        }
    }

    fn depacketise(
        epoch: Instant,
        t: &mut Track,
        p: Pkt,
        identity: &str,
        keys: &dyn KeySession,
        counters: &mut CallCounters,
        out: &mut Out,
    ) {
        let Some(d) = t.depack.as_mut() else {
            return;
        };
        let codec = d.codec();
        let before = d.dropped();
        let frame = d.push(RtpIn {
            seq: p.seq as u16,
            timestamp: p.timestamp,
            marker: p.marker,
            payload: &p.payload,
        });
        let lost = d.dropped() - before;
        if lost > 0 {
            // A gap the NACKs did not fill: the frame is gone, the decoder must wait for a
            // keyframe.
            counters.video_frames_dropped += lost;
            t.broken(out);
        }
        let Some(f) = frame else {
            return;
        };
        if t.need_key && !f.keyframe {
            counters.video_frames_dropped += 1;
            return;
        }
        let mut plain = Vec::with_capacity(f.data.len());
        match keys.decrypt_video(identity, codec, &f.data, &mut plain) {
            Ok(()) if !plain.is_empty() => {}
            Ok(()) => {
                counters.video_decrypt_failures += 1;
                t.broken(out);
                return;
            }
            Err(e) => {
                match e {
                    CryptoError::KeyPending => counters.video_frames_before_key += 1,
                    CryptoError::NoKey => counters.refused_frames += 1,
                    _ => counters.video_decrypt_failures += 1,
                }
                t.broken(out);
                return;
            }
        }
        let size = if f.keyframe {
            match codec {
                VideoCodec::Vp8 => super::size::vp8(&plain),
                VideoCodec::H264 => super::size::h264(&plain),
            }
        } else {
            None
        };
        if f.keyframe {
            t.need_key = false;
            t.kf.on_keyframe();
        }
        counters.video_frames_received += 1;
        out.items.push(VideoRecvItem::Frame(VideoFrame {
            track: t.info.id,
            codec,
            data: Bytes::from(plain),
            keyframe: f.keyframe,
            rtp_ts: f.timestamp,
            arrival_us: p.at.saturating_duration_since(epoch).as_micros() as u64,
            width: size.map(|s| s.0),
            height: size.map(|s| s.1),
        }));
    }

    /// The host's queue was full: the frame is gone, so the track waits for a keyframe.
    pub(crate) fn frame_not_delivered(&mut self, track: u64, out: &mut Out) {
        if let Some(t) = self.tracks.values_mut().find(|t| t.info.id == track) {
            t.broken(out);
        }
    }

    /// Time passed: gaps skipped, keyframe requests sent, settings flushed.
    pub(crate) fn tick(
        &mut self,
        now: Instant,
        keys: Option<&dyn KeySession>,
        counters: &mut CallCounters,
        out: &mut Out,
    ) {
        let epoch = self.epoch;
        let due: Vec<String> = self
            .tracks
            .iter()
            .filter(|(_, t)| t.settings_due.is_some_and(|d| d <= now))
            .map(|(s, _)| s.clone())
            .collect();
        for sid in due {
            if let Some(t) = self.tracks.get_mut(&sid) {
                t.settings_due = None;
            }
            self.send_settings(&sid, out);
        }
        for (sid, t) in self.tracks.iter_mut() {
            if !t.flowing {
                continue;
            }
            let mut ready = Vec::new();
            let skipped = t.reorder.skipped;
            t.reorder.poll(now, &mut ready);
            Self::gap_skipped(t, skipped, counters, out);
            if !ready.is_empty()
                && let Some(keys) = keys
            {
                // The track's publisher is the frames' sender.
                let identity = t.info.identity.clone();
                for p in ready {
                    Self::depacketise(epoch, t, p, &identity, keys, counters, out);
                }
            }
            let Some(ssrc) = t.ssrc else {
                continue;
            };
            let d = t.kf.poll(now);
            match d.ask {
                Some(KeyframeAsk::Pli) => {
                    counters.video_plis += 1;
                    out.keyframes.push((ssrc, false));
                }
                Some(KeyframeAsk::Fir) => {
                    counters.video_firs += 1;
                    out.keyframes.push((ssrc, true));
                }
                None => {}
            }
            if d.stalled {
                out.stalled.push(sid.clone());
            }
        }
    }

    /// When [`Subscriptions::tick`] next has something to do.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        let mut d: Option<Instant> = None;
        for t in self.tracks.values() {
            let mut take = |x: Option<Instant>| {
                if let Some(x) = x {
                    d = Some(d.map_or(x, |y| y.min(x)));
                }
            };
            take(t.settings_due);
            if t.flowing {
                take(t.reorder.deadline());
                if t.ssrc.is_some() {
                    take(t.kf.deadline());
                }
            }
        }
        d
    }
}

impl Track {
    /// Whether the server was last asked to send the track (its priority can change).
    fn enabled(&self) -> bool {
        self.sent.is_some_and(|s| !s.disabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink(w: u32, h: u32, large: bool, visible: bool) -> VideoSinkSpec {
        VideoSinkSpec::new(w, h, large).visible(visible)
    }

    #[test]
    fn no_visible_sink_disables_the_track() {
        assert_eq!(
            settings_for([].iter(), VideoSource::Camera, false),
            Settings::DISABLED
        );
        let hidden = [sink(640, 360, true, false)];
        assert!(settings_for(hidden.iter(), VideoSource::Camera, true).disabled);
    }

    #[test]
    fn the_largest_visible_sink_sets_the_size_and_a_large_tile_30_fps() {
        let sinks = [
            sink(320, 180, false, true),
            sink(1280, 720, true, false),
            sink(640, 400, false, true),
        ];
        let s = settings_for(sinks.iter(), VideoSource::Camera, false);
        assert_eq!(
            s,
            Settings {
                disabled: false,
                width: 640,
                height: 400,
                fps: 15,
                priority: 0
            }
        );
        let sinks = [sink(320, 180, false, true), sink(960, 540, true, true)];
        let s = settings_for(sinks.iter(), VideoSource::Camera, false);
        assert_eq!((s.width, s.height, s.fps), (960, 540, 30));
    }

    #[test]
    fn screens_rank_above_cameras_and_the_speaker_above_others() {
        let sinks = [sink(320, 180, false, true)];
        let cam = settings_for(sinks.iter(), VideoSource::Camera, false).priority;
        let cam_speaking = settings_for(sinks.iter(), VideoSource::Camera, true).priority;
        let screen = settings_for(sinks.iter(), VideoSource::ScreenShare, false).priority;
        assert!(cam_speaking > cam);
        assert!(screen > cam_speaking);
    }

    fn camera(sid: &str, muted: bool) -> TrackInfo {
        TrackInfo {
            sid: sid.to_owned(),
            r#type: TrackType::Video as i32,
            source: TrackSource::Camera as i32,
            mime_type: "video/VP8".to_owned(),
            width: 1280,
            height: 720,
            muted,
            ..TrackInfo::default()
        }
    }

    fn roster(tracks: Vec<TrackInfo>) -> BTreeMap<String, ParticipantInfo> {
        let mut m = BTreeMap::new();
        m.insert(
            "PA_a".to_owned(),
            ParticipantInfo {
                sid: "PA_a".to_owned(),
                identity: "p_a".to_owned(),
                tracks,
                ..ParticipantInfo::default()
            },
        );
        m
    }

    fn setting(r: &SignalRequest) -> Option<&UpdateTrackSettings> {
        match &r.message {
            Some(signal_request::Message::TrackSetting(s)) => Some(s),
            _ => None,
        }
    }

    #[test]
    fn a_new_track_is_announced_and_disabled_until_a_sink_opens() {
        let t0 = Instant::now();
        let mut s = Subscriptions::new(t0);
        let mut out = Out::default();
        s.roster(
            t0,
            &roster(vec![camera("TR_c", false)]),
            &HashMap::new(),
            &mut out,
        );
        assert!(
            matches!(&out.events[..], [CallEvent::VideoAdded(v)] if v.track_sid == "TR_c"
            && v.codec == Some(VideoCodec::Vp8) && v.source == VideoSource::Camera)
        );
        let u = setting(&out.requests[0]).unwrap();
        assert!(u.disabled);
        assert_eq!(u.track_sids, vec!["TR_c".to_owned()]);
        assert!(out.items.is_empty(), "nothing flows without a sink");
        // The first sink: frames start (Added, a keyframe asked) and the track is enabled now.
        let mut out = Out::default();
        s.sink(
            t0,
            SinkCmd::Open {
                id: 1,
                track_sid: "TR_c".to_owned(),
                spec: VideoSinkSpec::new(640, 360, false),
            },
            &mut out,
        );
        assert!(matches!(
            out.items[0],
            VideoRecvItem::Added {
                codec: VideoCodec::Vp8,
                ..
            }
        ));
        assert!(matches!(out.items[1], VideoRecvItem::KeyframeNeeded { .. }));
        let u = setting(&out.requests[0]).unwrap();
        assert!(!u.disabled);
        assert_eq!((u.width, u.height, u.fps), (640, 360, 15));
        // A resize is debounced 250 ms.
        let mut out = Out::default();
        s.sink(
            t0,
            SinkCmd::Update {
                id: 1,
                spec: VideoSinkSpec::new(1280, 720, true),
            },
            &mut out,
        );
        assert!(out.requests.is_empty());
        assert_eq!(s.deadline(), Some(t0 + SETTINGS_DEBOUNCE));
        let mut c = CallCounters::default();
        let mut out = Out::default();
        s.tick(t0 + SETTINGS_DEBOUNCE, None, &mut c, &mut out);
        let u = setting(&out.requests[0]).unwrap();
        assert_eq!((u.width, u.height, u.fps), (1280, 720, 30));
        // The last sink closes: frames stop and the track is disabled at once.
        let mut out = Out::default();
        s.sink(t0, SinkCmd::Close { id: 1 }, &mut out);
        assert!(matches!(out.items[..], [VideoRecvItem::Removed { .. }]));
        assert!(setting(&out.requests[0]).unwrap().disabled);
    }

    #[test]
    fn mute_size_and_congestion_are_changes_and_a_gone_track_is_removed() {
        let t0 = Instant::now();
        let mut s = Subscriptions::new(t0);
        let mut out = Out::default();
        s.roster(
            t0,
            &roster(vec![camera("TR_c", false)]),
            &HashMap::new(),
            &mut out,
        );
        let mut out = Out::default();
        s.roster(
            t0,
            &roster(vec![camera("TR_c", true)]),
            &HashMap::new(),
            &mut out,
        );
        assert!(matches!(&out.events[..], [CallEvent::VideoChanged(v)] if v.muted));
        let mut out = Out::default();
        s.stream_states(
            &[StreamStateInfo {
                participant_sid: "PA_a".to_owned(),
                track_sid: "TR_c".to_owned(),
                state: StreamState::Paused as i32,
            }],
            &mut out,
        );
        assert!(
            matches!(&out.events[..], [CallEvent::VideoChanged(v)] if v.paused == Some(VideoPause::Congestion))
        );
        assert_eq!(
            s.views("p_a").0.unwrap().paused,
            Some(VideoPause::Congestion)
        );
        let mut out = Out::default();
        s.roster(t0, &roster(vec![]), &HashMap::new(), &mut out);
        assert!(
            matches!(&out.events[..], [CallEvent::VideoRemoved { track_sid, .. }] if track_sid == "TR_c")
        );
        assert_eq!(s.views("p_a"), (None, None));
    }

    /// Keys that hand every frame back as it is (a unit test of the pipeline's bookkeeping;
    /// the engine's keys are never this).
    struct EchoKeys;

    impl KeySession for EchoKeys {
        fn encrypt(&self, _: &[u8], _: u32, _: u32, _: &mut Vec<u8>) -> Result<(), CryptoError> {
            Err(CryptoError::NoSendKey)
        }
        fn decrypt(&self, _: &str, _: &[u8], _: &mut Vec<u8>) -> Result<(), CryptoError> {
            Err(CryptoError::NoKey)
        }
        fn decrypt_video(
            &self,
            _: &str,
            _: VideoCodec,
            frame: &[u8],
            out: &mut Vec<u8>,
        ) -> Result<(), CryptoError> {
            out.extend_from_slice(frame);
            Ok(())
        }
        fn may_subscribe(&self, _: &str, _: &crate::e2ee::RemoteTrack) -> crate::e2ee::Subscribe {
            crate::e2ee::Subscribe::Yes
        }
        fn close(&self) -> crate::provider::BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }

    /// One single-packet VP8 frame: the descriptor (S, PID 0), then a keyframe or delta tag.
    fn vp8_packet(seq: u64, key: bool) -> crate::pc::RtpRx {
        let mut payload = vec![0x10, if key { 0x00 } else { 0x01 }];
        payload.extend_from_slice(&[0x9d, 0x01, 0x2a, 0x40, 0x01, 0xb4, 0x00, 0x55, 0x55]);
        crate::pc::RtpRx {
            mid: "1".to_owned(),
            ssrc: 7,
            seq,
            timestamp: seq as u32 * 3_000,
            marker: true,
            video: Some(VideoCodec::Vp8),
            payload: Arc::from(payload),
        }
    }

    #[test]
    fn whole_frames_lost_on_a_frame_boundary_break_the_track() {
        let t0 = Instant::now();
        let mut s = Subscriptions::new(t0);
        let mut out = Out::default();
        s.roster(
            t0,
            &roster(vec![camera("TR_c", false)]),
            &HashMap::new(),
            &mut out,
        );
        s.sink(
            t0,
            SinkCmd::Open {
                id: 1,
                track_sid: "TR_c".to_owned(),
                spec: VideoSinkSpec::new(640, 360, false),
            },
            &mut out,
        );
        let mut c = CallCounters::default();
        let rx = |s: &mut Subscriptions, seq, key, at, out: &mut Out, c: &mut CallCounters| {
            let r = vp8_packet(seq, key);
            s.on_rtp(at, "TR_c", "p_a", &r, VideoCodec::Vp8, &EchoKeys, c, out);
        };
        let mut out = Out::default();
        rx(&mut s, 10, true, t0, &mut out, &mut c);
        rx(&mut s, 11, false, t0, &mut out, &mut c);
        let frames = |o: &Out| {
            o.items
                .iter()
                .filter(|i| matches!(i, VideoRecvItem::Frame(_)))
                .count()
        };
        assert_eq!(frames(&out), 2);
        // Packets 12..=14 (three whole frames) never arrive; 15 waits the window, then the gap
        // is skipped: the track breaks, and the delta frames after it are not handed out.
        let mut out = Out::default();
        rx(&mut s, 15, false, t0, &mut out, &mut c);
        assert_eq!(frames(&out), 0, "held for the gap");
        s.tick(
            t0 + Duration::from_millis(200),
            Some(&EchoKeys),
            &mut c,
            &mut out,
        );
        assert!(
            out.items
                .iter()
                .any(|i| matches!(i, VideoRecvItem::Broken { .. })),
            "{:?}",
            out.items
        );
        assert_eq!(frames(&out), 0, "a delta frame after the loss");
        assert_eq!(c.video_packets_lost, 3);
        let mut out = Out::default();
        rx(
            &mut s,
            16,
            true,
            t0 + Duration::from_millis(210),
            &mut out,
            &mut c,
        );
        assert_eq!(frames(&out), 1, "the keyframe restarts the track");
    }
}
