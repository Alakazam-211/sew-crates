//! Examples for the wire types of the member-channels sync.

use super::{ID_BOB, ID_CHANNEL, ID_JOB, ID_OTHER_CHANNEL, ID_SPACE, PRINCIPAL, WORKSPACE, ext};
use crate::api::spaces::SpaceLevel;
use crate::error::{ApiError, Code, ReasonDetails};
use crate::mgmt::*;
use crate::registry::Example;

impl Example for MgmtMemberChannelsRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MgmtMemberChannelsRequest {
                    app_id: Some("jobs".into()),
                    channel_ids: vec![ID_CHANNEL, ID_OTHER_CHANNEL],
                    level: SpaceLevel::Moderator,
                },
            ),
            (
                "min",
                MgmtMemberChannelsRequest {
                    app_id: Some("jobs".into()),
                    channel_ids: vec![],
                    level: SpaceLevel::Member,
                },
            ),
        ]
    }
}

fn answer_full() -> MgmtMemberChannelsResponse {
    MgmtMemberChannelsResponse {
        external_id: ext(PRINCIPAL),
        member_id: ID_BOB,
        added: vec![ID_CHANNEL],
        removed: vec![ID_SPACE],
        unchanged: vec![ID_OTHER_CHANNEL],
        level: Some(SpaceLevel::Moderator),
        unknown_channel_ids: vec![ID_JOB],
        skipped: None,
        queued: false,
        job_id: None,
    }
}

fn answer_skipped() -> MgmtMemberChannelsResponse {
    MgmtMemberChannelsResponse {
        external_id: ext(PRINCIPAL),
        member_id: ID_BOB,
        added: vec![],
        removed: vec![],
        unchanged: vec![],
        level: None,
        unknown_channel_ids: vec![],
        skipped: Some(MgmtMemberChannelsSkip::Banned),
        queued: false,
        job_id: None,
    }
}

fn answer_queued() -> MgmtMemberChannelsResponse {
    MgmtMemberChannelsResponse {
        external_id: ext(WORKSPACE),
        member_id: ID_BOB,
        added: vec![],
        removed: vec![],
        unchanged: vec![],
        level: Some(SpaceLevel::Member),
        unknown_channel_ids: vec![],
        skipped: None,
        queued: true,
        job_id: Some(ID_JOB),
    }
}

impl Example for MgmtMemberChannelsResponse {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            ("full", answer_full()),
            ("full_skipped", answer_skipped()),
            ("full_queued", answer_queued()),
        ]
    }
}

impl Example for MgmtMemberChannelsBulkRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            MgmtMemberChannelsBulkRequest {
                app_id: Some("jobs".into()),
                items: vec![
                    MgmtMemberChannelsItem {
                        external_id: ext(PRINCIPAL),
                        channel_ids: vec![ID_CHANNEL, ID_OTHER_CHANNEL],
                        level: SpaceLevel::Moderator,
                    },
                    MgmtMemberChannelsItem {
                        external_id: ext(WORKSPACE),
                        channel_ids: vec![],
                        level: SpaceLevel::Member,
                    },
                ],
            },
        )]
    }
}

impl Example for MgmtMemberChannelsBulkResponse {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            MgmtMemberChannelsBulkResponse {
                items: vec![
                    MgmtMemberChannelsItemResult {
                        external_id: ext(PRINCIPAL),
                        status: 200,
                        answer: Some(answer_full()),
                        error: None,
                    },
                    MgmtMemberChannelsItemResult {
                        external_id: ext(WORKSPACE),
                        status: 422,
                        answer: None,
                        error: Some(
                            ApiError::new(
                                Code::InvalidState,
                                "an agent is synced at level member only",
                            )
                            .with_details(ReasonDetails::with_field(reasons::AGENT_LEVEL, "level")),
                        ),
                    },
                ],
            },
        )]
    }
}
