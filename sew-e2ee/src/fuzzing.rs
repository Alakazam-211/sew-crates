//! Fuzz entry points (feature `testing`). A fuzz target reads one input and calls one of these
//! (Sew's own fuzz targets, which run them under libFuzzer, are not published), and Sew's tests
//! replay every seed through them.
//!
//! Invariants: no panic on any input; decode∘encode is the identity on accepted input; every
//! session output keeps I1–I5. The fixture is deterministic (fixed keys, the fixture
//! clock), so a crash reproduces.

use std::cell::RefCell;

use openmls::prelude::{BasicCredential, Credential, MlsMessageIn};
use sew_proto::e2ee::tls::{
    LeafBindingTableTbs, RecorderCert, RecorderCertTbs, SewCallIdentity, SewDeviceCert, TlsCodec,
};
use sew_proto::e2ee::{
    B64, CallId, CallKeysInit, CallKeysMessage, CallScope, JoinIdHex, KeyResyncReason,
    LeafBindingTable, LkIdentity, Release, TransitionReason,
};
use tls_codec::{Deserialize as _, Serialize as _};
use zeroize::Zeroizing;

use crate::bridge::virtual_sdk::{Flavour, VirtualSdk};
use crate::bridge::{KeyEvent, KeyRing, KeySink};
use crate::config::{Params, caps};
use crate::identity::cert::CertContext;
use crate::identity::credential::{LeafContext, check_identity, check_leaf};
use crate::identity::{verify_device_cert, verify_table};
use crate::ids::{Consent, FenceId, Id, Slot};
use crate::keys::FrameIkm;
use crate::recorder::{ReleaseOpener, verify_recorder_cert};
use crate::session::{CallSession, Input, Output};
use crate::testing::fixtures::{Install, T0, TestDevice};
use crate::wire::{KeyReply, KeyRequest};

/// This crate's fuzz targets, in order.
pub const TARGETS: [&str; 9] = [
    "e2ee_server_msg_json",
    "e2ee_leaf_binding",
    "e2ee_call_identity_tls",
    "e2ee_device_cert_tls",
    "e2ee_recorder_cert_tls",
    "e2ee_mls_message_in",
    "e2ee_release_open",
    "e2ee_session_sequence",
    "e2ee_bridge_inputs",
];

struct Fixture {
    install: Install,
    device: TestDevice,
}

thread_local! {
    static FIXTURE: RefCell<Option<Fixture>> = const { RefCell::new(None) };
}

fn with_fixture<R>(f: impl FnOnce(&mut Fixture) -> R) -> R {
    FIXTURE.with(|cell| {
        let mut g = cell.borrow_mut();
        let fx = g.get_or_insert_with(|| {
            let mut install = Install::acme();
            let device = install.enroll(10_001, 20_001, 0x51, T0);
            Fixture { install, device }
        });
        f(fx)
    })
}

fn call_id() -> CallId {
    CallId::from_random([7; 16])
}

const GROUP: [u8; 16] = [0x42; 16];

fn scope() -> CallScope {
    Install::lounge()
}

fn init(install: &Install, create: bool) -> CallKeysInit {
    let j = JoinIdHex::from_bytes([1; 16]);
    CallKeysInit {
        call_id: call_id(),
        lk_identity: j.lk_identity(),
        join_id: j,
        scope: scope(),
        group_id: B64::new(GROUP.to_vec()),
        epoch: 0,
        create,
        activate_on_create: create,
        slot_base: 0,
        external_sender: B64::new(install.external_sender()),
        params: Params::default_wire(),
    }
}

/// A fresh session of the fixture device (`create`: the creator of a fresh call; else joining).
fn session(fx: &mut Fixture, create: bool) -> Option<CallSession> {
    let kp = fx.device.prepare(scope(), T0);
    kp.into_session(&fx.device.client, init(&fx.install, create), "gw-fuzz", T0)
        .ok()
}

