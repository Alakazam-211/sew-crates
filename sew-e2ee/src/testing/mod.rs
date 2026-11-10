//! The test harness (feature `testing`): the standard fixtures (test CA, DS,
//! install, members with software device keys), the in-process delivery service model
//! [`mini_ds::MiniDs`] and the [`sim::Sim`] harness.
//!
//! **Tests only.** This module is unstable and not covered by semver; it exposes secret key
//! material (test CA, DS and device keys) and builds malicious protocol messages ([`rogue`]).
//! Never enable the `testing` feature outside tests.

pub mod fixtures;

pub use fixtures::{Install, TestCa, TestDevice, TestDs};
pub mod mini_ds;
pub mod rogue;
pub mod sim;

pub use mini_ds::{ClientId, DsConfig, DsFaults, MiniDs, TableCorruption};
pub use sim::{FrameStats, Sim, SimClient, SimConfig, Trace};
