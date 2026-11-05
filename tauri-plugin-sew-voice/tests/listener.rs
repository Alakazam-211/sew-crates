//! The native listener and control (0.3.0): every scenario of
//! `tests/sew-native-voice-listener.json`, driven through `VoiceControl` against the fake
//! engine, gives exactly the fixture's listener calls; every argument round-trips through the
//! wire types; the listener sees what the webview sees, in the same order; and the control's
//! own rules (leave twice, `not_in_call`, the network and audio-session pass-through).

// The helpers (not only #[test] fns) unwrap: a test that cannot set up must fail loudly.
#![allow(clippy::expect_used)]

use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tauri_plugin_sew_voice::fake::{
    FakeEngine, FakeProvider, RecordingListener, RecordingSink, consent_required,
};
use tauri_plugin_sew_voice::{
    AudioSessionEvent, CallEvent, DeviceList, ErrorInfo, JoinArgs, Participant, ProviderError,
    RecordingInfo, SelfMute, Spawn, StateEvent, VoiceControl, VoiceService,
};

const ME: &str = "48831765147549699";

struct World {
    control: VoiceControl,
    engine: Arc<FakeEngine>,
    sink: Arc<RecordingSink>,
    listener: Arc<RecordingListener>,
}

fn world(provider: FakeProvider) -> World {
    let engine = Arc::new(FakeEngine::new());
    let sink = Arc::new(RecordingSink::new());
    let listener = Arc::new(RecordingListener::new());
    let spawn: Spawn = Arc::new(|f| {
        tokio::spawn(f);
    });
    let svc = VoiceService::with_listener(
        engine.clone(),
        Arc::new(provider),
        sink.clone(),
        listener.clone(),
        spawn,
    );
    svc.start();
    World {
        control: VoiceControl::new(svc),
        engine,
        sink,
        listener,
    }
}

/// Let spawned tasks run until `pred` holds; panics (loudly) if it never does.
async fn until(what: &str, mut pred: impl FnMut() -> bool) {
    for _ in 0..2000 {
        if pred() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("timed out waiting for: {what}");
}

/// Let spawned tasks run for a while (to show that nothing more arrives).
async fn settle() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("sew-native-voice-listener.json")).expect("the fixture")
}

fn from<T: DeserializeOwned>(v: &Value, what: &str) -> T {
    serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{what}: {e}: {v}"))
}

/// One step of a scenario, through the control (the host) or the fake engine (Sew).
async fn run_step(w: &World, step: &Value, what: &str) {
    let call = || w.engine.last_call().expect("a call");
    match step["do"].as_str().expect("do") {
        "join" => {
            let args: JoinArgs = from(&step["args"], what);
            let r = w.control.join(args).await.expect(what);
            assert_eq!(json!({"call_id": r.call_id}), step["answer"], "{what}");
        }
        "consent" => {
            let granted = step["granted"].as_bool().expect("granted");
            let v = step["notice_version"].as_u64().expect("notice_version");
            let v = u32::try_from(v).expect("u32");
            w.control.consent(granted, v).await.expect(what);
        }
        "withdraw_consent" => w.control.withdraw_consent().await.expect(what),
        "leave" => w.control.leave().await.expect(what),
        "set_muted" => {
            let m = step["muted"].as_bool().expect("muted");
            w.control.set_muted(m).await.expect(what);
        }
        "sew_recording" => {
            let r: Option<RecordingInfo> = from(&step["recording"], what);
            call().set_recording(r);
        }
        "engine_participants" => {
            let p: Vec<Participant> = from(&step["participants"], what);
            call().emit(CallEvent::Participants(p));
        }
        "speak_denied" => {
            let d = step["denied"].as_bool().expect("denied");
            call().set_speak_denied(ME, d);
        }
        other => panic!("{what}: unknown step {other}"),
    }
}

