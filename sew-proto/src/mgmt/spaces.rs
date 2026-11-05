//! Spaces over mgmt: K2 creates additional Spaces for `k2 chat` and its Apps,
//! binds a Space to an App, and adds or removes a Space's members (an App's
//! Space gets its members only from that App's user grants, through these calls).
//!
//! - `POST /mgmt/v1/spaces/create` ([`MgmtSpaceCreateRequest`] → [`MgmtSpaceResponse`]):
//!   the owner is `k2:owner` or a person, never an agent (agents never own Spaces);
//!   `managers` (persons or coworker agents) join and hold the Space's manager role,
//!   [`crate::perms::SPACE_MANAGER`], which has no ADMINISTRATOR. Idempotent on `slug`: the same
//!   slug, owner and `app_id` again answers the existing Space (`created: false`) and adds any
//!   listed manager it lacks; another owner or App on that slug is `409 conflict`
//!   `slug_taken`.
//! - `POST /mgmt/v1/spaces/bind` ([`MgmtSpaceBindRequest`] → [`MgmtSpaceResponse`]): binds an
//!   unbound Space to an App, once. The same App again is a no-op; another is `409 conflict`
//!   `space_bound`.
//! - `POST /mgmt/v1/spaces/members/add` ([`MgmtSpaceMembersAddRequest`] →
//!   [`MgmtSpaceMembersAddResponse`]) and `…/members/remove`
//!   ([`MgmtSpaceMemberRemoveRequest`] → [`MgmtSpaceMemberRemoveResponse`]) for any Space, not
//!   only K2's default one. `app_id` must name the Space's App (absent for an unbound Space),
//!   else `409 conflict` `app_mismatch`. An add never fails for one member: a principal that
//!   cannot join comes back in `not_added` with a reason, and a member the Space bans in
//!   `skipped` (member ids, the shape of `members/add`).
//!
//! - `GET`/`POST /mgmt/v1/spaces/{space_id}/settings` ([`MgmtSpaceSettingsRequest`] →
//!   [`MgmtSpaceSettings`]): a Space's privacy, for any Space (K2 sets it on App-bound
//!   ones). `app_id` must name the Space's App as above. `private: true` hides the Space from
//!   everyone outside it and makes it invite-only; `false` makes it public again (an App's
//!   Space stays invite-only, others become `open`). The same value again changes nothing and
//!   audits nothing; a switch runs as the Space's owner, like the client's
//!   `POST /api/v1/spaces/{id}/update {private}` (audited `space.visibility`, a
//!   `space_updated` member entry and a `space_updated` event to the members).
//! - `POST /mgmt/v1/spaces/unbind` ([`MgmtSpaceUnbindRequest`] → [`MgmtSpaceOrphanResponse`]):
//!   the Space leaves its App and becomes a **read-only orphan**: `app_id` moves to
//!   `orphaned_from`, App sessions lose it, home and unscoped sessions keep it, labelled
//!   by `Space.orphaned_from`. Posts, edits, reactions, uploads, joins, calls and member adds
//!   are `422 invalid_state` `space_orphaned` for everyone; reads, search and export still
//!   work. `app_id` must name the Space's App (`409 conflict` `app_mismatch`); a repeat is
//!   `200` with `changed: false`. A bind to the same App brings it back; a bind to another App
//!   is `409 conflict` `space_bound_elsewhere` until `spaces/release`.
//! - `POST /mgmt/v1/spaces/release` ([`MgmtSpaceReleaseRequest`] → [`MgmtSpaceOrphanResponse`]):
//!   clears `orphaned_from`, so the Space is a plain unbound Space again (writable, and
//!   any App may bind it). `confirm: true` is required; a Space that is not orphaned answers
//!   `changed: false` (`409 conflict` `space_bound` while it is bound).
//!
//! - `POST /mgmt/v1/spaces/{space_id}/apps/link` and `…/apps/unlink`
//!   ([`MgmtSpaceAppLinkRequest`] → [`MgmtSpaceAppsChange`], Sew 0.8.0): a
//!   **participant link** lets a second App's sessions see and use the Space as the person,
//!   with no say over it. `governing_app_id` must name the Space's governing App (`409
//!   conflict` `app_mismatch`); a Space without one (unbound, an orphan, the K5 Space) is `409
//!   conflict` `no_governing_app`; the governing App as `app_id` is `409 conflict`
//!   `app_is_governing`; the home client is `409 conflict` `home_client_cannot_link`; a ninth
//!   participant is `409 conflict` `participant_apps_max`. A repeat is `changed: false`.
//!   Audited `space.app_link` / `space.app_unlink`. A participant App's id on a Space write
//!   that takes `app_id` is `409 conflict` `app_not_governing`.
//! - `GET /mgmt/v1/apps/{app_id}/spaces` ([`MgmtAppSpacesQuery`] → [`MgmtAppSpaces`]): every
//!   link of one App, both roles, by Space id, up to 500 a page.
//!
//! On a bound Space the client API never adds members: `members/add`, the open join, a channel
//! add that would bring in a new Space member, and switching it to `open` are `422
//! invalid_state` `app_bound_space`. Leaving and moderator removal still work.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::addr::Slug;
use crate::error::{Validate, ValidationError, check_chars};
use crate::external_id::ExternalId;
use crate::id::Id;
use crate::limits::NAME_MAX_CHARS;

