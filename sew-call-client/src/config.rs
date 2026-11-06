//! The engine's configuration and its TLS trust.
//!
//! Production trust is the platform's (`rustls-platform-verifier`: the macOS and Windows
//! verifiers, the system roots on Linux), on one rustls crypto provider: the host's
//! ([`ClientConfig::with_crypto_provider`]), else the process default
//! (`CryptoProvider::get_default`), else the built-in ring provider (feature
//! `builtin-ring-provider`, on by default; without it the engine refuses with
//! `no_crypto_provider`). Signalling, the TURN-TLS connection and Sew's
//! client API all use it. A test-only option (feature
//! `testing`) adds a run CA **on top of** the platform roots; there is no way to trust only a
//! test CA.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rustls::ClientConfig as TlsConfig;
use rustls::crypto::CryptoProvider;
use rustls_platform_verifier::Verifier;

use crate::device::{DeviceKeyStore, DevicePlatform, HardwareDeviceKey, StoreChoice};

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
    /// The platform the device enrols as. Default from `target_os` (iOS, Android, otherwise
    /// desktop); a host sets it with [`ClientConfig::with_device_platform`].
    pub device_platform: DevicePlatform,
    /// The reconnect window (Sew's media server's `reconnect_window_ms`, 20 s): resume and full
    /// reconnects inside it; after it, a re-join through the provider.
    pub reconnect_window: Duration,
    /// How long the first connect may take before the call ends with `network`.
    pub connect_timeout: Duration,
    /// The rustls crypto provider of every TLS connection (signalling, TURN-TLS, the client
    /// API and the key routes). `None` (the default): the process default
    /// (`CryptoProvider::get_default`, e.g. one the host installed with `install_default`),
    /// else the built-in ring provider (feature `builtin-ring-provider`, on by default; without
    /// it, TLS setup fails with `no_crypto_provider`). A host sets it with
    /// [`ClientConfig::with_crypto_provider`].
    pub crypto_provider: Option<Arc<CryptoProvider>>,
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
            device_platform: crate::device::default_platform(),
            reconnect_window: Duration::from_secs(20),
            connect_timeout: Duration::from_secs(20),
            crypto_provider: None,
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

    /// Keeps the device's private key in the host's hardware (Secure Enclave, StrongBox/TEE)
    /// through [`HardwareDeviceKey`] ([`StoreChoice::Hardware`]); the public record stays in
    /// the data directory. It replaces any other store, and the engine never falls back to a
    /// software key.
    pub fn with_hardware_device_key(mut self, key: Arc<dyn HardwareDeviceKey>) -> ClientConfig {
        self.device_store = StoreChoice::Hardware(key);
        self
    }

    /// The platform the device enrols as (instead of the `target_os` default).
    pub fn with_device_platform(mut self, platform: DevicePlatform) -> ClientConfig {
        self.device_platform = platform;
        self
    }

    /// Builds every TLS connection on `provider` (e.g. the host's own rustls provider) instead
    /// of the process default.
    pub fn with_crypto_provider(mut self, provider: Arc<CryptoProvider>) -> ClientConfig {
        self.crypto_provider = Some(provider);
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
    /// The media transport's source address (the TURN sockets and the host candidate bind
    /// to it), shared so a test can move the client to another "network" mid-call (`None`: the
    /// OS chooses, as in production).
    pub source_ip: Arc<std::sync::RwLock<Option<IpAddr>>>,
    /// The RTP sequence number the microphone's first packet carries in each new media session
    /// (each new PC; `None`: the engine's own random start in [0, 2^15)). Lanes set it near
    /// 65535 to make the sequence wrap early.
    pub first_seq: Option<u16>,
}

/// The testing source address of the media transport, if one is set.
#[cfg(feature = "testing")]
pub(crate) fn source_ip(cfg: &ClientConfig) -> Option<IpAddr> {
    cfg.testing.source_ip.read().ok().and_then(|g| *g)
}

/// The media transport's source address: the OS chooses (no testing override in this build).
#[cfg(not(feature = "testing"))]
pub(crate) fn source_ip(_cfg: &ClientConfig) -> Option<IpAddr> {
    None
}

