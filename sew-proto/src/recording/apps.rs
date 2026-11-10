//! Recording in Apps: what started a recording, the rule a room records under, the level from
//! which members of an App's Spaces may record, the org policy that requires recording, and the
//! kinds of an App's recording feed.
//!
//! None of these values changes anything on an install that sets no recording setting: such an
//! install's recordings are `manual` or `auto`, its rooms `none` or `auto`, and its Apps'
//! start level `none`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

coded_enum! {
    /// What started a recording. Stored in `recordings.trigger`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    #[non_exhaustive]
    pub enum RecordingTrigger {
        /// A person pressed Record (the start route).
        Manual = "manual" => 1,
        /// An auto-record room's first join.
        Auto = "auto" => 2,
        /// The room reached two people while recording is required.
        Required = "required" => 3,
    }
}

impl RecordingTrigger {
    /// The trigger of a recording known only by its `auto` flag (rows written before
    /// triggers existed): `auto` or `manual`.
    pub const fn from_auto(auto: bool) -> RecordingTrigger {
        if auto {
            RecordingTrigger::Auto
        } else {
            RecordingTrigger::Manual
        }
    }

    /// Whether the recording started without anyone pressing Record (`auto` or `required`).
    pub const fn is_automatic(self) -> bool {
        matches!(self, RecordingTrigger::Auto | RecordingTrigger::Required)
    }
}

/// `manual`: what a payload without `trigger` reads as.
impl Default for RecordingTrigger {
    fn default() -> RecordingTrigger {
        RecordingTrigger::Manual
    }
}

/// `none`: what a payload without `recording_rule` reads as.
impl Default for RecordingRule {
    fn default() -> RecordingRule {
        RecordingRule::None
    }
}

impl RecordingRule {
    /// Whether this is `none` (the field is then left out).
    pub fn is_none(&self) -> bool {
        *self == RecordingRule::None
    }
}

unit_enum! {
    /// The rule a voice room records under, as its settings and their sources resolve it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    #[non_exhaustive]
    pub enum RecordingRule {
        /// People press Record.
        None = "none",
        /// The room records from its first join.
        Auto = "auto",
        /// The room records every call with two or more people.
        Required = "required",
    }
}

unit_enum! {
    /// The lowest level of an App's governed Space whose members may start and stop
    /// recordings. `member` means everyone in the Space. Stored as its wire string.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    #[non_exhaustive]
    pub enum RecordingStartLevel {
        /// Nobody through the App's levels (the default; roles and overwrites still apply).
        None = "none",
        /// Admins.
        Admin = "admin",
        /// Moderators and admins.
        Moderator = "moderator",
        /// Everyone in the Space.
        Member = "member",
    }
}

impl RecordingStartLevel {
    /// The level used when nothing is set.
    pub const DEFAULT: RecordingStartLevel = RecordingStartLevel::None;
}

unit_enum! {
    /// Org policy `recording.required`: whether rooms record every call with two or more
    /// people.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    #[non_exhaustive]
    pub enum RecordingRequiredPolicy {
        /// Not required by the org (the default; Apps, Spaces and channels may still require
        /// it).
        Off = "off",
        /// Every call with two or more people is recorded.
        MultiPerson = "multi_person",
    }
}

coded_enum! {
    /// What an event of an App's recording feed is. Stored in `recording_events.kind`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    #[non_exhaustive]
    pub enum RecordingEventKind {
        /// A recording started.
        Started = "started" => 1,
        /// A recording stopped.
        Stopped = "stopped" => 2,
        /// Its transcript became final.
        TranscriptReady = "transcript_ready" => 3,
        /// Its audio expired.
        AudioExpired = "audio_expired" => 4,
        /// Its record message was deleted.
        Deleted = "deleted" => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_frozen() {
        let t: Vec<(i16, &str)> = RecordingTrigger::ALL
            .iter()
            .map(|v| (v.as_i16(), v.as_str()))
            .collect();
        assert_eq!(t, [(1, "manual"), (2, "auto"), (3, "required")]);
        let k: Vec<(i16, &str)> = RecordingEventKind::ALL
            .iter()
            .map(|v| (v.as_i16(), v.as_str()))
            .collect();
        assert_eq!(
            k,
            [
                (1, "started"),
                (2, "stopped"),
                (3, "transcript_ready"),
                (4, "audio_expired"),
                (5, "deleted")
            ]
        );
        assert_eq!(RecordingTrigger::from_i16(0), None);
        assert_eq!(RecordingEventKind::from_i16(6), None);
    }

    #[test]
    fn trigger_from_auto() {
        assert_eq!(RecordingTrigger::from_auto(true), RecordingTrigger::Auto);
        assert_eq!(RecordingTrigger::from_auto(false), RecordingTrigger::Manual);
        assert!(RecordingTrigger::Required.is_automatic());
        assert!(!RecordingTrigger::Manual.is_automatic());
    }

    #[test]
    fn levels_and_policy_wire() {
        let l: Vec<&str> = RecordingStartLevel::ALL
            .iter()
            .map(|v| v.as_str())
            .collect();
        assert_eq!(l, ["none", "admin", "moderator", "member"]);
        assert_eq!(RecordingStartLevel::DEFAULT, RecordingStartLevel::None);
        assert_eq!(
            RecordingRequiredPolicy::from_wire("multi_person"),
            Some(RecordingRequiredPolicy::MultiPerson)
        );
        let r: Vec<&str> = RecordingRule::ALL.iter().map(|v| v.as_str()).collect();
        assert_eq!(r, ["none", "auto", "required"]);
    }
}
