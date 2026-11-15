//! Whether a person's Record and Stop would be taken now: the one rule table both the server
//! and a client fallback use ([`decide`]).
//!
//! It mirrors Sew's checks on the start and stop routes, in their order:
//!
//! - **Both.** The person is in the call (connected or reconnecting in the recording's room),
//!   is not an agent, and the session's pass is not refused `voice:record`.
//! - **Record.** No recording is active, the org policy `recording.enabled` is on, a DM also
//!   needs `recording.dm_enabled`, and the person holds RECORD_CALLS on the room's channel.
//! - **Stop.** A recording is active; in a DM anyone in the call may stop it; elsewhere its
//!   starter, a holder of RECORD_CALLS, or a holder of MANAGE_CHANNELS. A recording the room
//!   requires (trigger `required`) is not stopped while two or more persons are in the room.
//!
//! Passing conditions (install capacity, the recorder's or transcriber's health, a key group
//! rebuilding) are not part of the table: they stay the routes' refusals. The server's own
//! stable checks (its recorder switch, a channel that is not a voice channel, a Space whose App
//! forbids uploads) can only clear `can_record` after the table.

use super::RecordingControls;

/// The active recording of the room, as [`decide`] needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActiveInput {
    /// The person started it.
    pub mine: bool,
    /// The room required it (trigger `required`).
    pub required_trigger: bool,
    /// Persons in the room (standard voice entries, guests included; never the recorder or
    /// agents).
    pub persons: u32,
}

impl ActiveInput {
    /// An active recording.
    pub const fn new(mine: bool, required_trigger: bool, persons: u32) -> ActiveInput {
        ActiveInput {
            mine,
            required_trigger,
            persons,
        }
    }
}

/// The facts [`decide`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ControlsInput {
    /// The person has a connected or reconnecting voice entry in the room.
    pub in_call: bool,
    /// The person is an agent (or the request acts as one).
    pub agent: bool,
    /// The session's pass may not record (`voice:record` refused where its App requires it).
    pub record_capped: bool,
    /// Org policy `recording.enabled`.
    pub recording_enabled: bool,
    /// The room is a DM or group DM call.
    pub dm: bool,
    /// Org policy `recording.dm_enabled`.
    pub recording_dm_enabled: bool,
    /// RECORD_CALLS held on the room's channel.
    pub record_calls: bool,
    /// MANAGE_CHANNELS held on the room's channel (unmasked).
    pub manage_channels: bool,
    /// The room's active recording, if any.
    pub active: Option<ActiveInput>,
}

impl ControlsInput {
    /// Every fact false and no active recording.
    pub const fn new() -> ControlsInput {
        ControlsInput {
            in_call: false,
            agent: false,
            record_capped: false,
            recording_enabled: false,
            dm: false,
            recording_dm_enabled: false,
            record_calls: false,
            manage_channels: false,
            active: None,
        }
    }
}

/// The person's Record and Stop for `input` (`rev` 0, no hint).
pub fn decide(input: &ControlsInput) -> RecordingControls {
    if !input.in_call || input.agent || input.record_capped {
        return RecordingControls::new(false, false);
    }
    let can_record = input.active.is_none()
        && input.recording_enabled
        && (!input.dm || input.recording_dm_enabled)
        && input.record_calls;
    let can_stop = input.active.is_some_and(|a| {
        let allowed = input.dm || a.mine || input.record_calls || input.manage_channels;
        let held_by_rule = a.required_trigger && a.persons >= 2;
        allowed && !held_by_rule
    });
    RecordingControls::new(can_record, can_stop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ControlsInput {
        let mut i = ControlsInput::new();
        i.in_call = true;
        i.recording_enabled = true;
        i
    }

    fn pair(c: RecordingControls) -> (bool, bool) {
        (c.can_record, c.can_stop)
    }

    #[test]
    fn manage_channels_stops_but_does_not_start() {
        let mut i = base();
        i.manage_channels = true;
        assert_eq!(pair(decide(&i)), (false, false));
        i.active = Some(ActiveInput::new(false, false, 3));
        assert_eq!(pair(decide(&i)), (false, true));
    }

    #[test]
    fn a_required_recording_stops_only_alone() {
        let mut i = base();
        i.active = Some(ActiveInput::new(true, true, 1));
        assert_eq!(pair(decide(&i)), (false, true));
        i.active = Some(ActiveInput::new(true, true, 2));
        assert_eq!(pair(decide(&i)), (false, false));
    }

    #[test]
    fn not_in_call_agent_or_capped_do_neither() {
        let blocks: [fn(&mut ControlsInput); 3] = [
            |i| i.in_call = false,
            |i| i.agent = true,
            |i| i.record_capped = true,
        ];
        for f in blocks {
            let mut i = base();
            i.record_calls = true;
            i.dm = true;
            i.recording_dm_enabled = true;
            f(&mut i);
            assert_eq!(pair(decide(&i)), (false, false));
            i.active = Some(ActiveInput::new(true, false, 2));
            assert_eq!(pair(decide(&i)), (false, false));
        }
    }
}
