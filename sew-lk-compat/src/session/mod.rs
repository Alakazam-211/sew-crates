// Copyright 2023 LiveKit, Inc.
// Copyright 2026 Alakazam Labs
// SPDX-License-Identifier: Apache-2.0
// Rewritten from a written behaviour specification of the earlier port of livekit/livekit's test client (Apache-2.0).

//! The signalling state of a voice client with one peer connection, without any I/O.
//!
//! The caller owns the socket, the clock, the peer connection, the media and the keys. It feeds
//! server messages to [`Session::handle_response`], calls [`Session::handle_timeout`] when
//! [`Session::poll_timeout`] says so, asks for client requests through methods such as
//! [`Session::publish_audio`] or [`Session::offer`], and drains two queues: requests to send
//! ([`Session::poll_request`]) and things that happened ([`Session::poll_event`]).
//!
//! The client publishes one microphone track and receives on sections the server asks for; there
//! is no video publish, simulcast, data channel or second peer connection here.
//!
//! Lifecycle as the caller sees it: a fresh session sends no keep-alives; the join answer starts
//! them; [`Session::leave`] and [`Session::signal_lost`] stop them (the roster stays); a resume
//! answer starts them again; [`Session::reset`] goes back to fresh but remembers the refresh
//! token. Server messages are taken in any order and none is fatal.

mod heartbeat;
mod roster;

use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;

use sew_lkproto::livekit::signal_request::Message as Req;
use sew_lkproto::livekit::signal_response::Message as Msg;
use sew_lkproto::livekit::{
    AddTrackRequest, ClientConfigSetting, DisconnectReason, IceServer, JoinResponse, LeaveRequest,
    MuteTrackRequest, ParticipantInfo, Ping, SessionDescription, SignalRequest, SignalResponse,
    SpeakerInfo, StreamStateInfo, SubscribedQualityUpdate, TrackInfo, TrackSource, TrackType,
    encryption, leave_request, request_response,
};

use crate::signal::{CandidateInit, response_kind, trickle_for};

use heartbeat::Heartbeat;
use roster::Roster;

/// `TrackInfo.encryption` / `AddTrackRequest.encryption` value for AES-GCM frame encryption.
pub const ENCRYPTION_GCM: i32 = encryption::Type::Gcm as i32;

/// What a server-sent leave asks the client to do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LeaveAction {
    /// Hang up for good (also used for any action value this client does not know).
    Disconnect,
    /// Reconnect the signalling socket and resume the same participant.
    Resume,
    /// Start over with a fresh join.
    Reconnect,
}

/// Something the caller has to react to, in arrival order.
// `TrackPublished` carries a whole `TrackInfo`; the variants and their field types are the
// public contract the caller matches on, so no boxing.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum SessionEvent {
    /// The join answer: who we are, which ICE servers to use and whether relay is mandatory.
    Joined {
        /// Our identity (empty when the answer named no participant).
        identity: String,
        /// Our participant sid (empty when the answer named no participant).
        sid: String,
        /// ICE servers, exactly as the server listed them.
        ice_servers: Vec<IceServer>,
        /// True only when the server's client configuration enables forced relay.
        force_relay: bool,
    },
    /// The server's answer to our offer.
    Answer {
        /// Answer SDP.
        sdp: String,
        /// Offer id this answers.
        id: u32,
        /// Receive-section mid to track sid.
        mid_to_track_id: BTreeMap<String, String>,
    },
    /// A server-initiated offer (a single-connection client does not expect one).
    Offer {
        /// Offer SDP.
        sdp: String,
        /// Offer id.
        id: u32,
    },
    /// A remote ICE candidate line; empty marks the end of candidates.
    RemoteCandidate {
        /// The `candidate:` line.
        candidate: String,
    },
    /// The server wants this many receive sections and a new offer (sent even for 0 and 0).
    MediaSections {
        /// Audio receive sections.
        audios: u32,
        /// Video receive sections.
        videos: u32,
    },
    /// The server accepted a track we asked to publish.
    TrackPublished {
        /// The client id we gave the track.
        cid: String,
        /// The server's record of it.
        track: TrackInfo,
    },
    /// The roster changed (see [`Session::remotes`]).
    Participants,
    /// The active speakers, as the server listed them.
    Speakers(Vec<SpeakerInfo>),
    /// The server muted or unmuted a track.
    Muted {
        /// Track sid.
        sid: String,
        /// New state.
        muted: bool,
    },
    /// A resume succeeded; keep-alives are running again.
    Resumed {
        /// ICE servers for the resumed connection.
        ice_servers: Vec<IceServer>,
    },
    /// The server asked us to leave.
    Leave {
        /// Disconnect reason code.
        reason: i32,
        /// What to do next.
        action: LeaveAction,
    },
    /// The server paused or resumed tracks it sends us (`StreamStateUpdate`: a congested
    /// downlink pauses video, never audio).
    StreamStates(Vec<StreamStateInfo>),
    /// The server says which simulcast qualities of our track anyone subscribes to
    /// (dynacast).
    SubscribedQuality(SubscribedQualityUpdate),
    /// The server unpublished one of our tracks.
    TrackUnpublished {
        /// Track sid.
        sid: String,
    },
    /// The server turned down one of our requests.
    Refused {
        /// Id of the refused request.
        request_id: u32,
        /// Reason code (never OK).
        reason: i32,
        /// Server's explanation.
        message: String,
    },
}

