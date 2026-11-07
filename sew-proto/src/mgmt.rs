//! Management API bodies and act-as headers.
//!
//! HTTP over the Unix socket `/run/sew/mgmt.sock` with `Bearer sewsvc_…`. `Idempotency-Key` is
//! required on every mgmt `POST`.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::addr::{Addr, Handle};
use crate::api::members::{AgentKind, MemberKind};
use crate::changes::NotificationReason;
use crate::error::{Validate, ValidationError, check_chars, check_known, check_opt_chars};
use crate::external_id::{ExternalId, is_lower_uuid};
use crate::http::Patch;
use crate::id::{Id, parse_id_str};
use crate::limits::{
    DELIVERIES_LONG_POLL_MAX_MS, DISPLAY_NAME_MAX_CHARS, EMAIL_MAX_BYTES, K2_SESSION_ID_MAX_LEN,
    PRONOUNS_MAX_CHARS, TITLE_MAX_CHARS,
};
use crate::time::Timestamp;

mod app_settings;
mod apps;
mod backups;
mod channels;
mod erase;
mod identity;
pub mod local;
mod meet;
mod member_channels;
mod notifications;
mod ops;
mod spaces;
mod voice;

pub use app_settings::{
    APP_ORIGIN_MAX_BYTES, APP_ORIGINS_MAX, FEATURE_APP_ORIGIN, FEATURE_APP_ORIGINS,
    FEATURE_APP_SCOPED_SESSIONS, FEATURE_HOME_CLIENT, MgmtAppSettings, MgmtAppSettingsRequest,
    MgmtHomeClient, MgmtHomeClientRequest, canonical_origins, default_max_level, is_app_origin,
};
pub use apps::{
    APP_QUOTA_COUNT_MAX, APP_QUOTA_FILE_BYTES_MAX, APP_QUOTA_MESSAGES_PER_MIN_MAX, APP_QUOTAS,
    AppQuotaDetails, AppQuotaUsage, AppQuotas, AppQuotasResponse, QUOTA_CALL_PARTICIPANTS_MAX,
    QUOTA_CHANNELS_PER_SPACE_MAX, QUOTA_FILE_BYTES_MAX, QUOTA_MEMBERS_PER_SPACE_MAX,
    QUOTA_MESSAGES_PER_MIN, QUOTA_SPACES_MAX,
};
pub use backups::{BACKUP_JOB_KIND, BackupJobResult, MgmtBackup, MgmtBackupList};
pub use channels::{
    CHANNEL_BULK_OPS_MAX, CHANNEL_MEMBERS_ADD_MAX, MgmtChannelBulkRequest, MgmtChannelBulkResponse,
    MgmtChannelCreateRequest, MgmtChannelCreateResponse, MgmtChannelNotAdded,
    MgmtChannelNotAddedReason, MgmtChannelOp, MgmtChannelOpKind, MgmtChannelOpResult,
    MgmtChannelTypeMismatchDetails, MgmtMemberRef,
};
pub use erase::{
    EraseKeptKind, EraseKeptReason, EraseMode, EraseState, FEATURE_PRINCIPAL_ERASE,
    MGMT_ERASURES_LIMIT_DEFAULT, MGMT_ERASURES_LIMIT_MAX, MgmtEraseAccepted, MgmtEraseCounts,
    MgmtEraseKept, MgmtEraseRequest, MgmtEraseResult, MgmtErasurePage, MgmtErasuresQuery,
    StatusErasures,
};
pub use identity::{
    FEATURE_PRINCIPAL_RELINK, IdentitySwitchRequest, IdentitySwitchResponse,
    PrincipalRelinkRequest, PrincipalRelinkResponse, is_relink_target,
};
pub use meet::{MEET_RETENTION_DAYS_MAX, MeetInitRequest, MeetInitResponse};
pub use member_channels::{
    FEATURE_APP_MEMBER_CHANNELS, MEMBER_CHANNELS_ITEMS_MAX, MEMBER_CHANNELS_MAX,
    MgmtMemberChannelsBulkRequest, MgmtMemberChannelsBulkResponse, MgmtMemberChannelsItem,
    MgmtMemberChannelsItemResult, MgmtMemberChannelsRequest, MgmtMemberChannelsResponse,
    MgmtMemberChannelsSkip,
};
pub use notifications::{
    CursorExpiredDetails, FEATURE_PERSON_NOTIFICATIONS, PERSON_NOTIFICATION_PREVIEW_MAX_CHARS,
    PERSON_NOTIFICATIONS_LIMIT_DEFAULT, PERSON_NOTIFICATIONS_LIMIT_MAX,
    PERSON_NOTIFICATIONS_WAIT_MAX_S, PersonNotification, PersonNotificationKind,
    PersonNotificationsPage, PersonNotificationsQuery, preview_of,
};
pub use ops::{
    AuditVerifyMode, AuditVerifyReport, AuditVerifyRequest, EXPORT_RECIPIENTS_MAX, ExportRequest,
};
pub use spaces::{
    ADMIN_SPACE_SLUG, APP_ID_MAX_BYTES, FEATURE_APP_GOVERNED_SPACES, FEATURE_APP_PARTICIPANT_LINKS,
    FEATURE_PRIVATE_SPACES, FEATURE_SERVER_GOVERNED_SPACE, FEATURE_SPACE_UNBIND,
    MGMT_APP_SPACES_LIMIT_DEFAULT, MGMT_APP_SPACES_LIMIT_MAX, MgmtAppSpace, MgmtAppSpaces,
    MgmtAppSpacesQuery, MgmtBoxUsersDetails, MgmtSpaceApp, MgmtSpaceAppLinkRequest,
    MgmtSpaceAppRole, MgmtSpaceApps, MgmtSpaceAppsChange, MgmtSpaceBindRequest,
    MgmtSpaceCreateRequest, MgmtSpaceMemberRef, MgmtSpaceMemberRemoveRequest,
    MgmtSpaceMemberRemoveResponse, MgmtSpaceMembersAddRequest, MgmtSpaceMembersAddResponse,
    MgmtSpaceNotAdded, MgmtSpaceNotAddedReason, MgmtSpaceOrphanResponse, MgmtSpaceReleaseRequest,
    MgmtSpaceResponse, MgmtSpaceSettings, MgmtSpaceSettingsRequest, MgmtSpaceUnbindRequest,
    SPACE_MANAGER_ROLE_NAME, SPACE_MANAGERS_MAX, SPACE_MEMBERS_ADD_MAX, is_app_id,
};
pub use voice::{
    FEATURE_MGMT_VOICE_STATES, FEATURE_VOICE_CHANNEL_CLOSING, FEATURE_VOICE_MEDIA_PATH,
    MEDIA_PATH_REASON_MAX_CHARS, MediaPathState, MgmtChannelClosingRequest,
    MgmtChannelClosingResponse, MgmtMediaPathRequest, MgmtMediaPathResponse, MgmtVoiceChange,
    MgmtVoiceChangeKind, MgmtVoiceChanges, MgmtVoiceChangesQuery, MgmtVoiceCursorExpiredDetails,
    MgmtVoiceState, MgmtVoiceStates, VOICE_CHANGES_LIMIT_DEFAULT, VOICE_CHANGES_LIMIT_MAX,
    VOICE_CHANGES_RING, VOICE_CHANGES_WAIT_MAX_S, VoiceMediaPathStatus,
};

/// Since Sew 0.7.1: the call-scoped credential. K2's backend asks for one
/// (`call_credential: true`) only when this flag is listed.
pub const FEATURE_CALL_CREDENTIAL: &str = "call_credential";
/// Since Sew 0.7.1: the call-stats beacon takes client `native` and the audio fields
/// (`stats.audio_fields` in join answers).
pub const FEATURE_CALL_STATS_NATIVE: &str = "call_stats_native";

// ---- act-as ----

/// The `Sew-Act-As` header: a member id (decimal) or an external id (contains `:`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ActAsTarget {
    /// A member id.
    Member(Id),
    /// An external id.
    External(ExternalId),
}

impl FromStr for ActAsTarget {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.contains(':') {
            return s
                .parse()
                .map(ActAsTarget::External)
                .map_err(|e: crate::external_id::ExternalIdError| e.to_string());
        }
        parse_id_str(s)
            .map(ActAsTarget::Member)
            .ok_or_else(|| format!("invalid Sew-Act-As {s:?}: expected a member id or external id"))
    }
}

