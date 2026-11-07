//! Erasing a principal's account over mgmt (in-app account deletion).
//!
//! - `POST /mgmt/v1/principals/erase {external_id, mode?, dry_run?}` deprovisions the member,
//!   scrubs its profile, cuts its external id mapping (the member keeps its id as a tombstone
//!   named `erased:<member_id>`, so a later sign-in with the same id makes a new member) and
//!   starts the erase job. The answer is `200` [`MgmtEraseResult`] when the job finishes within
//!   the inline window, else `202` [`MgmtEraseAccepted`].
//! - `GET /mgmt/v1/principals/erase/{job_id}` reads one erasure, in any state.
//! - `GET /mgmt/v1/principals/erasures?state=&after=&limit=` lists them.
//!
//! An erasure is safe to retry: a retry after the mapping cut finds the erasure by a keyed
//! hash of the old external id, never by the id itself.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationError, check_known};
use crate::external_id::ExternalId;
use crate::id::Id;
use crate::time::Timestamp;

/// The feature flag of the erase routes. Listed in [`super::FEATURES`]; K2 calls the routes
/// only when it sees it.
pub const FEATURE_PRINCIPAL_ERASE: &str = "principal_erase";

/// Default page size of `GET /mgmt/v1/principals/erasures`.
pub const MGMT_ERASURES_LIMIT_DEFAULT: u32 = 50;
/// Largest page size of `GET /mgmt/v1/principals/erasures`.
pub const MGMT_ERASURES_LIMIT_MAX: u32 = 100;

unit_enum! {
    /// What an erase does with the person's content.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum EraseMode {
        /// Messages, reactions and files stay, attributed to "Deleted user".
        Anonymise = "anonymise",
        /// The person's messages are tombstoned and purged, reactions removed, uploads deleted.
        DeleteContent = "delete_content",
    }
}

impl EraseMode {
    /// The mode used when a request leaves `mode` out: `delete_content` for an App user
    /// (`k2:principal:`), `anonymise` for everyone else (Connect users, local users, agents).
    pub fn default_for(e: &ExternalId) -> EraseMode {
        match e {
            ExternalId::K2Principal(_) => EraseMode::DeleteContent,
            _ => EraseMode::Anonymise,
        }
    }

    /// Whether this mode does at least what `other` does (`delete_content` covers
    /// `anonymise`).
    pub fn covers(self, other: EraseMode) -> bool {
        self == other || self == EraseMode::DeleteContent
    }
}

unit_enum! {
    /// Where an erasure stands.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum EraseState {
        /// The account is cut off; the job is still erasing.
        Running = "running",
        /// The pass is done, but some kept rows are pending (a legal hold, backups).
        Waiting = "waiting",
        /// Nothing is pending; any rows left are final.
        Completed = "completed",
    }
}

unit_enum! {
    /// What a kept row is. The set is open: a kind this build does not know is still kept.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum EraseKeptKind {
        /// Messages.
        Messages = "messages",
        /// Edit history.
        Revisions = "revisions",
        /// Reactions.
        Reactions = "reactions",
        /// Poll votes.
        PollVotes = "poll_votes",
        /// Files.
        Files = "files",
        /// Recordings.
        Recordings = "recordings",
        /// Speaker segments of recordings.
        RecordingSegments = "recording_segments",
        /// Mixed parts of recordings (they hold other people's audio too).
        RecordingMixes = "recording_mixes",
        /// Space content the person made (emoji, bookmarks, pins, groups).
        SpaceAssets = "space_assets",
        /// A sealed copy of who the person was, kept for a legal hold.
        Identity = "identity",
        /// Export archives made before the erase.
        Exports = "exports",
        /// Backup bundles made before the erase.
        Backups = "backups",
        /// Rows of the append-only audit log.
        AuditRows = "audit_rows",
    }
}

unit_enum! {
    /// Why a row was kept. The set is open: a reason this build does not know is still kept.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum EraseKeptReason {
        /// A legal hold covers it; it is erased when the hold lifts.
        LegalHold = "legal_hold",
        /// A keep-at-least policy (reserved; not sent today).
        RetentionMin = "retention_min",
        /// It holds other people's content too; it stays.
        SharedWithOthers = "shared_with_others",
        /// It lives in a backup bundle or export archive until that is pruned.
        BackupRetention = "backup_retention",
        /// The audit log is append-only.
        AuditLog = "audit_log",
    }
}

/// Body of `POST /mgmt/v1/principals/erase`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtEraseRequest {
    /// Whose account to erase. Never `link:` or `erased:`.
    pub external_id: ExternalId,
    /// What happens to the person's content. Absent: [`EraseMode::default_for`] the id (for an
    /// erasure already made, the mode it ran with). The answer echoes the mode that ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<EraseMode>,
    /// Count only: the same answer, computed without writing anything.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
}

