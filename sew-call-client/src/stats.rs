//! The call-stats beacon: one aggregate, anonymous [`CallStatsSample`] per window to Sew's
//! `POST /api/v1/me/voice/stats` with the call credential, and the same per-window numbers for
//! the host ([`crate::Call::stats`], [`crate::CallEvent::Stats`]).
//!
//! **When.** The join answer's `stats` terms (`calls.stats_beacon` on) set the window
//! (`interval_ms`, 15 s from Sew). The first window starts when the call first reaches
//! `connected`; a sample goes out at the end of every window and once more when the host
//! leaves (the partial window, before Sew's leave, so the caller is still in voice). A join
//! with no terms sends nothing, but the windows are still measured (at 15 s) for the host. A
//! re-join takes the new answer's terms. Sew's `403 call_stats_off` (the policy turned off)
//! stops the beacon for the rest of the call; any other refusal is recorded on that window
//! and the next window is sent as usual.
//!
//! **What.** `client` is `native` and the six `audio_*` fields are sent **only** when the terms
//! say `audio_fields: true` (an older Sew refuses both: `deny_unknown_fields`); otherwise the
//! sample is the browsers' shape with `client: other`. The video fields (`frames_decoded`,
//! `frames_dropped`, `freezes`, `freeze_ms`) are 0: the engine carries audio only. Every value
//! is clamped to Sew's caps (`sew_proto::voice::CALL_STATS_*`) and the sample is validated
//! before it is sent.
//!
//! How each value is derived (a "stream" is one remote audio track; packets are counted on
//! RTP arrival in the engine, before decryption, for tracks the keys allow):
//!
//! | Field | From |
//! |---|---|
//! | `interval_ms` | the window's wall length (the leave sample: the partial window, at least 1 ms) |
//! | `decrypt_failures` | remote frames that failed to decrypt in the window ([`crate::CallCounters::decrypt_failures`]): not the frames whose key was not installed yet, around a join or a key switch ([`crate::CallCounters::frames_before_key`], in the host's window only) |
//! | `candidate`, `relay_protocol` | the local candidate of str0m's selected pair (its latest `PeerStats`, once a second), matched to the transport's candidates: a TURN relay is `relay` with its `udp`/`tcp`/`tls`, else `host` |
//! | `rtt_ms` | the selected pair's latest ICE (STUN consent) round-trip time from the same `PeerStats` |
//! | `audio_played_ms` | 20 ms × the frames the audio pipeline played in the window, summed over streams: kessel-audio's `frames_decoded + frames_fec + frames_concealed` (DTX silence not counted, as kessel-audio's `concealed_pct`), read once a second and differenced; a host with its own pipeline reports them through [`crate::Call::set_playout_stats`]; with neither, the engine's view: 20 ms × (frames handed to the pipeline + loss marks) |
//! | `audio_concealed_ms` | 20 ms × kessel-audio's `frames_concealed` in the window (loss concealment; FEC recovery is not concealment); the engine's fallback: 20 ms × loss marks |
//! | `audio_packets_expected` | per stream, the highest RTP sequence number at the window's end minus the highest at its start (a new stream: from its first packet), summed |
//! | `audio_packets_lost` | expected minus the unique packets that arrived in the window (a late packet still counts as received), at least 0 |
//! | `audio_jitter_ms` | the worst stream's RFC 3550 interarrival jitter (arrival time against the 48 kHz RTP timestamp, smoothed 1/16), the highest value it reached in the window |
//! | `audio_longest_gap_ms` | the longest stretch with no audio arriving on one stream, counted when the stream's next packet arrives: the arrival interval beyond what the RTP timestamps advanced (a network stall) plus 20 ms per packet missing in between (loss), at most the arrival interval; sender silence (DTX, or a pause the timestamps cover) is not a gap. A gap is counted in the window where audio resumes, clipped to that window |
//!
//! **Before the first window.** Frames counted before the call first reached `connected` (the
//! keys still settling: refused before the subscription, or crossing the join's key switch) are
//! in no window; [`CallStatsReport::before_first_window`] holds them, so a counter of
//! [`crate::CallCounters`] is that bucket plus the sum of the windows (while every window is
//! kept).
//!
//! Known limits: `audio_longest_gap_ms` is an inter-packet measure on one stream. A stream
//! that stops and never resumes adds no gap (and no loss) for that stretch, and a reconnect
//! that replaces a track (a full reconnect, a re-join or a move: new PC, new streams) starts a
//! new stream, so the silence of the reconnect itself is never a gap: a call with reconnects
//! of 1 to 3 s of silence can read gaps of a few ms. Silence at the speaker across reconnects
//! is the host's to measure (the playout side). A track removed mid-window keeps the counts up
//! to its removal; a host that never plays audio (no pipeline attached) gets the engine's
//! fallback for played and concealed.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde::Serialize;
use sew_proto::error::Validate as _;
use sew_proto::voice::{
    CALL_STATS_AUDIO_JITTER_MAX_MS, CALL_STATS_AUDIO_PACKETS_MAX, CALL_STATS_AUDIO_STREAMS_MAX,
    CALL_STATS_EVENTS_MAX, CALL_STATS_INTERVAL_MS, CALL_STATS_RTT_MAX_MS, CALL_STATS_WINDOW_MAX_MS,
    CallStatsBeacon, CallStatsCandidate, CallStatsClient, CallStatsRelayProtocol, CallStatsSample,
};

