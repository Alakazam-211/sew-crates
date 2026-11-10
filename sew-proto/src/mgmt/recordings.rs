//! Recording in Apps over mgmt: an App's recording settings, a room's recording settings,
//! stopping a recording, reading recordings and transcripts, and an App's recording feed.
//!
//! - Flag [`FEATURE_APP_RECORDING`]: the `recording` object of
//!   `POST /mgmt/v1/apps/{app_id}/settings` ([`MgmtAppRecordingRequest`] →
//!   [`MgmtAppRecording`]); `POST /mgmt/v1/channels/{channel_id}/recording`
//!   ([`MgmtChannelRecordingRequest`] → [`MgmtChannelRecording`]) sets a voice room's recording
//!   settings; `POST /mgmt/v1/recordings/{id}/stop` ([`MgmtRecordingStopRequest`] →
//!   [`MgmtRecordingSummary`]) stops a recording (stop reason `mgmt_stopped`). The stop belongs
//!   to the install's Owner or an Admin, named by `Sew-Actor`; an App never stops a recording,
//!   so a stop naming one is refused.
//! - Flag [`FEATURE_MGMT_RECORDING_READS`]: `GET /mgmt/v1/spaces/{space_id}/recordings`
//!   ([`MgmtRecordingsQuery`] → [`MgmtRecordingsPage`]), `GET /mgmt/v1/recordings/{id}`
//!   ([`MgmtRecordingQuery`] → [`MgmtRecordingSummary`]),
//!   `GET /mgmt/v1/recordings/{id}/transcript` ([`MgmtTranscriptQuery`] →
//!   [`MgmtTranscriptPage`]) and the long-poll `GET /mgmt/v1/apps/{app_id}/recordings/changes`
//!   ([`MgmtRecordingChangesQuery`] → [`MgmtRecordingChanges`]). With `app_id` a read is the
//!   governing App's view: only Spaces it governs, and only with its `recording.server_read`
//!   setting (else `403 policy_denied`
//!   [`reasons::RECORDING_SERVER_READ_OFF`](super::reasons::RECORDING_SERVER_READ_OFF)), which
//!   the feed needs too. Without `app_id` a read is the compliance view of the install's Owner
//!   or an Admin named by `Sew-Actor`, over every Space. The feed carries no content.
//! - Flag [`FEATURE_RECORDING_REQUIRED`]: rooms that record every call with two or more people.
//! - Flag [`FEATURE_VOICE_RECORD_CAP`]: the pass cap `voice:record`.
//!
//! No route here ever returns audio, and none answers or changes anyone's consent.
//!
//! The flags are listed in [`super::FEATURES`] once the server serves what they name; a client
//! tests the flag, never the version.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::spaces::check_app_id;
use crate::error::{Validate, ValidationError};
use crate::external_id::ExternalId;
use crate::http::Patch;
use crate::id::Id;
use crate::policy::RECORDING_AUDIO_DAYS_LIMIT;
use crate::recording::{
    AudioState, Consent, RecordingEventKind, RecordingRule, RecordingStartLevel, RecordingState,
    RecordingTrigger, StopReason, TranscriptState,
};
use crate::time::Timestamp;

/// App, Space and channel recording settings over mgmt, and the mgmt stop.
pub const FEATURE_APP_RECORDING: &str = "app_recording";
/// The pass cap `voice:record` (a ceiling on starting and stopping recordings).
pub const FEATURE_VOICE_RECORD_CAP: &str = "voice_record_cap";
/// Recording required for every call with two or more people.
pub const FEATURE_RECORDING_REQUIRED: &str = "recording_required";
/// The mgmt recording reads, the transcript route and an App's recording feed.
pub const FEATURE_MGMT_RECORDING_READS: &str = "mgmt_recording_reads";

/// How long an App's recording feed keeps an event, in days.
pub const RECORDING_EVENTS_KEEP_DAYS: u32 = 7;
/// The longest `recordings/changes` long-poll, in seconds.
pub const RECORDING_CHANGES_WAIT_MAX_S: u32 = 30;
/// The default `recordings/changes` page size.
pub const RECORDING_CHANGES_LIMIT_DEFAULT: u32 = 100;
/// The largest `recordings/changes` page size.
pub const RECORDING_CHANGES_LIMIT_MAX: u32 = 500;
/// The default page size of a Space's recordings.
pub const MGMT_RECORDINGS_LIMIT_DEFAULT: u32 = 50;
/// The largest page size of a Space's recordings.
pub const MGMT_RECORDINGS_LIMIT_MAX: u32 = 100;
/// The default page size of a transcript read.
pub const MGMT_TRANSCRIPT_LIMIT_DEFAULT: u32 = 200;
/// The largest page size of a transcript read.
pub const MGMT_TRANSCRIPT_LIMIT_MAX: u32 = 1_000;