fn drain(s: &mut CallSession) -> Vec<Output> {
    let mut v = Vec::new();
    while let Some(o) = s.poll_output() {
        v.push(o);
    }
    v
}

/// I1 and I5 on one batch of outputs: the local identity only in `Activate`, remote ones only
/// in `Install`/`Retire`/`Forget`.
fn check_outputs(s: &CallSession, outs: &[Output]) {
    let me = s.lk_identity();
    for o in outs {
        if let Output::Key(ev) = o {
            match ev {
                KeyEvent::Install { identity, .. } => {
                    assert!(identity != me, "I1: the local key in an Install")
                }
                KeyEvent::Activate { identity, .. } => {
                    assert!(identity == me, "Activate for a remote identity")
                }
                KeyEvent::Retire { identity, slot, .. } if identity == me => {
                    assert!(
                        s.local_slot() != Some(*slot),
                        "I4: the local send slot retired"
                    );
                }
                _ => {}
            }
        }
    }
}

// ------------------------------------------------------------------ JSON

/// `e2ee_server_msg_json`: a `call_keys_delivered` body. Accepted bodies round-trip through
/// JSON and are fed to a joining session (no panic).
pub fn e2ee_server_msg_json(data: &[u8]) {
    if data.len() > caps::BODY_JSON {
        return;
    }
    let Ok(m) = serde_json::from_slice::<CallKeysMessage>(data) else {
        return;
    };
    if let Ok(again) = serde_json::to_vec(&m) {
        let back = serde_json::from_slice::<CallKeysMessage>(&again);
        assert!(back.as_ref().is_ok_and(|b| *b == m), "JSON round trip");
    }
    with_fixture(|fx| {
        if let Some(mut s) = session(fx, false) {
            let _ = s.handle(T0, Input::Server(m.clone()));
            let outs = drain(&mut s);
            check_outputs(&s, &outs);
        }
        if let Some(mut s) = session(fx, true) {
            drain(&mut s);
            let _ = s.handle(T0, Input::Server(m));
            let outs = drain(&mut s);
            check_outputs(&s, &outs);
        }
    });
}

/// `e2ee_leaf_binding`: a signed table (JSON) through `verify_table`, and the TLS TBS codec.
pub fn e2ee_leaf_binding(data: &[u8]) {
    if data.len() > caps::LEAF_BINDING_TABLE {
        return;
    }
    if let Ok(t) = LeafBindingTableTbs::from_bytes(data) {
        assert_eq!(t.to_bytes().ok().as_deref(), Some(data), "TBS round trip");
    }
    if let Ok(t) = serde_json::from_slice::<LeafBindingTable>(data) {
        with_fixture(|fx| {
            if let Ok(v) = verify_table(&t, &fx.install.ds.public()) {
                for e in v.by_leaf.values().chain(v.by_ref.values()) {
                    assert!(
                        e.lk_identity == e.join_id.lk_identity(),
                        "identity = join id's"
                    );
                }
            }
        });
    }
}

// ------------------------------------------------------------------ TLS

fn leaf_ctx<'a>(ca: &'a [u8; 32], crl: &'a std::collections::BTreeSet<u64>) -> LeafContext<'a> {
    LeafContext {
        install_id: crate::testing::fixtures::INSTALL_ID,
        ca_public: ca,
        crl,
        scope: scope(),
        now: T0,
        skew_s: 300,
    }
}

/// `e2ee_call_identity_tls`: `SewCallIdentity` decode/encode and the full leaf check.
pub fn e2ee_call_identity_tls(data: &[u8]) {
    if data.len() > caps::CALL_IDENTITY {
        return;
    }
    with_fixture(|fx| {
        let ca = fx.install.ca.public();
        let crl = std::collections::BTreeSet::new();
        let ctx = leaf_ctx(&ca, &crl);
        if let Ok(i) = SewCallIdentity::from_bytes(data) {
            assert_eq!(i.to_bytes().ok().as_deref(), Some(data), "round trip");
            let _ = check_identity(&i, &[0u8; 32], &ctx);
        }
        let cred: Credential = BasicCredential::new(data.to_vec()).into();
        let _ = check_leaf(&cred, data.get(..32).unwrap_or(&[]), &ctx);
    });
}

