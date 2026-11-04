// Copyright 2023 LiveKit, Inc.
// Copyright 2026 Alakazam Labs
// SPDX-License-Identifier: Apache-2.0
// Rewritten from a written behaviour specification of the earlier port of livekit/livekit's test client (Apache-2.0).

//! The v1 signalling endpoint: its path and its query string.

use super::{JoinParams, join_request, query_escape};

/// Path of the v1 signalling endpoint, appended to the server's base URL.
pub const PATH: &str = "/rtc/v1";

/// `?join_request=<value>` (the value is URL-safe base64 and goes in as is), followed by
/// `&access_token=<escaped token>` only when the params ask for the token in the URL.
pub fn query(p: &JoinParams) -> String {
    let mut q = String::from("?join_request=");
    q.push_str(&join_request::encode(&p.join_request()));
    if p.token_in_query {
        q.push_str("&access_token=");
        q.push_str(&query_escape(&p.token));
    }
    q
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::signal::ClientInfoOpts;

    #[test]
    fn query_has_one_parameter_by_default() {
        let p = JoinParams::new("secret", ClientInfoOpts::default());
        let q = query(&p);
        assert!(q.starts_with("?join_request="), "{q}");
        assert!(!q.contains('&'), "{q}");
        assert!(!q.contains("secret"), "{q}");
        assert_eq!(PATH, "/rtc/v1");
    }
}
