//! Provider trait and built-in `NativeProvider` for the runtime discovery layer.
//!
//! # Contract
//!
//! - [`Provider::index`] **must be cheap**: it returns only name + one-line description and
//!   must not perform any I/O, schema work, or blocking operations.
//! - [`Provider::describe`] **may be heavier/lazy**: it is called on-demand for a single
//!   capability and is allowed to do additional work (e.g. remote fetches in other
//!   implementations), but must never panic.
//! - [`Provider::resolve`] converts a capability invocation (local name + JSON params) into
//!   a [`ResolvedCommand`] (program + argv). Params are bound positionally — never shell
//!   interpolated — so invocation is free of shell injection.
//! - No method may panic; all return [`crate::Result`].

use std::collections::HashMap;

use crate::{Error, Result};

/// How a [`NativeProvider`] capability is executed: a program plus an argument template.
///
/// Params are bound into argv slots by [`NativeProvider::resolve`] — NEVER interpolated
/// into a shell string — so capability invocation is free of shell injection.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionSpec {
    /// Executable name/path, passed to `Command::new` (not via `sh -c`).
    pub program: String,
    /// Argument template; each slot is a literal string or a named param reference.
    pub args_template: Vec<CapabilityArg>,
}

/// One slot in an [`ExecutionSpec`] argument template.
#[derive(Debug, Clone, PartialEq)]
pub enum CapabilityArg {
    /// A fixed argument string.
    Literal(String),
    /// A reference to a named field in the invocation's JSON params object. The field
    /// **must be a JSON string value** (`"like_this"`); non-string types (numbers,
    /// booleans, objects, arrays, null) are rejected with [`Error::CapabilityParse`]
    /// at resolve time — no coercion or stringification is performed.
    Param(String),
}

/// A fully-resolved command: program + concrete argv, ready for `Command::new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCommand {
    /// Executable name/path.
    pub program: String,
    /// Concrete argument vector (all params already bound in).
    pub args: Vec<String>,
}

/// A cheap catalog entry returned by [`Provider::index`]: local name + one-line description.
///
/// The name is **local** to the provider; the [`crate::ProviderRegistry`] applies namespacing
/// when a collision is detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityListing {
    /// Local capability name within this provider (e.g. `"git_diff"`).
    pub name: String,
    /// One-line human-readable description (no newlines).
    pub desc: String,
}

/// Full provider-side descriptor used to construct a [`NativeProvider`].
///
/// This is **not** returned by [`Provider::index`] (that remains cheap).  Only
/// [`Provider::describe`] exposes the schema, via [`axp_proto::CapabilityDetail`].
/// [`Provider::resolve`] uses `exec` to build a concrete argv without shell expansion.
#[derive(Debug, Clone, PartialEq)]
pub struct CapabilityDescriptor {
    /// Local capability name within this provider.
    pub name: String,
    /// One-line human-readable description.
    pub desc: String,
    /// TypeScript-style signature string, e.g. `git_diff(): string`.
    pub signature: String,
    /// JSON Schema 2020-12 for the capability's parameters.
    pub schema: serde_json::Value,
    /// Execution spec: program + argument template used by [`Provider::resolve`].
    pub exec: ExecutionSpec,
}

/// A runtime capability provider.
///
/// Implementors must be `Send + Sync + 'static` so they can be stored in the
/// [`crate::ProviderRegistry`] and shared across threads.
///
/// # Contract
///
/// - [`Provider::index`] is **cheap**: no I/O, no schema work.
/// - [`Provider::describe`] may be heavier or lazy but must never panic.
/// - Both methods return [`Result`]; neither panics.
pub trait Provider: Send + Sync + 'static {
    /// Stable unique identifier for this provider, used as a namespace (e.g. `"native"`).
    fn id(&self) -> &str;

    /// Returns a cheap catalog of all capabilities: local name + one-line description only.
    ///
    /// No schema computation or I/O should occur here.
    fn index(&self) -> Result<Vec<CapabilityListing>>;

    /// Returns full detail for the capability identified by its **local** name (no provider prefix).
    ///
    /// Returns [`Error::CapabilityNotFound`] if `local_name` is not known to this provider.
    /// May perform heavier work (e.g. lazy resolution) but must never panic.
    fn describe(&self, local_name: &str) -> Result<axp_proto::CapabilityDetail>;

    /// Resolve a capability invocation to a concrete argv command.
    ///
    /// `params` is the invocation's JSON params object. Returns
    /// [`Error::CapabilityNotFound`] if the local name is unknown, or
    /// [`Error::CapabilityParse`] if a referenced param is missing or not a string.
    /// Every referenced param is validated before `Ok` is returned — no partial binding.
    fn resolve(&self, local_name: &str, params: &serde_json::Value) -> Result<ResolvedCommand>;
}

