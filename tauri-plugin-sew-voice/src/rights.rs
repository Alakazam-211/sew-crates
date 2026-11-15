//! The recording controls ([`RecordingControls`]): whether Sew would take this person's Record
//! and Stop now.
//!
//! - **From Sew** ([`from_sew`]): a server that computes them sends them to the call; the
//!   plugin passes them on, gated by its own call state (both `false` unless the call is
//!   connected or reconnecting, so the gap before the server's first value and after the call
//!   ends shows nothing).
//! - **From the host's rights** ([`controls`]): against a server that does not send them, the
//!   plugin works them out from the host's [`RecordingRights`] and what it knows of the call,
//!   with the one rule table Sew's own checks use (`sew_proto::recording::controls::decide`):
//!
//!   - **Both.** The person is in the call (connected or reconnecting: Sew acts only for a
//!     voice entry in the room, and a join held for a notice has none), is not an agent, and
//!     the session's pass is not refused `voice:record` (`cap_ceiling`).
//!   - **Record.** No recording is active (Sew answers `recording_active`), the org policy
//!     `recording.enabled` is on, a DM also needs `recording.dm_enabled`, and the person holds
//!     RECORD_CALLS on the room's channel (Sew's DM permissions hold it only for a person,
//!     never a guest, while `recording.dm_enabled` is on).
//!   - **Stop.** A recording is active; in a DM anyone in the call may stop it; elsewhere its
//!     starter, a person in the call with RECORD_CALLS, or one with MANAGE_CHANNELS. A
//!     recording Sew started because the room requires one (`trigger: required`) is not
//!     stopped while two or more people are in the room (`recording_required`).
//!
//!   What the host's rights cannot carry (the server's recorder switched off, a Space whose
//!   App forbids uploads, capacity) stays Sew's to refuse; the command's error then says why.

use sew_proto::perms::Perms;
use sew_proto::recording::controls::{ActiveInput, ControlsInput, decide};

use crate::engine::ServerControls;
use crate::wire::{
    CallState, ControlsSource, Participant, RecordingControls, RecordingInfo, RecordingRights,
};

/// What the plugin knows of the call.
pub(crate) struct CallView<'a> {
    pub(crate) state: &'a CallState,
    /// The call's room (`join`'s, or a move's destination).
    pub(crate) room: Option<&'a str>,
    /// The members in the call, this device's own included.
    pub(crate) participants: &'a [Participant],
}

/// The person has a voice entry in the room: connected, or reconnecting (Sew keeps the entry).
fn in_call(state: &CallState) -> bool {
    matches!(state, CallState::Connected | CallState::Reconnecting { .. })
}

/// The controls for `call` with the host's `rights` and the active recording `rec`.
pub(crate) fn controls(
    rights: Option<&RecordingRights>,
    call: &CallView<'_>,
    rec: Option<&RecordingInfo>,
) -> RecordingControls {
    let Some(r) = rights.filter(|r| call.room == Some(r.room.as_str())) else {
        return RecordingControls::default();
    };
    let perms = Perms(r.perms().unwrap_or(0));
    let me = call
        .participants
        .iter()
        .find(|p| p.is_self)
        .map(|p| p.member_id.as_str());
    let persons = u32::try_from(call.participants.len()).unwrap_or(u32::MAX);
    let mut input = ControlsInput::new();
    input.in_call = in_call(call.state);
    input.agent = r.agent;
    input.record_capped = r.record_capped;
    input.recording_enabled = r.recording_enabled;
    input.dm = r.dm;
    input.recording_dm_enabled = r.recording_dm_enabled;
    input.record_calls = perms.contains(Perms::RECORD_CALLS);
    input.manage_channels = perms.contains(Perms::MANAGE_CHANNELS);
    input.active = rec.map(|rec| {
        ActiveInput::new(
            me.is_some() && rec.started_by.as_deref() == me,
            rec.trigger == crate::wire::trigger::REQUIRED,
            persons,
        )
    });
    let d = decide(&input);
    RecordingControls {
        can_record: d.can_record,
        can_stop: d.can_stop,
        ..RecordingControls::default()
    }
}

