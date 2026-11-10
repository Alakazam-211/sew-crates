//! Examples for the wire types of recording in Apps over mgmt.

use super::recording::ID_RECORDING;
use super::{CONNECT_USER, ID_CHANNEL, ID_MESSAGE, ID_REPLY, ID_SPACE, PRINCIPAL, ext, t};
use crate::external_id::ExternalId;
use crate::http::Patch;
use crate::mgmt::*;
use crate::recording::{
    AudioState, Consent, RecordingEventKind, RecordingRule, RecordingStartLevel, RecordingState,
    RecordingTrigger, StopReason, TranscriptState,
};
use crate::registry::Example;

const APP: &str = "jobs";

impl Example for MgmtChannelRecordingRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MgmtChannelRecordingRequest {
                    app_id: Some(APP.into()),
                    auto: Some(false),
                    required: Patch::Set(true),
                    audio_days: Patch::Set(30),
                    require_consent: Patch::Set(false),
                },
            ),
            ("min", MgmtChannelRecordingRequest::default()),
        ]
    }
}

impl Example for MgmtChannelRecording {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MgmtChannelRecording {
                    channel_id: ID_CHANNEL,
                    auto: false,
                    required: Some(true),
                    audio_days: Some(30),
                    require_consent: Some(false),
                    rule: RecordingRule::Required,
                },
            ),
            ("min", MgmtChannelRecording::new(ID_CHANNEL)),
        ]
    }
}

/// The full `recording` object of an App settings request.
pub(super) fn app_recording_request_full() -> MgmtAppRecordingRequest {
    MgmtAppRecordingRequest {
        start_level: Some(RecordingStartLevel::Moderator),
        required: Some(true),
        cap_required: Some(true),
        server_read: Some(true),
        audio_max_days: Patch::Set(30),
    }
}

/// The full `recording` object of an App's settings.
pub(super) fn app_recording_full() -> MgmtAppRecording {
    MgmtAppRecording {
        start_level: RecordingStartLevel::Moderator,
        required: true,
        cap_required: true,
        server_read: true,
        audio_max_days: Some(30),
    }
}

impl Example for MgmtAppRecordingRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            ("full", app_recording_request_full()),
            (
                "clear",
                MgmtAppRecordingRequest {
                    audio_max_days: Patch::Clear,
                    ..MgmtAppRecordingRequest::default()
                },
            ),
            ("min", MgmtAppRecordingRequest::new()),
        ]
    }
}

impl Example for MgmtAppRecording {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            ("full", app_recording_full()),
            ("min", MgmtAppRecording::new()),
        ]
    }
}

impl Example for MgmtRecordingStopRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            ("full", MgmtRecordingStopRequest::new(Some(APP.into()))),
            ("min", MgmtRecordingStopRequest::new(None)),
        ]
    }
}

impl Example for MgmtRecordingQuery {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            ("full", MgmtRecordingQuery::new(Some(APP.into()))),
            ("min", MgmtRecordingQuery::new(None)),
        ]
    }
}

fn participants() -> Vec<MgmtRecordingParticipant> {
    vec![
        MgmtRecordingParticipant::new(ext(PRINCIPAL), Consent::Granted, true),
        MgmtRecordingParticipant::new(ext(CONNECT_USER), Consent::Declined, false),
    ]
}

impl Example for MgmtRecordingParticipant {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            MgmtRecordingParticipant::new(ExternalId::K2Owner, Consent::Pending, false),
        )]
    }
}

fn summary_full() -> MgmtRecordingSummary {
    MgmtRecordingSummary {
        id: ID_RECORDING,
        space_id: Some(ID_SPACE),
        channel_id: ID_CHANNEL,
        state: RecordingState::Stopped,
        trigger: RecordingTrigger::Required,
        started_by: ext(PRINCIPAL),
        started_at: t(0),
        stopped_at: Some(t(1_800_000)),
        stop_reason: Some(StopReason::MgmtStopped),
        record_message_id: ID_MESSAGE,
        transcript_state: TranscriptState::Done,
        audio_state: AudioState::Available,
        audio_keep_days: 30,
        participants: participants(),
    }
}

fn summary_min() -> MgmtRecordingSummary {
    MgmtRecordingSummary {
        space_id: None,
        state: RecordingState::Recording,
        trigger: RecordingTrigger::Manual,
        stopped_at: None,
        stop_reason: None,
        transcript_state: TranscriptState::Pending,
        audio_keep_days: 0,
        participants: vec![],
        ..summary_full()
    }
}

impl Example for MgmtRecordingSummary {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![("full", summary_full()), ("min", summary_min())]
    }
}

impl Example for MgmtRecordingsQuery {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MgmtRecordingsQuery {
                    app_id: Some(APP.into()),
                    after: Some(ID_RECORDING),
                    limit: Some(MGMT_RECORDINGS_LIMIT_MAX),
                },
            ),
            ("min", MgmtRecordingsQuery::default()),
        ]
    }
}

impl Example for MgmtRecordingsPage {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MgmtRecordingsPage {
                    items: vec![summary_full()],
                    next: Some(ID_RECORDING),
                },
            ),
            ("min", MgmtRecordingsPage::default()),
        ]
    }
}

impl Example for MgmtTranscriptQuery {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MgmtTranscriptQuery {
                    app_id: Some(APP.into()),
                    after: Some(ID_REPLY),
                    limit: Some(MGMT_TRANSCRIPT_LIMIT_MAX),
                },
            ),
            ("min", MgmtTranscriptQuery::default()),
        ]
    }
}

fn turn() -> MgmtTranscriptTurn {
    MgmtTranscriptTurn::new(
        ID_REPLY,
        ext(PRINCIPAL),
        1_200,
        4_800,
        "Let's start with the hiring plan.".into(),
    )
}

impl Example for MgmtTranscriptTurn {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![("full", turn())]
    }
}

impl Example for MgmtTranscriptPage {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MgmtTranscriptPage {
                    recording_id: ID_RECORDING,
                    transcript_state: TranscriptState::Running,
                    turns: vec![turn()],
                    next: Some(ID_REPLY),
                },
            ),
            (
                "min",
                MgmtTranscriptPage::new(ID_RECORDING, TranscriptState::Done),
            ),
        ]
    }
}

impl Example for MgmtRecordingChangesQuery {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MgmtRecordingChangesQuery {
                    after: Some(41),
                    wait: Some(RECORDING_CHANGES_WAIT_MAX_S),
                    limit: Some(RECORDING_CHANGES_LIMIT_MAX),
                },
            ),
            ("min", MgmtRecordingChangesQuery::default()),
        ]
    }
}

fn change(seq: u64, kind: RecordingEventKind, at_ms: u64) -> MgmtRecordingChange {
    MgmtRecordingChange::new(seq, kind, ID_RECORDING, ID_SPACE, ID_CHANNEL, t(at_ms))
}

impl Example for MgmtRecordingChange {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![("full", change(42, RecordingEventKind::Started, 0))]
    }
}

impl Example for MgmtRecordingChanges {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MgmtRecordingChanges {
                    items: vec![
                        change(42, RecordingEventKind::Started, 0),
                        change(43, RecordingEventKind::Stopped, 1_800_000),
                        change(44, RecordingEventKind::TranscriptReady, 1_900_000),
                    ],
                    next: 44,
                },
            ),
            (
                "min",
                MgmtRecordingChanges {
                    items: vec![],
                    next: 41,
                },
            ),
        ]
    }
}
