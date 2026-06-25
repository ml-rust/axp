//! Built-in capability providers wired into every runtime by default.

use crate::{
    CapabilityDescriptor, NativeProvider, ProviderRegistry,
    provider::{CapabilityArg, ExecutionSpec},
};

/// Build a [`ProviderRegistry`] pre-populated with the built-in providers.
///
/// Used by the server/CLI/tests so discovery returns a consistent default
/// catalog. Built-ins are validated at construction; a malformed built-in is a
/// programming error, so this panics rather than returning a `Result`.
pub fn builtin_registry() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    registry
        .register(Box::new(native_provider()))
        .expect("built-in native provider must be valid");
    registry
}

/// The default in-process "native" provider. Descriptors carry name/desc/signature/schema
/// and an [`ExecutionSpec`] used by the engine's capability resolver.
fn native_provider() -> NativeProvider {
    let descriptors = vec![
        CapabilityDescriptor {
            name: "git_diff".to_string(),
            desc: "Show uncommitted working-tree changes as a unified diff".to_string(),
            signature: "git_diff(): string".to_string(),
            schema: serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
            exec: ExecutionSpec {
                program: "git".into(),
                args_template: vec![CapabilityArg::Literal("diff".into())],
            },
        },
        CapabilityDescriptor {
            name: "git_log".to_string(),
            desc: "List recent commits in compact one-line summary form".to_string(),
            signature: "git_log(): string".to_string(),
            schema: serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
            exec: ExecutionSpec {
                program: "git".into(),
                args_template: vec![
                    CapabilityArg::Literal("log".into()),
                    CapabilityArg::Literal("--oneline".into()),
                    CapabilityArg::Literal("-20".into()),
                ],
            },
        },
    ];
    NativeProvider::new("native", descriptors).expect("built-in capability names are unique")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    #[test]
    fn builtin_registry_index_contains_both_capabilities() {
        let registry = builtin_registry();
        let resp = registry.index().unwrap();
        let mut names: Vec<&str> = resp.entries.iter().map(|e| e.name.as_str()).collect();
        names.sort();
        assert!(
            names.contains(&"git_diff"),
            "expected git_diff in index: {names:?}"
        );
        assert!(
            names.contains(&"git_log"),
            "expected git_log in index: {names:?}"
        );
    }

    #[test]
    fn builtin_registry_describe_git_diff_returns_correct_signature() {
        let registry = builtin_registry();
        let detail = registry.describe("git_diff").unwrap();
        assert_eq!(detail.signature, "git_diff(): string");
    }

    #[test]
    fn builtin_registry_describe_nonexistent_returns_capability_not_found() {
        let registry = builtin_registry();
        let err = registry.describe("nonexistent").unwrap_err();
        assert!(
            matches!(err, Error::CapabilityNotFound { ref name } if name == "nonexistent"),
            "unexpected error: {err}"
        );
    }
}
