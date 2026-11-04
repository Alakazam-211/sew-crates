//! `sew-e2ee`: the reference client library for end-to-end encrypted Sew calls.
//!
//! Every Sew call is end-to-end encrypted. The keys exist only on participant devices,
//! agreed with MLS (RFC 9420, openmls 0.9.0), and are fed into the LiveKit SDKs' own frame
//! encryption. This crate is that protocol, written once:
//!
//! - **Identity** ([`identity`], [`trust`], [`store`]): the P-256 device key interface
//!   ([`DeviceSigner`]), device certificates from the server's call CA, the call credential
//!   (`SewCallIdentity` v2, scope `call:<channel_id>`), the DS-signed `LeafBinding` table, TOFU
//!   pins and the CRL.
//! - **Joining** ([`E2eeClient`], [`PendingKp`], [`PreparedKp`]): key packages made offline,
//!   uploaded for a single-use `key_package_ref`, claimed by the voice join or the key join.
//! - **The call** ([`CallSession`]): a sans-IO state machine — per-sender keys from the MLS
//!   exporter by leaf index, slots `(slot_base + e) mod 64`, the two-phase install → ready →
//!   activate switch, retirement after 10 s, recovery without user action, the recorder release.
//! - **Bridges** ([`bridge`]): the [`KeyEvent`] contract every SDK bridge applies, the
//!   [`KeyRing`] shadow, [`Caps`], and the test [`bridge::virtual_sdk::VirtualSdk`].
//! - **The recorder side** ([`recorder`]): [`ReleaseOpener`], [`RecorderKeyPair`],
//!   [`identity::verify_table`] for the recorder; [`open_recording_chunk`] for the admin download.
//!
//! Sans-IO: no IO, no clock, no threads, no `Send` requirement. Sew's test driver
//! (not published) wires a session to Sew's Rust client; the WASM build
//! (`@sew/e2ee`, not published here) wraps it for browsers.

pub mod bridge;
pub mod chunk;
pub mod code;
pub mod config;
pub mod error;
pub mod identity;
pub mod ids;
pub mod keypackage;
pub mod keys;
pub mod provider;
pub mod recorder;
pub mod roster;
pub mod session;
pub mod store;
pub mod trust;
pub mod wire;

#[cfg(feature = "testing")]
pub mod fuzzing;
#[cfg(feature = "testing")]
pub mod testing;

pub use bridge::{Caps, KeyEvent, KeyRing, KeySink, SinkAck, SinkError};
pub use chunk::{ChunkLayout, open_recording_chunk, recording_chunk_layout};
pub use code::SecurityCode;
pub use config::{E2eeConfig, Params};
pub use error::{
    BindingError, ChunkError, DecodeError, IdentityError, JoinError, ReleaseError, SessionError,
    TrustError,
};
pub use identity::{
    DeviceIdentity, DeviceSigner, EnrollAction, EnrollDraft, Enrollment, SignRequest,
    SoftwareSigner, VerifiedTable, verify_table,
};
pub use ids::{
    Consent, Epoch, FenceId, Id, JoinId, LkIdentity, MemberId, Millis, RecordingId, RequestId,
    RoomName, SignId, Slot, UnixMs,
};
pub use keypackage::{E2eeClient, PendingKp, PreparedKp};
pub use keys::{FrameIkm, ikm_context, lk_frame_key};
pub use recorder::{RecorderKeyPair, ReleaseOpener, Released};
pub use roster::{MediaLabel, RosterEntry, RosterState};
pub use session::{
    CallSession, Counters, E2eeStatus, EngineAttack, Input, LeftReason, Output, SessionState,
    Transition, UndecryptReason, UnknownCallBuffer,
};
pub use store::{DeviceStore, MemoryDeviceStore, Pins, StoredDevice};
pub use trust::TrustAnchors;
pub use wire::{ApiFailure, KeyReply, KeyRequest};

/// The call scope: always `call:<channel_id>` (voice channels and DM calls alike).
/// It has one constructor and no empty form.
pub type Scope = sew_proto::e2ee::CallScope;

/// For Sew's own bridges in other crates (the LiveKit Rust SDK and WASM bridges, not published)
/// only: builds [`Caps`]. Not part of the supported API (`caps_only_from_bridge`). An app that
/// calls it declares a capability its SDK may lack, which the server then refuses only by
/// observation.
#[doc(hidden)]
pub mod __bridge {
    use crate::bridge::Caps;

    /// The caps a bridge reports for its SDK.
    pub fn caps(
        sender_key_index: bool,
        sdk: &str,
        sdk_version: &str,
        browser: Option<&str>,
    ) -> Caps {
        Caps::from_bridge(sender_key_index, sdk, sdk_version, browser)
    }
}
