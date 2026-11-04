//! `Caps`: what a client's SDK can do.
//!
//! `Caps` has **no public constructor**: only a bridge builds it (`JsBridge::caps()`,
//! `LiveKitRustSink::caps()`, the test `VirtualSdk`), so an app cannot claim a capability its
//! SDK lacks by mistake. The in-tree bridges in other crates use the hidden
//! [`crate::__bridge::caps`]; it is not part of the supported API (test
//! `caps_only_from_bridge`).

use sew_proto::e2ee::E2eeCaps;

use crate::config::{CIPHERSUITE, KDF, LIB};
use crate::ids::KEY_RING_SIZE;

/// The SDK capabilities a join declares (the voice join's `e2ee.caps`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caps {
    sender_key_index: bool,
    sdk: String,
    sdk_version: String,
    browser: Option<String>,
}

impl Caps {
    pub(crate) fn from_bridge(
        sender_key_index: bool,
        sdk: &str,
        sdk_version: &str,
        browser: Option<&str>,
    ) -> Caps {
        Caps {
            sender_key_index,
            sdk: sdk.to_owned(),
            sdk_version: sdk_version.to_owned(),
            browser: browser.map(str::to_owned),
        }
    }

    /// Whether the SDK can move its sender to a new key slot.
    pub fn sender_key_index(&self) -> bool {
        self.sender_key_index
    }

    /// The SDK family (`js`, `rust`, `virtual`).
    pub fn sdk(&self) -> &str {
        &self.sdk
    }

    /// The SDK version, as its `ClientInfo` reports it.
    pub fn sdk_version(&self) -> &str {
        &self.sdk_version
    }

    /// The browser (`chrome`, `firefox`, `safari`, `other`), web only.
    pub fn browser(&self) -> Option<&str> {
        self.browser.as_deref()
    }

    /// The wire object: `{sender_key_index, kdf: "hkdf", key_ring_size: 64, sdk,
    /// sdk_version, browser?, lib: "sew-e2ee/<ver>", ciphersuites: [1]}`.
    pub fn to_wire(&self) -> E2eeCaps {
        E2eeCaps {
            sender_key_index: self.sender_key_index,
            kdf: KDF.to_owned(),
            key_ring_size: u32::from(KEY_RING_SIZE),
            sdk: self.sdk.clone(),
            sdk_version: self.sdk_version.clone(),
            browser: self.browser.clone(),
            lib: Some(LIB.to_owned()),
            ciphersuites: Some(vec![CIPHERSUITE]),
        }
    }
}
