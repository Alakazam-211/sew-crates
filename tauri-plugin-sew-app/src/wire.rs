//! The commands' names, events and the notification bridge's shapes.
//!
//! Arguments are flat snake_case keys, as in `tauri-plugin-sew-voice`:
//! `invoke('plugin:sew-app|sign_in', {server, username, password})`. An account is named by
//! `key: {server, account}`; without `key` a command uses the active account. The answers are
//! the core's types: [`crate::ServersView`], [`crate::PendingServer`], [`crate::AccountView`],
//! [`crate::SignOutResult`], [`crate::HandoffView`], [`crate::SessionInfo`],
//! [`crate::LaneInfo`], and for the API transport [`crate::ApiReply`],
//! [`crate::GatewayOpened`] with [`crate::GatewayEvent`]s on its channel.

use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// The commands, as the webview invokes them after `plugin:sew-app|`: the seventeen of
/// `sew-app:default`, then `handoff` (permission `sew-app:handoff` only), then the API
/// transport: the five of `sew-app:transport` ([`TRANSPORT_COMMANDS`]) and `upload_file`
/// (`sew-app:upload-file` only).
pub const COMMANDS: [&str; 24] = [
    "servers",
    "add_server",
    "confirm_server",
    "remove_server",
    "server_info",
    "set_active",
    "sign_in",
    "pair",
    "sign_in_door",
    "sign_in_door_set_password",
    "pair_confirm",
    "pair_decline",
    "sign_out",
    "devices",
    "revoke_device",
    "open",
    "notify",
    "handoff",
    "api",
    "gateway_open",
    "gateway_send",
    "gateway_close",
    "upload_bytes",
    "upload_file",
];

/// `sew-app:transport`: the API transport for a bundled UI.
pub const TRANSPORT_COMMANDS: [&str; 5] = [
    "api",
    "gateway_open",
    "gateway_send",
    "gateway_close",
    "upload_bytes",
];

/// The events, after `sew-app://`.
pub const EVENTS: [&str; 6] = [
    "servers",
    "signed_out",
    "warning",
    "deep_link",
    "pair_request",
    "pair_refused",
];

/// The app badge a `notify` sets: `badge` absent or `null` leaves it, `0` clears it, `n` sets
/// it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Badge {
    /// Unchanged.
    #[default]
    Unchanged,
    /// No badge.
    Clear,
    /// A count.
    Count(i64),
}

impl Badge {
    /// From `notify`'s `badge` (`invalid_args` when negative).
    pub fn from_arg(badge: Option<i64>) -> Result<Badge, AppError> {
        match badge {
            None => Ok(Badge::Unchanged),
            Some(0) => Ok(Badge::Clear),
            Some(n) if n < 0 => Err(AppError::invalid_args("badge must be 0 or more")),
            Some(n) => Ok(Badge::Count(n)),
        }
    }
}

/// The longest notification title (characters).
pub const MAX_TITLE: usize = 256;
/// The longest notification body (characters).
pub const MAX_BODY: usize = 4096;

/// Checks `notify`'s title and body (`invalid_args`): a body needs a title; at most
/// [`MAX_TITLE`] and [`MAX_BODY`] characters.
pub fn check_notification(title: Option<&str>, body: Option<&str>) -> Result<(), AppError> {
    let len = |s: Option<&str>| s.map_or(0, |s| s.chars().count());
    if len(title) > MAX_TITLE || len(body) > MAX_BODY {
        return Err(AppError::invalid_args(
            "title up to 256, body up to 4096 characters",
        ));
    }
    if title.is_none() && body.is_some() {
        return Err(AppError::invalid_args("a notification needs a title"));
    }
    Ok(())
}

/// `notify`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyResult {
    /// Whether a notification was shown.
    pub shown: bool,
    /// What happened to the badge: `set`, `cleared`, `unchanged` or `unsupported` (Windows has
    /// no app badge; some Linux desktops neither).
    pub badge: String,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn badge_and_notification_checks() {
        assert_eq!(Badge::from_arg(None).unwrap(), Badge::Unchanged);
        assert_eq!(Badge::from_arg(Some(0)).unwrap(), Badge::Clear);
        assert_eq!(Badge::from_arg(Some(4)).unwrap(), Badge::Count(4));
        assert!(Badge::from_arg(Some(-1)).is_err());
        assert!(check_notification(None, Some("no title")).is_err());
        assert!(check_notification(Some("t"), Some("b")).is_ok());
        assert!(check_notification(None, None).is_ok());
        let long = "x".repeat(MAX_TITLE + 1);
        assert!(check_notification(Some(&long), None).is_err());
    }
}
