//! The JSON shapes that cross the IPC bridge (frozen with K2), the provider's
//! HTTP shapes, and that the command list, the build script and the permission set agree.

// The helpers (not only #[test] fns) unwrap: a test that cannot set up must fail loudly.
#![allow(clippy::expect_used)]

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tauri_plugin_sew_voice::fake::fake_answer;
use tauri_plugin_sew_voice::wire::{EVENT_NAMES, EVENT_PREFIX};
use tauri_plugin_sew_voice::{
    COMMANDS, CallState, ConsentArgs, DEFAULT_DEVICE, Device, DeviceChoice, DeviceList,
    EngineError, ErrorInfo, JoinArgs, JoinRequest, JoinResult, MuteArgs, PLUGIN_NAME, Participant,
    PluginEvent, ProviderError, RecordingInfo, RejoinReason, SetDeviceArgs, Speaking, StateEvent,
    StateSnapshot, app_server_answer,
};

/// `v` serializes to exactly `want` and `want` parses back to `v`.
fn exact<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(v: &T, want: Value) {
    assert_eq!(serde_json::to_value(v).expect("serialize"), want);
    let back: T = serde_json::from_value(want.clone()).expect("deserialize");
    assert_eq!(&back, v, "round trip of {want}");
}

fn refused<T: DeserializeOwned + std::fmt::Debug>(v: Value) {
    let r = serde_json::from_value::<T>(v.clone());
    assert!(r.is_err(), "{v} should not parse, got {r:?}");
}

#[test]
fn state_values() {
    exact(&CallState::Idle, json!({"state": "idle"}));
    exact(&CallState::Connecting, json!({"state": "connecting"}));
    exact(&CallState::Connected, json!({"state": "connected"}));
    exact(
        &CallState::Reconnecting { attempt: 3 },
        json!({"state": "reconnecting", "attempt": 3}),
    );
    exact(
        &CallState::Ended {
            reason: "channel_archived".into(),
        },
        json!({"state": "ended", "reason": "channel_archived"}),
    );
    exact(
        &CallState::ConsentRequired {
            recording_id: "48831765147549720".into(),
            notice_version: 3141,
        },
        json!({"state": "consent_required", "recording_id": "48831765147549720", "notice_version": 3141}),
    );
    assert!(
        CallState::ConsentRequired {
            recording_id: "auto".into(),
            notice_version: 1
        }
        .is_live(),
        "a held join is a call: a second join is refused, leave ends it"
    );
    refused::<CallState>(json!({"state": "ringing"}));
    refused::<CallState>(json!({"state": "reconnecting"}));
    refused::<CallState>(json!({"state": "ended"}));
    refused::<CallState>(json!({"state": "consent_required", "recording_id": "1"}));
}

#[test]
fn participant_has_no_name() {
    let p = Participant {
        member_id: "48831765147549699".into(),
        is_self: true,
        muted: true,
        deafened: false,
        server_muted: false,
        server_deafened: true,
        speak_denied: false,
    };
    // `speak_denied` (Sew 0.7.2) is written only when true: this is the 0.7.1 shape.
    exact(
        &p,
        json!({
            "member_id": "48831765147549699", "is_self": true, "muted": true,
            "deafened": false, "server_muted": false, "server_deafened": true
        }),
    );
    refused::<Participant>(json!({
        "member_id": "1", "is_self": true, "muted": true, "deafened": false,
        "server_muted": false, "server_deafened": true, "name": "Avery"
    }));
    refused::<Participant>(json!({"member_id": "1", "is_self": true}));
}

#[test]
fn participant_speak_denied_is_its_own_flag() {
    let p = Participant {
        member_id: "48831765147549700".into(),
        is_self: false,
        muted: true,
        deafened: false,
        server_muted: false,
        server_deafened: false,
        speak_denied: true,
    };
    exact(
        &p,
        json!({
            "member_id": "48831765147549700", "is_self": false, "muted": true,
            "deafened": false, "server_muted": false, "server_deafened": false,
            "speak_denied": true
        }),
    );
    // An explicit `false` reads as absent.
    let f: Participant = serde_json::from_value(json!({
        "member_id": "1", "is_self": true, "muted": false, "deafened": false,
        "server_muted": false, "server_deafened": false, "speak_denied": false
    }))
    .expect("explicit false");
    assert!(!f.speak_denied);
    refused::<Participant>(json!({
        "member_id": "1", "is_self": true, "muted": false, "deafened": false,
        "server_muted": false, "server_deafened": false, "speak_denied": "yes"
    }));
}

