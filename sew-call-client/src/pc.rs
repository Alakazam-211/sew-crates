//! The str0m peer connection of a voice call: the client is the full-ICE, controlling
//! offerer, DTLS 1.2, RTP mode (the engine writes each Opus packet itself), Opus only with the
//! RFC 6464 audio level, one `_reliable` data channel (as the SDKs open on the publisher PC),
//! one send section for the microphone and the receive sections the engine asks for. With
//! `video-core`, the PC also negotiates VP8 and H.264 (each with RTX, NACK, PLI and FIR) and the
//! Dependency Descriptor, and adds the video receive sections the engine asks for; without it,
//! the offer is audio only, as before.
//!
//! Adapted from Sew's media test client (Alakazam Labs' own str0m glue, not published), trimmed
//! to audio. All of this crate's str0m use lives here.

use std::collections::HashMap;
use std::sync::Arc;
#[cfg(not(feature = "rust-crypto"))]
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use str0m::bwe::{Bitrate, BweKind};
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::config::{CryptoProvider, DtlsVersion};
use str0m::media::{Direction, MediaKind, Mid, Pt, Rid, Simulcast, SimulcastLayer};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{ExtensionValues, RtpWrite};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};

use crate::transport::{Datagram, LocalCandidate, LocalKind};

/// One received RTP packet.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RtpRx {
    /// The mid of its receive section.
    pub(crate) mid: String,
    /// SSRC.
    pub(crate) ssrc: u32,
    /// Extended sequence number.
    pub(crate) seq: u64,
    /// RTP timestamp.
    pub(crate) timestamp: u32,
    /// The marker bit (video: the last packet of a frame).
    pub(crate) marker: bool,
    /// The negotiated video codec of its payload type (`None`: audio, or a payload type that is
    /// no video codec of this PC).
    pub(crate) video: Option<crate::video::VideoCodec>,
    /// Payload (an encrypted Opus frame, or a part of an encrypted video frame).
    pub(crate) payload: Arc<[u8]>,
}

/// What the PC reports.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PcEvent {
    /// ICE state change.
    Ice(IceState),
    /// DTLS completed.
    Connected,
    /// One received RTP packet.
    Rtp(RtpRx),
    /// The SFU asked for a keyframe (PLI or FIR) on our send section `mid`, layer `rid`.
    KeyframeRequest {
        /// The send section.
        mid: String,
        /// The simulcast layer.
        rid: Option<String>,
    },
    /// str0m's send-side bandwidth estimate (TWCC), bit/s.
    Bwe(u64),
    /// str0m's peer stats (about once a second): the selected pair's local candidate address
    /// and its ICE round-trip time.
    Peer {
        /// The local candidate of the selected pair.
        local: Option<std::net::SocketAddr>,
        /// Its latest STUN round-trip time.
        rtt: Option<Duration>,
    },
}

/// The ICE connection state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IceState {
    /// Checking or new.
    Checking,
    /// Connected or completed.
    Connected,
    /// Disconnected.
    Disconnected,
}

#[derive(Clone, Debug)]
enum Change {
    SendAudio {
        stream: String,
        track: String,
    },
    /// A sendonly video section with simulcast send layers `rids` (lowest first).
    SendVideo {
        stream: String,
        track: String,
        rids: Vec<String>,
    },
    /// A send section stops (`inactive`): the engine unpublishes its track.
    StopSend(String),
    RecvAudio,
    RecvVideo,
    Channel(String),
    Renegotiate,
    IceRestart,
}

/// The DTLS/SRTP provider: this crate's pure-Rust one with `rust-crypto`.
#[cfg(feature = "rust-crypto")]
fn provider() -> Arc<CryptoProvider> {
    crate::rust_crypto::provider()
}

/// The DTLS/SRTP provider: str0m's aws-lc-rs one (without `rust-crypto`).
#[cfg(not(feature = "rust-crypto"))]
fn provider() -> Arc<CryptoProvider> {
    static P: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    P.get_or_init(|| Arc::new(str0m::crypto::from_feature_flags()))
        .clone()
}

/// str0m's BWE starts here (the media server's initial estimate).
pub(crate) const BWE_INITIAL_BPS: u64 = 1_500_000;