use crate::call::CallCounters;

/// One Opus frame's duration (the engine and kessel-audio send and play 20 ms frames).
pub(crate) const FRAME_MS: u64 = 20;
/// Windows [`CallStatsReport::windows`] keeps (the latest).
pub(crate) const WINDOWS_KEPT: usize = 256;
/// How often the playout counters and the selected pair are read.
const POLL_EVERY: Duration = Duration::from_secs(1);
/// Missing sequence numbers remembered per stream (a later arrival still counts as received).
const MISSING_KEPT: usize = 128;
/// Gaps in the sequence longer than this are not remembered packet by packet.
const MISSING_MAX_RUN: u64 = 64;

/// Cumulative playout counters of one remote track since the pipeline added it, in 20 ms
/// frames (kessel-audio's `ParticipantStats` fields of the same names).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PlayoutTotals {
    /// Frames decoded from packets.
    pub decoded: u64,
    /// Frames rebuilt from the next packet's in-band FEC.
    pub fec: u64,
    /// Frames synthesised by loss concealment.
    pub concealed: u64,
}

impl PlayoutTotals {
    /// Counters from a host's own pipeline.
    pub fn new(decoded: u64, fec: u64, concealed: u64) -> PlayoutTotals {
        PlayoutTotals {
            decoded,
            fec,
            concealed,
        }
    }

    fn played(self) -> u64 {
        self.decoded + self.fec + self.concealed
    }
}

/// The audio pipeline's playout counters, which the beacon's `audio_played_ms` and
/// `audio_concealed_ms` come from. [`crate::AudioBridge`] reports kessel-audio's; a host with
/// its own pipeline passes one to [`crate::Call::set_playout_stats`].
pub trait PlayoutStats: Send + Sync + 'static {
    /// Every remote track the pipeline plays, by the id of [`crate::RecvItem::Added`], with its
    /// cumulative counters (read about once a second; cheap).
    fn playout_totals(&self) -> Vec<(u64, PlayoutTotals)>;
}

/// Where a window's played and concealed audio came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PlayoutSource {
    /// The audio pipeline's own counters (kessel-audio through [`crate::AudioBridge`], or the
    /// host's [`PlayoutStats`]).
    Pipeline,
    /// No pipeline reported: the engine's frames and loss marks.
    Engine,
}

/// What became of a window's sample.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BeaconOutcome {
    /// The join answer carried no stats terms (`calls.stats_beacon` off at the join): nothing
    /// was sent.
    Off,
    /// Sew refused an earlier sample with `call_stats_off`: the beacon stopped.
    Stopped,
    /// The join has no call credential to send it with.
    NoCredential,
    /// Sew took it.
    Accepted {
        /// The HTTP status (200).
        http_status: u16,
    },
    /// Sew (or the door in front of it) refused it.
    Refused {
        /// The HTTP status.
        http_status: u16,
        /// `error.code` of the answer, when it had one.
        code: Option<String>,
        /// `error.details.reason`, when it had one.
        reason: Option<String>,
    },
    /// It could not be sent (the network, a timeout, or a sample that failed its own checks).
    Failed {
        /// Why.
        error: String,
    },
}