/// Every scenario: the listener's calls are exactly the fixture's, step by step.
#[tokio::test]
async fn every_scenario_gives_exactly_the_fixture_calls() {
    let f = fixture();
    let scenarios = f["scenarios"].as_object().expect("scenarios");
    assert_eq!(
        scenarios
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([
            "held_join",
            "mid_call_declined",
            "mid_call_pending",
            "self_mute_server_muted",
            "self_mute_speak_denied",
            "stop",
        ]),
        "every scenario is driven"
    );
    for (name, sc) in scenarios {
        let provider = FakeProvider::answering(ME);
        match sc["provider"].as_str() {
            None => {}
            Some("consent_required") => provider.refuse_next(ProviderError::ConsentRequired(
                Box::new(consent_required(48831765147549720, 3141)),
            )),
            Some(p) => panic!("{name}: unknown provider {p}"),
        }
        let w = world(provider);
        let mut want: Vec<Value> = Vec::new();
        for (i, step) in sc["steps"].as_array().expect("steps").iter().enumerate() {
            let what = format!("{name} step {i} ({})", step["do"]);
            run_step(&w, step, &what).await;
            want.extend(step["calls"].as_array().expect("calls").iter().cloned());
            until(&what, || w.listener.calls().len() >= want.len()).await;
            settle().await;
            assert_eq!(
                w.listener.calls(),
                want,
                "{what}: the listener's calls (left) against the fixture's (right)"
            );
        }
        mirrors_the_webview(&w, name);
    }
}

/// Every listener call but `on_self_mute` is the webview's event with the same payload, in
/// the same order (`speaking` has no listener method).
fn mirrors_the_webview(w: &World, name: &str) {
    let webview: Vec<Value> = w
        .sink
        .events()
        .into_iter()
        .filter(|(n, _)| n != "speaking")
        .map(|(n, v)| {
            let method = match n.as_str() {
                "errors" => "on_error".to_owned(),
                n => format!("on_{n}"),
            };
            json!({ "m": method, "v": v })
        })
        .collect();
    let native: Vec<Value> = w
        .listener
        .calls()
        .into_iter()
        .filter(|c| c.get("on_self_mute").is_none())
        .map(|c| {
            let (m, v) = c.as_object().expect("call").iter().next().expect("one");
            json!({ "m": m, "v": v })
        })
        .collect();
    assert_eq!(native, webview, "{name}: listener against webview events");
}

fn round_trip<T: Serialize + DeserializeOwned>(v: &Value, what: &str) {
    let t: T = from(v, what);
    assert_eq!(&serde_json::to_value(&t).expect("serialize"), v, "{what}");
}

/// Every argument in the fixture is the wire type's JSON exactly; `on_self_mute` keeps its
/// rules (muted covers every cause; self_muted is never more than muted).
#[test]
fn fixture_arguments_are_the_wire_types() {
    let f = fixture();
    let mut methods = std::collections::BTreeSet::new();
    for (name, sc) in f["scenarios"].as_object().expect("scenarios") {
        for step in sc["steps"].as_array().expect("steps") {
            for c in step["calls"].as_array().expect("calls") {
                let obj = c.as_object().expect("a call is an object");
                assert_eq!(obj.len(), 1, "{name}: one method per call: {c}");
                let (m, v) = obj.iter().next().expect("one");
                let what = format!("{name} {m}");
                match m.as_str() {
                    "on_state" => round_trip::<StateEvent>(v, &what),
                    "on_participants" => round_trip::<Vec<Participant>>(v, &what),
                    "on_recording" => round_trip::<Option<RecordingInfo>>(v, &what),
                    "on_error" => round_trip::<ErrorInfo>(v, &what),
                    "on_devices" => round_trip::<DeviceList>(v, &what),
                    "on_self_mute" => {
                        round_trip::<SelfMute>(v, &what);
                        let s: SelfMute = from(v, &what);
                        assert_eq!(
                            s.muted,
                            s.self_muted || s.deafened || s.server_muted || s.speak_denied,
                            "{what}: {v}"
                        );
                    }
                    other => panic!("{name}: no listener method {other}"),
                }
                methods.insert(m.clone());
            }
        }
    }
    for m in [
        "on_state",
        "on_participants",
        "on_self_mute",
        "on_recording",
    ] {
        assert!(methods.contains(m), "the fixture shows {m}");
    }
}