impl fmt::Display for ActAsTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActAsTarget::Member(id) => fmt::Display::fmt(id, f),
            ActAsTarget::External(e) => fmt::Display::fmt(e, f),
        }
    }
}

/// Whether `s` is 1–128 chars of `[A-Za-z0-9_.-]` (a K2 session id).
pub fn is_k2_session_id(s: &str) -> bool {
    (1..=K2_SESSION_ID_MAX_LEN).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'-')
}

/// The `Sew-Via` header, exactly `k2; agent=<uuid>; k2_session=<id>`. Only
/// `system = k2` exists in v1.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SewVia {
    /// The agent's K2 workspace uuid.
    pub agent: String,
    /// The K2 session id, 1–128 chars of `[A-Za-z0-9_.-]`.
    pub k2_session: String,
}

impl FromStr for SewVia {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || format!("invalid Sew-Via {s:?}: expected k2; agent=<uuid>; k2_session=<id>");
        let rest = s.strip_prefix("k2; agent=").ok_or_else(err)?;
        let (agent, session) = rest.split_once("; k2_session=").ok_or_else(err)?;
        if !is_lower_uuid(agent) || !is_k2_session_id(session) {
            return Err(err());
        }
        Ok(SewVia {
            agent: agent.to_owned(),
            k2_session: session.to_owned(),
        })
    }
}

impl fmt::Display for SewVia {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "k2; agent={}; k2_session={}",
            self.agent, self.k2_session
        )
    }
}

// ---- health and status ----

/// Response of `GET /mgmt/v1/health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Health {
    /// Whether the process is live.
    pub ok: bool,
}

unit_enum! {
    /// The install's identity provider.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum ProviderKind {
        /// K2 grants create accounts; K2 signs the pass.
        K2 = "k2",
        /// Local users and passwords.
        Standalone = "standalone",
        /// A meeting-link session (session provider 3, made by redeeming a
        /// meeting link). Never an install's provider.
        Link = "link",
    }
}

/// Database status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusDb {
    /// Whether the database answers (`SELECT 1` within 1 s).
    pub ok: bool,
    /// Replica lag; absent in v1 (single node).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag_ms: Option<u64>,
    /// Round trip of the `SELECT 1` probe; absent when it failed (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ping_ms: Option<u64>,
}

/// Job queue status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusJobs {
    /// Jobs waiting (not under a live lease).
    pub queued: u64,
    /// Jobs that exhausted their attempts.
    pub dead: u64,
    /// Jobs under a live lease (additive).
    #[serde(default)]
    pub running: u64,
    /// `now − min(run_at)` over due, unleased jobs; absent if none (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_due_age_ms: Option<u64>,
}

unit_enum! {
    /// A status alarm. K2 shows each one verbatim.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    #[non_exhaustive]
    pub enum StatusAlarm {
        /// `db.ok = false`.
        DbDown = "db_down",
        /// `jobs.dead > 0`.
        JobsDead = "jobs_dead",
        /// `keys.provider_ok = false`.
        KeysProviderDown = "keys_provider_down",
        /// `keys.current_month_ready = false`.
        KeysCurrentMonthMissing = "keys_current_month_missing",
        /// The keyring mirror is stale (or kind `none`).
        KeysMirrorStale = "keys_mirror_stale",
        /// A crypto-shred started but did not finish.
        KeysShredPending = "keys_shred_pending",
        /// Old key files for shredded key ids were found and refused.
        KeysResurrected = "keys_resurrected",
        /// Keys wait for a re-wrap but no retiring KEK is loaded.
        KeysRewrapStalled = "keys_rewrap_stalled",
        /// Search indexing trails writes by more than 60 s.
        SearchLagging = "search_lagging",
        /// The search index is missing and being rebuilt; searches answer 503.
        SearchUnavailable = "search_unavailable",
        /// Three search index commits in a row failed.
        SearchCommitFailing = "search_commit_failing",
        /// Free disk under the search index is below its size plus 1 GiB.
        SearchDiskLow = "search_disk_low",
        /// The last good backup is older than 36 h (set by the backup engine).
        BackupStale = "backup_stale",
        /// The last backup failed (set by the backup engine).
        BackupFailed = "backup_failed",
        /// The last restore drill failed (set by the backup engine).
        DrillFailed = "drill_failed",
        /// The last good restore drill is older than 8 days (set by the backup engine).
        DrillStale = "drill_stale",
        /// GIF search is on but its provider key does not resolve or was refused.
        GifsUnconfigured = "gifs_unconfigured",
        /// A finite retention policy is set and no run finished `done` for 48 h.
        RetentionOverdue = "retention_overdue",
        /// Purged messages' search documents are still waiting after 24 h.
        RetentionSearchBacklog = "retention_search_backlog",
        /// Three or more mail sends in a row failed over at least 10 minutes.
        MailFailing = "mail_failing",
        /// `digest.enabled` is on but `[mail.smtp]` is not configured.
        DigestMailUnconfigured = "digest_mail_unconfigured",
        /// A running export wrote no progress for 10 minutes.
        ExportStuck = "export_stuck",
        /// A running account erasure made no progress for 10 minutes (its job is lost, overdue
        /// or has failed for that long; a wait for an active recording is not stuck).
        EraseStuck = "erase_stuck",
        /// Voice is enabled and the media engine has been unreachable for more than 60 s.
        VoiceMediaDown = "voice_media_down",
        /// The snapshot audit found voice state Sew did not expect.
        VoiceDrift = "voice_drift",
        /// The media engine reported a panic in the last hour.
        MediaPanic = "media_panic",
        /// More than 3 call key group resets install-wide in the last hour, or a call in reset
        /// back-off.
        E2eeGroupResets = "e2ee_group_resets",
        /// A key removal was not activated by its hard bound in the last hour.
        E2eeRemovalOverdue = "e2ee_removal_overdue",
        /// A key group rebuild ended in a new group or a reset in the last hour.
        E2eeRebuildFailed = "e2ee_rebuild_failed",
        /// A client reported a roster mismatch in the last hour.
        E2eeRosterMismatch = "e2ee_roster_mismatch",
        /// A stale-slot resync or a `peer_undecryptable` quorum in the last hour.
        E2eeSenderStale = "e2ee_sender_stale",
        /// A client's bridge reported a misbehaving engine in the last hour.
        E2eeEngineAnomaly = "e2ee_engine_anomaly",
        /// Voice is enabled and the call DS or CA key failed to load.
        E2eeKeysUnavailable = "e2ee_keys_unavailable",
        /// The media budget tier has been `critical` for more than 5 min;
        /// clears when the tier drops.
        MediaBudgetCritical = "media_budget_critical",
        /// The recorder failed health for 60 s.
        RecorderDown = "recorder_down",
        /// The transcriber failed health for 60 s.
        TranscriberDown = "transcriber_down",
        /// The oldest untranscribed segment is older than 30 min.
        TranscribeBacklog = "transcribe_backlog",
        /// A recording stopped on `storage_full` in the last 24 h.
        RecordingStorageFull = "recording_storage_full",
        /// Three auto-starts failed in 10 min for one channel.
        RecordingAutoFailed = "recording_auto_failed",
        /// A segment of an ungranted identity reached `sew` in the last 24 h.
        RecordingConsentViolation = "recording_consent_violation",
    }
}

/// Identity provider state in `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusIdentity {
    /// The provider.
    pub provider: ProviderKind,
    /// `identity.k2.jwks_url` (K2 only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks_url: Option<String>,
    /// Whether the JWKS cache holds keys (K2 only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks_ok: Option<bool>,
    /// The install's stored identity mode. It differs from `provider` only between
    /// `POST /mgmt/v1/identity/switch` and the restart that follows (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ProviderKind>,
    /// When the install switched from standalone to K2 (absent when it never did).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switched_at: Option<Timestamp>,
    /// Active members still on a `local:` id. On a switched install they keep password
    /// sign-in until K2 relinks or deactivates them (the transition state); absent on an
    /// install that never switched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlinked_local_members: Option<u64>,
    /// The mode was switched and this process still serves the old one; restart Sew.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restart_required: bool,
}

/// Backup state in `status`. The backup engine fills it; absent until then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusBackup {
    /// The last good backup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_at: Option<Timestamp>,
    /// The last good backup's bundle id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_id: Option<String>,
    /// Age of the last good backup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
    /// Why the last run failed, if it did (cleared by the next good one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The last good backup's kind (`scheduled`, `manual`, …; additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_kind: Option<String>,
    /// When the last failed run failed (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_at: Option<Timestamp>,
}