/// The contract fixture K2 tests against (tests/native-voice-contract.json, Sew 0.7.2).
fn fixture() -> Value {
    serde_json::from_str(include_str!("native-voice-contract.json")).expect("the fixture")
}

#[test]
fn fixture_ended_payloads_are_the_state_event() {
    use tauri_plugin_sew_voice::wire::reason;
    let f = fixture();
    assert_eq!(f["ended"]["event"], json!("sew-voice://state"));
    let reasons = |key: &str| -> Vec<String> {
        f["ended"][key]
            .as_array()
            .expect(key)
            .iter()
            .map(|c| {
                let r = c["reason"].as_str().expect("reason").to_owned();
                let ev = PluginEvent::State(StateEvent {
                    state: CallState::Ended { reason: r.clone() },
                    target: Some("k2:acme".into()),
                    room: Some("48831765147549698".into()),
                });
                assert_eq!(ev.event_name(), "sew-voice://state");
                assert_eq!(ev.payload(), c["payload"], "{r}");
                let back: CallState =
                    serde_json::from_value(c["payload"].clone()).expect("payload");
                assert_eq!(back, CallState::Ended { reason: r.clone() });
                r
            })
            .collect()
    };
    assert_eq!(reasons("sew_reasons"), reason::SEW.to_vec());
    assert_eq!(
        reasons("fallbacks"),
        vec![reason::KICKED, reason::ROOM_CLOSED, reason::CREDENTIAL_LOST]
    );
    assert_eq!(reasons("recording_reasons"), reason::RECORDING.to_vec());
    // The kicked fallback is also what a Sew older than 0.7.1 ends a removal with.
    assert!(
        f["ended"]["fallbacks"][0]["cause"]
            .as_str()
            .expect("cause")
            .contains("older than 0.7.1")
    );
}

/// The `state` event says which target and room the call is on; a move changes `room`.
#[test]
fn state_event_carries_target_and_room() {
    let f = fixture();
    assert_eq!(f["moved"]["event"], json!("sew-voice://state"));
    let moved = StateEvent {
        state: CallState::Connected,
        target: Some("k2:acme".into()),
        room: Some("48831765147549710".into()),
    };
    exact(&moved, f["moved"]["payload"].clone());
    assert_eq!(PluginEvent::State(moved).payload(), f["moved"]["payload"]);
    // Idle has neither (the shape before 0.7.2).
    exact(&StateEvent::bare(CallState::Idle), json!({"state": "idle"}));
    exact(
        &StateEvent {
            state: CallState::Reconnecting { attempt: 2 },
            target: Some("acme".into()),
            room: Some("123".into()),
        },
        json!({"state": "reconnecting", "attempt": 2, "target": "acme", "room": "123"}),
    );
    // An older plugin's payload (no target, no room) still reads.
    let old: StateEvent = serde_json::from_value(json!({"state": "connected"})).expect("old");
    assert_eq!(old, StateEvent::bare(CallState::Connected));
}

#[test]
fn fixture_participants_and_snapshot_round_trip() {
    let f = fixture();
    assert_eq!(
        f["participants"]["event"],
        json!("sew-voice://participants")
    );
    let cases = f["participants"].as_object().expect("participants");
    let mut n = 0;
    for (name, v) in cases {
        if name == "event" {
            continue;
        }
        let list: Vec<Participant> =
            serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            PluginEvent::Participants(list.clone()).payload(),
            *v,
            "{name}"
        );
        for p in &list {
            // SPEAK denied always shows muted; it is never a moderator mute.
            if p.speak_denied {
                assert!(p.muted && !p.server_muted, "{name}: {p:?}");
            }
        }
        n += 1;
    }
    assert_eq!(n, 6, "every participants case is checked");
    let snap = &f["state_snapshot"]["speak_denied"];
    exact(
        &serde_json::from_value::<StateSnapshot>(snap.clone()).expect("snapshot"),
        snap.clone(),
    );
}

