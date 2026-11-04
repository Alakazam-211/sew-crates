// Copyright 2023 LiveKit, Inc.
// Copyright 2026 Alakazam Labs
// SPDX-License-Identifier: Apache-2.0
// Rewritten from a written behaviour specification of the earlier port of livekit/livekit's test client (Apache-2.0).

//! The other participants in the room, keyed by participant sid.

use std::collections::BTreeMap;

use sew_lkproto::livekit::ParticipantInfo;
use sew_lkproto::livekit::participant_info::State;

fn has_left(p: &ParticipantInfo) -> bool {
    p.state == State::Disconnected as i32
}

#[derive(Debug, Default)]
pub(super) struct Roster {
    by_sid: BTreeMap<String, ParticipantInfo>,
}

impl Roster {
    /// Start over from a join answer's list, leaving out anyone already gone.
    pub(super) fn reload(&mut self, people: Vec<ParticipantInfo>) {
        self.by_sid = people
            .into_iter()
            .filter(|p| !has_left(p))
            .map(|p| (p.sid.clone(), p))
            .collect();
    }

    /// Apply one update record: a departure removes the entry, anything else replaces it whole.
    pub(super) fn apply(&mut self, p: ParticipantInfo) {
        if has_left(&p) {
            self.by_sid.remove(&p.sid);
        } else {
            self.by_sid.insert(p.sid.clone(), p);
        }
    }

    pub(super) fn as_map(&self) -> &BTreeMap<String, ParticipantInfo> {
        &self.by_sid
    }

    /// The first participant, in sid order, that lists a track with this sid.
    pub(super) fn owner_of(&self, track_sid: &str) -> Option<&ParticipantInfo> {
        self.by_sid
            .values()
            .find(|p| p.tracks.iter().any(|t| t.sid == track_sid))
    }
}