impl Validate for MgmtEraseRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.external_id.is_link() {
            return Err(ValidationError::invalid(
                "external_id",
                "a meeting link guest has no account to erase",
            ));
        }
        if self.external_id.is_erased() {
            return Err(ValidationError::invalid(
                "external_id",
                "erased:<member_id> names an erased member; erase by the account's own id",
            ));
        }
        if let Some(m) = self.mode {
            check_known("mode", m)?;
        }
        Ok(())
    }
}

/// What an erasure erased, by kind. Every key is always present (zeros included).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtEraseCounts {
    /// The profile (1 once scrubbed).
    #[serde(default)]
    pub profile: u64,
    /// Sessions.
    #[serde(default)]
    pub sessions: u64,
    /// Call devices.
    #[serde(default)]
    pub devices: u64,
    /// Notification settings rows.
    #[serde(default)]
    pub notify_settings: u64,
    /// Drafts.
    #[serde(default)]
    pub drafts: u64,
    /// Read positions.
    #[serde(default)]
    pub read_positions: u64,
    /// Saved items.
    #[serde(default)]
    pub saved_items: u64,
    /// Pending uploads.
    #[serde(default)]
    pub uploads_pending: u64,
    /// Messages tombstoned and purged (`delete_content`).
    #[serde(default)]
    pub messages_deleted: u64,
    /// Messages now attributed to "Deleted user" (`anonymise`).
    #[serde(default)]
    pub messages_anonymised: u64,
    /// Reactions.
    #[serde(default)]
    pub reactions: u64,
    /// Poll votes.
    #[serde(default)]
    pub poll_votes: u64,
    /// Files.
    #[serde(default)]
    pub files: u64,
    /// Recordings of only this person.
    #[serde(default)]
    pub recordings: u64,
    /// The person's segments in shared recordings.
    #[serde(default)]
    pub recording_segments: u64,
    /// Others' forward snapshots of the person's messages, blanked.
    #[serde(default)]
    pub forward_snapshots: u64,
    /// Pinned assistants erased with the person.
    #[serde(default)]
    pub assistants: u64,
}

impl MgmtEraseCounts {
    /// Adds `other` to these counts, key by key (saturating).
    pub fn add(&mut self, other: &MgmtEraseCounts) {
        let MgmtEraseCounts {
            profile,
            sessions,
            devices,
            notify_settings,
            drafts,
            read_positions,
            saved_items,
            uploads_pending,
            messages_deleted,
            messages_anonymised,
            reactions,
            poll_votes,
            files,
            recordings,
            recording_segments,
            forward_snapshots,
            assistants,
        } = *other;
        for (mine, theirs) in [
            (&mut self.profile, profile),
            (&mut self.sessions, sessions),
            (&mut self.devices, devices),
            (&mut self.notify_settings, notify_settings),
            (&mut self.drafts, drafts),
            (&mut self.read_positions, read_positions),
            (&mut self.saved_items, saved_items),
            (&mut self.uploads_pending, uploads_pending),
            (&mut self.messages_deleted, messages_deleted),
            (&mut self.messages_anonymised, messages_anonymised),
            (&mut self.reactions, reactions),
            (&mut self.poll_votes, poll_votes),
            (&mut self.files, files),
            (&mut self.recordings, recordings),
            (&mut self.recording_segments, recording_segments),
            (&mut self.forward_snapshots, forward_snapshots),
            (&mut self.assistants, assistants),
        ] {
            *mine = mine.saturating_add(theirs);
        }
    }

    /// Whether every count is zero.
    pub fn is_zero(&self) -> bool {
        *self == MgmtEraseCounts::default()
    }
}

/// One kind of row an erasure kept, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtEraseKept {
    /// What was kept.
    pub kind: EraseKeptKind,
    /// How many.
    pub count: u64,
    /// Why.
    pub reason: EraseKeptReason,
    /// When it is expected to go, when that is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Timestamp>,
}

/// One erasure: what it erased, what it kept and where it stands. `200` of
/// `POST /mgmt/v1/principals/erase` and the answer of `GET /mgmt/v1/principals/erase/{job_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtEraseResult {
    /// The erasure's id (the status route's `{job_id}`). For a dry run, a fresh id that names
    /// nothing stored.
    pub job_id: Id,
    /// The erased member (its id stays, as a tombstone).
    pub member_id: Id,
    /// The mode that ran.
    pub mode: EraseMode,
    /// Whether this is a dry run (nothing was written).
    pub dry_run: bool,
    /// Where it stands.
    pub state: EraseState,
    /// When the erase was asked for.
    pub requested_at: Timestamp,
    /// When it reached `completed` (`null` before).
    pub finished_at: Option<Timestamp>,
    /// What it erased.
    pub erased: MgmtEraseCounts,
    /// What it kept.
    pub kept: Vec<MgmtEraseKept>,
    /// The latest `until` of the pending kept rows, present only when every pending row has
    /// one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_until: Option<Timestamp>,
}