/// `e2ee_device_cert_tls`: `SewDeviceCert` decode/encode and `verify_device_cert`.
pub fn e2ee_device_cert_tls(data: &[u8]) {
    if let Ok(c) = SewDeviceCert::from_bytes(data) {
        assert_eq!(c.to_bytes().ok().as_deref(), Some(data), "round trip");
    }
    with_fixture(|fx| {
        let ca = fx.install.ca.public();
        let ctx = CertContext {
            install_id: crate::testing::fixtures::INSTALL_ID,
            ca_public: &ca,
            skew_s: 300,
            now: T0,
        };
        if let Ok(v) = verify_device_cert(data, &ctx) {
            assert_eq!(v.to_bytes(), data, "a verified certificate is the input");
        }
    });
}

/// `e2ee_recorder_cert_tls`: `RecorderCert` decode/encode and `verify_recorder_cert`.
pub fn e2ee_recorder_cert_tls(data: &[u8]) {
    if let Ok(c) = RecorderCert::from_bytes(data) {
        assert_eq!(c.to_bytes().ok().as_deref(), Some(data), "round trip");
    }
    with_fixture(|fx| {
        let pk = data
            .get(..32)
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
            .unwrap_or([0; 32]);
        let _ = verify_recorder_cert(
            data,
            &fx.install.ca.public(),
            fx.install.install_id,
            &call_id(),
            1,
            &pk,
            T0,
        );
    });
}

// ------------------------------------------------------------------ MLS

/// `e2ee_mls_message_in`: our `MLSMessage` entry. Accepted messages round-trip; the bytes are
/// fed to a creator as a commit and a proposal, and to a joiner as a Welcome (no panic).
pub fn e2ee_mls_message_in(data: &[u8]) {
    if data.len() > caps::BODY_JSON {
        return;
    }
    if let Ok(m) = MlsMessageIn::tls_deserialize_exact(data) {
        assert_eq!(
            m.tls_serialize_detached().ok().as_deref(),
            Some(data),
            "round trip"
        );
    }
    with_fixture(|fx| {
        let table = |epoch: u64| {
            fx.install.ds.sign_table(&LeafBindingTableTbs {
                call_id: call_id().as_str().as_bytes().to_vec(),
                group_id: GROUP.to_vec(),
                epoch,
                entries: vec![],
            })
        };
        let (t0, t1) = (table(0), table(1));
        if let Some(mut s) = session(fx, true) {
            drain(&mut s);
            let _ = s.handle(
                T0,
                Input::Server(CallKeysMessage::Commit {
                    call_id: call_id(),
                    group_id: B64::new(GROUP.to_vec()),
                    epoch_from: 0,
                    commit: B64::new(data.to_vec()),
                    slot: 1,
                    bindings: t1.clone(),
                }),
            );
            let outs = drain(&mut s);
            check_outputs(&s, &outs);
        }
        if let Some(mut s) = session(fx, true) {
            drain(&mut s);
            let _ = s.handle(
                T0,
                Input::Server(CallKeysMessage::Proposals {
                    call_id: call_id(),
                    group_id: B64::new(GROUP.to_vec()),
                    epoch: 0,
                    proposals: vec![B64::new(data.to_vec())],
                    committer: 1,
                    reason: TransitionReason::Join,
                    bindings: t0,
                }),
            );
            let outs = drain(&mut s);
            check_outputs(&s, &outs);
        }
        if let Some(mut s) = session(fx, false) {
            let _ = s.handle(
                T0,
                Input::Server(CallKeysMessage::Welcome {
                    call_id: call_id(),
                    group_id: B64::new(GROUP.to_vec()),
                    join_id: JoinIdHex::from_bytes([1; 16]),
                    epoch: 1,
                    welcome: B64::new(data.to_vec()),
                    ratchet_tree: B64::new(data.to_vec()),
                    slot_base: 0,
                    slot: 1,
                    bindings: t1,
                }),
            );
            let outs = drain(&mut s);
            check_outputs(&s, &outs);
        }
    });
}