/// The `a=mid` → `a=msid` map of an SDP (LiveKit's stream id is `PA_…|TR_…`).
pub(crate) fn sdp_msids(sdp: &str) -> HashMap<String, (String, String)> {
    let mut out = HashMap::new();
    let mut mid: Option<String> = None;
    let mut msid: Option<(String, String)> = None;
    let flush = |mid: &mut Option<String>,
                 msid: &mut Option<(String, String)>,
                 out: &mut HashMap<String, (String, String)>| {
        if let (Some(m), Some(s)) = (mid.take(), msid.take()) {
            out.insert(m, s);
        }
    };
    for line in sdp.lines() {
        let line = line.trim_end();
        if line.starts_with("m=") {
            flush(&mut mid, &mut msid, &mut out);
        } else if let Some(m) = line.strip_prefix("a=mid:") {
            mid = Some(m.to_owned());
        } else if let Some(s) = line.strip_prefix("a=msid:") {
            let mut it = s.split_whitespace();
            if let (Some(a), Some(b)) = (it.next(), it.next()) {
                msid = Some((a.to_owned(), b.to_owned()));
            }
        }
    }
    flush(&mut mid, &mut msid, &mut out);
    out
}

/// The call's peer connection.
pub(crate) struct Pc {
    rtc: Rtc,
    pending: Option<SdpPendingOffer>,
    queued: Vec<Change>,
    send_mids: Vec<(String, String)>,
    next_offer_id: u32,
    /// DTLS finished.
    pub(crate) connected: bool,
    /// The latest ICE state.
    pub(crate) ice: IceState,
    msids: HashMap<String, (String, String)>,
    offered_mids: Vec<String>,
    timeout: Instant,
    fed: Option<Instant>,
    /// Errors str0m returned (counted, logged).
    pub(crate) errors: u64,
    /// The local candidates added (a network change invalidates them).
    locals: Vec<Candidate>,
}

impl std::fmt::Debug for Pc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pc")
            .field("connected", &self.connected)
            .field("ice", &self.ice)
            .finish_non_exhaustive()
    }
}

fn ice_candidate(l: &LocalCandidate) -> Option<Candidate> {
    match &l.kind {
        LocalKind::Host => Candidate::host(l.addr, "udp").ok(),
        LocalKind::Relayed { base, .. } => Candidate::relayed(l.addr, *base, "udp").ok(),
    }
}

