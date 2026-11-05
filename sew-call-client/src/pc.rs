//! The str0m peer connection of a voice call: the client is the full-ICE, controlling
//! offerer, DTLS 1.2, RTP mode (the engine writes each Opus packet itself), Opus only with the
//! RFC 6464 audio level, one `_reliable` data channel (as the SDKs open on the publisher PC),
//! one send section for the microphone and the receive sections the engine asks for.
//!
//! Adapted from Sew's media test client (Alakazam Labs' own str0m glue, not published), trimmed
//! to audio. All of this crate's str0m use lives here.

use std::collections::HashMap;
use std::sync::Arc;
#[cfg(not(feature = "rust-crypto"))]
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::config::{CryptoProvider, DtlsVersion};
use str0m::media::{Direction, MediaKind, Mid, Pt};
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
    /// Payload (an encrypted Opus frame).
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
    SendAudio { stream: String, track: String },
    RecvAudio,
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
    pub(crate) fn new(locals: &[LocalCandidate], now: Instant) -> Pc {
        let cfg = RtcConfig::new()
            .set_crypto_provider(provider())
            .set_dtls_version(DtlsVersion::Dtls12)
            .set_rtp_mode(true)
            .clear_codecs()
            .enable_opus(true, false)
            .set_stats_interval(Some(Duration::from_secs(1)));
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
                Change::RecvAudio => {
                    api.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
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
                events.push(PcEvent::Rtp(RtpRx {
                    mid,
                    ssrc: *ssrc,
                    seq: *p.seq_no,
                    timestamp: p.header.timestamp,
                    payload: p.payload.clone(),
                }));
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

    /// Closes the PC.
    pub(crate) fn disconnect(&mut self) {
        self.rtc.disconnect();
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
    fn msids_by_mid() {
        let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:1\r\na=msid:PA_x|TR_y TR_y\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:2\r\n";
        let m = sdp_msids(sdp);
        assert_eq!(m.get("1").unwrap().0, "PA_x|TR_y");
        assert!(!m.contains_key("2"));
    }
}
