//! Examples for the wire types of voice channels.

use super::{ID_BOB, ID_CATEGORY, ID_CHANNEL, ID_ME, ID_OTHER_CHANNEL, ID_SPACE, slug, t};
use crate::e2ee::JoinIdHex;
use crate::http::Patch;
use crate::perms;
use crate::registry::Example;
use crate::voice::*;

const TOKEN: &str = "eyJhbGciOiJFUzI1NiIsImtpZCI6IjlhMWYzMzA0In0.eyJzdWIiOiJwX2xtaGEifQ.c2ln";
const VOICE_URL: &str = "wss://voice.chat.example.com";

pub(super) fn voice_state_full() -> VoiceState {
    VoiceState {
        member_id: ID_ME,
        space_id: Some(ID_SPACE),
        channel_id: ID_CHANNEL,
        connection: VoiceConnection::Connected,
        secure: true,
        self_mute: true,
        self_deaf: false,
        server_mute: false,
        server_deaf: false,
        speak_denied: false,
        joined_at: t(0),
        call_id: None,
        self_video: true,
        self_stream: true,
    }
}

fn voice_state_min() -> VoiceState {
    VoiceState {
        space_id: None,
        connection: VoiceConnection::Connecting,
        secure: false,
        self_mute: false,
        self_video: false,
        self_stream: false,
        ..voice_state_full()
    }
}

/// A DM call entry: no space, `call_id` set.
fn voice_state_call() -> VoiceState {
    VoiceState {
        space_id: None,
        channel_id: super::ID_DM,
        call_id: Some(super::calls::ID_CALL),
        self_stream: false,
        ..voice_state_full()
    }
}

impl Example for VoiceState {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            ("full", voice_state_full()),
            ("full_call", voice_state_call()),
            ("min", voice_state_min()),
        ]
    }
}

impl Example for VoiceJoinRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                VoiceJoinRequest {
                    self_mute: true,
                    self_deaf: false,
                    self_video: true,
                    e2ee: Some(super::call_keys::e2ee_join_full()),
                    recording_notice: Some(super::recording::recording_notice_full()),
                    call_credential: false,
                    origin: Some("wss://jobs.example.com".to_owned()),
                },
            ),
            ("min", VoiceJoinRequest::default()),
            (
                "full_credential",
                VoiceJoinRequest {
                    self_mute: true,
                    call_credential: true,
                    ..VoiceJoinRequest::default()
                },
            ),
        ]
    }
}

/// A credential as a join answer carries it.
pub(super) fn call_credential_full() -> CallCredential {
    CallCredential {
        token: format!("sewcc_q2Vx7mK9pL3nR8sT1wY4zA.{}", "c".repeat(43)),
        expires_at: t(600_000),
        join_id: JoinIdHex::from_bytes([0x5c; 16]),
        device_scope: "3f9a0c47d2e81b65a0c4f7e29d13b8a6".into(),
    }
}

impl Example for CallCredential {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![("full", call_credential_full())]
    }
}

impl Example for CallCredentialRenewResponse {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            CallCredentialRenewResponse {
                token: format!("sewcc_q2Vx7mK9pL3nR8sT1wY4zA.{}", "d".repeat(43)),
                expires_at: t(900_000),
            },
        )]
    }
}

impl Example for VoiceJoinResponse {
    fn examples() -> Vec<(&'static str, Self)> {
        let full = VoiceJoinResponse {
            url: VOICE_URL.into(),
            token: TOKEN.into(),
            expires_at: t(120_000),
            room: super::call_keys::call_id(),
            state: voice_state_min(),
            e2ee: Some(super::call_keys::init_full()),
            media: Some(super::calls::media_policy_full()),
            call_id: Some(super::calls::ID_CALL),
            recording: Some(super::recording::recording_full()),
            stats: Some(CallStatsBeacon {
                interval_ms: CALL_STATS_INTERVAL_MS,
                audio_fields: false,
            }),
            call_credential: None,
        };
        vec![
            ("full", full.clone()),
            (
                "min",
                VoiceJoinResponse {
                    e2ee: None,
                    media: None,
                    call_id: None,
                    stats: None,
                    ..full.clone()
                },
            ),
            (
                "full_credential",
                VoiceJoinResponse {
                    e2ee: None,
                    call_id: None,
                    recording: None,
                    stats: Some(CallStatsBeacon {
                        interval_ms: CALL_STATS_INTERVAL_MS,
                        audio_fields: true,
                    }),
                    call_credential: Some(call_credential_full()),
                    ..full
                },
            ),
        ]
    }
}

