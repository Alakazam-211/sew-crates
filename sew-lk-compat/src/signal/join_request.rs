// Copyright 2023 LiveKit, Inc.
// Copyright 2026 Alakazam Labs
// SPDX-License-Identifier: Apache-2.0
// Rewritten from a written behaviour specification of the earlier port of livekit/livekit's test client (Apache-2.0).

//! The `join_request` query value: a `JoinRequest`, protobuf-encoded, wrapped in an uncompressed
//! `WrappedJoinRequest`, protobuf-encoded again and written as padded URL-safe base64.

use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use prost::Message as _;
use sew_lkproto::livekit::{
    ClientInfo, ConnectionSettings, JoinRequest, WrappedJoinRequest, wrapped_join_request,
};

/// A `JoinRequest` with the client info, the subscription settings and the participant
/// attributes set (a repeated attribute key keeps its last value). Data tracks are never
/// auto-subscribed: `auto_subscribe_data_track` is present and false.
pub fn join_request(
    client_info: ClientInfo,
    auto_subscribe: bool,
    adaptive_stream: bool,
    attributes: &[(String, String)],
) -> JoinRequest {
    let settings = ConnectionSettings {
        auto_subscribe,
        adaptive_stream,
        auto_subscribe_data_track: Some(false),
        ..ConnectionSettings::default()
    };
    JoinRequest {
        client_info: Some(client_info),
        connection_settings: Some(settings),
        participant_attributes: attributes.iter().cloned().collect(),
        ..JoinRequest::default()
    }
}

/// Pack `req` into the query value. The wrapper always says "no compression".
pub fn encode(req: &JoinRequest) -> String {
    let wrapper = WrappedJoinRequest {
        compression: wrapped_join_request::Compression::None as i32,
        join_request: req.encode_to_vec(),
    };
    URL_SAFE.encode(wrapper.encode_to_vec())
}

/// Unpack a query value (URL-safe base64, padded or not). `None` if any layer is malformed or
/// the wrapper claims a compression this client does not produce.
pub fn decode(value: &str) -> Option<JoinRequest> {
    let bytes = URL_SAFE
        .decode(value)
        .or_else(|_| URL_SAFE_NO_PAD.decode(value))
        .ok()?;
    let wrapper = WrappedJoinRequest::decode(bytes.as_slice()).ok()?;
    if wrapper.compression != wrapped_join_request::Compression::None as i32 {
        return None;
    }
    JoinRequest::decode(wrapper.join_request.as_slice()).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn sample() -> JoinRequest {
        let ci = ClientInfo {
            sdk: 8,
            protocol: 17,
            version: "x/1".to_owned(),
            ..ClientInfo::default()
        };
        join_request(
            ci,
            true,
            false,
            &[("room-role".to_owned(), "host".to_owned())],
        )
    }

    #[test]
    fn join_request_roundtrips_through_the_query_value() {
        let req = sample();
        let v = encode(&req);
        assert!(!v.contains('+') && !v.contains('/'), "{v}");
        let back = decode(&v).expect("decodes");
        assert_eq!(back, req);
        let ci = back.client_info.as_ref().expect("client info");
        assert_eq!(ci.sdk, 8);
        assert_eq!(ci.protocol, 17);
        assert!(
            back.connection_settings
                .as_ref()
                .expect("settings")
                .auto_subscribe
        );
        assert_eq!(back.participant_attributes["room-role"], "host");
    }

    #[test]
    fn later_duplicate_attributes_win() {
        let attrs = [
            ("k".to_owned(), "first".to_owned()),
            ("k".to_owned(), "second".to_owned()),
        ];
        let req = join_request(ClientInfo::default(), false, true, &attrs);
        assert_eq!(req.participant_attributes.len(), 1);
        assert_eq!(req.participant_attributes["k"], "second");
        let cs = req.connection_settings.expect("settings");
        assert!(!cs.auto_subscribe);
        assert!(cs.adaptive_stream);
        assert_eq!(cs.auto_subscribe_data_track, Some(false));
    }

    #[test]
    fn encoding_is_padded_and_uncompressed() {
        // Find a request whose wrapper length is not a multiple of 3, so padding must appear.
        let mut req = sample();
        let mut v = encode(&req);
        while !v.ends_with('=') {
            req.metadata.push('m');
            v = encode(&req);
        }
        let raw = URL_SAFE.decode(&v).expect("padded URL-safe base64");
        let w = WrappedJoinRequest::decode(raw.as_slice()).expect("wrapper");
        assert_eq!(w.compression, 0);
        assert_eq!(
            JoinRequest::decode(w.join_request.as_slice()).expect("inner"),
            req
        );
        // The unpadded spelling is accepted too.
        let trimmed = v.trim_end_matches('=');
        assert_eq!(decode(trimmed), Some(req));
    }

    #[test]
    fn decode_rejects_bad_input() {
        assert_eq!(decode("***"), None);
        // Valid base64 that is not a wrapper (a truncated varint field).
        assert_eq!(decode(&URL_SAFE.encode([0x08])), None);
        // A compressed wrapper is refused.
        let gz = WrappedJoinRequest {
            compression: wrapped_join_request::Compression::Gzip as i32,
            join_request: sample().encode_to_vec(),
        };
        assert_eq!(decode(&URL_SAFE.encode(gz.encode_to_vec())), None);
        // An uncompressed wrapper whose payload is not a JoinRequest.
        let junk = WrappedJoinRequest {
            compression: 0,
            join_request: vec![0x0a, 0x05, 0x01],
        };
        assert_eq!(decode(&URL_SAFE.encode(junk.encode_to_vec())), None);
    }
}
