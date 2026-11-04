// Copyright 2023 LiveKit, Inc.
// Copyright 2026 Alakazam Labs
// SPDX-License-Identifier: Apache-2.0
// Rewritten from a written behaviour specification of the earlier port of livekit/livekit's test client (Apache-2.0).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use sew_lkproto::livekit::participant_info::State;
use sew_lkproto::livekit::{
    ClientConfiguration, MediaSectionsRequirement, ParticipantUpdate, Pong, ReconnectResponse,
    RequestResponse, RoomUpdate, SpeakersChanged, StoreDataBlobResponse, TrackPublishedResponse,
    TrickleRequest,
};

use super::*;

fn person(sid: &str, identity: &str, state: State) -> ParticipantInfo {
    ParticipantInfo {
        sid: sid.to_owned(),
        identity: identity.to_owned(),
        state: state as i32,
        ..ParticipantInfo::default()
    }
}

fn with_track(mut p: ParticipantInfo, track_sid: &str) -> ParticipantInfo {
    p.tracks.push(TrackInfo {
        sid: track_sid.to_owned(),
        ..TrackInfo::default()
    });
    p
}

fn msg(m: Msg) -> SignalResponse {
    SignalResponse { message: Some(m) }
}

fn join(others: Vec<ParticipantInfo>, ping_secs: i32, relay: Option<ClientConfigSetting>) -> Msg {
    Msg::Join(JoinResponse {
        participant: Some(person("PA_me", "p_me", State::Active)),
        other_participants: others,
        ping_interval: ping_secs,
        ice_servers: vec![IceServer {
            urls: vec!["turns:turn.example:443".to_owned()],
            ..IceServer::default()
        }],
        client_configuration: relay.map(|s| ClientConfiguration {
            force_relay: s as i32,
            ..ClientConfiguration::default()
        }),
        ..JoinResponse::default()
    })
}

fn update(people: Vec<ParticipantInfo>) -> Msg {
    Msg::Update(ParticipantUpdate {
        participants: people,
    })
}

fn events(s: &mut Session) -> Vec<SessionEvent> {
    std::iter::from_fn(|| s.poll_event()).collect()
}

fn requests(s: &mut Session) -> Vec<Req> {
    std::iter::from_fn(|| s.poll_request())
        .map(|r| r.message.expect("request has a message"))
        .collect()
}

