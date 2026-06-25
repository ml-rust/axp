//! `axp-core` — AXP runtime core.
//!
//! This crate provides the runtime machinery that drives agent execution.
//! It depends on `axp-proto` for protocol types and provides:
//!
//! - [`Workspace`] — canonical, absolute workspace root.
//! - [`RuntimeCapability`] / [`CapabilitySet`] — structured, attenuable capability grants.
//! - [`Session`] / [`SessionStore`] — runtime session lifecycle and in-memory store.
//! - [`Provider`] / [`NativeProvider`] — runtime capability provider trait and in-memory impl.
//! - [`ProviderRegistry`] — aggregates providers and serves the unified discovery catalog.
//! - [`builtin_registry`] — constructs the default registry pre-populated with built-in providers.
//! - [`Job`] / [`JobStore`] — runtime job model, log buffering, and in-memory job store.
//!
//! # Security note
//!
//! Capability attenuation is a security-sensitive operation: a child grant can
//! only narrow (never broaden) authority, and verbs are strictly orthogonal.
//! See [`capability`] for the full contract.

mod builtins;
mod capability;
mod error;
mod job;
mod provider;
mod registry;
mod session;
mod workspace;

pub use builtins::builtin_registry;
pub use capability::{CapabilitySet, RuntimeCapability};
pub use error::{Error, Result};
pub use job::{
    DEFAULT_LOG_BYTE_CAP, Job, JobEngine, JobLogStream, JobStatus, JobStore, LogBuffer, LogEvent,
    LogStream, Seq, resolve_cwd,
};
pub use provider::{CapabilityDescriptor, CapabilityListing, NativeProvider, Provider};
pub use registry::ProviderRegistry;
pub use session::{AuditEvent, AuditEventKind, Session, SessionStore};
pub use workspace::Workspace;
