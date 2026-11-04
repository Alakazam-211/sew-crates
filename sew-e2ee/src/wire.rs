//! The key routes a session drives: this crate's enums over
//! `sew_proto::e2ee` request and response types, and the failure shape the driver feeds back.

use serde::Serialize;
use sew_proto::e2ee::reasons;
use sew_proto::e2ee::{
    CallId, KeyCommitAccepted, KeyCommitRequest, KeyInvalidRequest, KeyReadyRequest,
    KeyRekeyRequest, KeyReleaseRequest, KeyReportRequest, KeyResetPackageRequest, KeyStateRequest,
    KeyStateResponse,
};
use sew_proto::routes;

/// A key route request for one call. The driver POSTs `body()` to `path()`; every body carries the
/// Sew-minted `join_id` and is idempotent per the server's key, so a failed POST is retried
/// unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyRequest {
    /// `state`.
    State(CallId, KeyStateRequest),
    /// `commit`.
    Commit(CallId, KeyCommitRequest),
    /// `ready`.
    Ready(CallId, KeyReadyRequest),
    /// `invalid` (a re-add).
    Invalid(CallId, KeyInvalidRequest),
    /// `key_package` (a reset).
    ResetPackage(CallId, KeyResetPackageRequest),
    /// `release`.
    Release(CallId, KeyReleaseRequest),
    /// `rekey`.
    Rekey(CallId, KeyRekeyRequest),
    /// `report`.
    Report(CallId, KeyReportRequest),
}

impl KeyRequest {
    /// `state`, `commit`, `ready`, `invalid`, `key_package`, `release`, `rekey`, `report`.
    pub fn kind(&self) -> &'static str {
        match self {
            KeyRequest::State(..) => "state",
            KeyRequest::Commit(..) => "commit",
            KeyRequest::Ready(..) => "ready",
            KeyRequest::Invalid(..) => "invalid",
            KeyRequest::ResetPackage(..) => "key_package",
            KeyRequest::Release(..) => "release",
            KeyRequest::Rekey(..) => "rekey",
            KeyRequest::Report(..) => "report",
        }
    }

    /// The call.
    pub fn call_id(&self) -> &CallId {
        match self {
            KeyRequest::State(c, _)
            | KeyRequest::Commit(c, _)
            | KeyRequest::Ready(c, _)
            | KeyRequest::Invalid(c, _)
            | KeyRequest::ResetPackage(c, _)
            | KeyRequest::Release(c, _)
            | KeyRequest::Rekey(c, _)
            | KeyRequest::Report(c, _) => c,
        }
    }

    /// The route path (`/api/v1/calls/{call_id}/keys/<kind>`).
    pub fn path(&self) -> String {
        let t = match self {
            KeyRequest::State(..) => routes::CALL_KEYS_STATE,
            KeyRequest::Commit(..) => routes::CALL_KEYS_COMMIT,
            KeyRequest::Ready(..) => routes::CALL_KEYS_READY,
            KeyRequest::Invalid(..) => routes::CALL_KEYS_INVALID,
            KeyRequest::ResetPackage(..) => routes::CALL_KEYS_KEY_PACKAGE,
            KeyRequest::Release(..) => routes::CALL_KEYS_RELEASE,
            KeyRequest::Rekey(..) => routes::CALL_KEYS_REKEY,
            KeyRequest::Report(..) => routes::CALL_KEYS_REPORT,
        };
        routes::with_call_id(t, self.call_id().as_str())
    }

    /// The JSON body.
    pub fn body(&self) -> serde_json::Value {
        fn j<T: Serialize>(t: &T) -> serde_json::Value {
            serde_json::to_value(t).unwrap_or(serde_json::Value::Null)
        }
        match self {
            KeyRequest::State(_, b) => j(b),
            KeyRequest::Commit(_, b) => j(b),
            KeyRequest::Ready(_, b) => j(b),
            KeyRequest::Invalid(_, b) => j(b),
            KeyRequest::ResetPackage(_, b) => j(b),
            KeyRequest::Release(_, b) => j(b),
            KeyRequest::Rekey(_, b) => j(b),
            KeyRequest::Report(_, b) => j(b),
        }
    }
}

/// A successful answer (the server's `KeyStateResponse`, `KeyCommitAccepted`, or `202 {}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyReply {
    /// `state`.
    State(KeyStateResponse),
    /// `commit` `200`.
    CommitAccepted(KeyCommitAccepted),
    /// Any other `2xx`.
    Accepted,
}

impl KeyReply {
    /// For logs.
    pub fn kind(&self) -> &'static str {
        match self {
            KeyReply::State(_) => "state",
            KeyReply::CommitAccepted(_) => "commit_accepted",
            KeyReply::Accepted => "accepted",
        }
    }
}

/// A failed key route: the HTTP status and `details.reason`, or a transport failure
/// (`status = 0`). The driver retries transport failures and `503 voice_unavailable` itself
/// (with backoff, unchanged content); what reaches the session is final.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiFailure {
    /// HTTP status (`0` = transport).
    pub status: u16,
    /// `details.reason`, when the envelope has one.
    pub reason: Option<String>,
    /// `details.check` / `details.unsupported`, when present.
    pub detail: Option<String>,
    /// `retry_after_ms` / `Retry-After`, when present.
    pub retry_after_ms: Option<u64>,
}

impl ApiFailure {
    /// A failure with a status and reason.
    pub fn new(status: u16, reason: &str) -> ApiFailure {
        ApiFailure {
            status,
            reason: Some(reason.to_owned()),
            detail: None,
            retry_after_ms: None,
        }
    }

    /// A transport failure.
    pub fn transport() -> ApiFailure {
        ApiFailure {
            status: 0,
            reason: None,
            detail: None,
            retry_after_ms: None,
        }
    }

    /// Whether `details.reason` is `r`.
    pub fn is(&self, r: &str) -> bool {
        self.reason.as_deref() == Some(r)
    }

    /// A failure the driver retries (transport, `503`).
    pub fn is_retryable(&self) -> bool {
        self.status == 0 || self.status == 503
    }

    /// `state`'s three `409`s that mean "re-add with reason `behind`".
    pub fn is_behind(&self) -> bool {
        self.status == 409
            && (self.is(reasons::E2EE_EPOCH_BEHIND)
                || self.is(reasons::E2EE_GROUP_MISMATCH)
                || self.is(reasons::E2EE_NOT_IN_GROUP))
    }
}