fn check_opt_app_id(v: Option<&str>) -> Result<(), ValidationError> {
    v.map_or(Ok(()), |a| check_app_id("app_id", a))
}

fn check_limit(v: Option<u32>, max: u32) -> Result<(), ValidationError> {
    match v {
        Some(l) if !(1..=max).contains(&l) => Err(ValidationError::invalid(
            "limit",
            format!("limit must be 1–{max}"),
        )),
        _ => Ok(()),
    }
}

/// Body of `POST /mgmt/v1/channels/{channel_id}/recording`: any subset of a voice room's
/// recording settings. Absent fields are kept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MgmtChannelRecordingRequest {
    /// The Space's governing App. Required in an App's Space (a participant App gets
    /// `409 app_not_governing`); absent for a Space no App governs, such as the server-governed
    /// Space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Record from the room's first join.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    /// Record every call with two or more people; `null` inherits from the Space, its App
    /// and the org. `false` never turns off a rule a higher level sets.
    #[serde(default, skip_serializing_if = "Patch::is_keep")]
    pub required: Patch<bool>,
    /// The room's default audio keep days (0 = until the transcript is final); `null`
    /// inherits the org's default.
    #[serde(default, skip_serializing_if = "Patch::is_keep")]
    pub audio_days: Patch<u16>,
    /// Declining the recording notice means not joining; `null` inherits the org policy.
    #[serde(default, skip_serializing_if = "Patch::is_keep")]
    pub require_consent: Patch<bool>,
}

impl MgmtChannelRecordingRequest {
    /// A request that changes nothing; set the fields to change.
    pub fn new() -> MgmtChannelRecordingRequest {
        MgmtChannelRecordingRequest::default()
    }
}

impl Validate for MgmtChannelRecordingRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        check_opt_app_id(self.app_id.as_deref())?;
        if let Patch::Set(d) = self.audio_days
            && d > RECORDING_AUDIO_DAYS_LIMIT
        {
            return Err(ValidationError::invalid(
                "audio_days",
                format!("audio_days must be at most {RECORDING_AUDIO_DAYS_LIMIT}"),
            ));
        }
        Ok(())
    }
}

/// A voice room's recording settings: the answer of
/// `POST /mgmt/v1/channels/{channel_id}/recording`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct MgmtChannelRecording {
    /// The voice channel.
    pub channel_id: Id,
    /// Records from the room's first join.
    pub auto: bool,
    /// The room's own "record every call with two or more people"; absent when it inherits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// The room's default audio keep days; absent when it inherits the org's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_days: Option<u16>,
    /// Declining means not joining; absent when it inherits the org policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_consent: Option<bool>,
    /// The rule the room records under now, every source applied.
    pub rule: RecordingRule,
}

impl MgmtChannelRecording {
    /// The settings of a room that sets nothing (`auto` off, everything inherited, rule
    /// `none`).
    pub fn new(channel_id: Id) -> MgmtChannelRecording {
        MgmtChannelRecording {
            channel_id,
            auto: false,
            required: None,
            audio_days: None,
            require_consent: None,
            rule: RecordingRule::None,
        }
    }
}

/// Body of `POST /mgmt/v1/recordings/{id}/stop`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MgmtRecordingStopRequest {
    /// Always absent: an App never stops a recording, so a stop naming one is refused
    /// (`400 validation_failed`, field `app_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
}

impl MgmtRecordingStopRequest {
    /// A stop request (pass `None`: a stop naming an App is refused).
    pub fn new(app_id: Option<String>) -> MgmtRecordingStopRequest {
        MgmtRecordingStopRequest { app_id }
    }
}

impl Validate for MgmtRecordingStopRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        check_opt_app_id(self.app_id.as_deref())
    }
}

/// Query of `GET /mgmt/v1/recordings/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MgmtRecordingQuery {
    /// The governing App of the recording's Space (its view); absent: the compliance view of
    /// the install's Owner or an Admin named by `Sew-Actor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
}

impl MgmtRecordingQuery {
    /// A read on behalf of `app_id` (`None`: the compliance view).
    pub fn new(app_id: Option<String>) -> MgmtRecordingQuery {
        MgmtRecordingQuery { app_id }
    }
}