/// Diagnostics: what the session has seen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionCounters {
    /// Messages received, by [`response_kind`] name.
    pub responses: BTreeMap<String, u64>,
    /// Messages this client does not handle (including ones with no content).
    pub unknown: u64,
    /// Remote trickles whose candidate JSON did not parse.
    pub bad_trickles: u64,
}

/// The signalling session (see the module docs).
#[derive(Debug)]
pub struct Session {
    beat: Heartbeat,
    me: Option<ParticipantInfo>,
    others: Roster,
    refresh: Option<String>,
    to_send: VecDeque<SignalRequest>,
    happened: VecDeque<SessionEvent>,
    seen: SessionCounters,
    has_joined: bool,
}

/// How one server message was dealt with, for the counters and the event queue. Short-lived (one
/// per message), so the size of the event it may carry does not matter.
#[allow(clippy::large_enum_variant)]
enum Outcome {
    Emit(SessionEvent),
    Handled,
    Unhandled,
}

fn request(m: Req) -> SignalRequest {
    SignalRequest { message: Some(m) }
}

fn leave_action(wire: i32) -> LeaveAction {
    match leave_request::Action::try_from(wire) {
        Ok(leave_request::Action::Resume) => LeaveAction::Resume,
        Ok(leave_request::Action::Reconnect) => LeaveAction::Reconnect,
        Ok(leave_request::Action::Disconnect) | Err(_) => LeaveAction::Disconnect,
    }
}

impl Session {
    /// A fresh session whose clock starts at `now`.
    pub fn new(now: Instant) -> Session {
        Session {
            beat: Heartbeat::new(now),
            me: None,
            others: Roster::default(),
            refresh: None,
            to_send: VecDeque::new(),
            happened: VecDeque::new(),
            seen: SessionCounters::default(),
            has_joined: false,
        }
    }

    /// Take one server message.
    pub fn handle_response(&mut self, now: Instant, r: SignalResponse) {
        *self
            .seen
            .responses
            .entry(response_kind(&r).to_owned())
            .or_default() += 1;
        let outcome = match r.message {
            Some(m) => self.dispatch(now, m),
            None => Outcome::Unhandled,
        };
        match outcome {
            Outcome::Emit(e) => self.happened.push_back(e),
            Outcome::Handled => {}
            Outcome::Unhandled => self.seen.unknown += 1,
        }
    }

