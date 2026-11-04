//! Error envelope and codes.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::perms::Perms;
use crate::policy::PolicyKey;

unit_enum! {
    /// A stable machine error code. `lower_snake`; the HTTP status comes from [`Code::status`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum Code {
        /// 400: the request is malformed.
        BadRequest = "bad_request",
        /// 400: a field failed validation; `details.field` names it.
        ValidationFailed = "validation_failed",
        /// 401: no or unknown credentials.
        Unauthenticated = "unauthenticated",
        /// 401: the session expired.
        SessionExpired = "session_expired",
        /// 403: a permission bit is missing.
        MissingPermission = "missing_permission",
        /// 403: an org policy forbids this.
        PolicyDenied = "policy_denied",
        /// 403: a valid pass for a principal with no Sew account.
        NotProvisioned = "not_provisioned",
        /// 403: act-as targeted a person; only agents can be acted as.
        ActAsPersonForbidden = "act_as_person_forbidden",
        /// 403: the command is off on a K2-provided install.
        ProviderIsK2 = "provider_is_k2",
        /// 403: a quota or count limit is reached.
        QuotaExceeded = "quota_exceeded",
        /// 404: missing, or hidden from the caller (masked).
        NotFound = "not_found",
        /// 405: only GET reads and POST mutations exist.
        MethodNotAllowed = "method_not_allowed",
        /// 409: conflicts with current state.
        Conflict = "conflict",
        /// 409: an `Idempotency-Key` was reused with a different body.
        IdempotencyConflict = "idempotency_conflict",
        /// 410: the cursor is behind the change-log floor; `details.floor` says where.
        ResyncRequired = "resync_required",
        /// 413: the body or a field is too large.
        TooLarge = "too_large",
        /// 422: valid request, but the target's state forbids it.
        InvalidState = "invalid_state",
        /// 429: rate limited; `details.retry_after_ms` says when to retry.
        RateLimited = "rate_limited",
        /// 500: a server bug.
        Internal = "internal",
        /// 503: temporarily unavailable.
        Unavailable = "unavailable",
        /// 416: the byte range is outside the file; `Content-Range: bytes */<size>`.
        RangeNotSatisfiable = "range_not_satisfiable",
        /// 507: the server's file storage is below its free-space floor.
        StorageFull = "storage_full",
        /// 410: the meeting has ended or was cancelled (also a voice join in its
        /// channel).
        MeetingEnded = "meeting_ended",
        /// 425: too early: the meeting opens at `details.opens_at` (also a voice join
        /// before `starts_at` − 15 min).
        TooEarly = "too_early",
        /// 404: a meeting link that is unknown, has a wrong secret, was revoked or rotated, or
        /// whose meeting ended or was cancelled (one body for all four).
        LinkInvalid = "link_invalid",
        /// 423: the meeting is locked.
        MeetingLocked = "meeting_locked",
        /// 429: the meeting's lobby or room is full (`max_waiting`, `max_guests_inside`;
        /// nothing was written).
        LobbyFull = "lobby_full",
        /// 403: this join nonce (or member) was turned away twice within 10 minutes and may
        /// not knock on this meeting for an hour.
        KnockBlocked = "knock_blocked",
        /// 410: the feed's cursor is behind the pruned part of the log (the person
        /// notification feed); `details.oldest_seq` says where to resume.
        CursorExpired = "cursor_expired",
    }
}

impl Code {
    /// The HTTP status for this code. `Unknown` maps to 500.
    pub const fn status(self) -> u16 {
        match self {
            Code::BadRequest | Code::ValidationFailed => 400,
            Code::Unauthenticated | Code::SessionExpired => 401,
            Code::MissingPermission
            | Code::PolicyDenied
            | Code::NotProvisioned
            | Code::ActAsPersonForbidden
            | Code::ProviderIsK2
            | Code::QuotaExceeded
            | Code::KnockBlocked => 403,
            Code::NotFound | Code::LinkInvalid => 404,
            Code::MethodNotAllowed => 405,
            Code::Conflict | Code::IdempotencyConflict => 409,
            Code::ResyncRequired | Code::MeetingEnded | Code::CursorExpired => 410,
            Code::TooLarge => 413,
            Code::RangeNotSatisfiable => 416,
            Code::InvalidState => 422,
            Code::MeetingLocked => 423,
            Code::TooEarly => 425,
            Code::RateLimited | Code::LobbyFull => 429,
            Code::Internal | Code::Unknown => 500,
            Code::Unavailable => 503,
            Code::StorageFull => 507,
        }
    }
}

