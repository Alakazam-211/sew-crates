//! The plugin's commands, events and state transitions against the fake engine.

// The helpers (not only #[test] fns) unwrap: a test that cannot set up must fail loudly.
#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tauri_plugin_sew_voice::fake::{FakeEngine, FakeProvider, RecordingSink, demo_devices};
use tauri_plugin_sew_voice::{
    CallEvent, CallSnapshot, CallState, EngineError, ErrorInfo, JoinArgs, JoinProvider, MuteArgs,
    Participant, ProviderError, SetDeviceArgs, Spawn, Speaking, VoiceService,
};

const ME: &str = "48831765147549699";
const ROOM: &str = "r_fakeroomfakeroomfakeroomfa";
/// `args()`'s target and room.
const TARGET: &str = "k2:acme";
const CHANNEL: &str = "48831765147549698";

struct World {
    svc: VoiceService,
    engine: Arc<FakeEngine>,
    sink: Arc<RecordingSink>,
}

fn world_with(engine: FakeEngine, provider: Arc<dyn JoinProvider>) -> World {
    let engine = Arc::new(engine);
    let sink = Arc::new(RecordingSink::new());
    let spawn: Spawn = Arc::new(|f| {
        tokio::spawn(f);
    });
    let svc = VoiceService::new(engine.clone(), provider, sink.clone(), spawn);
    svc.start();
    World { svc, engine, sink }
}

fn world() -> World {
    world_with(FakeEngine::new(), Arc::new(FakeProvider::answering(ME)))
}

fn args() -> JoinArgs {
    JoinArgs {
        target: "k2:acme".into(),
        room: "48831765147549698".into(),
        muted: None,
        deafened: None,
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

fn states(sink: &RecordingSink) -> Vec<Value> {
    sink.of("state")
}

/// A `state` payload of the tests' call: with `args()`'s target and room.
fn st(mut v: Value) -> Value {
    v["target"] = json!(TARGET);
    v["room"] = json!(CHANNEL);
    v
}

fn me(muted: bool, deafened: bool) -> Value {
    json!({
        "member_id": ME, "is_self": true, "muted": muted, "deafened": deafened,
        "server_muted": false, "server_deafened": false
    })
}

async fn joined() -> World {
    let w = world();
    let r = w.svc.join(args()).await.expect("join");
    assert_eq!(r.call_id, ROOM);
    w
}

#[tokio::test]
async fn starts_idle_with_no_call() {
    let w = world();
    let snap = w.svc.state().await.expect("state");
    assert_eq!(
        serde_json::to_value(&snap).expect("json"),
        json!({"state": "idle", "participants": [], "call_id": null})
    );
    assert!(w.sink.events().is_empty(), "no event before a command");
}

#[tokio::test]
async fn join_goes_connecting_then_connected() {
    let provider = Arc::new(FakeProvider::answering(ME));
    let w = world_with(FakeEngine::new(), provider.clone());
    let r = w.svc.join(args()).await.expect("join");
    assert_eq!(r.call_id, ROOM);
    assert_eq!(
        w.sink.events(),
        vec![
            ("state".into(), st(json!({"state": "connecting"}))),
            ("participants".into(), json!([me(false, false)])),
            ("state".into(), st(json!({"state": "connected"}))),
        ]
    );
    let calls = provider.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].target, "k2:acme");
    assert_eq!(calls[0].room, "48831765147549698");
    assert!(!calls[0].muted && !calls[0].deafened);
    assert_eq!(calls[0].rejoin, None);
    let snap = w.svc.snapshot();
    assert_eq!(snap.state, CallState::Connected);
    assert_eq!(snap.call_id.as_deref(), Some(ROOM));
    assert_eq!(snap.participants.len(), 1);
}

#[tokio::test]
async fn join_deafened_implies_muted() {
    let provider = Arc::new(FakeProvider::answering(ME));
    let w = world_with(FakeEngine::new(), provider.clone());
    w.svc
        .join(JoinArgs {
            deafened: Some(true),
            ..args()
        })
        .await
        .expect("join");
    let req = &provider.calls()[0];
    assert!(req.muted && req.deafened, "deafened implies muted: {req:?}");
    assert_eq!(w.sink.of("participants"), vec![json!([me(true, true)])]);

    let w = world();
    w.svc
        .join(JoinArgs {
            muted: Some(true),
            ..args()
        })
        .await
        .expect("join");
    assert_eq!(w.sink.of("participants"), vec![json!([me(true, false)])]);
}

#[tokio::test]
async fn connecting_until_the_engine_connects() {
    let engine = FakeEngine::new();
    engine.set_auto_connect(false);
    let w = world_with(engine, Arc::new(FakeProvider::answering(ME)));
    w.svc.join(args()).await.expect("join");
    assert_eq!(states(&w.sink), vec![st(json!({"state": "connecting"}))]);
    assert_eq!(w.svc.snapshot().state, CallState::Connecting);
    let call = w.engine.last_call().expect("call");
    call.connect();
    until("connected", || {
        w.svc.snapshot().state == CallState::Connected
    })
    .await;
    assert_eq!(
        states(&w.sink),
        vec![
            st(json!({"state": "connecting"})),
            st(json!({"state": "connected"}))
        ]
    );
}

