//! Members.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::addr::Handle;
use crate::id::Id;
use crate::time::Timestamp;

unit_enum! {
    /// Whether a member is a person or an agent.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum MemberKind {
        /// A human.
        Person = "person",
        /// A K2 agent.
        Agent = "agent",
    }
}

unit_enum! {
    /// What kind of agent a member is.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum AgentKind {
        /// A team member in its own right.
        Coworker = "coworker",
        /// Pinned to one person; acts within that person's access.
        Assistant = "assistant",
    }
}

unit_enum! {
    /// Install-wide role. Separate from space roles.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum InstallRole {
        /// All install admin, plus transfer.
        Owner = "owner",
        /// Spaces, policies, members, guests. No content access to private channels or DMs.
        Admin = "admin",
        /// A regular member.
        Member = "member",
        /// A guest.
        Guest = "guest",
        /// A meeting link guest: made by redeeming a meeting
        /// link, bound to one meeting channel, capped by `LINK_GUEST_MASK`, never a K2
        /// principal. Every guest rule applies, and only the link-guest allowlist passes.
        LinkGuest = "link_guest",
    }
}

impl InstallRole {
    /// A guest of either kind (guest or link guest): every guest rule applies.
    pub const fn is_guest_kind(self) -> bool {
        matches!(self, InstallRole::Guest | InstallRole::LinkGuest)
    }
}

/// A member of the install, person or agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Member {
    /// The member id.
    pub id: Id,
    /// Person or agent.
    pub kind: MemberKind,
    /// For agents, coworker or assistant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_kind: Option<AgentKind>,
    /// For assistants, the member id of their person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_of: Option<Id>,
    /// Install-wide role.
    pub install_role: InstallRole,
    /// The handle.
    pub username: Handle,
    /// The display name.
    pub display_name: String,
    /// Job title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Pronouns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pronouns: Option<String>,
    /// Signed avatar URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    /// Whether the member is deactivated.
    pub deactivated: bool,
    /// When the member was created.
    pub created_at: Timestamp,
    /// Email address; sent only to the member themself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Whether the member holds the compliance flag; sent only to the member themself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compliance: Option<bool>,
    /// 64 px signed avatar URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_thumb_url: Option<String>,
    /// IANA timezone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Status; absent when none or expired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<crate::api::people::MemberStatus>,
    /// Bumped by every change of this object; clients drop older events.
    #[serde(default)]
    pub profile_rev: u64,
    /// Custom field values the viewer may see; only on `GET /members/{id}` and the self object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_fields: Option<Vec<crate::api::people::ProfileFieldValue>>,
    /// Fields the client must show read-only; self only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_fields: Option<Vec<String>>,
    /// The member's K2 id (`k2:principal:<uuid>`, `k2:workspace:<uuid>` for agents, ...;
    /// additive). Only on a K2-identity install, and only when the caller's session sees a
    /// Space or DM shared with the member (an App session counts only its App's Spaces), or
    /// on the self object. Never for guests or link guests; never on standalone installs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
}

/// `external_id` on the client `Member` (listed in [`crate::mgmt::FEATURES`]).
pub const FEATURE_MEMBER_EXTERNAL_ID: &str = "member_external_id";

// ---------------------------------------------------------------- install roles and space members

unit_enum! {
    /// An install role the install owner may give.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum SettableInstallRole {
        /// Install admin.
        Admin = "admin",
        /// Regular member.
        Member = "member",
    }
}

/// Body of `POST /api/v1/install/members/{id}/role` (install owner only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetInstallRoleRequest {
    /// The new role.
    pub install_role: SettableInstallRole,
}

impl crate::error::Validate for SetInstallRoleRequest {
    fn validate(&self) -> Result<(), crate::error::ValidationError> {
        crate::error::check_known("install_role", self.install_role)
    }
}