/// Every error response body: `{"ok": false, "error": {code, hint, details}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorBody {
    /// Always `false`.
    pub ok: bool,
    /// The error.
    pub error: ApiError,
}

impl ErrorBody {
    /// Wraps an error.
    pub fn new(error: ApiError) -> Self {
        ErrorBody { ok: false, error }
    }
}

impl From<ApiError> for ErrorBody {
    fn from(error: ApiError) -> Self {
        ErrorBody::new(error)
    }
}

/// A structured error: a stable code, a human hint, and typed details.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApiError {
    /// The stable machine code.
    pub code: Code,
    /// For humans; may change between releases.
    pub hint: String,
    /// Code-specific details, such as `field`, `retry_after_ms`, `floor` or `reason`.
    #[serde(default)]
    pub details: Map<String, Value>,
}

impl ApiError {
    /// An error with empty details.
    pub fn new(code: Code, hint: impl Into<String>) -> Self {
        ApiError {
            code,
            hint: hint.into(),
            details: Map::new(),
        }
    }

    /// Merges a details struct into `details`. A value that does not serialize to a JSON object
    /// is stored under `"value"`.
    pub fn with_details(mut self, details: impl Serialize) -> Self {
        match serde_json::to_value(details) {
            Ok(Value::Object(map)) => self.details.extend(map),
            Ok(Value::Null) => {}
            Ok(other) => {
                self.details.insert("value".to_owned(), other);
            }
            Err(e) => {
                self.details
                    .insert("details_error".to_owned(), Value::String(e.to_string()));
            }
        }
        self
    }

    /// The HTTP status.
    pub fn status(&self) -> u16 {
        self.code.status()
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}): {}", self.code, self.code.status(), self.hint)
    }
}

impl std::error::Error for ApiError {}

/// Details for `validation_failed` (and `too_large` on a field): the field that failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ValidationDetails {
    /// The failing field, dotted for nested fields (for example `reference.kind`).
    pub field: String,
}

/// Details for `rate_limited`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RetryAfterDetails {
    /// Milliseconds to wait before retrying.
    pub retry_after_ms: u64,
    /// Which limit refused, for limits other than the plain rate buckets (`slow_mode`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `details.reason` of a `429 rate_limited` from a channel's slow mode.
pub const RATE_REASON_SLOW_MODE: &str = "slow_mode";

/// `details.reason` of the `409 conflict` a role reorder answers when its list is not the
/// space's current role set.
pub const CONFLICT_REASON_STALE_ROLE_LIST: &str = "stale_role_list";

/// Details of an error that names a reason: mgmt and act-as refusals such as
/// `409 conflict {reason: "username_taken", field: "username"}`. The reasons are listed in
/// `sew_proto::mgmt::reasons`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReasonDetails {
    /// Why, as a stable `lower_snake` string.
    pub reason: String,
    /// The field or header the reason is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

impl ReasonDetails {
    /// Details with a reason only.
    pub fn new(reason: &str) -> Self {
        ReasonDetails {
            reason: reason.to_owned(),
            field: None,
        }
    }

    /// Details with a reason and a field.
    pub fn with_field(reason: &str, field: &str) -> Self {
        ReasonDetails {
            reason: reason.to_owned(),
            field: Some(field.to_owned()),
        }
    }
}

unit_enum! {
    /// Why a change-log reader must start over (`details.reason` of `410 resync_required`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum ResyncReason {
        /// `since` is below the floor: entries were trimmed.
        BehindFloor = "behind_floor",
        /// `since` is above the head: a restore, or another install.
        AheadOfHead = "ahead_of_head",
        /// The entries were not contiguous (must never happen).
        Gap = "gap",
    }
}

/// Details for `resync_required`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResyncRequiredDetails {
    /// The change-log floor; catch up from a fresh snapshot instead.
    pub floor: u64,
    /// The log head at the read (K2 resets its deliveries cursor here; additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<u64>,
    /// `behind_floor`, `ahead_of_head` or `gap` (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ResyncReason>,
    /// The install's `sync_epoch` (additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_epoch: Option<String>,
}