/// The controls from a server that computes them: its latest `value` for the call's room
/// (`None`: none yet), while the call is connected or reconnecting; both `false` otherwise.
pub(crate) fn from_sew(value: Option<&ServerControls>, state: &CallState) -> RecordingControls {
    let mut c = RecordingControls {
        source: ControlsSource::Sew,
        ..RecordingControls::default()
    };
    if let Some(v) = value.filter(|_| in_call(state)) {
        c.can_record = v.can_record;
        c.can_stop = v.can_stop;
        c.record_unavailable = v.record_unavailable;
    }
    c
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    const ME: &str = "1001";
    const OTHER: &str = "1002";
    const ROOM: &str = "2002";

    fn person(id: &str, me: bool) -> Participant {
        Participant {
            member_id: id.to_owned(),
            is_self: me,
            muted: false,
            deafened: false,
            server_muted: false,
            server_deafened: false,
            speak_denied: false,
        }
    }

    fn room(n: usize) -> Vec<Participant> {
        let mut p = vec![person(ME, true)];
        for i in 1..n {
            p.push(person(&format!("10{i:02}9"), false));
        }
        p
    }

    fn rights(perms: Perms) -> RecordingRights {
        let mut r = RecordingRights::new(ROOM, perms.0.to_string());
        r.recording_enabled = true;
        r
    }

    fn rec(started_by: &str, trigger: &str) -> RecordingInfo {
        serde_json::from_value(serde_json::json!({
            "recording_id": "77", "auto": trigger != "manual", "started_by": started_by,
            "my_consent": "granted", "participants": [], "recorder_present": true,
            "trigger": trigger, "required": trigger == "required",
        }))
        .expect("recording")
    }

    fn at(r: &RecordingRights, people: usize, rec: Option<&RecordingInfo>) -> (bool, bool) {
        let p = room(people);
        let c = controls(
            Some(r),
            &CallView {
                state: &CallState::Connected,
                room: Some(ROOM),
                participants: &p,
            },
            rec,
        );
        (c.can_record, c.can_stop)
    }

    const MEMBER: Perms = Perms(Perms::CONNECT.0 | Perms::SPEAK.0);
    const RECORDER: Perms = Perms(MEMBER.0 | Perms::RECORD_CALLS.0);
    const MANAGER: Perms = Perms(MEMBER.0 | Perms::MANAGE_CHANNELS.0);

    #[test]
    fn record_needs_record_calls_and_the_policy() {
        assert_eq!(at(&rights(MEMBER), 2, None), (false, false));
        assert_eq!(at(&rights(RECORDER), 2, None), (true, false));
        // MANAGE_CHANNELS stops, but does not start.
        assert_eq!(at(&rights(MANAGER), 2, None), (false, false));
        let mut off = rights(RECORDER);
        off.recording_enabled = false;
        assert_eq!(at(&off, 2, None), (false, false), "recording.enabled off");
        // Not a decimal number: no bits.
        let mut bad = rights(RECORDER);
        bad.my_perms = "lots".into();
        assert_eq!(at(&bad, 2, None), (false, false));
    }

    #[test]
    fn record_is_refused_while_one_is_active() {
        let r = rights(RECORDER);
        for (by, trigger) in [(OTHER, "manual"), (ME, "manual"), (OTHER, "auto")] {
            assert!(!at(&r, 2, Some(&rec(by, trigger))).0, "{by} {trigger}");
        }
    }

    #[test]
    fn stop_by_starter_record_calls_or_manage_channels() {
        let other = rec(OTHER, "manual");
        let mine = rec(ME, "manual");
        // A member without either bit stops only their own.
        assert_eq!(at(&rights(MEMBER), 3, Some(&other)), (false, false));
        assert_eq!(at(&rights(MEMBER), 3, Some(&mine)), (false, true));
        assert_eq!(at(&rights(RECORDER), 3, Some(&other)), (false, true));
        assert_eq!(at(&rights(MANAGER), 3, Some(&other)), (false, true));
        // An auto-record room's recording, started by whoever joined first.
        assert_eq!(
            at(&rights(MEMBER), 3, Some(&rec(OTHER, "auto"))),
            (false, false)
        );
        assert_eq!(
            at(&rights(MEMBER), 3, Some(&rec(ME, "auto"))),
            (false, true)
        );
    }

    #[test]
    fn a_required_recording_stops_only_with_one_person_in_the_room() {
        for p in [RECORDER, MANAGER, MEMBER] {
            let r = rights(p);
            let mine = rec(ME, "required");
            assert_eq!(at(&r, 1, Some(&mine)), (false, true), "{p:?} alone");
            assert_eq!(at(&r, 2, Some(&mine)), (false, false), "{p:?} two");
            assert_eq!(at(&r, 5, Some(&mine)), (false, false), "{p:?} five");
        }
        // The rule follows what started it: a manual recording in such a room still stops.
        let mut manual = rec(OTHER, "manual");
        manual.required = true;
        assert_eq!(at(&rights(RECORDER), 3, Some(&manual)), (false, true));
    }

    #[test]
    fn dms_follow_the_dm_policy_and_anyone_in_the_call_stops() {
        // Sew's DM permissions: RECORD_CALLS only for a person while recording.dm_enabled is
        // on (never a guest).
        let mut dm = rights(RECORDER);
        dm.dm = true;
        assert_eq!(at(&dm, 2, None), (false, false), "dm_enabled off");
        dm.recording_dm_enabled = true;
        assert_eq!(at(&dm, 2, None), (true, false));
        let mut guest = rights(MEMBER);
        guest.dm = true;
        guest.recording_dm_enabled = true;
        assert_eq!(at(&guest, 2, None), (false, false), "a guest never starts");
        // Stop: anyone in the call, whoever started it and whatever their bits.
        assert_eq!(at(&guest, 2, Some(&rec(OTHER, "manual"))), (false, true));
        assert_eq!(at(&dm, 2, Some(&rec(OTHER, "manual"))), (false, true));
    }

    #[test]
    fn guests_hold_neither_bit_in_a_space_channel() {
        // The guest mask strips RECORD_CALLS and a guest never has MANAGE_CHANNELS.
        let bits = sew_proto::perms::GUEST_MASK.0 & RECORDER.0;
        let g = rights(Perms(bits));
        assert_eq!(at(&g, 2, None), (false, false));
        assert_eq!(at(&g, 2, Some(&rec(OTHER, "manual"))), (false, false));
        // A guest whose join started a required room's recording stops it alone, as Sew lets
        // a starter.
        assert_eq!(at(&g, 1, Some(&rec(ME, "required"))), (false, true));
    }

    #[test]
    fn agents_and_capped_sessions_do_neither() {
        for (agent, capped) in [(true, false), (false, true)] {
            let mut r = rights(Perms(RECORDER.0 | MANAGER.0));
            r.agent = agent;
            r.record_capped = capped;
            assert_eq!(at(&r, 2, None), (false, false));
            assert_eq!(at(&r, 2, Some(&rec(ME, "manual"))), (false, false));
            r.dm = true;
            r.recording_dm_enabled = true;
            assert_eq!(at(&r, 2, Some(&rec(OTHER, "manual"))), (false, false));
        }
    }

    #[test]
    fn only_in_the_call_and_only_for_its_room() {
        let r = rights(RECORDER);
        let p = room(2);
        let mine = rec(ME, "manual");
        fn view<'a>(
            state: &'a CallState,
            room: Option<&'a str>,
            participants: &'a [Participant],
        ) -> CallView<'a> {
            CallView {
                state,
                room,
                participants,
            }
        }
        for state in [
            CallState::Idle,
            CallState::Connecting,
            CallState::ConsentRequired {
                recording_id: "77".into(),
                notice_version: 1,
            },
            CallState::Ended {
                reason: "left".into(),
            },
        ] {
            let c = controls(Some(&r), &view(&state, Some(ROOM), &p), None);
            assert!(c.is_none(), "{state:?}");
            let c = controls(Some(&r), &view(&state, Some(ROOM), &p), Some(&mine));
            assert!(c.is_none(), "{state:?}");
        }
        let back = CallState::Reconnecting { attempt: 1 };
        let c = controls(Some(&r), &view(&back, Some(ROOM), &p), None);
        assert!(c.can_record, "reconnecting keeps the voice entry");
        // Another room's rights (before a move's new ones come), or none at all.
        let c = controls(
            Some(&r),
            &view(&CallState::Connected, Some("2003"), &p),
            None,
        );
        assert!(c.is_none());
        let c = controls(
            None,
            &view(&CallState::Connected, Some(ROOM), &p),
            Some(&mine),
        );
        assert!(c.is_none());
    }
}