/// Where the engine's crypto provider came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderSource {
    /// [`ClientConfig::with_crypto_provider`].
    Caller,
    /// `CryptoProvider::get_default` (installed by the host).
    ProcessDefault,
    /// ring, the built-in (feature `builtin-ring-provider`).
    #[cfg_attr(not(feature = "builtin-ring-provider"), allow(dead_code))]
    BuiltIn,
}

impl ProviderSource {
    fn as_str(self) -> &'static str {
        match self {
            ProviderSource::Caller => "caller",
            ProviderSource::ProcessDefault => "process default",
            ProviderSource::BuiltIn => "built-in",
        }
    }
}

/// The refusal when no provider is supplied, none is installed and the built-in is off.
#[cfg(not(feature = "builtin-ring-provider"))]
pub(crate) const NO_CRYPTO_PROVIDER: &str = "no_crypto_provider";

/// The crypto provider the engine's TLS uses: `explicit`, else the process default, else the
/// built-in ring provider (feature `builtin-ring-provider`); without that feature, an error.
/// The choice is logged at info the first time it is made and whenever it changes.
pub(crate) fn crypto_provider(
    explicit: Option<&Arc<CryptoProvider>>,
) -> Result<Arc<CryptoProvider>, rustls::Error> {
    let (provider, source) = choose_provider(explicit, CryptoProvider::get_default())?;
    log_choice(&provider, source);
    Ok(provider)
}

fn choose_provider(
    explicit: Option<&Arc<CryptoProvider>>,
    default: Option<&Arc<CryptoProvider>>,
) -> Result<(Arc<CryptoProvider>, ProviderSource), rustls::Error> {
    if let Some(p) = explicit {
        return Ok((p.clone(), ProviderSource::Caller));
    }
    if let Some(p) = default {
        return Ok((p.clone(), ProviderSource::ProcessDefault));
    }
    builtin_provider()
}

#[cfg(feature = "builtin-ring-provider")]
fn builtin_provider() -> Result<(Arc<CryptoProvider>, ProviderSource), rustls::Error> {
    Ok((
        Arc::new(rustls::crypto::ring::default_provider()),
        ProviderSource::BuiltIn,
    ))
}

#[cfg(not(feature = "builtin-ring-provider"))]
fn builtin_provider() -> Result<(Arc<CryptoProvider>, ProviderSource), rustls::Error> {
    Err(rustls::Error::General(format!(
        "{NO_CRYPTO_PROVIDER}: no rustls crypto provider was passed \
         (ClientConfig::with_crypto_provider) or installed as the process default, and the \
         built-in one is off (feature `builtin-ring-provider`)"
    )))
}

/// A provider's name: its random source's `Debug` name (`Ring` for ring's, `AwsLcRs` for
/// aws-lc-rs's; a host provider's own).
pub(crate) fn provider_name(p: &CryptoProvider) -> String {
    format!("{:?}", p.secure_random)
}

fn log_choice(provider: &CryptoProvider, source: ProviderSource) {
    static LAST: std::sync::Mutex<Option<(String, ProviderSource)>> = std::sync::Mutex::new(None);
    let choice = (provider_name(provider), source);
    let mut last = LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.as_ref() != Some(&choice) {
        tracing::info!("tls provider: {} ({})", choice.0, source.as_str());
        *last = Some(choice);
    }
}