#[tokio::test]
async fn reconnecting_attempts_then_connected() {
    let w = joined().await;
    let call = w.engine.last_call().expect("call");
    call.reconnecting(1);
    call.reconnecting(2);
    until("attempt 2", || {
        w.svc.snapshot().state == CallState::Reconnecting { attempt: 2 }
    })
    .await;
    // A late window sees the reconnect in the snapshot.
    let snap = serde_json::to_value(w.svc.state().await.expect("state")).expect("json");
    assert_eq!(snap["state"], "reconnecting");
    assert_eq!(snap["attempt"], 2);
    assert_eq!(snap["participants"], json!([me(false, false)]));
    call.connect();
    until("connected", || {
        w.svc.snapshot().state == CallState::Connected
    })
    .await;
    assert_eq!(
        states(&w.sink),
        vec![
            st(json!({"state": "connecting"})),
            st(json!({"state": "connected"})),
            st(json!({"state": "reconnecting", "attempt": 1})),
            st(json!({"state": "reconnecting", "attempt": 2})),
            st(json!({"state": "connected"})),
        ]
    );
}

#[tokio::test]
async fn a_second_join_is_already_in_call() {
    let w = joined().await;
    let e = w.svc.join(args()).await.expect_err("second join");
    assert_eq!(e, ErrorInfo::new("already_in_call"));
    assert_eq!(w.engine.joins().len(), 1, "the engine was not asked again");

    // Also while the first join is still connecting.
    let (provider, gate) = FakeProvider::gated(ME);
    let w = world_with(FakeEngine::new(), Arc::new(provider));
    let svc = w.svc.clone();
    let first = tokio::spawn(async move { svc.join(args()).await });
    until("connecting", || {
        w.svc.snapshot().state == CallState::Connecting
    })
    .await;
    let e = w.svc.join(args()).await.expect_err("join while connecting");
    assert_eq!(e.code, "already_in_call");
    gate.add_permits(1);
    first.await.expect("task").expect("first join");
    assert_eq!(w.svc.snapshot().state, CallState::Connected);
}

/// The engine still holds a call the plugin lost hold of (the slot is `ended`): a `leave`
/// releases it, and so does a join the engine refuses `already_in_call` (released, then joined
/// once more), so a host recovers without a reload.
#[tokio::test]
async fn a_call_the_plugin_lost_is_released_by_a_leave_or_the_next_join() {
    let w = world();
    w.engine.strand_call();
    w.svc.leave().await.expect("leave with no call");
    assert_eq!(
        w.engine.releases(),
        1,
        "the leave released the engine's call"
    );
    w.svc.join(args()).await.expect("join after the release");
    assert_eq!(w.engine.joins().len(), 1);
    w.svc.leave().await.expect("leave");

    w.engine.strand_call();
    let r = w
        .svc
        .join(args())
        .await
        .expect("the join releases and joins again");
    assert_eq!(r.call_id, ROOM);
    assert_eq!(w.engine.releases(), 2);
    assert_eq!(w.engine.joins().len(), 3, "refused once, then joined");
    assert_eq!(w.svc.snapshot().state, CallState::Connected);
}

#[tokio::test]
async fn the_engine_ends_the_call() {
    let w = joined().await;
    let call = w.engine.last_call().expect("call");
    call.add_remote("48831765147549700");
    until("two members", || w.svc.snapshot().participants.len() == 2).await;
    w.sink.clear();
    call.end("removed_by_moderator");
    until("ended", || !w.svc.snapshot().state.is_live()).await;
    assert_eq!(
        w.sink.events(),
        vec![
            ("participants".into(), json!([])),
            (
                "state".into(),
                st(json!({"state": "ended", "reason": "removed_by_moderator"}))
            ),
        ]
    );
    let snap = w.svc.snapshot();
    assert_eq!(snap.call_id, None);
    assert!(snap.participants.is_empty());
    // The ended call takes no commands and its later events are ignored.
    let e = w
        .svc
        .mute(MuteArgs {
            muted: Some(true),
            deafened: None,
        })
        .await;
    assert_eq!(e.expect_err("mute after end").code, "not_in_call");
    call.connect();
    settle().await;
    assert_eq!(
        w.sink.events().len(),
        2,
        "nothing after the end: {:?}",
        w.sink.events()
    );
}

#[tokio::test]
async fn join_again_after_an_end() {
    let w = joined().await;
    let old = w.engine.last_call().expect("call");
    old.end("call_ended");
    until("ended", || !w.svc.snapshot().state.is_live()).await;
    w.sink.clear();
    w.svc.join(args()).await.expect("second join");
    assert_eq!(w.engine.joins().len(), 2);
    assert_eq!(w.svc.snapshot().state, CallState::Connected);
    assert_eq!(
        w.sink.events(),
        vec![
            ("state".into(), st(json!({"state": "connecting"}))),
            ("participants".into(), json!([me(false, false)])),
            ("state".into(), st(json!({"state": "connected"}))),
        ]
    );
    // The old call's events don't reach the new one.
    old.reconnecting(1);
    settle().await;
    assert_eq!(w.svc.snapshot().state, CallState::Connected);
}

#[tokio::test]
async fn leave_ends_with_left() {
    let w = joined().await;
    let call = w.engine.last_call().expect("call");
    w.sink.clear();
    w.svc.leave().await.expect("leave");
    assert!(call.left(), "the engine left");
    assert_eq!(
        w.sink.events(),
        vec![
            ("participants".into(), json!([])),
            (
                "state".into(),
                st(json!({"state": "ended", "reason": "left"}))
            ),
        ]
    );
    // Engine events after the leave are ignored.
    call.reconnecting(1);
    settle().await;
    assert_eq!(w.sink.events().len(), 2);
    // leave again, and leave with no call: nothing happens.
    w.svc.leave().await.expect("leave again");
    world().svc.leave().await.expect("leave idle");
    assert_eq!(w.sink.events().len(), 2);
}

