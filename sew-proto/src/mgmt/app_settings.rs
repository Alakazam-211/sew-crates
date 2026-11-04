//! Apps over mgmt (K2 tells Sew about the Apps on the box).
//!
//! - `GET`/`POST /mgmt/v1/home-client` ([`MgmtHomeClientRequest`] → [`MgmtHomeClient`]): the
//!   install's home client, one App id or none (`{"app_id": null}`). A session whose pass names
//!   the home client sees every Space the member belongs to except the Spaces of Apps flagged
//!   `hidden_from_home`, and every DM.
//! - `GET`/`POST /mgmt/v1/apps/{app_id}/settings` ([`MgmtAppSettingsRequest`] →
//!   [`MgmtAppSettings`]): one App's settings. A `POST` sets any subset of the fields; an
//!   absent field keeps its value and `null` clears `display_name`, `origin` or `origins`
//!   (`[]` clears `origins` too). An App Sew was
//!   never told about reads as the defaults (`hidden_from_home: false`, no name, no origin,
//!   `max_level: moderator`, `uploads: true`). The home client cannot govern, so
//!   `max_level` on it is `409 conflict` `home_client_cannot_govern`, as is a home client that
//!   names an App governing a Space.
//!
//! Both writes are idempotent (the same values again change nothing and audit nothing) and
//! audited (`home_client.set`, `app.settings_set`). A change takes effect at once for live
//! sessions: their gateway sessions are closed with `4000` (`scope_changed`) so the client
//! resumes, is told the session is gone, and IDENTIFYs for a fresh READY.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::spaces::check_app_id;
use crate::error::{Validate, ValidationError, check_chars};
use crate::http::Patch;
use crate::limits::NAME_MAX_CHARS;

/// Longest App origin, bytes.
pub const APP_ORIGIN_MAX_BYTES: usize = 255;

/// The feature flag of App-scoped sessions (the pass's `app` claim).
pub const FEATURE_APP_SCOPED_SESSIONS: &str = "app_scoped_sessions";
/// The feature flag of the home client (`/mgmt/v1/home-client`, `hidden_from_home`).
pub const FEATURE_HOME_CLIENT: &str = "home_client";
/// The feature flag of per-App signalling origins (`origin` in the App settings).
pub const FEATURE_APP_ORIGIN: &str = "app_origin";
/// The feature flag of an App's further web origins (since 0.7.2): `origins` in the App
/// settings, and the optional `origin` of a voice or call join (`422 origin_not_listed`).
pub const FEATURE_APP_ORIGINS: &str = "app_origins";

/// Most further origins one App may list (`origins`).
pub const APP_ORIGINS_MAX: usize = 32;

/// `origins` as stored and answered: sorted, without duplicates.
pub fn canonical_origins(origins: &[String]) -> Vec<String> {
    let mut v = origins.to_vec();
    v.sort_unstable();
    v.dedup();
    v
}

/// Whether `s` is an App origin Sew accepts: `wss://<host>[:port]`, lower-case, no path, query,
/// user info or fragment, at most 255 bytes. The voice and call join answers append
/// `/rtc/v1`-style paths themselves, so the origin stays a bare origin.
pub fn is_app_origin(s: &str) -> bool {
    if s.len() > APP_ORIGIN_MAX_BYTES {
        return false;
    }
    let Some(rest) = s.strip_prefix("wss://") else {
        return false;
    };
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (rest, None),
    };
    let host_ok = !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
    let port_ok = port.is_none_or(|p| {
        !p.is_empty()
            && p.len() <= 5
            && !p.starts_with('0')
            && p.bytes().all(|b| b.is_ascii_digit())
            && p.parse::<u32>().is_ok_and(|n| (1..=65_535).contains(&n))
    });
    host_ok && port_ok
}

/// Body of `POST /mgmt/v1/home-client`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtHomeClientRequest {
    /// The App that is the install's home client; `null` for none.
    pub app_id: Option<String>,
    /// The current install owner's approval, as `owner/set` takes it (flag
    /// `home_client_owner_approval`): required on every write, a repeat that
    /// changes nothing included. Absent or bad: `403 owner_approval_required`, nothing
    /// changes, and the refusal is audited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<super::OwnerApproval>,
}

impl Validate for MgmtHomeClientRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        match &self.app_id {
            Some(a) => check_app_id("app_id", a),
            None => Ok(()),
        }
    }
}

/// `GET`/`POST /mgmt/v1/home-client`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtHomeClient {
    /// The home client's App id; `null` for none.
    pub app_id: Option<String>,
}