impl ResyncRequiredDetails {
    /// Details with only the floor.
    pub fn new(floor: u64) -> Self {
        ResyncRequiredDetails {
            floor,
            head: None,
            reason: None,
            sync_epoch: None,
        }
    }
}

unit_enum! {
    /// `details.reason` of a `403 missing_permission` from the permission engine.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum PermissionReason {
        /// A permission bit is missing; `details.missing` lists it.
        MissingPermission = "missing_permission",
        /// The change touches bits the actor does not hold; `details.missing` lists them.
        UnheldBits = "unheld_bits",
        /// The target is at or above the actor on the role ladder.
        Hierarchy = "hierarchy",
        /// The space owner cannot be the target.
        OwnerImmune = "owner_immune",
        /// Only the space owner may do this.
        OwnerOnly = "owner_only",
        /// Guests cannot do or receive this.
        GuestLimit = "guest_limit",
        /// A meeting link guest cannot do this (every route off the link-guest
        /// allowlist, before the handler).
        LinkGuestLimit = "link_guest_limit",
        /// Agents cannot do or receive this.
        AgentLimit = "agent_limit",
        /// Needs an install role (owner, admin) or the compliance flag.
        InstallRole = "install_role",
        /// Only the author may do this (edits, edit history).
        NotAuthor = "not_author",
        /// Only the person an assistant posted for may do this (on-behalf approval).
        NotOnBehalfPerson = "not_on_behalf_person",
        /// The person's own session is needed: act-as cannot do this.
        OwnSessionRequired = "own_session_required",
        /// A relayed `Sew-On-Behalf-Of` claim is never accepted here.
        RelayNotAllowed = "relay_not_allowed",
        /// A session with an `app` claim (the home client included) never
        /// reaches the server-governed Space (governance `server`), whoever the member is.
        AppSessionNotAllowed = "app_session_not_allowed",
        /// An App user (`k2:principal:`) never reaches the server-governed Space,
        /// even from a session without an `app` claim.
        AppUserNotAllowed = "app_user_not_allowed",
        /// A call-scoped credential called a route outside its call's scope.
        CallCredentialScope = "call_credential_scope",
    }
}