impl Pc {
    /// A PC with the transport's local candidates.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(locals: &[LocalCandidate], now: Instant) -> Pc {
        Pc::new_with(locals, now, false, true)
    }

    /// A PC with the transport's local candidates; `send_video` (with `video-core`): the call
    /// may publish video (the join's policy has video on and a camera or screen allowed), so
    /// str0m's send-side bandwidth estimator is on too; `h264`: H.264 is negotiated beside VP8
    /// ([`crate::ClientConfig::video_h264`]), else every video section is VP8 only.
    pub(crate) fn new_with(
        locals: &[LocalCandidate],
        now: Instant,
        send_video: bool,
        h264: bool,
    ) -> Pc {
        let cfg = RtcConfig::new()
            .set_crypto_provider(provider())
            .set_dtls_version(DtlsVersion::Dtls12)
            .set_rtp_mode(true)
            .clear_codecs()
            .enable_opus(true, false)
            .set_stats_interval(Some(Duration::from_secs(1)));
        let mut cfg = if VIDEO { video_config(cfg, h264) } else { cfg };
        if VIDEO && send_video {
            cfg = cfg.enable_bwe(Some(Bitrate::bps(BWE_INITIAL_BPS)));
        }
        let mut rtc = cfg.build(now);
        let mut added = Vec::new();
        for l in locals {
            if let Some(c) = ice_candidate(l)
                && rtc.add_local_candidate(c.clone()).is_some()
            {
                added.push(c);
            }
        }
        Pc {
            rtc,
            pending: None,
            queued: vec![Change::Channel("_reliable".to_owned())],
            send_mids: Vec::new(),
            next_offer_id: 1,
            connected: false,
            ice: IceState::Checking,
            msids: HashMap::new(),
            offered_mids: Vec::new(),
            timeout: now,
            fed: None,
            errors: 0,
            locals: added,
        }
    }

    /// A network change: the old transport's local candidates are invalidated (their
    /// pairs go) and the new transport's are added; the next ICE restart offers only these.
    pub(crate) fn replace_local_candidates(&mut self, locals: &[LocalCandidate]) {
        for c in std::mem::take(&mut self.locals) {
            let _ = self.rtc.direct_api().invalidate_candidate(&c);
        }
        for l in locals {
            if let Some(c) = ice_candidate(l)
                && self.rtc.add_local_candidate(c.clone()).is_some()
            {
                self.locals.push(c);
            }
        }
    }

    /// Queues the microphone's send section (msid `stream track`).
    pub(crate) fn add_send_audio(&mut self, stream: &str, track: &str) {
        self.queued.push(Change::SendAudio {
            stream: stream.to_owned(),
            track: track.to_owned(),
        });
    }

    /// Queues `n` recvonly audio sections (`MediaSectionsRequirement`).
    pub(crate) fn add_recv_audio(&mut self, n: u32) {
        for _ in 0..n {
            self.queued.push(Change::RecvAudio);
        }
    }

    /// Queues a sendonly video section for a camera or screen share (msid `stream track`) with
    /// the simulcast send layers `rids`, lowest first (`a=rid:<rid> send`, `a=simulcast:send`).
    pub(crate) fn add_send_video(&mut self, stream: &str, track: &str, rids: &[String]) {
        self.queued.push(Change::SendVideo {
            stream: stream.to_owned(),
            track: track.to_owned(),
            rids: rids.to_vec(),
        });
    }

    /// Queues the end of the send section `mid` (`inactive`): the engine unpublishes its track.
    /// The section is never reused (a new publication takes a new one).
    pub(crate) fn stop_send(&mut self, mid: &str) {
        self.queued.push(Change::StopSend(mid.to_owned()));
    }

    /// Queues a renegotiation without changes.
    pub(crate) fn renegotiate(&mut self) {
        self.queued.push(Change::Renegotiate);
    }

    /// Queues an ICE restart for the next offer (after a resume).
    pub(crate) fn ice_restart(&mut self) {
        self.queued.push(Change::IceRestart);
    }

    /// Drops an outstanding offer (its answer was lost with the old WebSocket).
    pub(crate) fn forget_pending_offer(&mut self) {
        self.pending = None;
    }

    /// Builds an offer from the queued changes if none is outstanding: `(sdp, offer id)`.
    pub(crate) fn negotiate(&mut self) -> Option<(String, u32)> {
        if self.pending.is_some() || self.queued.is_empty() {
            return None;
        }
        let queued = std::mem::take(&mut self.queued);
        let reneg: Option<(Mid, Direction)> =
            if queued.iter().any(|c| matches!(c, Change::Renegotiate)) {
                self.offered_mids.iter().find_map(|m| {
                    let mid: Mid = m.as_str().into();
                    self.rtc.media(mid).map(|x| (mid, x.direction()))
                })
            } else {
                None
            };
        let mut api = self.rtc.sdp_api();
        let mut new_tracks = Vec::new();
        for c in queued {
            match c {
                Change::SendAudio { stream, track } => {
                    let mid = api.add_media(
                        MediaKind::Audio,
                        Direction::SendOnly,
                        Some(stream),
                        Some(track.clone()),
                        None,
                    );
                    new_tracks.push((track, mid.to_string()));
                }
                Change::SendVideo {
                    stream,
                    track,
                    rids,
                } => {
                    let mut sc = Simulcast::new();
                    for r in &rids {
                        sc.add_send_layer(SimulcastLayer::new(r));
                    }
                    let mid = api.add_media(
                        MediaKind::Video,
                        Direction::SendOnly,
                        Some(stream),
                        Some(track.clone()),
                        (!rids.is_empty()).then_some(sc),
                    );
                    new_tracks.push((track, mid.to_string()));
                }
                Change::StopSend(mid) => {
                    api.set_direction(mid.as_str().into(), Direction::Inactive);
                }
                Change::RecvAudio => {
                    api.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
                }
                Change::RecvVideo => {
                    api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
                }
                Change::Channel(label) => {
                    api.add_channel(label);
                }
                Change::Renegotiate => {
                    if let Some((mid, dir)) = reneg {
                        let other = if dir == Direction::Inactive {
                            Direction::RecvOnly
                        } else {
                            Direction::Inactive
                        };
                        api.set_direction(mid, other);
                        api.set_direction(mid, dir);
                    }
                }
                Change::IceRestart => {
                    api.ice_restart(true);
                }
            }
        }
        let (offer, pending) = api.apply()?;
        let text = offer.to_sdp_string();
        for l in text.lines() {
            if let Some(m) = l.trim_end().strip_prefix("a=mid:")
                && !self.offered_mids.iter().any(|x| x == m)
            {
                self.offered_mids.push(m.to_owned());
            }
        }
        self.send_mids.extend(new_tracks);
        self.pending = Some(pending);
        let id = self.next_offer_id;
        self.next_offer_id += 1;
        Some((text, id))
    }

    /// Whether changes wait for the next offer.
    pub(crate) fn needs_negotiation(&self) -> bool {
        !self.queued.is_empty()
    }

    /// Applies the engine's answer.
    pub(crate) fn handle_answer(&mut self, sdp: &str) -> Result<(), String> {
        let Some(pending) = self.pending.take() else {
            return Err("answer without an outstanding offer".to_owned());
        };
        let answer = SdpAnswer::from_sdp_string(sdp).map_err(|e| e.to_string())?;
        self.msids.extend(sdp_msids(sdp));
        self.rtc
            .sdp_api()
            .accept_answer(pending, answer)
            .map_err(|e| e.to_string())
    }

    /// Adds a remote (engine) candidate from a trickle `candidate:` string.
    pub(crate) fn add_remote_candidate(&mut self, candidate: &str) -> bool {
        let s = candidate
            .trim()
            .strip_prefix("a=")
            .unwrap_or(candidate.trim());
        if s.is_empty() {
            return false;
        }
        match Candidate::from_sdp_string(s) {
            Ok(c) => {
                self.rtc.add_remote_candidate(c);
                true
            }
            Err(_) => {
                self.errors += 1;
                false
            }
        }
    }

    /// The stream/track ids (msid) of a mid, as the engine described it.
    pub(crate) fn msid(&self, mid: &str) -> Option<&(String, String)> {
        self.msids.get(mid)
    }

    /// Records a mid → (stream, track) mapping learned from `mid_to_track_id`.
    pub(crate) fn set_msid(&mut self, mid: &str, stream: &str, track: &str) {
        self.msids
            .insert(mid.to_owned(), (stream.to_owned(), track.to_owned()));
    }

    /// The mid of our send section for track id `track`.
    pub(crate) fn mid_for_track(&self, track: &str) -> Option<String> {
        self.send_mids
            .iter()
            .find(|(t, _)| t == track)
            .map(|(_, m)| m.clone())
    }

    /// Feeds one received datagram.
    pub(crate) fn handle_datagram(&mut self, now: Instant, d: &Datagram) {
        let Ok(contents) = d.data.as_slice().try_into() else {
            return;
        };
        let input = Input::Receive(
            now,
            Receive {
                proto: Protocol::Udp,
                source: d.source,
                destination: d.destination,
                contents,
            },
        );
        if self.rtc.accepts(&input) && self.rtc.handle_input(input).is_err() {
            self.errors += 1;
        }
    }

    /// Advances time.
    pub(crate) fn handle_timeout(&mut self, now: Instant) {
        let now = self.fed.map_or(now, |f| now.max(f));
        self.fed = Some(now);
        if self.rtc.handle_input(Input::Timeout(now)).is_err() {
            self.errors += 1;
        }
    }

    /// When [`Pc::handle_timeout`] is due.
    pub(crate) fn timeout(&self) -> Instant {
        self.timeout
    }

    /// Drains str0m's outputs until its next timeout.
    pub(crate) fn poll(&mut self, now: Instant) -> (Vec<Datagram>, Vec<PcEvent>) {
        let mut out = Vec::new();
        let mut raw = Vec::new();
        let mut fed = 0;
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(t)) => {
                    // A due timeout is fed at once (a bounded number of times) so a burst of
                    // writes is not held to one packet per wake-up.
                    if t <= now && self.rtc.is_alive() && fed < 64 {
                        fed += 1;
                        self.handle_timeout(now);
                        continue;
                    }
                    self.timeout = t;
                    break;
                }
                Ok(Output::Transmit(t)) => out.push(Datagram {
                    source: t.source,
                    destination: t.destination,
                    data: t.contents.to_vec(),
                }),
                Ok(Output::Event(e)) => raw.push(e),
                Err(_) => {
                    self.errors += 1;
                    self.timeout = now + Duration::from_millis(10);
                    break;
                }
            }
        }
        let mut events = Vec::new();
        for e in raw {
            self.event(e, &mut events);
        }
        (out, events)
    }

    fn event(&mut self, e: Event, events: &mut Vec<PcEvent>) {
        match e {
            Event::Connected => {
                self.connected = true;
                events.push(PcEvent::Connected);
            }
            Event::IceConnectionStateChange(s) => {
                self.ice = match s {
                    IceConnectionState::New | IceConnectionState::Checking => IceState::Checking,
                    IceConnectionState::Connected | IceConnectionState::Completed => {
                        IceState::Connected
                    }
                    IceConnectionState::Disconnected => IceState::Disconnected,
                };
                events.push(PcEvent::Ice(self.ice));
            }
            Event::RtpPacket(p) => {
                let ssrc = p.header.ssrc;
                let mid = match self.rtc.direct_api().stream_rx(&ssrc) {
                    Some(rx) => rx.mid().to_string(),
                    None => return,
                };
                let video = self.video_codec_of_pt(*p.header.payload_type);
                events.push(PcEvent::Rtp(RtpRx {
                    mid,
                    ssrc: *ssrc,
                    seq: *p.seq_no,
                    timestamp: p.header.timestamp,
                    marker: p.header.marker,
                    video,
                    payload: p.payload.clone(),
                }));
            }
            Event::KeyframeRequest(k) => events.push(PcEvent::KeyframeRequest {
                mid: k.mid.to_string(),
                rid: k.rid.map(|r| r.to_string()),
            }),
            Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                events.push(PcEvent::Bwe(estimate.as_u64()));
            }
            Event::PeerStats(p) => {
                let pair = p.selected_candidate_pair.as_ref();
                events.push(PcEvent::Peer {
                    local: pair.map(|c| c.local.addr),
                    rtt: pair.and_then(|c| c.current_round_trip_time).or(p.rtt),
                });
            }
            _ => {}
        }
    }

    /// The negotiated Opus payload type of `mid`.
    pub(crate) fn opus_pt(&mut self, mid: &str) -> Option<u8> {
        let mid: Mid = mid.into();
        let remote: Vec<Pt> = self.rtc.media(mid)?.remote_pts().to_vec();
        self.rtc
            .codec_config()
            .params()
            .iter()
            .find(|p| p.spec().codec == str0m::format::Codec::Opus && remote.contains(&p.pt()))
            .map(|p| *p.pt())
    }

    /// The SSRC str0m sends `mid` on, once negotiated.
    pub(crate) fn tx_ssrc(&mut self, mid: &str) -> Option<u32> {
        let mid: Mid = mid.into();
        self.rtc
            .direct_api()
            .stream_tx_by_mid(mid, None)
            .map(|s| *s.ssrc())
    }

    /// Writes one Opus packet on the send section `mid`. `level` is RFC 6464's `-dBov` (0
    /// loudest … 127 silent).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_audio(
        &mut self,
        mid: &str,
        pt: u8,
        seq: u64,
        timestamp: u32,
        now: Instant,
        payload: Vec<u8>,
        level: u8,
        voice: bool,
    ) -> Result<(), String> {
        let ev = ExtensionValues {
            audio_level: Some(-(level.min(127) as i8)),
            voice_activity: Some(voice),
            ..Default::default()
        };
        let midv: Mid = mid.into();
        let mut api = self.rtc.direct_api();
        let Some(tx) = api.stream_tx_by_mid(midv, None) else {
            return Err(format!("no send stream for mid {mid}"));
        };
        tx.write_rtp(
            RtpWrite::new(Pt::from(pt), seq.into(), timestamp, now, payload)
                .marker(false)
                .ext_vals(ev)
                .nackable(false),
        );
        Ok(())
    }

    /// The negotiated payload type of `codec` on the send section `mid`: for H.264 one in
    /// packetization mode 1, Constrained Baseline (`42e01f`) first.
    pub(crate) fn video_pt(&mut self, mid: &str, codec: crate::video::VideoCodec) -> Option<u8> {
        let mid: Mid = mid.into();
        let remote: Vec<Pt> = self.rtc.media(mid)?.remote_pts().to_vec();
        self.rtc
            .codec_config()
            .params()
            .iter()
            .filter(|p| remote.contains(&p.pt()))
            .filter_map(|p| {
                let spec = p.spec();
                let rank = match codec {
                    crate::video::VideoCodec::Vp8 => {
                        (spec.codec == str0m::format::Codec::Vp8).then_some(0u8)
                    }
                    crate::video::VideoCodec::H264 => (spec.codec == str0m::format::Codec::H264
                        && spec.format.packetization_mode == Some(1))
                    .then_some(u8::from(spec.format.profile_level_id != Some(0x42e01f))),
                };
                rank.map(|r| (r, *p.pt()))
            })
            .min_by_key(|(r, _)| *r)
            .map(|(_, pt)| pt)
    }

    /// The SSRC str0m sends simulcast layer `rid` of `mid` on, once negotiated.
    pub(crate) fn tx_ssrc_rid(&mut self, mid: &str, rid: &str) -> Option<u32> {
        let mid: Mid = mid.into();
        let rid: Rid = rid.into();
        self.rtc
            .direct_api()
            .stream_tx_by_mid(mid, Some(rid))
            .map(|s| *s.ssrc())
    }

    /// Writes one video packet on layer `rid` of the send section `mid`: nackable, so str0m
    /// answers the SFU's NACKs (over RTX where negotiated); `dd` is the packet's Dependency
    /// Descriptor.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_video(
        &mut self,
        mid: &str,
        rid: &str,
        pt: u8,
        seq: u64,
        timestamp: u32,
        now: Instant,
        payload: Vec<u8>,
        marker: bool,
        dd: Vec<u8>,
    ) -> Result<(), String> {
        let mut ev = ExtensionValues::default();
        if !dd.is_empty() {
            ev.user_values.set(DdRaw(Arc::from(dd)));
        }
        let midv: Mid = mid.into();
        let ridv: Rid = rid.into();
        let mut api = self.rtc.direct_api();
        let Some(tx) = api.stream_tx_by_mid(midv, Some(ridv)) else {
            return Err(format!("no send stream for mid {mid} rid {rid}"));
        };
        tx.write_rtp(
            RtpWrite::new(Pt::from(pt), seq.into(), timestamp, now, payload)
                .marker(marker)
                .ext_vals(ev)
                .nackable(true),
        );
        Ok(())
    }

    /// The bitrate str0m's BWE probes up to (the sum of the wanted layers' caps).
    pub(crate) fn set_desired_bitrate(&mut self, bps: u64) {
        self.rtc.bwe().set_desired_bitrate(Bitrate::bps(bps));
    }

    /// Closes the PC.
    pub(crate) fn disconnect(&mut self) {
        self.rtc.disconnect();
    }
}

