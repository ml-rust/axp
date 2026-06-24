//! [`ProviderRegistry`] — aggregates [`Provider`]s and serves the runtime discovery catalog.
//!
//! The registry caches name+desc at registration time (via [`Provider::index`]) so that
//! [`ProviderRegistry::index`] can be answered entirely from an in-memory cache without
//! calling any provider.  [`ProviderRegistry::describe`] routes on-demand to the owning
//! provider.
//!
//! # Collision-correct exposure
//!
//! When a single local name is exposed by exactly **one** provider it appears under its bare
//! local name (e.g. `"search"`).  When **two or more** providers expose the same local name,
//! every occurrence is qualified as `"<provider_id>:<local_name>"` (e.g. `"a:search"`).

use std::collections::{HashMap, HashSet};

use crate::{Error, Result, provider::Provider};

/// Minimum allowed length (in bytes) for a capability description.
const DESC_MIN_LEN: usize = 10;
/// Maximum allowed length (in bytes) for a capability description.
const DESC_MAX_LEN: usize = 120;

/// Internal catalog entry mapping an exposed name back to its owning provider and local name.
struct CatalogEntry {
    /// The provider id that owns this capability.
    provider_id: String,
    /// The local name within the owning provider.
    local_name: String,
    /// Cached one-line description.
    desc: String,
}

/// Aggregates multiple [`Provider`]s and serves the unified runtime discovery catalog.
///
/// The registry enforces:
/// - No duplicate provider ids.
/// - No duplicate local names within a single provider.
/// - Description quality constraints on every capability description.
/// - Collision-correct namespacing when multiple providers share a local name.
///
/// All mutation happens through [`register`](Self::register); once a registration is rejected
/// the registry is left **completely unchanged** (validate-then-mutate).
pub struct ProviderRegistry {
    /// Registered providers keyed by their id.
    providers: HashMap<String, Box<dyn Provider>>,
    /// Exposed catalog: maps the canonical exposed name → [`CatalogEntry`].
    catalog: HashMap<String, CatalogEntry>,
    /// Tracks how many providers currently expose each local name (for collision detection).
    name_counts: HashMap<String, usize>,
}

