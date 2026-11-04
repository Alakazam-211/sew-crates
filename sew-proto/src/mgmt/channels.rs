//! Channels over mgmt: K2 creates channels in App-bound Spaces (`chat.createChannel`).
//!
//! - `POST /mgmt/v1/channels/create` ([`MgmtChannelCreateRequest`] →
//!   [`MgmtChannelCreateResponse`]): a text or voice channel in a Space bound to an App,
//!   made as the
//!   Space's owner through the client API's channel create (the same checks, events, change-log
//!   entries and audit rows, with the mgmt peer uid on them). `app_id` must name the Space's App
//!   (`409 conflict` `app_mismatch`); an unbound Space is `409 conflict` `space_unbound`.
//!   Idempotent on `(space_id, slug)`: a repeat answers the existing channel (`created: false`)
//!   with its current name and visibility (a different `name` or `private` changes nothing) and
//!   adds any listed member it lacks. A repeat naming the other `type` is `409 conflict`
//!   `channel_type_mismatch` ([`MgmtChannelTypeMismatchDetails`]).
//! - `members` (0–100, member ids or external ids) are added like `spaces/members/add` answers:
//!   `added`, `already`, `skipped` (member ids a Space ban keeps out) and
//!   `not_added` with a reason. A member must already be in the Space (an App's Space gets its
//!   members only through `spaces/members/add`).
//! - `POST /mgmt/v1/channels/bulk` ([`MgmtChannelBulkRequest`] → [`MgmtChannelBulkResponse`]):
//!   1–200 ops (`ensure`, `rename`, `archive`, `unarchive`) in a Space whose
//!   governing App is `app_id`, run as the Space's owner through the same functions as the
//!   single calls, each answered on its own ([`MgmtChannelOpResult`]).

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::addr::Slug;
use crate::api::channels::ChannelType;
use crate::error::{Validate, ValidationError, check_chars};
use crate::external_id::ExternalId;
use crate::id::{Id, parse_id_str};
use crate::limits::NAME_MAX_CHARS;

use super::spaces::{MgmtSpaceMemberRef, check_app_id};

/// Most members in one mgmt channel create.
pub const CHANNEL_MEMBERS_ADD_MAX: usize = 100;

/// A member named by its member id (decimal) or its external id (contains `:`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MgmtMemberRef {
    /// A member id.
    Member(Id),
    /// An external id.
    External(ExternalId),
}

impl FromStr for MgmtMemberRef {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.contains(':') {
            return s
                .parse()
                .map(MgmtMemberRef::External)
                .map_err(|e: crate::external_id::ExternalIdError| e.to_string());
        }
        parse_id_str(s)
            .map(MgmtMemberRef::Member)
            .ok_or_else(|| format!("{s:?} is neither a member id nor an external id"))
    }
}

impl From<ExternalId> for MgmtMemberRef {
    fn from(e: ExternalId) -> Self {
        MgmtMemberRef::External(e)
    }
}

impl fmt::Display for MgmtMemberRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MgmtMemberRef::Member(id) => fmt::Display::fmt(id, f),
            MgmtMemberRef::External(e) => fmt::Display::fmt(e, f),
        }
    }
}

impl Serialize for MgmtMemberRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MgmtMemberRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = Cow::<'de, str>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for MgmtMemberRef {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "MgmtMemberRef".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "maxLength": 128,
            "description": "A member id (decimal string) or an external id (k2:principal:<uuid>, k2:workspace:<uuid>, k2:connect:<uuid>, k2:owner or local:<handle>)."
        })
    }
}

/// Body of `POST /mgmt/v1/channels/create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtChannelCreateRequest {
    /// The Space; it must be bound to an App, or be the server-governed Space.
    pub space_id: Id,
    /// The Space's App; absent for the server-governed Space (governance `server`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// The slug, unique in the Space (the client rules). With `space_id`, the idempotency key.
    pub slug: Slug,
    /// The name, 1–100 characters. Used at create only.
    pub name: String,
    /// A private channel (its members are exactly the owner and `members`). Used at create
    /// only.
    pub private: bool,
    /// 0–100 distinct members (member ids or external ids) to add.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<MgmtMemberRef>,
    /// `text` (absent = text) or `voice`; `announcement` and the DM types are refused
    /// (`400 validation_failed`, field `type`). Part of the idempotency check: an existing slug
    /// of the other type is `409 conflict` `channel_type_mismatch`.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ChannelType>,
    /// Voice only: at most this many people in the call, 0–99, 0 = no per-channel limit;
    /// absent = 0. On a text channel it is `400 validation_failed` (field `user_limit`). Used at
    /// create only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_limit: Option<u8>,
}

impl MgmtChannelCreateRequest {
    /// The channel type asked for: `type`, absent = text.
    pub fn channel_type(&self) -> ChannelType {
        self.kind.unwrap_or(ChannelType::Text)
    }
}

