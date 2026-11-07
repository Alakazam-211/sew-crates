//! The call details (0.4.1) against the fake engine: the security, roster, quality and incoming
//! events, the call_details answer, and the ring commands, as `native-voice-details.json` shows
//! them.

// The helpers (not only #[test] fns) unwrap: a test that cannot set up must fail loudly.
#![allow(clippy::expect_used)]

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tauri_plugin_sew_voice::fake::{FakeEngine, FakeProvider, RecordingListener, RecordingSink};
use tauri_plugin_sew_voice::{
    CallDetails, CallState, IncomingCall, JoinArgs, QualityInfo, RingEndedArgs, RosterEntry,
    SecurityInfo, Spawn, VoiceControl, VoiceService,
};

const ME: &str = "48831765147549699";
const TARGET: &str = "k2:acme";
const CHANNEL: &str = "48831765147549698";

struct World {
    svc: VoiceService,
    engine: Arc<FakeEngine>,
    sink: Arc<RecordingSink>,
    listener: Arc<RecordingListener>,
}

fn world() -> World {
    let engine = Arc::new(FakeEngine::new());
    let sink = Arc::new(RecordingSink::new());
    let listener = Arc::new(RecordingListener::new());
    let spawn: Spawn = Arc::new(|f| {
        tokio::spawn(f);
    });
    let svc = VoiceService::with_listener(
        engine.clone(),
        Arc::new(FakeProvider::answering(ME)),
        sink.clone(),
        listener.clone(),
        spawn,
    );
    svc.start();
    World {
        svc,
        engine,
        sink,
        listener,
    }
}

fn args(target: &str, room: &str) -> JoinArgs {
    JoinArgs {
        target: target.into(),
        room: room.into(),
        muted: None,
        deafened: None,
    }
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("native-voice-details.json")).expect("the fixture parses")
}

fn from<T: DeserializeOwned>(v: &Value) -> T {
    serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{v}: {e}"))
}

