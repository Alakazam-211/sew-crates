//! The plugin's one error shape: `{code, detail, hint}`, as every command rejects with.

use serde::{Deserialize, Serialize};

/// The codes the plugin writes itself. A refusal of the App server's lane keeps the lane's own
/// code (`device_limit`, `locked`, `rate_limited`, `validation_failed`, ...), and a refusal of
/// the host's sign-in door keeps the host's (`bad_login`, `locked`, `rate_limited` with
/// `detail.retry_after_ms`, `password_change_required`, `password_too_short`, `bad_token`,
/// ...).
pub mod code {
    /// A command's arguments did not parse or are out of range (`detail` says which).
    pub const INVALID_ARGS: &str = "invalid_args";
    /// The server address is not a URL, or has a user, a password or an unknown scheme.
    pub const INVALID_URL: &str = "invalid_url";
    /// The server address is `http://` on a host that is not this machine.
    pub const INSECURE_URL: &str = "insecure_url";
    /// The address answers, but not as an App server with a native lane (no `/sew/native/info`).
    pub const NOT_A_SEW_APP: &str = "not_a_sew_app";
    /// The server's lane is older than this plugin needs (`detail: {lane, needs}`).
    pub const LANE_TOO_OLD: &str = "lane_too_old";
    /// No saved or pending server has this origin.
    pub const UNKNOWN_SERVER: &str = "unknown_server";
    /// `confirm_server` came too late (the pending entry is gone) or for a server never added.
    pub const NOT_PENDING: &str = "not_pending";
    /// No account with this key on that server.
    pub const UNKNOWN_ACCOUNT: &str = "unknown_account";
    /// No account is active (a join or `open` without one).
    pub const NO_ACCOUNT: &str = "no_account";
    /// The account's device session ended (`detail.reason`: `signed_out`, `revoked`,
    /// `replaced`, `idle`, `ended`): sign in again.
    pub const SIGNED_OUT: &str = "signed_out";
    /// The App server could not be reached (DNS, TLS, refused, timed out).
    pub const UNREACHABLE: &str = "unreachable";
    /// The App server answered something this plugin can't read.
    pub const BAD_ANSWER: &str = "bad_answer";
    /// The secret store failed (`detail` names the step, never a secret).
    pub const SECRET_STORE: &str = "secret_store";
    /// No OS credential store answers, and the app did not opt in to the file fallback.
    pub const SECRET_STORE_UNAVAILABLE: &str = "secret_store_unavailable";
    /// A warning (the `warning` event): the device bearer is kept in a 0600 file, because
    /// no OS credential store answers and the app opted in to the file fallback.
    pub const SECRET_STORAGE_FILE: &str = "secret_storage_file";
    /// The server does not offer password sign-in (`info.signin.password` is false).
    pub const PASSWORD_UNAVAILABLE: &str = "password_unavailable";
    /// The server does not offer pairing (`info.signin.pairing` is false).
    pub const PAIRING_UNAVAILABLE: &str = "pairing_unavailable";
    /// The server does not offer the host's sign-in door (`info.signin.door` is not true), or
    /// the host's `/.k2/auth/ping` does not answer `{"k2": true}` (`detail.reason: "ping"`).
    /// No password was sent: use the password form.
    pub const SIGNIN_DOOR_UNAVAILABLE: &str = "signin_door_unavailable";
    /// The lane's sign-in begin named a door that isn't `/.k2/auth/sign-in` on the confirmed
    /// server. No password was sent.
    pub const SIGNIN_DOOR_MISMATCH: &str = "signin_door_mismatch";
    /// The host's door (K2's code, passed on): the person must set a new password first.
    /// Ask for one and call `sign_in_door_set_password` with the same `server` and `account`
    /// (the change token stays in the plugin).
    pub const PASSWORD_CHANGE_REQUIRED: &str = "password_change_required";
    /// `sign_in_door_set_password` with no `password_change_required` held for that server
    /// and account (none came, it was used, or it is older than the door's begin).
    pub const NO_PASSWORD_CHANGE: &str = "no_password_change";
    /// The host set the new password but made no sign-in code: the person can't use this App.
    pub const PASSWORD_SET_NO_SIGNIN: &str = "password_set_no_signin";
    /// The App needs a newer shell (`info.shell.min`): `detail: {min, version}`. Update the
    /// app.
    pub const SHELL_TOO_OLD: &str = "shell_too_old";
    /// `pair_confirm` with no pairing link held for that server (none came, it was declined,
    /// used, or is older than 2 minutes).
    pub const PAIRING_NOT_PENDING: &str = "pairing_not_pending";
    /// The App server has no native lane at this route (a `404` it did not explain).
    pub const LANE_NOT_FOUND: &str = "lane_not_found";
    /// A server's live site named another server or one of its accounts (a live site's
    /// commands act only on its own server), or `handoff` asked from a page whose origin is
    /// not the account's server.
    pub const WRONG_ORIGIN: &str = "wrong_origin";
    /// The command answers only the app's bundled UI (`add_server`, `confirm_server`).
    pub const BUNDLED_ONLY: &str = "bundled_only";
    /// The saved server list could not be read or written.
    pub const STORE: &str = "store";
    /// A notification could not be shown.
    pub const NOTIFY_FAILED: &str = "notify_failed";
    /// A window or webview operation failed (navigation, focus).
    pub const WINDOW: &str = "window";
    /// `upload_file` or `pick_files` could not use the file: the handle is not one the file
    /// dialog gave this page, or the file can't be read. One answer for every case, so a page
    /// learns nothing about files it wasn't given.
    pub const FILE: &str = "file";
    /// `gateway_send` or `gateway_close` named no open socket (it closed, or never was).
    pub const UNKNOWN_SOCKET: &str = "unknown_socket";
    /// The lane's own code, also written by the plugin: a third gateway socket of one account
    /// (`detail: {max: 2}`; the lane allows 2 per device, so a reconnect can overlap the old
    /// socket).
    pub const RATE_LIMITED: &str = "rate_limited";
}

/// What a command rejects with: `{code, detail, hint}`.
///
/// `code` is one of [`code`] or the App server lane's own. `hint` is the server's sentence for
/// a person when it gave one (the lane's `error.hint`), else `None`. Never carries a secret.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}")]
pub struct AppError {
    /// The code.
    pub code: String,
    /// Its details (`null` when none).
    #[serde(default)]
    pub detail: serde_json::Value,
    /// A sentence to show (the lane's `error.hint`), when the server gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl AppError {
    /// An error with a code alone.
    pub fn new(code: impl Into<String>) -> AppError {
        AppError {
            code: code.into(),
            detail: serde_json::Value::Null,
            hint: None,
        }
    }

    /// An error with details.
    pub fn with_detail(code: impl Into<String>, detail: impl Into<serde_json::Value>) -> AppError {
        AppError {
            code: code.into(),
            detail: detail.into(),
            hint: None,
        }
    }

    /// `invalid_args` with a sentence saying what.
    pub fn invalid_args(what: impl Into<String>) -> AppError {
        AppError::with_detail(code::INVALID_ARGS, what.into())
    }

    /// The `signed_out` reason, when this is `signed_out`.
    pub fn signed_out_reason(&self) -> Option<&str> {
        (self.code == code::SIGNED_OUT)
            .then(|| {
                self.detail
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
            })
            .flatten()
    }
}