/// Restore-drill state in `status`. The backup engine fills it; absent until then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusDrill {
    /// The last good drill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_at: Option<Timestamp>,
    /// Whether the last drill passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok: Option<bool>,
    /// Why the last drill failed, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Age of the last good drill (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
}

/// Search index status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusSearch {
    /// How far indexing trails writes: `now − min(created_at)` over pending or running
    /// `search_index` / `search_delete` jobs; 0 when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag_ms: Option<u64>,
    /// `ready` or `missing` (rebuilding; searches answer 503).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<SearchState>,
    /// Live documents in the index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs: Option<StatusSearchDocs>,
    /// Bytes of the live index on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_bytes: Option<u64>,
    /// The live index generation (`gen-<n>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// When the indexer last committed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_commit_at: Option<Timestamp>,
    /// Present only while a rebuild runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebuild: Option<StatusSearchRebuild>,
}

unit_enum! {
    /// Search index state.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum SearchState {
        /// Queries use the live index.
        Ready = "ready",
        /// No usable index; a rebuild runs and searches answer `503 search_rebuilding`.
        Missing = "missing",
    }
}

/// Live search documents by type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusSearchDocs {
    /// Message documents.
    pub messages: u64,
    /// File-name documents.
    pub files: u64,
}

/// A running search rebuild.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusSearchRebuild {
    /// Why: `marker`, `missing`, `corrupt`, `meta_mismatch` or `admin`.
    pub reason: String,
    /// The generation being built.
    pub generation: u64,
    /// 0–99 until the swap.
    pub progress_pct: u8,
    /// Documents in the building index so far.
    pub docs: u64,
    /// When it started.
    pub started_at: Timestamp,
}

/// Body of `POST /mgmt/v1/search/rebuild`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchRebuildRequest {
    /// Free text for the operator, at most 200 bytes (never audited as content).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Validate for SearchRebuildRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(r) = &self.reason
            && r.len() > crate::limits::SEARCH_REBUILD_REASON_MAX_BYTES
        {
            return Err(ValidationError::invalid(
                "reason",
                format!(
                    "reason must be at most {} bytes",
                    crate::limits::SEARCH_REBUILD_REASON_MAX_BYTES
                ),
            ));
        }
        Ok(())
    }
}

/// Response of `POST /mgmt/v1/search/rebuild`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchRebuildResponse {
    /// The `search_rebuild` job (poll `GET /mgmt/v1/jobs/{id}`).
    pub job_id: Id,
}

/// Key provider and keyring status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusKeys {
    /// Whether the master key provider works.
    pub provider_ok: bool,
    /// Whether this month's keys exist.
    pub current_month_ready: bool,
    /// Whether the off-box keyring mirror is current.
    pub mirror_ok: bool,
    /// Age of the last good mirror write; absent if never ok.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror_age_ms: Option<u64>,
    /// The key provider kind: `systemd-credential`, `file` (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Current KEK id, 16 hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kek_id: Option<String>,
    /// Retiring KEK ids, 16 hex each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retiring_kek_ids: Vec<String>,
    /// Keyring entries still wrapped under a retiring KEK.
    #[serde(default)]
    pub rewrap_pending: u64,
    /// Whether next month's install key exists.
    #[serde(default)]
    pub next_month_prepared: bool,
    /// Keys created lazily at write time this month.
    #[serde(default)]
    pub lazy_creates_this_month: u64,
    /// Shreds started but not finished.
    #[serde(default)]
    pub shred_pending: u64,
    /// Old key files for shredded kids, refused at open.
    #[serde(default)]
    pub resurrected_keys: u64,
    /// The keyring mirror block; absent from servers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<StatusKeysMirror>,
}

/// `status.keys.mirror`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusKeysMirror {
    /// Mirror kind: `dir`, `none` (`k2` reserved).
    pub kind: String,
    /// Alarm: a pending write is older than the stale threshold, or kind `none`.
    pub stale: bool,
    /// Names waiting to be pushed.
    pub pending: u64,
    /// Age of the oldest pending name, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_pending_age_s: Option<u64>,
    /// Last successful mirror operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_at: Option<Timestamp>,
    /// Mirror entries that are neither local nor on the ledger.
    pub unknown_keys: u64,
}

/// Response of `GET /mgmt/v1/status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Status {
    /// The Sew version.
    pub version: String,
    /// The applied schema version.
    pub schema_version: u64,
    /// The install id.
    pub install_id: Id,
    /// The identity provider.
    pub provider: ProviderKind,
    /// Database status.
    pub db: StatusDb,
    /// Job queue status.
    pub jobs: StatusJobs,
    /// Search status.
    pub search: StatusSearch,
    /// Key status.
    pub keys: StatusKeys,
    /// Live login sessions.
    pub sessions: u64,
    /// Open gateway sockets.
    pub sockets: u64,
    /// When this process started (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    /// Time since this process started (additive).
    #[serde(default)]
    pub uptime_ms: u64,
    /// Every active alarm, sorted (additive). K2 shows them verbatim.
    #[serde(default)]
    pub alarms: Vec<StatusAlarm>,
    /// The identity provider's state (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<StatusIdentity>,
    /// The client listener as `http://<bound address>` (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_listen: Option<String>,
    /// Backups (the backup engine fills it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<StatusBackup>,
    /// Restore drills (the backup engine fills it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drill: Option<StatusDrill>,
    /// Files (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<StatusFiles>,
    /// Retention runs, pending policies, holds and shred candidates (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<crate::api::compliance::StatusRetention>,
    /// Outgoing mail (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mail: Option<StatusMail>,
    /// The email digest (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<StatusDigest>,
    /// Exports running, queued and on disk (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exports: Option<crate::api::exports::StatusExports>,
    /// Account erasures running and waiting (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub erasures: Option<StatusErasures>,
    /// Voice, the media engine and call keys (additive). Absent while
    /// voice was never configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<crate::voice::VoiceStatus>,
    /// Recording and transcription (additive). Absent while recording is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<crate::recording::StatusRecording>,
    /// The features this build has, sorted ([`FEATURES`]; additive). K2 tests a feature, not a
    /// version. A flag is never removed or renamed once shipped.
    #[serde(default)]
    pub features: Vec<String>,
    /// The gateway's counters since this process started (0.7.1, additive): IDENTIFY and RESUME
    /// outcomes and server-sent closes by code. Absent from servers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<StatusGateway>,
}

/// `status.gateway` (0.7.1): the gateway's counters since this process started, the same
/// numbers as the `sew_gateway_*` metric families.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusGateway {
    /// Sockets with READY or RESUMED queued.
    pub sockets_live: u64,
    /// Upgraded sockets not live yet (HELLO sent, no READY).
    pub sockets_pending: u64,
    /// IDENTIFY outcomes.
    pub identify: StatusGatewayIdentify,
    /// RESUME outcomes.
    pub resume: StatusGatewayResume,
    /// REAUTH outcomes (0.7.2, additive; zeros when an older server left it out).
    #[serde(default)]
    pub reauth: StatusGatewayReauth,
    /// Server-sent closes by code (`"4000"`..`"4014"`, and `"stall"` for a writer that gave
    /// up); every code is listed, zeros included. Clients must accept a key they do not know.
    pub closes: std::collections::BTreeMap<String, u64>,
}

/// `status.gateway.identify`: IDENTIFY outcomes since start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusGatewayIdentify {
    /// READY sent.
    pub ok: u64,
    /// Closed `4008` (the install or the per-login limit).
    pub rate_limited: u64,
    /// Closed `4004`.
    pub auth_failed: u64,
    /// Closed `4003 identify_timeout`: no IDENTIFY in time, or its ticket or token was not
    /// redeemed in time; or the READY build timed out (`4000`).
    pub timeout: u64,
    /// Closed `4000` for any other reason.
    pub error: u64,
}

/// `status.gateway.resume`: RESUME outcomes since start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusGatewayResume {
    /// RESUMED sent.
    pub resumed: u64,
    /// INVALID_SESSION sent.
    pub invalid: u64,
    /// Closed `4007`.
    pub bad_seq: u64,
    /// Closed `4008`.
    pub rate_limited: u64,
    /// Closed `4004`.
    pub auth_failed: u64,
    /// Closed `4003 identify_timeout`: RESUME's ticket or token was not redeemed in time.
    pub timeout: u64,
}

