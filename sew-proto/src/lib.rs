#![deny(missing_docs)]
//! Sew wire contracts.
//!
//! Every wire type lives here: ids, chat addresses, errors, HTTP request and response bodies,
//! gateway frames and events, change-log kinds, mgmt API bodies, permission bits, org policies
//! and audit events. Server, clients and K2 share this crate; nothing else defines wire JSON.
//!
//! The crate is pure: it reads the clock (only in [`clock`]) and does no other IO. It builds on
//! macOS as well as Linux.

#[macro_use]
mod macros;

pub mod addr;
pub mod api;
pub mod audit;
pub mod calls;
pub mod changes;
pub mod clock;
pub mod e2ee;
pub mod emoji_token;
pub mod error;
pub mod export;
pub mod external_id;
pub mod gateway;
pub mod http;
pub mod id;
pub mod install;
pub mod limits;
pub mod links;
pub mod mention;
pub mod mgmt;
pub mod pass;
pub mod perms;
pub mod policy;
pub mod recording;
pub mod registry;
pub mod routes;
pub mod search;
pub mod time;
pub mod token;
pub mod voice;
pub mod when;

mod examples;

/// Client API and gateway protocol version. Changes only with a breaking change after the
/// first release.
pub const PROTOCOL_VERSION: u32 = 1;

/// Logical export archive format version carried in export manifests.
pub const EXPORT_FORMAT: u32 = 1;

/// Additive revision of export format 1. Readers refuse archives with
/// a higher minor (`format_too_new`); a breaking change bumps [`EXPORT_FORMAT`]. Minor 1:
/// `spaces.ndjson` records carry the space's default channels, description, rules
/// and welcome channels. Minor 2: they also carry the App a space belongs to. Minor 3:
/// and whether a space is hidden. Minor 4: `space_bans.ndjson` and
/// `space_timeout_carry.ndjson`; importing an older minor gives every role holding
/// REMOVE_MEMBERS (bit 38) BAN_MEMBERS (bit 47), as the migration did. Minor 5: message
/// records of on-behalf posts carry `on_behalf_approval`; an older archive's on-behalf posts
/// import as pending. Minor 6: space records carry `private`; a private space imports
/// invite-only. Minor 7: space records carry `orphaned_from`, member records
/// `relinked_from` and the install record `identity_switched_at`. Minor 8 (Sew 0.6.0):
/// space records carry `governance` and role records the managed-role mark `managed`. Minor 9
/// (Sew 0.7.0): space records may carry governance `server` (the server-governed Space,
/// owned by the `k2:server` member) and channel records `closing`. Minor 10 (Sew 0.8.0): space
/// records carry `participant_app_ids` (the participant links).
pub const EXPORT_FORMAT_MINOR: u32 = 10;

/// The first export minor that carries BAN_MEMBERS (bit 47) itself.
pub const EXPORT_MINOR_BAN_MEMBERS: u32 = 4;

/// The recorder participant's user-facing name. Clients label
/// the recorder's roster entry, the recording banner and notices with it. The engine never sees
/// it (only the pseudonymous `p_…` identity); the binary, unit and user stay
/// Sew's call recorder (not published).
pub const RECORDER_DISPLAY_NAME: &str = "Bobbin";

pub use addr::{Addr, AddrError, ChannelRef, DmTarget, Handle, Slug, SpaceRef};
pub use clock::{Clock, SystemClock};
pub use error::{ApiError, Code, ErrorBody, Validate, ValidationError};
pub use external_id::ExternalId;
pub use http::{IdempotencyKey, Nonce, Page, PageQuery, Patch};
pub use id::{Id, IdGenerator, IdList, NodeId, id_to_time};
pub use perms::Perms;
pub use time::Timestamp;