#[tokio::test]
async fn a_failed_leave_still_ends_and_reports() {
    let w = joined().await;
    let call = w.engine.last_call().expect("call");
    call.fail_next_leave(EngineError::failed("network"));
    w.svc.leave().await.expect("leave answers ok");
    assert_eq!(
        w.svc.snapshot().state,
        CallState::Ended {
            reason: "left".into()
        }
    );
    assert_eq!(
        w.sink.of("errors"),
        vec![json!({"code": "network", "detail": null})]
    );
}

#[tokio::test]
async fn leave_while_connecting_cancels_the_join() {
    let (provider, gate) = FakeProvider::gated(ME);
    let w = world_with(FakeEngine::new(), Arc::new(provider));
    let svc = w.svc.clone();
    let join = tokio::spawn(async move { svc.join(args()).await });
    until("connecting", || {
        w.svc.snapshot().state == CallState::Connecting
    })
    .await;
    w.svc.leave().await.expect("leave");
    assert_eq!(
        w.svc.snapshot().state,
        CallState::Ended {
            reason: "left".into()
        }
    );
    gate.add_permits(1);
    let e = join.await.expect("task").expect_err("cancelled join");
    assert_eq!(e.code, "left");
    let call = w.engine.last_call().expect("the engine joined");
    until("the late call is hung up", || call.left()).await;
    settle().await;
    assert_eq!(
        states(&w.sink),
        vec![
            st(json!({"state": "connecting"})),
            st(json!({"state": "ended", "reason": "left"})),
        ],
        "never connected"
    );
}

#[tokio::test]
async fn provider_failures_end_the_attempt() {
    let cases: Vec<(ProviderError, Value, &str)> = vec![
        (
            ProviderError::SigninRequired,
            json!({"code": "signin_required", "detail": null}),
            "signin_required",
        ),
        (
            ProviderError::Refused {
                code: "voice_channel_full".into(),
                detail: json!({"limit": 25}),
            },
            json!({"code": "voice_channel_full", "detail": {"limit": 25}}),
            "voice_channel_full",
        ),
        (
            ProviderError::Unavailable {
                retry_after_ms: Some(5000),
            },
            json!({"code": "unavailable", "detail": {"retry_after_ms": 5000}}),
            "unavailable",
        ),
        (
            ProviderError::Other("connection refused".into()),
            json!({"code": "provider_error", "detail": "connection refused"}),
            "provider_error",
        ),
    ];
    for (err, want, reason) in cases {
        let w = world_with(FakeEngine::new(), Arc::new(FakeProvider::failing(err)));
        let e = w.svc.join(args()).await.expect_err("join fails");
        assert_eq!(serde_json::to_value(&e).expect("json"), want);
        assert_eq!(w.sink.of("errors"), vec![want.clone()]);
        assert_eq!(
            states(&w.sink),
            vec![
                st(json!({"state": "connecting"})),
                st(json!({"state": "ended", "reason": reason})),
            ]
        );
        // A failed join leaves the plugin ready for the next one.
        assert!(!w.svc.snapshot().state.is_live());
    }
}

#[tokio::test]
async fn engine_join_failure_is_passed_through() {
    let provider = Arc::new(FakeProvider::answering(ME));
    let w = world_with(FakeEngine::new(), provider.clone());
    w.engine
        .fail_next_join(EngineError::Failed(ErrorInfo::with_detail(
            "mic_denied",
            "TCC",
        )));
    let e = w.svc.join(args()).await.expect_err("join fails");
    assert_eq!(e, ErrorInfo::with_detail("mic_denied", "TCC"));
    assert_eq!(
        w.svc.snapshot().state,
        CallState::Ended {
            reason: "mic_denied".into()
        }
    );
    assert!(provider.calls().is_empty());
    w.svc.join(args()).await.expect("the next join works");
}

#[tokio::test]
async fn mute_and_deafen_follow_sews_rules() {
    let w = joined().await;
    let call = w.engine.last_call().expect("call");
    let self_state = |w: &World| {
        let p = w.svc.snapshot().participants;
        let me = p.iter().find(|p| p.is_self).expect("self").clone();
        (me.muted, me.deafened)
    };
    let mute = |muted, deafened| MuteArgs { muted, deafened };

    w.svc.mute(mute(Some(true), None)).await.expect("mute");
    until("muted", || self_state(&w) == (true, false)).await;
    w.svc.mute(mute(None, Some(true))).await.expect("deafen");
    until("deafened", || self_state(&w) == (true, true)).await;
    w.svc.mute(mute(None, Some(false))).await.expect("undeafen");
    until("muted again", || self_state(&w) == (true, false)).await;

    w.svc.mute(mute(Some(false), None)).await.expect("unmute");
    until("unmuted", || self_state(&w) == (false, false)).await;
    w.svc.mute(mute(None, Some(true))).await.expect("deafen");
    until("deafened", || self_state(&w) == (true, true)).await;
    w.svc.mute(mute(None, Some(false))).await.expect("undeafen");
    until("back to unmuted", || self_state(&w) == (false, false)).await;

    assert_eq!(
        call.mutes(),
        vec![
            (Some(true), None),
            (None, Some(true)),
            (None, Some(false)),
            (Some(false), None),
            (None, Some(true)),
            (None, Some(false)),
        ]
    );
    let last = w
        .sink
        .of("participants")
        .last()
        .cloned()
        .expect("participants");
    assert_eq!(last, json!([me(false, false)]));
}

