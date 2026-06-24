//! `axp-proto` — AXP wire-protocol types.
//!
//! This crate contains pure data types (no IO) that define the AXP protocol
//! wire format. It is the lowest-level crate in the workspace and has no
//! internal AXP dependencies.

mod version;

pub use version::PROTOCOL_VERSION;
