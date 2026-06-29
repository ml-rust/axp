//! WASM Component Model runner.

use wasmtime::{
    Config, Engine, Store,
    component::{Component, Linker},
};

use crate::{Error, Result};

/// Default exported function called by [`CodeModeRunner`].
pub const DEFAULT_ENTRYPOINT: &str = "run";

/// Default fuel budget charged to a component execution.
pub const DEFAULT_FUEL: u64 = 10_000_000;

/// Result of one code-mode component execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    /// The exported function that was called.
    pub entrypoint: String,
}

/// Configuration for a [`CodeModeRunner`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerConfig {
    /// Exported no-argument function to call after instantiation.
    pub entrypoint: String,
    /// Fuel budget for each run.
    pub fuel: u64,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            entrypoint: DEFAULT_ENTRYPOINT.to_string(),
            fuel: DEFAULT_FUEL,
        }
    }
}

/// Runs WebAssembly components with no ambient host imports.
#[derive(Clone)]
pub struct CodeModeRunner {
    engine: Engine,
    config: RunnerConfig,
}

impl CodeModeRunner {
    /// Build a runner using the default entrypoint and fuel budget.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying wasmtime engine cannot be configured.
    pub fn new() -> Result<Self> {
        Self::with_config(RunnerConfig::default())
    }

    /// Build a runner with explicit configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying wasmtime engine cannot be configured.
    pub fn with_config(config: RunnerConfig) -> Result<Self> {
        let mut wasmtime_config = Config::new();
        wasmtime_config.wasm_component_model(true);
        wasmtime_config.consume_fuel(true);

        let engine = Engine::new(&wasmtime_config).map_err(Error::Engine)?;
        Ok(Self { engine, config })
    }

    /// Compile, instantiate, and call the configured component entrypoint.
    ///
    /// The component receives no WASI or other ambient imports in this runner.
    ///
    /// # Errors
    ///
    /// Returns an error if the component fails to compile or instantiate, if the
    /// entrypoint is missing or has the wrong type, or if execution traps.
    pub fn run_component(&self, bytes: &[u8]) -> Result<RunOutput> {
        let component = Component::new(&self.engine, bytes).map_err(Error::Compile)?;
        let linker = Linker::new(&self.engine);
        let mut store = Store::new(&self.engine, ());
        store.set_fuel(self.config.fuel).map_err(Error::Fuel)?;

        let instance = linker
            .instantiate(&mut store, &component)
            .map_err(Error::Instantiate)?;
        let entrypoint = instance
            .get_typed_func::<(), ()>(&mut store, &self.config.entrypoint)
            .map_err(|source| Error::Entrypoint {
                name: self.config.entrypoint.clone(),
                source,
            })?;
        entrypoint.call(&mut store, ()).map_err(Error::Run)?;

        Ok(RunOutput {
            entrypoint: self.config.entrypoint.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOOP_COMPONENT: &str = r#"
        (component
          (core module $m
            (func (export "run")))
          (core instance $i (instantiate $m))
          (func (export "run") (canon lift (core func $i "run"))))
    "#;

    const LOOP_COMPONENT: &str = r#"
        (component
          (core module $m
            (func $run (export "run")
              (loop $again
                br $again)))
          (core instance $i (instantiate $m))
          (func (export "run") (canon lift (core func $i "run"))))
    "#;

    #[test]
    fn runs_no_arg_component_entrypoint() {
        let runner = CodeModeRunner::new().expect("runner config");

        let output = runner
            .run_component(NOOP_COMPONENT.as_bytes())
            .expect("component should run");

        assert_eq!(output.entrypoint, DEFAULT_ENTRYPOINT);
    }

    #[test]
    fn missing_entrypoint_errors_before_run() {
        let runner = CodeModeRunner::with_config(RunnerConfig {
            entrypoint: "missing".to_string(),
            fuel: DEFAULT_FUEL,
        })
        .expect("runner config");

        let err = runner
            .run_component(NOOP_COMPONENT.as_bytes())
            .expect_err("missing export should fail");

        assert!(matches!(err, Error::Entrypoint { .. }));
    }

    #[test]
    fn fuel_exhaustion_stops_component() {
        let runner = CodeModeRunner::with_config(RunnerConfig {
            entrypoint: DEFAULT_ENTRYPOINT.to_string(),
            fuel: 1_000,
        })
        .expect("runner config");

        let err = runner
            .run_component(LOOP_COMPONENT.as_bytes())
            .expect_err("loop should exhaust fuel");

        assert!(matches!(err, Error::Run(_)));
    }
}