// ------------------------------------------------------------------ video (receive)
//
// Kept apart from the audio path: the publish side (simulcast send sections) adds its own
// functions here.

/// Whether this build negotiates video (`video-core`). Without it the PC is audio only and
/// the engine adds no video section.
pub(crate) const VIDEO: bool = cfg!(feature = "video-core");

/// The Dependency Descriptor's header extension id in our offer (the answer maps it by URI).
const DD_EXT_ID: u8 = 12;
/// The Dependency Descriptor's URI (AV1 RTP specification, appendix A), which the media server
/// negotiates for every video codec.
const DD_URI: &str =
    "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";

/// H.264 payload types (main, RTX, profile-level-id), packetization mode 1 with
/// level-asymmetry-allowed: Constrained Baseline 3.1 and Constrained High 3.1.
const H264_PTS: [(u8, u8, u32); 2] = [(108, 109, 0x42e01f), (112, 113, 0x640c1f)];

/// The codecs and the extension video adds to the PC: VP8 (str0m's default, with RTX), with
/// `h264` the two H.264 profiles (each with RTX), NACK, PLI and FIR feedback on all of them
/// (str0m's default for video codecs), and the Dependency Descriptor. mid, rid, repaired-rid,
/// TWCC and abs-send-time are in str0m's standard extension map already. Without `h264` the
/// receive sections offer VP8 only, so the media server never forwards this client H.264.
fn video_config(cfg: RtcConfig, h264: bool) -> RtcConfig {
    let mut cfg = cfg.enable_vp8(true);
    if h264 {
        for (pt, rtx, profile_level_id) in H264_PTS {
            cfg.codec_config()
                .add_h264(pt.into(), Some(rtx.into()), true, profile_level_id);
        }
    }
    cfg.set_extension(
        DD_EXT_ID,
        str0m::rtp::Extension::with_serializer(DD_URI, DdExtension),
    )
}

