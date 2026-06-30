//! Shared application state threaded through axum handlers.

use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU64, Ordering},
};

use axp_core::{
    JobEngine, JobStore, McpBridgeCommand, McpToolDescriptor, McpToolProvider, ProviderRegistry,
    SessionStore, builtin_registry,
};
use axp_proto::SessionId;

fn static_mcp_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": true
    })
}

fn static_mcp_bridge_args() -> Vec<String> {
    vec!["call".to_owned()]
}

/// Shared state for all axum request handlers.
///
/// All fields are cheap to clone: `SessionStore` and `JobEngine` are
/// `Arc`-backed; `registry` is wrapped in an `Arc<RwLock<_>>` here because
/// `ProviderRegistry` is neither `Clone` nor `Send + Sync` on its own.
///
/// # Important invariant
///
/// `sessions` MUST be the same [`SessionStore`] that `engine` was built from
/// (i.e. `engine = JobEngine::new(sessions.clone(), ...)`).  `session.open`
/// inserts into the store that the engine observes for authorization checks —
/// mismatching them is a logic error.
#[derive(Clone)]
pub struct AppState {
    /// Live session store, shared with the job engine.
    pub sessions: SessionStore,

    /// Async job execution engine. Holds a reference to the same session store.
    pub engine: JobEngine,

    /// Capability provider registry, guarded by an `RwLock` because
    /// [`ProviderRegistry`] is not `Clone`.
    pub registry: Arc<RwLock<ProviderRegistry>>,

    /// Monotonically-increasing counter used to mint unique session ids.
    pub session_counter: Arc<AtomicU64>,
}

impl AppState {
    /// Build a fresh runtime state: a new session store, a job engine over it
    /// (sharing the same store), a provider registry pre-populated with the
    /// built-in capabilities, and a session-id counter starting at 1.
    ///
    /// This is the standard wiring used by the server and the tests.
    pub fn new() -> Self {
        Self::with_registry(builtin_registry())
    }

    /// Build runtime state with a caller-supplied capability provider registry.
    ///
    /// The same registry instance is shared by the job engine and the public
    /// discovery field.
    pub fn with_registry(registry: ProviderRegistry) -> Self {
        let sessions = SessionStore::new();
        // One registry, shared by the engine (for resolving capability payloads)
        // and the `registry` field (for the discovery handlers).
        let registry = Arc::new(RwLock::new(registry));
        let engine = JobEngine::new(sessions.clone(), JobStore::new(), Arc::clone(&registry));
        Self {
            sessions,
            engine,
            registry,
            session_counter: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Build runtime state with the built-in providers plus one static MCP tool
    /// provider exposed through the existing provider registry.
    pub fn with_mcp_tool(
        provider_id: String,
        tool_name: String,
        desc: String,
        bridge_program: String,
    ) -> axp_core::Result<Self> {
        let mut registry = builtin_registry();
        let provider = McpToolProvider::new(
            provider_id.clone(),
            vec![McpToolDescriptor {
                provider_id,
                name: tool_name,
                desc,
                schema: static_mcp_schema(),
                bridge: McpBridgeCommand {
                    program: bridge_program,
                    args: static_mcp_bridge_args(),
                },
            }],
        )?;
        registry.register(Box::new(provider))?;
        Ok(Self::with_registry(registry))
    }

    /// Atomically allocate the next session id and return it formatted as
    /// `s_<n>`.
    ///
    /// Uses `Relaxed` ordering — the returned id is unique within this process
    /// but no cross-thread synchronization of other state is implied.
    pub fn next_session_id(&self) -> SessionId {
        let n = self.session_counter.fetch_add(1, Ordering::Relaxed);
        SessionId(format!("s_{n}"))
    }
}

impl Default for AppState {
    /// Delegates to [`AppState::new`].
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_session_id_is_unique_and_prefixed() {
        let state = AppState::new();
        let id1 = state.next_session_id();
        let id2 = state.next_session_id();
        assert!(
            id1.as_str().starts_with("s_"),
            "expected s_ prefix: {}",
            id1.as_str()
        );
        assert_ne!(id1, id2, "consecutive ids must differ");
    }

    #[test]
    fn next_session_id_starts_at_one() {
        let state = AppState::new();
        let id = state.next_session_id();
        assert_eq!(id.as_str(), "s_1");
    }

    #[test]
    fn clone_shares_counter() {
        let state = AppState::new();
        let clone = state.clone();
        let _id1 = state.next_session_id(); // consumes 1
        let id2 = clone.next_session_id(); // should consume 2
        assert_eq!(id2.as_str(), "s_2");
    }

    #[test]
    fn with_registry_exposes_supplied_registry_and_starts_counter_at_one() {
        let mut registry = ProviderRegistry::new();
        let provider = McpToolProvider::new(
            "docs",
            vec![McpToolDescriptor {
                provider_id: "docs".to_owned(),
                name: "search".to_owned(),
                desc: "Search documentation with an external MCP bridge".to_owned(),
                schema: static_mcp_schema(),
                bridge: McpBridgeCommand {
                    program: "axp-mcp-bridge".to_owned(),
                    args: static_mcp_bridge_args(),
                },
            }],
        )
        .expect("valid MCP provider");
        registry
            .register(Box::new(provider))
            .expect("valid registry provider");

        let state = AppState::with_registry(registry);

        let index = state
            .registry
            .read()
            .expect("registry lock")
            .index()
            .expect("registry index");
        assert!(index.entries.iter().any(|entry| entry.name == "search"));
        assert_eq!(state.next_session_id().as_str(), "s_1");
    }

    #[test]
    fn with_mcp_tool_registers_static_mount() {
        let state = AppState::with_mcp_tool(
            "docs".to_owned(),
            "search".to_owned(),
            "Search documentation with an external MCP bridge".to_owned(),
            "axp-mcp-bridge".to_owned(),
        )
        .expect("valid MCP mount");

        let detail = state
            .registry
            .read()
            .expect("registry lock")
            .describe("search")
            .expect("registered MCP tool");
        assert_eq!(detail.schema, static_mcp_schema());
    }
}
