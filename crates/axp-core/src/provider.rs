//! Provider trait and built-in `NativeProvider` for the runtime discovery layer.
//!
//! # Contract
//!
//! - [`Provider::index`] **must be cheap**: it returns only name + one-line description and
//!   must not perform any I/O, schema work, or blocking operations.
//! - [`Provider::describe`] **may be heavier/lazy**: it is called on-demand for a single
//!   capability and is allowed to do additional work (e.g. remote fetches in other
//!   implementations), but must never panic.
//! - Neither method may panic; both return [`crate::Result`].
//!
//! # Future seam
//!
//! An `invoke` method will be added in the job-engine unit.  It is intentionally absent here.

use std::collections::HashMap;

use crate::{Error, Result};

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
}
