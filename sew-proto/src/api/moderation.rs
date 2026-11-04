//! Timeout and slow-mode bodies; space bans.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationError};
use crate::http::Page;
use crate::id::Id;
use crate::limits::{
    BAN_DELETE_WINDOW_MAX_S, BAN_REASON_MAX_CHARS, SLOW_MODE_MAX_S, TIMEOUT_MAX_MS, TIMEOUT_MIN_MS,
};
use crate::time::Timestamp;

/// Body of `POST /api/v1/spaces/{id}/members/{member_id}/timeout`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimeoutRequest {
    /// 60,000 – 2,419,200,000 ms (1 min – 28 days).
    pub duration_ms: u64,
}

impl Validate for TimeoutRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if !(TIMEOUT_MIN_MS..=TIMEOUT_MAX_MS).contains(&self.duration_ms) {
            return Err(ValidationError::invalid(
                "duration_ms",
                format!("duration_ms must be {TIMEOUT_MIN_MS}–{TIMEOUT_MAX_MS}"),
            ));
        }
        Ok(())
    }
}

/// Body of `POST /api/v1/channels/{id}/slow_mode/set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SlowModeRequest {
    /// Seconds between posts per member, 0 – 21,600; 0 turns it off.
    pub slow_mode_s: u32,
}

impl Validate for SlowModeRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.slow_mode_s > SLOW_MODE_MAX_S {
            return Err(ValidationError::invalid(
                "slow_mode_s",
                format!("slow_mode_s must be 0–{SLOW_MODE_MAX_S}"),
            ));
        }
        Ok(())
    }
}

/// Body of `POST /api/v1/spaces/{id}/bans`. The target may be a non-member
/// (a pre-emptive ban). Banning someone already banned answers the existing ban and writes
/// nothing (to change the reason, unban and ban again).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BanRequest {
    /// The member to ban.
    pub member_id: Id,
    /// Why, 1–512 characters, no control characters except line breaks. Sealed at rest, shown
    /// only to BAN_MEMBERS holders and install owners and admins, never in audit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Delete the member's messages in the space from the last this many seconds, 0 (default)
    /// – 604,800 (7 days).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_window_s: Option<u32>,
}

impl Validate for BanRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(r) = &self.reason {
            let n = r.chars().count();
            if n == 0 || n > BAN_REASON_MAX_CHARS {
                return Err(ValidationError::invalid(
                    "reason",
                    format!("reason must be 1–{BAN_REASON_MAX_CHARS} characters, got {n}"),
                ));
            }
            if r.chars().any(|c| c.is_control() && c != '\n' && c != '\r') {
                return Err(ValidationError::invalid(
                    "reason",
                    "reason holds a control character other than a line break",
                ));
            }
        }
        if let Some(w) = self.delete_window_s
            && w > BAN_DELETE_WINDOW_MAX_S
        {
            return Err(ValidationError::invalid(
                "delete_window_s",
                format!("delete_window_s must be 0–{BAN_DELETE_WINDOW_MAX_S}"),
            ));
        }
        Ok(())
    }
}

/// One space ban (`POST /api/v1/spaces/{id}/bans`, `GET …/bans`, `GET …/bans/{member_id}`).
/// Readable only by BAN_MEMBERS holders and install owners and admins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SpaceBan {
    /// The space.
    pub space_id: Id,
    /// The banned member.
    pub member_id: Id,
    /// When.
    pub banned_at: Timestamp,
    /// Who banned them (absent for a ban restored without one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banned_by: Option<Id>,
    /// The reason, when one was given (and its key is not shredded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The recent-message delete window chosen at ban time, seconds.
    pub delete_window_s: u32,
}

/// A page of a space's bans, ascending by member id (`GET /api/v1/spaces/{id}/bans`).
pub type SpaceBanPage = Page<SpaceBan>;