#[test]
fn speaking_devices_and_errors() {
    exact(
        &vec![Speaking {
            member_id: "7".into(),
            level: 0.25,
        }],
        json!([{"member_id": "7", "level": 0.25}]),
    );
    let list = DeviceList {
        inputs: vec![Device {
            id: "in:1".into(),
            name: "Mic".into(),
            default: true,
        }],
        outputs: vec![],
        input: Some("in:1".into()),
        output: None,
    };
    exact(
        &list,
        json!({
            "inputs": [{"id": "in:1", "name": "Mic", "default": true}],
            "outputs": [], "input": "in:1", "output": null
        }),
    );
    exact(
        &ErrorInfo::new("mic_denied"),
        json!({"code": "mic_denied", "detail": null}),
    );
    exact(
        &ErrorInfo::with_detail("voice_channel_full", json!({"limit": 25})),
        json!({"code": "voice_channel_full", "detail": {"limit": 25}}),
    );
}

#[test]
fn command_arguments_and_answers() {
    // join {target, room, muted?, deafened?} -> {call_id}
    let min: JoinArgs =
        serde_json::from_value(json!({"target": "k2", "room": "123"})).expect("min join");
    assert_eq!(min.muted, None);
    assert_eq!(min.deafened, None);
    exact(
        &JoinArgs {
            target: "k2".into(),
            room: "123".into(),
            muted: Some(true),
            deafened: Some(false),
        },
        json!({"target": "k2", "room": "123", "muted": true, "deafened": false}),
    );
    // No token in: a join carrying one is refused.
    refused::<JoinArgs>(json!({"target": "k2", "room": "1", "token": "x"}));
    refused::<JoinArgs>(json!({"room": "1"}));
    exact(
        &JoinResult {
            call_id: "r_x".into(),
        },
        json!({"call_id": "r_x"}),
    );
    // mute {muted?, deafened?}
    let m: MuteArgs = serde_json::from_value(json!({"deafened": true})).expect("mute");
    assert_eq!(
        m,
        MuteArgs {
            muted: None,
            deafened: Some(true)
        }
    );
    refused::<MuteArgs>(json!({"mute": true}));
    // set_device {input?, output?}
    let s: SetDeviceArgs = serde_json::from_value(json!({"output": "out:2"})).expect("set");
    assert_eq!(s.output.as_deref(), Some("out:2"));
    assert_eq!(s.input, None);
    assert_eq!(s.output_choice(), Some(DeviceChoice::Id("out:2".into())));
    assert_eq!(s.input_choice(), None);
    refused::<SetDeviceArgs>(json!({"speaker": "out:2"}));
    // "default" is the OS default (followed when it changes); null is "unchanged".
    assert_eq!(DEFAULT_DEVICE, "default");
    let s: SetDeviceArgs =
        serde_json::from_value(json!({"input": "default", "output": null})).expect("default");
    assert_eq!(s.input_choice(), Some(DeviceChoice::Default));
    assert_eq!(s.output_choice(), None);
}

#[test]
fn state_snapshot_is_the_state_event_plus_roster() {
    exact(
        &StateSnapshot {
            state: CallState::Reconnecting { attempt: 1 },
            target: Some("k2".into()),
            room: Some("123".into()),
            participants: vec![],
            call_id: Some("r_x".into()),
            recording: None,
        },
        json!({
            "state": "reconnecting", "attempt": 1, "target": "k2", "room": "123",
            "participants": [], "call_id": "r_x"
        }),
    );
    exact(
        &StateSnapshot {
            state: CallState::Idle,
            target: None,
            room: None,
            participants: vec![],
            call_id: None,
            recording: None,
        },
        json!({"state": "idle", "participants": [], "call_id": null}),
    );
    // The fixture's snapshots (after SPEAK lost, after a move) parse.
    let f = fixture();
    for k in ["speak_denied", "moved"] {
        let s: StateSnapshot =
            serde_json::from_value(f["state_snapshot"][k].clone()).expect("snapshot");
        assert_eq!(s.target.as_deref(), Some("k2:acme"), "{k}");
        assert_eq!(
            serde_json::to_value(&s).expect("json"),
            f["state_snapshot"][k]
        );
    }
}

