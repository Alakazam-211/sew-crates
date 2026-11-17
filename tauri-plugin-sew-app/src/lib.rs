//! `tauri-plugin-sew-app`: the shell of a Tauri 2 App on a Sew server.
//!
//! A Tauri App on a Sew server (on K2, or standalone) is a thin shell that loads the App's live
//! site and signs in to the App's server. This plugin is the part every such shell would
//! otherwise write itself:
//!
//! - **Servers.** A saved list of App servers, each added with a confirm step (the person
//!   checks the host before anything is saved), with several accounts per server from the
//!   start: everything is keyed by server and account ([`AccountKey`]).
//! - **Secrets.** The device bearer (`sewdv_…`) of each account in the OS keychain
//!   ([`SecretStore`]: macOS Keychain, Windows Credential Manager, the Secret Service), with a
//!   0600 file only when the app opts in ([`SecretChoice::OsOrFile`]).
//! - **Sign-in** through the App server's native lane (`/sew/native/*`): the host's sign-in
//!   door when the server offers it (the password goes from Rust to the host, never through
//!   the page or the lane), the password form, and pairing with a code a signed-in page shows
//!   (typed in, or from a pairing link the person confirms). A server that needs a newer shell
//!   (`info.shell.min`) refuses sign-in with `shell_too_old`.
//!   **Sign-out** ends the device session on the server, deletes the bearer and,
//!   when no signed-in account is left on that Sew install, calls the voice plugin's
//!   `forget_server`.
//! - **Voice.** [`LaneJoinProvider`], a ready `JoinProvider` for `tauri-plugin-sew-voice`.
//! - **The webview.** A handoff code for the page's cookie session, and `open`, which loads a
//!   page of the server signed in.
//! - **The API transport**, for a bundled UI that holds no secret: `api` (the App's client
//!   API through the lane, [`api`]), the gateway socket held in Rust and relayed over a
//!   channel (`gateway_open`, `gateway_send`, `gateway_close`, [`gateway`]), the signed byte
//!   routes on the `sewfile:` URI scheme, and uploads (`upload_bytes`, `upload_file`). The
//!   device bearer and the gateway tickets never reach the page.
//! - **Shell plumbing** (feature `tauri`): the remote-origin capability of each saved server,
//!   deep links, single instance and a notifications bridge (`plugin:sew-app|notify`).
//!
//! ```ignore
//! let app = tauri_plugin_sew_app::Builder::new()
//!     .deep_link_scheme("myapp")
//!     .handoff_page("/native/handoff");
//! let provider = app.join_provider();
//! app.install(tauri::Builder::default())
//!     .plugin(tauri_plugin_sew_voice::Builder::new(provider).build())
//!     .run(tauri::generate_context!())?;
//! ```
//!
//! Without the `tauri` feature the crate is the core ([`AppService`], the lane client over a
//! [`Transport`], the stores, the provider, the fake lane), which builds and tests without
//! Tauri's system libraries. The README is the guide.

pub mod api;
pub mod deeplink;
pub mod error;
pub mod fake;
pub mod gateway;
#[cfg(feature = "http")]
pub mod http;
pub mod lane;
pub mod origin;
pub mod provider;
pub mod secrets;
pub mod servers;
pub mod service;
pub mod wire;
#[cfg(feature = "http")]
pub mod ws;

#[cfg(feature = "tauri")]
mod commands;
#[cfg(feature = "tauri")]
mod plugin;

pub use api::{ApiCall, ApiReply, ByteReply, FILE_SCHEME, Upload, UploadData};
pub use deeplink::{DeepLinkEvent, DeepLinkRoute};
pub use error::{AppError, code};
pub use gateway::{
    ConnectError, GatewayEvent, GatewayOpened, GatewaySink, MAX_GATEWAY_SOCKETS, SocketConnector,
    SocketPipe, WsIn, WsOut,
};
pub use lane::{
    ByteMethod, ByteRequest, ByteResponse, DeviceName, DoorStart, HostDoorFailure, LANE_VERSION,
    LaneClient, LaneFailure, LaneInfo, LaneRequest, LaneResponse, Method, Refusal, Secret,
    SessionInfo, ShellInfo, SignIn, SigninOffer, TicketAnswer, Transport, TransportError, WhoAmI,
};
pub use origin::Origin;
#[cfg(feature = "tauri")]
pub use plugin::{Builder, RemoteCapability, TauriAppSink};
pub use provider::LaneJoinProvider;
#[cfg(feature = "keyring-store")]
pub use secrets::KeyringStore;
pub use secrets::{
    FileStore, KeychainOptions, MemoryStore, SecretChoice, SecretError, SecretKey, SecretStore,
};
pub use servers::{Account, AccountKey, AccountState, AccountView, Book, ServerView, ServersView};
pub use service::{
    AppConfig, AppEvent, AppEventSink, AppService, HandoffView, NoEvents, PendingServer,
    SignOutResult, VoiceForget,
};

/// The plugin's name: commands are `plugin:sew-app|<command>`, permissions `sew-app:*`.
pub const PLUGIN_NAME: &str = "sew-app";

/// The events' prefix (`sew-app://servers`, ...).
pub const EVENT_PREFIX: &str = "sew-app://";

/// README.md's examples, compiled and run as doc tests (with the `tauri` feature).
#[cfg(all(doctest, feature = "tauri"))]
#[doc = include_str!("../README.md")]
struct ReadmeExamples;
