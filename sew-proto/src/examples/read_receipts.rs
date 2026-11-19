//! Examples for read receipts in DMs: seen positions, the GET answer, the member setting, the
//! policy event and the export records.

use super::{ID_BOB, ID_CAROL, ID_DM, ID_ME, ID_MESSAGE, ID_REPLY, t};
use crate::api::read_receipts::*;
use crate::export::{DmSeenRecord, ReadReceiptPrefRecord};
use crate::registry::Example;

/// Bob has seen the DM up to the message.
pub(super) fn dm_seen_full() -> DmSeen {
    DmSeen::new(ID_DM, ID_BOB, ID_MESSAGE, t(60_000))
}

/// Read receipts on, shared.
pub(super) fn settings_full() -> ReadReceiptSettings {
    ReadReceiptSettings::new(true, true)
}

impl Example for DmSeen {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            ("full", dm_seen_full()),
            // The event that removes Bob's position (he stopped sharing).
            ("removed", DmSeen::removed(ID_DM, ID_BOB)),
        ]
    }
}

impl Example for DmReadReceipts {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                DmReadReceipts::new(
                    ID_DM,
                    true,
                    vec![
                        dm_seen_full(),
                        DmSeen::new(ID_DM, ID_CAROL, ID_REPLY, t(90_000)),
                    ],
                ),
            ),
            ("inactive", DmReadReceipts::new(ID_DM, false, vec![])),
        ]
    }
}

impl Example for ReadReceiptSettings {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            ("full", settings_full()),
            ("not_shared", ReadReceiptSettings::new(false, true)),
            ("policy_off", ReadReceiptSettings::new(true, false)),
        ]
    }
}

impl Example for SetReadReceiptsRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            ("full", SetReadReceiptsRequest::new(false)),
            ("share", SetReadReceiptsRequest::new(true)),
        ]
    }
}

impl Example for ReadReceiptsPolicyUpdated {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            ("full", ReadReceiptsPolicyUpdated::new(false)),
            ("on", ReadReceiptsPolicyUpdated::new(true)),
        ]
    }
}

impl Example for DmSeenRecord {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            DmSeenRecord::new(ID_DM, ID_BOB, ID_MESSAGE, t(60_000)),
        )]
    }
}

impl Example for ReadReceiptPrefRecord {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![("full", ReadReceiptPrefRecord::new(ID_ME, false, t(0)))]
    }
}
