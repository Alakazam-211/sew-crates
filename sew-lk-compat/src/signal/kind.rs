// Copyright 2023 LiveKit, Inc.
// Copyright 2026 Alakazam Labs
// SPDX-License-Identifier: Apache-2.0
// Rewritten from a written behaviour specification of the earlier port of livekit/livekit's test client (Apache-2.0).

//! Stable names for server messages (the session's per-kind counters).

use sew_lkproto::livekit::SignalResponse;
use sew_lkproto::livekit::signal_response::Message as Msg;

/// The snake_case name of the message `r` carries, or `"empty"` when it carries none. The match
/// has no catch-all on purpose: a message added to the protocol must be named here before the
/// crate builds again.
pub fn response_kind(r: &SignalResponse) -> &'static str {
    let Some(m) = &r.message else {
        return "empty";
    };
    match m {
        Msg::Answer(_) => "answer",
        Msg::ConnectionQuality(_) => "connection_quality",
        Msg::DataTrackSubscriberHandles(_) => "data_track_subscriber_handles",
        Msg::GetDataBlobResponse(_) => "get_data_blob_response",
        Msg::Join(_) => "join",
        Msg::Leave(_) => "leave",
        Msg::MediaSectionsRequirement(_) => "media_sections_requirement",
        Msg::Mute(_) => "mute",
        Msg::Offer(_) => "offer",
        Msg::Pong(_) => "pong",
        Msg::PongResp(_) => "pong_resp",
        Msg::PublishDataTrackResponse(_) => "publish_data_track_response",
        Msg::Reconnect(_) => "reconnect",
        Msg::RefreshToken(_) => "refresh_token",
        Msg::RequestResponse(_) => "request_response",
        Msg::RoomMoved(_) => "room_moved",
        Msg::RoomUpdate(_) => "room_update",
        Msg::SpeakersChanged(_) => "speakers_changed",
        Msg::StoreDataBlobResponse(_) => "store_data_blob_response",
        Msg::StreamStateUpdate(_) => "stream_state_update",
        Msg::SubscribedAudioCodecUpdate(_) => "subscribed_audio_codec_update",
        Msg::SubscribedQualityUpdate(_) => "subscribed_quality_update",
        Msg::SubscriptionPermissionUpdate(_) => "subscription_permission_update",
        Msg::SubscriptionResponse(_) => "subscription_response",
        Msg::TrackPublished(_) => "track_published",
        Msg::TrackSubscribed(_) => "track_subscribed",
        Msg::TrackUnpublished(_) => "track_unpublished",
        Msg::Trickle(_) => "trickle",
        Msg::UnpublishDataTrackResponse(_) => "unpublish_data_track_response",
        Msg::Update(_) => "update",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use sew_lkproto::livekit::{Pong, ReconnectResponse};

    fn of(m: Msg) -> &'static str {
        response_kind(&SignalResponse { message: Some(m) })
    }

    #[test]
    fn names_are_snake_case_variant_names() {
        assert_eq!(response_kind(&SignalResponse::default()), "empty");
        assert_eq!(of(Msg::Pong(1)), "pong");
        assert_eq!(of(Msg::PongResp(Pong::default())), "pong_resp");
        assert_eq!(of(Msg::RefreshToken(String::new())), "refresh_token");
        assert_eq!(
            of(Msg::Reconnect(ReconnectResponse::default())),
            "reconnect"
        );
        assert_eq!(
            of(Msg::MediaSectionsRequirement(Default::default())),
            "media_sections_requirement"
        );
        assert_eq!(
            of(Msg::GetDataBlobResponse(Default::default())),
            "get_data_blob_response"
        );
    }
}