impl ProviderRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            catalog: HashMap::new(),
            name_counts: HashMap::new(),
        }
    }

    /// Registers a provider with the registry.
    ///
    /// This method:
    /// 1. Rejects duplicate provider ids ([`Error::DuplicateProvider`]).
    /// 2. Calls [`Provider::index`] once to snapshot the catalog.
    /// 3. Rejects within-provider duplicate capability names ([`Error::DuplicateCapability`]).
    /// 4. Validates every capability description ([`Error::DescriptionQuality`]).
    /// 5. Updates the catalog with collision-correct namespacing.
    ///
    /// If any step fails the registry is left **unchanged**.
    pub fn register(&mut self, provider: Box<dyn Provider>) -> Result<()> {
        let id = provider.id().to_owned();

        // Step 1: reject duplicate provider id.
        if self.providers.contains_key(&id) {
            return Err(Error::DuplicateProvider { id });
        }

        // Step 2: snapshot the catalog from the provider.
        let listings = provider.index()?;

        // Step 3: within-provider duplicate name check.
        {
            let mut seen: HashSet<&str> = HashSet::new();
            for listing in &listings {
                if !seen.insert(listing.name.as_str()) {
                    return Err(Error::DuplicateCapability {
                        provider: id,
                        name: listing.name.clone(),
                    });
                }
            }
        }

        // Step 4: validate all descriptions before mutating anything.
        for listing in &listings {
            check_description_quality(&id, &listing.name, &listing.desc)?;
        }

        // Step 5: all validations passed — now mutate catalog and name_counts.
        //
        // Build temporary maps so that we can commit atomically (any panic here would be a
        // logic error, not a user error, but defensive construction is still cleaner).
        //
        // Collision-correct promotion logic:
        //  count == 1  → bare name (first owner)
        //  count == 2  → promote prior bare entry to qualified; add new as qualified
        //  count >= 3  → add new as qualified only
        for listing in &listings {
            let n = &listing.name;
            let count = self.name_counts.entry(n.clone()).or_insert(0);
            *count += 1;
            let qualified = format!("{id}:{n}");

            let make_entry = || CatalogEntry {
                provider_id: id.clone(),
                local_name: n.clone(),
                desc: listing.desc.clone(),
            };

            match *count {
                1 => {
                    self.catalog.insert(n.clone(), make_entry());
                }
                2 => {
                    // Promote the previous bare entry to its qualified form.
                    if let Some(prev) = self.catalog.remove(n) {
                        let prev_qualified = format!("{}:{}", prev.provider_id, prev.local_name);
                        self.catalog.insert(prev_qualified, prev);
                    }
                    // Insert the new provider's entry as qualified.
                    self.catalog.insert(qualified, make_entry());
                }
                _ => {
                    // Third provider onwards: always qualified, no promotion needed.
                    self.catalog.insert(qualified, make_entry());
                }
            }
        }

        self.providers.insert(id, provider);
        Ok(())
    }

    /// Returns the full capability catalog as an [`axp_proto::IndexResponse`].
    ///
    /// Served entirely from the in-memory cache — no providers are called.
    pub fn index(&self) -> Result<axp_proto::IndexResponse> {
        let entries = self
            .catalog
            .iter()
            .map(|(exposed_name, entry)| axp_proto::IndexEntry {
                name: exposed_name.clone(),
                desc: entry.desc.clone(),
            })
            .collect();
        Ok(axp_proto::IndexResponse { entries })
    }

    /// Returns full detail for the capability identified by its **exposed** catalog name.
    ///
    /// The `name` must match an entry in the catalog (e.g. `"search"` when unambiguous, or
    /// `"a:search"` when qualified).  Returns [`Error::CapabilityNotFound`] if not found.
    pub fn describe(&self, name: &str) -> Result<axp_proto::CapabilityDetail> {
        let entry = self
            .catalog
            .get(name)
            .ok_or_else(|| Error::CapabilityNotFound {
                name: name.to_owned(),
            })?;

        let provider =
            self.providers
                .get(&entry.provider_id)
                .ok_or_else(|| Error::CapabilityNotFound {
                    name: name.to_owned(),
                })?;

        provider.describe(&entry.local_name)
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Validates a capability description against quality constraints.
///
/// Hard failures (each returns [`Error::DescriptionQuality`]):
/// - Empty (after trimming).
/// - Shorter than [`DESC_MIN_LEN`] bytes.
/// - Longer than [`DESC_MAX_LEN`] bytes.
/// - Merely echoes the capability name (case-insensitive, with or without underscores).
/// - Contains newline characters (`\n` or `\r`).
fn check_description_quality(provider: &str, cap_name: &str, desc: &str) -> Result<()> {
    let trimmed = desc.trim();

    let make_err = |reason: String| Error::DescriptionQuality {
        provider: provider.to_owned(),
        capability: cap_name.to_owned(),
        reason,
    };

    if trimmed.is_empty() {
        return Err(make_err("description must not be empty".to_owned()));
    }

    if trimmed.len() < DESC_MIN_LEN {
        return Err(make_err(format!(
            "description is too short ({} chars); minimum is {DESC_MIN_LEN}",
            trimmed.len()
        )));
    }

    if trimmed.len() > DESC_MAX_LEN {
        return Err(make_err(format!(
            "description is too long ({} chars); maximum is {DESC_MAX_LEN}",
            trimmed.len()
        )));
    }

    let lower = trimmed.to_lowercase();
    let name_lower = cap_name.to_lowercase();
    let name_spaced = cap_name.replace('_', " ").to_lowercase();
    if lower == name_lower || lower == name_spaced {
        return Err(make_err(
            "description must not merely echo the capability name".to_owned(),
        ));
    }

    if trimmed.contains('\n') || trimmed.contains('\r') {
        return Err(make_err(
            "description must be a single line (no \\n or \\r)".to_owned(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{CapabilityDescriptor, CapabilityListing, NativeProvider};

    // ── helpers ──────────────────────────────────────────────────────────────

    fn make_descriptor(name: &str, desc: &str, sig: &str) -> CapabilityDescriptor {
        CapabilityDescriptor {
            name: name.to_string(),
            desc: desc.to_string(),
            signature: sig.to_string(),
            schema: serde_json::json!({"type": "object"}),
        }
    }

    fn native(id: &str, caps: &[(&str, &str, &str)]) -> Box<dyn Provider> {
        let descriptors = caps
            .iter()
            .map(|(n, d, s)| make_descriptor(n, d, s))
            .collect();
        Box::new(NativeProvider::new(id, descriptors).expect("test descriptors have unique names"))
    }

    fn sorted_names(registry: &ProviderRegistry) -> Vec<String> {
        let mut names: Vec<String> = registry
            .index()
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.name)
            .collect();
        names.sort();
        names
    }

    // ── single provider ───────────────────────────────────────────────────────

    #[test]
    fn single_provider_index_exposes_bare_names() {
        let mut reg = ProviderRegistry::new();
        reg.register(native(
            "native",
            &[
                (
                    "git_diff",
                    "Show uncommitted changes as a patch diff",
                    "git_diff(): string",
                ),
                (
                    "git_log",
                    "Show recent commit history for a repository",
                    "git_log(): string",
                ),
            ],
        ))
        .unwrap();

        let names = sorted_names(&reg);
        assert_eq!(names, vec!["git_diff", "git_log"]);
    }

    #[test]
    fn single_provider_describe_bare_name_works() {
        let mut reg = ProviderRegistry::new();
        reg.register(native(
            "native",
            &[(
                "git_diff",
                "Show uncommitted changes as a patch diff",
                "git_diff(): string",
            )],
        ))
        .unwrap();

        let detail = reg.describe("git_diff").unwrap();
        assert_eq!(detail.signature, "git_diff(): string");
    }

    #[test]
    fn single_provider_describe_unknown_returns_capability_not_found() {
        let mut reg = ProviderRegistry::new();
        reg.register(native(
            "native",
            &[(
                "git_diff",
                "Show uncommitted changes as a patch diff",
                "git_diff(): string",
            )],
        ))
        .unwrap();

        let err = reg.describe("nonexistent").unwrap_err();
        assert!(
            matches!(err, Error::CapabilityNotFound { ref name } if name == "nonexistent"),
            "unexpected error: {err}"
        );
    }

    // ── 2-provider collision ──────────────────────────────────────────────────

    #[test]
    fn two_provider_collision_qualifies_both_and_unique_caps_stay_bare() {
        let mut reg = ProviderRegistry::new();
        reg.register(native(
            "a",
            &[
                (
                    "search",
                    "Search across all indexed documents in provider A",
                    "search(q: string): string",
                ),
                (
                    "fetch_a",
                    "Fetch a resource from provider A endpoint",
                    "fetch_a(): string",
                ),
            ],
        ))
        .unwrap();
        reg.register(native(
            "b",
            &[
                (
                    "search",
                    "Search across all indexed documents in provider B",
                    "search(q: string): string",
                ),
                (
                    "fetch_b",
                    "Fetch a resource from provider B endpoint",
                    "fetch_b(): string",
                ),
            ],
        ))
        .unwrap();

        let names = sorted_names(&reg);

        // Qualified collision entries must be present.
        assert!(
            names.contains(&"a:search".to_string()),
            "missing a:search in {names:?}"
        );
        assert!(
            names.contains(&"b:search".to_string()),
            "missing b:search in {names:?}"
        );

        // Bare "search" must NOT be present (it was promoted away).
        assert!(
            !names.contains(&"search".to_string()),
            "bare 'search' must not appear in {names:?}"
        );

        // Unique caps must remain bare.
        assert!(
            names.contains(&"fetch_a".to_string()),
            "missing fetch_a in {names:?}"
        );
        assert!(
            names.contains(&"fetch_b".to_string()),
            "missing fetch_b in {names:?}"
        );
    }

    #[test]
    fn two_provider_collision_describe_routes_to_correct_provider() {
        let mut reg = ProviderRegistry::new();
        reg.register(native(
            "a",
            &[(
                "search",
                "Search across all indexed documents in provider A",
                "search_a(): string",
            )],
        ))
        .unwrap();
        reg.register(native(
            "b",
            &[(
                "search",
                "Search across all indexed documents in provider B",
                "search_b(): string",
            )],
        ))
        .unwrap();

        let detail_a = reg.describe("a:search").unwrap();
        assert_eq!(detail_a.signature, "search_a(): string");

        let detail_b = reg.describe("b:search").unwrap();
        assert_eq!(detail_b.signature, "search_b(): string");
    }

    // ── 3-provider collision ──────────────────────────────────────────────────

    #[test]
    fn three_provider_collision_all_qualified_no_bare() {
        let mut reg = ProviderRegistry::new();
        for id in ["a", "b", "c"] {
            reg.register(native(
                id,
                &[(
                    "search",
                    "Search across all indexed documents in this provider",
                    "search(q: string): string",
                )],
            ))
            .unwrap();
        }

        let names = sorted_names(&reg);

        assert!(
            names.contains(&"a:search".to_string()),
            "missing a:search in {names:?}"
        );
        assert!(
            names.contains(&"b:search".to_string()),
            "missing b:search in {names:?}"
        );
        assert!(
            names.contains(&"c:search".to_string()),
            "missing c:search in {names:?}"
        );
        assert!(
            !names.contains(&"search".to_string()),
            "bare 'search' must not appear with 3 providers in {names:?}"
        );
    }

    // ── within-provider duplicate name ─────────────────────────────────────────

    #[test]
    fn within_provider_duplicate_name_returns_duplicate_capability() {
        // NativeProvider's HashMap collapses duplicate names before `index()`, so a
        // within-provider duplicate can only be surfaced by a provider that returns both
        // listings from `index()` directly — hence the custom `DuplicateNameProvider`.
        let mut reg = ProviderRegistry::new();
        let err = reg.register(Box::new(DuplicateNameProvider)).unwrap_err();
        assert!(
            matches!(err, Error::DuplicateCapability { ref provider, ref name }
                if provider == "dup_provider" && name == "dup_cap"),
            "unexpected error: {err}"
        );
    }

    // ── duplicate provider id ─────────────────────────────────────────────────

    #[test]
    fn duplicate_provider_id_returns_duplicate_provider() {
        let mut reg = ProviderRegistry::new();
        reg.register(native(
            "native",
            &[(
                "git_diff",
                "Show uncommitted changes as a patch diff",
                "git_diff(): string",
            )],
        ))
        .unwrap();

        let err = reg
            .register(native(
                "native",
                &[(
                    "git_log",
                    "Show recent commit history for a repository",
                    "git_log(): string",
                )],
            ))
            .unwrap_err();

        assert!(
            matches!(err, Error::DuplicateProvider { ref id } if id == "native"),
            "unexpected error: {err}"
        );
    }

    // ── description quality ───────────────────────────────────────────────────

    #[test]
    fn empty_description_returns_description_quality() {
        let mut reg = ProviderRegistry::new();
        let err = reg
            .register(native("p", &[("git_diff", "", "git_diff(): string")]))
            .unwrap_err();
        assert!(
            matches!(err, Error::DescriptionQuality { .. }),
            "expected DescriptionQuality, got: {err}"
        );
    }

    #[test]
    fn too_short_description_returns_description_quality() {
        // 5-char desc is below the 10-char minimum.
        let mut reg = ProviderRegistry::new();
        let err = reg
            .register(native("p", &[("git_diff", "Short", "git_diff(): string")]))
            .unwrap_err();
        assert!(
            matches!(err, Error::DescriptionQuality { .. }),
            "expected DescriptionQuality, got: {err}"
        );
    }

    #[test]
    fn too_long_description_returns_description_quality() {
        let long = "A".repeat(200); // 200 chars, exceeds 120-char max.
        let mut reg = ProviderRegistry::new();
        let err = reg
            .register(native("p", &[("git_diff", &long, "git_diff(): string")]))
            .unwrap_err();
        assert!(
            matches!(err, Error::DescriptionQuality { .. }),
            "expected DescriptionQuality, got: {err}"
        );
    }

    #[test]
    fn description_echoing_name_returns_description_quality() {
        // "git diff" is the underscore-replaced form of "git_diff".
        let mut reg = ProviderRegistry::new();
        let err = reg
            .register(native(
                "p",
                &[("git_diff", "git diff", "git_diff(): string")],
            ))
            .unwrap_err();
        assert!(
            matches!(err, Error::DescriptionQuality { .. }),
            "expected DescriptionQuality, got: {err}"
        );
    }

    #[test]
    fn multiline_description_returns_description_quality() {
        let mut reg = ProviderRegistry::new();
        let err = reg
            .register(native(
                "p",
                &[("git_diff", "Show diff\nwith newline", "git_diff(): string")],
            ))
            .unwrap_err();
        assert!(
            matches!(err, Error::DescriptionQuality { .. }),
            "expected DescriptionQuality, got: {err}"
        );
    }

    #[test]
    fn rejected_registration_leaves_registry_unchanged() {
        let mut reg = ProviderRegistry::new();

        // Register a valid provider first.
        reg.register(native(
            "native",
            &[(
                "git_diff",
                "Show uncommitted changes as a patch diff",
                "git_diff(): string",
            )],
        ))
        .unwrap();

        // Attempt to register a provider with a bad description — must fail.
        let result = reg.register(native("bad", &[("tool", "x", "tool(): void")]));
        assert!(result.is_err(), "expected registration to fail");

        // Registry must still contain only the original provider's entry.
        let names = sorted_names(&reg);
        assert_eq!(
            names,
            vec!["git_diff"],
            "registry was modified by rejected registration"
        );

        // The bad provider must not be reachable.
        let err = reg.describe("tool").unwrap_err();
        assert!(matches!(err, Error::CapabilityNotFound { .. }));
    }

    // ── custom provider for within-provider dup test ──────────────────────────

    /// A provider that intentionally returns two listings with the same name,
    /// exercising the registry's within-provider duplicate check.
    struct DuplicateNameProvider;

    impl Provider for DuplicateNameProvider {
        fn id(&self) -> &str {
            "dup_provider"
        }

        fn index(&self) -> Result<Vec<CapabilityListing>> {
            Ok(vec![
                CapabilityListing {
                    name: "dup_cap".to_string(),
                    desc: "First listing of the duplicated capability here".to_string(),
                },
                CapabilityListing {
                    name: "dup_cap".to_string(),
                    desc: "Second listing of the duplicated capability here".to_string(),
                },
            ])
        }

        fn describe(&self, local_name: &str) -> Result<axp_proto::CapabilityDetail> {
            Err(Error::CapabilityNotFound {
                name: local_name.to_owned(),
            })
        }
    }
}