/// `e2ee_release_open`: a relayed release (`enc` ‖ `ct` ‖ epoch ‖ leaf ‖ slot) through
/// `ReleaseOpener::open` with the fixture recorder key. Random input never opens.
pub fn e2ee_release_open(data: &[u8]) {
    let take = |at: usize, n: usize| data.get(at..at + n).unwrap_or(&[]);
    let enc = data.get(..32.min(data.len())).unwrap_or(&[]).to_vec();
    let ct = take(32, 48).to_vec();
    let epoch = take(80, 8)
        .iter()
        .fold(0u64, |a, b| (a << 8) | u64::from(*b));
    let leaf = take(88, 4)
        .iter()
        .fold(0u32, |a, b| (a << 8) | u32::from(*b));
    let slot = data.get(92).copied().unwrap_or(0);
    let sk = Zeroizing::new([0x55; 32]);
    let opener = ReleaseOpener::new(call_id(), 1, sk);
    let Some(rid) = Id::new(1) else {
        return;
    };
    let r = Release {
        call_id: call_id(),
        recording_id: rid,
        group_id: B64::new(GROUP.to_vec()),
        epoch,
        leaf,
        slot,
        sender_identity: JoinIdHex::from_bytes([9; 16]).lk_identity(),
        enc: B64::new(enc),
        ct: B64::new(ct),
    };
    assert!(opener.open(&r).is_err(), "random bytes opened a release");
}

// ------------------------------------------------------------------ sequences

fn rekey_proposals(fx: &Fixture, s: &CallSession) -> Option<CallKeysMessage> {
    let e = s.mls_epoch()?;
    Some(CallKeysMessage::Proposals {
        call_id: call_id(),
        group_id: B64::new(GROUP.to_vec()),
        epoch: e,
        proposals: vec![],
        committer: s.own_leaf()?,
        reason: TransitionReason::Rekey,
        bindings: fx.install.ds.sign_table(&LeafBindingTableTbs {
            call_id: call_id().as_str().as_bytes().to_vec(),
            group_id: GROUP.to_vec(),
            epoch: e,
            entries: vec![],
        }),
    })
}

struct SeqState {
    now: u64,
    fences: Vec<FenceId>,
    ready_epochs: std::collections::BTreeSet<u64>,
    activated: std::collections::BTreeSet<u64>,
}