/// `status.gateway.reauth` (0.7.2): REAUTH outcomes since start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusGatewayReauth {
    /// The socket's login was swapped.
    pub ok: u64,
    /// Closed `4004`: the token was refused, or names another member or credential.
    pub auth_failed: u64,
    /// Closed `4003 identify_timeout`: REAUTH's token was not redeemed within
    /// `identify_timeout_ms` of the REAUTH (the session left resumable).
    pub timeout: u64,
}

/// Every feature flag of this build, sorted: `features` in `GET /mgmt/v1/status` and
/// `sew version --json`. A flag is never removed or renamed once shipped.
pub const FEATURES: &[&str] = &[
    // `POST /mgmt/v1/channels/create`.
    "app_channel_create",
    "app_controls", // channels/bulk, sessions/revoke app_id, uploads off.
    // Since 0.6.0: App-governed Spaces (`app:` ids, the governance switch, the links, the App
    // levels and uploads settings).
    FEATURE_APP_GOVERNED_SPACES,
    FEATURE_APP_MEMBER_CHANNELS, // The member-channels sync.
    "app_mode_gate", // App mode's `governed_by_app` gate, `role_managed` and masked perms.
    // `origin` in the App settings is the join answers' signalling URL.
    FEATURE_APP_ORIGIN,
    // Since 0.7.2: `origins` in the App settings; a voice or call join may name its `origin`.
    FEATURE_APP_ORIGINS,
    // Since 0.8.0: participant links (`spaces/{space_id}/apps/link`, `participant_app_ids`).
    FEATURE_APP_PARTICIPANT_LINKS,
    // `GET`/`POST /mgmt/v1/apps/{app_id}/quotas` and their enforcement.
    "app_quotas",
    // The pass's `app` claim scopes the session.
    FEATURE_APP_SCOPED_SESSIONS,
    "app_session_member_role", // An App session acts with install role at most `member`.
    // `type: voice` and `user_limit` on `POST /mgmt/v1/channels/create`.
    "app_voice_channel_create",
    // Since 0.7.1: the call-scoped credential (`call_credential: true` on voice and DM
    // call joins, `sewcc_`, the call socket, `POST /api/v1/me/voice/credential`).
    FEATURE_CALL_CREDENTIAL,
    // Since 0.7.1: the beacon's client `native` and audio fields (`stats.audio_fields`).
    FEATURE_CALL_STATS_NATIVE,
    // `/mgmt/v1/home-client` and `hidden_from_home`.
    FEATURE_HOME_CLIENT,
    "home_client_owner_approval", // `POST /mgmt/v1/home-client` needs the owner's approval.
    // `sew import --to-standalone [--identity-map]` (moving off K2).
    "import_identity_map",
    // Policy `identity.managed_fields` (an avatar K2 owns: client uploads refused).
    "k2_managed_avatar",
    "k2_managed_guard", // `--i-am-k2` and `sew install --managed-by` (the K2-managed guard).
    // `external_id` on the client `Member` (K2 installs, shared Spaces and DMs).
    crate::api::members::FEATURE_MEMBER_EXTERNAL_ID,
    // `voice/states` and the `voice/changes` feed over mgmt.
    FEATURE_MGMT_VOICE_STATES,
    // `GET /mgmt/v1/people/{external_id}/notifications`.
    FEATURE_PERSON_NOTIFICATIONS,
    // `principals/erase`, its status and list routes, and the `erased:` tombstone id.
    FEATURE_PRINCIPAL_ERASE,
    // `principals/relink`, `identity/switch` and the transition state.
    FEATURE_PRINCIPAL_RELINK,
    // Private Spaces (`Space.private`, `/mgmt/v1/spaces/{space_id}/settings`).
    FEATURE_PRIVATE_SPACES,
    // The server-governed Space (governance `server`, the `k2:server` owner).
    FEATURE_SERVER_GOVERNED_SPACE,
    // `spaces/unbind`, `spaces/release` and read-only orphan Spaces.
    FEATURE_SPACE_UNBIND,
    // Relay-only through the edge (`voice.ops.ice_relay_only`,
    // `MediaPolicy.relay_only`, no node IP in tunnel mode).
    "turn_relay_only",
    // Closing a voice room (`channels/{channel_id}/closing`, `Channel.closing`).
    FEATURE_VOICE_CHANNEL_CLOSING,
    // `POST /mgmt/v1/voice/media-path`, `status.voice.media_path`.
    FEATURE_VOICE_MEDIA_PATH,
    // The pass cap `voice:video` (camera and screen leases).
    "voice_video_cap",
];

/// [`FEATURES`] as owned strings.
pub fn features() -> Vec<String> {
    FEATURES.iter().map(|f| (*f).to_owned()).collect()
}

unit_enum! {
    /// How the SMTP connection is secured (`[mail.smtp] tls`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum MailTls {
        /// Plain SMTP; loopback relays only.
        None = "none",
        /// STARTTLS, required.
        Starttls = "starttls",
        /// Implicit TLS.
        Tls = "tls",
    }
}

/// Outgoing mail in `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusMail {
    /// `[mail.smtp]` is set.
    pub configured: bool,
    /// The connection security (absent when not configured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<MailTls>,
    /// The last accepted send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_at: Option<Timestamp>,
    /// The last failure (at most 200 characters, never an address).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// When the last failure happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_at: Option<Timestamp>,
    /// Sends accepted in the last 24 h.
    pub sent_24h: u64,
    /// Sends that failed in the last 24 h.
    pub failed_24h: u64,
}

/// The email digest in `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusDigest {
    /// Org policy `digest.enabled`.
    pub enabled: bool,
    /// Queued `email_digest` jobs.
    pub pending_jobs: u64,
}

/// Body of `POST /mgmt/v1/mail/test`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailTestRequest {
    /// The recipient (never audited).
    pub to: String,
}

impl Validate for MailTestRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        check_email("to", &self.to)
    }
}

/// Response of `POST /mgmt/v1/mail/test`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MailTestResponse {
    /// The relay accepted the mail.
    pub ok: bool,
    /// The relay's reply code, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp_code: Option<u16>,
    /// Why it failed (at most 200 characters, never the address).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// File state in `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusFiles {
    /// Whether `ffmpeg` answered `-version` at boot (video posters need it).
    pub ffmpeg_available: bool,
}

// ---- provisioning ----

fn check_email(field: &str, email: &str) -> Result<(), ValidationError> {
    if email.is_empty() || email.len() > EMAIL_MAX_BYTES || !email.contains('@') {
        return Err(ValidationError::invalid(
            field,
            format!("{field} must be an address of at most {EMAIL_MAX_BYTES} bytes"),
        ));
    }
    Ok(())
}

/// A profile at provisioning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileInput {
    /// Display name, 1–100 characters.
    pub display_name: String,
    /// Handle.
    pub username: Handle,
    /// Email address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Title, at most 100 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Pronouns, at most 40 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pronouns: Option<String>,
    /// Provider custom fields: declarative, unlisted provider fields are cleared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_fields: Vec<crate::api::people::MgmtCustomFieldInput>,
}

impl Validate for ProfileInput {
    fn validate(&self) -> Result<(), ValidationError> {
        check_chars(
            "display_name",
            &self.display_name,
            1,
            DISPLAY_NAME_MAX_CHARS,
        )?;
        if let Some(e) = &self.email {
            check_email("email", e)?;
        }
        check_opt_chars("title", self.title.as_deref(), 0, TITLE_MAX_CHARS)?;
        check_opt_chars("pronouns", self.pronouns.as_deref(), 0, PRONOUNS_MAX_CHARS)
    }
}

/// Body of `POST /mgmt/v1/principals/provision`. Idempotent by `external_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProvisionRequest {
    /// Who the member is outside Sew.
    pub external_id: ExternalId,
    /// Person or agent.
    pub kind: MemberKind,
    /// Required iff `kind = agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_kind: Option<AgentKind>,
    /// The assistant's person; required iff `agent_kind = assistant`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_of: Option<ExternalId>,
    /// Provision as a guest; only for persons.
    #[serde(default)]
    pub guest: bool,
    /// The profile.
    pub profile: ProfileInput,
}