/// The TLS client config every connection of the engine uses.
pub(crate) fn tls_config(cfg: &ClientConfig) -> Result<Arc<TlsConfig>, rustls::Error> {
    let provider = crypto_provider(cfg.crypto_provider.as_ref())?;
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

/// The platform's trust alone (the key session's default when the engine gives it none), on the
/// process default provider, else the built-in.
pub(crate) fn platform_tls() -> Result<Arc<TlsConfig>, rustls::Error> {
    let provider = crypto_provider(None)?;
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
    /// `testing` build, plus the run CA), on the provider [`ClientConfig::crypto_provider`]
    /// describes. Hosts that set up the keys themselves
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

    #[cfg(feature = "builtin-ring-provider")]
    #[test]
    fn the_platform_verifier_builds() {
        let cfg = ClientConfig::new("test.app", std::env::temp_dir());
        assert!(tls_config(&cfg).is_ok());
    }

    #[cfg(feature = "builtin-ring-provider")]
    fn suites(p: &CryptoProvider) -> Vec<rustls::CipherSuite> {
        p.cipher_suites.iter().map(|s| s.suite()).collect()
    }

    #[cfg(feature = "builtin-ring-provider")]
    fn groups(p: &CryptoProvider) -> Vec<rustls::NamedGroup> {
        p.kx_groups.iter().map(|g| g.name()).collect()
    }

    /// A provider that is not ring's default: one cipher suite, one group.
    fn narrow() -> Arc<CryptoProvider> {
        let ring = rustls::crypto::ring::default_provider();
        Arc::new(CryptoProvider {
            cipher_suites: vec![rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256],
            kx_groups: vec![rustls::crypto::ring::kx_group::X25519],
            ..ring
        })
    }

    #[test]
    fn the_explicit_provider_wins_then_the_default() {
        let explicit = narrow();
        let default = narrow();
        let (p, src) = choose_provider(Some(&explicit), Some(&default)).unwrap();
        assert!(Arc::ptr_eq(&p, &explicit), "the explicit provider is used");
        assert_eq!(src, ProviderSource::Caller);
        let (p, src) = choose_provider(None, Some(&default)).unwrap();
        assert!(Arc::ptr_eq(&p, &default), "the process default is used");
        assert_eq!(src, ProviderSource::ProcessDefault);
    }

    #[test]
    fn provider_names() {
        assert_eq!(
            provider_name(&rustls::crypto::ring::default_provider()),
            "Ring"
        );
    }

    /// Nothing in this test binary installs a process default, so the configs are ring's, as
    /// before the option existed (tests/tls_provider.rs covers an installed default; nothing
    /// here may call a rustls builder that installs one from the crate features).
    #[cfg(feature = "builtin-ring-provider")]
    #[test]
    fn without_a_default_the_configs_are_on_the_builtin_ring() {
        assert!(CryptoProvider::get_default().is_none());
        let ring = rustls::crypto::ring::default_provider();
        let (p, src) = choose_provider(None, None).unwrap();
        assert_eq!(src, ProviderSource::BuiltIn);
        assert_eq!(suites(&p), suites(&ring), "ring's suites");
        assert_eq!(groups(&p), groups(&ring), "ring's groups");
        let cfg = ClientConfig::new("test.app", std::env::temp_dir());
        for tls in [tls_config(&cfg).unwrap(), platform_tls().unwrap()] {
            assert_eq!(suites(tls.crypto_provider()), suites(&ring));
            assert_eq!(groups(tls.crypto_provider()), groups(&ring));
        }
        let explicit = narrow();
        let tls = tls_config(&cfg.with_crypto_provider(explicit.clone())).unwrap();
        assert!(Arc::ptr_eq(tls.crypto_provider(), &explicit));
    }

    /// Without the built-in and with nothing passed or installed, TLS setup is refused; an
    /// explicit provider still works.
    #[cfg(not(feature = "builtin-ring-provider"))]
    #[test]
    fn without_the_builtin_nothing_is_guessed() {
        assert!(CryptoProvider::get_default().is_none());
        let e = choose_provider(None, None).unwrap_err();
        assert!(e.to_string().contains(NO_CRYPTO_PROVIDER), "{e}");
        let cfg = ClientConfig::new("test.app", std::env::temp_dir());
        let e = tls_config(&cfg).unwrap_err();
        assert!(e.to_string().contains(NO_CRYPTO_PROVIDER), "{e}");
        let e = platform_tls().unwrap_err();
        assert!(e.to_string().contains(NO_CRYPTO_PROVIDER), "{e}");
        let explicit = narrow();
        let tls = tls_config(&cfg.with_crypto_provider(explicit.clone())).unwrap();
        assert!(Arc::ptr_eq(tls.crypto_provider(), &explicit));
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
