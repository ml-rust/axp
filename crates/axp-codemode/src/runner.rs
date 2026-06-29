//! WASM Component Model runner.

use wasmtime::{
    Config, Engine, Store, StoreContextMut,
    component::{Component, Linker},
};

use crate::{Error, Result};

/// Default exported function called by [`CodeModeRunner`].
pub const DEFAULT_ENTRYPOINT: &str = "run";

/// Default fuel budget charged to a component execution.
pub const DEFAULT_FUEL: u64 = 10_000_000;

/// Default root import name for a host-provided string result.
pub const DEFAULT_HOST_RESULT_IMPORT: &str = "host-result";

/// Result of one code-mode component execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    /// The exported function that was called.
    pub entrypoint: String,
    /// String returned by the component entrypoint, when it returns one.
    pub result: Option<String>,
}

/// Host imports available to a code-mode component.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostImports {
    /// String value exposed through the `host-result` root import.
    pub host_result: Option<String>,
}

/// Configuration for a [`CodeModeRunner`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerConfig {
    /// Exported no-argument function to call after instantiation.
    pub entrypoint: String,
    /// Fuel budget for each run.
    pub fuel: u64,
    /// Host imports linked into each component instance.
    pub host_imports: HostImports,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            entrypoint: DEFAULT_ENTRYPOINT.to_string(),
            fuel: DEFAULT_FUEL,
            host_imports: HostImports::default(),
        }
    }
}

#[derive(Debug, Clone)]
struct CodeModeStore {
    host_result: String,
}

/// Runs WebAssembly components with configured host imports.
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
    /// The component receives no WASI or ambient imports beyond those configured
    /// for this runner.
    ///
    /// # Errors
    ///
    /// Returns an error if the component fails to compile or instantiate, if the
    /// entrypoint is missing or has the wrong type, or if execution traps.
    pub fn run_component(&self, bytes: &[u8]) -> Result<RunOutput> {
        let component = Component::new(&self.engine, bytes).map_err(Error::Compile)?;
        let mut linker = Linker::new(&self.engine);
        let returns_host_result = self.config.host_imports.host_result.is_some();
        if returns_host_result {
            linker
                .root()
                .func_wrap(
                    DEFAULT_HOST_RESULT_IMPORT,
                    |store: StoreContextMut<'_, CodeModeStore>, (): ()| {
                        Ok((store.data().host_result.clone(),))
                    },
                )
                .map_err(Error::Instantiate)?;
        }

        let mut store = Store::new(
            &self.engine,
            CodeModeStore {
                host_result: self
                    .config
                    .host_imports
                    .host_result
                    .clone()
                    .unwrap_or_default(),
            },
        );
        store.set_fuel(self.config.fuel).map_err(Error::Fuel)?;

        let instance = linker
            .instantiate(&mut store, &component)
            .map_err(Error::Instantiate)?;
        let result = if returns_host_result {
            let entrypoint = instance
                .get_typed_func::<(), (String,)>(&mut store, &self.config.entrypoint)
                .map_err(|source| Error::Entrypoint {
                    name: self.config.entrypoint.clone(),
                    source,
                })?;
            let (result,) = entrypoint.call(&mut store, ()).map_err(Error::Run)?;
            Some(result)
        } else {
            let entrypoint = instance
                .get_typed_func::<(), ()>(&mut store, &self.config.entrypoint)
                .map_err(|source| Error::Entrypoint {
                    name: self.config.entrypoint.clone(),
                    source,
                })?;
            entrypoint.call(&mut store, ()).map_err(Error::Run)?;
            None
        };

        Ok(RunOutput {
            entrypoint: self.config.entrypoint.clone(),
            result,
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

    const HOST_RESULT_COMPONENT: &str = r#"
        (component
          (import "host-result" (func $host-result (result string)))
          (core module $memory
            (memory (export "memory") 1)
            (func (export "cabi_realloc")
              (param i32 i32 i32 i32)
              (result i32)
              i32.const 16))
          (core instance $memory-instance (instantiate $memory))
          (alias core export $memory-instance "memory" (core memory $memory-export))
          (alias core export $memory-instance "cabi_realloc" (core func $realloc))
          (core func $host-result-lowered
            (canon lower
              (func $host-result)
              (memory $memory-export)
              (realloc $realloc)
              string-encoding=utf8))
          (core instance $imports
            (export "host-result" (func $host-result-lowered)))
          (core instance $env
            (export "memory" (memory $memory-export)))
          (core module $m
            (import "" "host-result" (func $host-result-core (param i32)))
            (import "env" "memory" (memory 1))
            (func (export "run") (result i32)
              i32.const 0
              call $host-result-core
              i32.const 0))
          (core instance $i
            (instantiate $m
              (with "" (instance $imports))
              (with "env" (instance $env))))
          (alias core export $i "run" (core func $run))
          (func (export "run")
            (result string)
            (canon lift
              (core func $run)
              (memory $memory-export)
              (realloc $realloc)
              string-encoding=utf8)))
    "#;

    #[test]
    fn runs_no_arg_component_entrypoint() {
        let runner = CodeModeRunner::new().expect("runner config");

        let output = runner
            .run_component(NOOP_COMPONENT.as_bytes())
            .expect("component should run");

        assert_eq!(output.entrypoint, DEFAULT_ENTRYPOINT);
        assert_eq!(output.result, None);
    }

    #[test]
    fn returns_configured_host_result() {
        let runner = CodeModeRunner::with_config(RunnerConfig {
            host_imports: HostImports {
                host_result: Some("ready".to_string()),
            },
            ..RunnerConfig::default()
        })
        .expect("runner config");

        let output = runner
            .run_component(HOST_RESULT_COMPONENT.as_bytes())
            .expect("component should run");

        assert_eq!(output.entrypoint, DEFAULT_ENTRYPOINT);
        assert_eq!(output.result, Some("ready".to_string()));
    }

    #[test]
    fn missing_host_result_import_fails_without_config() {
        let runner = CodeModeRunner::new().expect("runner config");

        let err = runner
            .run_component(HOST_RESULT_COMPONENT.as_bytes())
            .expect_err("missing import should fail");

        assert!(matches!(err, Error::Instantiate(_)));
    }

    #[test]
    fn missing_entrypoint_errors_before_run() {
        let runner = CodeModeRunner::with_config(RunnerConfig {
            entrypoint: "missing".to_string(),
            ..RunnerConfig::default()
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
            fuel: 1_000,
            ..RunnerConfig::default()
        })
        .expect("runner config");

        let err = runner
            .run_component(LOOP_COMPONENT.as_bytes())
            .expect_err("loop should exhaust fuel");

        assert!(matches!(err, Error::Run(_)));
    }
}