    fn dispatch(&mut self, now: Instant, m: Msg) -> Outcome {
        match m {
            Msg::Join(j) => {
                self.joined_room(now, j);
                Outcome::Handled
            }
            Msg::Reconnect(rc) => {
                self.beat.start(now);
                Outcome::Emit(SessionEvent::Resumed {
                    ice_servers: rc.ice_servers,
                })
            }
            Msg::Update(u) => {
                for p in u.participants {
                    match &mut self.me {
                        Some(me) if me.sid == p.sid => *me = p,
                        _ => self.others.apply(p),
                    }
                }
                Outcome::Emit(SessionEvent::Participants)
            }
            Msg::RefreshToken(t) => {
                self.refresh = Some(t);
                Outcome::Handled
            }
            Msg::Pong(stamp) => {
                self.beat.pong(now, stamp);
                Outcome::Handled
            }
            Msg::PongResp(p) => {
                self.beat.pong(now, p.timestamp);
                Outcome::Handled
            }
            Msg::Trickle(t) => match serde_json::from_str::<CandidateInit>(&t.candidate_init) {
                Ok(c) => Outcome::Emit(SessionEvent::RemoteCandidate {
                    candidate: c.candidate,
                }),
                Err(_) => {
                    self.seen.bad_trickles += 1;
                    Outcome::Handled
                }
            },
            Msg::Answer(d) => Outcome::Emit(SessionEvent::Answer {
                sdp: d.sdp,
                id: d.id,
                mid_to_track_id: d.mid_to_track_id.into_iter().collect(),
            }),
            Msg::Offer(d) => Outcome::Emit(SessionEvent::Offer {
                sdp: d.sdp,
                id: d.id,
            }),
            Msg::TrackPublished(tp) => match tp.track {
                Some(track) => Outcome::Emit(SessionEvent::TrackPublished { cid: tp.cid, track }),
                None => Outcome::Handled,
            },
            Msg::MediaSectionsRequirement(ms) => Outcome::Emit(SessionEvent::MediaSections {
                audios: ms.num_audios,
                videos: ms.num_videos,
            }),
            Msg::SpeakersChanged(s) => Outcome::Emit(SessionEvent::Speakers(s.speakers)),
            Msg::StreamStateUpdate(u) => Outcome::Emit(SessionEvent::StreamStates(u.stream_states)),
            Msg::Mute(mt) => Outcome::Emit(SessionEvent::Muted {
                sid: mt.sid,
                muted: mt.muted,
            }),
            Msg::Leave(l) => Outcome::Emit(SessionEvent::Leave {
                reason: l.reason,
                action: leave_action(l.action),
            }),
            Msg::RequestResponse(rr) if rr.reason != request_response::Reason::Ok as i32 => {
                Outcome::Emit(SessionEvent::Refused {
                    request_id: rr.request_id,
                    reason: rr.reason,
                    message: rr.message,
                })
            }
            Msg::RequestResponse(_) => Outcome::Handled,
            // A publisher's notices: the qualities subscribed (dynacast) and an unpublication.
            Msg::SubscribedQualityUpdate(u) => Outcome::Emit(SessionEvent::SubscribedQuality(u)),
            Msg::TrackUnpublished(t) => {
                Outcome::Emit(SessionEvent::TrackUnpublished { sid: t.track_sid })
            }
            // Room, quality, subscription and codec notices: nothing for a voice client to do.
            Msg::RoomUpdate(_)
            | Msg::RoomMoved(_)
            | Msg::ConnectionQuality(_)
            | Msg::TrackSubscribed(_)
            | Msg::SubscriptionResponse(_)
            | Msg::SubscriptionPermissionUpdate(_)
            | Msg::SubscribedAudioCodecUpdate(_) => Outcome::Handled,
            // Data tracks and data blobs are not part of this client.
            Msg::PublishDataTrackResponse(_)
            | Msg::UnpublishDataTrackResponse(_)
            | Msg::DataTrackSubscriberHandles(_)
            | Msg::StoreDataBlobResponse(_)
            | Msg::GetDataBlobResponse(_) => Outcome::Unhandled,
        }
    }

    fn joined_room(&mut self, now: Instant, j: JoinResponse) {
        let (identity, sid) = j
            .participant
            .as_ref()
            .map(|p| (p.identity.clone(), p.sid.clone()))
            .unwrap_or_default();
        let force_relay = j
            .client_configuration
            .as_ref()
            .is_some_and(|c| c.force_relay == ClientConfigSetting::Enabled as i32);
        self.me = j.participant;
        self.others.reload(j.other_participants);
        self.beat.adopt_period_secs(j.ping_interval);
        self.beat.start(now);
        self.has_joined = true;
        self.happened.push_back(SessionEvent::Joined {
            identity,
            sid,
            ice_servers: j.ice_servers,
            force_relay,
        });
        self.happened.push_back(SessionEvent::Participants);
    }

    /// Send a keep-alive if one is due at `now` (a legacy ping, then a ping carrying the RTT).
    pub fn handle_timeout(&mut self, now: Instant) {
        if let Some((timestamp, rtt)) = self.beat.tick(now) {
            self.to_send.push_back(request(Req::Ping(timestamp)));
            self.to_send
                .push_back(request(Req::PingReq(Ping { timestamp, rtt })));
        }
    }