/// The raw Dependency Descriptor of a packet (the publish side writes it; receive ignores it:
/// the depacketisers read VP8's and H.264's own fields).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DdRaw(pub(crate) Arc<[u8]>);

/// The Dependency Descriptor as an opaque extension: parsed into [`DdRaw`], written from it.
#[derive(Debug)]
struct DdExtension;

impl str0m::rtp::ExtensionSerializer for DdExtension {
    fn write_to(&self, buf: &mut [u8], ev: &ExtensionValues) -> usize {
        match ev.user_values.get::<DdRaw>() {
            Some(d) if d.0.len() <= buf.len() => {
                buf[..d.0.len()].copy_from_slice(&d.0);
                d.0.len()
            }
            _ => 0,
        }
    }

    fn parse_value(&self, buf: &[u8], ev: &mut ExtensionValues) -> bool {
        ev.user_values.set(DdRaw(Arc::from(buf)));
        true
    }

    fn is_video(&self) -> bool {
        true
    }

    fn is_audio(&self) -> bool {
        false
    }

    fn requires_two_byte_form(&self, ev: &ExtensionValues) -> bool {
        ev.user_values
            .get::<DdRaw>()
            .is_some_and(|d| d.0.len() > 16)
    }
}

impl Pc {
    /// Queues `n` recvonly video sections (`MediaSectionsRequirement.num_videos`). Nothing
    /// without `video-core`.
    pub(crate) fn add_recv_video(&mut self, n: u32) {
        if !VIDEO {
            return;
        }
        for _ in 0..n {
            self.queued.push(Change::RecvVideo);
        }
    }

