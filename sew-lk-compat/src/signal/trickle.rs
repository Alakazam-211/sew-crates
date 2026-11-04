// Copyright 2023 LiveKit, Inc.
// Copyright 2026 Alakazam Labs
// SPDX-License-Identifier: Apache-2.0
// Rewritten from a written behaviour specification of the earlier port of livekit/livekit's test client (Apache-2.0).

//! ICE candidates in trickle messages: the W3C `RTCIceCandidateInit` JSON shape.

use serde::{Deserialize, Serialize};
use sew_lkproto::livekit::signal_request::Message as Req;
use sew_lkproto::livekit::{SignalRequest, TrickleRequest};

/// One candidate as JSON (`candidate`, `sdpMid`, `sdpMLineIndex`, `usernameFragment`). Absent
/// optional keys parse as `None` and are left out when writing; unknown keys are ignored;
/// `candidate` is required.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateInit {
    /// The `candidate:` attribute line; empty means "no more candidates".
    pub candidate: String,
    /// Media section id the candidate belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdp_mid: Option<String>,
    /// Media section index the candidate belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdp_m_line_index: Option<u32>,
    /// ICE username fragment, when given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username_fragment: Option<String>,
}

/// A trickle request carrying `candidate` for the given target (0 = publisher connection,
/// 1 = subscriber connection). The section index is always written as 0; `last` marks the end of
/// the candidates.
pub fn trickle_for(candidate: &str, mid: Option<&str>, target: i32, last: bool) -> SignalRequest {
    let init = CandidateInit {
        candidate: candidate.to_owned(),
        sdp_mid: mid.map(str::to_owned),
        sdp_m_line_index: Some(0),
        username_fragment: None,
    };
    SignalRequest {
        message: Some(Req::Trickle(TrickleRequest {
            candidate_init: serde_json::to_string(&init).unwrap_or_default(),
            target,
            r#final: last,
        })),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const LINE: &str = "candidate:1 1 udp 2130706431 192.0.2.7 50000 typ host";

    fn trickle_of(r: SignalRequest) -> TrickleRequest {
        match r.message {
            Some(Req::Trickle(t)) => t,
            other => panic!("not a trickle: {other:?}"),
        }
    }

    #[test]
    fn candidate_init_is_camel_case() {
        let t = trickle_of(trickle_for(LINE, Some("0"), 0, false));
        assert!(
            t.candidate_init.contains("\"sdpMid\":\"0\""),
            "{}",
            t.candidate_init
        );
        assert!(!t.candidate_init.contains("usernameFragment"));
        let back: CandidateInit = serde_json::from_str(&t.candidate_init).expect("parses");
        assert_eq!(back.candidate, LINE);
        assert_eq!(back.sdp_mid.as_deref(), Some("0"));
        assert_eq!(back.sdp_m_line_index, Some(0));
        assert_eq!(back.username_fragment, None);
        assert_eq!(t.target, 0);
        assert!(!t.r#final);
    }

    #[test]
    fn no_mid_omits_the_key_and_target_and_final_pass_through() {
        let t = trickle_of(trickle_for(LINE, None, 1, true));
        assert!(!t.candidate_init.contains("sdpMid"), "{}", t.candidate_init);
        assert!(t.candidate_init.contains("\"sdpMLineIndex\":0"));
        assert_eq!(t.target, 1);
        assert!(t.r#final);
    }

    #[test]
    fn parsing_rules() {
        let c: CandidateInit =
            serde_json::from_str(r#"{"candidate":"x","usernameFragment":"uf","extra":true}"#)
                .expect("unknown keys ignored");
        assert_eq!(c.sdp_mid, None);
        assert_eq!(c.sdp_m_line_index, None);
        assert_eq!(c.username_fragment.as_deref(), Some("uf"));
        assert!(serde_json::from_str::<CandidateInit>(r#"{"sdpMid":"0"}"#).is_err());
        assert!(serde_json::from_str::<CandidateInit>("not json").is_err());
    }
}