impl Validate for ProvisionRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        check_known("kind", self.kind)?;
        if let Some(k) = self.agent_kind {
            check_known("agent_kind", k)?;
        }
        match self.kind {
            MemberKind::Agent => {
                let Some(agent_kind) = self.agent_kind else {
                    return Err(ValidationError::invalid(
                        "agent_kind",
                        "agent_kind is required for agents",
                    ));
                };
                if !self.external_id.is_agent() {
                    return Err(ValidationError::invalid(
                        "external_id",
                        "an agent's external_id must be k2:workspace:<uuid>",
                    ));
                }
                if self.guest {
                    return Err(ValidationError::invalid("guest", "agents cannot be guests"));
                }
                match (agent_kind, &self.assistant_of) {
                    (AgentKind::Assistant, None) => {
                        return Err(ValidationError::invalid(
                            "assistant_of",
                            "assistant_of is required for assistants",
                        ));
                    }
                    (AgentKind::Coworker, Some(_)) => {
                        return Err(ValidationError::invalid(
                            "assistant_of",
                            "assistant_of is only for assistants",
                        ));
                    }
                    _ => {}
                }
                if let Some(person) = &self.assistant_of
                    && person.is_agent()
                {
                    return Err(ValidationError::invalid(
                        "assistant_of",
                        "an assistant belongs to a person",
                    ));
                }
            }
            _ => {
                if self.agent_kind.is_some() {
                    return Err(ValidationError::invalid(
                        "agent_kind",
                        "agent_kind is only for agents",
                    ));
                }
                if self.assistant_of.is_some() {
                    return Err(ValidationError::invalid(
                        "assistant_of",
                        "assistant_of is only for assistants",
                    ));
                }
                if self.external_id.is_agent() {
                    return Err(ValidationError::invalid(
                        "external_id",
                        "a person's external_id must not be k2:workspace:*",
                    ));
                }
            }
        }
        self.profile.validate().map_err(|e| e.nested("profile"))
    }
}

/// Response of `POST /mgmt/v1/principals/provision`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProvisionResponse {
    /// The member id.
    pub member_id: Id,
    /// Whether this call created the member.
    pub created: bool,
    /// Whether this call reactivated a deactivated member (additive).
    #[serde(default)]
    pub reactivated: bool,
    /// Whether an existing member's profile, guest flag, agent kind or pin changed
    /// (additive).
    #[serde(default)]
    pub changed: bool,
}

/// A partial profile update from K2.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfilePatch {
    /// A new display name, 1–100 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// A new handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<Handle>,
    /// Set or clear the email.
    #[serde(default, skip_serializing_if = "Patch::is_keep")]
    pub email: Patch<String>,
    /// Set or clear the title (≤ 100).
    #[serde(default, skip_serializing_if = "Patch::is_keep")]
    pub title: Patch<String>,
    /// Set or clear the pronouns (≤ 40).
    #[serde(default, skip_serializing_if = "Patch::is_keep")]
    pub pronouns: Patch<String>,
    /// Provider custom fields to set or clear; unlisted keys are kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_fields: Vec<crate::api::people::MgmtCustomFieldInput>,
}

impl Validate for ProfilePatch {
    fn validate(&self) -> Result<(), ValidationError> {
        check_opt_chars(
            "display_name",
            self.display_name.as_deref(),
            1,
            DISPLAY_NAME_MAX_CHARS,
        )?;
        if let Some(e) = self.email.as_set() {
            check_email("email", e)?;
        }
        check_opt_chars(
            "title",
            self.title.as_set().map(String::as_str),
            0,
            TITLE_MAX_CHARS,
        )?;
        check_opt_chars(
            "pronouns",
            self.pronouns.as_set().map(String::as_str),
            0,
            PRONOUNS_MAX_CHARS,
        )
    }
}

/// Body of `POST /mgmt/v1/principals/update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatePrincipalRequest {
    /// Who to update.
    pub external_id: ExternalId,
    /// What to change.
    pub profile: ProfilePatch,
}

impl Validate for UpdatePrincipalRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        not_erased("external_id", &self.external_id)?;
        self.profile.validate().map_err(|e| e.nested("profile"))
    }
}

/// Refuses an erased member's tombstone id (`erased:<member_id>`) as a mgmt input: it is
/// Sew's own name for an erased account, never an id a caller addresses.
fn not_erased(field: &str, e: &ExternalId) -> Result<(), ValidationError> {
    if e.is_erased() {
        return Err(ValidationError::invalid(
            field,
            "erased:<member_id> names an erased account and is never a mgmt input",
        ));
    }
    Ok(())
}

/// Body of `POST /mgmt/v1/principals/deprovision`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeprovisionRequest {
    /// Who to deactivate.
    pub external_id: ExternalId,
}

impl Validate for DeprovisionRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        not_erased("external_id", &self.external_id)
    }
}

/// Response of `POST /mgmt/v1/principals/deprovision`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeprovisionResponse {
    /// The deactivated member.
    pub member_id: Id,
    /// Sessions revoked.
    pub sessions_revoked: u64,
}

/// Body of `POST /mgmt/v1/sessions/revoke`: at least one of `external_id`, `k2_sid` and
/// `app_id`. `app_id` goes alone or with `external_id`, never with `k2_sid`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RevokeSessionsRequest {
    /// Revoke every session of this principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<ExternalId>,
    /// Revoke sessions bound to this K2 login session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k2_sid: Option<String>,
    /// Revoke every session whose pass `app` claim is this App (with `external_id`:
    /// only that principal's). K2 sends it when it revokes an App's key or uninstalls the App,
    /// never on a key rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
}

impl Validate for RevokeSessionsRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(e) = &self.external_id {
            not_erased("external_id", e)?;
        }
        if self.external_id.is_none() && self.k2_sid.is_none() && self.app_id.is_none() {
            return Err(ValidationError::invalid(
                "external_id",
                "send external_id, k2_sid, app_id, or external_id with one of the other two",
            ));
        }
        if let Some(app) = &self.app_id {
            spaces::check_app_id("app_id", app)?;
            if self.k2_sid.is_some() {
                return Err(ValidationError::invalid(
                    "app_id",
                    "app_id goes alone or with external_id, never with k2_sid",
                ));
            }
        }
        if let Some(sid) = &self.k2_sid
            && !is_k2_session_id(sid)
        {
            return Err(ValidationError::invalid(
                "k2_sid",
                "k2_sid must be 1–128 chars of [A-Za-z0-9_.-]",
            ));
        }
        Ok(())
    }
}

/// Response of `POST /mgmt/v1/sessions/revoke`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RevokeSessionsResponse {
    /// Sessions revoked.
    pub revoked: u64,
}

/// The current owner's inline approval of an owner transfer. `Debug` redacts the
/// secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerApproval {
    /// K2 mode: a chat pass whose `sub` is the current owner.
    OwnerPass {
        /// The pass.
        pass: String,
    },
    /// Standalone: the current owner's password.
    OwnerPassword {
        /// The password.
        password: String,
    },
}

impl OwnerApproval {
    /// The wire `kind`.
    pub fn kind_str(&self) -> &'static str {
        match self {
            OwnerApproval::OwnerPass { .. } => "owner_pass",
            OwnerApproval::OwnerPassword { .. } => "owner_password",
        }
    }
}

impl fmt::Debug for OwnerApproval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OwnerApproval::OwnerPass { .. } => f
                .debug_struct("OwnerPass")
                .field("pass", &"<redacted>")
                .finish(),
            OwnerApproval::OwnerPassword { .. } => f
                .debug_struct("OwnerPassword")
                .field("password", &"<redacted>")
                .finish(),
        }
    }
}

/// Body of `POST /mgmt/v1/owner/set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OwnerSetRequest {
    /// The new install owner.
    pub external_id: ExternalId,
    /// The current owner's approval; not needed while no owner exists (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<OwnerApproval>,
    /// Break-glass transfer: standalone only, and only for peer uid 0 (additive).
    #[serde(default)]
    pub force: bool,
}

impl Validate for OwnerSetRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.external_id.is_agent() {
            return Err(ValidationError::invalid(
                "external_id",
                "the install owner must be a person",
            ));
        }
        match &self.approval {
            Some(OwnerApproval::OwnerPass { pass }) if pass.is_empty() || pass.len() > 4_096 => {
                Err(ValidationError::invalid(
                    "approval.pass",
                    "approval.pass must be 1–4096 bytes",
                ))
            }
            Some(OwnerApproval::OwnerPassword { password })
                if password.is_empty() || password.len() > crate::limits::PASSWORD_MAX_BYTES =>
            {
                Err(ValidationError::invalid(
                    "approval.password",
                    "approval.password must be 1–1024 bytes",
                ))
            }
            _ => Ok(()),
        }
    }
}