/// A synchronous, in-memory [`Provider`] backed by a pre-built map of [`CapabilityDescriptor`]s.
///
/// `NativeProvider` holds descriptors in a `HashMap` keyed by local name for O(1) `describe`
/// lookups.  `index` projects each stored descriptor to a [`CapabilityListing`] (name + desc
/// only; signature and schema are omitted).
pub struct NativeProvider {
    /// Stable identifier for this provider instance.
    id: String,
    /// Descriptors keyed by local name for O(1) lookup.
    descriptors: HashMap<String, CapabilityDescriptor>,
}

impl NativeProvider {
    /// Creates a new `NativeProvider` with the given `id` and capability `descriptors`.
    ///
    /// Returns [`Error::DuplicateCapability`] if two descriptors share a local name: a
    /// duplicate is rejected at construction rather than silently dropped, so a typo can
    /// never make a capability vanish unnoticed.
    pub fn new(id: impl Into<String>, descriptors: Vec<CapabilityDescriptor>) -> Result<Self> {
        let id = id.into();
        let mut map: HashMap<String, CapabilityDescriptor> =
            HashMap::with_capacity(descriptors.len());
        for descriptor in descriptors {
            if map.contains_key(&descriptor.name) {
                return Err(Error::DuplicateCapability {
                    provider: id,
                    name: descriptor.name,
                });
            }
            map.insert(descriptor.name.clone(), descriptor);
        }
        Ok(Self {
            id,
            descriptors: map,
        })
    }
}