#[tokio::test]
async fn mute_refusals() {
    let w = world();
    let e = w
        .svc
        .mute(MuteArgs {
            muted: Some(true),
            deafened: None,
        })
        .await;
    assert_eq!(e.expect_err("idle").code, "not_in_call");

    let w = joined().await;
    let e = w.svc.mute(MuteArgs::default()).await.expect_err("mute {}");
    assert_eq!(e.code, "invalid_args");
    let call = w.engine.last_call().expect("call");
    assert!(
        call.mutes().is_empty(),
        "mute {{}} never reaches the engine"
    );
    call.fail_next_mute(EngineError::failed("not_connected"));
    let e = w
        .svc
        .mute(MuteArgs {
            muted: Some(true),
            deafened: None,
        })
        .await;
    assert_eq!(e.expect_err("engine refuses").code, "not_connected");
}

#[tokio::test]
async fn devices_list_switch_and_hot_plug() {
    let w = world_with(
        FakeEngine::with_demo_devices(),
        Arc::new(FakeProvider::answering(ME)),
    );
    let list = w.svc.devices().await.expect("devices");
    assert_eq!(list, demo_devices());
    assert_eq!(list.input, None, "the OS default until set");

    w.svc
        .set_device(SetDeviceArgs {
            input: Some("in:headset".into()),
            output: None,
        })
        .await
        .expect("set input");
    until("devices event", || !w.sink.of("devices").is_empty()).await;
    let ev = w.sink.of("devices");
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["input"], "in:headset");
    assert_eq!(ev[0]["output"], Value::Null);
    assert_eq!(
        w.svc.devices().await.expect("devices").input.as_deref(),
        Some("in:headset")
    );

    let e = w
        .svc
        .set_device(SetDeviceArgs {
            input: None,
            output: Some("out:nope".into()),
        })
        .await
        .expect_err("unknown output");
    assert_eq!(
        e,
        ErrorInfo::with_detail("device_not_found", json!({"output": "out:nope"}))
    );
    let e = w
        .svc
        .set_device(SetDeviceArgs::default())
        .await
        .expect_err("set_device {}");
    assert_eq!(e.code, "invalid_args");

    // Hot-plug: a headset goes away.
    let mut unplugged = demo_devices();
    unplugged.inputs.retain(|d| d.id != "in:headset");
    w.engine.set_devices(unplugged.clone());
    until("hot-plug event", || w.sink.of("devices").len() == 2).await;
    assert_eq!(
        w.sink.of("devices")[1],
        serde_json::to_value(&unplugged).expect("json")
    );
}

/// The plugin subscribes to the engine's device events (so the real engine lists and
/// polls) on the first `devices`, `set_device` or `join`, not at start.
#[tokio::test]
async fn devices_are_used_from_the_first_command_on() {
    for first in ["devices", "set_device", "join"] {
        let w = world_with(
            FakeEngine::with_demo_devices(),
            Arc::new(FakeProvider::answering(ME)),
        );
        settle().await;
        assert_eq!(w.engine.device_subscribers(), 0, "{first}: not at start");
        match first {
            "devices" => {
                w.svc.devices().await.expect("devices");
            }
            "set_device" => w
                .svc
                .set_device(SetDeviceArgs {
                    input: Some("in:headset".into()),
                    output: None,
                })
                .await
                .expect("set_device"),
            _ => {
                w.svc.join(args()).await.expect("join");
            }
        }
        assert_eq!(w.engine.device_subscribers(), 1, "{first}: subscribed");
        // Once: later commands add no subscriber.
        w.svc.devices().await.expect("devices");
        assert_eq!(w.engine.device_subscribers(), 1, "{first}: once");
        // And the subscription forwards hot-plug.
        w.engine.set_devices(demo_devices());
        until("hot-plug event", || !w.sink.of("devices").is_empty()).await;
    }
}

/// `set_device` with `"default"` goes back to the OS default (`null` in the list).
#[tokio::test]
async fn set_device_default_is_the_os_default() {
    let w = world_with(
        FakeEngine::with_demo_devices(),
        Arc::new(FakeProvider::answering(ME)),
    );
    let set = |input: &str, output: &str| SetDeviceArgs {
        input: Some(input.into()),
        output: Some(output.into()),
    };
    w.svc
        .set_device(set("in:headset", "out:headset"))
        .await
        .expect("set");
    let list = w.svc.devices().await.expect("devices");
    assert_eq!(list.input.as_deref(), Some("in:headset"));
    assert_eq!(list.output.as_deref(), Some("out:headset"));
    // Back to the default, input only: the output stays.
    w.svc
        .set_device(SetDeviceArgs {
            input: Some("default".into()),
            output: None,
        })
        .await
        .expect("input default");
    let list = w.svc.devices().await.expect("devices");
    assert_eq!(list.input, None, "the OS default");
    assert_eq!(list.output.as_deref(), Some("out:headset"));
    until("devices events", || w.sink.of("devices").len() == 2).await;
    assert_eq!(w.sink.of("devices")[1]["input"], Value::Null);
    // Both, during a call too.
    w.svc.join(args()).await.expect("join");
    w.svc
        .set_device(set("default", "default"))
        .await
        .expect("both default");
    let list = w.svc.devices().await.expect("devices");
    assert_eq!((list.input, list.output), (None, None));
}

