//! The member-channels sync (since Sew 0.6.0). K2 states, per person, the full list of
//! private channels they are in and their level in an App-governed Space; Sew applies the
//! difference.
//!
//! - `POST /mgmt/v1/spaces/{space_id}/members/{external_id}/channels`
//!   ([`MgmtMemberChannelsRequest`] → [`MgmtMemberChannelsResponse`]).
//! - `POST /mgmt/v1/spaces/{space_id}/members/channels` ([`MgmtMemberChannelsBulkRequest`] →
//!   [`MgmtMemberChannelsBulkResponse`]): 1–500 items, each its own transaction and answered
//!   on its own (one bad item fails only that item).
//!
//! `app_id` must name the Space's governing App (`409 conflict` `app_mismatch`), and the Space
//! must be App-governed (`409 conflict` `not_app_governed`). The sync never creates members:
//! an unknown person is `422 invalid_state` `member_not_provisioned`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::spaces::SpaceLevel;
use crate::error::{ApiError, Validate, ValidationError, check_known};
use crate::external_id::ExternalId;
use crate::id::Id;

use super::spaces::check_app_id;

/// The feature flag of the member-channels sync. Listed in [`super::FEATURES`].
pub const FEATURE_APP_MEMBER_CHANNELS: &str = "app_member_channels";

/// Most channel ids in one sync item (the person's full list of private channels).
pub const MEMBER_CHANNELS_MAX: usize = 5_000;
/// Most items in one bulk sync.
pub const MEMBER_CHANNELS_ITEMS_MAX: usize = 500;

fn check_channels(field: &str, ids: &[Id]) -> Result<(), ValidationError> {
    if ids.len() > MEMBER_CHANNELS_MAX {
        return Err(ValidationError::invalid(
            field,
            format!("0–{MEMBER_CHANNELS_MAX} channel ids"),
        ));
    }
    let mut seen = std::collections::HashSet::with_capacity(ids.len());
    for (i, c) in ids.iter().enumerate() {
        if !seen.insert(*c) {
            return Err(ValidationError::invalid(
                format!("{field}.{i}"),
                "listed twice",
            ));
        }
    }
    Ok(())
}

/// Body of `POST /mgmt/v1/spaces/{space_id}/members/{external_id}/channels`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtMemberChannelsRequest {
    /// The Space's governing App; absent for the server-governed Space (governance
    /// `server`), where levels stop at `moderator`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// The person's full list of private channels in the Space, 0–5,000 distinct ids. Public
    /// channels are open to every member and are not listed (a listed one comes back in
    /// `unknown_channel_ids`).
    pub channel_ids: Vec<Id>,
    /// The person's level: `member` (`@everyone`), `moderator` or `admin` (the managed roles),
    /// at most the App's `max_level`.
    pub level: SpaceLevel,
}

impl Validate for MgmtMemberChannelsRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(a) = &self.app_id {
            check_app_id("app_id", a)?;
        }
        check_known("level", self.level)?;
        check_channels("channel_ids", &self.channel_ids)
    }
}

/// One item of a bulk sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtMemberChannelsItem {
    /// The person.
    pub external_id: ExternalId,
    /// As [`MgmtMemberChannelsRequest::channel_ids`].
    pub channel_ids: Vec<Id>,
    /// As [`MgmtMemberChannelsRequest::level`].
    pub level: SpaceLevel,
}

/// Body of `POST /mgmt/v1/spaces/{space_id}/members/channels`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtMemberChannelsBulkRequest {
    /// The Space's governing App; absent for the server-governed Space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// 1–500 items, distinct people.
    pub items: Vec<MgmtMemberChannelsItem>,
}

impl Validate for MgmtMemberChannelsBulkRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(a) = &self.app_id {
            check_app_id("app_id", a)?;
        }
        if self.items.is_empty() || self.items.len() > MEMBER_CHANNELS_ITEMS_MAX {
            return Err(ValidationError::invalid(
                "items",
                format!("1–{MEMBER_CHANNELS_ITEMS_MAX} items"),
            ));
        }
        let mut seen = std::collections::HashSet::with_capacity(self.items.len());
        for (i, it) in self.items.iter().enumerate() {
            if !seen.insert(&it.external_id) {
                return Err(ValidationError::invalid(
                    format!("items.{i}.external_id"),
                    "listed twice",
                ));
            }
            check_known(&format!("items.{i}.level"), it.level)?;
            check_channels(&format!("items.{i}.channel_ids"), &it.channel_ids)?;
        }
        Ok(())
    }
}

unit_enum! {
    /// Why a sync item changed nothing for its person.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    pub enum MgmtMemberChannelsSkip {
        /// The person is the Space's owner (the App's service owner): the sync never removes,
        /// demotes or replaces the owner, who is in every channel.
        Owner = "owner",
        /// The person was deprovisioned; only `principals/provision` brings them back.
        Deprovisioned = "deprovisioned",
        /// The Space bans the person; bans win over the sync.
        Banned = "banned",
        /// The Space is at its App's `members_per_space_max`, so the person could not join.
        QuotaExceeded = "quota_exceeded",
    }
}

/// Answer of one sync item. Channel lists are ascending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtMemberChannelsResponse {
    /// The person.
    pub external_id: ExternalId,
    /// Their member id.
    pub member_id: Id,
    /// Channels the person joined.
    pub added: Vec<Id>,
    /// Channels the person left.
    pub removed: Vec<Id>,
    /// Listed channels the person was in already.
    pub unchanged: Vec<Id>,
    /// The person's level after the sync (absent when `skipped`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<SpaceLevel>,
    /// Listed ids that are not live private channels of this Space (unknown, in another
    /// Space, archived, public or a meeting's channel); the rest of the list applied.
    pub unknown_channel_ids: Vec<Id>,
    /// Set when the item changed nothing for its person, with why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<MgmtMemberChannelsSkip>,
    /// The item has more than 1,000 channel changes and runs as a job: `added`, `removed`
    /// and `unchanged` are empty, `job_id` names the job (`GET /mgmt/v1/jobs/{id}`). A later
    /// sync of the same person replaces a job that has not run.
    #[serde(default)]
    pub queued: bool,
    /// The job of a `queued` item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<Id>,
}

/// One item's answer in a bulk sync: `answer` on success, `error` (the error body a single
/// sync would answer) otherwise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtMemberChannelsItemResult {
    /// The item's person.
    pub external_id: ExternalId,
    /// The HTTP status the single sync would answer (`200`, `409`, `422`, …).
    pub status: u16,
    /// The answer (status `200`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<MgmtMemberChannelsResponse>,
    /// The error (any other status).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

/// Answer of `POST /mgmt/v1/spaces/{space_id}/members/channels`, in request order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtMemberChannelsBulkResponse {
    /// One result per item.
    pub items: Vec<MgmtMemberChannelsItemResult>,
}