fn step(fx: &Fixture, s: &mut CallSession, st: &mut SeqState, input: Input, timeout: bool) {
    let installed = match &input {
        Input::Installed { fence } => Some(*fence),
        _ => None,
    };
    // An `activate` dispatch switches even before the fence returned (the server
    // decided; counted as `activate_before_fence`).
    let dispatch_activate = matches!(&input, Input::Server(CallKeysMessage::Activate { .. }));
    if timeout {
        s.handle_timeout(st.now);
    } else {
        let _ = s.handle(st.now, input);
    }
    let outs = drain(s);
    check_outputs(s, &outs);
    for o in &outs {
        match o {
            Output::Key(KeyEvent::Fence { id }) => st.fences.push(*id),
            Output::Send(_, KeyRequest::Ready(_, b)) => {
                // I3: `ready` only right after the bridge acknowledged the latest fence.
                assert!(
                    installed.is_some() && installed == st.fences.last().copied(),
                    "I3: ready without its fence"
                );
                st.ready_epochs.insert(b.epoch);
            }
            Output::Key(KeyEvent::Activate { .. }) => {
                // I2: activate only after ready, by the fallback, at creation, or a re-apply.
                if let Some(e) = s.active_epoch() {
                    assert!(
                        e == 0
                            || timeout
                            || dispatch_activate
                            || st.ready_epochs.contains(&e)
                            || st.activated.contains(&e),
                        "I2: activate for epoch {e} without ready"
                    );
                    st.activated.insert(e);
                }
            }
            Output::Send(id, KeyRequest::Commit(_, b)) => {
                // Answer and broadcast the own commit (one member: always the committer).
                let _ = s.handle(
                    st.now,
                    Input::Reply(
                        *id,
                        Ok(KeyReply::CommitAccepted(
                            sew_proto::e2ee::KeyCommitAccepted {
                                accepted: true,
                                epoch: b.epoch + 1,
                                slot: Slot::of(0, b.epoch + 1).get(),
                            },
                        )),
                    ),
                );
                let t = fx.install.ds.sign_table(&LeafBindingTableTbs {
                    call_id: call_id().as_str().as_bytes().to_vec(),
                    group_id: GROUP.to_vec(),
                    epoch: b.epoch + 1,
                    entries: vec![],
                });
                let _ = s.handle(
                    st.now,
                    Input::Server(CallKeysMessage::Commit {
                        call_id: call_id(),
                        group_id: B64::new(GROUP.to_vec()),
                        epoch_from: b.epoch,
                        commit: b.commit.clone(),
                        slot: Slot::of(0, b.epoch + 1).get(),
                        bindings: t,
                    }),
                );
                let more = drain(s);
                check_outputs(s, &more);
                for m in &more {
                    if let Output::Key(KeyEvent::Fence { id }) = m {
                        st.fences.push(*id);
                    }
                }
            }
            _ => {}
        }
    }
}

