//! Per-App quotas: limits for each App, keyed on the Space binding's `app_id`.
//!
//! - `POST /mgmt/v1/apps/{app_id}/quotas` ([`AppQuotas`] → [`AppQuotasResponse`]) sets the
//!   App's quotas: the body is the whole set, so a quota left out (or `null`) is unset. Unset
//!   means no App limit; the install-wide limits always apply as well. A repeat with the same
//!   body changes nothing (no audit row); a change writes one `app.quotas_set` row with the
//!   old and new values and the mgmt peer uid. An App needs no Space to have quotas.
//! - `GET /mgmt/v1/apps/{app_id}/quotas` answers the same [`AppQuotasResponse`]: the quotas
//!   and the current usage against each one ([`AppQuotaUsage`]), for an App with no quotas
//!   too.
//!
//! A limit is checked where it is crossed. A count limit answers `422 quota_exceeded` and a
//! rate `429 quota_exceeded` (with `retry_after_ms` and `Retry-After`), both with
//! [`AppQuotaDetails`]. A batch add over mgmt never fails on a quota: the members past it are
//! in `not_added` with reason `quota_exceeded`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationError};

/// `spaces_max`: Spaces bound to the App (checked at a bound create and at a bind).
pub const QUOTA_SPACES_MAX: &str = "spaces_max";
/// `channels_per_space_max`: channels in each App Space (the client and mgmt creates).
pub const QUOTA_CHANNELS_PER_SPACE_MAX: &str = "channels_per_space_max";
/// `members_per_space_max`: members in each App Space (every way into the Space).
pub const QUOTA_MEMBERS_PER_SPACE_MAX: &str = "members_per_space_max";
/// `file_bytes_max`: file bytes in the App's Spaces (checked on each new upload).
pub const QUOTA_FILE_BYTES_MAX: &str = "file_bytes_max";
/// `call_participants_max`: concurrent call participants across the App's Spaces (joins).
pub const QUOTA_CALL_PARTICIPANTS_MAX: &str = "call_participants_max";
/// `messages_per_min`: posts per minute across the App's Spaces, a token bucket per App.
pub const QUOTA_MESSAGES_PER_MIN: &str = "messages_per_min";

/// Every quota name, in wire order.
pub const APP_QUOTAS: [&str; 6] = [
    QUOTA_SPACES_MAX,
    QUOTA_CHANNELS_PER_SPACE_MAX,
    QUOTA_MEMBERS_PER_SPACE_MAX,
    QUOTA_FILE_BYTES_MAX,
    QUOTA_CALL_PARTICIPANTS_MAX,
    QUOTA_MESSAGES_PER_MIN,
];

/// The highest count limit (Spaces, channels, members, participants).
pub const APP_QUOTA_COUNT_MAX: u32 = 1_000_000_000;
/// The highest `messages_per_min`.
pub const APP_QUOTA_MESSAGES_PER_MIN_MAX: u32 = 1_000_000;
/// The highest `file_bytes_max` (2^62 bytes).
pub const APP_QUOTA_FILE_BYTES_MAX: u64 = 1 << 62;

/// An App's quotas: the body of `POST /mgmt/v1/apps/{app_id}/quotas` and `quotas` in its
/// answer. Each is optional; absent means no App limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppQuotas {
    /// Spaces bound to the App, 0–1e9.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spaces_max: Option<u32>,
    /// Channels in each App Space, 0–1e9.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels_per_space_max: Option<u32>,
    /// Members in each App Space (the owner included), 0–1e9.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members_per_space_max: Option<u32>,
    /// File bytes in the App's Spaces (what the Space file quota counts: uploads, attachments,
    /// emoji and Space icons), 0–2^62; checked on each new upload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_bytes_max: Option<u64>,
    /// Concurrent call participants across the App's Spaces, 0–1e9.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_participants_max: Option<u32>,
    /// Posts per minute across the App's Spaces (posts, replies, forwards and scheduled sends
    /// when they fire), 1–1e6: a token bucket of this size that refills over a minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages_per_min: Option<u32>,
}

impl AppQuotas {
    /// No quota set.
    pub fn is_empty(&self) -> bool {
        *self == AppQuotas::default()
    }

    /// The quota named `name` ([`APP_QUOTAS`]), if set.
    pub fn get(&self, name: &str) -> Option<u64> {
        match name {
            QUOTA_SPACES_MAX => self.spaces_max.map(u64::from),
            QUOTA_CHANNELS_PER_SPACE_MAX => self.channels_per_space_max.map(u64::from),
            QUOTA_MEMBERS_PER_SPACE_MAX => self.members_per_space_max.map(u64::from),
            QUOTA_FILE_BYTES_MAX => self.file_bytes_max,
            QUOTA_CALL_PARTICIPANTS_MAX => self.call_participants_max.map(u64::from),
            QUOTA_MESSAGES_PER_MIN => self.messages_per_min.map(u64::from),
            _ => None,
        }
    }
}