    /// The video codec of a negotiated (main) payload type.
    fn video_codec_of_pt(&self, pt: u8) -> Option<crate::video::VideoCodec> {
        if !VIDEO {
            return None;
        }
        self.rtc
            .codec_config()
            .params()
            .iter()
            .find(|p| *p.pt() == pt)
            .and_then(|p| match p.spec().codec {
                str0m::format::Codec::Vp8 => Some(crate::video::VideoCodec::Vp8),
                str0m::format::Codec::H264 => Some(crate::video::VideoCodec::H264),
                _ => None,
            })
    }

    /// Asks the sender of the remote stream `ssrc` for a keyframe: a PLI, or a FIR (`fir`).
    /// `false` when no such receive stream exists.
    pub(crate) fn request_keyframe(&mut self, ssrc: u32, fir: bool) -> bool {
        let kind = if fir {
            str0m::media::KeyframeRequestKind::Fir
        } else {
            str0m::media::KeyframeRequestKind::Pli
        };
        match self.rtc.direct_api().stream_rx(&ssrc.into()) {
            Some(rx) => {
                rx.request_keyframe(kind);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn first_offer_is_audio_only_with_a_data_channel() {
        let now = Instant::now();
        let locals = [LocalCandidate {
            addr: "127.0.0.1:40000".parse().unwrap(),
            kind: LocalKind::Relayed {
                server: "127.0.0.1:443".parse().unwrap(),
                via: crate::transport::Via::Tls,
                base: "127.0.0.1:50000".parse().unwrap(),
            },
        }];
        let mut pc = Pc::new(&locals, now);
        pc.add_send_audio("PA_me", "mic-1");
        let (sdp, id) = pc.negotiate().unwrap();
        assert_eq!(id, 1);
        assert!(sdp.contains("m=audio"), "{sdp}");
        assert!(!sdp.contains("m=video"), "{sdp}");
        assert!(sdp.contains("m=application"), "{sdp}");
        assert!(sdp.contains("opus"), "{sdp}");
        assert!(sdp.contains("typ relay"), "{sdp}");
        assert!(
            !sdp.contains("typ host"),
            "relay-only offers no host candidate: {sdp}"
        );
        assert!(pc.negotiate().is_none(), "one offer at a time");
        assert!(pc.mid_for_track("mic-1").is_some());
    }

    #[test]
    fn a_video_build_adds_video_sections_only_when_asked() {
        let now = Instant::now();
        let locals = [LocalCandidate {
            addr: "127.0.0.1:40000".parse().unwrap(),
            kind: LocalKind::Host,
        }];
        let mut pc = Pc::new(&locals, now);
        pc.add_send_audio("PA_me", "mic-1");
        let (first, _) = pc.negotiate().unwrap();
        assert!(!first.contains("m=video"), "{first}");
        assert!(!first.contains(DD_URI), "{first}");
        let mut pc = Pc::new(&locals, now);
        pc.add_send_audio("PA_me", "mic-1");
        let queued = pc.queued.len();
        pc.add_recv_video(2);
        if !VIDEO {
            assert_eq!(
                pc.queued.len(),
                queued,
                "no video sections without video-core"
            );
            return;
        }
        let (sdp, _) = pc.negotiate().unwrap();
        assert_eq!(sdp.matches("m=video").count(), 2, "{sdp}");
        let video = &sdp[sdp.find("m=video").unwrap()..];
        assert!(video.contains("a=recvonly"), "{video}");
        assert!(video.contains("VP8/90000"), "{video}");
        assert!(video.contains("H264/90000"), "{video}");
        assert!(video.contains("rtx/90000"), "{video}");
        for plid in ["42e01f", "640c1f"] {
            let fmtp = video
                .lines()
                .find(|l| {
                    l.starts_with("a=fmtp:") && l.contains(&format!("profile-level-id={plid}"))
                })
                .unwrap_or_else(|| panic!("no H.264 {plid} fmtp: {video}"));
            assert!(fmtp.contains("packetization-mode=1"), "{fmtp}");
            assert!(fmtp.contains("level-asymmetry-allowed=1"), "{fmtp}");
        }
        assert!(video.contains("nack pli"), "{video}");
        assert!(video.contains("ccm fir"), "{video}");
        assert!(video.contains(DD_URI), "{video}");
        for uri in [
            "urn:ietf:params:rtp-hdrext:sdes:mid",
            "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id",
            "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id",
            "transport-wide-cc",
            "abs-send-time",
        ] {
            assert!(video.contains(uri), "{uri}: {video}");
        }
        let audio = &sdp[sdp.find("m=audio").unwrap()..sdp.find("m=video").unwrap()];
        assert!(!audio.contains(DD_URI), "the DD is video only: {audio}");
    }

    #[test]
    fn the_camera_offer_has_the_opened_layers_rids_and_simulcast() {
        if !VIDEO {
            return;
        }
        let now = Instant::now();
        let locals = [LocalCandidate {
            addr: "127.0.0.1:40000".parse().unwrap(),
            kind: LocalKind::Host,
        }];
        let mut pc = Pc::new_with(&locals, now, true, true);
        // Two layers opened (q, h): only theirs are offered.
        pc.add_send_video("PA_me", "cam-1", &["q".to_owned(), "h".to_owned()]);
        let (sdp, _) = pc.negotiate().unwrap();
        let video = &sdp[sdp.find("m=video").expect("a video section")..];
        for want in [
            "a=sendonly",
            "a=rid:q send",
            "a=rid:h send",
            "a=simulcast:send q;h",
            "VP8/90000",
            "H264/90000",
            "packetization-mode=1",
            "profile-level-id=42e01f",
            "rtx/90000",
            DD_URI,
            "a=msid:PA_me cam-1",
        ] {
            assert!(video.contains(want), "{want} missing: {video}");
        }
        assert!(!video.contains("a=rid:f"), "{video}");
        assert!(pc.mid_for_track("cam-1").is_some());
    }

    #[test]
    fn without_h264_the_receive_sections_offer_vp8_only() {
        if !VIDEO {
            return;
        }
        let now = Instant::now();
        let locals = [LocalCandidate {
            addr: "127.0.0.1:40000".parse().unwrap(),
            kind: LocalKind::Host,
        }];
        let mut pc = Pc::new_with(&locals, now, false, false);
        pc.add_send_audio("PA_me", "mic-1");
        pc.add_recv_video(2);
        let (sdp, _) = pc.negotiate().unwrap();
        assert_eq!(sdp.matches("m=video").count(), 2, "{sdp}");
        let video = &sdp[sdp.find("m=video").unwrap()..];
        assert!(video.contains("a=recvonly"), "{video}");
        assert!(video.contains("VP8/90000"), "{video}");
        assert!(video.contains("rtx/90000"), "{video}");
        assert!(!video.contains("H264"), "no H.264 offered: {video}");
        assert!(!video.contains("profile-level-id"), "{video}");
        for line in video.lines().filter(|l| l.starts_with("m=video")) {
            // VP8 and its RTX: two payload types.
            assert_eq!(line.split(' ').skip(3).count(), 2, "{line}");
        }
    }

    #[test]
    fn linux_negotiates_vp8_only_by_default() {
        let cfg = crate::ClientConfig::new("t", std::env::temp_dir());
        assert_eq!(cfg.video_h264, !cfg!(target_os = "linux"));
        assert!(cfg.clone().with_video_h264(true).video_h264);
        assert!(!cfg.with_video_h264(false).video_h264);
    }

    #[test]
    fn msids_by_mid() {
        let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:1\r\na=msid:PA_x|TR_y TR_y\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:2\r\n";
        let m = sdp_msids(sdp);
        assert_eq!(m.get("1").unwrap().0, "PA_x|TR_y");
        assert!(!m.contains_key("2"));
    }
}
