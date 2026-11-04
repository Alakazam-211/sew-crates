//! The engine's configuration and its TLS trust.
//!
//! Production trust is the platform's (`rustls-platform-verifier`: the macOS and Windows
//! verifiers, the system roots on Linux), on rustls with ring. Signalling, the
//! TURN-TLS connection and Sew's client API all use it. A test-only option (feature
//! `testing`) adds a run CA **on top of** the platform roots; there is no way to trust only a
//! test CA.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rustls::ClientConfig as TlsConfig;
use rustls_platform_verifier::Verifier;

use crate::device::{DeviceKeyStore, StoreChoice};

/// The engine's configuration (one per app).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ClientConfig {
    /// The host's app identifier (with feature `keyring-store`, `sew-voice.<app id>` is the OS
    /// credential store's service).
    pub app_id: String,
    /// The host's data directory (the device store's public data).
    pub data_dir: PathBuf,
    /// Where [`crate::CallClient::with_mls`] keeps the device key and its public record.
    /// Default [`StoreChoice::Auto`]: a 0600 file, warned as `key_storage_file` (with feature
    /// `keyring-store`, the OS credential store first). A host passes its own store with
    /// [`ClientConfig::with_device_key_store`].
    pub device_store: StoreChoice,
    /// The reconnect window (Sew's media server's `reconnect_window_ms`, 20 s): resume and full
    /// reconnects inside it; after it, a re-join through the provider.
    pub reconnect_window: Duration,
    /// How long the first connect may take before the call ends with `network`.
    pub connect_timeout: Duration,
    /// Test-only overrides (feature `testing`).
    #[cfg(feature = "testing")]
    pub testing: TestingOverrides,
}

impl ClientConfig {
    /// A config with the defaults.
    pub fn new(app_id: &str, data_dir: PathBuf) -> ClientConfig {
        ClientConfig {
            app_id: app_id.to_owned(),
            data_dir,
            device_store: StoreChoice::Auto,
            reconnect_window: Duration::from_secs(20),
            connect_timeout: Duration::from_secs(20),
            #[cfg(feature = "testing")]
            testing: TestingOverrides::default(),
        }
    }

    /// Keeps the device's private key and public record in the host's own store (e.g. K2's
    /// keyring or vault) instead of the built-in file ([`StoreChoice::Host`]).
    pub fn with_device_key_store(mut self, store: Arc<dyn DeviceKeyStore>) -> ClientConfig {
        self.device_store = StoreChoice::Host(store);
        self
    }
}

/// TEST ONLY (feature `testing`): what the loopback lanes change.
#[cfg(feature = "testing")]
#[derive(Clone, Debug, Default)]
pub struct TestingOverrides {
    /// CA certificates (DER) trusted **in addition to** the platform roots.
    pub extra_roots: Vec<Vec<u8>>,
    /// Fixed name resolution (`turn.sew.test` → 127.0.0.1), as Chrome's
    /// `--host-resolver-rules`.
    pub resolve: Vec<(String, IpAddr)>,
}

/// The TLS client config every connection of the engine uses.
pub(crate) fn tls_config(cfg: &ClientConfig) -> Result<Arc<TlsConfig>, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    #[cfg(feature = "testing")]
    let verifier = if cfg.testing.extra_roots.is_empty() {
        Verifier::new(provider.clone())?
    } else {
        let extra = cfg
            .testing
            .extra_roots
            .iter()
            .map(|d| rustls::pki_types::CertificateDer::from(d.clone()));
        Verifier::new_with_extra_roots(extra, provider.clone())?
    };
    #[cfg(not(feature = "testing"))]
    let verifier = {
        let _ = cfg;
        Verifier::new(provider.clone())?
    };
    let tls = TlsConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(tls))
}

/// The platform's trust alone (the key session's default when the engine gives it none).
pub(crate) fn platform_tls() -> Result<Arc<TlsConfig>, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Verifier::new(provider.clone())?;
    let tls = TlsConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(tls))
}

impl ClientConfig {
    /// The TLS client config the engine uses for every connection (the platform's trust; in a
    /// `testing` build, plus the run CA). Hosts that set up the keys themselves
    /// ([`crate::e2ee::MlsKeys::prepare_session_join`]) pass it on.
    pub fn tls(&self) -> Result<Arc<TlsConfig>, rustls::Error> {
        tls_config(self)
    }
}

/// Resolves `host:port` (the testing overrides first).
pub(crate) async fn resolve(
    cfg: &ClientConfig,
    host: &str,
    port: u16,
) -> std::io::Result<std::net::SocketAddr> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(std::net::SocketAddr::new(ip, port));
    }
    #[cfg(feature = "testing")]
    if let Some((_, ip)) = cfg.testing.resolve.iter().find(|(n, _)| n == host) {
        return Ok(std::net::SocketAddr::new(*ip, port));
    }
    #[cfg(not(feature = "testing"))]
    let _ = cfg;
    tokio::net::lookup_host((host, port))
        .await?
        .next()
        .ok_or_else(|| std::io::Error::other(format!("{host} does not resolve")))
}

/// Whether `host` names this machine (plain `ws://` and `http://` are allowed only there).
pub(crate) fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_platform_verifier_builds() {
        let cfg = ClientConfig::new("test.app", std::env::temp_dir());
        assert!(tls_config(&cfg).is_ok());
    }

    #[test]
    fn loopback_hosts() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("localhost"));
        assert!(!is_loopback_host("voice.example"));
        assert!(!is_loopback_host("10.0.0.1"));
    }
}
