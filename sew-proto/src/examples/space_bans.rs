//! Examples for the wire types of space bans and timeouts.

use super::{ID_BOB, ID_CAROL, ID_ME, ID_SPACE, t};
use crate::api::moderation::*;
use crate::export::{SpaceBanRecord, TimeoutCarryRecord};
use crate::http::Page;
use crate::registry::Example;

fn ban_full() -> SpaceBan {
    SpaceBan {
        space_id: ID_SPACE,
        member_id: ID_BOB,
        banned_at: t(5_000),
        banned_by: Some(ID_ME),
        reason: Some("Spam after two warnings.".into()),
        delete_window_s: 86_400,
    }
}

fn ban_min() -> SpaceBan {
    SpaceBan {
        space_id: ID_SPACE,
        member_id: ID_CAROL,
        banned_at: t(6_000),
        banned_by: None,
        reason: None,
        delete_window_s: 0,
    }
}

impl Example for BanRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                BanRequest {
                    member_id: ID_BOB,
                    reason: Some("Spam after two warnings.".into()),
                    delete_window_s: Some(86_400),
                },
            ),
            (
                "min",
                BanRequest {
                    member_id: ID_BOB,
                    reason: None,
                    delete_window_s: None,
                },
            ),
        ]
    }
}

impl Example for SpaceBan {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![("full", ban_full()), ("min", ban_min())]
    }
}

impl Example for Page<SpaceBan> {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            Page {
                items: vec![ban_full(), ban_min()],
                has_more: false,
            },
        )]
    }
}

impl Example for SpaceBanRecord {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                SpaceBanRecord {
                    space_id: ID_SPACE,
                    member_id: ID_BOB,
                    banned_at: t(5_000),
                    banned_by: Some(ID_ME),
                    reason: Some("Spam after two warnings.".into()),
                    delete_window_s: 86_400,
                },
            ),
            (
                "min",
                SpaceBanRecord {
                    space_id: ID_SPACE,
                    member_id: ID_CAROL,
                    banned_at: t(6_000),
                    banned_by: None,
                    reason: None,
                    delete_window_s: 0,
                },
            ),
        ]
    }
}

impl Example for TimeoutCarryRecord {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            TimeoutCarryRecord {
                space_id: ID_SPACE,
                member_id: ID_BOB,
                timeout_until: t(600_000),
            },
        )]
    }
}