/// Response of `POST /mgmt/v1/owner/set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OwnerSetResponse {
    /// The install owner after the call.
    pub owner_id: Id,
    /// The owner before the call, now an admin; absent on the first call or when unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_owner_id: Option<Id>,
    /// Whether the owner changed.
    pub changed: bool,
}

/// Body of `POST /mgmt/v1/agents/pin` and `unpin` (external ids).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PinRequest {
    /// The assistant, `k2:workspace:<uuid>`.
    pub agent: ExternalId,
    /// The person.
    pub person: ExternalId,
}

impl Validate for PinRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if !self.agent.is_agent() {
            return Err(ValidationError::invalid(
                "agent",
                "agent must be k2:workspace:<uuid>",
            ));
        }
        if self.person.is_agent() {
            return Err(ValidationError::invalid(
                "person",
                "person must be a person",
            ));
        }
        Ok(())
    }
}

/// Response of `POST /mgmt/v1/agents/pin` and `unpin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PinResponse {
    /// The assistant.
    pub agent_id: Id,
    /// The person it is pinned to; absent after unpin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_id: Option<Id>,
    /// Whether the pin changed.
    pub changed: bool,
}

// ---- principals list ----

fn default_true() -> bool {
    true
}

fn is_true(v: &bool) -> bool {
    *v
}

/// Query of `GET /mgmt/v1/principals`: pages by member id,
/// ascending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrincipalsQuery {
    /// Members with an id above this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Id>,
    /// Page size, 1–500 (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Only persons or only agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MemberKind>,
    /// Only this external id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<ExternalId>,
    /// Only this handle (exact match).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<Handle>,
    /// Include deactivated members (default `true`).
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub include_deactivated: bool,
    /// Only compliance-flag holders (`true`) or only non-holders (`false`) (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compliance: Option<bool>,
    /// `link_guests` also lists meeting link guests (`link:*` members), which are left out by
    /// default so K2's reconcile never sees them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<PrincipalsInclude>,
}

unit_enum! {
    /// `GET /mgmt/v1/principals?include=`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum PrincipalsInclude {
        /// Meeting link guests (`link:*`, install role `link_guest`) too.
        LinkGuests = "link_guests",
    }
}

/// Body of `POST /mgmt/v1/principals/compliance`: grant or revoke the compliance
/// flag. Returns `ComplianceFlagResponse`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrincipalComplianceRequest {
    /// The member.
    pub external_id: ExternalId,
    /// Grant (`true`) or revoke (`false`).
    pub compliance: bool,
}

impl Validate for PrincipalComplianceRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}

/// Largest `GET /mgmt/v1/principals` page.
pub const PRINCIPALS_LIMIT_MAX: u32 = 500;
/// Default `GET /mgmt/v1/principals` page.
pub const PRINCIPALS_LIMIT_DEFAULT: u32 = 100;

impl PrincipalsQuery {
    /// The page size, default 100.
    pub fn limit(&self) -> u32 {
        self.limit.unwrap_or(PRINCIPALS_LIMIT_DEFAULT)
    }
}

impl Validate for PrincipalsQuery {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(l) = self.limit
            && !(1..=PRINCIPALS_LIMIT_MAX).contains(&l)
        {
            return Err(ValidationError::invalid(
                "limit",
                format!("limit must be 1–{PRINCIPALS_LIMIT_MAX}"),
            ));
        }
        if let Some(k) = self.kind {
            check_known("kind", k)?;
        }
        if let Some(i) = self.include {
            check_known("include", i)?;
        }
        Ok(())
    }
}

/// One principal in `GET /mgmt/v1/principals` (directory metadata only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtPrincipal {
    /// The member id.
    pub member_id: Id,
    /// Who the member is outside Sew.
    pub external_id: ExternalId,
    /// Person or agent.
    pub kind: MemberKind,
    /// For agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_kind: Option<AgentKind>,
    /// For pinned assistants, the person's external id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_of: Option<ExternalId>,
    /// Install role.
    pub install_role: crate::api::members::InstallRole,
    /// The handle.
    pub username: Handle,
    /// The display name.
    pub display_name: String,
    /// The email, as provisioned (absent when unset; additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// The title, as provisioned (absent when unset; additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The pronouns, as provisioned (absent when unset; additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pronouns: Option<String>,
    /// Whether the member is deactivated.
    pub deactivated: bool,
    /// When the member was created.
    pub created_at: Timestamp,
    /// When the member was deactivated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deactivated_at: Option<Timestamp>,
}

/// Response of `GET /mgmt/v1/principals`.
pub type MgmtPrincipalPage = crate::http::Page<MgmtPrincipal>;

// ---- act-as and mgmt reasons ----

/// Whether `s` is a valid `Sew-Actor` header: 1–128 chars of `[A-Za-z0-9_.:@-]`.
/// It names the K2 person behind a mgmt call, for audit only.
pub fn is_sew_actor(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.bytes().all(|c| {
            c.is_ascii_alphanumeric()
                || c == b'_'
                || c == b'.'
                || c == b':'
                || c == b'@'
                || c == b'-'
        })
}