/// Sew 0.7.3: the held join's `state` and `recording`, the `recording` event in a call and once
/// it stopped, the `consent` arguments and the `state` command's `recording`, as the fixture
/// writes them.
#[test]
fn fixture_recording_round_trips() {
    let f = fixture();
    let r = &f["recording"];
    assert_eq!(r["event"], json!("sew-voice://recording"));
    let held: StateEvent = serde_json::from_value(r["held"]["state"].clone()).expect("held");
    assert_eq!(
        held.state,
        CallState::ConsentRequired {
            recording_id: "48831765147549720".into(),
            notice_version: 3141
        }
    );
    exact(&held, r["held"]["state"].clone());
    for k in ["granted", "withdrawn"] {
        let info: RecordingInfo = serde_json::from_value(r[k].clone()).expect(k);
        exact(&info, r[k].clone());
        assert!(info.consent_prompt.is_none(), "{k}: answered, no prompt");
        let ev = PluginEvent::Recording(Some(info));
        assert_eq!(ev.event_name(), "sew-voice://recording");
        assert_eq!(ev.payload(), r[k], "{k}");
    }
    let pending: RecordingInfo =
        serde_json::from_value(r["held"]["recording"].clone()).expect("pending");
    exact(&pending, r["held"]["recording"].clone());
    let prompt = pending
        .consent_prompt
        .as_ref()
        .expect("the notice while pending");
    assert_eq!(prompt.notice_version, 3141);
    assert_eq!(pending.my_consent, "pending");
    // The recorder is no participant: no name, no member, only `recorder_present`.
    assert!(pending.participants.is_empty());
    assert_eq!(r["stopped"], Value::Null);
    assert_eq!(PluginEvent::Recording(None).payload(), Value::Null);
    let args: ConsentArgs =
        serde_json::from_value(r["held"]["consent"]["args"].clone()).expect("args");
    exact(&args, json!({"granted": true, "notice_version": 3141}));
    assert!(COMMANDS.contains(&"consent") && COMMANDS.contains(&"withdraw_consent"));
    refused::<ConsentArgs>(json!({"granted": true}));
    refused::<ConsentArgs>(json!({"granted": true, "notice_version": 1, "device": "x"}));
    let mut extra = r["granted"].clone();
    extra["name"] = json!("Avery");
    refused::<RecordingInfo>(extra);
    // The `state` command while held: no call id yet, the notice in `recording`.
    let s: StateSnapshot =
        serde_json::from_value(f["state_snapshot"]["consent_required"].clone()).expect("snap");
    assert_eq!(s.call_id, None);
    assert_eq!(s.recording.as_ref(), Some(&pending));
    assert_eq!(
        serde_json::to_value(&s).expect("json"),
        f["state_snapshot"]["consent_required"]
    );
}

/// A notice that changed before the answer reached Sew: the command error carries Sew's
/// refusal details for the app to show the new notice.
#[test]
fn a_changed_notice_is_the_consent_error() {
    let c = tauri_plugin_sew_voice::fake::consent_required(48831765147549720, 7);
    let e = EngineError::Provider(ProviderError::ConsentRequired(Box::new(c)));
    let info = e.to_info();
    assert_eq!(info.code, "recording_consent_required");
    assert_eq!(info.detail["consent_prompt"]["notice_version"], json!(7));
    assert_eq!(info.detail["auto"], json!(false));
    assert_eq!(e.end_reason(), "recording_consent_required");
}

#[test]
fn event_names_are_frozen() {
    assert_eq!(PLUGIN_NAME, "sew-voice");
    assert_eq!(EVENT_PREFIX, "sew-voice://");
    assert_eq!(
        EVENT_NAMES,
        [
            "state",
            "participants",
            "speaking",
            "errors",
            "devices",
            "recording"
        ]
    );
    let events = [
        PluginEvent::State(StateEvent::bare(CallState::Idle)),
        PluginEvent::Participants(vec![]),
        PluginEvent::Speaking(vec![]),
        PluginEvent::Errors(ErrorInfo::new("x")),
        PluginEvent::Devices(DeviceList::default()),
        PluginEvent::Recording(None),
    ];
    let names: Vec<&str> = events.iter().map(PluginEvent::name).collect();
    assert_eq!(names, EVENT_NAMES);
    assert_eq!(events[0].event_name(), "sew-voice://state");
    assert_eq!(events[0].payload(), json!({"state": "idle"}));
    assert_eq!(events[1].payload(), json!([]));
}