/// One stats window: the numbers measured, the sample sent (if any) and Sew's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct StatsWindow {
    /// The window's number in this call, from 1.
    pub index: u32,
    /// Its length, ms.
    pub interval_ms: u32,
    /// The leave sample (the partial last window).
    pub last: bool,
    /// Audio played, ms, summed over streams.
    pub audio_played_ms: u32,
    /// Of that, concealed, ms.
    pub audio_concealed_ms: u32,
    /// Audio packets expected, summed over streams.
    pub audio_packets_expected: u32,
    /// Audio packets that arrived (unique).
    pub audio_packets_received: u32,
    /// Of the expected, lost.
    pub audio_packets_lost: u32,
    /// The worst stream's jitter, ms.
    pub audio_jitter_ms: u32,
    /// The longest gap between two packets of one stream, ms (counted when the next packet
    /// arrives; a reconnect starts new streams, so its silence is not counted: see the module
    /// docs).
    pub audio_longest_gap_ms: u32,
    /// Where played and concealed came from.
    pub playout_source: PlayoutSource,
    /// Remote frames that failed to decrypt.
    pub decrypt_failures: u32,
    /// Remote frames dropped because the sender's key was not installed yet (a join or a key
    /// switch; the host's view only, not sent to Sew).
    pub frames_before_key: u64,
    /// Remote frames of tracks the keys did not allow (the host's view only).
    pub refused_frames: u64,
    /// Frames encrypted and sent in the window.
    pub frames_sent: u64,
    /// Remote frames decrypted in the window.
    pub frames_received: u64,
    /// The selected pair's local candidate type (`host`, `relay`), when known.
    pub candidate: Option<String>,
    /// For `relay`: `udp`, `tcp` or `tls`.
    pub relay_protocol: Option<String>,
    /// The selected pair's round-trip time, ms, when known.
    pub rtt_ms: Option<u32>,
    /// The body sent to Sew (absent when nothing was sent).
    pub sample: Option<CallStatsSample>,
    /// What became of it.
    pub beacon: BeaconOutcome,
}

/// What the counters held when the first window started (the call first `connected`): the
/// frames no window covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct BeforeFirstWindow {
    /// Remote frames that failed to decrypt.
    pub decrypt_failures: u64,
    /// Remote frames dropped because the sender's key was not installed yet.
    pub frames_before_key: u64,
    /// Remote frames of tracks the keys did not allow yet.
    pub refused_frames: u64,
    /// Remote frames decrypted.
    pub frames_received: u64,
    /// Frames encrypted and sent.
    pub frames_sent: u64,
}

impl BeforeFirstWindow {
    fn of(c: &CallCounters) -> BeforeFirstWindow {
        BeforeFirstWindow {
            decrypt_failures: c.decrypt_failures,
            frames_before_key: c.frames_before_key,
            refused_frames: c.refused_frames,
            frames_received: c.frames_received,
            frames_sent: c.frames_sent,
        }
    }
}

/// The call's stats so far ([`crate::Call::stats`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CallStatsReport {
    /// The beacon's terms of the current join (`None`: the beacon is off).
    pub terms: Option<CallStatsBeacon>,
    /// The counts from before the first window (`None` until the call first connected).
    pub before_first_window: Option<BeforeFirstWindow>,
    /// The latest windows (at most 256), oldest first.
    pub windows: Vec<StatsWindow>,
    /// Samples sent.
    pub samples_sent: u64,
    /// Samples Sew accepted.
    pub samples_accepted: u64,
}

impl CallStatsReport {
    pub(crate) fn push(&mut self, w: StatsWindow) {
        if w.sample.is_some() {
            self.samples_sent += 1;
        }
        if matches!(w.beacon, BeaconOutcome::Accepted { .. }) {
            self.samples_accepted += 1;
        }
        if self.windows.len() >= WINDOWS_KEPT {
            self.windows.remove(0);
        }
        self.windows.push(w);
    }
}

/// The selected pair's path, as the beacon names it.
pub(crate) type Path = (CallStatsCandidate, Option<CallStatsRelayProtocol>);

#[derive(Debug, Default)]
struct Stream {
    highest: Option<u64>,
    /// The first sequence number this window expects (a new stream: its first packet's; then
    /// one past the highest before the window): expected = highest + 1 − this.
    first_expected: u64,
    /// Unique packets that arrived in this window.
    received: u64,
    last_arrival: Option<Instant>,
    last_ts: u32,
    jitter_s: f64,
    missing: VecDeque<u64>,
}

