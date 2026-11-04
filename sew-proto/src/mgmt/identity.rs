//! Moving a standalone install to K2 identity over mgmt.
//!
//! - `POST /mgmt/v1/identity/switch {to: "k2", confirm: true}` switches the install's identity
//!   mode from standalone to K2, once and one way. It needs `[identity.k2] jwks_url` in the
//!   running config and no upgrade in progress. The stored mode changes at once; the running
//!   process keeps serving standalone until it restarts (`restart_required`). After the restart
//!   K2 passes are accepted, and password sign-in stays on only for members still on a
//!   `local:` id (the transition state, `status.identity.unlinked_local_members`).
//! - `POST /mgmt/v1/principals/relink {from: "local:<handle>", to: "k2:principal:<uuid>"}`
//!   gives a local member a K2 identity and keeps its member id, so its messages, roles,
//!   overwrites, DMs, files, bans and timeouts stay. Its password, setup links and sessions are
//!   dropped. Only on an install whose mode is K2 (after the switch); the same pair again is
//!   `200` with `changed: false`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ProviderKind;
use crate::error::{Validate, ValidationError};
use crate::external_id::ExternalId;
use crate::id::Id;
use crate::time::Timestamp;

/// The feature flag of the standalone → K2 identity move (`principals/relink`,
/// `identity/switch`). Listed in [`super::FEATURES`].
pub const FEATURE_PRINCIPAL_RELINK: &str = "principal_relink";

/// Whether `e` is a K2 person id a local member can be relinked to: `k2:principal:<uuid>`,
/// `k2:owner` or `k2:connect:<uuid>` (never an agent).
pub fn is_relink_target(e: &ExternalId) -> bool {
    matches!(
        e,
        ExternalId::K2Principal(_) | ExternalId::K2Owner | ExternalId::K2Connect(_)
    )
}

/// Body of `POST /mgmt/v1/principals/relink`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrincipalRelinkRequest {
    /// The member's current id; must be `local:<handle>`.
    pub from: ExternalId,
    /// Its K2 id: `k2:principal:<uuid>` (or `k2:owner`, `k2:connect:<uuid>`).
    pub to: ExternalId,
}

impl Validate for PrincipalRelinkRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if !matches!(self.from, ExternalId::Local(_)) {
            return Err(ValidationError::invalid(
                "from",
                "from must be a local user: local:<handle>",
            ));
        }
        if !is_relink_target(&self.to) {
            return Err(ValidationError::invalid(
                "to",
                "to must be a K2 person: k2:principal:<uuid>, k2:owner or k2:connect:<uuid>",
            ));
        }
        Ok(())
    }
}

/// Answer of `POST /mgmt/v1/principals/relink`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PrincipalRelinkResponse {
    /// The member (unchanged by the relink).
    pub member_id: Id,
    /// The old id.
    pub from: ExternalId,
    /// The new id.
    pub to: ExternalId,
    /// `false` when the same pair was relinked before (nothing changed).
    pub changed: bool,
    /// Sessions this call revoked.
    pub sessions_revoked: u64,
}

/// Body of `POST /mgmt/v1/identity/switch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IdentitySwitchRequest {
    /// The new mode; only `k2`.
    pub to: ProviderKind,
    /// Must be `true`: the switch cannot be undone.
    pub confirm: bool,
}

impl Validate for IdentitySwitchRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.to != ProviderKind::K2 {
            return Err(ValidationError::invalid(
                "to",
                "an install switches only from standalone to k2",
            ));
        }
        if !self.confirm {
            return Err(ValidationError::invalid(
                "confirm",
                "the switch cannot be undone; send confirm: true",
            ));
        }
        Ok(())
    }
}

/// Answer of `POST /mgmt/v1/identity/switch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IdentitySwitchResponse {
    /// The install's identity mode now (`k2`).
    pub mode: ProviderKind,
    /// When the switch happened.
    pub switched_at: Timestamp,
    /// `true`: the running process still serves standalone until Sew restarts.
    pub restart_required: bool,
    /// Active members still on a `local:` id (to relink, or deactivate).
    pub unlinked_local_members: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::Handle;

    #[test]
    fn features_sorted_unique_and_listed() {
        let mut v = super::super::FEATURES.to_vec();
        v.sort_unstable();
        v.dedup();
        assert_eq!(v, super::super::FEATURES);
        assert!(super::super::FEATURES.contains(&FEATURE_PRINCIPAL_RELINK));
    }

    #[test]
    fn relink_validation() {
        let local = ExternalId::Local(Handle::new("bob").expect("handle"));
        let k2 = ExternalId::K2Principal("8f14e45f-ceea-4e7a-9c3b-0123456789ab".into());
        let ok = PrincipalRelinkRequest {
            from: local.clone(),
            to: k2.clone(),
        };
        assert!(ok.validate().is_ok());
        let bad_from = PrincipalRelinkRequest {
            from: k2.clone(),
            to: k2.clone(),
        };
        assert_eq!(bad_from.validate().map_err(|e| e.field), Err("from".into()));
        let bad_to = PrincipalRelinkRequest {
            from: local.clone(),
            to: ExternalId::K2Workspace("8f14e45f-ceea-4e7a-9c3b-0123456789ab".into()),
        };
        assert_eq!(bad_to.validate().map_err(|e| e.field), Err("to".into()));
        let local_to = PrincipalRelinkRequest {
            from: local.clone(),
            to: local,
        };
        assert_eq!(local_to.validate().map_err(|e| e.field), Err("to".into()));
    }

    #[test]
    fn switch_validation() {
        let ok = IdentitySwitchRequest {
            to: ProviderKind::K2,
            confirm: true,
        };
        assert!(ok.validate().is_ok());
        let back = IdentitySwitchRequest {
            to: ProviderKind::Standalone,
            confirm: true,
        };
        assert_eq!(back.validate().map_err(|e| e.field), Err("to".into()));
        let unconfirmed = IdentitySwitchRequest {
            to: ProviderKind::K2,
            confirm: false,
        };
        assert_eq!(
            unconfirmed.validate().map_err(|e| e.field),
            Err("confirm".into())
        );
    }
}
