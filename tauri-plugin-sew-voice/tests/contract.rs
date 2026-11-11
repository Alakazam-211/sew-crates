//! The JSON shapes that cross the IPC bridge (frozen with K2), the provider's
//! HTTP shapes, and that the command list, the build script and the permission set agree.

// The helpers (not only #[test] fns) unwrap: a test that cannot set up must fail loudly.
#![allow(clippy::expect_used)]

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tauri_plugin_sew_voice::fake::fake_answer;
use tauri_plugin_sew_voice::wire::{EVENT_NAMES, EVENT_PREFIX, RECORDING_EVENTS};
use tauri_plugin_sew_voice::{
    COMMANDS, CallState, ConsentArgs, DEFAULT_DEVICE, DETAIL_COMMANDS, DETAIL_EVENTS,
    DEVICE_KEY_COMMANDS, Device, DeviceChoice, DeviceList, EngineError, ErrorInfo, JoinArgs,
    JoinRequest, JoinResult, MuteArgs, PLUGIN_NAME, Participant, PluginEvent, ProviderError,
    RECORDING_COMMANDS, RECORDING_RIGHTS_COMMANDS, RecordingControl, RecordingControls,
    RecordingInfo, RecordingRequest, RecordingRights, RejoinReason, SetDeviceArgs, Speaking,
    StateEvent, StateSnapshot, VIDEO_CODEC_COMMANDS, VIDEO_COMMANDS, app_server_answer,
};
use tauri_plugin_sew_voice::{
    CameraArgs, FrameHeader, LocalVideo, LocalVideoState, PreviewArgs, ScreenKind, ScreenSource,
    SetVideoDeviceArgs, SinkIdArgs, SinkMode, SinkStats, Tile, VideoCodec, VideoDevices,
    VideoPaused, VideoSinkAnswer, VideoSinkArgs, VideoSinkUpdateArgs, VideoSource, VideoTrack,
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
        vec![
            reason::KICKED,
            reason::ROOM_CLOSED,
            reason::CREDENTIAL_LOST,
            reason::CREDENTIAL_EXPIRED
        ]
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
    // Non-exhaustive since 0.5.0: built with `new`, then its public fields.
    let mut reconnecting = StateSnapshot::new(CallState::Reconnecting { attempt: 1 });
    reconnecting.target = Some("k2".into());
    reconnecting.room = Some("123".into());
    reconnecting.call_id = Some("r_x".into());
    exact(
        &reconnecting,
        json!({
            "state": "reconnecting", "attempt": 1, "target": "k2", "room": "123",
            "participants": [], "call_id": "r_x"
        }),
    );
    let idle = StateSnapshot::new(CallState::Idle);
    assert_eq!(idle.local_video, LocalVideo::default());
    exact(
        &idle,
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

/// Sew 0.9.0: Record and Stop as the fixture writes them: the commands (outside the default
/// set), what the host's provider sends (the `RecordingRequest`'s JSON and Sew's route) and
/// the `recording` event after each.
#[test]
fn fixture_recording_controls_round_trip() {
    let f = fixture();
    let c = &f["recording"]["controls"];
    assert_eq!(c["permission"], json!("sew-voice:recording"));
    assert_eq!(RECORDING_COMMANDS, ["start_recording", "stop_recording"]);
    for (k, cmd) in [("start", "start_recording"), ("stop", "stop_recording")] {
        assert_eq!(c[k]["command"], json!(cmd));
        assert_eq!(c[k]["args"], json!({}), "{k} takes no arguments");
        assert!(
            !COMMANDS.contains(&cmd),
            "{cmd} is not in the default eight"
        );
    }
    let start: RecordingRequest =
        serde_json::from_value(c["start"]["provider_request"].clone()).expect("start");
    exact(&start, c["start"]["provider_request"].clone());
    assert_eq!(start.action, RecordingControl::Start);
    assert_eq!(start.body(), json!({}));
    let stop: RecordingRequest =
        serde_json::from_value(c["stop"]["provider_request"].clone()).expect("stop");
    exact(&stop, c["stop"]["provider_request"].clone());
    assert_eq!(stop.action, RecordingControl::Stop);
    assert_eq!(stop.path(), "/api/v1/recordings/48831765147549721/stop");
    // What the engine builds serializes to exactly the fixture's body (the kit's lane);
    // its channel is for a provider that calls Sew itself.
    let channel = "48831765147549698".parse().expect("id");
    let built = RecordingRequest::start("k2:acme", "48831765147549698", channel);
    assert_eq!(
        serde_json::to_value(&built).expect("json"),
        c["start"]["provider_request"]
    );
    assert_eq!(
        built.path(),
        "/api/v1/channels/48831765147549698/recording/start"
    );
    let built = RecordingRequest::stop(
        "k2:acme",
        "48831765147549698",
        channel,
        "48831765147549721".parse().expect("id"),
    );
    assert_eq!(
        serde_json::to_value(&built).expect("json"),
        c["stop"]["provider_request"]
    );
    refused::<RecordingRequest>(json!({"target": "a", "room": "b", "action": "pause"}));
    let started: RecordingInfo =
        serde_json::from_value(c["start"]["recording"].clone()).expect("started");
    exact(&started, c["start"]["recording"].clone());
    assert_eq!(
        started.my_consent, "granted",
        "Record is the starter's own consent"
    );
    assert_eq!(
        (started.trigger.as_str(), started.required),
        ("manual", false)
    );
    // A room that requires recording: `trigger` and `required` as written.
    let req: RecordingInfo =
        serde_json::from_value(c["required_room"]["recording"].clone()).expect("required");
    exact(&req, c["required_room"]["recording"].clone());
    assert_eq!(
        (req.trigger.as_str(), req.required),
        (tauri_plugin_sew_voice::wire::trigger::REQUIRED, true)
    );
    // An older payload without them reads as manual and not required.
    let mut old = c["start"]["recording"].clone();
    let o = old.as_object_mut().expect("object");
    o.remove("trigger");
    o.remove("required");
    let old: RecordingInfo = serde_json::from_value(old).expect("old");
    assert_eq!((old.trigger.as_str(), old.required), ("manual", false));
    assert_eq!(
        tauri_plugin_sew_voice::wire::trigger::of(
            tauri_plugin_sew_voice::sew_call_client::RecordingTrigger::Required
        ),
        "required"
    );
    assert_eq!(c["stop"]["recording"], Value::Null);
    let errors: Vec<&str> = c["errors"]
        .as_array()
        .expect("errors")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for code in [
        tauri_plugin_sew_voice::wire::code::NOT_IN_CALL,
        tauri_plugin_sew_voice::wire::code::NOT_IN_VOICE,
        tauri_plugin_sew_voice::wire::code::NO_RECORDING,
        tauri_plugin_sew_voice::wire::code::RECORDING_UNSUPPORTED,
        tauri_plugin_sew_voice::RECORDING_UNSUPPORTED,
        "recording_required",
    ] {
        assert!(
            errors.contains(&code),
            "{code} is not in the fixture's errors"
        );
    }
}

/// Plugin 0.6.1: the recording rights' command, the `recording_controls` event, the `recording`
/// event's `can_record` and `can_stop` and the `state` command's `recording_controls`, as the
/// fixture writes them; older payloads (no `can_*`) still read.
#[test]
fn fixture_recording_rights_round_trip() {
    let f = fixture();
    let r = &f["recording"]["controls"]["rights"];
    assert_eq!(r["command"], json!("set_recording_rights"));
    assert_eq!(RECORDING_RIGHTS_COMMANDS, ["set_recording_rights"]);
    assert_eq!(r["permission"], json!("sew-voice:recording"));
    let args: RecordingRights = serde_json::from_value(r["args"].clone()).expect("args");
    exact(&args, r["args"].clone());
    assert_eq!(args.room, "48831765147549698");
    let bits = sew_proto::perms::Perms(args.perms().expect("decimal"));
    assert!(bits.contains(sew_proto::perms::Perms::RECORD_CALLS));
    assert!(args.recording_enabled && !args.dm && !args.record_capped && !args.agent);
    // Only `room` and `my_perms` are required; the flags read `false`.
    let short: RecordingRights =
        serde_json::from_value(json!({"room": "1", "my_perms": "0"})).expect("short");
    assert_eq!(short, RecordingRights::new("1", "0"));
    refused::<RecordingRights>(json!({"room": "1"}));
    refused::<RecordingRights>(json!({"my_perms": "0"}));
    refused::<RecordingRights>(json!({"room": "1", "my_perms": "0", "name": "Avery"}));
    assert_eq!(RecordingRights::new("1", "lots").perms(), None);

    assert_eq!(r["event"], json!("sew-voice://recording_controls"));
    for (k, want) in [
        ("may_record", (true, false)),
        ("may_stop", (false, true)),
        ("neither", (false, false)),
    ] {
        let c: RecordingControls = serde_json::from_value(r[k].clone()).expect(k);
        exact(&c, r[k].clone());
        assert_eq!((c.can_record, c.can_stop), want, "{k}");
        assert_eq!(c.is_none(), k == "neither");
        assert_eq!(PluginEvent::RecordingControls(c).payload(), r[k]);
    }
    refused::<RecordingControls>(json!({"can_record": true}));
    // The recording event: `can_stop` written only when true, the rest as Sew 0.9.0 wrote it.
    for k in ["started", "granted", "required_two", "required_alone"] {
        let info: RecordingInfo = serde_json::from_value(r[k].clone()).expect(k);
        exact(&info, r[k].clone());
        assert!(!info.can_record, "{k}: never while a recording is active");
        assert_eq!(info.can_stop, k != "required_two", "{k}");
        assert_eq!(r[k].get("can_stop").is_some(), info.can_stop, "{k}");
    }
    let mut old = r["started"].clone();
    old.as_object_mut().expect("object").remove("can_stop");
    assert_eq!(old, f["recording"]["controls"]["start"]["recording"]);
    let old: RecordingInfo = serde_json::from_value(old).expect("old");
    assert!(!old.can_record && !old.can_stop);
    // The `state` command: `recording_controls` while either is true, absent otherwise.
    let s: StateSnapshot = serde_json::from_value(r["state_snapshot"].clone()).expect("snapshot");
    assert_eq!(
        (
            s.recording_controls.can_record,
            s.recording_controls.can_stop
        ),
        (true, false)
    );
    assert_eq!(serde_json::to_value(&s).expect("json"), r["state_snapshot"]);
    let none: StateSnapshot =
        serde_json::from_value(f["state_snapshot"]["moved"].clone()).expect("no controls");
    assert!(none.recording_controls.is_none());
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
            "recording",
            "video",
            "local_video",
            "video_devices",
            "policy"
        ]
    );
    let events = [
        PluginEvent::State(StateEvent::bare(CallState::Idle)),
        PluginEvent::Participants(vec![]),
        PluginEvent::Speaking(vec![]),
        PluginEvent::Errors(ErrorInfo::new("x")),
        PluginEvent::Devices(DeviceList::default()),
        PluginEvent::Recording(None),
        PluginEvent::Video(vec![]),
        PluginEvent::LocalVideo(LocalVideo::default()),
        PluginEvent::VideoDevices(VideoDevices::default()),
        PluginEvent::Policy(json!({})),
    ];
    let names: Vec<&str> = events.iter().map(PluginEvent::name).collect();
    assert_eq!(names, EVENT_NAMES);
    assert_eq!(events[0].event_name(), "sew-voice://state");
    // The call-detail events (0.4.1), beside the six of voice and the four of video.
    assert_eq!(DETAIL_EVENTS, ["security", "roster", "quality", "incoming"]);
    let details = [
        PluginEvent::Security(None),
        PluginEvent::Roster(vec![]),
        PluginEvent::Quality(tauri_plugin_sew_voice::QualityInfo::unknown()),
        PluginEvent::Incoming(None),
    ];
    let names: Vec<&str> = details.iter().map(PluginEvent::name).collect();
    assert_eq!(names, DETAIL_EVENTS);
    assert_eq!(details[2].payload(), json!({"quality": "unknown"}));
    assert_eq!(details[3].event_name(), "sew-voice://incoming");
    // The recording controls' event (0.6.1).
    assert_eq!(RECORDING_EVENTS, ["recording_controls"]);
    let controls = PluginEvent::RecordingControls(RecordingControls::default());
    assert_eq!(controls.name(), RECORDING_EVENTS[0]);
    assert_eq!(controls.event_name(), "sew-voice://recording_controls");
    assert_eq!(
        controls.payload(),
        json!({"can_record": false, "can_stop": false})
    );
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
    assert_eq!(DEVICE_KEY_COMMANDS, ["forget_server", "known_device_keys"]);
    assert_eq!(DETAIL_COMMANDS, ["call_details", "ring", "ring_ended"]);
    assert_eq!(
        VIDEO_COMMANDS,
        [
            "video_caps",
            "video_devices",
            "set_video_device",
            "camera",
            "screen_sources",
            "screen",
            "preview",
            "video_sink",
            "video_sink_update",
            "video_sink_close",
            "request_keyframe",
            "camera_permission",
            "request_camera_permission"
        ]
    );
    assert_eq!(VIDEO_CODEC_COMMANDS, ["set_video_codec_preference"]);
    let all: Vec<&str> = COMMANDS
        .iter()
        .chain(DEVICE_KEY_COMMANDS.iter())
        .chain(DETAIL_COMMANDS.iter())
        .chain(VIDEO_COMMANDS.iter())
        .chain(VIDEO_CODEC_COMMANDS.iter())
        .chain(RECORDING_COMMANDS.iter())
        .chain(RECORDING_RIGHTS_COMMANDS.iter())
        .copied()
        .collect();
    assert_eq!(
        listed, all,
        "build.rs's COMMANDS: the eight, the device-key commands, the call-detail ones, the video commands, the codec setting, the recording commands, then the recording rights"
    );
    for c in all {
        let generated = manifest_file(&format!("permissions/autogenerated/commands/{c}.toml"));
        assert!(
            generated.contains(&format!("identifier = \"allow-{}\"", c.replace('_', "-"))),
            "no generated permission for {c}"
        );
    }
    // The device-key commands are never in the default set: only an explicit permission (or
    // the `device-keys` set) allows them.
    let keys = manifest_file("permissions/device-keys.toml");
    assert!(keys.contains("identifier = \"device-keys\""));
    for c in DEVICE_KEY_COMMANDS {
        let perm = format!("\"allow-{}\"", c.replace('_', "-"));
        assert!(keys.contains(&perm), "sew-voice:device-keys lacks {perm}");
    }
    // The video commands: the `video` set has all of them, the default set none.
    let video = manifest_file("permissions/video.toml");
    assert!(video.contains("identifier = \"video\""));
    for c in VIDEO_COMMANDS.iter().chain(VIDEO_CODEC_COMMANDS.iter()) {
        let perm = format!("\"allow-{}\"", c.replace('_', "-"));
        assert!(video.contains(&perm), "sew-voice:video lacks {perm}");
    }
    assert_eq!(
        video.matches("\"allow-").count(),
        VIDEO_COMMANDS.len() + VIDEO_CODEC_COMMANDS.len()
    );
    let default = manifest_file("permissions/default.toml");
    for c in VIDEO_COMMANDS.iter().chain(VIDEO_CODEC_COMMANDS.iter()) {
        let perm = format!("allow-{}", c.replace('_', "-"));
        assert!(
            !default.contains(&perm),
            "sew-voice:default must not allow {c}"
        );
    }
    for c in DEVICE_KEY_COMMANDS
        .iter()
        .chain(DETAIL_COMMANDS.iter())
        .chain(RECORDING_COMMANDS.iter())
        .chain(RECORDING_RIGHTS_COMMANDS.iter())
    {
        let perm = format!("allow-{}", c.replace('_', "-"));
        assert!(
            !default.contains(&perm),
            "sew-voice:default must not allow {c}"
        );
    }
    // The recording commands (Sew 0.9.0) and the recording rights (0.6.1) are the `recording`
    // set, and only them.
    let recording = manifest_file("permissions/recording.toml");
    assert!(recording.contains("identifier = \"recording\""));
    for c in RECORDING_COMMANDS
        .iter()
        .chain(RECORDING_RIGHTS_COMMANDS.iter())
    {
        let perm = format!("\"allow-{}\"", c.replace('_', "-"));
        assert!(
            recording.contains(&perm),
            "sew-voice:recording lacks {perm}"
        );
    }
    assert_eq!(
        recording.matches("\"allow-").count(),
        RECORDING_COMMANDS.len() + RECORDING_RIGHTS_COMMANDS.len()
    );
    // The call-detail commands are the `call-details` set (0.4.1).
    let details = manifest_file("permissions/call-details.toml");
    assert!(details.contains("identifier = \"call-details\""));
    for c in DETAIL_COMMANDS {
        let perm = format!("\"allow-{}\"", c.replace('_', "-"));
        assert!(
            details.contains(&perm),
            "sew-voice:call-details lacks {perm}"
        );
    }
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

// ───────────────────────────── video ─────────────────────────────

#[test]
fn video_track_and_local_video_shapes() {
    let t = VideoTrack {
        member_id: "48831765147549700".into(),
        is_self: false,
        source: VideoSource::Screen,
        track: "TR_VSabc".into(),
        codec: VideoCodec::H264,
        width: 1920,
        height: 1080,
        muted: false,
        paused: Some(VideoPaused::Congestion),
        unsupported: false,
    };
    exact(
        &t,
        json!({"member_id": "48831765147549700", "is_self": false, "source": "screen",
               "track": "TR_VSabc", "codec": "h264", "width": 1920, "height": 1080,
               "muted": false, "paused": "congestion"}),
    );
    exact(
        &VideoTrack {
            unsupported: true,
            paused: None,
            ..t
        },
        json!({"member_id": "48831765147549700", "is_self": false, "source": "screen",
               "track": "TR_VSabc", "codec": "h264", "width": 1920, "height": 1080,
               "muted": false, "paused": null, "unsupported": true}),
    );
    exact(
        &LocalVideo {
            camera: LocalVideoState::Asking,
            screen: LocalVideoState::On,
            reason: None,
        },
        json!({"camera": "asking", "screen": "on"}),
    );
    refused::<VideoTrack>(
        json!({"member_id": "1", "is_self": false, "source": "camera",
        "track": "t", "codec": "av1", "width": 1, "height": 1, "muted": false, "paused": null}),
    );
}

#[test]
fn video_command_arguments() {
    let a: VideoSinkArgs = serde_json::from_value(json!({
        "track": "local:camera", "width": 640, "height": 360, "visible": true, "tile": "large",
        "decode": ["vp8", "h264"]
    }))
    .expect("sink args");
    assert_eq!(a.tile, Tile::Large);
    refused::<VideoSinkArgs>(
        json!({"track": "t", "width": 1, "height": 1, "visible": true,
        "tile": "huge", "decode": []}),
    );
    let u: VideoSinkUpdateArgs = serde_json::from_value(json!({
        "sink_id": 7, "visible": false,
        "stats": {"decoded": 30, "dropped": 2, "freezes": 1, "freeze_ms": 250}
    }))
    .expect("update");
    assert_eq!(u.sink_id, 7);
    assert_eq!(u.visible, Some(false));
    assert_eq!(
        u.stats,
        Some(SinkStats {
            decoded: 30,
            dropped: 2,
            freezes: 1,
            freeze_ms: 250
        })
    );
    refused::<VideoSinkUpdateArgs>(json!({"sink_id": 7, "colour": "red"}));
    exact(&SinkIdArgs { sink_id: 3 }, json!({"sink_id": 3}));
    exact(
        &CameraArgs {
            on: true,
            device: None,
        },
        json!({"on": true, "device": null}),
    );
    let p: PreviewArgs = serde_json::from_value(json!({"on": false})).expect("preview");
    assert!(!p.on && p.device.is_none());
    exact(
        &SetVideoDeviceArgs {
            camera: "default".into(),
        },
        json!({"camera": "default"}),
    );
    exact(
        &VideoSinkAnswer {
            sink_id: 1,
            codec: Some(VideoCodec::Vp8),
            mode: SinkMode::Encoded,
        },
        json!({"sink_id": 1, "codec": "vp8", "mode": "encoded"}),
    );
    exact(
        &ScreenSource {
            id: "display:1".into(),
            kind: ScreenKind::Display,
            title: "Built-in display".into(),
            app: None,
            width: 3024,
            height: 1964,
            thumbnail_png: None,
        },
        json!({"id": "display:1", "kind": "display", "title": "Built-in display",
               "width": 3024, "height": 1964}),
    );
}

#[test]
fn the_frame_header_is_32_little_endian_bytes() {
    let h = FrameHeader {
        kind: 1,
        codec: 2,
        flags: 0b011,
        sink_id: 0x0102_0304,
        timestamp_us: 0x1112_1314_1516_1718,
        width: 1280,
        height: 720,
        seq: 9,
        len: 0,
    };
    let msg = h.message(&[0xAA, 0xBB, 0xCC]);
    assert_eq!(msg.len(), 35);
    assert_eq!(
        &msg[..32],
        &[
            1, 1, 2, 3, // version, kind, codec, flags
            4, 3, 2, 1, // sink_id
            0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11, // timestamp_us
            0x00, 0x05, // width 1280
            0xD0, 0x02, // height 720
            9, 0, 0, 0, // seq
            3, 0, 0, 0, // payload length
            0, 0, 0, 0, // reserved
        ]
    );
    assert_eq!(&msg[32..], &[0xAA, 0xBB, 0xCC]);
    assert_eq!(FrameHeader::parse(&msg), Some(FrameHeader { len: 3, ..h }));
    // Short, another version, or a length that doesn't match: refused.
    assert_eq!(FrameHeader::parse(&msg[..31]), None);
    assert_eq!(FrameHeader::parse(&msg[..34]), None);
    let mut v2 = msg.clone();
    v2[0] = 2;
    assert_eq!(FrameHeader::parse(&v2), None);
}