impl Provider for NativeProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn index(&self) -> Result<Vec<CapabilityListing>> {
        Ok(self
            .descriptors
            .values()
            .map(|d| CapabilityListing {
                name: d.name.clone(),
                desc: d.desc.clone(),
            })
            .collect())
    }

    fn describe(&self, local_name: &str) -> Result<axp_proto::CapabilityDetail> {
        self.descriptors
            .get(local_name)
            .map(|d| axp_proto::CapabilityDetail {
                signature: d.signature.clone(),
                schema: d.schema.clone(),
            })
            .ok_or_else(|| Error::CapabilityNotFound {
                name: local_name.to_owned(),
            })
    }

    fn resolve(&self, local_name: &str, params: &serde_json::Value) -> Result<ResolvedCommand> {
        let descriptor =
            self.descriptors
                .get(local_name)
                .ok_or_else(|| Error::CapabilityNotFound {
                    name: local_name.to_owned(),
                })?;

        let mut args = Vec::with_capacity(descriptor.exec.args_template.len());
        for slot in &descriptor.exec.args_template {
            match slot {
                CapabilityArg::Literal(s) => args.push(s.clone()),
                CapabilityArg::Param(field) => {
                    let value = params.get(field).and_then(|v| v.as_str()).ok_or_else(|| {
                        Error::CapabilityParse {
                            raw: field.clone(),
                            reason: "missing or non-string capability param".into(),
                        }
                    })?;
                    args.push(value.to_owned());
                }
            }
        }

        Ok(ResolvedCommand {
            program: descriptor.exec.program.clone(),
            args,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_provider(id: &str, caps: &[(&str, &str, &str)]) -> NativeProvider {
        let descriptors = caps
            .iter()
            .map(|(name, desc, sig)| CapabilityDescriptor {
                name: name.to_string(),
                desc: desc.to_string(),
                signature: sig.to_string(),
                schema: serde_json::json!({"type": "object", "properties": {}}),
                exec: ExecutionSpec {
                    program: "true".into(),
                    args_template: vec![],
                },
            })
            .collect();
        NativeProvider::new(id, descriptors).expect("test descriptors have unique names")
    }

    #[test]
    fn index_lists_all_capabilities() {
        let provider = make_provider(
            "native",
            &[
                (
                    "git_diff",
                    "Show uncommitted changes as a patch",
                    "git_diff(): string",
                ),
                (
                    "git_log",
                    "Show recent commit history for a repo",
                    "git_log(): string",
                ),
            ],
        );

        let mut listings = provider.index().unwrap();
        listings.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(listings.len(), 2);
        assert_eq!(listings[0].name, "git_diff");
        assert_eq!(listings[0].desc, "Show uncommitted changes as a patch");
        assert_eq!(listings[1].name, "git_log");
        assert_eq!(listings[1].desc, "Show recent commit history for a repo");
    }

    #[test]
    fn index_does_not_include_signature_or_schema() {
        let provider = make_provider(
            "native",
            &[(
                "git_diff",
                "Show uncommitted changes as a patch",
                "git_diff(): string",
            )],
        );
        let listings = provider.index().unwrap();
        // CapabilityListing only has name and desc fields — no signature/schema
        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].name, "git_diff");
    }

    #[test]
    fn describe_known_capability_returns_correct_detail() {
        let provider = NativeProvider::new(
            "native",
            vec![CapabilityDescriptor {
                name: "git_diff".to_string(),
                desc: "Show uncommitted changes as a patch".to_string(),
                signature: "git_diff(): string".to_string(),
                schema: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
                exec: ExecutionSpec {
                    program: "git".into(),
                    args_template: vec![CapabilityArg::Literal("diff".into())],
                },
            }],
        )
        .expect("unique descriptor");

        let detail = provider.describe("git_diff").unwrap();
        assert_eq!(detail.signature, "git_diff(): string");
        assert_eq!(
            detail.schema,
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}})
        );
    }

    #[test]
    fn describe_unknown_capability_returns_capability_not_found() {
        let provider = make_provider(
            "native",
            &[(
                "git_diff",
                "Show uncommitted changes as a patch",
                "git_diff(): string",
            )],
        );

        let err = provider.describe("nonexistent").unwrap_err();
        assert!(
            matches!(err, crate::Error::CapabilityNotFound { ref name } if name == "nonexistent"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn provider_id_is_correct() {
        let provider = make_provider("my_provider", &[]);
        assert_eq!(provider.id(), "my_provider");
    }

    // ── resolve tests ─────────────────────────────────────────────────────────

    fn git_diff_provider() -> NativeProvider {
        NativeProvider::new(
            "native",
            vec![CapabilityDescriptor {
                name: "git_diff".to_string(),
                desc: "Show uncommitted changes as a patch".to_string(),
                signature: "git_diff(): string".to_string(),
                schema: serde_json::json!({"type": "object", "properties": {}}),
                exec: ExecutionSpec {
                    program: "git".into(),
                    args_template: vec![CapabilityArg::Literal("diff".into())],
                },
            }],
        )
        .expect("unique descriptor")
    }

    fn path_cat_provider() -> NativeProvider {
        NativeProvider::new(
            "native",
            vec![CapabilityDescriptor {
                name: "cat_file".to_string(),
                desc: "Print the contents of the named file".to_string(),
                signature: "cat_file(path: string): string".to_string(),
                schema: serde_json::json!({"type":"object","properties":{"path":{"type":"string"}},"additionalProperties":false}),
                exec: ExecutionSpec {
                    program: "cat".into(),
                    args_template: vec![CapabilityArg::Param("path".into())],
                },
            }],
        )
        .expect("unique descriptor")
    }

    #[test]
    fn resolve_literal_only_returns_correct_program_and_args() {
        let provider = git_diff_provider();
        let cmd = provider
            .resolve("git_diff", &serde_json::json!({}))
            .expect("resolve must succeed");
        assert_eq!(cmd.program, "git");
        assert_eq!(cmd.args, vec!["diff"]);
    }

    #[test]
    fn resolve_param_binding_substitutes_string_field() {
        let provider = path_cat_provider();
        let cmd = provider
            .resolve("cat_file", &serde_json::json!({"path": "/tmp/foo.txt"}))
            .expect("resolve must succeed");
        assert_eq!(cmd.program, "cat");
        assert_eq!(cmd.args, vec!["/tmp/foo.txt"]);
    }

    #[test]
    fn resolve_missing_param_returns_capability_parse() {
        let provider = path_cat_provider();
        let err = provider
            .resolve("cat_file", &serde_json::json!({}))
            .unwrap_err();
        assert!(
            matches!(err, crate::Error::CapabilityParse { ref raw, .. } if raw == "path"),
            "expected CapabilityParse for missing param, got: {err}"
        );
    }

    #[test]
    fn resolve_non_string_param_returns_capability_parse() {
        let provider = path_cat_provider();
        let err = provider
            .resolve("cat_file", &serde_json::json!({"path": 42}))
            .unwrap_err();
        assert!(
            matches!(err, crate::Error::CapabilityParse { ref raw, .. } if raw == "path"),
            "expected CapabilityParse for non-string param, got: {err}"
        );
    }

    #[test]
    fn resolve_unknown_local_name_returns_capability_not_found() {
        let provider = git_diff_provider();
        let err = provider
            .resolve("nonexistent", &serde_json::json!({}))
            .unwrap_err();
        assert!(
            matches!(err, crate::Error::CapabilityNotFound { ref name } if name == "nonexistent"),
            "expected CapabilityNotFound, got: {err}"
        );
    }
}
