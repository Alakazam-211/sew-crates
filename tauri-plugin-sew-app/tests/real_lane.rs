//! Against a real App server's native lane (a development App server with the lane on). Built
//! only with `--cfg sew_real_lane`; run it with the server named:
//!
//! ```text
//! SEW_APP_LANE_URL=https://<app>   (http://127.0.0.1:<port> is allowed)
//! SEW_APP_LANE_USER=<username> SEW_APP_LANE_PASSWORD=<password>
//! [SEW_APP_LANE_ROOM=<voice channel id>]   also joins (the join's answer only; no media)
//! RUSTFLAGS="--cfg sew_real_lane" cargo test --test real_lane -- --nocapture
//! ```
//!
//! It signs in with the password form, reads whoami and the device list, takes a handoff
//! code, joins the room when one is named, signs out, and checks the old bearer is refused
//! `401 signed_out`. With the variables missing it fails (never a silent pass).
#![cfg(all(feature = "http", sew_real_lane))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::sync::Arc;

use tauri_plugin_sew_app::fake::RecordingAppSink;
use tauri_plugin_sew_app::http::HttpTransport;
use tauri_plugin_sew_app::{
    AppConfig, AppService, LaneClient, LaneFailure, LaneJoinProvider, Origin, Secret, SecretChoice,
};
use tauri_plugin_sew_voice::{JoinProvider, JoinRequest};

fn env(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| panic!("{name} is not set (see the file's header)"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_lane_signs_in_hands_off_joins_and_signs_out() {
    let url = env("SEW_APP_LANE_URL");
    let user = env("SEW_APP_LANE_USER");
    let password = env("SEW_APP_LANE_PASSWORD");
    let room = std::env::var("SEW_APP_LANE_ROOM")
        .ok()
        .filter(|r| !r.is_empty());

    let transport =
        Arc::new(HttpTransport::new("sew-build (+https://github.com/Alakazam-211)", None).unwrap());
    let lane = LaneClient::new(transport);
    let mut cfg = AppConfig::new("com.alakazamlabs.sew-app-real-lane");
    cfg.secrets = SecretChoice::Memory;
    cfg.allow_loopback_http = true;
    cfg.device_name = "sew-app real-lane test".into();
    let svc = AppService::new(cfg, lane.clone(), Arc::new(RecordingAppSink::default())).unwrap();

    let p = svc.add_server(&url).await.unwrap();
    eprintln!(
        "real_lane: {} lane {} signalling {:?}",
        p.origin, p.info.lane, p.info.signalling
    );
    let origin: Origin = p.origin.clone();
    svc.confirm_server(&origin).unwrap();
    let a = svc
        .sign_in_password(&origin, &user, Secret::new(password), None)
        .await
        .unwrap();
    assert!(!a.member_id.is_empty());
    let bearer = svc.bearer(&a.key).await.unwrap();
    let who = lane.whoami(&origin, &bearer).await.unwrap();
    eprintln!(
        "real_lane: member {} install {:?}",
        a.member_id,
        who.install_id()
    );
    let devices = svc.devices(&a.key).await.unwrap();
    assert!(
        devices.iter().any(|d| d.current == Some(true)),
        "this session is listed as current"
    );
    let h = svc.handoff(&a.key, None).await.unwrap();
    assert!(!h.code.expose().is_empty());

    if let Some(room) = room {
        let provider = LaneJoinProvider::new(svc.clone());
        let answer = provider
            .join(JoinRequest::new("real-lane", room).with_muted(true))
            .await
            .unwrap();
        assert!(
            answer.call_credential().is_some(),
            "the lane's join carries a call credential"
        );
        eprintln!("real_lane: joined, api_base {}", answer.api_base);
    }

    let out = svc.sign_out(&a.key).await.unwrap();
    assert!(out.server_signed_out);
    match lane.whoami(&origin, &bearer).await {
        Err(LaneFailure::Refused(r)) => {
            assert_eq!(r.status, 401);
            assert_eq!(r.code.as_deref(), Some("signed_out"));
            assert_eq!(r.signed_out_reason(), "signed_out");
        }
        other => panic!("the old bearer still works: {other:?}"),
    }
}