/// `details.reason` values of mgmt and act-as errors.
/// Stable strings; no new error codes.
pub mod reasons {
    /// The sync names no provisioned member (422 `invalid_state`; nothing changes).
    pub const MEMBER_NOT_PROVISIONED: &str = "member_not_provisioned";
    /// Box users (`k2:owner`, `k2:connect:`) are never synced (422 `invalid_state`).
    pub const BOX_USER_NOT_ALLOWED: &str = "box_user_not_allowed";
    /// An agent is synced at level `member` only (422 `invalid_state`).
    pub const AGENT_LEVEL: &str = "agent_level";
    /// A level above the App's `max_level` (422 `invalid_state`).
    pub const LEVEL_ABOVE_CEILING: &str = "level_above_ceiling";
    /// The sync runs only in an App-governed Space (409). Never
    /// answered for the server-governed Space (governance `server`).
    pub const NOT_APP_GOVERNED: &str = "not_app_governed";
    /// App users (`k2:principal:`) and App service owners (`app:`) are never
    /// members of the server-governed Space (422 `invalid_state`; the member-channels sync and
    /// `spaces/members/add`).
    pub const APP_USER_NOT_ALLOWED: &str = "app_user_not_allowed";
    /// The install has its server-governed Space already, under another slug (409;
    /// `details.space_id` names it).
    pub const SERVER_SPACE_EXISTS: &str = "server_space_exists";
    /// `governance` never changes to or from `server`, and the server-governed Space is
    /// never bound to an App (409).
    pub const SERVER_GOVERNANCE_FIXED: &str = "server_governance_fixed";
    /// The slug `k5-admin` belongs to the server-governed Space (400
    /// `validation_failed`).
    pub const SLUG_RESERVED: &str = "slug_reserved";
    /// `governance: app` on a Space with no governing App (409).
    pub const NO_GOVERNING_APP: &str = "no_governing_app";
    /// The home client never governs a Space: a governing link for it, `max_level`
    /// on it, or a home client that governs a Space (409).
    pub const HOME_CLIENT_CANNOT_GOVERN: &str = "home_client_cannot_govern";
    /// Sew 0.8.0: an App id on a Space write names a participant App of the Space, not
    /// its governing App (409; a participant App has no say over the Space).
    pub const APP_NOT_GOVERNING: &str = "app_not_governing";
    /// Sew 0.8.0: a participant link (or unlink) names the Space's governing App
    /// (409; use `spaces/unbind`).
    pub const APP_IS_GOVERNING: &str = "app_is_governing";
    /// Sew 0.8.0: the Space has 8 participant Apps already (409).
    pub const PARTICIPANT_APPS_MAX: &str = "participant_apps_max";
    /// Sew 0.8.0: the home client is never a participant App: a link naming it, or a home
    /// client that is a participant somewhere (409).
    pub const HOME_CLIENT_CANNOT_LINK: &str = "home_client_cannot_link";
    /// An erase asks for `anonymise` after the account was erased with `delete_content` (409;
    /// the content is gone).
    pub const ERASE_MODE_DOWNGRADE: &str = "erase_mode_downgrade";
    /// `governance: app` while box users (`k2:owner`, `k2:connect:`) other than
    /// the owner are members of the Space (409, `MgmtBoxUsersDetails`).
    pub const BOX_USERS_PRESENT: &str = "box_users_present";
    /// A peer uid outside `[mgmt] allowed_uids` (403).
    pub const PEER_UID_NOT_ALLOWED: &str = "peer_uid_not_allowed";
    /// Act-as headers on a `/mgmt/v1/*` route (400).
    pub const ACT_AS_ON_MGMT_ROUTE: &str = "act_as_on_mgmt_route";
    /// `/api/v1/*` on the mgmt socket without both act-as headers (400).
    pub const ACT_AS_HEADERS_REQUIRED: &str = "act_as_headers_required";
    /// An act-as header does not parse (400).
    pub const ACT_AS_HEADER_INVALID: &str = "act_as_header_invalid";
    /// Act-as headers on the TCP client listener (400).
    pub const ACT_AS_ONLY_ON_MGMT_SOCKET: &str = "act_as_only_on_mgmt_socket";
    /// A route act-as may never reach (403).
    pub const ACT_AS_ROUTE_FORBIDDEN: &str = "act_as_route_forbidden";
    /// `Sew-Via` names another agent than `Sew-Act-As` (403).
    pub const VIA_AGENT_MISMATCH: &str = "via_agent_mismatch";
    /// The handle is held by another active member (409).
    pub const USERNAME_TAKEN: &str = "username_taken";
    /// The install owner cannot be deprovisioned or made a guest (422).
    pub const IS_OWNER: &str = "is_owner";
    /// The person is deactivated (422).
    pub const PERSON_DEACTIVATED: &str = "person_deactivated";
    /// The person is a guest (422).
    pub const PERSON_IS_GUEST: &str = "person_is_guest";
    /// The person is an agent (422).
    pub const PERSON_IS_AGENT: &str = "person_is_agent";
    /// Provisioning an assistant whose person is not provisioned yet (422;
    /// K2 retries for up to 1 h).
    pub const ASSISTANT_PERSON_UNKNOWN: &str = "assistant_person_unknown";
    /// Provisioning an assistant whose person is deactivated (422; K2 stops
    /// retrying).
    pub const ASSISTANT_PERSON_INACTIVE: &str = "assistant_person_inactive";
    /// The agent is deactivated (422).
    pub const AGENT_DEACTIVATED: &str = "agent_deactivated";
    /// Pinning a coworker; convert it through provision (422).
    pub const AGENT_IS_COWORKER: &str = "agent_is_coworker";
    /// Unpin names another person than the current pin (409).
    pub const PIN_MISMATCH: &str = "pin_mismatch";
    /// The owner must be an active, non-guest person (422).
    pub const TARGET_NOT_ELIGIBLE: &str = "target_not_eligible";
    /// This server has no backup engine (503).
    pub const BACKUPS_UNAVAILABLE: &str = "backups_unavailable";
    /// Another backup holds the backup lock (409).
    pub const BACKUP_RUNNING: &str = "backup_running";
    /// An owner, manager or member names no provisioned principal (422).
    pub const PRINCIPAL_UNKNOWN: &str = "principal_unknown";
    /// The slug is held by a Space of another owner or App (409).
    pub const SLUG_TAKEN: &str = "slug_taken";
    /// `app_id` does not name the Space's App (409).
    pub const APP_MISMATCH: &str = "app_mismatch";
    /// The Space is bound to another App already (409).
    pub const SPACE_BOUND: &str = "space_bound";
    /// The Space was unbound from another App; only that App may bind it until
    /// `spaces/release` (409).
    pub const SPACE_BOUND_ELSEWHERE: &str = "space_bound_elsewhere";
    /// Channels over mgmt need a Space bound to an App (409).
    pub const SPACE_UNBOUND: &str = "space_unbound";
    /// The slug is held by a channel of the other type (409; details
    /// `MgmtChannelTypeMismatchDetails`).
    pub const CHANNEL_TYPE_MISMATCH: &str = "channel_type_mismatch";
    /// The owner approval did not verify (403).
    pub const APPROVAL_INVALID: &str = "approval_invalid";
    /// A transfer needs the current owner's approval (403).
    pub const OWNER_APPROVAL_REQUIRED: &str = "owner_approval_required";
    /// `force` needs a standalone install and peer uid 0 (403).
    pub const FORCE_NOT_ALLOWED: &str = "force_not_allowed";
    /// A live job holds the dead job's dedupe key (409).
    pub const DEDUPE_KEY_TAKEN: &str = "dedupe_key_taken";
    /// Only dead jobs can be retried (422).
    pub const NOT_DEAD: &str = "not_dead";
    /// Master rotation needs `sew-master-key-prev` loaded (422).
    pub const NO_RETIRING_MASTER_KEY: &str = "no_retiring_master_key";
    /// An assistant may not open DMs (403, stage B).
    pub const ASSISTANT_CANNOT_OPEN_DM: &str = "assistant_cannot_open_dm";
    /// Assistants hold no space or channel memberships (422, stage B).
    pub const ASSISTANT_NOT_JOINABLE: &str = "assistant_not_joinable";
    /// A 1:1 DM with a deactivated partner is read-only (422, stage B).
    pub const DM_PARTNER_DEACTIVATED: &str = "dm_partner_deactivated";
    /// `sew init` when an owner exists (409).
    pub const OWNER_EXISTS: &str = "owner_exists";
    /// The local user is deactivated (409/422).
    pub const USER_DEACTIVATED: &str = "user_deactivated";
    /// The member is not a local person (422).
    pub const NOT_A_LOCAL_PERSON: &str = "not_a_local_person";
    /// A local user's handle is part of its external id and cannot change (422).
    pub const LOCAL_USERNAME_FIXED: &str = "local_username_fixed";
    /// No mgmt or act-as route matches (404).
    pub const UNKNOWN_ROUTE: &str = "unknown_route";
    /// The install owner's role changes only through `owner/set` (422).
    pub const OWNER_ROLE_FIXED: &str = "owner_role_fixed";
    /// A guest's role changes only through guest conversion (422).
    pub const GUEST_ROLE_FIXED: &str = "guest_role_fixed";
    /// Agents are always `member` (422).
    pub const AGENT_ROLE_FIXED: &str = "agent_role_fixed";
    /// A meeting link guest is never converted or given another role (422).
    pub const LINK_GUEST_FIXED: &str = "link_guest_fixed";
    /// A person with an active pinned assistant cannot become a guest (422; matches
    /// K2's own refusal). `details.assistant_ids` names them.
    pub const PERSON_HAS_ASSISTANTS: &str = "person_has_assistants";
    /// A guest login while `guests.enabled` is off (audit `details.reason`).
    pub const GUESTS_DISABLED: &str = "guests_disabled";
    /// The compliance flag is never on a guest (422).
    pub const TARGET_IS_GUEST: &str = "target_is_guest";
    /// The relink target already belongs to a member (409; merging is not in v1).
    pub const EXTERNAL_ID_TAKEN: &str = "external_id_taken";
    /// A relink before the install's identity mode is K2 (422; switch first).
    /// The server-governed Space (governance `server`) on a standalone install (409).
    pub const IDENTITY_NOT_K2: &str = "identity_not_k2";
    /// The identity switch needs `[identity.k2] jwks_url` in the running config (422).
    pub const K2_IDENTITY_UNCONFIGURED: &str = "k2_identity_unconfigured";
    /// The identity switch is refused while an upgrade or rollback runs (409).
    pub const UPGRADE_IN_PROGRESS: &str = "upgrade_in_progress";

    /// Every reason above.
    pub const ALL: &[&str] = &[
        PEER_UID_NOT_ALLOWED,
        ACT_AS_ON_MGMT_ROUTE,
        ACT_AS_HEADERS_REQUIRED,
        ACT_AS_HEADER_INVALID,
        ACT_AS_ONLY_ON_MGMT_SOCKET,
        ACT_AS_ROUTE_FORBIDDEN,
        VIA_AGENT_MISMATCH,
        USERNAME_TAKEN,
        IS_OWNER,
        PERSON_DEACTIVATED,
        PERSON_IS_GUEST,
        PERSON_IS_AGENT,
        ASSISTANT_PERSON_UNKNOWN,
        ASSISTANT_PERSON_INACTIVE,
        AGENT_DEACTIVATED,
        AGENT_IS_COWORKER,
        PIN_MISMATCH,
        TARGET_NOT_ELIGIBLE,
        APPROVAL_INVALID,
        OWNER_APPROVAL_REQUIRED,
        FORCE_NOT_ALLOWED,
        DEDUPE_KEY_TAKEN,
        NOT_DEAD,
        NO_RETIRING_MASTER_KEY,
        ASSISTANT_CANNOT_OPEN_DM,
        ASSISTANT_NOT_JOINABLE,
        DM_PARTNER_DEACTIVATED,
        OWNER_EXISTS,
        USER_DEACTIVATED,
        NOT_A_LOCAL_PERSON,
        LOCAL_USERNAME_FIXED,
        UNKNOWN_ROUTE,
        OWNER_ROLE_FIXED,
        GUEST_ROLE_FIXED,
        AGENT_ROLE_FIXED,
        LINK_GUEST_FIXED,
        PERSON_HAS_ASSISTANTS,
        GUESTS_DISABLED,
        TARGET_IS_GUEST,
        EXTERNAL_ID_TAKEN,
        IDENTITY_NOT_K2,
        K2_IDENTITY_UNCONFIGURED,
        UPGRADE_IN_PROGRESS,
    ];
}

