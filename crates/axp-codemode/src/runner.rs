//! WASM Component Model runner.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use wasmtime::{
    Config, Engine, Store, StoreContextMut,
    component::{Component, Linker},
};

use crate::{Error, Result};

/// Default exported function called by [`CodeModeRunner`].
pub const DEFAULT_ENTRYPOINT: &str = "run";

/// Default fuel budget charged to a component execution.
pub const DEFAULT_FUEL: u64 = 10_000_000;

/// Default epoch delta before an interrupt traps component execution.
pub const DEFAULT_EPOCH_DEADLINE: u64 = 1;

/// Default root import name for a host-provided string result.
pub const DEFAULT_HOST_RESULT_IMPORT: &str = "host-result";

/// Default root import name for invoking a host-provided capability.
pub const DEFAULT_CAPABILITY_INVOKE_IMPORT: &str = "axp:capability/invoke";

/// Result returned by a host capability invocation handler.
pub type CapabilityInvokeResult = std::result::Result<String, String>;

/// Synchronous host callback used by `axp:capability/invoke`.
pub type CapabilityInvokeHandler =
    Arc<dyn Fn(&str, &str) -> CapabilityInvokeResult + Send + Sync + 'static>;

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
#[derive(Clone)]
pub struct RunnerConfig {
    /// Exported no-argument function to call after instantiation.
    pub entrypoint: String,
    /// Fuel budget for each run.
    pub fuel: u64,
    /// Host imports linked into each component instance.
    pub host_imports: HostImports,
    /// Optional handler linked as the `axp:capability/invoke` root import.
    pub capability_invoke: Option<CapabilityInvokeHandler>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            entrypoint: DEFAULT_ENTRYPOINT.to_string(),
            fuel: DEFAULT_FUEL,
            host_imports: HostImports::default(),
            capability_invoke: None,
        }
    }
}

impl std::fmt::Debug for RunnerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnerConfig")
            .field("entrypoint", &self.entrypoint)
            .field("fuel", &self.fuel)
            .field("host_imports", &self.host_imports)
            .field("capability_invoke", &self.capability_invoke.is_some())
            .finish()
    }
}

/// Thread-safe handle that interrupts code-mode execution for one runner.
#[derive(Debug, Clone)]
pub struct CodeModeInterruptHandle {
    engine: Engine,
    interrupted: Arc<AtomicBool>,
}

impl CodeModeInterruptHandle {
    /// Request interruption of stores using this runner's engine.
    ///
    /// The next epoch check in a store whose deadline has been armed will trap.
    pub fn interrupt(&self) {
        self.interrupted.store(true, Ordering::Release);
        self.engine.increment_epoch();
    }
}

struct CodeModeStore {
    host_result: String,
    capability_invoke: Option<CapabilityInvokeHandler>,
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
        wasmtime_config.epoch_interruption(true);

