//! Examples for voice room notifications: the room start event, invite requests and results,
//! the invite itself and the caller's pending invites.

use super::{ID_BOB, ID_CAROL, ID_CHANNEL, ID_JOB, ID_ME, ID_REPLY, ID_SPACE, t};
use crate::registry::Example;
use crate::voice_notify::*;

/// Bob invites me into the voice channel; the invite expires five minutes later.
pub(super) fn invite_full() -> VoiceInvite {
    invite_in(ID_CHANNEL)
}

/// Bob's invite into `channel`.
pub(super) fn invite_in(channel: crate::id::Id) -> VoiceInvite {
    VoiceInvite::new(
        ID_JOB,
        channel,
        ID_SPACE,
        ID_BOB,
        ID_ME,
        t(360_000),
        t(60_000),
    )
}

impl Example for VoiceStartedEvent {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                VoiceStartedEvent::new(ID_CHANNEL, ID_SPACE, ID_BOB, true, t(5_000)),
            ),
            (
                "cleared",
                VoiceStartedEvent::new(ID_CHANNEL, ID_SPACE, ID_BOB, false, t(100_000)),
            ),
        ]
    }
}

impl Example for VoiceInviteRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            ("full", VoiceInviteRequest::new(vec![ID_ME, ID_CAROL])),
            ("one", VoiceInviteRequest::new(vec![ID_CAROL])),
        ]
    }
}

impl Example for VoiceInviteResults {
    fn examples() -> Vec<(&'static str, Self)> {
        let mut silent = VoiceInviteResult::pending(ID_CAROL, ID_REPLY, t(360_000));
        silent.silent = true;
        silent.reason = Some(invite_reason::BUSY.into());
        vec![(
            "full",
            VoiceInviteResults::new(vec![
                VoiceInviteResult::pending(ID_ME, ID_JOB, t(360_000)),
                silent,
                VoiceInviteResult::refused(ID_BOB, invite_reason::SELF),
            ]),
        )]
    }
}

impl Example for VoiceInviteDeclineRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![("full", VoiceInviteDeclineRequest::new(ID_JOB))]
    }
}

impl Example for VoiceInviteCancelRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![("full", VoiceInviteCancelRequest::new(ID_JOB))]
    }
}

impl Example for VoiceInvite {
    fn examples() -> Vec<(&'static str, Self)> {
        // The invitee's copy of a silent invite into a recorded room.
        let mut full = invite_full();
        full.silent = true;
        full.recorded = true;
        full.reason = Some(invite_reason::DND.into());
        let mut declined = invite_full();
        declined.state = VoiceInviteState::Declined;
        declined.reason = Some(VoiceInviteState::Declined.as_str().into());
        vec![
            ("full", full),
            ("pending", invite_full()),
            ("declined", declined),
        ]
    }
}

impl Example for MyVoiceInvites {
    fn examples() -> Vec<(&'static str, Self)> {
        let mut outgoing = VoiceInvite::new(
            ID_REPLY,
            ID_CHANNEL,
            ID_SPACE,
            ID_ME,
            ID_CAROL,
            t(420_000),
            t(120_000),
        );
        outgoing.reason = None;
        vec![
            (
                "full",
                MyVoiceInvites::new(vec![invite_full()], vec![outgoing]),
            ),
            ("min", MyVoiceInvites::default()),
        ]
    }
}