/// The adapter against the rule table's checked-in vectors (sew-proto's
/// `vectors/recording-controls-vectors.json`, the full cross product of its facts): the host's
/// rights and the call as the plugin sees them give exactly the vector's answer.
#[cfg(test)]
mod vectors {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::*;

    const ME: &str = "1001";
    const OTHER: &str = "1002";
    const ROOM: &str = "2002";

    fn person(id: String, me: bool) -> Participant {
        Participant {
            member_id: id,
            is_self: me,
            muted: false,
            deafened: false,
            server_muted: false,
            server_deafened: false,
            speak_denied: false,
        }
    }

    fn rec(started_by: &str, trigger: &str) -> RecordingInfo {
        serde_json::from_value(serde_json::json!({
            "recording_id": "77", "auto": trigger != "manual", "started_by": started_by,
            "my_consent": "granted", "participants": [], "recorder_present": true,
            "trigger": trigger, "required": trigger == "required",
        }))
        .expect("recording")
    }

    #[test]
    fn the_adapter_answers_every_vector() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../sew-proto/vectors/recording-controls-vectors.json"
        );
        let text =
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!("the vectors at {path}: {e}"));
        let file: serde_json::Value = serde_json::from_str(&text).expect("the vectors' JSON");
        assert_eq!(
            file["format"],
            serde_json::json!(1),
            "a format this test reads"
        );
        let vectors = file["vectors"].as_array().expect("vectors");
        // 256 fact combinations, 7 recording cases, 3 headcounts.
        assert_eq!(vectors.len(), 256 * 7 * 3);
        for v in vectors {
            let id = v["id"].as_str().expect("id");
            let mut parts = id.split('.');
            let (Some(flags), Some(active), Some(persons), None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                panic!("{id}: not <flags>.<active>.<persons>");
            };
            let bits: Vec<bool> = flags.chars().map(|c| c == '1').collect();
            assert_eq!(bits.len(), 8, "{id}");
            let persons: usize = persons.parse().expect("persons");
            let mut perms = Perms::CONNECT.0 | Perms::SPEAK.0;
            if bits[6] {
                perms |= Perms::RECORD_CALLS.0;
            }
            if bits[7] {
                perms |= Perms::MANAGE_CHANNELS.0;
            }
            let mut r = RecordingRights::new(ROOM, perms.to_string());
            r.agent = bits[1];
            r.record_capped = bits[2];
            r.recording_enabled = bits[3];
            r.dm = bits[4];
            r.recording_dm_enabled = bits[5];
            let state = if bits[0] {
                CallState::Connected
            } else {
                CallState::Connecting
            };
            let mut people = vec![person(ME.to_owned(), true)];
            for i in 1..persons {
                people.push(person(format!("20{i:02}"), false));
            }
            let recording = match active {
                "none" => None,
                a => {
                    let (trigger, who) = a.split_once('_').expect("<trigger>_<mine|other>");
                    let by = match who {
                        "mine" => ME,
                        "other" => OTHER,
                        _ => panic!("{id}: {who}"),
                    };
                    Some(rec(by, trigger))
                }
            };
            let c = controls(
                Some(&r),
                &CallView {
                    state: &state,
                    room: Some(ROOM),
                    participants: &people,
                },
                recording.as_ref(),
            );
            let want = (
                v["can_record"].as_bool().expect("can_record"),
                v["can_stop"].as_bool().expect("can_stop"),
            );
            assert_eq!((c.can_record, c.can_stop), want, "{id}");
            assert_eq!(c.source, ControlsSource::Host, "{id}");
            assert_eq!(c.record_unavailable, None, "{id}");
        }
    }

    #[test]
    fn sews_value_is_gated_by_the_call_state() {
        let mut v = ServerControls::new(true, true);
        v.record_unavailable = Some(sew_call_client::RecordUnavailable::RecordingCapacity);
        let c = from_sew(Some(&v), &CallState::Connected);
        assert_eq!((c.can_record, c.can_stop), (true, true));
        assert_eq!(c.source, ControlsSource::Sew);
        assert_eq!(
            c.record_unavailable,
            Some(sew_call_client::RecordUnavailable::RecordingCapacity)
        );
        let c = from_sew(Some(&v), &CallState::Reconnecting { attempt: 2 });
        assert!(c.can_record && c.can_stop, "reconnecting keeps them");
        for state in [
            CallState::Idle,
            CallState::Connecting,
            CallState::ConsentRequired {
                recording_id: "77".into(),
                notice_version: 1,
            },
            CallState::Ended {
                reason: "left".into(),
            },
        ] {
            let c = from_sew(Some(&v), &state);
            assert!(c.is_none(), "{state:?}");
            assert_eq!(c.record_unavailable, None, "{state:?}");
            assert_eq!(c.source, ControlsSource::Sew, "{state:?}");
        }
        // No value yet: neither, still Sew's.
        let c = from_sew(None, &CallState::Connected);
        assert!(c.is_none() && !c.is_unset());
    }
}
