//! The recording controls ([`RecordingControls`]): whether Sew would take this person's Record
//! and Stop now, from the host's [`RecordingRights`] and what the plugin knows of the call.
//!
//! It mirrors Sew's own checks on the two routes, in their order:
//!
//! - **Both.** The person is in the call (connected or reconnecting: Sew acts only for a voice
//!   entry in the room, and a join held for a notice has none), is not an agent, and the
//!   session's pass is not refused `voice:record` (`cap_ceiling`).
//! - **Record.** No recording is active (Sew answers `recording_active`), the org policy
//!   `recording.enabled` is on, a DM also needs `recording.dm_enabled`, and the person holds
//!   RECORD_CALLS on the room's channel (Sew's DM permissions hold it only for a person, never
//!   a guest, while `recording.dm_enabled` is on).
//! - **Stop.** A recording is active; in a DM anyone in the call may stop it; elsewhere its
//!   starter, a person in the call with RECORD_CALLS, or one with MANAGE_CHANNELS. A
//!   recording Sew started because the room requires one (`trigger: required`) is not stopped
//!   while two or more people are in the room (`recording_required`).
//!
//! What the plugin cannot see (the server's recorder switched off, a Space whose App forbids
//! uploads, capacity) stays Sew's to refuse; the command's error then says why.

use sew_proto::perms::Perms;

use crate::wire::{CallState, Participant, RecordingControls, RecordingInfo, RecordingRights};

/// What the plugin knows of the call.
pub(crate) struct CallView<'a> {
    pub(crate) state: &'a CallState,
    /// The call's room (`join`'s, or a move's destination).
    pub(crate) room: Option<&'a str>,
    /// The members in the call, this device's own included.
    pub(crate) participants: &'a [Participant],
}

/// The controls for `call` with the host's `rights` and the active recording `rec`.
pub(crate) fn controls(
    rights: Option<&RecordingRights>,
    call: &CallView<'_>,
    rec: Option<&RecordingInfo>,
) -> RecordingControls {
    let none = RecordingControls::default();
    let Some(r) = rights.filter(|r| call.room == Some(r.room.as_str())) else {
        return none;
    };
    if !matches!(
        call.state,
        CallState::Connected | CallState::Reconnecting { .. }
    ) || r.agent
        || r.record_capped
    {
        return none;
    }
    let perms = Perms(r.perms().unwrap_or(0));
    let can_record = rec.is_none()
        && r.recording_enabled
        && (!r.dm || r.recording_dm_enabled)
        && perms.contains(Perms::RECORD_CALLS);
    let can_stop = rec.is_some_and(|rec| {
        let me = call
            .participants
            .iter()
            .find(|p| p.is_self)
            .map(|p| p.member_id.as_str());
        let allowed = r.dm
            || (me.is_some() && rec.started_by.as_deref() == me)
            || perms.contains(Perms::RECORD_CALLS)
            || perms.contains(Perms::MANAGE_CHANNELS);
        let held_by_rule =
            rec.trigger == crate::wire::trigger::REQUIRED && call.participants.len() >= 2;
        allowed && !held_by_rule
    });
    RecordingControls {
        can_record,
        can_stop,
    }
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
