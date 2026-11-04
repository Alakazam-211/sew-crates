//! External identities.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::addr::Handle;
use crate::limits::EXTERNAL_ID_MAX_BYTES;

/// Who a member is outside Sew.
///
/// Wire forms: `k2:principal:<uuid>`, `k2:workspace:<uuid>`, `k2:connect:<uuid>`, `k2:owner`,
/// `local:<handle>`, `link:<channel_id>:<base32>` (a meeting link guest; never
/// accepted by mgmt provision or the K2 pass), `app:<app_id>` (an App's service owner,
/// made by Sew; never provisioned and never signs in), `k2:server` (K2's server
/// service owner, the owner of the server-governed Space; made by Sew, never provisioned, never
/// signs in). `<uuid>` is lowercase hyphenated 8-4-4-4-12 hex (K2 principal ids are
/// `Uuid::new_v4`; a Connect user's is the never-reused uuid K2 gives each Connect user).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ExternalId {
    /// A K2 person: `k2:principal:<uuid>`. Holds the uuid.
    K2Principal(String),
    /// A K2 agent (workspace): `k2:workspace:<uuid>`. Holds the uuid.
    K2Workspace(String),
    /// The K2 box owner: `k2:owner`.
    K2Owner,
    /// A K2 Connect user of the server: `k2:connect:<uuid>` (Connect
    /// users are members of their server's Space). A person, never an agent. Holds the uuid.
    K2Connect(String),
    /// A standalone local user: `local:<handle>`.
    Local(Handle),
    /// A meeting link guest: `link:<meeting_channel_id>:<16 base32 chars>`.
    /// Made only by `POST /api/v1/meetings/redeem`; mgmt provision and the K2 pass refuse it.
    Link {
        /// The meeting channel the guest is bound to.
        channel: crate::id::Id,
        /// 16 characters of RFC 4648 base32 (`A–Z2–7`, upper case): 10 random bytes.
        token: String,
    },
    /// An App's service owner, `app:<app_id>` (the only `app:` id in 0.6.0, always a
    /// person: the `members_app_service` CHECK). Sew makes it on first need, one per App; it
    /// owns the App's Spaces, never signs in (every `app:` pass is refused) and is hidden as a
    /// person.
    AppService {
        /// The App.
        app_id: String,
    },
    /// K2's server service owner, exactly `k2:server`, the sibling of
    /// `app:<app_id>`. It owns the server-governed Space (governance `server`), never signs in (a
    /// pass with this `sub` is refused `service_sub_refused`), is never provisioned and is hidden
    /// as a person; mgmt acts as it.
    K2Server,
}

/// The external id of K2's server service owner.
pub const K2_SERVER: &str = "k2:server";

/// The prefix of an App's service owner's external id.
pub const APP_SERVICE_PREFIX: &str = "app:";

/// Length of a link guest's random external id part, in base32 characters (10 bytes).
pub const LINK_GUEST_TOKEN_CHARS: usize = 16;

/// Whether `s` is 16 characters of upper-case RFC 4648 base32 (no padding).
pub fn is_link_token(s: &str) -> bool {
    s.len() == LINK_GUEST_TOKEN_CHARS
        && s.bytes()
            .all(|c| c.is_ascii_uppercase() || (b'2'..=b'7').contains(&c))
}

/// Error parsing an [`ExternalId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalIdError(String);

impl fmt::Display for ExternalIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid external id {:?}: expected k2:principal:<uuid>, k2:workspace:<uuid>, k2:connect:<uuid>, k2:owner, k2:server, local:<handle>, link:<channel_id>:<base32> or app:<app_id>",
            self.0
        )
    }
}

impl std::error::Error for ExternalIdError {}

/// Whether `s` is a lowercase hyphenated 8-4-4-4-12 hex uuid.
pub fn is_lower_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
        })
}

impl ExternalId {
    /// Whether this is an agent identity (`k2:workspace:*`).
    pub fn is_agent(&self) -> bool {
        matches!(self, ExternalId::K2Workspace(_))
    }

    /// Whether this is a meeting link guest (`link:*`).
    pub fn is_link(&self) -> bool {
        matches!(self, ExternalId::Link { .. })
    }

    /// A box user: the box owner (`k2:owner`) or an Admin (`k2:connect:<uuid>`).
    /// Box users are never members of App-governed Spaces and never sign in through an App.
    pub fn is_box_user(&self) -> bool {
        matches!(self, ExternalId::K2Owner | ExternalId::K2Connect(_))
    }