/// The engine's per-window accounting (owned by the engine task).
#[derive(Debug)]
pub(crate) struct Meter {
    interval: Duration,
    /// The current window's start (`None` until the call first connected).
    started: Option<Instant>,
    index: u32,
    streams: HashMap<u64, Stream>,
    /// Expected and received of the streams removed during this window.
    expected_gone: u64,
    received_gone: u64,
    jitter_peak_s: f64,
    gap_ms: u64,
    played_frames: u64,
    concealed_frames: u64,
    pipeline_seen: bool,
    playout_prev: HashMap<u64, PlayoutTotals>,
    next_poll: Instant,
    counters_at_start: CallCounters,
    rtt: Option<Duration>,
    path: Option<Path>,
}

fn sat_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

impl Meter {
    pub(crate) fn new(now: Instant, terms: Option<&CallStatsBeacon>) -> Meter {
        Meter {
            interval: interval_of(terms),
            started: None,
            index: 0,
            streams: HashMap::new(),
            expected_gone: 0,
            received_gone: 0,
            jitter_peak_s: 0.0,
            gap_ms: 0,
            played_frames: 0,
            concealed_frames: 0,
            pipeline_seen: false,
            playout_prev: HashMap::new(),
            next_poll: now,
            counters_at_start: CallCounters::default(),
            rtt: None,
            path: None,
        }
    }

    /// New terms (a re-join's answer): the next windows take its interval.
    pub(crate) fn set_terms(&mut self, terms: Option<&CallStatsBeacon>) {
        self.interval = interval_of(terms);
    }

    /// The call is connected: the first window starts (once). Returns what the counters held
    /// before it (`None` when the window had already started).
    pub(crate) fn start(
        &mut self,
        now: Instant,
        counters: &CallCounters,
    ) -> Option<BeforeFirstWindow> {
        if self.started.is_some() {
            return None;
        }
        self.started = Some(now);
        self.next_poll = now;
        self.reset_window(counters);
        Some(BeforeFirstWindow::of(counters))
    }

    pub(crate) fn started(&self) -> bool {
        self.started.is_some()
    }