/// Longest App id, bytes.
pub const APP_ID_MAX_BYTES: usize = 64;
/// Most managers in one create.
pub const SPACE_MANAGERS_MAX: usize = 10;
/// Most principals in one mgmt add.
pub const SPACE_MEMBERS_ADD_MAX: usize = 100;
/// The manager role's name.
pub const SPACE_MANAGER_ROLE_NAME: &str = "Managers";

/// Whether `s` is an App id: 1–64 bytes of `[a-z0-9][a-z0-9._-]*`.
pub fn is_app_id(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=APP_ID_MAX_BYTES).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'.' || *c == b'_' || *c == b'-'
        })
}

pub(super) fn check_app_id(field: &str, v: &str) -> Result<(), ValidationError> {
    if is_app_id(v) {
        Ok(())
    } else {
        Err(ValidationError::invalid(
            field,
            "an App id is 1–64 bytes of [a-z0-9][a-z0-9._-]*",
        ))
    }
}

fn check_opt_app_id(v: &Option<String>) -> Result<(), ValidationError> {
    v.as_deref().map_or(Ok(()), |a| check_app_id("app_id", a))
}

/// Body of `POST /mgmt/v1/spaces/create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtSpaceCreateRequest {
    /// The slug; `dm` is reserved. The idempotency key of the create.
    pub slug: Slug,
    /// The name, 1–100 characters.
    pub name: String,
    /// The owner: `k2:owner` or a person (never an agent). With `app_id`, `app:<app_id>`
    /// (or no owner at all) makes the App's service owner the owner, created on first need;
    /// another App's service owner is `409 conflict` `app_mismatch`. Required without `app_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<ExternalId>,
    /// 0–10 distinct members who join and hold the manager role (persons or coworker agents,
    /// not the owner).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub managers: Vec<ExternalId>,
    /// The App the Space belongs to; absent for a Space of K2's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// A hidden Space, left out of `GET /spaces`, browse, the directory and
    /// (without the `hidden_spaces` capability) READY. Set at create only. Default `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    /// `server` creates the server-governed Space (owner `k2:server`, slug
    /// [`ADMIN_SPACE_SLUG`], no `app_id`, no managers, not hidden; a K2 install only). Only
    /// `server` is accepted here (`app` is the settings switch). Absent: today's create.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub governance: Option<crate::api::spaces::SpaceGovernance>,
}

/// The server-governed Space's slug. Reserved: any other Space's create (client or mgmt)
/// or rename to it answers `400 validation_failed` with `details.reason` `slug_reserved`.
pub const ADMIN_SPACE_SLUG: &str = "k5-admin";