impl Validate for MgmtChannelCreateRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(a) = &self.app_id {
            check_app_id("app_id", a)?;
        }
        let kind = self.channel_type();
        if !matches!(kind, ChannelType::Text | ChannelType::Voice) {
            return Err(ValidationError::invalid(
                "type",
                "type must be text or voice",
            ));
        }
        crate::voice::check_user_limit(self.user_limit)?;
        if self.user_limit.is_some() && kind != ChannelType::Voice {
            return Err(ValidationError::invalid(
                "user_limit",
                "user_limit is for voice channels only",
            ));
        }
        check_chars("name", &self.name, 1, NAME_MAX_CHARS)?;
        if self.members.len() > CHANNEL_MEMBERS_ADD_MAX {
            return Err(ValidationError::invalid(
                "members",
                format!("at most {CHANNEL_MEMBERS_ADD_MAX} members"),
            ));
        }
        for (i, m) in self.members.iter().enumerate() {
            if self.members[..i].contains(m) {
                return Err(ValidationError::invalid(
                    format!("members.{i}"),
                    "listed twice",
                ));
            }
        }
        Ok(())
    }
}

unit_enum! {
    /// Why a listed member was not added to the channel (a ban is not a reason here: a
    /// banned member is in `skipped`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum MgmtChannelNotAddedReason {
        /// No member has this id or external id (provision first).
        NotProvisioned = "not_provisioned",
        /// The member is deactivated.
        Deactivated = "deactivated",
        /// An assistant never joins a channel (it reads through its person).
        Assistant = "assistant",
        /// A guest outside the Space (an App's Space takes no guests through channel adds).
        Guest = "guest",
        /// Not a member of the Space: add it with `spaces/members/add` first.
        NotSpaceMember = "not_space_member",
    }
}

/// A listed member the create did not add, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtChannelNotAdded {
    /// The member as the request named it.
    pub member: MgmtMemberRef,
    /// Why.
    pub reason: MgmtChannelNotAddedReason,
}

/// Response of `POST /mgmt/v1/channels/create`, each member list in request order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtChannelCreateResponse {
    /// The channel.
    pub channel_id: Id,
    /// Its Space.
    pub space_id: Id,
    /// Its slug.
    pub slug: Slug,
    /// Its current name (a repeat never changes it).
    pub name: String,
    /// Whether it is private (current; a repeat never changes it).
    pub private: bool,
    /// Its type, `text` or `voice` (additive; absent from an older Sew = text).
    #[serde(rename = "type", default = "text_type")]
    pub kind: ChannelType,
    /// Voice only (additive): its current per-channel limit, 0 = none. Absent for text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_limit: Option<u8>,
    /// Whether this call created it.
    pub created: bool,
    /// Listed members this call added. The owner, who is always a member, is never here; when
    /// listed it is in `already`.
    pub added: Vec<MgmtSpaceMemberRef>,
    /// Listed members that were in the channel already.
    pub already: Vec<MgmtSpaceMemberRef>,
    /// Listed members a Space ban keeps out (member ids, the shape of `members/add`). Always
    /// present.
    #[serde(default)]
    pub skipped: Vec<Id>,
    /// Listed members that cannot join, with the reason.
    pub not_added: Vec<MgmtChannelNotAdded>,
}

fn text_type() -> ChannelType {
    ChannelType::Text
}

/// Details of `409 conflict` `channel_type_mismatch` on `POST /mgmt/v1/channels/create`:
/// the slug is held by a channel of another type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtChannelTypeMismatchDetails {
    /// `channel_type_mismatch`.
    pub reason: String,
    /// `type`.
    pub field: String,
    /// The existing channel's type.
    pub existing_type: ChannelType,
}

/// Most ops in one `POST /mgmt/v1/channels/bulk`.
pub const CHANNEL_BULK_OPS_MAX: usize = 200;

/// One op of `POST /mgmt/v1/channels/bulk`, tagged by `op`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum MgmtChannelOp {
    /// Create: ensure, never update. A repeat of an existing slug answers the channel
    /// (`created: false`) and changes neither its name nor its visibility; a slug held by the
    /// other type fails the op with `409 conflict` `channel_type_mismatch`.
    Ensure {
        /// The slug, unique in the Space.
        slug: Slug,
        /// The name, 1–100 characters. Used at create only.
        name: String,
        /// A private channel (its members are the owner until the sync adds people). Used at
        /// create only.
        private: bool,
        /// `text` (absent = text) or `voice`.
        #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
        kind: Option<ChannelType>,
        /// Voice only: at most this many people in the call, 0–99 (0 = no limit). Used at
        /// create only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_limit: Option<u8>,
    },
    /// A new name (1–100 characters) for a channel of the Space.
    Rename {
        /// The channel.
        channel_id: Id,
        /// The new name.
        name: String,
    },
    /// Archives a channel of the Space (a no-op when it is archived).
    Archive {
        /// The channel.
        channel_id: Id,
    },
    /// Unarchives a channel of the Space (a no-op when it is not archived).
    Unarchive {
        /// The channel.
        channel_id: Id,
    },
}

