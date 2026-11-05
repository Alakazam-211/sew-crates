//! The call credential: `sewcc_…`, minted with the join answer, held in Rust memory
//! only, renewed at half its life (`POST /api/v1/me/voice/credential`).
//!
//! `VoiceJoinResponse.call_credential` and `CallCredentialRenewResponse` are `sew-proto`'s wire
//! types (Sew 0.7.1). This module carries its own copies of their fields ([`CredentialGrant`],
//! [`RenewAnswer`]).

use std::fmt;
use std::sync::{Arc, RwLock};

use serde::Deserialize;
use sew_e2ee::JoinId;
use sew_proto::time::Timestamp;
use zeroize::Zeroizing;

/// `POST /api/v1/me/voice/credential` (`sew_proto::routes::ME_VOICE_CREDENTIAL`).
pub const ME_VOICE_CREDENTIAL: &str = "/api/v1/me/voice/credential";

/// The call credential's token prefix (`sew_proto::token::CALL_CREDENTIAL_PREFIX`).
pub const CALL_CREDENTIAL_PREFIX: &str = "sewcc_";

/// A bearer token: zeroized on drop, never printed (`Debug` shows the kind only).
#[derive(Clone)]
pub struct Bearer(Zeroizing<String>);

impl Bearer {
    /// Wraps a token.
    pub fn new(token: impl Into<String>) -> Bearer {
        Bearer(Zeroizing::new(token.into()))
    }

    /// The token, for one header.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether this is a call credential (`sewcc_`).
    pub fn is_call_credential(&self) -> bool {
        self.0.starts_with(CALL_CREDENTIAL_PREFIX)
    }
}

impl fmt::Debug for Bearer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if self.is_call_credential() {
            "sewcc_"
        } else if self.0.starts_with("sews_") {
            "sews_"
        } else {
            "other"
        };
        write!(f, "Bearer({kind}***)")
    }
}

/// `VoiceJoinResponse.call_credential`: `{token, expires_at, join_id, device_scope}`.
#[derive(Clone, Deserialize)]
pub struct CredentialGrant {
    /// `sewcc_<selector>.<secret>`.
    pub token: String,
    /// When it expires.
    pub expires_at: Timestamp,
    /// The join the deferred key join must name.
    pub join_id: JoinId,
    /// hex(SHA-256("sew call device scope v1" ‖ install_id ‖ lineage))[..32]: the engine keys its
    /// stored device on it (the device store).
    pub device_scope: String,
}

impl fmt::Debug for CredentialGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialGrant")
            .field("token", &"sewcc_***")
            .field("expires_at", &self.expires_at)
            .field("join_id", &self.join_id)
            .field("device_scope", &self.device_scope)
            .finish()
    }
}

/// `CallCredentialRenewResponse`: `{token, expires_at}`.
#[derive(Clone, Deserialize)]
pub struct RenewAnswer {
    /// The same credential with a new secret.
    pub token: String,
    /// When it expires.
    pub expires_at: Timestamp,
}

impl fmt::Debug for RenewAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenewAnswer")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// Why renewing failed for good (`401` with `details.reason`), as a key session that renews
/// its own credential reads it: it ends with [`KeyEnd::CredentialLost`](super::KeyEnd), whose
/// `rejoin` is [`RenewRefused::allows_rejoin`].
///
/// The call engine ([`crate::Call`]) renews the call credential itself (its key session does
/// not) and ends the call by Sew's reason: `signed_out` is `signin_required`, `expired` is
/// `credential_expired`, `call_ended` (or none) is `credential_lost`. It never re-joins on a
/// refused renewal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RenewRefused {
    /// The join ended (`call_ended`).
    CallEnded,
    /// The login lineage was revoked or the member deactivated (`signed_out`). A lineage that
    /// merely expired still renews, up to the server's call bound.
    SignedOut,
    /// The credential expired (`expired`, e.g. after a long sleep), or its login expired longer
    /// ago than the server's call bound (4 h from the credential's mint) allows. The only one
    /// that [`allows_rejoin`](RenewRefused::allows_rejoin).
    Expired,
    /// Any other final answer.
    Other(u16, Option<String>),
}

impl RenewRefused {
    /// From a final failure of the renew route.
    pub fn from_failure(f: &sew_e2ee::ApiFailure) -> RenewRefused {
        match (f.status, f.reason.as_deref()) {
            (401, Some("call_ended")) => RenewRefused::CallEnded,
            (401, Some("signed_out")) => RenewRefused::SignedOut,
            (401, Some("expired")) => RenewRefused::Expired,
            (s, r) => RenewRefused::Other(s, r.map(str::to_owned)),
        }
    }

    /// Whether the engine may re-join through the provider (`expired` only).
    pub fn allows_rejoin(&self) -> bool {
        matches!(self, RenewRefused::Expired)
    }
}

/// When to renew a credential that expires at `expires_at_ms`, minted (or last renewed) at
/// `issued_at_ms`: half its life later (5 min of the default 10).
pub fn renew_at(issued_at_ms: u64, expires_at_ms: u64) -> u64 {
    issued_at_ms + expires_at_ms.saturating_sub(issued_at_ms) / 2
}

/// A wire timestamp as unix ms (0 before 1970).
pub fn unix_ms(t: Timestamp) -> u64 {
    u64::try_from(t.unix_ms()).unwrap_or(0)
}

/// The live bearer every request and the socket's REAUTH read. Shared between the transport and
/// the socket so a renewal reaches both at once.
#[derive(Clone)]
pub struct SharedBearer(Arc<RwLock<Bearer>>);

impl fmt::Debug for SharedBearer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SharedBearer({:?})", self.get())
    }
}

impl SharedBearer {
    /// Holds `b`.
    pub fn new(b: Bearer) -> SharedBearer {
        SharedBearer(Arc::new(RwLock::new(b)))
    }

    /// The current bearer.
    pub fn get(&self) -> Bearer {
        match self.0.read() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// Replaces it (a renewal).
    pub fn set(&self, b: Bearer) {
        match self.0.write() {
            Ok(mut g) => *g = b,
            Err(p) => *p.into_inner() = b,
        }
    }
}
