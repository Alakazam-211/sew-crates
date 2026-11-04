//! `sew-lk-compat`: the LiveKit-protocol pieces of Sew's native voice engine. Not
//! affiliated with or endorsed by LiveKit, Inc.
//!
//! - [`signal`]: the `/rtc/v1` join URL with its `join_request`, `ClientInfo`, trickles.
//! - [`session`]: the Sans-IO signalling session of a single-PC voice client.
//! - [`e2ee`]: LiveKit's AES-GCM frame format (`LkGcmV1`) and its configuration.
//! - [`compat`]: client-sdk-js rules (`compareVersions`, the validate classifier).
//!
//! `signal` and `session` were rewritten from a written behaviour specification; `compat` and
//! `e2ee::lkgcm` are ported from LiveKit's client-sdk-js (Apache-2.0). `sew-call-client` (MIT OR
//! Apache-2.0) builds the engine on these and holds no LiveKit-derived code itself. See this
//! crate's README and NOTICE for the origins.

pub mod compat;
pub mod e2ee;
pub mod session;
pub mod signal;

pub use session::{LeaveAction, Session, SessionCounters, SessionEvent};
pub use signal::{ClientInfoOpts, JoinParams};