/// The state says which target and room the call is on, and a moderator move changes the
/// room (a `state` event with the same state and the destination room).
#[tokio::test]
async fn state_has_target_and_room_and_a_move_changes_the_room() {
    let f = fixture();
    let w = joined().await;
    let snap = serde_json::to_value(w.svc.state().await.expect("state")).expect("json");
    assert_eq!(snap["target"], TARGET);
    assert_eq!(snap["room"], CHANNEL);
    assert_eq!(
        states(&w.sink).last(),
        Some(&st(json!({"state": "connected"})))
    );
    w.sink.clear();

    let call = w.engine.last_call().expect("call");
    call.move_to("48831765147549710");
    until("moved", || {
        w.svc.snapshot().room.as_deref() == Some("48831765147549710")
    })
    .await;
    assert_eq!(
        w.sink.events(),
        vec![("state".into(), f["moved"]["payload"].clone())]
    );
    assert_eq!(
        serde_json::to_value(w.svc.snapshot()).expect("json"),
        f["state_snapshot"]["moved"]
    );
    // The same move again says nothing new.
    call.move_to("48831765147549710");
    settle().await;
    assert_eq!(w.sink.events().len(), 1);
    // The end keeps the call's target and room; the next join has its own.
    call.end("removed_by_moderator");
    until("ended", || !w.svc.snapshot().state.is_live()).await;
    let ended = states(&w.sink).last().cloned().expect("ended");
    assert_eq!(ended["room"], "48831765147549710");
    assert_eq!(ended["target"], TARGET);
    w.svc
        .join(JoinArgs {
            target: "k2:other".into(),
            room: "7".into(),
            ..args()
        })
        .await
        .expect("join again");
    let s = w.svc.snapshot();
    assert_eq!(
        (s.target.as_deref(), s.room.as_deref()),
        (Some("k2:other"), Some("7"))
    );
}

/// A move the pump missed (a lag) is picked up from the engine with the snapshot.
#[tokio::test(flavor = "current_thread")]
async fn a_missed_move_is_resynced() {
    let w = joined().await;
    let call = w.engine.last_call().expect("call");
    call.move_silently("48831765147549711");
    for i in 0..100u32 {
        call.emit(CallEvent::State(CallState::Reconnecting { attempt: i + 1 }));
    }
    until("resynced room", || {
        w.svc.snapshot().room.as_deref() == Some("48831765147549711")
    })
    .await;
}

#[tokio::test]
async fn call_errors_become_errors_events() {
    let w = joined().await;
    let call = w.engine.last_call().expect("call");
    call.emit(CallEvent::Error(ErrorInfo::new("device_lost")));
    call.emit(CallEvent::Error(ErrorInfo::with_detail(
        "key_storage_file",
        "no OS keystore; the device key is in a file",
    )));
    until("two errors", || w.sink.of("errors").len() == 2).await;
    assert_eq!(
        w.sink.of("errors"),
        vec![
            json!({"code": "device_lost", "detail": null}),
            json!({"code": "key_storage_file", "detail": "no OS keystore; the device key is in a file"}),
        ]
    );
    assert_eq!(
        w.svc.snapshot().state,
        CallState::Connected,
        "errors don't end the call"
    );
}

