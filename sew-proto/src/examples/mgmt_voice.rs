//! Examples for the wire types of voice over mgmt.

use super::{CONNECT_USER, ID_BOB, ID_CHANNEL, ID_ME, ext, t};
use crate::external_id::ExternalId;
use crate::mgmt::*;
use crate::registry::Example;
use crate::voice::{VoiceConnection, VoiceLeaveReason};

const CURSOR: &str = "1790510400123.42";
const NEXT: &str = "1790510400123.44";

fn state_full() -> MgmtVoiceState {
    MgmtVoiceState {
        channel_id: ID_CHANNEL,
        member_id: ID_BOB,
        external_id: ext(CONNECT_USER),
        connection: VoiceConnection::Connected,
        self_mute: true,
        self_deaf: false,
        server_mute: false,
        server_deaf: false,
        joined_at: t(60_000),
    }
}

impl Example for MgmtVoiceState {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![("full", state_full())]
    }
}

impl Example for MgmtVoiceStates {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MgmtVoiceStates {
                    states: vec![
                        MgmtVoiceState {
                            member_id: ID_ME,
                            external_id: ExternalId::K2Owner,
                            connection: VoiceConnection::Connecting,
                            self_mute: false,
                            ..state_full()
                        },
                        state_full(),
                    ],
                    cursor: CURSOR.into(),
                },
            ),
            (
                "min",
                MgmtVoiceStates {
                    states: vec![],
                    cursor: CURSOR.into(),
                },
            ),
        ]
    }
}

impl Example for MgmtVoiceChangesQuery {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MgmtVoiceChangesQuery {
                    after: CURSOR.into(),
                    wait: Some(30),
                    limit: Some(500),
                },
            ),
            (
                "min",
                MgmtVoiceChangesQuery {
                    after: CURSOR.into(),
                    wait: None,
                    limit: None,
                },
            ),
        ]
    }
}

impl Example for MgmtVoiceChange {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MgmtVoiceChange {
                    seq: 43,
                    kind: MgmtVoiceChangeKind::Joined,
                    channel_id: ID_CHANNEL,
                    member_id: Some(ID_BOB),
                    external_id: Some(ext(CONNECT_USER)),
                    state: Some(state_full()),
                    reason: None,
                },
            ),
            (
                "full_left",
                MgmtVoiceChange {
                    seq: 44,
                    kind: MgmtVoiceChangeKind::Left,
                    channel_id: ID_CHANNEL,
                    member_id: Some(ID_BOB),
                    external_id: Some(ext(CONNECT_USER)),
                    state: None,
                    reason: Some(VoiceLeaveReason::MediaPathDown),
                },
            ),
            (
                "min",
                MgmtVoiceChange {
                    seq: 45,
                    kind: MgmtVoiceChangeKind::ChannelClosing,
                    channel_id: ID_CHANNEL,
                    member_id: None,
                    external_id: None,
                    state: None,
                    reason: None,
                },
            ),
        ]
    }
}

impl Example for MgmtVoiceChanges {
    fn examples() -> Vec<(&'static str, Self)> {
        let all = MgmtVoiceChange::examples()
            .into_iter()
            .map(|(_, c)| c)
            .collect();
        vec![
            (
                "full",
                MgmtVoiceChanges {
                    changes: all,
                    next: NEXT.into(),
                },
            ),
            (
                "min",
                MgmtVoiceChanges {
                    changes: vec![],
                    next: CURSOR.into(),
                },
            ),
        ]
    }
}

impl Example for MgmtVoiceCursorExpiredDetails {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            MgmtVoiceCursorExpiredDetails {
                head: "1790510999000.0".into(),
            },
        )]
    }
}

impl Example for MgmtChannelClosingRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            ("full", MgmtChannelClosingRequest { closing: true }),
            ("reopen", MgmtChannelClosingRequest { closing: false }),
        ]
    }
}

impl Example for MgmtChannelClosingResponse {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MgmtChannelClosingResponse {
                    closing: true,
                    archived: true,
                },
            ),
            (
                "live",
                MgmtChannelClosingResponse {
                    closing: true,
                    archived: false,
                },
            ),
        ]
    }
}

impl Example for MgmtMediaPathRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MgmtMediaPathRequest {
                    state: MediaPathState::Down,
                    reason: Some("voice_lane_down".into()),
                },
            ),
            (
                "min",
                MgmtMediaPathRequest {
                    state: MediaPathState::Up,
                    reason: None,
                },
            ),
        ]
    }
}

impl Example for MgmtMediaPathResponse {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            MgmtMediaPathResponse {
                state: MediaPathState::Down,
                since: t(120_000),
                ended_calls: 2,
            },
        )]
    }
}

/// `status.voice.media_path` while down.
pub(super) fn media_path_status_down() -> VoiceMediaPathStatus {
    VoiceMediaPathStatus {
        state: MediaPathState::Down,
        since: t(120_000),
        media_path_reason: Some("voice_lane_down".into()),
    }
}

impl Example for VoiceMediaPathStatus {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            ("full", media_path_status_down()),
            (
                "min",
                VoiceMediaPathStatus {
                    state: MediaPathState::Up,
                    since: t(0),
                    media_path_reason: None,
                },
            ),
        ]
    }
}
