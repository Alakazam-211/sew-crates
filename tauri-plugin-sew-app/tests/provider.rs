//! The ready JoinProvider over the fake lane, alone and under the voice plugin's service.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::json;
use tauri_plugin_sew_app::fake::{FakeLane, RecordingAppSink, lane_error};
use tauri_plugin_sew_app::{
    AccountState, AppConfig, AppService, LaneClient, LaneJoinProvider, LaneResponse, Origin,
    Secret, SecretChoice,
};
use tauri_plugin_sew_voice::fake::{FakeEngine, RecordingSink};
use tauri_plugin_sew_voice::{
    ConsentAnswer, JoinArgs, JoinProvider, JoinRequest, ProviderError, RejoinReason, Spawn,
    VoiceService,
};

const APP: &str = "https://myapp.example.com";

async fn signed_in() -> (AppService, FakeLane, LaneJoinProvider) {
    let lane = FakeLane::new();
    lane.add_user("carol", "carol-pw", "1003", "Carol");
    let mut cfg = AppConfig::new("com.example.test");
    cfg.secrets = SecretChoice::Memory;
    let svc = AppService::new(
        cfg,
        LaneClient::new(Arc::new(lane.clone())),
        Arc::new(RecordingAppSink::default()),
    )
    .unwrap();
    let origin = Origin::parse(APP, false).unwrap();
    svc.add_server(APP).await.unwrap();
    svc.confirm_server(&origin).unwrap();
    svc.sign_in_password(&origin, "bob", Secret::new("bob-test-pw"), None)
        .await
        .unwrap();
    let p = LaneJoinProvider::new(svc.clone());
    (svc, lane, p)
}

fn req(room: &str) -> JoinRequest {
    JoinRequest::new("myapp", room).with_muted(true)
}

#[tokio::test]
async fn a_join_posts_the_request_unchanged_with_the_bearer() {
    let (svc, lane, p) = signed_in().await;
    let a = p.join(req("5")).await.unwrap();
    assert_eq!(a.api_base.as_str(), format!("{APP}/sew/native/"));
    assert_eq!(a.join.url.as_str(), "wss://myapp.example.com/sew/native");
    assert!(a.call_credential().is_some());
    let sent = lane
        .seen()
        .into_iter()
        .rfind(|r| r.path == "/sew/native/voice/join")
        .unwrap();
    assert_eq!(
        sent.body.unwrap(),
        json!({"target": "myapp", "room": "5", "muted": true, "deafened": false, "rejoin": null})
    );
    let key = svc.active().unwrap();
    assert_eq!(
        sent.bearer.unwrap(),
        svc.bearer(&key).await.unwrap().expose()
    );
    assert_eq!(svc.call_account(), Some(key));
}