// ---- agent deliveries ----

/// Largest `limit` of a deliveries page.
pub const DELIVERIES_LIMIT_MAX: u32 = 500;
/// Default `limit` of a deliveries page.
pub const DELIVERIES_LIMIT_DEFAULT: u32 = 100;
/// A delivery's `body` is cut to at most this many bytes, at a char boundary.
pub const DELIVERY_BODY_MAX_BYTES: usize = 4_000;

/// Query of `GET /mgmt/v1/agents/{member_id}/deliveries`: long-poll after a member seq.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveriesQuery {
    /// Return entries with `seq > after`. K2 sends the previous page's `next_after`.
    pub after: u64,
    /// Long-poll up to this long (≤ 25000, default 25000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// At most this many entries (1–500, default 100; additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl DeliveriesQuery {
    /// The long-poll timeout, default 25 s.
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms.unwrap_or(DELIVERIES_LONG_POLL_MAX_MS)
    }

    /// The page size, default 100.
    pub fn limit(&self) -> u32 {
        self.limit.unwrap_or(DELIVERIES_LIMIT_DEFAULT)
    }
}

impl Validate for DeliveriesQuery {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(t) = self.timeout_ms
            && t > DELIVERIES_LONG_POLL_MAX_MS
        {
            return Err(ValidationError::invalid(
                "timeout_ms",
                format!("timeout_ms must be at most {DELIVERIES_LONG_POLL_MAX_MS}"),
            ));
        }
        if let Some(l) = self.limit
            && !(1..=DELIVERIES_LIMIT_MAX).contains(&l)
        {
            return Err(ValidationError::invalid(
                "limit",
                format!("limit must be 1–{DELIVERIES_LIMIT_MAX}"),
            ));
        }
        Ok(())
    }
}

unit_enum! {
    /// What kind of message a delivery carries. v1 sends `text` only;
    /// `Unknown` renders as text.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum DeliveryKind {
        /// A chat message.
        #[default]
        Text = "text",
        /// A DM call record.
        CallRecord = "call_record",
        /// A poll.
        Poll = "poll",
        /// A call transcript.
        Transcript = "transcript",
    }
}

unit_enum! {
    /// Where a delivered message lives, from the agent's side.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum DeliveryPlace {
        /// A space channel.
        SpaceChannel = "space_channel",
        /// A 1:1 DM the agent is in.
        Dm = "dm",
        /// A group DM the agent is in.
        GroupDm = "group_dm",
        /// A DM or group DM of an assistant's person, read (never posted to) by the assistant.
        PersonDm = "person_dm",
    }
}

/// A call record's summary in a delivery (`kind = call_record`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeliveryCall {
    /// The call was missed.
    pub missed: bool,
    /// How long it lasted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// A message summary in an agent delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeliveryMessage {
    /// The message.
    pub id: Id,
    /// Its channel.
    pub channel_id: Id,
    /// Its space; absent for DMs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<Id>,
    /// Its thread root, for replies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_root_id: Option<Id>,
    /// The author.
    pub author_id: Id,
    /// The author's current display name (also for deactivated authors).
    pub author_display_name: String,
    /// The author's current username.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_username: Option<Handle>,
    /// Person or agent.
    pub author_kind: MemberKind,
    /// The message's conversation address (the thread address for replies).
    pub addr: Addr,
    /// Where a reply goes: `<channel addr>/<message id>` for a top-level
    /// space message, the thread address for replies, the DM address for DMs. Absent for
    /// `place = person_dm` (the assistant may not post there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_addr: Option<Addr>,
    /// Where the message lives from the agent's side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<DeliveryPlace>,
    /// What kind of message it is.
    #[serde(default)]
    pub kind: DeliveryKind,
    /// A call record's summary (`kind = call_record` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<DeliveryCall>,
    /// A recording record's summary (`kind = transcript` only). Never audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<crate::recording::RecordingSummary>,
    /// The current body (after edits), cut to 4,000 bytes at a char boundary.
    pub body: String,
    /// Whether `body` was cut.
    #[serde(default)]
    pub body_truncated: bool,
    /// How many attachments the message has.
    #[serde(default)]
    pub attachment_count: u32,
    /// For assistant-authored messages: the person the assistant posted for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<Id>,
    /// That person's current display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of_display_name: Option<String>,
    /// When it was posted.
    pub created_at: Timestamp,
}

/// One agent delivery: a `notification` entry of the agent's member log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Delivery {
    /// The member seq.
    pub seq: u64,
    /// When it was written.
    pub at: Timestamp,
    /// Why the agent was notified.
    pub reason: NotificationReason,
    /// The message.
    pub message: DeliveryMessage,
}

/// Response of `GET /mgmt/v1/agents/{member_id}/deliveries`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeliveriesPage {
    /// Deliveries with `seq > after`, oldest first. Entries the agent cannot read now are
    /// skipped without a trace.
    pub entries: Vec<Delivery>,
    /// The member log head.
    pub head: u64,
    /// The next `after`: the seq of the last row scanned (delivered or skipped), `after` when
    /// nothing was scanned, `head` after an empty long-poll (additive).
    #[serde(default)]
    pub next_after: u64,
    /// Whether more entries follow `next_after`.
    pub has_more: bool,
    /// The install's `sync_epoch` (opaque; changes on restore).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_epoch: Option<String>,
}

// ---- jobs ----

/// Response of the job-starting routes (backup, export, rotate-master).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StartJobResponse {
    /// The job.
    pub job_id: Id,
}

unit_enum! {
    /// A job's state.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum JobState {
        /// Waiting to run.
        Queued = "queued",
        /// Running.
        Running = "running",
        /// Finished.
        Succeeded = "succeeded",
        /// Failed; may retry.
        Failed = "failed",
        /// Exhausted its attempts.
        Dead = "dead",
    }
}

/// Response of `GET /mgmt/v1/jobs/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobStatus {
    /// The job.
    pub id: Id,
    /// The job kind (for example `backup`).
    pub kind: String,
    /// The state.
    pub state: JobState,
    /// Attempts so far.
    pub attempts: u32,
    /// The last error kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// When it was queued.
    pub created_at: Timestamp,
    /// When it finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Timestamp>,
    /// What a `succeeded` job recorded (`jobs_done.result`; additive).
    /// Absent otherwise. Ids and counts only, never content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

/// Query of `GET /mgmt/v1/jobs`: newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobsQuery {
    /// Only jobs in this state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<JobState>,
    /// Only jobs of this kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Jobs with an id below this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Id>,
    /// Page size, 1–100 (default 50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl JobsQuery {
    /// The page size, default 50.
    pub fn limit(&self) -> u32 {
        self.limit.unwrap_or(crate::limits::PAGE_LIMIT_DEFAULT)
    }
}

impl Validate for JobsQuery {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(s) = self.state {
            check_known("state", s)?;
        }
        if let Some(k) = &self.kind
            && (k.is_empty()
                || k.len() > 64
                || !k
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
        {
            return Err(ValidationError::invalid(
                "kind",
                "kind must be 1–64 chars of [a-z0-9_]",
            ));
        }
        if let Some(l) = self.limit
            && !(1..=crate::limits::PAGE_LIMIT_MAX).contains(&l)
        {
            return Err(ValidationError::invalid(
                "limit",
                format!("limit must be 1–{}", crate::limits::PAGE_LIMIT_MAX),
            ));
        }
        Ok(())
    }
}

/// Response of `GET /mgmt/v1/jobs`.
pub type JobStatusPage = crate::http::Page<JobStatus>;
