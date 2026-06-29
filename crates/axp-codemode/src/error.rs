//! Error types for WASM code-mode execution.

/// Code-mode runner errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Failed to configure the WebAssembly engine.
    #[error("failed to configure WASM engine: {0}")]
    Engine(#[source] wasmtime::Error),
    /// Failed to compile the component bytes.
    #[error("failed to compile WASM component: {0}")]
    Compile(#[source] wasmtime::Error),
    /// Failed to initialize the fuel budget.
    #[error("failed to initialize WASM fuel: {0}")]
    Fuel(#[source] wasmtime::Error),
    /// Failed to instantiate the component.
    #[error("failed to instantiate WASM component: {0}")]
    Instantiate(#[source] wasmtime::Error),
    /// The configured entrypoint was missing or had an incompatible signature.
    #[error("WASM component entrypoint `{name}` is not callable with the configured signature")]
    Entrypoint {
        /// Entrypoint name requested by the runner configuration.
        name: String,
        /// Underlying wasmtime error.
        #[source]
        source: wasmtime::Error,
    },
    /// The entrypoint trapped or failed while running.
    #[error("WASM component execution failed: {0}")]
    Run(#[source] wasmtime::Error),
}

/// Result alias for code-mode operations.
pub type Result<T> = std::result::Result<T, Error>;