/// `SelfMute::of`: the row's `muted` is the choice while nothing forces it; otherwise the
/// choice is kept apart and `muted` is forced on.
#[test]
fn self_mute_from_the_row() {
    let row = |muted, deafened, server_muted, speak_denied| Participant {
        member_id: ME.into(),
        is_self: true,
        muted,
        deafened,
        server_muted,
        server_deafened: false,
        speak_denied,
    };
    let s = SelfMute::of(&row(true, false, false, false), false);
    assert!(s.muted && s.self_muted, "unforced: the row is the choice");
    let s = SelfMute::of(&row(false, false, true, false), false);
    assert!(s.muted && !s.self_muted && s.server_muted);
    let s = SelfMute::of(&row(true, true, false, false), false);
    assert!(s.muted && s.deafened && !s.self_muted);
    let s = SelfMute::of(&row(true, false, false, true), true);
    assert!(s.muted && s.self_muted && s.speak_denied);
    assert_eq!(
        serde_json::to_value(s).expect("json"),
        json!({"muted": true, "deafened": false, "self_muted": true, "server_muted": false,
               "speak_denied": true})
    );
}

/// A deafen while joined: on_self_mute says deafened and muted, self_muted stays your own.
#[tokio::test]
async fn deafen_is_reported_and_undone() {
    let w = world(FakeProvider::answering(ME));
    w.control
        .join(JoinArgs {
            target: "k2:acme".into(),
            room: "48831765147549698".into(),
            muted: None,
            deafened: None,
        })
        .await
        .expect("join");
    until("joined", || w.listener.calls().len() >= 4).await;
    let self_mutes = |w: &World| -> Vec<Value> {
        w.listener
            .calls()
            .into_iter()
            .filter_map(|c| c.get("on_self_mute").cloned())
            .collect()
    };
    w.control.set_deafened(true).await.expect("deafen");
    until("deafened", || self_mutes(&w).len() == 2).await;
    assert_eq!(
        self_mutes(&w)[1],
        json!({"muted": true, "deafened": true, "self_muted": false, "server_muted": false,
               "speak_denied": false})
    );
    w.control.set_deafened(false).await.expect("undeafen");
    until("undeafened", || self_mutes(&w).len() == 3).await;
    assert_eq!(
        self_mutes(&w)[2],
        json!({"muted": false, "deafened": false, "self_muted": false, "server_muted": false,
               "speak_denied": false})
    );
}

/// The control's rules: `leave` is safe to repeat (idle or ended is `Ok`); a mute with no
/// call is `not_in_call`, as the webview's `mute`; `snapshot` is the `state`
/// command's; a refused mute leaves `self_muted` as it was.
#[tokio::test]
async fn control_rules() {
    let w = world(FakeProvider::answering(ME));
    w.control.leave().await.expect("leave while idle");
    w.control.leave().await.expect("again");
    let e = w.control.set_muted(true).await.expect_err("no call");
    assert_eq!(e.code, "not_in_call");
    let e = w.control.set_deafened(true).await.expect_err("no call");
    assert_eq!(e.code, "not_in_call");
    let e = w.control.consent(true, 1).await.expect_err("no call");
    assert_eq!(e.code, "not_in_call");
    assert!(w.listener.calls().is_empty(), "nothing happened");
    assert_eq!(
        serde_json::to_value(w.control.snapshot()).expect("json"),
        json!({"state": "idle", "participants": [], "call_id": null})
    );

    w.control
        .join(JoinArgs {
            target: "k2:acme".into(),
            room: "48831765147549698".into(),
            muted: Some(true),
            deafened: None,
        })
        .await
        .expect("join");
    let snap = w.control.snapshot();
    assert_eq!(
        snap.call_id.as_deref(),
        Some("r_fakeroomfakeroomfakeroomfa")
    );
    assert!(snap.participants[0].muted);
    let call = w.engine.last_call().expect("call");
    // A moderator mute, then a refused unmute: self_muted stays on.
    let mut p = snap.participants.clone();
    p[0].server_muted = true;
    call.emit(CallEvent::Participants(p));
    until("server muted", || {
        w.control.snapshot().participants[0].server_muted
    })
    .await;
    settle().await;
    let n = w.listener.calls().len();
    call.fail_next_mute(tauri_plugin_sew_voice::EngineError::failed("busy"));
    let e = w.control.set_muted(false).await.expect_err("refused");
    assert_eq!(e.code, "busy");
    settle().await;
    let after: Vec<Value> = w.listener.calls()[n..].to_vec();
    assert_eq!(
        after,
        vec![
            json!({"on_self_mute": {"muted": true, "deafened": false, "self_muted": false,
                                    "server_muted": true, "speak_denied": false}}),
            json!({"on_self_mute": {"muted": true, "deafened": false, "self_muted": true,
                                    "server_muted": true, "speak_denied": false}}),
        ],
        "the choice is shown at once and put back when the engine refuses"
    );

    w.control.leave().await.expect("leave");
    w.control.leave().await.expect("leave again");
    assert_eq!(
        w.control.snapshot().state,
        tauri_plugin_sew_voice::CallState::Ended {
            reason: "left".into()
        }
    );
}