async fn until(what: &str, mut pred: impl FnMut() -> bool) {
    for _ in 0..2000 {
        if pred() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("timed out waiting for: {what}");
}

async fn settle() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn no_details_before_a_call() {
    let f = fixture();
    let w = world();
    assert_eq!(
        serde_json::to_value(w.svc.details()).expect("json"),
        f["call_details"]["idle"]
    );
    assert!(w.sink.events().is_empty());
}

#[tokio::test]
async fn security_roster_and_quality_follow_the_engine() {
    let f = fixture();
    let w = world();
    w.svc.join(args(TARGET, CHANNEL)).await.expect("join");
    let call = w.engine.last_call().expect("call");
    w.sink.clear();

    call.set_security(Some(from(&f["security"]["securing"])));
    call.set_security(Some(from(&f["security"]["active"])));
    let roster: Vec<RosterEntry> = from(&f["roster"]["payload"]);
    call.set_roster(roster.clone());
    call.set_quality(from(&f["quality"]["good"]));
    until("three kinds", || w.sink.of("quality").len() == 1).await;
    until("roster", || w.sink.of("roster").len() == 1).await;
    assert_eq!(
        w.sink.of("security"),
        vec![
            f["security"]["securing"].clone(),
            f["security"]["active"].clone()
        ]
    );
    assert_eq!(w.sink.of("roster"), vec![f["roster"]["payload"].clone()]);
    assert_eq!(w.sink.of("quality"), vec![f["quality"]["good"].clone()]);
    // The same again says nothing new.
    call.set_quality(from(&f["quality"]["good"]));
    call.set_roster(roster);
    settle().await;
    assert_eq!(w.sink.of("quality").len(), 1);
    assert_eq!(w.sink.of("roster").len(), 1);

    let d = serde_json::to_value(w.svc.details()).expect("json");
    assert_eq!(d["security"], f["security"]["active"]);
    assert_eq!(d["quality"], f["quality"]["good"]);
    assert_eq!(d["roster"], f["roster"]["payload"]);
    assert_eq!(d["incoming"], Value::Null);
    // CallKit sees none of them.
    assert!(
        w.listener
            .calls()
            .iter()
            .all(|c| c.get("on_security").is_none()
                && c.get("on_roster").is_none()
                && c.get("on_quality").is_none()),
        "{:?}",
        w.listener.calls()
    );
}

#[tokio::test]
async fn quality_is_lost_while_reconnecting_and_unknown_after_the_call() {
    let f = fixture();
    let w = world();
    w.svc.join(args(TARGET, CHANNEL)).await.expect("join");
    let call = w.engine.last_call().expect("call");
    call.set_quality(from(&f["quality"]["excellent"]));
    until("excellent", || {
        w.svc.details().quality.quality == "excellent"
    })
    .await;
    call.reconnecting(1);
    until("lost", || w.svc.details().quality.quality == "lost").await;
    call.connect();
    until("back", || w.svc.details().quality.quality == "excellent").await;
    call.set_security(Some(from(&f["security"]["active"])));
    call.set_roster(from(&f["roster"]["payload"]));
    until("security", || w.svc.details().security.is_some()).await;
    until("roster", || !w.svc.details().roster.is_empty()).await;
    w.svc.leave().await.expect("leave");
    assert_eq!(
        w.sink.of("quality"),
        vec![
            f["quality"]["excellent"].clone(),
            f["quality"]["reconnecting"].clone(),
            f["quality"]["excellent"].clone(),
            f["quality"]["unmeasured"].clone(),
        ]
    );
    assert_eq!(
        w.sink.of("security").last(),
        Some(&f["security"]["no_call"])
    );
    assert_eq!(w.sink.of("roster").last(), Some(&json!([])));
    assert_eq!(
        serde_json::to_value(w.svc.details()).expect("json"),
        f["call_details"]["idle"]
    );
}

#[tokio::test]
async fn an_engine_end_clears_the_details() {
    let f = fixture();
    let w = world();
    w.svc.join(args(TARGET, CHANNEL)).await.expect("join");
    let call = w.engine.last_call().expect("call");
    call.set_security(Some(from(&f["security"]["active"])));
    call.set_quality(from(&f["quality"]["poor"]));
    until("poor", || w.svc.details().quality.quality == "poor").await;
    call.end("perms_lost");
    until("ended", || !w.svc.snapshot().state.is_live()).await;
    assert_eq!(
        serde_json::to_value(w.svc.details()).expect("json"),
        f["call_details"]["idle"]
    );
}

#[tokio::test]
async fn a_new_call_starts_with_its_engines_details() {
    let f = fixture();
    let w = world();
    w.svc.join(args(TARGET, CHANNEL)).await.expect("join");
    let call = w.engine.last_call().expect("call");
    call.set_security(Some(from(&f["security"]["active"])));
    call.set_roster(from(&f["call_details"]["in_call"]["roster"]));
    call.set_quality(from(&f["quality"]["good"]));
    until("all", || {
        serde_json::to_value(w.svc.details()).ok() == Some(f["call_details"]["in_call"].clone())
    })
    .await;
}

#[tokio::test]
async fn a_ring_is_set_cleared_and_accepted_by_its_join() {
    let f = fixture();
    let w = world();
    let control = VoiceControl::new(w.svc.clone());
    let ring: IncomingCall = from(&f["incoming"]["ring_args"]);
    w.svc.ring(ring.clone()).expect("ring");
    assert_eq!(
        w.sink.of("incoming"),
        vec![f["incoming"]["ringing"].clone()]
    );
    assert_eq!(control.incoming(), Some(ring.clone()));
    assert_eq!(
        serde_json::to_value(w.svc.details()).expect("json")["incoming"],
        f["incoming"]["ringing"]
    );
    // The same ring again is no new event; another call's end changes nothing.
    w.svc.ring(ring.clone()).expect("ring again");
    w.svc
        .ring_ended(RingEndedArgs {
            call_id: "1".into(),
        })
        .expect("other");
    assert_eq!(w.sink.of("incoming").len(), 1);
    // Its end clears it.
    w.svc
        .ring_ended(from(&f["incoming"]["ended_args"]))
        .expect("ended");
    assert_eq!(
        w.sink.of("incoming"),
        vec![
            f["incoming"]["ringing"].clone(),
            f["incoming"]["stopped"].clone()
        ]
    );
    assert_eq!(
        w.listener.calls(),
        vec![
            json!({"on_incoming": f["incoming"]["ringing"]}),
            json!({"on_incoming": null}),
        ]
    );
    // A ring, then a join elsewhere: it keeps ringing. A join with its target and room
    // accepts it.
    control.ring(ring.clone()).expect("ring");
    w.svc.join(args(TARGET, CHANNEL)).await.expect("join");
    assert_eq!(w.svc.incoming(), Some(ring.clone()));
    w.svc.leave().await.expect("leave");
    w.svc
        .join(args(&ring.target, &ring.room))
        .await
        .expect("accept");
    assert_eq!(w.svc.incoming(), None);
    assert_eq!(w.svc.snapshot().state, CallState::Connected);
    assert_eq!(w.sink.of("incoming").last(), Some(&Value::Null));
    control.ring_ended(&ring.call_id);
}

#[tokio::test]
async fn a_ring_needs_its_ids() {
    let w = world();
    for bad in [
        json!({"call_id": "", "target": "dm", "room": "1"}),
        json!({"call_id": "1", "target": "", "room": "1"}),
        json!({"call_id": "1", "target": "dm", "room": ""}),
    ] {
        let e = w.svc.ring(from(&bad)).expect_err("refused");
        assert_eq!(e.code, "invalid_args", "{bad}");
    }
    assert!(w.sink.events().is_empty());
}

#[test]
fn the_fixture_is_the_wire_types() {
    let f = fixture();
    for k in ["active", "hidden_by_policy", "securing", "recovering"] {
        let s: SecurityInfo = from(&f["security"][k]);
        assert_eq!(
            serde_json::to_value(&s).expect("json"),
            f["security"][k],
            "{k}"
        );
    }
    let r: Vec<RosterEntry> = from(&f["roster"]["payload"]);
    assert_eq!(
        serde_json::to_value(&r).expect("json"),
        f["roster"]["payload"]
    );
    for k in ["excellent", "good", "poor", "reconnecting", "unmeasured"] {
        let q: QualityInfo = from(&f["quality"][k]);
        assert_eq!(
            serde_json::to_value(&q).expect("json"),
            f["quality"][k],
            "{k}"
        );
    }
    for k in ["ring_args", "ringing"] {
        let i: IncomingCall = from(&f["incoming"][k]);
        assert_eq!(
            serde_json::to_value(&i).expect("json"),
            f["incoming"][k],
            "{k}"
        );
    }
    for k in ["idle", "in_call"] {
        let d: CallDetails = from(&f["call_details"][k]);
        assert_eq!(
            serde_json::to_value(&d).expect("json"),
            f["call_details"][k],
            "{k}"
        );
    }
    // `silent` defaults to false; unknown keys are refused.
    let i: IncomingCall = from(&json!({"call_id": "1", "target": "dm", "room": "2"}));
    assert!(!i.silent && i.started_by.is_none());
    assert!(
        serde_json::from_value::<IncomingCall>(
            json!({"call_id": "1", "target": "dm", "room": "2", "token": "x"})
        )
        .is_err()
    );
}

#[test]
fn the_fixture_quality_levels_are_the_rule() {
    let f = fixture();
    for k in ["excellent", "good", "poor"] {
        let q: QualityInfo = from(&f["quality"][k]);
        // From the counts behind the numbers: 1000 packets expected, 10 s played.
        let lost = (q.packet_loss.expect("loss") * 1000.0).round() as u32;
        let concealed = (q.concealed.expect("concealed") * 10_000.0).round() as u32;
        let again = QualityInfo::from_counts(
            1000,
            1000 - lost,
            lost,
            10_000,
            concealed,
            q.jitter_ms.expect("jitter"),
            q.rtt_ms,
        );
        assert_eq!(again.quality, q.quality, "{k}");
    }
}