/// `202` of `POST /mgmt/v1/principals/erase`: the account is cut off and the job is still
/// running. Poll `GET /mgmt/v1/principals/erase/{job_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtEraseAccepted {
    /// The erasure's id.
    pub job_id: Id,
    /// The erased member.
    pub member_id: Id,
    /// The mode that runs.
    pub mode: EraseMode,
    /// Always `true`.
    pub pending: bool,
}

/// Query of `GET /mgmt/v1/principals/erasures`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtErasuresQuery {
    /// Only erasures in this state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<EraseState>,
    /// Erasures with an id above this one (the previous page's `next`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Id>,
    /// Page size, 1–100 (default 50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl MgmtErasuresQuery {
    /// The page size, default [`MGMT_ERASURES_LIMIT_DEFAULT`].
    pub fn limit(&self) -> u32 {
        self.limit.unwrap_or(MGMT_ERASURES_LIMIT_DEFAULT)
    }
}

impl Validate for MgmtErasuresQuery {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(s) = self.state {
            check_known("state", s)?;
        }
        if let Some(l) = self.limit
            && !(1..=MGMT_ERASURES_LIMIT_MAX).contains(&l)
        {
            return Err(ValidationError::invalid(
                "limit",
                format!("limit must be 1–{MGMT_ERASURES_LIMIT_MAX}"),
            ));
        }
        Ok(())
    }
}

/// Answer of `GET /mgmt/v1/principals/erasures`: erasures by id ascending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtErasurePage {
    /// One page.
    pub items: Vec<MgmtEraseResult>,
    /// The `after` of the next page; absent on the last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<Id>,
}

/// `status.erasures`: account erasures not yet completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusErasures {
    /// Erasures still in their pass (`running`).
    pub running: u64,
    /// Erasures whose pass is done but whose kept rows are pending (`waiting`).
    pub waiting: u64,
    /// Age of the oldest running erasure since it was requested; absent when none runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_running_age_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::Handle;

    const P: &str = "8f14e45f-ceea-4e7a-9c3b-0123456789ab";

    #[test]
    fn flag_listed() {
        assert!(super::super::FEATURES.contains(&FEATURE_PRINCIPAL_ERASE));
    }

    #[test]
    fn default_mode_follows_the_id() {
        let app_user = ExternalId::K2Principal(P.into());
        assert_eq!(EraseMode::default_for(&app_user), EraseMode::DeleteContent);
        for e in [
            ExternalId::K2Connect(P.into()),
            ExternalId::K2Workspace(P.into()),
            ExternalId::Local(Handle::new("bob").expect("handle")),
            ExternalId::K2Owner,
        ] {
            assert_eq!(EraseMode::default_for(&e), EraseMode::Anonymise, "{e}");
        }
        assert!(EraseMode::DeleteContent.covers(EraseMode::Anonymise));
        assert!(EraseMode::Anonymise.covers(EraseMode::Anonymise));
        assert!(!EraseMode::Anonymise.covers(EraseMode::DeleteContent));
    }

    #[test]
    fn request_refuses_link_erased_and_unknown_modes() {
        let ok: MgmtEraseRequest =
            serde_json::from_str(&format!(r#"{{"external_id":"k2:principal:{P}"}}"#))
                .expect("parse");
        assert!(ok.validate().is_ok());
        assert!(!ok.dry_run);
        for ext in [
            "erased:48831765147549703",
            "link:48831765147549703:ABCDEFGHIJKLMNOP",
        ] {
            let r: MgmtEraseRequest =
                serde_json::from_str(&format!(r#"{{"external_id":"{ext}"}}"#)).expect("parse");
            assert_eq!(
                r.validate().map_err(|e| e.field),
                Err("external_id".into()),
                "{ext}"
            );
        }
        let r: MgmtEraseRequest = serde_json::from_str(&format!(
            r#"{{"external_id":"k2:principal:{P}","mode":"shred"}}"#
        ))
        .expect("parse");
        assert_eq!(r.validate().map_err(|e| e.field), Err("mode".into()));
    }

    #[test]
    fn counts_always_carry_every_key_and_add() {
        let mut a = MgmtEraseCounts {
            profile: 1,
            sessions: 2,
            ..MgmtEraseCounts::default()
        };
        a.add(&MgmtEraseCounts {
            sessions: 3,
            assistants: 1,
            ..MgmtEraseCounts::default()
        });
        assert_eq!((a.profile, a.sessions, a.assistants), (1, 5, 1));
        let v = serde_json::to_value(MgmtEraseCounts::default()).expect("json");
        assert_eq!(v.as_object().map(|m| m.len()), Some(17));
        assert!(MgmtEraseCounts::default().is_zero());
    }
}