/// Body of `POST /mgmt/v1/apps/{app_id}/settings`: any subset of the fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MgmtAppSettingsRequest {
    /// Leave the App's Spaces out of the home client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_from_home: Option<bool>,
    /// The App's name as clients label its Spaces (`Space.app_display_name`), 1–100
    /// characters; `null` clears it.
    #[serde(default, skip_serializing_if = "Patch::is_keep")]
    pub display_name: Patch<String>,
    /// The App's public origin, `wss://<host>[:port]`: the signalling URL of the voice and
    /// call join answers of the App's sessions; `null` clears it.
    #[serde(default, skip_serializing_if = "Patch::is_keep")]
    pub origin: Patch<String>,
    /// The highest level K2's sync may give in the App's governed Spaces (default
    /// `moderator`). Refused for the home client (`409 home_client_cannot_govern`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_level: Option<crate::api::spaces::SpaceLevel>,
    /// Whether the App's people may upload files in its governed Spaces (default
    /// `true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uploads: Option<bool>,
    /// Flag `app_origins`: the App's further web origins, each `wss://<host>[:port]`
    /// as `origin`, at most 32: a voice or call join of the App's sessions may name `origin` or
    /// one of these as its own `origin` (the host the browser reached the App on). Duplicates
    /// are dropped and the list is kept sorted; `null` or `[]` clears it.
    #[serde(default, skip_serializing_if = "Patch::is_keep")]
    pub origins: Patch<Vec<String>>,
}

impl Validate for MgmtAppSettingsRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(l) = self.max_level {
            crate::error::check_known("max_level", l)?;
        }
        if let Patch::Set(n) = &self.display_name {
            check_chars("display_name", n, 1, NAME_MAX_CHARS)?;
        }
        if let Patch::Set(o) = &self.origin
            && !is_app_origin(o)
        {
            return Err(ValidationError::invalid(
                "origin",
                "an App origin is wss://<host>[:port] in lower case, without a path",
            ));
        }
        if let Patch::Set(list) = &self.origins {
            if list.len() > APP_ORIGINS_MAX {
                return Err(ValidationError::invalid(
                    "origins",
                    "an App lists at most 32 further origins",
                ));
            }
            if list.iter().any(|o| !is_app_origin(o)) {
                return Err(ValidationError::invalid(
                    "origins",
                    "each App origin is wss://<host>[:port] in lower case, without a path",
                ));
            }
        }
        Ok(())
    }
}

/// `GET`/`POST /mgmt/v1/apps/{app_id}/settings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MgmtAppSettings {
    /// The App.
    pub app_id: String,
    /// The App's Spaces are left out of the home client.
    pub hidden_from_home: bool,
    /// The App's display name; absent when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The App's signalling origin; absent when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// The highest level K2's sync may give in the App's governed Spaces. Old
    /// payloads read as `moderator`.
    #[serde(default = "default_max_level")]
    pub max_level: crate::api::spaces::SpaceLevel,
    /// Whether the App's people may upload files in its governed Spaces. Old payloads
    /// read as `true`.
    #[serde(default = "default_uploads")]
    pub uploads: bool,
    /// Flag `app_origins`: the App's further web origins, sorted; absent when none
    /// (old payloads read as none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub origins: Vec<String>,
}

/// The default `max_level`.
pub const fn default_max_level() -> crate::api::spaces::SpaceLevel {
    crate::api::spaces::SpaceLevel::Moderator
}

const fn default_uploads() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        for ok in [
            "wss://jobs.example.com",
            "wss://app.example.com:8443",
            "wss://a",
            "wss://x-1.example.com:443",
        ] {
            assert!(is_app_origin(ok), "{ok}");
        }
        for bad in [
            "ws://jobs.example.com",
            "https://jobs.example.com",
            "wss://Jobs.example.com",
            "wss://jobs.example.com/",
            "wss://jobs.example.com/rtc",
            "wss://user@jobs.example.com",
            "wss://jobs.example.com:0",
            "wss://jobs.example.com:65536",
            "wss://jobs.example.com:",
            "wss://",
            "wss://-a.com",
            "wss://a..com",
            "wss://jobs.example.com?x=1",
        ] {
            assert!(!is_app_origin(bad), "{bad}");
        }
    }

    #[test]
    fn features_sorted_unique() {
        let mut v = crate::mgmt::FEATURES.to_vec();
        v.sort_unstable();
        v.dedup();
        assert_eq!(v, crate::mgmt::FEATURES);
    }
}
