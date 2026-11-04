//! Trust anchors: the install's CA and DS keys, the `external_sender` bytes and the CRL
//! (READY `call_keys`).
//!
//! **Pinning is TOFU per install.** The first sight of the CA key, the DS key and the
//! `external_sender` bytes is stored in the `DeviceStore`. A later READY or `init` with
//! different bytes is a hard error: `E2eeStatus::ServerIdentityChanged`, no joins, and a `report`
//! `ca_changed` / `external_sender_mismatch` report for the admins' alarm. Only clearing the
//! device store (a new enrollment) accepts the new keys.

use std::collections::BTreeSet;
use std::fmt;

use openmls::prelude::{BasicCredential, Credential, ExternalSender, SignaturePublicKey};
use sew_proto::e2ee::{CallKeysParams, ReadyCallKeys, VerificationCode};
use tls_codec::Serialize as _;

use crate::config::{Params, caps};
use crate::error::TrustError;
use crate::store::{DeviceStore, Pins};

/// The `ExternalSender` TLS bytes for a DS key and install (`"sew-call-ds:<install_id>"`
/// Basic credential).
pub fn external_sender_bytes(ds_public: &[u8; 32], install_id: u64) -> Vec<u8> {
    let credential: Credential =
        BasicCredential::new(format!("sew-call-ds:{install_id}").into_bytes()).into();
    ExternalSender::new(SignaturePublicKey::from(ds_public.to_vec()), credential)
        .tls_serialize_detached()
        .unwrap_or_default()
}

/// What a client trusts in one install.
#[derive(Clone)]
pub struct TrustAnchors {
    install_id: u64,
    ca_public: [u8; 32],
    ca_kid: String,
    ds_public: [u8; 32],
    external_sender: Vec<u8>,
    crl: BTreeSet<u64>,
    crl_partial: bool,
    verification_code: VerificationCode,
    params_wire: CallKeysParams,
}

impl fmt::Debug for TrustAnchors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrustAnchors")
            .field("install_id", &self.install_id)
            .field("ca_kid", &self.ca_kid)
            .field("crl", &self.crl.len())
            .finish_non_exhaustive()
    }
}

fn key32(b: &[u8], what: &'static str) -> Result<[u8; 32], TrustError> {
    <[u8; 32]>::try_from(b).map_err(|_| TrustError::Malformed(what))
}

impl TrustAnchors {
    /// Reads READY's `call_keys`, checks it against the install's pins (TOFU), and stores the
    /// pins on first sight.
    pub fn from_ready(
        r: &ReadyCallKeys,
        install_id: u64,
        pins: &mut impl DeviceStore,
    ) -> Result<TrustAnchors, TrustError> {
        let ca_public = key32(r.ca.public_key.as_bytes(), "ca.public_key")?;
        let ds_public = key32(r.ds_public_key.as_bytes(), "ds_public_key")?;
        if r.external_sender.len() > caps::EXTERNAL_SENDER {
            return Err(TrustError::Malformed("external_sender"));
        }
        let external_sender = r.external_sender.as_bytes().to_vec();
        if external_sender != external_sender_bytes(&ds_public, install_id) {
            return Err(TrustError::Malformed("external_sender"));
        }
        match pins.load_pins(install_id) {
            Some(p) => {
                if p.ca_public != ca_public {
                    return Err(TrustError::ServerIdentityChanged { what: "ca" });
                }
                if p.ds_public != ds_public || p.external_sender != external_sender {
                    return Err(TrustError::ServerIdentityChanged {
                        what: "external_sender",
                    });
                }
            }
            None => pins
                .save_pins(&Pins {
                    install_id,
                    ca_public,
                    ds_public,
                    external_sender: external_sender.clone(),
                })
                .map_err(TrustError::Store)?,
        }
        Ok(TrustAnchors {
            install_id,
            ca_public,
            ca_kid: r.ca.kid.clone(),
            ds_public,
            external_sender,
            crl: r.crl.iter().map(|s| s.get()).collect(),
            crl_partial: r.crl_partial,
            verification_code: r.verification_code,
            params_wire: r.params.clone(),
        })
    }

    /// A fresh READY on the same install: the pins must not change; the CRL is replaced and
    /// the params and policy updated.
    pub fn refresh(&mut self, r: &ReadyCallKeys) -> Result<(), TrustError> {
        let ca_public = key32(r.ca.public_key.as_bytes(), "ca.public_key")?;
        let ds_public = key32(r.ds_public_key.as_bytes(), "ds_public_key")?;
        if ca_public != self.ca_public {
            return Err(TrustError::ServerIdentityChanged { what: "ca" });
        }
        if ds_public != self.ds_public || r.external_sender.as_bytes() != self.external_sender {
            return Err(TrustError::ServerIdentityChanged {
                what: "external_sender",
            });
        }
        self.crl = r.crl.iter().map(|s| s.get()).collect();
        self.crl_partial = r.crl_partial;
        self.verification_code = r.verification_code;
        self.params_wire = r.params.clone();
        Ok(())
    }

    /// Adds revoked serials (a `revoked` dispatch, or a CRL page).
    pub fn revoke(&mut self, serials: &[u64]) {
        self.crl.extend(serials.iter().copied());
    }

    /// The install.
    pub fn install_id(&self) -> u64 {
        self.install_id
    }

    /// The pinned CA key.
    pub fn ca_public(&self) -> &[u8; 32] {
        &self.ca_public
    }

    /// The pinned DS key.
    pub fn ds_public(&self) -> &[u8; 32] {
        &self.ds_public
    }

    /// The pinned `external_sender` bytes.
    pub fn external_sender(&self) -> &[u8] {
        &self.external_sender
    }

    /// Revoked serials.
    pub fn crl(&self) -> &BTreeSet<u64> {
        &self.crl
    }

    /// Whether READY said more serials exist (page them with the CRL route).
    pub fn crl_partial(&self) -> bool {
        self.crl_partial
    }

    /// Policy `calls.verification_code`.
    pub fn verification_code(&self) -> VerificationCode {
        self.verification_code
    }

    /// The server's params, checked (the only source of client timings).
    pub fn params(&self) -> Result<Params, crate::error::SessionError> {
        Params::from_wire(&self.params_wire)
    }

    /// The server's params as sent.
    pub fn params_wire(&self) -> &CallKeysParams {
        &self.params_wire
    }
}
