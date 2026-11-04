//! Examples for the wire types of meeting locks and removals.

use super::id;
use crate::api::meetings::*;
use crate::registry::Example;

const ID_GUEST: crate::id::Id = id(610);

impl Example for LockMeetingRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![("full", LockMeetingRequest { locked: true })]
    }
}

impl Example for RemoveMeetingMemberRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            RemoveMeetingMemberRequest {
                member_id: ID_GUEST,
            },
        )]
    }
}