fn joined_session(t0: Instant) -> Session {
    let mut s = Session::new(t0);
    s.handle_response(
        t0,
        msg(join(vec![person("PA_b", "p_b", State::Active)], 0, None)),
    );
    let _ = events(&mut s);
    s
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn join_reports_relay_roster_and_pings() {
    let t0 = Instant::now();
    let mut s = Session::new(t0);
    s.handle_response(
        t0,
        msg(join(
            vec![person("PA_b", "p_b", State::Active)],
            2,
            Some(ClientConfigSetting::Enabled),
        )),
    );
    let ev = events(&mut s);
    assert_eq!(ev.len(), 2, "{ev:?}");
    match &ev[0] {
        SessionEvent::Joined {
            identity,
            sid,
            ice_servers,
            force_relay,
        } => {
            assert_eq!(identity, "p_me");
            assert_eq!(sid, "PA_me");
            assert_eq!(ice_servers.len(), 1);
            assert_eq!(ice_servers[0].urls, ["turns:turn.example:443"]);
            assert!(*force_relay);
        }
        other => panic!("first event should be Joined, got {other:?}"),
    }
    assert_eq!(ev[1], SessionEvent::Participants);
    assert_eq!(s.remotes().len(), 1);
    assert!(s.joined());
    assert!(s.poll_request().is_none(), "a join answer sends nothing");

    assert_eq!(s.poll_timeout(), Some(t0 + Duration::from_secs(2)));
    s.handle_timeout(t0 + Duration::from_secs(2));
    let sent = requests(&mut s);
    assert_eq!(
        sent,
        vec![
            Req::Ping(2000),
            Req::PingReq(Ping {
                timestamp: 2000,
                rtt: 0
            })
        ]
    );

    s.handle_response(
        t0 + ms(2040),
        msg(Msg::PongResp(Pong {
            last_ping_timestamp: 999_999,
            timestamp: 2000,
        })),
    );
    assert_eq!(s.rtt_ms(), Some(40));

    // The next ping carries the measured RTT.
    s.handle_timeout(t0 + Duration::from_secs(4));
    assert_eq!(
        requests(&mut s),
        vec![
            Req::Ping(4000),
            Req::PingReq(Ping {
                timestamp: 4000,
                rtt: 40
            })
        ]
    );
}

#[test]
fn force_relay_unset_is_false() {
    for setting in [
        None,
        Some(ClientConfigSetting::Unset),
        Some(ClientConfigSetting::Disabled),
    ] {
        let t0 = Instant::now();
        let mut s = Session::new(t0);
        s.handle_response(t0, msg(join(vec![], 0, setting)));
        match s.poll_event() {
            Some(SessionEvent::Joined { force_relay, .. }) => {
                assert!(!force_relay, "{setting:?}");
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn roster_updates_remove_disconnected() {
    let t0 = Instant::now();
    let mut s = joined_session(t0);
    assert_eq!(s.remotes().keys().collect::<Vec<_>>(), ["PA_b"]);
    s.handle_response(
        t0,
        msg(update(vec![
            person("PA_b", "p_b", State::Disconnected),
            with_track(person("PA_c", "p_c", State::Joined), "TR_c"),
        ])),
    );
    assert_eq!(events(&mut s), vec![SessionEvent::Participants]);
    assert_eq!(s.remotes().keys().collect::<Vec<_>>(), ["PA_c"]);
    assert_eq!(s.publisher_of("TR_c").expect("found").identity, "p_c");
    assert!(s.publisher_of("TR_none").is_none());
}

#[test]
fn join_skips_departed_and_replaces_the_roster() {
    let t0 = Instant::now();
    let mut s = Session::new(t0);
    s.handle_response(
        t0,
        msg(join(
            vec![
                person("PA_b", "p_b", State::Active),
                person("PA_gone", "p_gone", State::Disconnected),
            ],
            0,
            None,
        )),
    );
    assert_eq!(s.remotes().keys().collect::<Vec<_>>(), ["PA_b"]);
    s.handle_response(t0, msg(update(vec![person("PA_z", "p_z", State::Active)])));
    assert_eq!(s.remotes().len(), 2);
    // A second, full join throws the old roster away.
    s.handle_response(
        t0,
        msg(join(vec![person("PA_y", "p_y", State::Joining)], 0, None)),
    );
    assert_eq!(s.remotes().keys().collect::<Vec<_>>(), ["PA_y"]);
}

#[test]
fn updates_replace_whole_records_and_ignore_unknown_departures() {
    let t0 = Instant::now();
    let mut s = joined_session(t0);
    let b = with_track(person("PA_b", "p_b", State::Active), "TR_b");
    s.handle_response(t0, msg(update(vec![b])));
    assert!(s.publisher_of("TR_b").is_some());
    // Replaced, not merged: the new record has no tracks.
    s.handle_response(
        t0,
        msg(update(vec![
            person("PA_b", "p_b2", State::Active),
            person("PA_never", "x", State::Disconnected),
        ])),
    );
    assert!(s.publisher_of("TR_b").is_none());
    assert_eq!(s.remotes()["PA_b"].identity, "p_b2");
    assert_eq!(s.remotes().len(), 1);
    // An empty update still says the roster may have changed.
    let _ = events(&mut s);
    s.handle_response(t0, msg(update(vec![])));
    assert_eq!(events(&mut s), vec![SessionEvent::Participants]);
}

#[test]
fn own_record_updates_local_and_never_enters_the_roster() {
    let t0 = Instant::now();
    let mut s = joined_session(t0);
    let mut me = with_track(person("PA_me", "p_me", State::Disconnected), "TR_mine");
    me.metadata = "new".to_owned();
    s.handle_response(t0, msg(update(vec![me])));
    let local = s.local().expect("local kept");
    assert_eq!(local.metadata, "new");
    assert_eq!(local.state, State::Disconnected as i32);
    assert!(!s.remotes().contains_key("PA_me"));
    assert!(s.publisher_of("TR_mine").is_none(), "local is not searched");
}

#[test]
fn join_without_participant() {
    let t0 = Instant::now();
    let mut s = Session::new(t0);
    s.handle_response(
        t0,
        msg(Msg::Join(JoinResponse {
            other_participants: vec![person("PA_b", "p_b", State::Active)],
            ..JoinResponse::default()
        })),
    );
    assert_eq!(
        s.poll_event(),
        Some(SessionEvent::Joined {
            identity: String::new(),
            sid: String::new(),
            ice_servers: vec![],
            force_relay: false,
        })
    );
    assert!(s.local().is_none());
    // With no local record, an update with an empty sid goes to the roster.
    s.handle_response(t0, msg(update(vec![person("", "anon", State::Active)])));
    assert!(s.remotes().contains_key(""));
    assert!(s.local().is_none());
}

#[test]
fn publish_is_encrypted_audio_without_red() {
    let mut s = Session::new(Instant::now());
    s.publish_audio("mic-1", "microphone", true);
    match requests(&mut s).as_slice() {
        [Req::AddTrack(a)] => {
            assert_eq!(
                *a,
                AddTrackRequest {
                    cid: "mic-1".to_owned(),
                    name: "microphone".to_owned(),
                    r#type: 0,
                    source: 2,
                    muted: true,
                    disable_red: true,
                    encryption: ENCRYPTION_GCM,
                    ..AddTrackRequest::default()
                }
            );
            assert_eq!(ENCRYPTION_GCM, 1);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn client_requests() {
    let mut s = Session::new(Instant::now());
    s.offer("v=0 o".to_owned(), 3);
    s.answer("v=0 a".to_owned(), 4);
    s.mute("TR_m", true);
    s.trickle("candidate:x");
    let sent = requests(&mut s);
    assert_eq!(sent.len(), 4);
    assert_eq!(
        sent[0],
        Req::Offer(SessionDescription {
            r#type: "offer".to_owned(),
            sdp: "v=0 o".to_owned(),
            id: 3,
            ..SessionDescription::default()
        })
    );
    assert_eq!(
        sent[1],
        Req::Answer(SessionDescription {
            r#type: "answer".to_owned(),
            sdp: "v=0 a".to_owned(),
            id: 4,
            ..SessionDescription::default()
        })
    );
    assert_eq!(
        sent[2],
        Req::Mute(MuteTrackRequest {
            sid: "TR_m".to_owned(),
            muted: true
        })
    );
    assert_eq!(
        Some(sent[3].clone()),
        trickle_for("candidate:x", None, 0, false).message
    );
}

#[test]
fn leave_resume_and_published() {
    let t0 = Instant::now();
    let mut s = joined_session(t0);
    s.handle_response(
        t0,
        msg(Msg::Leave(LeaveRequest {
            reason: 3,
            action: 2,
            ..LeaveRequest::default()
        })),
    );
    assert_eq!(
        s.poll_event(),
        Some(SessionEvent::Leave {
            reason: 3,
            action: LeaveAction::Reconnect
        })
    );
    assert!(
        s.poll_timeout().is_some(),
        "a server leave does not stop pings"
    );

    s.signal_lost();
    let ice = vec![IceServer {
        urls: vec!["stun:stun.example".to_owned()],
        ..IceServer::default()
    }];
    let later = t0 + Duration::from_secs(30);
    s.handle_response(
        later,
        msg(Msg::Reconnect(ReconnectResponse {
            ice_servers: ice.clone(),
            ..ReconnectResponse::default()
        })),
    );
    assert_eq!(
        s.poll_event(),
        Some(SessionEvent::Resumed { ice_servers: ice })
    );
    assert_eq!(s.poll_timeout(), Some(later + Duration::from_secs(5)));
    assert_eq!(s.remotes().len(), 1);
    assert!(s.local().is_some());

    let track = TrackInfo {
        sid: "TR_mic".to_owned(),
        encryption: ENCRYPTION_GCM,
        ..TrackInfo::default()
    };
    s.handle_response(
        later,
        msg(Msg::TrackPublished(TrackPublishedResponse {
            cid: "mic-1".to_owned(),
            track: Some(track.clone()),
        })),
    );
    assert_eq!(
        s.poll_event(),
        Some(SessionEvent::TrackPublished {
            cid: "mic-1".to_owned(),
            track
        })
    );
    s.handle_response(
        later,
        msg(Msg::TrackPublished(TrackPublishedResponse {
            cid: "mic-2".to_owned(),
            track: None,
        })),
    );
    assert_eq!(s.poll_event(), None, "no track, no event");

    s.handle_response(later, msg(Msg::RefreshToken("rt".to_owned())));
    s.reset(later);
    assert_eq!(s.refresh_token(), Some("rt"));
    assert!(!s.joined());
}

#[test]
fn leave_action_mapping() {
    let t0 = Instant::now();
    let mut s = Session::new(t0);
    for (wire, want) in [
        (0, LeaveAction::Disconnect),
        (1, LeaveAction::Resume),
        (2, LeaveAction::Reconnect),
        (7, LeaveAction::Disconnect),
        (-1, LeaveAction::Disconnect),
    ] {
        s.handle_response(
            t0,
            msg(Msg::Leave(LeaveRequest {
                reason: 9,
                action: wire,
                ..LeaveRequest::default()
            })),
        );
        assert_eq!(
            s.poll_event(),
            Some(SessionEvent::Leave {
                reason: 9,
                action: want
            }),
            "action {wire}"
        );
    }
}

#[test]
fn client_leave_keeps_queue_order_and_stops_pings() {
    let t0 = Instant::now();
    let mut s = joined_session(t0);
    s.mute("TR_x", false);
    s.leave();
    assert_eq!(s.poll_timeout(), None);
    let sent = requests(&mut s);
    assert!(matches!(sent[0], Req::Mute(_)), "{sent:?}");
    assert_eq!(
        sent[1],
        Req::Leave(LeaveRequest {
            reason: DisconnectReason::ClientInitiated as i32,
            action: 0,
            ..LeaveRequest::default()
        })
    );
    assert_eq!(sent.len(), 2);
    s.handle_timeout(t0 + Duration::from_secs(60));
    assert!(s.poll_request().is_none());
    assert!(s.joined());
    assert_eq!(s.remotes().len(), 1);
}

#[test]
fn signal_lost_drops_requests_but_keeps_the_rest() {
    let t0 = Instant::now();
    let mut s = joined_session(t0);
    s.handle_response(t0, msg(Msg::RefreshToken("rt1".to_owned())));
    s.handle_response(t0, msg(update(vec![])));
    s.mute("TR_x", true);
    s.offer("sdp".to_owned(), 1);
    s.signal_lost();
    assert!(s.poll_request().is_none());
    assert_eq!(s.poll_timeout(), None);
    assert_eq!(s.poll_event(), Some(SessionEvent::Participants));
    assert!(s.joined());
    assert!(s.local().is_some());
    assert_eq!(s.remotes().len(), 1);
    assert_eq!(s.refresh_token(), Some("rt1"));
    s.handle_timeout(t0 + Duration::from_secs(60));
    assert!(
        s.poll_request().is_none(),
        "no pings while the signal is down"
    );
}

#[test]
fn fresh_session_and_reset() {
    let t0 = Instant::now();
    let s = Session::new(t0);
    assert!(!s.joined());
    assert!(s.local().is_none());
    assert!(s.remotes().is_empty());
    assert_eq!(s.refresh_token(), None);
    assert_eq!(s.poll_timeout(), None);
    assert_eq!(s.rtt_ms(), None);
    assert_eq!(*s.counters(), SessionCounters::default());

    let mut s = Session::new(t0);
    s.handle_response(
        t0,
        msg(join(vec![person("PA_b", "b", State::Active)], 30, None)),
    );
    s.handle_response(t0 + ms(10), msg(Msg::Pong(0)));
    s.handle_response(t0, msg(Msg::RefreshToken("old".to_owned())));
    s.handle_response(t0, msg(Msg::RefreshToken("new".to_owned())));
    s.mute("TR", true);
    let t1 = t0 + Duration::from_secs(100);
    s.reset(t1);
    assert!(!s.joined());
    assert!(s.local().is_none());
    assert!(s.remotes().is_empty());
    assert_eq!(s.poll_timeout(), None);
    assert_eq!(s.rtt_ms(), None);
    assert!(s.poll_request().is_none());
    assert!(s.poll_event().is_none());
    assert_eq!(*s.counters(), SessionCounters::default());
    assert_eq!(s.refresh_token(), Some("new"));

    // New epoch, and the ping period is back to 5 s.
    s.handle_response(t1, msg(join(vec![], 0, None)));
    assert_eq!(s.poll_timeout(), Some(t1 + Duration::from_secs(5)));
    s.handle_timeout(t1 + Duration::from_secs(5));
    assert_eq!(requests(&mut s)[0], Req::Ping(5000));

    // `new` does not carry the token over.
    assert_eq!(Session::new(t1).refresh_token(), None);
}

#[test]
fn ping_period_rules() {
    let t0 = Instant::now();
    let mut s = Session::new(t0);
    assert_eq!(s.poll_timeout(), None, "no pings before a join");
    s.handle_timeout(t0 + Duration::from_secs(10));
    assert!(s.poll_request().is_none());

    s.handle_response(t0, msg(join(vec![], -3, None)));
    assert_eq!(s.poll_timeout(), Some(t0 + Duration::from_secs(5)));
    s.handle_response(t0, msg(join(vec![], 7, None)));
    assert_eq!(s.poll_timeout(), Some(t0 + Duration::from_secs(7)));
    s.handle_response(t0, msg(join(vec![], 0, None)));
    assert_eq!(
        s.poll_timeout(),
        Some(t0 + Duration::from_secs(7)),
        "0 keeps the previous period"
    );

    // Not due yet: nothing.
    s.handle_timeout(t0 + ms(6999));
    assert!(s.poll_request().is_none());
    // Late by 20 s: one ping pair, rescheduled from the call.
    let late = t0 + Duration::from_secs(27);
    s.handle_timeout(late);
    assert_eq!(requests(&mut s).len(), 2);
    assert_eq!(s.poll_timeout(), Some(late + Duration::from_secs(7)));
    s.handle_timeout(late);
    assert!(s.poll_request().is_none());
}

#[test]
fn both_pong_kinds_measure_rtt() {
    let t0 = Instant::now();
    let mut s = Session::new(t0);
    s.handle_response(t0 + ms(300), msg(Msg::Pong(250)));
    assert_eq!(s.rtt_ms(), Some(50), "a pong is taken even before any ping");
    s.handle_response(t0 + ms(300), msg(Msg::Pong(301)));
    assert_eq!(s.rtt_ms(), Some(50), "future stamp ignored");
    s.handle_response(
        t0 + ms(400),
        msg(Msg::PongResp(Pong {
            last_ping_timestamp: 0,
            timestamp: 390,
        })),
    );
    assert_eq!(s.rtt_ms(), Some(10));
    assert!(s.poll_event().is_none());
    // Before the epoch the clock reads 0.
    let mut early = Session::new(t0 + Duration::from_secs(1));
    early.handle_response(t0, msg(Msg::Pong(0)));
    assert_eq!(early.rtt_ms(), Some(0));
}

#[test]
fn passthrough_events() {
    let t0 = Instant::now();
    let mut s = Session::new(t0);
    let mut map = std::collections::HashMap::new();
    map.insert("1".to_owned(), "TR_a".to_owned());
    s.handle_response(
        t0,
        msg(Msg::Answer(SessionDescription {
            r#type: "pranswer".to_owned(),
            sdp: "ans".to_owned(),
            id: 5,
            mid_to_track_id: map,
        })),
    );
    s.handle_response(
        t0,
        msg(Msg::Offer(SessionDescription {
            r#type: "offer".to_owned(),
            sdp: "off".to_owned(),
            id: 6,
            ..SessionDescription::default()
        })),
    );
    s.handle_response(
        t0,
        msg(Msg::MediaSectionsRequirement(MediaSectionsRequirement {
            num_audios: 0,
            num_videos: 0,
        })),
    );
    s.handle_response(
        t0,
        msg(Msg::MediaSectionsRequirement(MediaSectionsRequirement {
            num_audios: 2,
            num_videos: 1,
        })),
    );
    let speakers = vec![SpeakerInfo {
        sid: "PA_b".to_owned(),
        level: 0.5,
        active: true,
    }];
    s.handle_response(
        t0,
        msg(Msg::SpeakersChanged(SpeakersChanged {
            speakers: speakers.clone(),
        })),
    );
    s.handle_response(
        t0,
        msg(Msg::Mute(MuteTrackRequest {
            sid: "TR_other".to_owned(),
            muted: true,
        })),
    );
    assert_eq!(
        events(&mut s),
        vec![
            SessionEvent::Answer {
                sdp: "ans".to_owned(),
                id: 5,
                mid_to_track_id: [("1".to_owned(), "TR_a".to_owned())].into_iter().collect(),
            },
            SessionEvent::Offer {
                sdp: "off".to_owned(),
                id: 6
            },
            SessionEvent::MediaSections {
                audios: 0,
                videos: 0
            },
            SessionEvent::MediaSections {
                audios: 2,
                videos: 1
            },
            SessionEvent::Speakers(speakers),
            SessionEvent::Muted {
                sid: "TR_other".to_owned(),
                muted: true
            },
        ]
    );
    assert!(
        s.poll_request().is_none(),
        "the session never answers by itself"
    );
}

#[test]
fn remote_trickles() {
    let t0 = Instant::now();
    let mut s = Session::new(t0);
    let trickle = |json: &str| {
        msg(Msg::Trickle(TrickleRequest {
            candidate_init: json.to_owned(),
            target: 1,
            r#final: true,
        }))
    };
    s.handle_response(
        t0,
        trickle(r#"{"candidate":"candidate:1 1 udp 1 192.0.2.1 9 typ host","sdpMid":"0"}"#),
    );
    s.handle_response(t0, trickle(r#"{"candidate":""}"#));
    s.handle_response(t0, trickle(r#"{"sdpMid":"0"}"#));
    s.handle_response(t0, trickle("garbage"));
    assert_eq!(
        events(&mut s),
        vec![
            SessionEvent::RemoteCandidate {
                candidate: "candidate:1 1 udp 1 192.0.2.1 9 typ host".to_owned()
            },
            SessionEvent::RemoteCandidate {
                candidate: String::new()
            },
        ]
    );
    assert_eq!(s.counters().bad_trickles, 2);
    assert_eq!(s.counters().unknown, 0);
    assert_eq!(s.counters().responses["trickle"], 4);
}

#[test]
fn request_responses() {
    let t0 = Instant::now();
    let mut s = Session::new(t0);
    s.handle_response(
        t0,
        msg(Msg::RequestResponse(RequestResponse {
            request_id: 1,
            reason: request_response::Reason::Ok as i32,
            message: "fine".to_owned(),
            ..RequestResponse::default()
        })),
    );
    assert_eq!(s.poll_event(), None);
    s.handle_response(
        t0,
        msg(Msg::RequestResponse(RequestResponse {
            request_id: 2,
            reason: request_response::Reason::NotAllowed as i32,
            message: "no".to_owned(),
            ..RequestResponse::default()
        })),
    );
    assert_eq!(
        s.poll_event(),
        Some(SessionEvent::Refused {
            request_id: 2,
            reason: 2,
            message: "no".to_owned()
        })
    );
}

#[test]
fn counters_ignored_and_unknown() {
    let t0 = Instant::now();
    let mut s = Session::new(t0);
    s.handle_response(t0, SignalResponse::default());
    s.handle_response(t0, msg(Msg::RoomUpdate(RoomUpdate::default())));
    s.handle_response(t0, msg(Msg::RoomUpdate(RoomUpdate::default())));
    s.handle_response(
        t0,
        msg(Msg::StoreDataBlobResponse(StoreDataBlobResponse::default())),
    );
    s.handle_response(t0, msg(Msg::RefreshToken("t".to_owned())));
    assert!(s.poll_event().is_none());
    assert!(s.poll_request().is_none());
    let c = s.counters();
    assert_eq!(c.unknown, 2, "empty and data blob responses are unknown");
    assert_eq!(c.bad_trickles, 0);
    let want: BTreeMap<String, u64> = [
        ("empty", 1),
        ("room_update", 2),
        ("store_data_blob_response", 1),
        ("refresh_token", 1),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    assert_eq!(c.responses, want);
    assert!(!s.joined());
}

#[test]
fn reconnect_before_join_is_accepted() {
    let t0 = Instant::now();
    let mut s = Session::new(t0);
    s.handle_response(t0, msg(Msg::Reconnect(ReconnectResponse::default())));
    assert_eq!(
        s.poll_event(),
        Some(SessionEvent::Resumed {
            ice_servers: vec![]
        })
    );
    assert_eq!(s.poll_timeout(), Some(t0 + Duration::from_secs(5)));
    assert!(!s.joined());
}