unit_enum! {
    /// `details.reason` of a `422 invalid_state` from the permission engine.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum InvalidStateReason {
        /// `@everyone` cannot be moved, deleted, assigned or removed.
        EveryoneRole = "everyone_role",
        /// Role position 0 belongs to `@everyone`.
        BadPosition = "bad_position",
        /// Bits outside the registry, or outside what an overwrite may hold.
        UnknownBits = "unknown_bits",
        /// The actor is the target.
        SelfTarget = "self_target",
        /// The space owner must transfer ownership before leaving.
        OwnerMustTransfer = "owner_must_transfer",
        /// Holders of ADMINISTRATOR cannot be timed out.
        TargetIsAdministrator = "target_is_administrator",
        /// The timeout end is not in the future.
        TimeoutInPast = "timeout_in_past",
        /// Timeouts last at most 28 days.
        TimeoutTooLong = "timeout_too_long",
        /// The channel is archived.
        ChannelArchived = "channel_archived",
        /// The channel has no category to sync with.
        NoCategory = "no_category",
        /// The target is not a member of the space.
        NotSpaceMember = "not_space_member",
        /// The target is deactivated.
        InactiveTarget = "inactive_target",
        /// A DM participant is deactivated.
        InactiveParticipant = "inactive_participant",
        /// A DM holds 2–9 people.
        DmSize = "dm_size",
        /// The install owner's role changes only by an owner transfer.
        OwnerRoleFixed = "owner_role_fixed",
        /// A guest's install role cannot be set here.
        GuestRoleFixed = "guest_role_fixed",
        /// Agents are never install admins.
        AgentRoleFixed = "agent_role_fixed",
        /// The space owner cannot be removed; transfer first.
        SpaceOwner = "space_owner",
        /// Removing oneself is `leave`.
        UseLeave = "use_leave",
        /// A new space owner must be an active non-guest person and a space member.
        InvalidTransferTarget = "invalid_transfer_target",
        /// The target would not see the channel after the add.
        TargetCannotView = "target_cannot_view",
        /// Assistants are never space or channel members.
        AssistantNotJoinable = "assistant_not_joinable",
        /// DMs cannot be renamed or changed.
        DmImmutable = "dm_immutable",
        /// DMs cannot be archived.
        DmArchiveNotAllowed = "dm_archive_not_allowed",
        /// DMs cannot be left.
        DmLeaveNotAllowed = "dm_leave_not_allowed",
        /// A DM's member set is fixed.
        DmMembershipFixed = "dm_membership_fixed",
        /// A DM needs someone besides the caller.
        SelfDm = "self_dm",
        /// A DM needs at least one person.
        AgentOnlyDm = "agent_only_dm",
        /// Guests enter a space only through a channel add.
        GuestNeedsChannel = "guest_needs_channel",
        // ---- Messaging
        /// The message is deleted.
        MessageDeleted = "message_deleted",
        /// The thread root is deleted; the thread is closed.
        ThreadRootDeleted = "thread_root_deleted",
        /// Replies point at a top-level message, never at a reply.
        NotAThreadRoot = "not_a_thread_root",
        /// A reply or quote target is missing, deleted or in another channel.
        ReferenceNotFound = "reference_not_found",
        /// The other person in a 1:1 DM is deactivated; the DM is read-only.
        DmPartnerDeactivated = "dm_partner_deactivated",
        /// A message holds at most 1,000 versions.
        EditLimitReached = "edit_limit_reached",
        /// Custom emoji are not available yet.
        CustomEmojiUnavailable = "custom_emoji_unavailable",
        /// An edit cannot add `@here` or `@channel`.
        BroadMentionOnEdit = "broad_mention_on_edit",
        /// Overwrites and slow mode exist only on space channels, not DMs.
        NotSpaceChannel = "not_space_channel",
        /// A space holds at most 250 roles, `@everyone` included.
        RoleLimit = "role_limit",
        /// A channel or category holds at most 100 overwrites.
        OverwriteLimit = "overwrite_limit",
        // ---- Sync
        /// The message is missing or in another channel.
        MessageNotInChannel = "message_not_in_channel",
        /// A thread reply not also sent to the channel (use the thread's mark unread).
        NotATimelineMessage = "not_a_timeline_message",
        /// The message is neither the root nor a reply of this thread.
        MessageNotInThread = "message_not_in_thread",
        /// Mark unread works on messages up to `[sync] mark_unread_window_days` old.
        MarkUnreadTooOld = "mark_unread_too_old",
        /// The space belongs to an App; its members come only through mgmt calls for that
        /// App, never a client add, an open join or a guest channel add.
        AppBoundSpace = "app_bound_space",
        /// A generic channel route on a meeting channel, or a generic channel create in a
        /// hidden Space; meeting channels change only through the meeting routes.
        MeetingChannel = "meeting_channel",
        // ---- Bans
        /// Install owners and admins cannot be banned: they can lift any ban, so it would not
        /// hold.
        TargetIsInstallAdmin = "target_is_install_admin",
        /// The target is banned from the space (told only to BAN_MEMBERS holders and install
        /// owners and admins).
        TargetBanned = "target_banned",
        /// The target cannot join the space (what everyone else is told about a banned
        /// target).
        TargetCannotJoin = "target_cannot_join",
        // ---- Private Spaces
        /// A private Space is invite-only; make it public (`private: false`) to open it.
        PrivateSpace = "private_space",
        // ---- Unbound App Spaces
        /// The Space was unbound from its App and is a read-only orphan: no posts, edits,
        /// reactions, uploads, joins, calls or member adds until that App binds it again (or an
        /// admin releases it). Reads, search and export still work.
        SpaceOrphaned = "space_orphaned",
        // ---- App-governed Spaces
        /// The member is an App's service owner (`app:<app_id>`): it owns the App's Spaces but
        /// is never in a DM, never mentioned and never in calls.
        ServiceMember = "service_member",
        /// The Space is App-governed (`governance: app`): its App decides its structure
        /// (settings, members, roles, overwrites, channels, categories, emoji, user groups,
        /// ownership, and leaving an assigned private channel or the Space). Only mgmt changes
        /// it.
        GovernedByApp = "governed_by_app",
        /// The Space is the server-governed Space (`governance: server`): K2's daemon
        /// decides its structure over mgmt (settings, members, roles, overwrites, channels,
        /// categories, emoji, user groups, ownership and leaving it); clients never change it.
        GovernedByServer = "governed_by_server",
        /// The role is one of an App-governed Space's managed level roles (Moderator, Admin):
        /// clients never edit, delete or assign it.
        RoleManaged = "role_managed",
        /// The Space's governing App has `uploads: false`: no uploads, file posts,
        /// forwarded file copies, scheduled sends with files or recordings in it. Link previews
        /// stay.
        UploadsDisabled = "uploads_disabled",
    }
}