/// `e2ee_session_sequence`: each input byte is one step on a creator session (rekey cycles,
/// fences, activates, timeouts, re-identifies, recording and consent changes, sender-slot
/// observations, `stale_slot`, removal, garbage commits, SDK signals); I1–I5 hold after each.
pub fn e2ee_session_sequence(data: &[u8]) {
    with_fixture(|fx| {
        let Some(mut s) = session(fx, true) else {
            return;
        };
        drain(&mut s);
        let mut st = SeqState {
            now: T0,
            fences: Vec::new(),
            ready_epochs: std::collections::BTreeSet::new(),
            activated: [0].into_iter().collect(),
        };
        let mut it = data.iter().copied();
        while let Some(op) = it.next() {
            let arg = it.next().unwrap_or(0);
            match op % 14 {
                0 | 1 => {
                    if let Some(p) = rekey_proposals(fx, &s) {
                        step(fx, &mut s, &mut st, Input::Server(p), false);
                    }
                    if op % 14 == 0
                        && let Some(f) = st.fences.last().copied()
                    {
                        step(fx, &mut s, &mut st, Input::Installed { fence: f }, false);
                    }
                }
                2 => {
                    let e = s.mls_epoch().unwrap_or(0);
                    let m = CallKeysMessage::Activate {
                        call_id: call_id(),
                        group_id: B64::new(GROUP.to_vec()),
                        epoch: e,
                        slot: Slot::of(0, e).get(),
                    };
                    step(fx, &mut s, &mut st, Input::Server(m), false);
                }
                3 => {
                    let m = CallKeysMessage::Activate {
                        call_id: call_id(),
                        group_id: B64::new(GROUP.to_vec()),
                        epoch: u64::from(arg),
                        slot: arg % 70,
                    };
                    step(fx, &mut s, &mut st, Input::Server(m), false);
                }
                4 => {
                    let f = st
                        .fences
                        .get(usize::from(arg) % st.fences.len().max(1))
                        .copied()
                        .unwrap_or(FenceId(u64::from(arg)));
                    step(fx, &mut s, &mut st, Input::Installed { fence: f }, false);
                }
                5 => step(
                    fx,
                    &mut s,
                    &mut st,
                    Input::GatewayReidentified {
                        gateway_session: format!("gw-{arg}"),
                    },
                    false,
                ),
                6 => step(
                    fx,
                    &mut s,
                    &mut st,
                    Input::Recording {
                        recording_id: if arg & 1 == 1 { Id::new(1) } else { None },
                        my_consent: if arg & 2 == 2 {
                            Consent::Granted
                        } else {
                            Consent::Denied
                        },
                    },
                    false,
                ),
                7 => {
                    st.now += u64::from(arg) * 100;
                    step(fx, &mut s, &mut st, Input::GatewayResumed, true);
                }
                8 => {
                    let observed = Slot::new(arg % 64).unwrap_or(Slot::of(0, 0));
                    step(
                        fx,
                        &mut s,
                        &mut st,
                        Input::SenderSlotObserved { observed },
                        false,
                    );
                }
                9 => step(
                    fx,
                    &mut s,
                    &mut st,
                    Input::Server(CallKeysMessage::Resync {
                        call_id: call_id(),
                        join_id: JoinIdHex::from_bytes([1; 16]),
                        reason: KeyResyncReason::StaleSlot,
                        expected_slot: Some(arg % 64),
                    }),
                    false,
                ),
                10 => step(
                    fx,
                    &mut s,
                    &mut st,
                    Input::Server(CallKeysMessage::Removed {
                        call_id: call_id(),
                        group_id: B64::new(GROUP.to_vec()),
                        join_id: JoinIdHex::from_bytes([1; 16]),
                        reason: sew_proto::e2ee::RemoveReason::RemovedByModerator,
                    }),
                    false,
                ),
                11 => {
                    let rest: Vec<u8> = it.by_ref().take(usize::from(arg)).collect();
                    let e = s.mls_epoch().unwrap_or(0);
                    let t = fx.install.ds.sign_table(&LeafBindingTableTbs {
                        call_id: call_id().as_str().as_bytes().to_vec(),
                        group_id: GROUP.to_vec(),
                        epoch: e + 1,
                        entries: vec![],
                    });
                    let m = CallKeysMessage::Commit {
                        call_id: call_id(),
                        group_id: B64::new(GROUP.to_vec()),
                        epoch_from: e,
                        commit: B64::new(rest),
                        slot: Slot::of(0, e + 1).get(),
                        bindings: t,
                    };
                    step(fx, &mut s, &mut st, Input::Server(m), false);
                }
                12 => {
                    let id = JoinIdHex::from_bytes([arg; 16]).lk_identity();
                    let since = st.now;
                    step(
                        fx,
                        &mut s,
                        &mut st,
                        Input::SdkMissingKey {
                            identity: id,
                            since,
                        },
                        false,
                    );
                }
                _ => {
                    let id = JoinIdHex::from_bytes([arg; 16]).lk_identity();
                    step(
                        fx,
                        &mut s,
                        &mut st,
                        Input::MediaParticipants(vec![id]),
                        false,
                    );
                }
            }
        }
    });
}