impl Validate for MgmtRecordingQuery {
    fn validate(&self) -> Result<(), ValidationError> {
        check_opt_app_id(self.app_id.as_deref())
    }
}

/// One person of a recording, as an App's server sees them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct MgmtRecordingParticipant {
    /// The member's external id.
    pub external_id: ExternalId,
    /// What they said to the recording notice.
    pub consent: Consent,
    /// Whether any of their speech was recorded.
    pub recorded: bool,
}

impl MgmtRecordingParticipant {
    /// A participant.
    pub fn new(external_id: ExternalId, consent: Consent, recorded: bool) -> Self {
        MgmtRecordingParticipant {
            external_id,
            consent,
            recorded,
        }
    }
}

/// One recording as an App's server sees it: no content, no audio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct MgmtRecordingSummary {
    /// The recording.
    pub id: Id,
    /// The Space (absent for a DM call).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<Id>,
    /// The voice channel or DM.
    pub channel_id: Id,
    /// Its state.
    pub state: RecordingState,
    /// What started it.
    pub trigger: RecordingTrigger,
    /// Who started it (for `required`, the person whose join made two).
    pub started_by: ExternalId,
    /// When it started.
    pub started_at: Timestamp,
    /// When it stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<Timestamp>,
    /// Why it stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    /// The record message (its thread holds the transcript turns).
    pub record_message_id: Id,
    /// The transcript.
    pub transcript_state: TranscriptState,
    /// The audio (never served to an App).
    pub audio_state: AudioState,
    /// Audio keep days (0 = until the transcript is final).
    pub audio_keep_days: u16,
    /// The people in the recording, in member order.
    pub participants: Vec<MgmtRecordingParticipant>,
}

impl MgmtRecordingSummary {
    /// A summary of a manual recording that is starting, with a pending transcript and
    /// available audio, no keep days and no participants; set the other fields.
    pub fn new(
        id: Id,
        channel_id: Id,
        started_by: ExternalId,
        started_at: Timestamp,
        record_message_id: Id,
    ) -> MgmtRecordingSummary {
        MgmtRecordingSummary {
            id,
            space_id: None,
            channel_id,
            state: RecordingState::Starting,
            trigger: RecordingTrigger::Manual,
            started_by,
            started_at,
            stopped_at: None,
            stop_reason: None,
            record_message_id,
            transcript_state: TranscriptState::Pending,
            audio_state: AudioState::Available,
            audio_keep_days: 0,
            participants: Vec::new(),
        }
    }
}

/// Query of `GET /mgmt/v1/spaces/{space_id}/recordings`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MgmtRecordingsQuery {
    /// The Space's governing App (its view); absent: the compliance view of the install's
    /// Owner or an Admin named by `Sew-Actor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Recordings with ids above this one (the previous page's `next`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Id>,
    /// At most this many (1–100, default 50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl MgmtRecordingsQuery {
    /// The page size, default 50.
    pub fn limit(&self) -> u32 {
        self.limit.unwrap_or(MGMT_RECORDINGS_LIMIT_DEFAULT)
    }
}

impl Validate for MgmtRecordingsQuery {
    fn validate(&self) -> Result<(), ValidationError> {
        check_opt_app_id(self.app_id.as_deref())?;
        check_limit(self.limit, MGMT_RECORDINGS_LIMIT_MAX)
    }
}

/// Response of `GET /mgmt/v1/spaces/{space_id}/recordings`: oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct MgmtRecordingsPage {
    /// The recordings.
    pub items: Vec<MgmtRecordingSummary>,
    /// The `after` of the next page; absent on the last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<Id>,
}

/// Query of `GET /mgmt/v1/recordings/{id}/transcript`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MgmtTranscriptQuery {
    /// The governing App of the recording's Space (its view); absent: the compliance view of
    /// the install's Owner or an Admin named by `Sew-Actor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Turns after this turn message (the previous page's `next`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Id>,
    /// At most this many turns (1–1000, default 200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl MgmtTranscriptQuery {
    /// The page size, default 200.
    pub fn limit(&self) -> u32 {
        self.limit.unwrap_or(MGMT_TRANSCRIPT_LIMIT_DEFAULT)
    }
}

impl Validate for MgmtTranscriptQuery {
    fn validate(&self) -> Result<(), ValidationError> {
        check_opt_app_id(self.app_id.as_deref())?;
        check_limit(self.limit, MGMT_TRANSCRIPT_LIMIT_MAX)
    }
}

