//! `axp-core` — AXP runtime core.
//!
//! This crate provides the runtime machinery that drives agent execution.
//! It depends on `axp-proto` for protocol types and provides:
//!
//! - [`Workspace`] — canonical, absolute workspace root.
//! - [`CapToken`] — unforgeable sparse-capability session credential.
//! - [`RuntimeCapability`] / [`CapabilitySet`] — structured, attenuable capability grants.
//! - [`Session`] / [`SessionStore`] — runtime session lifecycle and in-memory store.
//! - [`Provider`] / [`NativeProvider`] — runtime capability provider trait and in-memory impl.
//! - [`McpToolProvider`] — static MCP tool adapter for the provider layer.
//! - [`ProviderRegistry`] — aggregates providers and serves the unified discovery catalog.
//! - [`builtin_registry`] — constructs the default registry pre-populated with built-in providers.
//! - [`Job`] / [`JobStore`] — runtime job model, log buffering, and in-memory job store.
//! - [`AppendLogEvent`] / [`JobReplayLog`] / [`FileJobReplayLog`] / [`LogBuffer`] / [`LogEvent`] /
//!   [`LogStream`] / [`Seq`] — append/replay log types for job output.
//!
//! # Security note
//!
//! Capability attenuation is a security-sensitive operation: a child grant can
//! only narrow (never broaden) authority, and verbs are strictly orthogonal.
//! See [`capability`] for the full contract.

mod auth;
mod builtins;
mod capability;
mod error;
mod job;
mod mcp;
mod provider;
mod registry;
mod session;
mod workspace;

pub use auth::CapToken;
pub use builtins::builtin_registry;
pub use capability::{CapabilitySet, RuntimeCapability};
pub use error::{Error, Result};
pub use job::{
    AppendLogEvent, DEFAULT_LOG_BYTE_CAP, FileJobReplayLog, Job, JobEngine, JobLogStream,
    JobReplayLog, JobStatus, JobStore, LogBuffer, LogEvent, LogStream, Seq, resolve_cwd,
};
pub use mcp::{McpBridgeCommand, McpToolDescriptor, McpToolProvider};
pub use provider::{
    CapabilityArg, CapabilityDescriptor, CapabilityListing, ExecutionSpec, NativeProvider,
    Provider, ResolvedCommand,
};
pub use registry::ProviderRegistry;
pub use session::{AuditEvent, AuditEventKind, Session, SessionStore};
pub use workspace::Workspace;
