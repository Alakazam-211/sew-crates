//! Configuration, labels and limits.
//!
//! The "server" timings come only from the server's `params` (READY `call_keys` and every
//! `CallKeysInit`): [`Params::from_wire`] checks each against its allowed range and refuses the
//! session otherwise. The defaults exist so `Sim` and unit tests run without a server. The
//! "local" values are [`E2eeConfig`].

use sew_proto::e2ee::CallKeysParams;
use sew_proto::e2ee::tls::{EXPORTER_LABEL, RELEASE_INFO_LABEL};

use crate::error::SessionError;
use crate::ids::KEY_RING_SIZE;

/// The MLS exporter label of per-sender frame keys (RFC 9420 §17.8: a unique label).
pub const LABEL_FRAME_KEY: &str = EXPORTER_LABEL;
/// The HPKE `info` label of a recorder release.
pub const LABEL_RELEASE_INFO: &str = RELEASE_INFO_LABEL;
/// The only ciphersuite (`MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519`).
pub const CIPHERSUITE: u16 = 0x0001;
/// The SDK key-derivation family every bridge uses.
pub const KDF: &str = "hkdf";
/// The SDK's HKDF salt for frame keys (`"LKFrameEncryptionKey"`).
pub const LK_SALT: &[u8] = b"LKFrameEncryptionKey";
/// `lib` in `caps`.
pub const LIB: &str = concat!("sew-e2ee/", env!("CARGO_PKG_VERSION"));

/// Decode caps, checked before any parse (equal to or above the server's caps).
pub mod caps {
    /// A KeyPackage.
    pub const KEY_PACKAGE: usize = 16 * 1024;
    /// One proposal message.
    pub const PROPOSAL: usize = 16 * 1024;
    /// Proposals per `proposals` dispatch.
    pub const PROPOSALS_PER_MESSAGE: usize = 64;
    /// A commit.
    pub const COMMIT: usize = 64 * 1024;
    /// A GroupInfo.
    pub const GROUP_INFO: usize = 32 * 1024;
    /// A Welcome.
    pub const WELCOME: usize = 128 * 1024;
    /// A ratchet tree.
    pub const RATCHET_TREE: usize = 256 * 1024;
    /// A recorder certificate.
    pub const RECORDER_CERT: usize = 1024;
    /// A release `enc`.
    pub const RELEASE_ENC: usize = 32;
    /// A release `ct` (32-byte IKM + 16-byte tag).
    pub const RELEASE_CT: usize = 48;
    /// A `LeafBinding` table (as JSON or TLS).
    pub const LEAF_BINDING_TABLE: usize = 64 * 1024;
    /// Entries of a `LeafBinding` table (the server caps a call at a few hundred leaves).
    pub const LEAF_BINDING_ENTRIES: usize = 1024;
    /// Any dispatch or reply body, as raw JSON.
    pub const BODY_JSON: usize = 448 * 1024;
    /// A `SewCallIdentity` (TLS).
    pub const CALL_IDENTITY: usize = 4 * 1024;
    /// A `SewDeviceCert` (TLS).
    pub const DEVICE_CERT: usize = 2 * 1024;
    /// The `external_sender` bytes.
    pub const EXTERNAL_SENDER: usize = 1024;
}

/// The local (not server-relayed) timings and limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct E2eeConfig {
    /// The missing-key check runs this long after an activation.
    pub missing_key_check_ms: u64,
    /// `Resyncing` / `Resetting` watchdog period.
    pub recovery_watchdog_ms: u64,
    /// Watchdog retries before `Failed{recovery_timeout}`.
    pub recovery_watchdog_tries: u32,
    /// A prepared KeyPackage is stale after this long (and past the server TTL).
    pub prepared_kp_max_age_s: u64,
    /// Certificate clock skew (± seconds).
    pub clock_skew_s: u64,
    /// Renew the device certificate when less than this is left.
    pub cert_renew_before_s: u64,
    /// `report` roster anomalies per leaf per hour; slot signals per hour (separate bucket).
    pub report_max_per_hour: u32,
    /// The joining window for roster anomalies (`engine_participant_without_leaf`,
    /// `leaf_without_voice_state`).
    pub roster_grace_ms: u64,
    /// `peer_undecryptable`: failures must last this long (after the grace) before a `state` check.
    pub peer_undecryptable_after_ms: u64,
    /// Unknown-call buffer: messages and bytes per call, and calls.
    pub unknown_call_max_messages: usize,
    /// Bytes per unknown call.
    pub unknown_call_max_bytes: usize,
    /// Unknown calls buffered at once.
    pub unknown_call_max_calls: usize,
    /// Commit bytes are recorded at most this long for the security code (unused bound).
    pub key_package_lifetime_back_s: u64,
    /// KeyPackage lifetime forward from now (`now + 6600`, a 2 h range with the back margin).
    pub key_package_lifetime_fwd_s: u64,
}