/// Sew 0.9.0: Record and Stop go to the lane's `voice/recording` with the call's account and
/// the voice plugin's body; a lane without the route is `recording_unsupported`; a refusal is the
/// lane's code.
#[tokio::test]
async fn record_and_stop_go_to_the_lanes_recording_route() {
    use tauri_plugin_sew_voice::RecordingRequest;
    let (svc, lane, p) = signed_in().await;
    p.join(req("5")).await.unwrap();
    let bearer = svc.bearer(&svc.active().unwrap()).await.unwrap();
    let start = RecordingRequest::start("myapp", "5", "5".parse().unwrap());
    // This fake lane has no recording route: as an older kit's.
    match p.recording(start.clone()).await {
        Err(ProviderError::Refused { code, .. }) => assert_eq!(code, "recording_unsupported"),
        other => panic!("{other:?}"),
    }
    let rec = json!({
        "id": "77", "channel_id": "5", "state": "recording", "started_by": "1002",
        "started_at": "2026-11-06T12:00:00Z", "auto": false, "message_id": "88",
        "mixed": false, "audio_keep_days": 30, "max_duration_ms": 14400000,
        "participants": [], "my_consent": "granted"
    });
    lane.answer_next(
        "/sew/native/voice/recording",
        LaneResponse {
            status: 200,
            retry_after_ms: None,
            body: serde_json::to_vec(&rec).unwrap(),
        },
    );
    let r = p.recording(start).await.unwrap();
    assert_eq!(r.id.to_string(), "77");
    let sent = lane
        .seen()
        .into_iter()
        .rfind(|r| r.path == "/sew/native/voice/recording")
        .unwrap();
    assert_eq!(sent.bearer.as_deref(), Some(bearer.expose()));
    assert_eq!(
        sent.body,
        Some(json!({"target": "myapp", "room": "5", "action": "start"}))
    );
    lane.answer_next(
        "/sew/native/voice/recording",
        lane_error(
            422,
            "recording_required",
            json!({"reason": "recording_required"}),
            "This room records every call.",
        ),
    );
    let stop = RecordingRequest::stop("myapp", "5", "5".parse().unwrap(), "77".parse().unwrap());
    match p.recording(stop).await {
        Err(ProviderError::Refused { code, .. }) => assert_eq!(code, "recording_required"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn re_joins_and_consent_keep_the_calls_account() {
    let (svc, lane, p) = signed_in().await;
    let origin = Origin::parse(APP, false).unwrap();
    let bob = svc.active().unwrap();
    p.join(req("5")).await.unwrap();
    // The person switches to another account mid-call.
    let carol = svc
        .sign_in_password(&origin, "carol", Secret::new("carol-pw"), None)
        .await
        .unwrap();
    assert_eq!(svc.active(), Some(carol.key.clone()));
    let bob_bearer = svc.bearer(&bob).await.unwrap();
    p.join(req("5").with_rejoin(RejoinReason::AdmitLost))
        .await
        .unwrap();
    let sent = lane
        .seen()
        .into_iter()
        .rfind(|r| r.path == "/sew/native/voice/join")
        .unwrap();
    assert_eq!(
        sent.bearer.as_deref(),
        Some(bob_bearer.expose()),
        "the re-join is bob's"
    );
    let ans: ConsentAnswer = serde_json::from_value(json!({
        "target": "myapp", "room": "5", "recording_id": "77", "consent": true,
        "notice_version": 3, "device_id": "dev-1"
    }))
    .unwrap();
    let rec = p.consent(ans).await.unwrap();
    assert_eq!(
        serde_json::to_value(rec.my_consent).unwrap(),
        json!("granted")
    );
    let sent = lane
        .seen()
        .into_iter()
        .rfind(|r| r.path == "/sew/native/voice/consent")
        .unwrap();
    assert_eq!(sent.bearer.as_deref(), Some(bob_bearer.expose()));
    assert_eq!(lane.consents()[0]["notice_version"], 3);
    // A new call takes the active account.
    p.join(req("6")).await.unwrap();
    assert_eq!(svc.call_account(), Some(carol.key));
}

#[tokio::test]
async fn a_recorded_room_holds_for_consent_then_joins() {
    let (_svc, lane, p) = signed_in().await;
    lane.record_room("9", 77, 3141);
    let e = p.join(req("9")).await.unwrap_err();
    let ProviderError::ConsentRequired(c) = e else {
        panic!("{e:?}")
    };
    assert_eq!(c.notice_version(), 3141);
    let a = p
        .join(req("9").with_recording_notice(c.notice(true)))
        .await
        .unwrap();
    assert!(a.call_credential().is_some());
}

#[tokio::test]
async fn refusals_map_as_the_voice_plugin_reads_them() {
    let (svc, lane, p) = signed_in().await;
    let join = "/sew/native/voice/join";
    let refuse = |status: u16, retry: Option<u64>, body: serde_json::Value| LaneResponse {
        status,
        retry_after_ms: retry,
        body: serde_json::to_vec(&body).unwrap(),
    };
    // No native lane here (the lane is off: its generic 404).
    lane.answer_next(
        join,
        lane_error(404, "not_found", json!(null), "Not found."),
    );
    match p.join(req("5")).await.unwrap_err() {
        ProviderError::Refused { code, detail } => {
            assert_eq!(code, "lane_not_found");
            assert_eq!(detail["route"], join);
        }
        e => panic!("{e:?}"),
    }
    // A bare 404 too.
    lane.answer_next(join, refuse(404, None, json!("nope")));
    assert!(matches!(
        p.join(req("5")).await.unwrap_err(),
        ProviderError::Refused { code, .. } if code == "lane_not_found"
    ));
    // Sew's own 404 for the room keeps its code.
    lane.answer_next(join, lane_error(404, "unknown_channel", json!(null), "x"));
    assert!(matches!(
        p.join(req("5")).await.unwrap_err(),
        ProviderError::Refused { code, .. } if code == "unknown_channel"
    ));
    for status in [429, 503] {
        lane.answer_next(join, refuse(status, Some(2000), json!({})));
        assert_eq!(
            p.join(req("5")).await.unwrap_err(),
            ProviderError::Unavailable {
                retry_after_ms: Some(2000)
            }
        );
    }
    lane.answer_next(join, lane_error(403, "forbidden", json!({"why": 1}), "x"));
    assert_eq!(
        p.join(req("5")).await.unwrap_err(),
        ProviderError::Refused {
            code: "forbidden".into(),
            detail: json!({"why": 1})
        }
    );
    lane.answer_next(join, refuse(502, None, json!({})));
    assert!(matches!(
        p.join(req("5")).await.unwrap_err(),
        ProviderError::Refused { code, .. } if code == "http_502"
    ));
    // No answer at all.
    lane.set_down(true);
    assert_eq!(
        p.join(req("5")).await.unwrap_err(),
        ProviderError::Unavailable {
            retry_after_ms: None
        }
    );
    lane.set_down(false);
    // A 401 is SigninRequired, and the account is signed out here.
    let key = svc.active().unwrap();
    lane.revoke_device_id(&key.account, "revoked");
    assert_eq!(
        p.join(req("5")).await.unwrap_err(),
        ProviderError::SigninRequired
    );
    let state = svc.book().account(&key).unwrap().state.clone();
    assert_eq!(
        state,
        AccountState::SignedOut {
            reason: "revoked".into()
        }
    );
    // And stays so without asking the server again.
    let before = lane.seen().len();
    assert_eq!(
        p.join(req("5").with_rejoin(RejoinReason::CredentialExpired))
            .await
            .unwrap_err(),
        ProviderError::SigninRequired
    );
    assert_eq!(lane.seen().len(), before);
}

#[tokio::test]
async fn an_unbound_provider_says_so_and_no_account_is_signin_required() {
    let p = LaneJoinProvider::unbound();
    assert!(matches!(
        p.join(req("5")).await.unwrap_err(),
        ProviderError::Other(_)
    ));
    let mut cfg = AppConfig::new("x");
    cfg.secrets = SecretChoice::Memory;
    let svc = AppService::new(
        cfg,
        LaneClient::new(Arc::new(FakeLane::new())),
        Arc::new(RecordingAppSink::default()),
    )
    .unwrap();
    p.bind(svc);
    assert_eq!(
        p.join(req("5")).await.unwrap_err(),
        ProviderError::SigninRequired
    );
}

#[tokio::test]
async fn the_voice_plugin_joins_through_it() {
    let (_svc, lane, p) = signed_in().await;
    let sink = Arc::new(RecordingSink::new());
    let spawn: Spawn = Arc::new(|f| {
        tokio::spawn(f);
    });
    let voice = VoiceService::new(Arc::new(FakeEngine::new()), Arc::new(p), sink, spawn);
    voice.start();
    let r = voice.join(JoinArgs::new("myapp", "5")).await.unwrap();
    assert!(!r.call_id.is_empty());
    assert!(
        lane.seen()
            .iter()
            .any(|s| s.path == "/sew/native/voice/join")
    );
}