/// `e2ee_bridge_inputs` (the "frame-key bridge inputs" target): `KeyEvent` sequences into
/// `VirtualSdk::{Js, Rust}` and `KeyRing`. Invariants: the sender switches only on `Activate`
/// (or, in JS, a local install); a local `Retire` never touches the JS sender; the ring's
/// local slot is the last activation; no slot outside the ring.
pub fn e2ee_bridge_inputs(data: &[u8]) {
    let local = JoinIdHex::from_bytes([0xee; 16]).lk_identity();
    let ids: Vec<LkIdentity> = (0..4u8)
        .map(|b| JoinIdHex::from_bytes([b; 16]).lk_identity())
        .collect();
    let mut js = VirtualSdk::new(Flavour::Js, local.clone());
    let mut rust = VirtualSdk::new(Flavour::Rust, local.clone());
    let mut ring = KeyRing::new();
    let mut last_activate: Option<Slot> = None;
    for c in data.chunks(3) {
        let (op, a, b) = (
            c.first().copied().unwrap_or(0),
            c.get(1).copied().unwrap_or(0),
            c.get(2).copied().unwrap_or(0),
        );
        let slot = Slot::new(b % 64).unwrap_or(Slot::of(0, 0));
        let who = if a % 5 == 4 {
            local.clone()
        } else {
            ids[usize::from(a % 4)].clone()
        };
        let ikm = FrameIkm::from_bytes([a ^ b ^ op; 32]);
        let ev = match op % 6 {
            0 if who != local => KeyEvent::Install {
                identity: who,
                slot,
                ikm,
            },
            0 | 1 => KeyEvent::Activate {
                identity: local.clone(),
                slot,
                ikm,
            },
            2 => KeyEvent::Retire {
                identity: who,
                slot,
                filler: FrameIkm::from_bytes([0xa5 ^ a; 32]),
            },
            3 => KeyEvent::Forget { identity: who },
            4 => KeyEvent::Fence {
                id: FenceId(u64::from(a)),
            },
            _ => KeyEvent::Clear,
        };
        let (js_before, rust_before) = (js.send_slot(), rust.send_slot());
        let is_activate = matches!(ev, KeyEvent::Activate { .. });
        let local_retire = matches!(&ev, KeyEvent::Retire { identity, .. } if *identity == local);
        ring.apply(&ev);
        let _ = js.apply(ev.duplicate());
        let _ = rust.apply(ev);
        if is_activate {
            last_activate = Some(slot);
            assert_eq!(js.send_slot(), Some(slot), "JS: Activate switches");
            assert_eq!(rust.send_slot(), Some(slot), "Rust: Activate switches");
        } else {
            assert_eq!(
                rust.send_slot(),
                rust_before,
                "Rust: only Activate switches"
            );
            if local_retire {
                assert_eq!(
                    js.send_slot(),
                    js_before,
                    "a local retire never moves the sender"
                );
            }
        }
        assert_eq!(ring.local_slot(), last_activate, "the ring's local slot");
        if let Some(s) = js.send_slot() {
            assert!(s.get() < 64);
        }
    }
}