/// The feature flag of the server-governed Space (governance
/// `server`, the `k2:server` service owner). Listed in [`super::FEATURES`].
pub const FEATURE_SERVER_GOVERNED_SPACE: &str = "server_governed_space";

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(v: &bool) -> bool {
    !*v
}

impl Validate for MgmtSpaceCreateRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        crate::api::spaces::check_space_slug("slug", &self.slug)?;
        check_chars("name", &self.name, 1, NAME_MAX_CHARS)?;
        match &self.owner {
            None if self.app_id.is_none() => {
                return Err(ValidationError::invalid(
                    "owner",
                    "a space without an app_id names its owner",
                ));
            }
            Some(o) if o.is_agent() => {
                return Err(ValidationError::invalid(
                    "owner",
                    "agents never own spaces; the owner is k2:owner or a person",
                ));
            }
            Some(ExternalId::K2Server)
                if self.governance != Some(crate::api::spaces::SpaceGovernance::Server) =>
            {
                return Err(ValidationError::invalid(
                    "owner",
                    "k2:server owns only the K5 Space (governance server)",
                ));
            }
            _ => {}
        }
        match self.governance {
            None => {}
            Some(crate::api::spaces::SpaceGovernance::Server) => {
                if self.owner != Some(ExternalId::K2Server) {
                    return Err(ValidationError::invalid(
                        "owner",
                        "the K5 Space (governance server) is owned by k2:server",
                    ));
                }
                if self.slug.as_str() != ADMIN_SPACE_SLUG {
                    return Err(ValidationError::invalid(
                        "slug",
                        format!("the K5 Space's slug is {ADMIN_SPACE_SLUG}"),
                    ));
                }
                if !self.managers.is_empty() {
                    return Err(ValidationError::invalid(
                        "managers",
                        "the K5 Space takes no managers: levels come from the member-channels sync",
                    ));
                }
                if self.hidden {
                    return Err(ValidationError::invalid(
                        "hidden",
                        "the K5 Space is never hidden",
                    ));
                }
            }
            Some(g) => {
                crate::error::check_known("governance", g)?;
                return Err(ValidationError::invalid(
                    "governance",
                    "only server is accepted at create (app is the settings switch)",
                ));
            }
        }
        if self.managers.len() > SPACE_MANAGERS_MAX {
            return Err(ValidationError::invalid(
                "managers",
                format!("at most {SPACE_MANAGERS_MAX} managers"),
            ));
        }
        for (i, m) in self.managers.iter().enumerate() {
            if Some(m) == self.owner.as_ref() {
                return Err(ValidationError::invalid(
                    format!("managers.{i}"),
                    "the owner is not listed as a manager",
                ));
            }
            if self.managers[..i].contains(m) {
                return Err(ValidationError::invalid(
                    format!("managers.{i}"),
                    "listed twice",
                ));
            }
        }
        check_opt_app_id(&self.app_id)
    }
}

/// A Space over mgmt: the answer of create and bind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtSpaceResponse {
    /// The Space.
    pub space_id: Id,
    /// Its slug.
    pub slug: Slug,
    /// Its name.
    pub name: String,
    /// Its owner's member id.
    pub owner_member_id: Id,
    /// The App it belongs to; absent when not bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Whether this call created it.
    pub created: bool,
    /// The manager role, once a manager was listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_role_id: Option<Id>,
    /// The listed managers' member ids, in request order (each a member holding the role).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manager_member_ids: Vec<Id>,
    /// The Space is hidden. Absent when `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    /// Who governs the Space's structure; absent for `sew` (`server` for the
    /// K2 install's own Space, `app` for an App-governed one).
    #[serde(
        default = "crate::api::spaces::SpaceGovernance::default_sew",
        skip_serializing_if = "crate::api::spaces::SpaceGovernance::is_sew"
    )]
    pub governance: crate::api::spaces::SpaceGovernance,
}

/// Body of `POST /mgmt/v1/spaces/bind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtSpaceBindRequest {
    /// The Space.
    pub space_id: Id,
    /// The App.
    pub app_id: String,
}

impl Validate for MgmtSpaceBindRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        check_app_id("app_id", &self.app_id)
    }
}