        let engine = Engine::new(&wasmtime_config).map_err(Error::Engine)?;
        Ok(Self { engine, config })
    }

    /// Return a handle that can interrupt component execution from another thread.
    pub fn interrupt_handle(&self) -> CodeModeInterruptHandle {
        CodeModeInterruptHandle {
            engine: self.engine.clone(),
            interrupted: Arc::new(AtomicBool::new(false)),
        }
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
        self.run_component_with_interrupt(bytes, &self.interrupt_handle())
    }

    /// Compile, instantiate, and call the configured entrypoint with cancellation.
    ///
    /// The interrupt handle must come from this runner.
    ///
    /// # Errors
    ///
    /// Returns an error if the component fails to compile or instantiate, if the
    /// entrypoint is missing or has the wrong type, or if execution traps.
    pub fn run_component_with_interrupt(
        &self,
        bytes: &[u8],
        interrupt: &CodeModeInterruptHandle,
    ) -> Result<RunOutput> {
        if !Engine::same(&self.engine, &interrupt.engine) {
            return Err(Error::Run(wasmtime::Error::msg(
                "interrupt handle belongs to a different code-mode runner",
            )));
        }

        let component = Component::new(&self.engine, bytes).map_err(Error::Compile)?;
        let mut linker = Linker::new(&self.engine);
        let requires_string_result = self.config.host_imports.host_result.is_some();
        let accepts_string_result =
            requires_string_result || self.config.capability_invoke.is_some();
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
        if self.config.capability_invoke.is_some() {
            linker
                .root()
                .func_wrap(
                    DEFAULT_CAPABILITY_INVOKE_IMPORT,
                    |store: StoreContextMut<'_, CodeModeStore>,
                     (name, params_json): (String, String)| {
                        let handler = store.data().capability_invoke.as_ref().ok_or_else(|| {
                            wasmtime::Error::msg("capability invoke handler not configured")
                        })?;
                        handler(&name, &params_json)
                            .map(|result| (result,))
                            .map_err(wasmtime::Error::msg)
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
                capability_invoke: self.config.capability_invoke.clone(),
            },
        );
        store.set_fuel(self.config.fuel).map_err(Error::Fuel)?;
        store.set_epoch_deadline(DEFAULT_EPOCH_DEADLINE);
        store.epoch_deadline_trap();
        if interrupt.interrupted.load(Ordering::Acquire) {
            store.set_epoch_deadline(0);
        }

        let instance = linker
            .instantiate(&mut store, &component)
            .map_err(Error::Instantiate)?;
        let result = if requires_string_result {
            let entrypoint = instance
                .get_typed_func::<(), (String,)>(&mut store, &self.config.entrypoint)
                .map_err(|source| Error::Entrypoint {
                    name: self.config.entrypoint.clone(),
                    source,
                })?;
            let (result,) = entrypoint.call(&mut store, ()).map_err(Error::Run)?;
            Some(result)
        } else if accepts_string_result {
            match instance.get_typed_func::<(), (String,)>(&mut store, &self.config.entrypoint) {
                Ok(entrypoint) => {
                    let (result,) = entrypoint.call(&mut store, ()).map_err(Error::Run)?;
                    Some(result)
                }
                Err(string_source) => {
                    let entrypoint = instance
                        .get_typed_func::<(), ()>(&mut store, &self.config.entrypoint)
                        .map_err(|_| Error::Entrypoint {
                            name: self.config.entrypoint.clone(),
                            source: string_source,
                        })?;
                    entrypoint.call(&mut store, ()).map_err(Error::Run)?;
                    None
                }
            }
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

    const CAPABILITY_INVOKE_COMPONENT: &str = r#"
        (component
          (import "axp:capability/invoke"
            (func $invoke (param "name" string) (param "params-json" string) (result string)))
          (core module $memory
            (memory (export "memory") 1)
            (global $next (mut i32) (i32.const 256))
            (data (i32.const 32) "say_hi")
            (data (i32.const 64) "{}")
            (func (export "cabi_realloc")
              (param i32 i32 i32 i32)
              (result i32)
              (local $ptr i32)
              global.get $next
              local.set $ptr
              global.get $next
              local.get 3
              i32.add
              global.set $next
              local.get $ptr))
          (core instance $memory-instance (instantiate $memory))
          (alias core export $memory-instance "memory" (core memory $memory-export))
          (alias core export $memory-instance "cabi_realloc" (core func $realloc))
          (core func $invoke-lowered
            (canon lower
              (func $invoke)
              (memory $memory-export)
              (realloc $realloc)
              string-encoding=utf8))
          (core instance $imports
            (export "axp:capability/invoke" (func $invoke-lowered)))
          (core instance $env
            (export "memory" (memory $memory-export)))
          (core module $m
            (import "" "axp:capability/invoke"
              (func $invoke-core (param i32 i32 i32 i32 i32)))
            (import "env" "memory" (memory 1))
            (func (export "run") (result i32)
              i32.const 32
              i32.const 6
              i32.const 64
              i32.const 2
              i32.const 0
              call $invoke-core
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
    fn invokes_configured_capability_handler() {
        let runner = CodeModeRunner::with_config(RunnerConfig {
            capability_invoke: Some(Arc::new(|name, params_json| {
                Ok(format!("{name}:{params_json}"))
            })),
            ..RunnerConfig::default()
        })
        .expect("runner config");

        let output = runner
            .run_component(CAPABILITY_INVOKE_COMPONENT.as_bytes())
            .expect("component should run");

        assert_eq!(output.entrypoint, DEFAULT_ENTRYPOINT);
        assert_eq!(output.result, Some("say_hi:{}".to_string()));
    }

    #[test]
    fn missing_capability_invoke_import_fails_without_config() {
        let runner = CodeModeRunner::new().expect("runner config");

        let err = runner
            .run_component(CAPABILITY_INVOKE_COMPONENT.as_bytes())
            .expect_err("missing import should fail");

        assert!(matches!(err, Error::Instantiate(_)));
    }

    #[test]
    fn capability_handler_error_traps_run() {
        let runner = CodeModeRunner::with_config(RunnerConfig {
            capability_invoke: Some(Arc::new(|_, _| Err("handler failed".to_string()))),
            ..RunnerConfig::default()
        })
        .expect("runner config");

        let err = runner
            .run_component(CAPABILITY_INVOKE_COMPONENT.as_bytes())
            .expect_err("handler error should trap");

        assert!(matches!(err, Error::Run(_)));
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

    #[test]
    fn interrupt_handle_stops_looping_component() {
        let runner = CodeModeRunner::with_config(RunnerConfig {
            fuel: u64::MAX,
            ..RunnerConfig::default()
        })
        .expect("runner config");
        let interrupt = runner.interrupt_handle();
        let run_interrupt = interrupt.clone();

        let run = std::thread::spawn(move || {
            runner.run_component_with_interrupt(LOOP_COMPONENT.as_bytes(), &run_interrupt)
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        interrupt.interrupt();

        let result = run.join().expect("runner thread");
        assert!(matches!(result, Err(Error::Run(_))));
    }

    #[test]
    fn foreign_interrupt_handle_is_rejected() {
        let runner = CodeModeRunner::new().expect("runner config");
        let foreign_runner = CodeModeRunner::new().expect("runner config");
        let foreign_interrupt = foreign_runner.interrupt_handle();

        assert!(matches!(
            runner
                .run_component_with_interrupt(NOOP_COMPONENT.as_bytes(), &foreign_interrupt),
            Err(Error::Run(source)) if source.to_string() == "interrupt handle belongs to a different code-mode runner"
        ));
    }
}