fn manifest_file(rel: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn commands_build_script_and_permissions_agree() {
    assert_eq!(
        COMMANDS,
        [
            "join",
            "leave",
            "mute",
            "devices",
            "set_device",
            "state",
            "consent",
            "withdraw_consent"
        ]
    );
    let build = manifest_file("build.rs");
    let start = build
        .find("const COMMANDS: &[&str] = &[")
        .expect("build.rs's COMMANDS");
    let list = &build[start..];
    let list = &list[list.find("= &[").expect("[") + 4..list.find("];").expect("];")];
    let listed: Vec<String> = list
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(listed, COMMANDS, "build.rs's COMMANDS");
    let default = manifest_file("permissions/default.toml");
    assert!(default.contains("[default]"));
    for c in COMMANDS {
        let perm = format!("\"allow-{}\"", c.replace('_', "-"));
        assert!(default.contains(&perm), "sew-voice:default lacks {perm}");
    }
    assert_eq!(
        default.matches("\"allow-").count(),
        COMMANDS.len(),
        "sew-voice:default allows exactly the eight commands"
    );
}

#[test]
fn provider_http_shapes() {
    // What the example's provider POSTs to the App's server.
    let req = JoinRequest {
        target: "acme".into(),
        room: "48831765147549698".into(),
        muted: false,
        deafened: false,
        rejoin: None,
        recording_notice: None,
    };
    exact(
        &req,
        json!({"target": "acme", "room": "48831765147549698", "muted": false, "deafened": false, "rejoin": null}),
    );
    exact(
        &JoinRequest {
            rejoin: Some(RejoinReason::AdmitLost),
            ..req
        },
        json!({"target": "acme", "room": "48831765147549698", "muted": false, "deafened": false, "rejoin": "admit_lost"}),
    );
    exact(
        &RejoinReason::CredentialExpired,
        json!("credential_expired"),
    );
    // Sew 0.7.3: the answer to a held join's notice, forwarded as the voice join's
    // `recording_notice` (written only when there is one).
    let held = tauri_plugin_sew_voice::fake::consent_required(48831765147549720, 3141);
    exact(
        &JoinRequest {
            target: "acme".into(),
            room: "48831765147549698".into(),
            muted: false,
            deafened: false,
            rejoin: None,
            recording_notice: Some(held.notice(true)),
        },
        json!({"target": "acme", "room": "48831765147549698", "muted": false, "deafened": false,
               "rejoin": null,
               "recording_notice": {"recording_id": "48831765147549720", "consent": true,
                                    "notice_version": 3141}}),
    );
    // And what it answers: Sew's VoiceJoinResponse unchanged, plus the door.
    let answer = fake_answer("48831765147549699").expect("fake answer");
    assert_eq!(answer.api_base.as_str(), "https://sew.invalid/");
    assert_eq!(answer.join.room.as_str(), "r_fakeroomfakeroomfakeroomfa");
    assert_eq!(answer.join.token, "fake-media-token");
    assert!(answer.call_credential().is_none());
    // `join` keeps Sew's call credential (the engine's key traffic, leave, renewal).
    let body = json!({
        "join": {
            "url": "wss://voice.sew.invalid",
            "token": "fake-media-token",
            "expires_at": "2026-11-02T12:02:00.000Z",
            "room": "r_fakeroomfakeroomfakeroomfa",
            "state": {
                "member_id": "48831765147549699",
                "channel_id": "48831765147549698",
                "connection": "connecting",
                "secure": false,
                "self_mute": false,
                "self_deaf": false,
                "server_deaf": false,
                "server_mute": false,
                "speak_denied": false,
                "joined_at": "2026-11-02T12:00:00.000Z"
            },
            "call_credential": {
                "token": "sewcc_sel.secret",
                "expires_at": "2026-11-02T12:10:00Z",
                "join_id": "0123456789abcdef0123456789abcdef",
                "device_scope": "fedcba9876543210fedcba9876543210"
            }
        },
        "api_base": "https://example.com/_k2/voice/sew/"
    });
    let with = app_server_answer(&serde_json::to_vec(&body).expect("json")).expect("parse");
    assert_eq!(with.api_base.as_str(), "https://example.com/_k2/voice/sew/");
    let cred = with.call_credential().expect("call_credential");
    assert_eq!(cred.join_id, "0123456789abcdef0123456789abcdef");
    // A body that does not parse fails without quoting it (it carries tokens).
    for bad in [
        json!({"join": {"token": "sewcc_x.y"}, "api_base": "https://x.invalid/"}),
        json!({"api_base": "https://x.invalid/"}),
        json!({"join": body["join"], "api_base": "not a url"}),
    ] {
        match app_server_answer(&serde_json::to_vec(&bad).expect("json")) {
            Err(ProviderError::Other(s)) => {
                assert!(
                    !s.contains("sewcc_") && !s.contains("fake-media-token"),
                    "{s}"
                )
            }
            other => panic!("{bad}: {other:?}"),
        }
    }
}