    /// When the next keep-alive is due; `None` while none is scheduled.
    pub fn poll_timeout(&self) -> Option<Instant> {
        self.beat.next_due()
    }

    /// The oldest request waiting to be sent.
    pub fn poll_request(&mut self) -> Option<SignalRequest> {
        self.to_send.pop_front()
    }

    /// The oldest event waiting for the caller.
    pub fn poll_event(&mut self) -> Option<SessionEvent> {
        self.happened.pop_front()
    }

    /// Ask to publish the microphone: an encrypted (GCM) audio track without RED.
    pub fn publish_audio(&mut self, cid: &str, name: &str, muted: bool) {
        self.to_send
            .push_back(request(Req::AddTrack(AddTrackRequest {
                cid: cid.to_owned(),
                name: name.to_owned(),
                r#type: TrackType::Audio as i32,
                source: TrackSource::Microphone as i32,
                muted,
                disable_red: true,
                encryption: ENCRYPTION_GCM,
                ..AddTrackRequest::default()
            })));
    }

    /// Ask to publish a track described by the caller (video: its layers and simulcast
    /// codecs). The caller sets `encryption`; [`Session::publish_audio`] is the microphone's.
    pub fn add_track(&mut self, add: AddTrackRequest) {
        self.to_send.push_back(request(Req::AddTrack(add)));
    }

    fn description(kind: &str, sdp: String, id: u32) -> SessionDescription {
        SessionDescription {
            r#type: kind.to_owned(),
            sdp,
            id,
            ..SessionDescription::default()
        }
    }

    /// Send our offer.
    pub fn offer(&mut self, sdp: String, id: u32) {
        let d = Self::description("offer", sdp, id);
        self.to_send.push_back(request(Req::Offer(d)));
    }

    /// Send an answer to a server offer.
    pub fn answer(&mut self, sdp: String, id: u32) {
        let d = Self::description("answer", sdp, id);
        self.to_send.push_back(request(Req::Answer(d)));
    }

    /// Send one local candidate for the publisher connection.
    pub fn trickle(&mut self, candidate: &str) {
        self.to_send
            .push_back(trickle_for(candidate, None, 0, false));
    }

    /// Mute or unmute one of our tracks.
    pub fn mute(&mut self, sid: &str, muted: bool) {
        self.to_send.push_back(request(Req::Mute(MuteTrackRequest {
            sid: sid.to_owned(),
            muted,
        })));
    }

    /// Say goodbye (client initiated, disconnect) after anything already queued, and stop the
    /// keep-alives.
    pub fn leave(&mut self) {
        self.to_send.push_back(request(Req::Leave(LeaveRequest {
            reason: DisconnectReason::ClientInitiated as i32,
            action: leave_request::Action::Disconnect as i32,
            ..LeaveRequest::default()
        })));
        self.beat.stop();
    }

    /// The socket is gone: stop the keep-alives and drop unsent requests. Events, roster and
    /// tokens stay.
    pub fn signal_lost(&mut self) {
        self.beat.stop();
        self.to_send.clear();
    }

    /// Back to a fresh session with its clock at `now`, keeping only the refresh token.
    pub fn reset(&mut self, now: Instant) {
        let kept = self.refresh.take();
        *self = Session::new(now);
        self.refresh = kept;
    }

    /// Our own participant record, from the join answer or a later update for our sid.
    pub fn local(&self) -> Option<&ParticipantInfo> {
        self.me.as_ref()
    }

    /// Everyone else in the room, by participant sid (never ourselves, never a departed entry).
    pub fn remotes(&self) -> &BTreeMap<String, ParticipantInfo> {
        self.others.as_map()
    }

    /// The remote participant publishing the track with this sid.
    pub fn publisher_of(&self, track_sid: &str) -> Option<&ParticipantInfo> {
        self.others.owner_of(track_sid)
    }

    /// The newest refresh token from the server, if any.
    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh.as_deref()
    }

    /// The last measured signalling round trip, in milliseconds.
    pub fn rtt_ms(&self) -> Option<u64> {
        self.beat.rtt_ms()
    }

    /// Whether a join answer has arrived since the session was created or reset.
    pub fn joined(&self) -> bool {
        self.has_joined
    }

    /// Diagnostic counters.
    pub fn counters(&self) -> &SessionCounters {
        &self.seen
    }
}

#[cfg(test)]
mod tests;
