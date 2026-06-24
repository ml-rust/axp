//! `axp-sandbox` — per-OS sandbox backends for AXP.
//!
//! This crate implements the sandboxing layer that constrains what a running
//! agent process may do. Backends are OS-specific; the public surface exposed
//! here is OS-agnostic.

mod tier;

pub use tier::EnforcementTier;