/// Details of a `409 conflict` on a unique field: the field that is taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConflictDetails {
    /// The taken field, for example `slug`.
    pub field: String,
}

/// Details of a `403 quota_exceeded` count limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QuotaDetails {
    /// The config key of the limit, for example `channels_per_space`.
    pub limit: String,
    /// The limit.
    pub max: u64,
}

/// Details of a `403 missing_permission` from the permission engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionDeniedDetails {
    /// Why.
    pub reason: PermissionReason,
    /// The missing bits, for `missing_permission` and `unheld_bits`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<Perms>,
}

/// Details of a `403 policy_denied`: the org policy that refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PolicyDeniedDetails {
    /// The policy key.
    pub policy: PolicyKey,
}

/// Details of a `422 invalid_state` from the permission engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InvalidStateDetails {
    /// Why.
    pub reason: InvalidStateReason,
}

/// A request that failed its range or cross-field rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// `validation_failed` or `too_large`.
    pub code: Code,
    /// The failing field, dotted for nested fields.
    pub field: String,
    /// For humans.
    pub hint: String,
}

impl ValidationError {
    /// A `validation_failed` error on `field`.
    pub fn invalid(field: impl Into<String>, hint: impl Into<String>) -> Self {
        ValidationError {
            code: Code::ValidationFailed,
            field: field.into(),
            hint: hint.into(),
        }
    }

    /// A `too_large` error on `field`.
    pub fn too_large(field: impl Into<String>, hint: impl Into<String>) -> Self {
        ValidationError {
            code: Code::TooLarge,
            field: field.into(),
            hint: hint.into(),
        }
    }

    /// Prefixes the field with a parent path: `reference` + `kind` → `reference.kind`.
    pub fn nested(mut self, parent: &str) -> Self {
        self.field = format!("{parent}.{}", self.field);
        self
    }

    /// The API error with `details.field`.
    pub fn into_api_error(self) -> ApiError {
        ApiError::new(self.code, self.hint).with_details(ValidationDetails { field: self.field })
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} on {}: {}", self.code, self.field, self.hint)
    }
}

impl std::error::Error for ValidationError {}

impl From<ValidationError> for ApiError {
    fn from(e: ValidationError) -> Self {
        e.into_api_error()
    }
}

/// Range and cross-field checks for a request type. Serde checks the shape; this checks the
/// rest. Every request type implements it (serde rule 11).
pub trait Validate {
    /// `Ok` when the request is acceptable.
    fn validate(&self) -> Result<(), ValidationError>;
}

// ---- helpers used by request validators ----

pub(crate) fn check_chars(
    field: &str,
    value: &str,
    min: usize,
    max: usize,
) -> Result<(), ValidationError> {
    let n = value.chars().count();
    if n < min || n > max {
        return Err(ValidationError::invalid(
            field,
            format!("{field} must be {min}–{max} characters, got {n}"),
        ));
    }
    Ok(())
}

pub(crate) fn check_opt_chars(
    field: &str,
    value: Option<&str>,
    min: usize,
    max: usize,
) -> Result<(), ValidationError> {
    match value {
        Some(v) => check_chars(field, v, min, max),
        None => Ok(()),
    }
}

pub(crate) fn check_known<T: crate::registry::UnitEnum>(
    field: &str,
    value: T,
) -> Result<(), ValidationError> {
    if value.is_unknown_value() {
        return Err(ValidationError::invalid(
            field,
            format!("{field} has a value this server does not know"),
        ));
    }
    Ok(())
}

pub(crate) fn check_unique_ids(
    field: &str,
    ids: &[crate::id::Id],
    min: usize,
    max: usize,
) -> Result<(), ValidationError> {
    if ids.len() < min || ids.len() > max {
        return Err(ValidationError::invalid(
            field,
            format!("{field} must hold {min}–{max} ids, got {}", ids.len()),
        ));
    }
    let mut seen = std::collections::HashSet::with_capacity(ids.len());
    for id in ids {
        if !seen.insert(*id) {
            return Err(ValidationError::invalid(
                field,
                format!("{field} holds {id} twice"),
            ));
        }
    }
    Ok(())
}
