// Copyright 2023 LiveKit, Inc.
// Copyright 2026 Alakazam Labs
// SPDX-License-Identifier: Apache-2.0
// Rewritten from a written behaviour specification of the earlier port of livekit/livekit's test client (Apache-2.0).

//! What a voice client sends to open the signalling socket, and the small wire helpers around it.
//!
//! - [`JoinParams`] and [`ClientInfoOpts`]: the inputs of one connection attempt (first join,
//!   resume or full reconnect) and the `/rtc/v1` URL they produce ([`JoinParams::url`]).
//! - [`join_request`]: building, packing and unpacking the `join_request` query value.
//! - [`v1`]: the path and query string of the v1 endpoint.
//! - [`CandidateInit`] and [`trickle_for`]: the ICE candidate JSON carried in trickles.
//! - [`response_kind`]: a stable name for each server message, used as a counter key.
//!
//! The access token is sent by the caller in the `Authorization: Bearer` header of the
//! WebSocket upgrade; nothing here puts it in the URL unless [`JoinParams::token_in_query`] is
//! set, and the `Debug` form of [`JoinParams`] never prints it.

use std::fmt;

use sew_lkproto::livekit::{ClientInfo, JoinRequest, client_info};

pub mod join_request;
pub mod v1;

mod kind;
mod trickle;

pub use kind::response_kind;
pub use trickle::{CandidateInit, trickle_for};

/// How this client describes itself in the join request's `ClientInfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientInfoOpts {
    /// The SDK family reported to the server (Rust by default).
    pub sdk: client_info::Sdk,
    /// Free-form version string, e.g. `sew-call-client/0.7.0`. Empty by default.
    pub version: String,
    /// Signalling protocol revision the client speaks.
    pub protocol: i32,
    /// Operating system name, as Rust spells it (`linux`, `macos`, `windows`, ...).
    pub os: String,
}

impl Default for ClientInfoOpts {
    fn default() -> Self {
        Self {
            sdk: client_info::Sdk::Rust,
            version: String::new(),
            protocol: sew_lkproto::CURRENT_PROTOCOL,
            os: std::env::consts::OS.to_owned(),
        }
    }
}

impl ClientInfoOpts {
    /// The protobuf `ClientInfo`: these four fields set, everything else left at its default.
    pub fn to_proto(&self) -> ClientInfo {
        ClientInfo {
            sdk: self.sdk as i32,
            version: self.version.clone(),
            protocol: self.protocol,
            os: self.os.clone(),
            ..ClientInfo::default()
        }
    }
}

/// One connection attempt's parameters.
///
/// A first join and a full reconnect use `reconnect = false`; a resume sets `reconnect = true`
/// and `participant_sid` to the sid the server gave us before.
#[derive(Clone, PartialEq, Eq)]
pub struct JoinParams {
    /// Access token (the original grant, or the latest refresh token).
    pub token: String,
    /// Ask the server to subscribe us to every published track.
    pub auto_subscribe: bool,
    /// How this client identifies itself.
    pub client_info: ClientInfoOpts,
    /// True when resuming an existing participant session.
    pub reconnect: bool,
    /// The participant sid being resumed, if any.
    pub participant_sid: Option<String>,
    /// Also put the token in the URL as `access_token` (off by default; the header is used).
    pub token_in_query: bool,
}

impl fmt::Debug for JoinParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The token is a credential: keep it out of logs.
        f.debug_struct("JoinParams")
            .field("token", &"<redacted>")
            .field("auto_subscribe", &self.auto_subscribe)
            .field("client_info", &self.client_info)
            .field("reconnect", &self.reconnect)
            .field("participant_sid", &self.participant_sid)
            .finish()
    }
}

impl JoinParams {
    /// A first-join attempt: auto-subscribe on, not a resume, token kept out of the URL.
    pub fn new(token: &str, client_info: ClientInfoOpts) -> JoinParams {
        JoinParams {
            token: token.to_owned(),
            auto_subscribe: true,
            client_info,
            reconnect: false,
            participant_sid: None,
            token_in_query: false,
        }
    }

    /// The `JoinRequest` this attempt sends (inside the URL's `join_request` value).
    pub fn join_request(&self) -> JoinRequest {
        let mut req = join_request::join_request(
            self.client_info.to_proto(),
            self.auto_subscribe,
            false,
            &[],
        );
        req.reconnect = self.reconnect;
        req.participant_sid = self.participant_sid.clone().unwrap_or_default();
        req
    }

    /// The signalling URL under `base` (e.g. `wss://voice.example`); trailing slashes on `base`
    /// are dropped. The scheme is not checked here.
    pub fn url(&self, base: &str) -> String {
        let root = base.trim_end_matches('/');
        format!("{root}{}{}", v1::PATH, v1::query(self))
    }
}