    /// The App of an App's service owner (`app:<app_id>`).
    pub fn service_app(&self) -> Option<&str> {
        match self {
            ExternalId::AppService { app_id } => Some(app_id),
            _ => None,
        }
    }

    /// Whether this is a service owner (`app:<app_id>` or
    /// `k2:server`): it owns Spaces, never signs in and is hidden as a person.
    pub fn is_service_owner(&self) -> bool {
        matches!(self, ExternalId::AppService { .. } | ExternalId::K2Server)
    }

    /// Never a member of the server-governed Space (governance `server`): App users
    /// (`k2:principal:`) and service owners of Apps (`app:`).
    pub fn is_app_user(&self) -> bool {
        matches!(
            self,
            ExternalId::K2Principal(_) | ExternalId::AppService { .. }
        )
    }

    /// The service owner of `app_id`.
    pub fn service_owner(app_id: &str) -> ExternalId {
        ExternalId::AppService {
            app_id: app_id.to_owned(),
        }
    }
}

impl FromStr for ExternalId {
    type Err = ExternalIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ExternalIdError(s.to_owned());
        if s.len() > EXTERNAL_ID_MAX_BYTES {
            return Err(err());
        }
        if s == "k2:owner" {
            return Ok(ExternalId::K2Owner);
        }
        if s == K2_SERVER {
            return Ok(ExternalId::K2Server);
        }
        if let Some(u) = s.strip_prefix("k2:principal:") {
            return if is_lower_uuid(u) {
                Ok(ExternalId::K2Principal(u.to_owned()))
            } else {
                Err(err())
            };
        }
        if let Some(u) = s.strip_prefix("k2:workspace:") {
            return if is_lower_uuid(u) {
                Ok(ExternalId::K2Workspace(u.to_owned()))
            } else {
                Err(err())
            };
        }
        if let Some(u) = s.strip_prefix("k2:connect:") {
            return if is_lower_uuid(u) {
                Ok(ExternalId::K2Connect(u.to_owned()))
            } else {
                Err(err())
            };
        }
        if let Some(h) = s.strip_prefix("local:") {
            return Handle::new(h).map(ExternalId::Local).map_err(|_| err());
        }
        if let Some(rest) = s.strip_prefix("link:") {
            let (c, t) = rest.split_once(':').ok_or_else(err)?;
            let channel: crate::id::Id = c.parse().map_err(|_| err())?;
            // Canonical form only: the id must print back as given.
            if channel.to_string() != c || !is_link_token(t) {
                return Err(err());
            }
            return Ok(ExternalId::Link {
                channel,
                token: t.to_owned(),
            });
        }
        if let Some(rest) = s.strip_prefix(APP_SERVICE_PREFIX) {
            // The App's service owner (an App id has no `:`).
            return if crate::mgmt::is_app_id(rest) {
                Ok(ExternalId::AppService {
                    app_id: rest.to_owned(),
                })
            } else {
                Err(err())
            };
        }
        Err(err())
    }
}

impl fmt::Display for ExternalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExternalId::K2Principal(u) => write!(f, "k2:principal:{u}"),
            ExternalId::K2Workspace(u) => write!(f, "k2:workspace:{u}"),
            ExternalId::K2Owner => f.write_str("k2:owner"),
            ExternalId::K2Connect(u) => write!(f, "k2:connect:{u}"),
            ExternalId::Local(h) => write!(f, "local:{h}"),
            ExternalId::Link { channel, token } => write!(f, "link:{channel}:{token}"),
            ExternalId::AppService { app_id } => write!(f, "app:{app_id}"),
            ExternalId::K2Server => f.write_str(K2_SERVER),
        }
    }
}

impl Serialize for ExternalId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ExternalId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = Cow::<'de, str>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for ExternalId {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "ExternalId".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "maxLength": 128,
            "pattern": "^(k2:(principal|workspace|connect):[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|k2:owner|k2:server|local:[a-z0-9_-]{2,64}|link:[1-9][0-9]{0,18}:[A-Z2-7]{16}|app:[a-z0-9][a-z0-9._-]{0,63})$",
            "description": "An external identity: k2:principal:<uuid>, k2:workspace:<uuid>, k2:connect:<uuid>, k2:owner, k2:server (K2's server service owner), local:<handle>, link:<channel_id>:<base32> (a meeting link guest) or app:<app_id> (an App's service owner)."
        })
    }
}