impl Example for CallStatsBeacon {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                CallStatsBeacon {
                    interval_ms: CALL_STATS_INTERVAL_MS,
                    audio_fields: true,
                },
            ),
            (
                "min",
                CallStatsBeacon {
                    interval_ms: CALL_STATS_INTERVAL_MS,
                    audio_fields: false,
                },
            ),
        ]
    }
}

impl Example for CallStatsSample {
    fn examples() -> Vec<(&'static str, Self)> {
        let full = CallStatsSample {
            interval_ms: 15_000,
            client: CallStatsClient::Chromium,
            frames_decoded: 450,
            frames_dropped: 3,
            freezes: 1,
            freeze_ms: 420,
            decrypt_failures: 0,
            candidate: Some(CallStatsCandidate::Relay),
            relay_protocol: Some(CallStatsRelayProtocol::Tls),
            rtt_ms: Some(48),
            audio_played_ms: None,
            audio_concealed_ms: None,
            audio_packets_expected: None,
            audio_packets_lost: None,
            audio_jitter_ms: None,
            audio_longest_gap_ms: None,
        };
        vec![
            ("full", full.clone()),
            (
                "full_native",
                CallStatsSample {
                    client: CallStatsClient::Native,
                    frames_decoded: 0,
                    frames_dropped: 0,
                    freezes: 0,
                    freeze_ms: 0,
                    audio_played_ms: Some(29_880),
                    audio_concealed_ms: Some(240),
                    audio_packets_expected: Some(1_500),
                    audio_packets_lost: Some(12),
                    audio_jitter_ms: Some(18),
                    audio_longest_gap_ms: Some(120),
                    ..full.clone()
                },
            ),
            (
                "audio_only",
                CallStatsSample {
                    client: CallStatsClient::Firefox,
                    frames_decoded: 0,
                    frames_dropped: 0,
                    freezes: 0,
                    freeze_ms: 0,
                    candidate: Some(CallStatsCandidate::Host),
                    relay_protocol: None,
                    rtt_ms: None,
                    ..full
                },
            ),
            (
                "min",
                CallStatsSample {
                    client: CallStatsClient::Other,
                    frames_decoded: 0,
                    frames_dropped: 0,
                    freezes: 0,
                    freeze_ms: 0,
                    candidate: None,
                    relay_protocol: None,
                    rtt_ms: None,
                    ..full
                },
            ),
        ]
    }
}

impl Example for VoiceSelfStateRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                VoiceSelfStateRequest {
                    self_mute: Some(true),
                    self_deaf: Some(true),
                    self_video: Some(true),
                    self_stream: Some(false),
                },
            ),
            (
                "mute",
                VoiceSelfStateRequest {
                    self_mute: Some(false),
                    self_deaf: None,
                    self_video: None,
                    self_stream: None,
                },
            ),
            (
                "camera",
                VoiceSelfStateRequest {
                    self_mute: None,
                    self_deaf: None,
                    self_video: Some(true),
                    self_stream: None,
                },
            ),
        ]
    }
}

impl Example for VoiceFlagsRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                VoiceFlagsRequest {
                    server_mute: Some(true),
                    server_deaf: Some(false),
                },
            ),
            (
                "deafen",
                VoiceFlagsRequest {
                    server_mute: None,
                    server_deaf: Some(true),
                },
            ),
        ]
    }
}

impl Example for VoiceFlags {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            VoiceFlags {
                space_id: ID_SPACE,
                member_id: ID_BOB,
                server_mute: true,
                server_deaf: false,
            },
        )]
    }
}

impl Example for VoiceMoveRequest {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                VoiceMoveRequest {
                    channel_id: Patch::Set(ID_OTHER_CHANNEL),
                },
            ),
            (
                "disconnect",
                VoiceMoveRequest {
                    channel_id: Patch::Clear,
                },
            ),
        ]
    }
}