impl Validate for AppQuotas {
    fn validate(&self) -> Result<(), ValidationError> {
        for (field, v) in [
            (QUOTA_SPACES_MAX, self.spaces_max),
            (QUOTA_CHANNELS_PER_SPACE_MAX, self.channels_per_space_max),
            (QUOTA_MEMBERS_PER_SPACE_MAX, self.members_per_space_max),
            (QUOTA_CALL_PARTICIPANTS_MAX, self.call_participants_max),
        ] {
            if v.is_some_and(|v| v > APP_QUOTA_COUNT_MAX) {
                return Err(ValidationError::invalid(
                    field,
                    format!("at most {APP_QUOTA_COUNT_MAX}"),
                ));
            }
        }
        if self
            .file_bytes_max
            .is_some_and(|v| v > APP_QUOTA_FILE_BYTES_MAX)
        {
            return Err(ValidationError::invalid(
                QUOTA_FILE_BYTES_MAX,
                "at most 2^62 bytes",
            ));
        }
        if let Some(m) = self.messages_per_min
            && !(1..=APP_QUOTA_MESSAGES_PER_MIN_MAX).contains(&m)
        {
            return Err(ValidationError::invalid(
                QUOTA_MESSAGES_PER_MIN,
                format!("1–{APP_QUOTA_MESSAGES_PER_MIN_MAX} posts a minute"),
            ));
        }
        Ok(())
    }
}

/// The App's usage against each quota (named as the quotas; present whether or not the quota
/// is set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct AppQuotaUsage {
    /// Spaces bound to the App.
    pub spaces_max: u64,
    /// Channels in the App's fullest Space (archived ones count).
    pub channels_per_space_max: u64,
    /// Members of the App's fullest Space.
    pub members_per_space_max: u64,
    /// File bytes in the App's Spaces.
    pub file_bytes_max: u64,
    /// Members in a call in one of the App's Spaces now.
    pub call_participants_max: u64,
    /// Posts the App's bucket holds back now: `messages_per_min` minus the tokens left (0
    /// without the quota).
    pub messages_per_min: u64,
}

/// Answer of `GET` and `POST /mgmt/v1/apps/{app_id}/quotas`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AppQuotasResponse {
    /// The App.
    pub app_id: String,
    /// The quotas set (absent: no App limit).
    pub quotas: AppQuotas,
    /// The current usage against each quota.
    pub used: AppQuotaUsage,
    /// `POST` only: whether this call changed the quotas (`false` on a repeat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<bool>,
}

/// `details` of an App quota's `422` / `429 quota_exceeded`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AppQuotaDetails {
    /// The quota ([`APP_QUOTAS`]).
    pub quota: String,
    /// Its limit.
    pub limit: u64,
    /// The usage the request found (for a per-Space quota, that Space's).
    pub used: u64,
    /// The App.
    pub app_id: String,
    /// `messages_per_min` only (429): milliseconds until a post can go through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_bounds() {
        assert!(AppQuotas::default().validate().is_ok());
        let ok = AppQuotas {
            spaces_max: Some(0),
            messages_per_min: Some(1),
            file_bytes_max: Some(APP_QUOTA_FILE_BYTES_MAX),
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
        let zero_rate = AppQuotas {
            messages_per_min: Some(0),
            ..Default::default()
        };
        assert_eq!(
            zero_rate.validate().unwrap_err().field,
            QUOTA_MESSAGES_PER_MIN
        );
        let big = AppQuotas {
            members_per_space_max: Some(APP_QUOTA_COUNT_MAX + 1),
            ..Default::default()
        };
        assert_eq!(
            big.validate().unwrap_err().field,
            QUOTA_MEMBERS_PER_SPACE_MAX
        );
    }

    #[test]
    fn get_by_name() {
        let q = AppQuotas {
            channels_per_space_max: Some(3),
            file_bytes_max: Some(9),
            ..Default::default()
        };
        assert_eq!(q.get(QUOTA_CHANNELS_PER_SPACE_MAX), Some(3));
        assert_eq!(q.get(QUOTA_FILE_BYTES_MAX), Some(9));
        assert_eq!(q.get(QUOTA_SPACES_MAX), None);
        assert_eq!(q.get("nope"), None);
    }
}
