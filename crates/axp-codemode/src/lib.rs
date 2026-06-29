//! `axp-codemode` — WASM Component Model execution support.
//!
//! This crate owns the in-process WebAssembly runtime used by AXP code-mode
//! jobs.

mod error;
mod runner;

pub use error::{Error, Result};
pub use runner::{
    CodeModeInterruptHandle, CodeModeRunner, DEFAULT_ENTRYPOINT, DEFAULT_EPOCH_DEADLINE,
    DEFAULT_FUEL, DEFAULT_HOST_RESULT_IMPORT, HostImports, RunOutput, RunnerConfig,
};