/// `network_changed` reaches the live call (and does nothing without one); the audio-session
/// hooks reach the engine unchanged, in order, with or without a call.
#[tokio::test]
async fn network_and_audio_session_pass_through() {
    let w = world(FakeProvider::answering(ME));
    w.control.network_changed();
    w.control.session_activated();
    w.control
        .join(JoinArgs {
            target: "k2:acme".into(),
            room: "48831765147549698".into(),
            muted: None,
            deafened: None,
        })
        .await
        .expect("join");
    let call = w.engine.last_call().expect("call");
    assert_eq!(call.network_changes(), 0, "the earlier change had no call");
    w.control.network_changed();
    w.control.network_changed();
    assert_eq!(call.network_changes(), 2);
    w.control.suspend();
    w.control.resume();
    w.control.route_changed();
    w.control.session_deactivated();
    assert_eq!(
        w.engine.audio_sessions(),
        vec![
            AudioSessionEvent::Activated,
            AudioSessionEvent::Suspend,
            AudioSessionEvent::Resume,
            AudioSessionEvent::RouteChanged,
            AudioSessionEvent::Deactivated,
        ]
    );
    w.control.leave().await.expect("leave");
    w.control.network_changed();
    assert_eq!(call.network_changes(), 2, "none after the call");
}

/// A listener that calls back into the control from its own queue (as the listener docs say:
/// copy, then hop) sees a snapshot consistent with the event it got.
#[tokio::test]
async fn a_listener_that_hops_reads_the_snapshot() {
    struct Hop {
        tx: tokio::sync::mpsc::UnboundedSender<StateEvent>,
    }
    impl tauri_plugin_sew_voice::VoiceListener for Hop {
        fn on_state(&self, state: &StateEvent) {
            let _ = self.tx.send(state.clone());
        }
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let engine = Arc::new(FakeEngine::new());
    let spawn: Spawn = Arc::new(|f| {
        tokio::spawn(f);
    });
    let svc = VoiceService::with_listener(
        engine,
        Arc::new(FakeProvider::answering(ME)),
        Arc::new(RecordingSink::new()),
        Arc::new(Hop { tx }),
        spawn,
    );
    let control = VoiceControl::new(svc);
    let c2 = control.clone();
    let reader = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(ev) = rx.recv().await {
            // Off the plugin's lock: the control answers.
            let snap = c2.snapshot();
            seen.push((ev.state, snap.state));
            if seen.len() == 2 {
                return seen;
            }
        }
        seen
    });
    control
        .join(JoinArgs {
            target: "k2:acme".into(),
            room: "48831765147549698".into(),
            muted: None,
            deafened: None,
        })
        .await
        .expect("join");
    let seen = reader.await.expect("reader");
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[1].0,
        tauri_plugin_sew_voice::CallState::Connected,
        "connecting, then connected"
    );
    assert_eq!(seen[1].1, tauri_plugin_sew_voice::CallState::Connected);
}