    /// When the engine must wake for the meter.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        let s = self.started?;
        Some((s + self.interval).min(self.next_poll))
    }

    /// The current window is over.
    pub(crate) fn due(&self, now: Instant) -> bool {
        self.started.is_some_and(|s| now >= s + self.interval)
    }

    /// One RTP packet of remote track `slot` (extended sequence number, 48 kHz timestamp).
    pub(crate) fn on_packet(&mut self, slot: u64, seq: u64, ts: u32, now: Instant) {
        let window_start = self.started;
        let st = self.streams.entry(slot).or_default();
        let Some(h) = st.highest else {
            st.highest = Some(seq);
            st.first_expected = seq;
            st.received = 1;
            st.last_arrival = Some(now);
            st.last_ts = ts;
            return;
        };
        if seq <= h {
            // Reordered (or a duplicate): counts only when it was missing.
            if let Some(i) = st.missing.iter().position(|m| *m == seq) {
                st.missing.remove(i);
                st.received += 1;
            }
            return;
        }
        let delta = seq - h;
        if delta <= MISSING_MAX_RUN {
            for m in (h + 1)..seq {
                if st.missing.len() >= MISSING_KEPT {
                    st.missing.pop_front();
                }
                st.missing.push_back(m);
            }
        }
        st.received += 1;
        if let Some(prev) = st.last_arrival {
            let interval_s = now.saturating_duration_since(prev).as_secs_f64();
            // The media time the timestamps say passed (wrapping, as RTP timestamps do).
            let ts_adv_s = f64::from(ts.wrapping_sub(st.last_ts) as i32) / 48_000.0;
            // RFC 3550 §6.4.1: D = (Rj − Ri) − (Sj − Si); J += (|D| − J) / 16.
            let d = interval_s - ts_adv_s;
            st.jitter_s += (d.abs() - st.jitter_s) / 16.0;
            if st.jitter_s > self.jitter_peak_s {
                self.jitter_peak_s = st.jitter_s;
            }
            let interval_ms = (interval_s * 1_000.0).round() as u64;
            let stall_ms = ((interval_s - ts_adv_s.max(0.0)).max(0.0) * 1_000.0).round() as u64;
            let missing_ms = (delta - 1).saturating_mul(FRAME_MS);
            let mut gap = stall_ms.saturating_add(missing_ms).min(interval_ms);
            if let Some(ws) = window_start {
                let in_window = now.saturating_duration_since(ws).as_millis();
                gap = gap.min(u64::try_from(in_window).unwrap_or(u64::MAX));
            }
            if gap > self.gap_ms {
                self.gap_ms = gap;
            }
        }
        st.highest = Some(seq);
        st.last_arrival = Some(now);
        st.last_ts = ts;
    }

    /// Remote track `slot` ended: its counts so far stay in this window.
    pub(crate) fn on_removed(&mut self, slot: u64) {
        if let Some(st) = self.streams.remove(&slot) {
            self.expected_gone += st
                .highest
                .map_or(0, |h| (h + 1).saturating_sub(st.first_expected));
            self.received_gone += st.received;
        }
    }

    /// str0m's `PeerStats`: the selected pair's path and round-trip time.
    pub(crate) fn on_peer(&mut self, path: Option<Path>, rtt: Option<Duration>) {
        if path.is_some() {
            self.path = path;
        }
        if rtt.is_some() {
            self.rtt = rtt;
        }
    }

    /// Reads the playout counters when a second has passed (or `force`).
    pub(crate) fn poll(&mut self, now: Instant, source: Option<&dyn PlayoutStats>, force: bool) {
        if self.started.is_none() || (!force && now < self.next_poll) {
            return;
        }
        self.next_poll = now + POLL_EVERY;
        let Some(src) = source else { return };
        self.pipeline_seen = true;
        let now_totals: HashMap<u64, PlayoutTotals> = src.playout_totals().into_iter().collect();
        for (id, t) in &now_totals {
            let prev = self.playout_prev.get(id).copied().unwrap_or_default();
            self.played_frames += t.played().saturating_sub(prev.played());
            self.concealed_frames += t.concealed.saturating_sub(prev.concealed);
        }
        self.playout_prev = now_totals;
    }

    /// Ends the current window at `now` and starts the next one.
    pub(crate) fn close(
        &mut self,
        now: Instant,
        counters: &CallCounters,
        source: Option<&dyn PlayoutStats>,
        last: bool,
    ) -> StatsWindow {
        self.poll(now, source, true);
        let start = self.started.unwrap_or(now);
        let interval_ms = u64::try_from(now.saturating_duration_since(start).as_millis())
            .unwrap_or(u64::MAX)
            .clamp(1, u64::from(CALL_STATS_WINDOW_MAX_MS));
        let mut expected = self.expected_gone;
        let mut received = self.received_gone;
        for st in self.streams.values() {
            expected += st
                .highest
                .map_or(0, |h| (h + 1).saturating_sub(st.first_expected));
            received += st.received;
        }
        let c0 = &self.counters_at_start;
        let lost_marks = counters.lost_marks.saturating_sub(c0.lost_marks);
        let (played_frames, concealed_frames, playout_source) = if self.pipeline_seen {
            (
                self.played_frames,
                self.concealed_frames,
                PlayoutSource::Pipeline,
            )
        } else {
            let handed = counters.frames_received.saturating_sub(c0.frames_received);
            (handed + lost_marks, lost_marks, PlayoutSource::Engine)
        };
        let played_max = interval_ms * u64::from(CALL_STATS_AUDIO_STREAMS_MAX);
        let played = (played_frames * FRAME_MS).min(played_max);
        let concealed = (concealed_frames * FRAME_MS).min(played);
        let packets_max = u64::from(CALL_STATS_AUDIO_PACKETS_MAX);
        let expected_c = expected.min(packets_max);
        let lost = expected.saturating_sub(received).min(expected_c);
        let jitter_ms = ((self.jitter_peak_s * 1_000.0).round() as u64)
            .min(u64::from(CALL_STATS_AUDIO_JITTER_MAX_MS));
        let (candidate, relay_protocol) = match self.path {
            Some((c, p)) => (
                Some(c.as_str().to_owned()),
                p.map(|p| p.as_str().to_owned()),
            ),
            None => (None, None),
        };
        let w = StatsWindow {
            index: self.index + 1,
            interval_ms: sat_u32(interval_ms),
            last,
            audio_played_ms: sat_u32(played),
            audio_concealed_ms: sat_u32(concealed),
            audio_packets_expected: sat_u32(expected_c),
            audio_packets_received: sat_u32(received.min(packets_max)),
            audio_packets_lost: sat_u32(lost),
            audio_jitter_ms: sat_u32(jitter_ms),
            audio_longest_gap_ms: sat_u32(self.gap_ms.min(interval_ms)),
            playout_source,
            decrypt_failures: sat_u32(
                counters
                    .decrypt_failures
                    .saturating_sub(c0.decrypt_failures)
                    .min(u64::from(CALL_STATS_EVENTS_MAX)),
            ),
            frames_before_key: counters
                .frames_before_key
                .saturating_sub(c0.frames_before_key),
            refused_frames: counters.refused_frames.saturating_sub(c0.refused_frames),
            frames_sent: counters.frames_sent.saturating_sub(c0.frames_sent),
            frames_received: counters.frames_received.saturating_sub(c0.frames_received),
            candidate,
            relay_protocol,
            rtt_ms: self.rtt.map(|r| {
                sat_u32(
                    u64::try_from(r.as_millis())
                        .unwrap_or(u64::MAX)
                        .min(u64::from(CALL_STATS_RTT_MAX_MS)),
                )
            }),
            sample: None,
            beacon: BeaconOutcome::Off,
        };
        self.index += 1;
        self.started = Some(now);
        self.reset_window(counters);
        w
    }

    fn reset_window(&mut self, counters: &CallCounters) {
        for st in self.streams.values_mut() {
            if let Some(h) = st.highest {
                st.first_expected = h + 1;
            }
            st.received = 0;
        }
        self.expected_gone = 0;
        self.received_gone = 0;
        self.jitter_peak_s = 0.0;
        self.gap_ms = 0;
        self.played_frames = 0;
        self.concealed_frames = 0;
        self.pipeline_seen = false;
        self.counters_at_start = counters.clone();
    }
}