/// The feature flag of private Spaces (`Space.private`, the client update's `private`, and
/// `/mgmt/v1/spaces/{space_id}/settings`).
pub const FEATURE_PRIVATE_SPACES: &str = "private_spaces";

/// Body of `POST /mgmt/v1/spaces/{space_id}/settings`. An absent field keeps its
/// value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtSpaceSettingsRequest {
    /// The Space's App; absent for an unbound Space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Private (`true`) or public (`false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,
    /// `app` hands the Space's structure to its governing App (`409 conflict`
    /// `no_governing_app` without one) and creates the managed Moderator and Admin roles;
    /// `sew` hands it back (the managed roles become ordinary roles). Never
    /// to or from `server` (`409 conflict` `server_governance_fixed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub governance: Option<crate::api::spaces::SpaceGovernance>,
}

impl Validate for MgmtSpaceSettingsRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(g) = self.governance {
            crate::error::check_known("governance", g)?;
        }
        match &self.app_id {
            Some(a) => check_app_id("app_id", a),
            None => Ok(()),
        }
    }
}

/// Answer of `GET`/`POST /mgmt/v1/spaces/{space_id}/settings`: the values after the
/// call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtSpaceSettings {
    /// The Space.
    pub space_id: Id,
    /// Its App; absent when unbound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Whether the Space is private.
    pub private: bool,
    /// Its join policy (`invite` whenever `private`).
    pub join_policy: crate::api::spaces::SpaceJoinPolicy,
    /// Who governs the Space's structure. Old payloads read as `sew`.
    #[serde(default = "crate::api::spaces::SpaceGovernance::default_sew")]
    pub governance: crate::api::spaces::SpaceGovernance,
}

/// The feature flag of App-governed Spaces (Sew 0.6.0): `app:` external ids, the
/// governance switch, the `space_apps` links and the App levels and uploads settings. Listed
/// in [`super::FEATURES`].
pub const FEATURE_APP_GOVERNED_SPACES: &str = "app_governed_spaces";

unit_enum! {
    /// What a Space–App link is. 0.6.0 writes only `governing` (one per Space, the
    /// Space's `app_id`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum MgmtSpaceAppRole {
        /// The App the Space is bound to; it may govern the Space (`governance: app`).
        Governing = "governing",
        /// An App that takes part in the Space without governing it (Sew 0.8.0:
        /// its sessions see and use the Space as the person; it has no say over it).
        Participant = "participant",
    }
}

/// Details of `409 conflict` `box_users_present` on the switch to `governance: app`:
/// box users (`k2:owner`, `k2:connect:`) are never members of an App-governed Space. K2
/// removes them first; a box-user owner is the one Sew removes itself (the App's service owner
/// takes over).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtBoxUsersDetails {
    /// `box_users_present`.
    pub reason: String,
    /// `governance`.
    pub field: String,
    /// The box users in the Space other than the owner, ascending.
    pub external_ids: Vec<ExternalId>,
}

/// One link of `GET /mgmt/v1/spaces/{space_id}/apps`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtSpaceApp {
    /// The App.
    pub app_id: String,
    /// What the link is.
    pub role: MgmtSpaceAppRole,
}

/// Answer of `GET /mgmt/v1/spaces/{space_id}/apps`: the Space's App links, the
/// governing one first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtSpaceApps {
    /// The Space.
    pub space_id: Id,
    /// Who governs its structure.
    pub governance: crate::api::spaces::SpaceGovernance,
    /// Its links (empty for an unbound Space).
    pub items: Vec<MgmtSpaceApp>,
}

/// Most links one page of `GET /mgmt/v1/apps/{app_id}/spaces` answers.
pub const MGMT_APP_SPACES_LIMIT_MAX: u32 = 500;
/// The default page size of `GET /mgmt/v1/apps/{app_id}/spaces`.
pub const MGMT_APP_SPACES_LIMIT_DEFAULT: u32 = 100;