#[tokio::test(start_paused = true)]
async fn speaking_is_at_most_ten_a_second() {
    let w = joined().await;
    let call = w.engine.last_call().expect("call");
    let lv = |level: f64| {
        vec![Speaking {
            member_id: ME.into(),
            level,
        }]
    };
    // Five level updates 20 ms apart (kessel-audio's pace is ~50 ms; an engine may go faster).
    for (i, l) in [0.1f64, 0.2, 0.3, 0.4, 0.5].into_iter().enumerate() {
        if i > 0 {
            tokio::time::advance(Duration::from_millis(20)).await;
        }
        call.emit(CallEvent::Speaking(lv(l)));
        settle().await;
    }
    // The first went at once; the rest are held to the 100 ms floor.
    assert_eq!(
        w.sink.of("speaking"),
        vec![json!([{"member_id": ME, "level": 0.1}])]
    );
    tokio::time::advance(Duration::from_millis(30)).await;
    until("the newest levels", || w.sink.of("speaking").len() == 2).await;
    assert_eq!(
        w.sink.of("speaking")[1],
        json!([{"member_id": ME, "level": 0.5}]),
        "only the newest levels are sent"
    );
    // A later update, well after the floor, goes at once.
    tokio::time::advance(Duration::from_millis(500)).await;
    call.emit(CallEvent::Speaking(lv(0.0)));
    until("immediate", || w.sink.of("speaking").len() == 3).await;
    settle().await;
    assert_eq!(w.sink.of("speaking").len(), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn a_lagging_pump_resyncs_from_the_snapshot() {
    let w = joined().await;
    let call = w.engine.last_call().expect("call");
    // Overflow the call's 64-event queue before the pump runs again.
    for i in 0..100u32 {
        call.emit(CallEvent::State(CallState::Reconnecting { attempt: i + 1 }));
    }
    let truth = CallSnapshot {
        state: CallState::Connected,
        participants: vec![Participant {
            member_id: ME.into(),
            is_self: true,
            muted: true,
            deafened: false,
            server_muted: true,
            server_deafened: false,
            speak_denied: false,
        }],
        recording: None,
    };
    call.set_snapshot_silently(truth.clone());
    until("resynced", || {
        let s = w.svc.snapshot();
        s.state == CallState::Connected && s.participants == truth.participants
    })
    .await;
}

#[tokio::test]
async fn no_token_reaches_the_webview() {
    let w = joined().await;
    let call = w.engine.last_call().expect("call");
    call.add_remote("48831765147549700");
    until("roster", || w.svc.snapshot().participants.len() == 2).await;
    let mut all: Vec<Value> = w.sink.events().into_iter().map(|(_, v)| v).collect();
    all.push(serde_json::to_value(w.svc.state().await.expect("state")).expect("json"));
    let text = serde_json::to_string(&all).expect("json");
    for secret in [
        "fake-media-token",
        "token",
        "sewcc_",
        "wss://",
        "api_base",
        "credential",
    ] {
        assert!(
            !text.contains(secret),
            "{secret:?} reached the webview: {text}"
        );
    }
}

// ------------------------------------------------------------------ Sew 0.7.2: ends, SPEAK

/// The contract fixture K2 tests against.
fn fixture() -> Value {
    serde_json::from_str(include_str!("native-voice-contract.json")).expect("the fixture")
}

/// Every end reason in the fixture, through the fake engine: the plugin's `state` event is the
/// fixture's payload exactly, and the call is over.
#[tokio::test]
async fn sew_end_reasons_reach_the_webview_unchanged() {
    let f = fixture();
    let mut cases: Vec<Value> = f["ended"]["sew_reasons"].as_array().expect("sew").clone();
    cases.extend(
        f["ended"]["fallbacks"]
            .as_array()
            .expect("fallbacks")
            .clone(),
    );
    assert_eq!(cases.len(), 12);
    for c in cases {
        let w = joined().await;
        let call = w.engine.last_call().expect("call");
        w.sink.clear();
        let reason = c["reason"].as_str().expect("reason");
        call.end(reason);
        until(reason, || !w.svc.snapshot().state.is_live()).await;
        assert_eq!(states(&w.sink).last(), Some(&c["payload"]), "{reason}");
        assert_eq!(
            w.svc.snapshot().state,
            CallState::Ended {
                reason: reason.into()
            }
        );
    }
}

/// SPEAK lost and given back, for this device (with and without a self-mute) and a remote
/// member, as the fixture writes it.
#[tokio::test]
async fn speak_denied_and_back_as_the_fixture_shows() {
    let f = fixture();
    let case = |name: &str| f["participants"][name].clone();
    let last = |w: &World| w.sink.of("participants").last().cloned();
    let w = joined().await;
    let call = w.engine.last_call().expect("call");
    until("joined", || {
        w.svc.snapshot().participants
            == serde_json::from_value::<Vec<Participant>>(case("joined")).expect("joined")
    })
    .await;

    // Lost: muted and speak_denied, not a moderator mute.
    call.set_speak_denied(ME, true);
    until("speak denied", || {
        last(&w) == Some(case("self_speak_denied"))
    })
    .await;
    // An unmute while denied publishes nothing: still muted.
    w.svc
        .mute(MuteArgs {
            muted: Some(false),
            deafened: None,
        })
        .await
        .expect("unmute while denied");
    settle().await;
    assert_eq!(last(&w), Some(case("self_speak_denied")));
    // Back: unmuted, as the member asked.
    call.set_speak_denied(ME, false);
    until("speak back", || {
        last(&w) == Some(case("self_speak_back_unmuted"))
    })
    .await;

    // Self-muted before the loss and through it: still self-muted when SPEAK is back.
    w.svc
        .mute(MuteArgs {
            muted: Some(true),
            deafened: None,
        })
        .await
        .expect("mute");
    call.set_speak_denied(ME, true);
    until("denied again", || {
        last(&w) == Some(case("self_speak_denied"))
    })
    .await;
    call.set_speak_denied(ME, false);
    until("back, still muted", || {
        last(&w) == Some(case("self_speak_back_still_self_muted"))
    })
    .await;
    w.svc
        .mute(MuteArgs {
            muted: Some(false),
            deafened: None,
        })
        .await
        .expect("unmute");

    // A remote member without SPEAK shows muted with its own flag.
    call.add_remote("48831765147549700");
    call.set_speak_denied("48831765147549700", true);
    until("remote denied", || {
        last(&w) == Some(case("remote_speak_denied"))
    })
    .await;

    // The `state` command of a window opened mid-call shows it too.
    let w2 = joined().await;
    let c2 = w2.engine.last_call().expect("call");
    c2.set_speak_denied(ME, true);
    let want = f["state_snapshot"]["speak_denied"].clone();
    until("snapshot", || {
        serde_json::to_value(w2.svc.snapshot()).expect("json") == want
    })
    .await;
}

/// A moderator mute is `server_muted`, never `speak_denied`.
#[tokio::test]
async fn a_moderator_mute_is_not_speak_denied() {
    let f = fixture();
    let w = joined().await;
    let call = w.engine.last_call().expect("call");
    let mut p = w.svc.snapshot().participants;
    p[0].muted = true;
    p[0].server_muted = true;
    call.emit(CallEvent::Participants(p));
    until("server muted", || {
        w.sink.of("participants").last() == Some(&f["participants"]["self_server_muted"])
    })
    .await;
}

// ------------------------------------------------------------------ Sew 0.7.3: recording

const REC: u64 = 48831765147549720;

/// A world whose provider refuses the first join(s) as given, then answers.
fn refusing(first: Vec<ProviderError>) -> (World, Arc<FakeProvider>) {
    let p = Arc::new(FakeProvider::answering(ME));
    for e in first {
        p.refuse_next(e);
    }
    (world_with(FakeEngine::new(), p.clone()), p)
}

fn held_refusal() -> ProviderError {
    ProviderError::ConsentRequired(Box::new(tauri_plugin_sew_voice::fake::consent_required(
        REC, 3141,
    )))
}

/// A join into a recorded room: held at `consent_required` with the notice (the fixture's
/// payloads), no call id; `consent` joins again with the answer and the call connects.
#[tokio::test]
async fn a_held_join_waits_for_consent_then_connects() {
    let f = fixture();
    let (w, p) = refusing(vec![held_refusal()]);
    let r = w.svc.join(args()).await.expect("join answers while held");
    assert_eq!(r.call_id, "", "no room while held");
    assert_eq!(
        states(&w.sink).last(),
        Some(&f["recording"]["held"]["state"])
    );
    assert_eq!(
        w.sink.of("recording").last(),
        Some(&f["recording"]["held"]["recording"])
    );
    let snap = w.svc.state().await.expect("state");
    assert_eq!(
        serde_json::to_value(&snap).expect("json"),
        f["state_snapshot"]["consent_required"]
    );
    // Held is a call: a second join is refused.
    let e = w.svc.join(args()).await.expect_err("one call");
    assert_eq!(e.code, "already_in_call");
    settle().await;
    assert_eq!(
        p.calls().len(),
        1,
        "nothing more is asked of Sew while held"
    );

    let a: tauri_plugin_sew_voice::ConsentArgs =
        serde_json::from_value(f["recording"]["held"]["consent"]["args"].clone()).expect("args");
    w.svc.consent(a).await.expect("consent");
    let calls = p.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        serde_json::to_value(calls[1].recording_notice).expect("json"),
        json!({"recording_id": REC.to_string(), "consent": true, "notice_version": 3141})
    );
    until("connected", || {
        w.svc.snapshot().state == CallState::Connected
    })
    .await;
    assert_eq!(w.svc.snapshot().call_id.as_deref(), Some(ROOM));
    assert_eq!(w.sink.of("recording").last(), Some(&Value::Null));
    assert_eq!(w.svc.snapshot().participants.len(), 1);
}

/// `leave` while held ends the call `left`; a decline Sew refuses ends it with the refusal.
#[tokio::test]
async fn a_held_join_ends_on_leave_or_a_refused_decline() {
    let (w, p) = refusing(vec![held_refusal()]);
    w.svc.join(args()).await.expect("held");
    w.svc.leave().await.expect("leave");
    assert_eq!(
        states(&w.sink).last(),
        Some(&st(json!({"state": "ended", "reason": "left"})))
    );
    assert_eq!(w.sink.of("recording").last(), Some(&Value::Null));
    assert_eq!(p.calls().len(), 1);
    assert!(w.engine.last_call().expect("call").left());

    let (w, _) = refusing(vec![
        held_refusal(),
        ProviderError::Refused {
            code: "recording_consent_required_to_join".into(),
            detail: Value::Null,
        },
    ]);
    w.svc.join(args()).await.expect("held");
    let e = w
        .svc
        .consent(tauri_plugin_sew_voice::ConsentArgs {
            granted: false,
            notice_version: 3141,
        })
        .await
        .expect_err("refused");
    assert_eq!(e.code, "recording_consent_required_to_join");
    until("ended", || !w.svc.snapshot().state.is_live()).await;
    assert_eq!(
        w.svc.snapshot().state,
        CallState::Ended {
            reason: "recording_consent_required_to_join".into()
        }
    );
}

/// In a call: the `recording` event follows the engine's recording (the fixture's payloads),
/// `consent` and `withdraw_consent` reach the engine, and a changed notice is the command's
/// error with Sew's details.
#[tokio::test]
async fn the_recording_event_and_consent_in_a_call() {
    let f = fixture();
    let w = joined().await;
    let call = w.engine.last_call().expect("call");
    for k in ["granted", "withdrawn"] {
        let info: tauri_plugin_sew_voice::RecordingInfo =
            serde_json::from_value(f["recording"][k].clone()).expect(k);
        call.set_recording(Some(info.clone()));
        until(k, || w.svc.snapshot().recording.as_ref() == Some(&info)).await;
        assert_eq!(w.sink.of("recording").last(), Some(&f["recording"][k]));
        let snap = serde_json::to_value(w.svc.state().await.expect("state")).expect("json");
        assert_eq!(snap["recording"], f["recording"][k]);
    }
    w.svc
        .consent(tauri_plugin_sew_voice::ConsentArgs {
            granted: true,
            notice_version: 9,
        })
        .await
        .expect("consent");
    w.svc.withdraw_consent().await.expect("withdraw");
    assert_eq!(call.consents(), vec![(true, Some(9)), (false, None)]);
    call.fail_next_consent(EngineError::Provider(held_refusal()));
    let e = w
        .svc
        .consent(tauri_plugin_sew_voice::ConsentArgs {
            granted: true,
            notice_version: 9,
        })
        .await
        .expect_err("changed notice");
    assert_eq!(e.code, "recording_consent_required");
    assert_eq!(e.detail["consent_prompt"]["notice_version"], json!(3141));
    assert_eq!(
        w.svc.snapshot().state,
        CallState::Connected,
        "the call goes on"
    );
    call.set_recording(None);
    until("stopped", || w.svc.snapshot().recording.is_none()).await;
    assert_eq!(
        w.sink.of("recording").last(),
        Some(&f["recording"]["stopped"])
    );
    let snap = serde_json::to_value(w.svc.state().await.expect("state")).expect("json");
    assert!(snap.get("recording").is_none(), "absent with none: {snap}");
}

#[tokio::test]
async fn consent_without_a_call_is_not_in_call() {
    let w = world();
    let e = w
        .svc
        .consent(tauri_plugin_sew_voice::ConsentArgs {
            granted: true,
            notice_version: 1,
        })
        .await
        .expect_err("no call");
    assert_eq!(e.code, "not_in_call");
    let e = w.svc.withdraw_consent().await.expect_err("no call");
    assert_eq!(e.code, "not_in_call");
    assert!(w.sink.events().is_empty());
}

/// The recording leave reasons end a call as the fixture shows.
#[tokio::test]
async fn recording_end_reasons_reach_the_webview_unchanged() {
    let f = fixture();
    let cases = f["ended"]["recording_reasons"]
        .as_array()
        .expect("recording")
        .clone();
    assert_eq!(cases.len(), 3);
    for c in cases {
        let w = joined().await;
        let call = w.engine.last_call().expect("call");
        w.sink.clear();
        let reason = c["reason"].as_str().expect("reason");
        call.end(reason);
        until(reason, || !w.svc.snapshot().state.is_live()).await;
        assert_eq!(states(&w.sink).last(), Some(&c["payload"]), "{reason}");
    }
}

fn key(install_id: u64, member: u64, scope: &str) -> tauri_plugin_sew_voice::DeviceKeyId {
    tauri_plugin_sew_voice::DeviceKeyId {
        install_id,
        member_id: sew_proto::id::Id::new(member).expect("id"),
        device_scope: scope.to_owned(),
    }
}

#[tokio::test]
async fn known_device_keys_and_forget_server_between_calls() {
    let w = world();
    assert_eq!(w.svc.known_device_keys().await.expect("known"), vec![]);
    w.engine
        .set_device_keys(vec![key(78, 6, "aa"), key(77, 5, "bb"), key(77, 5, "aa")]);
    assert_eq!(
        w.svc.known_device_keys().await.expect("known"),
        vec![key(77, 5, "aa"), key(77, 5, "bb"), key(78, 6, "aa")]
    );
    let gone = w.svc.forget_server(77).await.expect("forget");
    assert_eq!(gone, vec![key(77, 5, "aa"), key(77, 5, "bb")]);
    assert_eq!(w.engine.forgets(), vec![77]);
    assert_eq!(
        w.svc.known_device_keys().await.expect("known"),
        vec![key(78, 6, "aa")]
    );
    // Nothing left there: an empty answer, not an error.
    assert_eq!(w.svc.forget_server(77).await.expect("again"), vec![]);
    // No event: a sign-out is not a call change.
    assert!(w.sink.of("errors").is_empty());
}

#[tokio::test]
async fn forget_server_is_refused_in_a_call_and_runs_after_it() {
    let w = joined().await;
    w.engine.set_device_keys(vec![key(77, 5, "aa")]);
    let e = w.svc.forget_server(77).await.expect_err("in a call");
    assert_eq!(e.code, "in_call");
    assert!(w.engine.forgets().is_empty(), "the engine was never asked");
    // Listing is fine in a call.
    assert_eq!(
        w.svc.known_device_keys().await.expect("known"),
        vec![key(77, 5, "aa")]
    );
    w.svc.leave().await.expect("leave");
    assert_eq!(
        w.svc.forget_server(77).await.expect("after leave"),
        vec![key(77, 5, "aa")]
    );
}

#[tokio::test]
async fn a_failed_forget_keeps_the_keys_and_passes_the_error() {
    let w = world();
    w.engine.set_device_keys(vec![key(77, 5, "aa")]);
    w.engine
        .fail_next_forget(EngineError::Failed(ErrorInfo::with_detail(
            "device_store",
            "hardware key unavailable: errSecInteractionNotAllowed",
        )));
    let e = w.svc.forget_server(77).await.expect_err("fails");
    assert_eq!(e.code, "device_store");
    assert_eq!(
        w.svc.known_device_keys().await.expect("known"),
        vec![key(77, 5, "aa")],
        "a retry finds it again"
    );
    assert_eq!(
        w.svc.forget_server(77).await.expect("retry"),
        vec![key(77, 5, "aa")]
    );
}

#[test]
fn device_key_wire_shapes() {
    use tauri_plugin_sew_voice::{DeviceKeyInfo, ForgetServerArgs};
    let k = key(48831765147549700, 48831765147549699, "3f9a0c47");
    let info = DeviceKeyInfo::from(&k);
    assert_eq!(
        serde_json::to_value(&info).expect("json"),
        json!({
            "install_id": "48831765147549700",
            "member_id": "48831765147549699",
            "device_scope": "3f9a0c47",
            "account": "48831765147549700:48831765147549699:3f9a0c47",
        })
    );
    assert_eq!(info.account, k.account());
    let a: ForgetServerArgs =
        serde_json::from_value(json!({"install_id": "48831765147549700"})).expect("args");
    assert_eq!(a.install().expect("install"), 48831765147549700);
    for bad in [json!({"install_id": "x"}), json!({"install_id": "-1"})] {
        let a: ForgetServerArgs = serde_json::from_value(bad).expect("parses");
        assert_eq!(a.install().expect_err("refused").code, "invalid_args");
    }
    assert!(
        serde_json::from_value::<ForgetServerArgs>(json!({"install_id": 77})).is_err(),
        "a number is refused: ids are decimal strings"
    );
    assert!(
        serde_json::from_value::<ForgetServerArgs>(json!({"install_id": "77", "all": true}))
            .is_err()
    );
}
