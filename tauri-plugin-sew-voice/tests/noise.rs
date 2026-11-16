//! Noise removal through the plugin, on the fake engine: `set_noise_removal {mode}` and
//! `noise_removal` answer `{mode, active, reason?}`, the `noise_removal` event follows each
//! change, a mode this version can't run is refused with nothing changed, and an engine without
//! noise removal takes only `off`.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value, json};
use tauri_plugin_sew_voice::fake::{FakeEngine, FakeProvider, RecordingSink};
use tauri_plugin_sew_voice::{
    BoxFuture, CallHandle, EngineError, JoinArgs, JoinProvider, JoinRequest, NoiseRemoval,
    NoiseRemovalReason, NoiseRemovalState, SetNoiseRemovalArgs, Spawn, VoiceEngine, VoiceService,
};

const ME: &str = "48831765147549699";

struct World {
    svc: VoiceService,
    engine: Arc<FakeEngine>,
    sink: Arc<RecordingSink>,
}

fn world() -> World {
    let engine = Arc::new(FakeEngine::new());
    let sink = Arc::new(RecordingSink::new());
    let spawn: Spawn = Arc::new(|f| {
        tokio::spawn(f);
    });
    let svc = VoiceService::new(
        engine.clone(),
        Arc::new(FakeProvider::answering(ME)),
        sink.clone(),
        spawn,
    );
    svc.start();
    World { svc, engine, sink }
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

fn events(sink: &RecordingSink) -> Vec<Value> {
    sink.of("noise_removal")
}

async fn set(w: &World, mode: &str) -> Result<Value, String> {
    w.svc
        .set_noise_removal(SetNoiseRemovalArgs::new(mode))
        .await
        .map(|s| serde_json::to_value(s).expect("json"))
        .map_err(|e| e.code)
}

async fn get(w: &World) -> Value {
    serde_json::to_value(w.svc.noise_removal().await.expect("noise_removal")).expect("json")
}

#[tokio::test]
async fn set_and_get_before_and_during_a_call_with_events() {
    let w = world();
    // The default: Standard, running. No event before a change.
    assert_eq!(
        get(&w).await,
        json!({"mode": "standard", "active": "standard"})
    );
    settle().await;
    assert!(events(&w.sink).is_empty());

    // Before a call.
    assert_eq!(
        set(&w, "off").await,
        Ok(json!({"mode": "off", "active": "off"}))
    );
    until("the off event", || events(&w.sink).len() == 1).await;
    assert_eq!(
        events(&w.sink),
        vec![json!({"mode": "off", "active": "off"})]
    );

    // During a call: the setting is the engine's, so it holds across the join.
    w.svc
        .join(JoinArgs {
            target: "k2:acme".into(),
            room: "48831765147549698".into(),
            muted: None,
            deafened: None,
        })
        .await
        .expect("join");
    assert_eq!(get(&w).await, json!({"mode": "off", "active": "off"}));
    assert_eq!(
        set(&w, "standard").await,
        Ok(json!({"mode": "standard", "active": "standard"}))
    );
    until("the standard event", || events(&w.sink).len() == 2).await;

    // The same mode again: the same answer and no event.
    assert_eq!(
        set(&w, "standard").await,
        Ok(json!({"mode": "standard", "active": "standard"}))
    );
    settle().await;
    assert_eq!(events(&w.sink).len(), 2);

    // The pipeline steps the stage down: the event says why; then it runs again.
    w.engine.step_down_noise(Some(NoiseRemovalReason::Budget));
    until("the budget event", || events(&w.sink).len() == 3).await;
    assert_eq!(
        events(&w.sink)[2],
        json!({"mode": "standard", "active": "off", "reason": "budget"})
    );
    assert_eq!(
        get(&w).await,
        json!({"mode": "standard", "active": "off", "reason": "budget"})
    );
    w.engine.step_down_noise(Some(NoiseRemovalReason::Fault));
    until("the fault event", || events(&w.sink).len() == 4).await;
    assert_eq!(events(&w.sink)[3]["reason"], json!("fault"));
    // Off asked while stepped down: nothing is missing.
    assert_eq!(
        set(&w, "off").await,
        Ok(json!({"mode": "off", "active": "off"}))
    );
    w.engine.step_down_noise(None);
    assert_eq!(
        set(&w, "standard").await,
        Ok(json!({"mode": "standard", "active": "standard"}))
    );
    until("six events", || events(&w.sink).len() == 6).await;

    w.svc.leave().await.expect("leave");
    assert_eq!(
        get(&w).await,
        json!({"mode": "standard", "active": "standard"}),
        "the setting outlives the call"
    );
    assert_eq!(
        w.engine.noise_sets(),
        vec![
            NoiseRemoval::Off,
            NoiseRemoval::Standard,
            NoiseRemoval::Standard,
            NoiseRemoval::Off,
            NoiseRemoval::Standard,
        ]
    );
}

#[tokio::test]
async fn an_unavailable_mode_is_refused_and_nothing_changes() {
    let w = world();
    set(&w, "off").await.expect("off");
    until("the off event", || events(&w.sink).len() == 1).await;
    assert_eq!(
        set(&w, "strong").await,
        Err("noise_mode_unavailable".into())
    );
    let refused = w
        .svc
        .set_noise_removal(SetNoiseRemovalArgs::new("strong"))
        .await
        .expect_err("strong");
    assert_eq!(refused.detail, json!("strong"));
    // Not a mode at all: invalid_args. Names are exact.
    assert_eq!(set(&w, "loud").await, Err("invalid_args".into()));
    assert_eq!(set(&w, "Standard").await, Err("invalid_args".into()));
    assert_eq!(set(&w, "").await, Err("invalid_args".into()));
    settle().await;
    assert_eq!(get(&w).await, json!({"mode": "off", "active": "off"}));
    assert_eq!(events(&w.sink).len(), 1, "no event for a refusal");
    assert_eq!(
        w.engine.noise_sets(),
        vec![NoiseRemoval::Off],
        "a refused mode never reaches the engine"
    );
}

#[tokio::test]
async fn a_platform_without_the_stage_says_unavailable() {
    let w = world();
    w.engine
        .step_down_noise(Some(NoiseRemovalReason::Unavailable));
    until("the unavailable event", || events(&w.sink).len() == 1).await;
    assert_eq!(
        get(&w).await,
        json!({"mode": "standard", "active": "off", "reason": "unavailable"})
    );
    // Off is still taken (there is nothing to remove).
    assert_eq!(
        set(&w, "off").await,
        Ok(json!({"mode": "off", "active": "off"}))
    );
}

/// An engine that implements only what it must: the provided noise removal methods.
#[derive(Debug)]
struct Bare;

impl VoiceEngine for Bare {
    fn join(
        &self,
        _provider: Arc<dyn JoinProvider>,
        _req: JoinRequest,
    ) -> BoxFuture<'_, Result<Arc<dyn CallHandle>, EngineError>> {
        Box::pin(std::future::ready(Err(EngineError::failed("unused"))))
    }
    fn devices(&self) -> Result<tauri_plugin_sew_voice::DeviceList, EngineError> {
        Ok(tauri_plugin_sew_voice::DeviceList::default())
    }
    fn set_device(
        &self,
        _input: Option<tauri_plugin_sew_voice::DeviceChoice>,
        _output: Option<tauri_plugin_sew_voice::DeviceChoice>,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    fn device_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<tauri_plugin_sew_voice::DeviceList> {
        tokio::sync::broadcast::channel(1).1
    }
}

#[tokio::test]
async fn an_engine_without_noise_removal_takes_only_off() {
    let engine: Arc<dyn VoiceEngine> = Arc::new(Bare);
    assert_eq!(
        engine.noise_removal(),
        NoiseRemovalState::running(NoiseRemoval::Off)
    );
    assert!(engine.noise_removal_events().is_none());
    assert_eq!(
        engine.set_noise_removal(NoiseRemoval::Off),
        Ok(NoiseRemovalState::running(NoiseRemoval::Off))
    );
    assert_eq!(
        engine
            .set_noise_removal(NoiseRemoval::Standard)
            .expect_err("standard")
            .to_info()
            .code,
        "noise_mode_unavailable"
    );
}

fn fixture() -> Value {
    let all: Value =
        serde_json::from_str(include_str!("native-voice-contract.json")).expect("the fixture");
    all["noise_removal"].clone()
}

/// The contract fixture's noise removal section (what the host's settings and the kit replay):
/// each command's answer and event, each refusal, each step-down.
#[tokio::test]
async fn the_fixture_replays() {
    let fx = fixture();
    assert_eq!(fx["permission"], json!("sew-voice:noise-removal"));
    let w = world();
    assert_eq!(get(&w).await, fx["default"]);
    for (i, step) in fx["set"].as_array().expect("set").iter().enumerate() {
        assert_eq!(step["command"], json!("set_noise_removal"));
        let args: SetNoiseRemovalArgs = serde_json::from_value(step["args"].clone()).expect("args");
        let answer = w.svc.set_noise_removal(args).await.expect("set");
        assert_eq!(serde_json::to_value(answer).expect("json"), step["answer"]);
        until("the event", || events(&w.sink).len() == i + 1).await;
        assert_eq!(events(&w.sink)[i], step["event"]);
    }
    assert_eq!(fx["get"]["command"], json!("noise_removal"));
    assert_eq!(get(&w).await, fx["get"]["answer"]);
    for e in fx["errors"].as_array().expect("errors") {
        let args: SetNoiseRemovalArgs = serde_json::from_value(e["args"].clone()).expect("args");
        let err = w.svc.set_noise_removal(args).await.expect_err("refused");
        assert_eq!(serde_json::to_value(err).expect("json"), e["error"]);
    }
    let reasons = [
        NoiseRemovalReason::Budget,
        NoiseRemovalReason::Fault,
        NoiseRemovalReason::Unavailable,
    ];
    let stepped = fx["stepped_down"].as_array().expect("stepped_down");
    assert_eq!(stepped.len(), reasons.len());
    for (r, want) in reasons.into_iter().zip(stepped) {
        let before = events(&w.sink).len();
        w.engine.step_down_noise(Some(r));
        until("the step-down event", || {
            events(&w.sink).len() == before + 1
        })
        .await;
        assert_eq!(events(&w.sink).last(), Some(want));
        assert_eq!(&get(&w).await, want);
    }
}