/// One transcript turn as an App's server reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct MgmtTranscriptTurn {
    /// The turn's message (a reply in the record message's thread).
    pub message_id: Id,
    /// The speaker's external id.
    pub speaker_external_id: ExternalId,
    /// Start, ms from the recording start.
    pub start_ms: u64,
    /// End, ms from the recording start.
    pub end_ms: u64,
    /// What was said, as posted in the turn message (markdown-escaped).
    pub text: String,
}

impl MgmtTranscriptTurn {
    /// A turn.
    pub fn new(
        message_id: Id,
        speaker_external_id: ExternalId,
        start_ms: u64,
        end_ms: u64,
        text: String,
    ) -> Self {
        MgmtTranscriptTurn {
            message_id,
            speaker_external_id,
            start_ms,
            end_ms,
            text,
        }
    }
}

/// Response of `GET /mgmt/v1/recordings/{id}/transcript`: turns in spoken order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct MgmtTranscriptPage {
    /// The recording.
    pub recording_id: Id,
    /// The transcript's state (`running`: more turns may come).
    pub transcript_state: TranscriptState,
    /// The turns.
    pub turns: Vec<MgmtTranscriptTurn>,
    /// The `after` of the next page; absent on the last page posted so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<Id>,
}

impl MgmtTranscriptPage {
    /// An empty page of `recording_id`'s transcript.
    pub fn new(recording_id: Id, transcript_state: TranscriptState) -> Self {
        MgmtTranscriptPage {
            recording_id,
            transcript_state,
            turns: Vec::new(),
            next: None,
        }
    }
}

/// Query of `GET /mgmt/v1/apps/{app_id}/recordings/changes`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MgmtRecordingChangesQuery {
    /// Events after this `seq` (the previous page's `next`); absent: from the oldest kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    /// Long-poll up to this many seconds while there is nothing to return (0–30, default 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<u32>,
    /// At most this many events (1–500, default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl MgmtRecordingChangesQuery {
    /// The page size, default 100.
    pub fn limit(&self) -> u32 {
        self.limit.unwrap_or(RECORDING_CHANGES_LIMIT_DEFAULT)
    }

    /// The long-poll, in seconds (default 0: answer at once).
    pub fn wait_s(&self) -> u32 {
        self.wait.unwrap_or(0)
    }
}

impl Validate for MgmtRecordingChangesQuery {
    fn validate(&self) -> Result<(), ValidationError> {
        check_limit(self.limit, RECORDING_CHANGES_LIMIT_MAX)?;
        if let Some(w) = self.wait
            && w > RECORDING_CHANGES_WAIT_MAX_S
        {
            return Err(ValidationError::invalid(
                "wait",
                format!("wait must be at most {RECORDING_CHANGES_WAIT_MAX_S} seconds"),
            ));
        }
        Ok(())
    }
}

/// One event of an App's recording feed. No content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct MgmtRecordingChange {
    /// The event's place in the App's feed (increasing; gaps are never skipped events).
    pub seq: u64,
    /// What happened.
    pub kind: RecordingEventKind,
    /// The recording.
    pub recording_id: Id,
    /// Its Space (one the App governed when it happened).
    pub space_id: Id,
    /// Its voice channel.
    pub channel_id: Id,
    /// When it happened.
    pub at: Timestamp,
}

impl MgmtRecordingChange {
    /// An event.
    pub fn new(
        seq: u64,
        kind: RecordingEventKind,
        recording_id: Id,
        space_id: Id,
        channel_id: Id,
        at: Timestamp,
    ) -> Self {
        MgmtRecordingChange {
            seq,
            kind,
            recording_id,
            space_id,
            channel_id,
            at,
        }
    }
}

/// Response of `GET /mgmt/v1/apps/{app_id}/recordings/changes`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct MgmtRecordingChanges {
    /// The events, oldest first.
    pub items: Vec<MgmtRecordingChange>,
    /// The `after` of the next call (the last event's `seq`, or the query's `after` when
    /// there was none).
    pub next: u64,
}