/// Seeds made from the fixture (`(name, bytes)`), for a fuzz target's starting corpus.
pub fn seeds(target: &str) -> Vec<(String, Vec<u8>)> {
    with_fixture(|fx| {
        let mut out: Vec<(String, Vec<u8>)> = vec![("empty".into(), Vec::new())];
        let table = fx.install.ds.sign_table(&LeafBindingTableTbs {
            call_id: call_id().as_str().as_bytes().to_vec(),
            group_id: GROUP.to_vec(),
            epoch: 3,
            entries: (0..4u32)
                .map(|l| sew_proto::e2ee::tls::LeafBindingEntry {
                    key: sew_proto::e2ee::tls::BindingKey::Leaf(l),
                    join_id: [l as u8 + 1; 16],
                })
                .collect(),
        });
        match target {
            "e2ee_server_msg_json" => {
                let msgs = [
                    CallKeysMessage::Activate {
                        call_id: call_id(),
                        group_id: B64::new(GROUP.to_vec()),
                        epoch: 1,
                        slot: 1,
                    },
                    CallKeysMessage::Resync {
                        call_id: call_id(),
                        join_id: JoinIdHex::from_bytes([1; 16]),
                        reason: KeyResyncReason::StaleSlot,
                        expected_slot: Some(0),
                    },
                    CallKeysMessage::Init(init(&fx.install, true)),
                ];
                for (i, m) in msgs.iter().enumerate() {
                    if let Ok(b) = serde_json::to_vec(m) {
                        out.push((format!("msg_{i}"), b));
                    }
                }
                out.push(("unknown_kind".into(), br#"{"kind":"future"}"#.to_vec()));
            }
            "e2ee_leaf_binding" => {
                if let Ok(b) = serde_json::to_vec(&table) {
                    out.push(("table_json".into(), b));
                }
                if let Ok(tbs) = table.tbs()
                    && let Ok(b) = tbs.to_bytes()
                {
                    out.push(("table_tbs".into(), b));
                }
            }
            "e2ee_call_identity_tls" | "e2ee_mls_message_in" => {
                let kp = fx.device.prepare(scope(), T0);
                if target == "e2ee_mls_message_in" {
                    out.push(("key_package".into(), kp.key_package_bytes().to_vec()));
                } else {
                    let ident = SewCallIdentity {
                        version: 2,
                        kind: 1,
                        scope: scope().to_bytes(),
                        device_cert: fx.device.stored.cert.clone(),
                        device_sig: vec![0x11; 64],
                    };
                    if let Ok(b) = ident.to_bytes() {
                        out.push(("identity".into(), b));
                    }
                }
            }
            "e2ee_device_cert_tls" => out.push(("cert".into(), fx.device.stored.cert.clone())),
            "e2ee_recorder_cert_tls" => {
                let c = fx.install.ca.sign_recorder(RecorderCertTbs {
                    version: 1,
                    install_id: crate::testing::fixtures::INSTALL_ID
                        .to_string()
                        .into_bytes(),
                    call_id: call_id().as_str().as_bytes().to_vec(),
                    recording_id: 1,
                    hpke_pub: [0x22; 32],
                    not_before: T0 / 1000 - 60,
                    not_after: T0 / 1000 + 3_600,
                });
                if let Ok(b) = c.to_bytes() {
                    out.push(("recorder_cert".into(), b));
                }
            }
            "e2ee_release_open" => out.push(("zeros".into(), vec![0; 93])),
            "e2ee_session_sequence" => {
                out.push(("rekey_cycles".into(), vec![0, 0, 2, 0, 0, 0, 2, 0, 7, 120]));
                out.push(("stale_slot".into(), vec![0, 0, 2, 0, 9, 1, 8, 5, 8, 5]));
                out.push(("recording".into(), vec![6, 3, 0, 0, 2, 0, 6, 1, 1, 0]));
                out.push((
                    "timeouts".into(),
                    vec![1, 0, 7, 30, 7, 120, 12, 9, 13, 4, 7, 255],
                ));
                out.push((
                    "garbage".into(),
                    vec![11, 4, 1, 2, 3, 4, 3, 9, 4, 2, 5, 1, 10, 0],
                ));
            }
            "e2ee_bridge_inputs" => {
                out.push((
                    "cycle".into(),
                    vec![0, 0, 1, 1, 0, 1, 0, 1, 2, 2, 0, 0, 4, 1, 0],
                ));
                out.push((
                    "local".into(),
                    vec![0, 4, 3, 2, 4, 1, 1, 0, 5, 3, 0, 0, 5, 0, 0],
                ));
            }
            _ => {}
        }
        out
    })
}

/// Runs `target` on one input.
pub fn run(target: &str, data: &[u8]) {
    match target {
        "e2ee_server_msg_json" => e2ee_server_msg_json(data),
        "e2ee_leaf_binding" => e2ee_leaf_binding(data),
        "e2ee_call_identity_tls" => e2ee_call_identity_tls(data),
        "e2ee_device_cert_tls" => e2ee_device_cert_tls(data),
        "e2ee_recorder_cert_tls" => e2ee_recorder_cert_tls(data),
        "e2ee_mls_message_in" => e2ee_mls_message_in(data),
        "e2ee_release_open" => e2ee_release_open(data),
        "e2ee_session_sequence" => e2ee_session_sequence(data),
        "e2ee_bridge_inputs" => e2ee_bridge_inputs(data),
        other => panic!("unknown fuzz target {other}"),
    }
}