impl MgmtChannelOp {
    /// The op's kind.
    pub fn kind(&self) -> MgmtChannelOpKind {
        match self {
            MgmtChannelOp::Ensure { .. } => MgmtChannelOpKind::Ensure,
            MgmtChannelOp::Rename { .. } => MgmtChannelOpKind::Rename,
            MgmtChannelOp::Archive { .. } => MgmtChannelOpKind::Archive,
            MgmtChannelOp::Unarchive { .. } => MgmtChannelOpKind::Unarchive,
        }
    }

    fn validate_at(&self, i: usize) -> Result<(), ValidationError> {
        let at = |e: ValidationError| ValidationError {
            field: format!("ops.{i}.{}", e.field),
            ..e
        };
        match self {
            MgmtChannelOp::Ensure {
                name,
                kind,
                user_limit,
                ..
            } => {
                let kind = kind.unwrap_or(ChannelType::Text);
                if !matches!(kind, ChannelType::Text | ChannelType::Voice) {
                    return Err(at(ValidationError::invalid(
                        "type",
                        "type must be text or voice",
                    )));
                }
                crate::voice::check_user_limit(*user_limit).map_err(at)?;
                if user_limit.is_some() && kind != ChannelType::Voice {
                    return Err(at(ValidationError::invalid(
                        "user_limit",
                        "user_limit is for voice channels only",
                    )));
                }
                check_chars("name", name, 1, NAME_MAX_CHARS).map_err(at)
            }
            MgmtChannelOp::Rename { name, .. } => {
                check_chars("name", name, 1, NAME_MAX_CHARS).map_err(at)
            }
            MgmtChannelOp::Archive { .. } | MgmtChannelOp::Unarchive { .. } => Ok(()),
        }
    }
}

unit_enum! {
    /// The kind of a bulk channel op.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum MgmtChannelOpKind {
        /// Create (ensure, never update).
        Ensure = "ensure",
        /// A new name.
        Rename = "rename",
        /// Archive.
        Archive = "archive",
        /// Unarchive.
        Unarchive = "unarchive",
    }
}

/// Body of `POST /mgmt/v1/channels/bulk`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtChannelBulkRequest {
    /// The Space; `app_id` must be its governing App (absent: the server-governed Space).
    pub space_id: Id,
    /// The Space's governing App; absent for the server-governed Space (governance
    /// `server`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// 1–200 ops, run in order, each on its own; one failing op fails only itself.
    pub ops: Vec<MgmtChannelOp>,
}

impl Validate for MgmtChannelBulkRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(a) = &self.app_id {
            check_app_id("app_id", a)?;
        }
        if self.ops.is_empty() || self.ops.len() > CHANNEL_BULK_OPS_MAX {
            return Err(ValidationError::invalid(
                "ops",
                format!("1–{CHANNEL_BULK_OPS_MAX} ops"),
            ));
        }
        for (i, op) in self.ops.iter().enumerate() {
            op.validate_at(i)?;
        }
        Ok(())
    }
}

/// One op's answer in `POST /mgmt/v1/channels/bulk`, in request order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtChannelOpResult {
    /// The op's kind.
    pub op: MgmtChannelOpKind,
    /// Whether the op succeeded (a no-op repeat succeeds).
    pub ok: bool,
    /// The channel: the ensured one, or the one the op named. Absent when an `ensure` failed
    /// before a channel was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<Id>,
    /// `ensure` that succeeded: whether this op created the channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<bool>,
    /// `rename`, `archive` and `unarchive` that succeeded: whether the op changed the channel
    /// (`false`: it already had that name or state, and nothing was written).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<bool>,
    /// A failed op: the error the single call would have answered (its code, hint and
    /// details, such as `channel_type_mismatch` or `channel_not_found`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<crate::error::ApiError>,
}

/// Response of `POST /mgmt/v1/channels/bulk`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtChannelBulkResponse {
    /// The Space.
    pub space_id: Id,
    /// One answer per op, in request order.
    pub results: Vec<MgmtChannelOpResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_ref_parses_both_forms() {
        let id: MgmtMemberRef = "48831765147549697".parse().unwrap();
        assert!(matches!(id, MgmtMemberRef::Member(_)));
        let ext: MgmtMemberRef = "k2:owner".parse().unwrap();
        assert_eq!(ext, MgmtMemberRef::External(ExternalId::K2Owner));
        for bad in ["", "abc", "k2:nope", "-1"] {
            assert!(bad.parse::<MgmtMemberRef>().is_err(), "{bad}");
        }
        assert_eq!(
            serde_json::to_string(&ext).unwrap(),
            "\"k2:owner\"".to_owned()
        );
    }
}