/// The `recording` object of `POST /mgmt/v1/apps/{app_id}/settings`: any subset of an App's
/// recording settings; absent fields are kept. Refused for the home client (`409 conflict`
/// `home_client_cannot_govern`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MgmtAppRecordingRequest {
    /// The lowest level of the App's governed Spaces whose members may start and stop
    /// recordings (`member`: everyone in the Space).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_level: Option<RecordingStartLevel>,
    /// Record every call with two or more people in the App's governed Spaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// The App's sessions need the pass cap `voice:record` to start or stop recordings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap_required: Option<bool>,
    /// The App's server may read the recordings, transcripts and recording feed of its
    /// governed Spaces over mgmt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_read: Option<bool>,
    /// The most audio keep days a recording in the App's Spaces may have (the org's
    /// `recording.audio_max_days` still caps it); `null` clears it.
    #[serde(default, skip_serializing_if = "Patch::is_keep")]
    pub audio_max_days: Patch<u16>,
}

impl MgmtAppRecordingRequest {
    /// A request that changes nothing; set the fields to change.
    pub fn new() -> MgmtAppRecordingRequest {
        MgmtAppRecordingRequest::default()
    }

    /// Whether the request names no field.
    pub fn is_empty(&self) -> bool {
        self.start_level.is_none()
            && self.required.is_none()
            && self.cap_required.is_none()
            && self.server_read.is_none()
            && self.audio_max_days.is_keep()
    }
}

impl Validate for MgmtAppRecordingRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(l) = self.start_level {
            crate::error::check_known("recording.start_level", l)?;
        }
        if let Patch::Set(d) = self.audio_max_days
            && d > RECORDING_AUDIO_DAYS_LIMIT
        {
            return Err(ValidationError::invalid(
                "recording.audio_max_days",
                format!("audio_max_days must be at most {RECORDING_AUDIO_DAYS_LIMIT}"),
            ));
        }
        Ok(())
    }
}

/// An App's recording settings, in the answer of `GET`/`POST /mgmt/v1/apps/{app_id}/settings`
/// (left out while every field has its default).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct MgmtAppRecording {
    /// The lowest level whose members may record (default `none`).
    #[serde(default = "default_start_level")]
    pub start_level: RecordingStartLevel,
    /// Every call with two or more people is recorded (default `false`).
    #[serde(default)]
    pub required: bool,
    /// The App's sessions need `voice:record` to start or stop (default `false`).
    #[serde(default)]
    pub cap_required: bool,
    /// The App's server may read over mgmt (default `false`).
    #[serde(default)]
    pub server_read: bool,
    /// The App's cap on audio keep days; absent: the org's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_max_days: Option<u16>,
}

const fn default_start_level() -> RecordingStartLevel {
    RecordingStartLevel::DEFAULT
}

impl Default for MgmtAppRecording {
    fn default() -> Self {
        MgmtAppRecording {
            start_level: RecordingStartLevel::DEFAULT,
            required: false,
            cap_required: false,
            server_read: false,
            audio_max_days: None,
        }
    }
}

impl MgmtAppRecording {
    /// The defaults: nobody records through the App, nothing is required, no cap is needed
    /// and the App's server reads nothing.
    pub fn new() -> MgmtAppRecording {
        MgmtAppRecording::default()
    }

    /// Whether every field has its default.
    pub fn is_default(&self) -> bool {
        *self == MgmtAppRecording::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_request_patches_and_bounds() {
        let r: MgmtChannelRecordingRequest =
            serde_json::from_str(r#"{"app_id":"jobs","required":null,"audio_days":30}"#).unwrap();
        assert_eq!(r.required, Patch::Clear);
        assert_eq!(r.audio_days, Patch::Set(30));
        assert_eq!(r.require_consent, Patch::Keep);
        assert!(r.validate().is_ok());
        let mut bad = MgmtChannelRecordingRequest::new();
        bad.audio_days = Patch::Set(RECORDING_AUDIO_DAYS_LIMIT + 1);
        assert!(bad.validate().is_err());
        let mut bad = MgmtChannelRecordingRequest::new();
        bad.app_id = Some("Jobs!".into());
        assert!(bad.validate().is_err());
        assert!(
            serde_json::from_str::<MgmtChannelRecordingRequest>(r#"{"zz":1}"#).is_err(),
            "unknown fields are refused"
        );
    }

    #[test]
    fn query_limits() {
        let mut q = MgmtRecordingChangesQuery::default();
        assert_eq!((q.limit(), q.wait_s()), (100, 0));
        q.wait = Some(31);
        assert!(q.validate().is_err());
        let mut q = MgmtRecordingsQuery::default();
        assert_eq!(q.limit(), 50);
        q.limit = Some(101);
        assert!(q.validate().is_err());
        let mut q = MgmtTranscriptQuery::default();
        assert_eq!(q.limit(), 200);
        q.limit = Some(0);
        assert!(q.validate().is_err());
    }
}