impl Default for E2eeConfig {
    fn default() -> E2eeConfig {
        E2eeConfig {
            missing_key_check_ms: 2_000,
            recovery_watchdog_ms: 5_000,
            recovery_watchdog_tries: 3,
            prepared_kp_max_age_s: 3_600,
            clock_skew_s: 300,
            cert_renew_before_s: 604_800,
            report_max_per_hour: 10,
            roster_grace_ms: 5_000,
            peer_undecryptable_after_ms: 2_000,
            unknown_call_max_messages: 64,
            unknown_call_max_bytes: 1024 * 1024,
            unknown_call_max_calls: 2,
            key_package_lifetime_back_s: 600,
            key_package_lifetime_fwd_s: 6_600,
        }
    }
}

/// The server's relayed client parameters, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Params {
    /// Receivers retire an old slot this long after `activate`.
    pub retire_after_ms: u64,
    /// A member that sent `ready` and hears no `activate` switches after this long.
    pub client_activate_fallback_ms: u64,
    /// Longest KeyPackage lifetime the server accepts.
    pub key_package_max_lifetime_s: u64,
    /// The unknown-call buffer window.
    pub unknown_call_buffer_ms: u64,
    /// "Securing…" shows only after this long (passed through to the UI).
    pub securing_indicator_after_ms: u64,
}

impl Default for Params {
    fn default() -> Params {
        Params {
            retire_after_ms: 10_000,
            client_activate_fallback_ms: 2_500,
            key_package_max_lifetime_s: 7_200,
            unknown_call_buffer_ms: 5_000,
            securing_indicator_after_ms: 1_500,
        }
    }
}

fn range(key: &'static str, v: u64, lo: u64, hi: u64) -> Result<u64, SessionError> {
    if (lo..=hi).contains(&v) {
        Ok(v)
    } else {
        Err(SessionError::Params { key })
    }
}

impl Params {
    /// Checks the server's `params`. A value outside its range refuses the session
    /// (`SessionError::Params{key}`); a ring size other than 64 is `SessionError::RingSize`.
    pub fn from_wire(p: &CallKeysParams) -> Result<Params, SessionError> {
        if p.key_ring_size != u32::from(KEY_RING_SIZE) {
            return Err(SessionError::RingSize {
                got: p.key_ring_size,
            });
        }
        if p.ciphersuite != CIPHERSUITE {
            return Err(SessionError::Params { key: "ciphersuite" });
        }
        if !p.ciphersuites.contains(&CIPHERSUITE) {
            return Err(SessionError::Params {
                key: "ciphersuites",
            });
        }
        if p.kdf != KDF {
            return Err(SessionError::Params { key: "kdf" });
        }
        if p.exporter_label != LABEL_FRAME_KEY {
            return Err(SessionError::Params {
                key: "exporter_label",
            });
        }
        if p.release_info_label != LABEL_RELEASE_INFO {
            return Err(SessionError::Params {
                key: "release_info_label",
            });
        }
        Ok(Params {
            retire_after_ms: range("retire_after_ms", p.retire_after_ms, 1_000, 60_000)?,
            client_activate_fallback_ms: range(
                "client_activate_fallback_ms",
                p.client_activate_fallback_ms,
                500,
                10_000,
            )?,
            key_package_max_lifetime_s: range(
                "key_package_max_lifetime_s",
                p.key_package_max_lifetime_s,
                600,
                86_400,
            )?,
            unknown_call_buffer_ms: range(
                "unknown_call_buffer_ms",
                p.unknown_call_buffer_ms,
                0,
                30_000,
            )?,
            securing_indicator_after_ms: p.securing_indicator_after_ms,
        })
    }

    /// The wire form of the defaults (for `Sim` and tests).
    pub fn default_wire() -> CallKeysParams {
        let d = Params::default();
        CallKeysParams {
            ciphersuite: CIPHERSUITE,
            ciphersuites: vec![CIPHERSUITE],
            key_ring_size: u32::from(KEY_RING_SIZE),
            kdf: KDF.to_owned(),
            exporter_label: LABEL_FRAME_KEY.to_owned(),
            release_info_label: LABEL_RELEASE_INFO.to_owned(),
            retire_after_ms: d.retire_after_ms,
            client_activate_fallback_ms: d.client_activate_fallback_ms,
            key_package_max_lifetime_s: d.key_package_max_lifetime_s,
            unknown_call_buffer_ms: d.unknown_call_buffer_ms,
            securing_indicator_after_ms: d.securing_indicator_after_ms,
        }
    }
}