fn interval_of(terms: Option<&CallStatsBeacon>) -> Duration {
    let ms = terms.map_or(CALL_STATS_INTERVAL_MS, |t| t.interval_ms);
    Duration::from_millis(u64::from(ms.clamp(1_000, CALL_STATS_WINDOW_MAX_MS)))
}

/// The sample of window `w` under `terms`: `native` and the audio fields only when the terms
/// offer them. `Err` when it fails Sew's own checks (never sent).
pub(crate) fn sample(w: &StatsWindow, terms: &CallStatsBeacon) -> Result<CallStatsSample, String> {
    let audio = terms.audio_fields;
    let candidate = w
        .candidate
        .as_deref()
        .and_then(CallStatsCandidate::from_wire);
    let relay_protocol = match candidate {
        Some(CallStatsCandidate::Relay) => w
            .relay_protocol
            .as_deref()
            .and_then(CallStatsRelayProtocol::from_wire),
        _ => None,
    };
    let s = CallStatsSample {
        interval_ms: w.interval_ms,
        client: if audio {
            CallStatsClient::Native
        } else {
            CallStatsClient::Other
        },
        frames_decoded: 0,
        frames_dropped: 0,
        freezes: 0,
        freeze_ms: 0,
        decrypt_failures: w.decrypt_failures,
        candidate,
        relay_protocol,
        rtt_ms: w.rtt_ms,
        audio_played_ms: audio.then_some(w.audio_played_ms),
        audio_concealed_ms: audio.then_some(w.audio_concealed_ms),
        audio_packets_expected: audio.then_some(w.audio_packets_expected),
        audio_packets_lost: audio.then_some(w.audio_packets_lost),
        audio_jitter_ms: audio.then_some(w.audio_jitter_ms),
        audio_longest_gap_ms: audio.then_some(w.audio_longest_gap_ms),
    };
    s.validate().map_err(|e| format!("invalid sample: {e}"))?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const TERMS: CallStatsBeacon = CallStatsBeacon {
        interval_ms: 15_000,
        audio_fields: true,
    };

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    struct Fixed(Vec<(u64, PlayoutTotals)>);
    impl PlayoutStats for Fixed {
        fn playout_totals(&self) -> Vec<(u64, PlayoutTotals)> {
            self.0.clone()
        }
    }

    #[test]
    fn a_clean_stream_has_no_loss_no_gap_and_little_jitter() {
        let t0 = Instant::now();
        let mut m = Meter::new(t0, Some(&TERMS));
        m.start(t0, &CallCounters::default());
        for i in 0..750u64 {
            m.on_packet(1, 1_000 + i, (i as u32) * 960, t0 + ms(20 * i));
        }
        let w = m.close(t0 + ms(15_000), &CallCounters::default(), None, false);
        assert_eq!(w.interval_ms, 15_000);
        // From the first packet: 750.
        assert_eq!(w.audio_packets_expected, 750);
        assert_eq!(w.audio_packets_lost, 0);
        assert_eq!(w.audio_longest_gap_ms, 0);
        assert_eq!(w.audio_jitter_ms, 0);
        assert_eq!(w.playout_source, PlayoutSource::Engine);
        let s = sample(&w, &TERMS).unwrap();
        assert_eq!(s.client, CallStatsClient::Native);
        assert_eq!(s.audio_packets_lost, Some(0));
    }

    #[test]
    fn loss_and_a_stall_are_gaps_but_dtx_is_not() {
        let t0 = Instant::now();
        let mut m = Meter::new(t0, Some(&TERMS));
        m.start(t0, &CallCounters::default());
        // 0..10 clean, then 5 lost (seq 10..15 missing): the next arrives 120 ms later.
        let mut at = 0u64;
        for i in 0..10u64 {
            m.on_packet(1, i, (i as u32) * 960, t0 + ms(at));
            at += 20;
        }
        at += 100;
        m.on_packet(1, 15, 15 * 960, t0 + ms(at));
        let w = m.close(t0 + ms(1_000), &CallCounters::default(), None, false);
        assert_eq!(w.audio_packets_expected, 16);
        assert_eq!(w.audio_packets_lost, 5);
        assert_eq!(w.audio_longest_gap_ms, 100, "{w:?}");
        // On time, then a stall (seq + 1, timestamp + 960, 300 ms later), then DTX (seq + 1,
        // the timestamps cover 400 ms, 400 ms later).
        let t1 = t0 + ms(1_000);
        m.on_packet(1, 16, 16 * 960, t1 + ms(20));
        m.on_packet(1, 17, 17 * 960, t1 + ms(320));
        m.on_packet(1, 18, 17 * 960 + 400 * 48, t1 + ms(720));
        let w = m.close(t1 + ms(1_000), &CallCounters::default(), None, false);
        assert_eq!(w.audio_packets_expected, 3);
        assert_eq!(w.audio_packets_lost, 0);
        assert_eq!(w.audio_longest_gap_ms, 280, "the stall, not the DTX: {w:?}");
        assert!(w.audio_jitter_ms > 0);
    }

    #[test]
    fn a_late_packet_counts_as_received() {
        let t0 = Instant::now();
        let mut m = Meter::new(t0, Some(&TERMS));
        m.start(t0, &CallCounters::default());
        m.on_packet(1, 0, 0, t0);
        m.on_packet(1, 2, 2 * 960, t0 + ms(40));
        m.on_packet(1, 1, 960, t0 + ms(45));
        m.on_packet(1, 1, 960, t0 + ms(46));
        let w = m.close(t0 + ms(1_000), &CallCounters::default(), None, false);
        assert_eq!(w.audio_packets_expected, 3);
        assert_eq!(w.audio_packets_lost, 0);
    }

    #[test]
    fn the_pipeline_counters_are_differenced_per_window() {
        let t0 = Instant::now();
        let mut m = Meter::new(t0, Some(&TERMS));
        m.start(t0, &CallCounters::default());
        let a = Fixed(vec![(1, PlayoutTotals::new(700, 10, 40))]);
        let w = m.close(t0 + ms(15_000), &CallCounters::default(), Some(&a), false);
        assert_eq!(w.playout_source, PlayoutSource::Pipeline);
        assert_eq!(w.audio_played_ms, 750 * 20);
        assert_eq!(w.audio_concealed_ms, 40 * 20);
        let b = Fixed(vec![(1, PlayoutTotals::new(1_440, 10, 50))]);
        let w = m.close(t0 + ms(30_000), &CallCounters::default(), Some(&b), true);
        assert_eq!(w.audio_played_ms, 750 * 20);
        assert_eq!(w.audio_concealed_ms, 10 * 20);
        assert!(w.last);
    }

    #[test]
    fn the_engine_fallback_uses_frames_and_loss_marks() {
        let t0 = Instant::now();
        let mut m = Meter::new(t0, None);
        m.start(t0, &CallCounters::default());
        let c = CallCounters {
            frames_received: 740,
            lost_marks: 10,
            decrypt_failures: 2,
            frames_sent: 750,
            ..CallCounters::default()
        };
        let w = m.close(t0 + ms(15_000), &c, None, false);
        assert_eq!(w.audio_played_ms, 750 * 20);
        assert_eq!(w.audio_concealed_ms, 200);
        assert_eq!(w.decrypt_failures, 2);
        assert_eq!(w.frames_sent, 750);
    }

    /// Every counted frame is in the bucket before the first window or in exactly one window:
    /// the call's counter is their sum (the harness summary's `counters.decrypt_failures` once
    /// disagreed with the windows' sum by the failures before `connected`). Frames whose key was
    /// not installed yet are never in the sample's `decrypt_failures`.
    #[test]
    fn counters_are_the_bucket_before_the_first_window_plus_the_windows() {
        let t0 = Instant::now();
        let mut m = Meter::new(t0, Some(&TERMS));
        // Before `connected`: the join's key switch.
        let pre = CallCounters {
            decrypt_failures: 1,
            frames_before_key: 15,
            refused_frames: 6,
            frames_received: 20,
            frames_sent: 30,
            ..CallCounters::default()
        };
        let b = m.start(t0, &pre).expect("the first start");
        assert_eq!(
            b,
            BeforeFirstWindow {
                decrypt_failures: 1,
                frames_before_key: 15,
                refused_frames: 6,
                frames_received: 20,
                frames_sent: 30,
            }
        );
        assert_eq!(m.start(t0 + ms(5), &pre), None, "once");
        let mut c = pre.clone();
        c.frames_before_key += 3;
        c.decrypt_failures += 2;
        c.frames_received += 700;
        let w1 = m.close(t0 + ms(15_000), &c, None, false);
        c.refused_frames += 1;
        c.frames_received += 750;
        let w2 = m.close(t0 + ms(30_000), &c, None, true);
        assert_eq!((w1.frames_before_key, w1.decrypt_failures), (3, 2));
        assert_eq!((w2.frames_before_key, w2.decrypt_failures), (0, 0));
        assert_eq!(sample(&w1, &TERMS).unwrap().decrypt_failures, 2);
        let sum = |f: fn(&StatsWindow) -> u64| [&w1, &w2].iter().map(|w| f(w)).sum::<u64>();
        assert_eq!(
            b.decrypt_failures + sum(|w| u64::from(w.decrypt_failures)),
            c.decrypt_failures
        );
        assert_eq!(
            b.frames_before_key + sum(|w| w.frames_before_key),
            c.frames_before_key
        );
        assert_eq!(
            b.refused_frames + sum(|w| w.refused_frames),
            c.refused_frames
        );
        assert_eq!(
            b.frames_received + sum(|w| w.frames_received),
            c.frames_received
        );
    }

    #[test]
    fn without_audio_fields_the_sample_is_the_browsers_shape() {
        let t0 = Instant::now();
        let mut m = Meter::new(t0, None);
        m.start(t0, &CallCounters::default());
        m.on_peer(
            Some((CallStatsCandidate::Relay, Some(CallStatsRelayProtocol::Tls))),
            Some(ms(41)),
        );
        let w = m.close(t0 + ms(15_000), &CallCounters::default(), None, false);
        let old = CallStatsBeacon {
            interval_ms: 15_000,
            audio_fields: false,
        };
        let s = sample(&w, &old).unwrap();
        assert_eq!(s.client, CallStatsClient::Other);
        assert!(!s.has_audio_fields());
        let v = serde_json::to_value(&s).unwrap();
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert!(keys.iter().all(|k| !k.starts_with("audio_")), "{v}");
        assert_eq!(v["candidate"], "relay");
        assert_eq!(v["relay_protocol"], "tls");
        assert_eq!(v["rtt_ms"], 41);
    }

    #[test]
    fn values_are_clamped_to_the_caps() {
        let t0 = Instant::now();
        let mut m = Meter::new(t0, Some(&TERMS));
        m.start(t0, &CallCounters::default());
        m.on_peer(None, Some(Duration::from_secs(120)));
        let w = m.close(t0, &CallCounters::default(), None, true);
        assert_eq!(w.interval_ms, 1, "a window is at least 1 ms");
        assert_eq!(w.rtt_ms, Some(CALL_STATS_RTT_MAX_MS));
        assert!(sample(&w, &TERMS).is_ok());
    }
}