impl Example for VoiceStateUpdated {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                VoiceStateUpdated {
                    member_id: ID_ME,
                    space_id: Some(ID_SPACE),
                    channel_id: Some(ID_CHANNEL),
                    previous_channel_id: Some(ID_OTHER_CHANNEL),
                    state: Some(voice_state_full()),
                    reason: Some(VoiceLeaveReason::Moved),
                },
            ),
            (
                "left",
                VoiceStateUpdated {
                    member_id: ID_ME,
                    space_id: Some(ID_SPACE),
                    channel_id: None,
                    previous_channel_id: Some(ID_CHANNEL),
                    state: None,
                    reason: None,
                },
            ),
        ]
    }
}

fn move_key_join() -> VoiceMoveKeyJoin {
    let join_id: JoinIdHex = match "0123456789abcdef0123456789abcdef".parse() {
        Ok(j) => j,
        Err(e) => panic!("example join id: {e}"),
    };
    VoiceMoveKeyJoin {
        call_id: super::call_keys::call_id(),
        join_id,
        key_join: KeyJoinMode::Required,
    }
}

impl Example for VoiceMoveKeyJoin {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![("full", move_key_join())]
    }
}

impl Example for VoiceServerUpdated {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            VoiceServerUpdated {
                channel_id: ID_OTHER_CHANNEL,
                room: super::call_keys::call_id(),
                url: VOICE_URL.into(),
                token: TOKEN.into(),
                expires_at: t(120_000),
                e2ee: move_key_join(),
                reason: VoiceLeaveReason::Moved,
                media: Some(super::calls::media_policy_full()),
                call_id: Some(super::calls::ID_CALL),
                recording: Some(super::recording::recording_full()),
            },
        )]
    }
}

pub(super) fn ready_voice_channel_full() -> ReadyVoiceChannel {
    ReadyVoiceChannel {
        id: ID_OTHER_CHANNEL,
        space_id: ID_SPACE,
        category_id: Some(ID_CATEGORY),
        slug: slug("lounge"),
        name: "lounge".into(),
        position: 2,
        user_limit: 25,
        my_perms: perms::DEFAULT_EVERYONE,
    }
}

impl Example for ReadyVoiceChannel {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            ("full", ready_voice_channel_full()),
            (
                "min",
                ReadyVoiceChannel {
                    category_id: None,
                    user_limit: 0,
                    ..ready_voice_channel_full()
                },
            ),
        ]
    }
}

impl Example for MeVoice {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            (
                "full",
                MeVoice {
                    state: Some(voice_state_full()),
                },
            ),
            ("min", MeVoice { state: None }),
        ]
    }
}

impl Example for VoiceStatePage {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![(
            "full",
            VoiceStatePage {
                items: vec![voice_state_full(), voice_state_min()],
                has_more: false,
            },
        )]
    }
}

fn engine_full() -> VoiceEngineStatus {
    VoiceEngineStatus {
        state: VoiceEngineState::Down,
        version: Some("0.1.0".into()),
        rooms: 3,
        participants: 11,
        panics_1h: 1,
        down_since: Some(t(60_000)),
        last_event_at: Some(t(59_000)),
        last_snapshot_at: Some(t(30_000)),
        drift_fixes_1h: 2,
    }
}

impl Example for VoiceEngineStatus {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            ("full", engine_full()),
            (
                "min",
                VoiceEngineStatus {
                    state: VoiceEngineState::Up,
                    version: None,
                    rooms: 0,
                    participants: 0,
                    panics_1h: 0,
                    down_since: None,
                    last_event_at: None,
                    last_snapshot_at: None,
                    drift_fixes_1h: 0,
                },
            ),
        ]
    }
}

pub(super) fn voice_status_full() -> VoiceStatus {
    VoiceStatus {
        enabled: true,
        public_url: Some(VOICE_URL.into()),
        calls: 3,
        participants: 11,
        secure_participants: 10,
        reconnecting: 1,
        engine: engine_full(),
        e2ee: Some(super::call_keys::status_e2ee_full()),
        media: Some(super::calls::voice_media_status_full()),
        media_path: Some(super::mgmt_voice::media_path_status_down()),
    }
}

impl Example for VoiceStatus {
    fn examples() -> Vec<(&'static str, Self)> {
        vec![
            ("full", voice_status_full()),
            (
                "min",
                VoiceStatus {
                    public_url: None,
                    e2ee: None,
                    media: None,
                    media_path: None,
                    ..voice_status_full()
                },
            ),
        ]
    }
}