/// Form-style escaping of a query value: unreserved ASCII (`A-Z a-z 0-9 - _ . ~`) is kept, a
/// space becomes `+`, and every other UTF-8 byte becomes `%XX` with uppercase hex.
pub fn query_escape(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(char::from(HEX[usize::from(b >> 4)]));
                out.push(char::from(HEX[usize::from(b & 0x0f)]));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn params() -> JoinParams {
        JoinParams::new(
            "tok.en-SECRET",
            ClientInfoOpts {
                version: "sew-call-client/test".to_owned(),
                ..ClientInfoOpts::default()
            },
        )
    }

    fn value_of(url: &str) -> &str {
        let (_, v) = url
            .split_once("join_request=")
            .expect("join_request in the URL");
        v.split('&').next().unwrap_or_default()
    }

    #[test]
    fn v1_url_carries_the_join_request_and_no_token() {
        let p = params();
        let url = p.url("wss://voice.example/");
        assert!(
            url.starts_with("wss://voice.example/rtc/v1?join_request="),
            "{url}"
        );
        assert!(!url.contains("tok.en-SECRET"), "token leaked into {url}");
        assert!(!url.contains("access_token"), "{url}");

        let req = join_request::decode(value_of(&url)).expect("the value decodes");
        let ci = req.client_info.expect("client info present");
        assert_eq!(ci.sdk, client_info::Sdk::Rust as i32);
        assert_eq!(ci.protocol, sew_lkproto::CURRENT_PROTOCOL);
        assert_eq!(ci.version, "sew-call-client/test");
        assert!(!req.reconnect);

        let dbg = format!("{p:?}");
        assert!(!dbg.contains("tok.en-SECRET"), "{dbg}");
        assert!(dbg.starts_with("JoinParams"), "{dbg}");
        assert!(dbg.contains("\"<redacted>\""), "{dbg}");
        assert!(!dbg.contains("token_in_query"), "{dbg}");
    }

    #[test]
    fn resume_sets_reconnect_and_the_sid() {
        let mut p = params();
        p.reconnect = true;
        p.participant_sid = Some("PA_x".to_owned());
        let req = p.join_request();
        assert!(req.reconnect);
        assert_eq!(req.participant_sid, "PA_x");
    }

    #[test]
    fn first_join_request_shape() {
        let p = params();
        let req = p.join_request();
        assert_eq!(req.participant_sid, "");
        assert!(!req.reconnect);
        assert!(req.participant_attributes.is_empty());
        let cs = req
            .connection_settings
            .expect("connection settings present");
        assert!(cs.auto_subscribe);
        assert!(!cs.adaptive_stream);
        assert_eq!(cs.auto_subscribe_data_track, Some(false));
        assert_eq!(cs.subscriber_allow_pause, None);
        assert!(!cs.disable_ice_lite);
        assert_eq!(req.metadata, "");
        assert!(req.add_track_requests.is_empty());
        assert!(req.publisher_offer.is_none());

        let mut quiet = params();
        quiet.auto_subscribe = false;
        let cs = quiet.join_request().connection_settings.expect("present");
        assert!(!cs.auto_subscribe);
    }

    #[test]
    fn new_params_defaults() {
        let p = JoinParams::new("t", ClientInfoOpts::default());
        assert_eq!(p.token, "t");
        assert!(p.auto_subscribe);
        assert!(!p.reconnect);
        assert_eq!(p.participant_sid, None);
        assert!(!p.token_in_query);
    }

    #[test]
    fn client_info_defaults_and_proto() {
        let d = ClientInfoOpts::default();
        assert_eq!(d.sdk, client_info::Sdk::Rust);
        assert_eq!(d.sdk as i32, 8);
        assert_eq!(d.version, "");
        assert_eq!(d.protocol, sew_lkproto::CURRENT_PROTOCOL);
        assert_eq!(d.os, std::env::consts::OS);

        let ci = ClientInfoOpts {
            version: "v".to_owned(),
            ..d
        }
        .to_proto();
        assert_eq!(
            ci,
            ClientInfo {
                sdk: 8,
                version: "v".to_owned(),
                protocol: sew_lkproto::CURRENT_PROTOCOL,
                os: std::env::consts::OS.to_owned(),
                ..ClientInfo::default()
            }
        );
    }

    #[test]
    fn url_trims_every_trailing_slash() {
        let p = params();
        let a = p.url("wss://voice.example");
        let b = p.url("wss://voice.example///");
        assert_eq!(a, b);
        assert!(a.starts_with("wss://voice.example/rtc/v1?join_request="));
    }

    #[test]
    fn join_request_value_is_not_percent_escaped() {
        let p = params();
        let url = p.url("wss://voice.example");
        let v = value_of(&url);
        assert_eq!(v, join_request::encode(&p.join_request()));
        assert!(!v.contains('%'), "{v}");
    }

    #[test]
    fn token_in_query_adds_an_escaped_access_token() {
        let mut p = params();
        p.token = "a b/c".to_owned();
        p.token_in_query = true;
        let url = p.url("wss://voice.example");
        assert!(url.ends_with("&access_token=a+b%2Fc"), "{url}");
        let v = value_of(&url);
        assert!(join_request::decode(v).is_some());
    }

    #[test]
    fn query_escape_matches_form_encoding() {
        assert_eq!(query_escape("a b/c=d"), "a+b%2Fc%3Dd");
        assert_eq!(query_escape("AZaz09-_.~"), "AZaz09-_.~");
        assert_eq!(query_escape("+&?#%"), "%2B%26%3F%23%25");
        assert_eq!(query_escape("é"), "%C3%A9");
        assert_eq!(query_escape("\n\u{7f}"), "%0A%7F");
        assert_eq!(query_escape(""), "");
    }
}