/// Body of `POST /mgmt/v1/spaces/{space_id}/apps/link` and `…/apps/unlink` (Sew 0.8.0):
/// a participant link of `app_id` to the Space. `governing_app_id` must name the
/// Space's governing App (`409 conflict` `app_mismatch`), which guards against a stale view
/// after an unbind or rebind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtSpaceAppLinkRequest {
    /// The Space's governing App.
    pub governing_app_id: String,
    /// The participant App.
    pub app_id: String,
}

impl Validate for MgmtSpaceAppLinkRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        check_app_id("governing_app_id", &self.governing_app_id)?;
        check_app_id("app_id", &self.app_id)
    }
}

/// Answer of `POST /mgmt/v1/spaces/{space_id}/apps/link` and `…/apps/unlink`: the Space's
/// links after the call, the governing one first (as `GET …/apps` lists them).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtSpaceAppsChange {
    /// The Space.
    pub space_id: Id,
    /// Who governs its structure.
    pub governance: crate::api::spaces::SpaceGovernance,
    /// Its links after the call.
    pub items: Vec<MgmtSpaceApp>,
    /// `false` when the call changed nothing (linked already, or not linked).
    pub changed: bool,
}

/// Query of `GET /mgmt/v1/apps/{app_id}/spaces`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtAppSpacesQuery {
    /// Links of Spaces with an id above this one (the previous page's `next`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Id>,
    /// Page size, 1–500 (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl MgmtAppSpacesQuery {
    /// The page size, default [`MGMT_APP_SPACES_LIMIT_DEFAULT`].
    pub fn limit(&self) -> u32 {
        self.limit.unwrap_or(MGMT_APP_SPACES_LIMIT_DEFAULT)
    }
}

impl Validate for MgmtAppSpacesQuery {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(l) = self.limit
            && !(1..=MGMT_APP_SPACES_LIMIT_MAX).contains(&l)
        {
            return Err(ValidationError::invalid(
                "limit",
                format!("limit must be 1–{MGMT_APP_SPACES_LIMIT_MAX}"),
            ));
        }
        Ok(())
    }
}

/// One link of `GET /mgmt/v1/apps/{app_id}/spaces`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtAppSpace {
    /// The Space.
    pub space_id: Id,
    /// What the App's link to it is.
    pub role: MgmtSpaceAppRole,
}

/// Answer of `GET /mgmt/v1/apps/{app_id}/spaces`: every link of one App (both roles), by
/// Space id ascending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtAppSpaces {
    /// The App.
    pub app_id: String,
    /// One page of its links.
    pub items: Vec<MgmtAppSpace>,
    /// The `after` of the next page; absent on the last one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<Id>,
}

/// The feature flag of `spaces/unbind` and `spaces/release`. Listed in
/// [`super::FEATURES`].
pub const FEATURE_SPACE_UNBIND: &str = "space_unbind";

/// The feature flag of participant links (Sew 0.8.0): `spaces/{space_id}/apps/link` and
/// `…/unlink`, `GET /mgmt/v1/apps/{app_id}/spaces`, `participant` rows in
/// `GET /mgmt/v1/spaces/{space_id}/apps`, `participant_app_ids` on the client `Space`,
/// `ReadySpace` and the person feed, the voice leave reason `app_unlinked`, and export minor 10.
/// K2 calls the link routes only when it sees it. Listed in [`super::FEATURES`].
pub const FEATURE_APP_PARTICIPANT_LINKS: &str = "app_participant_links";

/// Body of `POST /mgmt/v1/spaces/unbind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtSpaceUnbindRequest {
    /// The Space.
    pub space_id: Id,
    /// The App it is bound to (on a repeat: the App it was orphaned from).
    pub app_id: String,
}

impl Validate for MgmtSpaceUnbindRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        check_app_id("app_id", &self.app_id)
    }
}

/// Body of `POST /mgmt/v1/spaces/release`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtSpaceReleaseRequest {
    /// The orphaned Space.
    pub space_id: Id,
    /// Must be `true`: after a release any App may bind the Space and inherit its history.
    pub confirm: bool,
}

impl Validate for MgmtSpaceReleaseRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if !self.confirm {
            return Err(ValidationError::invalid(
                "confirm",
                "a released Space can be bound by any App; send confirm: true",
            ));
        }
        Ok(())
    }
}

/// Answer of `POST /mgmt/v1/spaces/unbind` and `POST /mgmt/v1/spaces/release`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtSpaceOrphanResponse {
    /// The Space.
    pub space_id: Id,
    /// The App it is bound to now; absent for an orphaned or unbound Space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// The App it was unbound from while it is a read-only orphan; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphaned_from: Option<String>,
    /// `false` when the call changed nothing (a repeat).
    pub changed: bool,
}

/// Body of `POST /mgmt/v1/spaces/members/add`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtSpaceMembersAddRequest {
    /// The Space.
    pub space_id: Id,
    /// The Space's App; absent for an unbound Space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// 1–100 distinct principals.
    pub external_ids: Vec<ExternalId>,
}

impl Validate for MgmtSpaceMembersAddRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        check_opt_app_id(&self.app_id)?;
        if self.external_ids.is_empty() || self.external_ids.len() > SPACE_MEMBERS_ADD_MAX {
            return Err(ValidationError::invalid(
                "external_ids",
                format!("1–{SPACE_MEMBERS_ADD_MAX} principals"),
            ));
        }
        for (i, e) in self.external_ids.iter().enumerate() {
            if self.external_ids[..i].contains(e) {
                return Err(ValidationError::invalid(
                    format!("external_ids.{i}"),
                    "listed twice",
                ));
            }
        }
        Ok(())
    }
}

/// A principal and its member id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtSpaceMemberRef {
    /// The principal.
    pub external_id: ExternalId,
    /// Its member id.
    pub member_id: Id,
}

unit_enum! {
    /// Why a principal was not added (bans are not a reason here: a banned member is in
    /// `skipped`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum MgmtSpaceNotAddedReason {
        /// No member has this external id (provision first).
        NotProvisioned = "not_provisioned",
        /// The member is deactivated.
        Deactivated = "deactivated",
        /// An assistant never joins a Space (it reads through its person).
        Assistant = "assistant",
        /// A guest enters a Space only through a channel add.
        Guest = "guest",
        /// The Space is at its App's `members_per_space_max`.
        QuotaExceeded = "quota_exceeded",
    }
}

/// A principal an add did not add, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtSpaceNotAdded {
    /// The principal.
    pub external_id: ExternalId,
    /// Why.
    pub reason: MgmtSpaceNotAddedReason,
}

/// Response of `POST /mgmt/v1/spaces/members/add`, each list in request order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtSpaceMembersAddResponse {
    /// Added by this call.
    pub added: Vec<MgmtSpaceMemberRef>,
    /// Members already.
    pub already: Vec<MgmtSpaceMemberRef>,
    /// Members the Space bans, not added (the shape of `members/add`: member ids, no
    /// reason; K2 stops re-adding them). Always present.
    #[serde(default)]
    pub skipped: Vec<Id>,
    /// Principals that cannot join, with the reason.
    pub not_added: Vec<MgmtSpaceNotAdded>,
}

/// Body of `POST /mgmt/v1/spaces/members/remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtSpaceMemberRemoveRequest {
    /// The Space.
    pub space_id: Id,
    /// The Space's App; absent for an unbound Space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// The principal.
    pub external_id: ExternalId,
}

impl Validate for MgmtSpaceMemberRemoveRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        check_opt_app_id(&self.app_id)
    }
}

/// Response of `POST /mgmt/v1/spaces/members/remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtSpaceMemberRemoveResponse {
    /// Whether this call removed a member (`false`: not provisioned or not a member).
    pub removed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_grammar() {
        for ok in ["atrium", "a", "app.v2", "my_app-1", &"a".repeat(64)] {
            assert!(is_app_id(ok), "{ok}");
        }
        for bad in ["", "Lobby", "-a", ".a", "a b", "a/b", &"a".repeat(65)] {
            assert!(!is_app_id(bad), "{bad}");
        }
    }
}
