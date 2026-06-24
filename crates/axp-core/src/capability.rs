//! Runtime capability model.
//!
//! # Security contract
//!
//! **Verbs are orthogonal** — a `fs.write` grant does NOT imply a `fs.read`
//! grant.  Agents that need both must hold both grants explicitly.
//!
//! **Attenuation only narrows, never broadens.**  A child capability derived
//! from a parent grant can only cover an equal or smaller scope: a
//! sub-path (component-boundary containment via [`Path::starts_with`]) or a
//! sub-domain (dot-suffix matching).  It can never cross verb boundaries.
//!
//! **Tool namespacing** is currently exact-match only.  Future revisions may
//! introduce hierarchical tool namespaces (e.g. `tool:git/*`), but no such
//! semantics are inferred today; `Tool("git")` does NOT permit `Tool("gitx")`.

use std::path::{Component, PathBuf};

use crate::{Error, Result};

// ── Known verb prefixes ───────────────────────────────────────────────────────
// Any string that starts with one of these prefixes MUST conform to the full
// grammar for that verb.  If it starts with the prefix but is malformed, we
// return `CapabilityParse` rather than silently falling through to the
// bare-name `Tool` branch.
const PREFIX_FS_READ: &str = "fs.read(";
const PREFIX_FS_WRITE: &str = "fs.write(";
const PREFIX_NET_CONNECT: &str = "net.connect(";

/// A structured, runtime representation of a single capability grant.
///
/// Parsed from the wire-format string defined by the AXP spec grammar:
/// `fs.read(<abs-path>)`, `fs.write(<abs-path>)`, `net.connect(<domain>)`,
/// `proc.spawn`, `pty.open`, `tool:<name>`, or a bare tool name.
///
/// After parsing, the canonical string form can be recovered via [`Display`].
/// The canonical form for a bare-name input is `tool:<name>` — that is
/// intentional and ensures idempotence: `parse(display(x)) == x`.
///
/// [`Display`]: std::fmt::Display
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RuntimeCapability {
    /// Read access to a filesystem subtree.
    FsRead(PathBuf),
    /// Write access to a filesystem subtree.
    FsWrite(PathBuf),
    /// Outbound TCP/UDP to a domain (and its sub-domains via attenuation).
    NetConnect(String),
    /// Permission to spawn child processes.
    ProcSpawn,
    /// Permission to open a pseudo-terminal.
    PtyOpen,
    /// Permission to invoke a named tool.
    Tool(String),
}

impl RuntimeCapability {
    /// Parse a [`axp_proto::Capability`] wire value into a [`RuntimeCapability`].
    ///
    /// Delegates to [`parse_str`][Self::parse_str] after borrowing the inner string.
    pub fn parse(wire: &axp_proto::Capability) -> Result<Self> {
        Self::parse_str(wire.as_str())
    }

    /// Parse the AXP capability grammar string `s` into a [`RuntimeCapability`].
    ///
    /// # Strict verb-prefix rule
    ///
    /// If `s` begins with a known verb prefix (`fs.read(`, `fs.write(`, or
    /// `net.connect(`) the string MUST be fully well-formed for that verb.  A
    /// malformed match (e.g. `fs.read(/proj` with a missing closing paren)
    /// returns [`Error::CapabilityParse`] and does NOT fall through to the
    /// bare-name `Tool` branch.  This prevents silent misclassification of
    /// typos as tool names.
    pub fn parse_str(s: &str) -> Result<Self> {
        // Helper closure so we can return a consistent error.
        let err = |reason: &str| Error::CapabilityParse {
            raw: s.to_owned(),
            reason: reason.to_owned(),
        };

        if s.starts_with(PREFIX_FS_READ) {
            let inner = extract_paren_inner(s, PREFIX_FS_READ)
                .ok_or_else(|| err("expected closing `)` for fs.read(...)"))?;
            let path = parse_abs_path(inner, s)?;
            return Ok(RuntimeCapability::FsRead(path));
        }

        if s.starts_with(PREFIX_FS_WRITE) {
            let inner = extract_paren_inner(s, PREFIX_FS_WRITE)
                .ok_or_else(|| err("expected closing `)` for fs.write(...)"))?;
            let path = parse_abs_path(inner, s)?;
            return Ok(RuntimeCapability::FsWrite(path));
        }

        if s.starts_with(PREFIX_NET_CONNECT) {
            let inner = extract_paren_inner(s, PREFIX_NET_CONNECT)
                .ok_or_else(|| err("expected closing `)` for net.connect(...)"))?;
            let domain = parse_domain(inner, s)?;
            return Ok(RuntimeCapability::NetConnect(domain));
        }

        if s == "proc.spawn" {
            return Ok(RuntimeCapability::ProcSpawn);
        }

        if s == "pty.open" {
            return Ok(RuntimeCapability::PtyOpen);
        }

        if let Some(name) = s.strip_prefix("tool:") {
            if name.is_empty() {
                return Err(err("tool name must not be empty after `tool:`"));
            }
            return Ok(RuntimeCapability::Tool(name.to_owned()));
        }

        // Bare-name fallback: ONLY if the string does not start with a known
        // verb prefix (already handled above), is non-empty, and contains no
        // whitespace.
        if s.is_empty() {
            return Err(err("empty capability string"));
        }
        if s.chars().any(|c| c.is_whitespace()) {
            return Err(err("bare tool name must not contain whitespace"));
        }
        Ok(RuntimeCapability::Tool(s.to_owned()))
    }

    /// Return the wire-format [`axp_proto::Capability`] for this capability.
    ///
    /// The result is the canonical string form as defined by `Display`.
    pub fn to_wire(&self) -> axp_proto::Capability {
        axp_proto::Capability(self.to_string())
    }

    /// Returns `true` if `self` (a parent grant) authorises minting `child`.
    ///
    /// The child holds **no more** authority than the parent; this method
    /// checks that invariant.
    ///
    /// # Rules
    ///
    /// - `FsRead(pp)` permits `FsRead(cp)` iff `cp` is under `pp`
    ///   (component-boundary containment).
    /// - `FsWrite(pp)` permits `FsWrite(cp)` iff `cp` is under `pp`.
    ///   A write grant does **not** permit a read grant (verbs are orthogonal).
    /// - `NetConnect(pd)` permits `NetConnect(cd)` iff `cd == pd` **or**
    ///   `cd` ends with `".<pd>"` (dot-suffix sub-domain check).
    /// - `ProcSpawn` permits only `ProcSpawn`.
    /// - `PtyOpen` permits only `PtyOpen`.
    /// - `Tool(pn)` permits `Tool(cn)` iff `pn == cn` (exact match).
    /// - All other combinations return `false`.
    pub fn permits(&self, child: &RuntimeCapability) -> bool {
        match (self, child) {
            (RuntimeCapability::FsRead(pp), RuntimeCapability::FsRead(cp)) => cp.starts_with(pp),
            (RuntimeCapability::FsWrite(pp), RuntimeCapability::FsWrite(cp)) => cp.starts_with(pp),
            (RuntimeCapability::NetConnect(pd), RuntimeCapability::NetConnect(cd)) => {
                cd == pd
                    || cd
                        .strip_suffix(pd.as_str())
                        .is_some_and(|p| p.ends_with('.'))
            }
            (RuntimeCapability::ProcSpawn, RuntimeCapability::ProcSpawn) => true,
            (RuntimeCapability::PtyOpen, RuntimeCapability::PtyOpen) => true,
            (RuntimeCapability::Tool(pn), RuntimeCapability::Tool(cn)) => pn == cn,
            _ => false,
        }
    }
}

impl std::fmt::Display for RuntimeCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeCapability::FsRead(p) => write!(f, "fs.read({})", p.display()),
            RuntimeCapability::FsWrite(p) => write!(f, "fs.write({})", p.display()),
            RuntimeCapability::NetConnect(d) => write!(f, "net.connect({d})"),
            RuntimeCapability::ProcSpawn => write!(f, "proc.spawn"),
            RuntimeCapability::PtyOpen => write!(f, "pty.open"),
            RuntimeCapability::Tool(n) => write!(f, "tool:{n}"),
        }
    }
}

// ── A set of capability grants ────────────────────────────────────────────────

/// An ordered collection of [`RuntimeCapability`] grants held by a session.
///
/// A request is permitted if ANY grant in the set [`permits`](RuntimeCapability::permits)
/// the requested capability.
#[derive(Debug, Clone, Default)]
pub struct CapabilitySet {
    grants: Vec<RuntimeCapability>,
}

impl CapabilitySet {
    /// Create a `CapabilitySet` from a pre-parsed list of grants.
    pub fn new(grants: Vec<RuntimeCapability>) -> Self {
        Self { grants }
    }

    /// Parse a slice of wire-format [`axp_proto::Capability`] values.
    ///
    /// Returns an error at the first malformed entry.
    pub fn from_wire(wire: &[axp_proto::Capability]) -> Result<Self> {
        let grants = wire
            .iter()
            .map(RuntimeCapability::parse)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { grants })
    }

    /// The ordered list of grants held by this set.
    pub fn grants(&self) -> &[RuntimeCapability] {
        &self.grants
    }

    /// Returns `true` if any grant in this set permits `requested`.
    pub fn permits(&self, requested: &RuntimeCapability) -> bool {
        self.grants.iter().any(|g| g.permits(requested))
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Given that `s` starts with `prefix` and should end with `)`, extract the
/// inner content.  Returns `None` if the string does not end with `)`.
fn extract_paren_inner<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let after_prefix = s.strip_prefix(prefix)?;
    after_prefix.strip_suffix(')')
}

/// Parse and validate an absolute path component from a capability string.
fn parse_abs_path(raw: &str, original: &str) -> Result<PathBuf> {
    let err = |reason: &str| Error::CapabilityParse {
        raw: original.to_owned(),
        reason: reason.to_owned(),
    };

    if !raw.starts_with('/') {
        return Err(err("path must be absolute (start with `/`)"));
    }

    let path = PathBuf::from(raw);

    // Reject any `..` (ParentDir) components — they break containment logic.
    for component in path.components() {
        if component == Component::ParentDir {
            return Err(err("path must not contain `..` components"));
        }
    }

    Ok(path)
}

/// Validate a domain string from a `net.connect(...)` capability.
fn parse_domain(raw: &str, original: &str) -> Result<String> {
    let err = |reason: &str| Error::CapabilityParse {
        raw: original.to_owned(),
        reason: reason.to_owned(),
    };

    if raw.is_empty() {
        return Err(err("domain must not be empty"));
    }
    if raw.contains('/') {
        return Err(err("domain must not contain `/`"));
    }
    if raw.chars().any(|c| c.is_whitespace()) {
        return Err(err("domain must not contain whitespace"));
    }
    if raw.contains("://") {
        return Err(err(
            "domain must not contain `://` (bare domain expected, not a URL)",
        ));
    }
    if raw.contains(':') {
        return Err(err(
            "port notation not supported (use bare domain without `:port`)",
        ));
    }

    Ok(raw.to_owned())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Parsing round-trip ────────────────────────────────────────────────────

    #[test]
    fn parse_fs_read() {
        let cap = RuntimeCapability::parse_str("fs.read(/proj)").unwrap();
        assert_eq!(cap, RuntimeCapability::FsRead(PathBuf::from("/proj")));
    }

    #[test]
    fn parse_fs_write() {
        let cap = RuntimeCapability::parse_str("fs.write(/var/data)").unwrap();
        assert_eq!(cap, RuntimeCapability::FsWrite(PathBuf::from("/var/data")));
    }

    #[test]
    fn parse_net_connect() {
        let cap = RuntimeCapability::parse_str("net.connect(example.com)").unwrap();
        assert_eq!(cap, RuntimeCapability::NetConnect("example.com".to_owned()));
    }

    #[test]
    fn parse_proc_spawn() {
        let cap = RuntimeCapability::parse_str("proc.spawn").unwrap();
        assert_eq!(cap, RuntimeCapability::ProcSpawn);
    }

    #[test]
    fn parse_pty_open() {
        let cap = RuntimeCapability::parse_str("pty.open").unwrap();
        assert_eq!(cap, RuntimeCapability::PtyOpen);
    }

    #[test]
    fn parse_tool_with_prefix() {
        let cap = RuntimeCapability::parse_str("tool:git").unwrap();
        assert_eq!(cap, RuntimeCapability::Tool("git".to_owned()));
    }

    #[test]
    fn parse_bare_name_becomes_tool() {
        let cap = RuntimeCapability::parse_str("myeditor").unwrap();
        assert_eq!(cap, RuntimeCapability::Tool("myeditor".to_owned()));
    }

    #[test]
    fn display_fs_read() {
        let cap = RuntimeCapability::FsRead(PathBuf::from("/proj"));
        assert_eq!(cap.to_string(), "fs.read(/proj)");
    }

    #[test]
    fn display_fs_write() {
        let cap = RuntimeCapability::FsWrite(PathBuf::from("/var"));
        assert_eq!(cap.to_string(), "fs.write(/var)");
    }

    #[test]
    fn display_net_connect() {
        let cap = RuntimeCapability::NetConnect("example.com".to_owned());
        assert_eq!(cap.to_string(), "net.connect(example.com)");
    }

    #[test]
    fn display_proc_spawn() {
        assert_eq!(RuntimeCapability::ProcSpawn.to_string(), "proc.spawn");
    }

    #[test]
    fn display_pty_open() {
        assert_eq!(RuntimeCapability::PtyOpen.to_string(), "pty.open");
    }

    #[test]
    fn display_tool() {
        let cap = RuntimeCapability::Tool("git".to_owned());
        assert_eq!(cap.to_string(), "tool:git");
    }

    /// Bare-name input canonicalizes to `tool:<name>` so that
    /// `parse(display(x)) == x` (canonical idempotence).
    #[test]
    fn bare_name_canonical_idempotence() {
        let original = RuntimeCapability::parse_str("myeditor").unwrap();
        let wire = original.to_wire();
        let reparsed = RuntimeCapability::parse(&wire).unwrap();
        assert_eq!(original, reparsed);
    }

    #[test]
    fn round_trip_all_variants() {
        let caps = vec![
            RuntimeCapability::FsRead(PathBuf::from("/proj/src")),
            RuntimeCapability::FsWrite(PathBuf::from("/tmp")),
            RuntimeCapability::NetConnect("api.example.com".to_owned()),
            RuntimeCapability::ProcSpawn,
            RuntimeCapability::PtyOpen,
            RuntimeCapability::Tool("cargo".to_owned()),
        ];
        for cap in caps {
            let reparsed = RuntimeCapability::parse(&cap.to_wire()).unwrap();
            assert_eq!(cap, reparsed, "round-trip failed for {cap}");
        }
    }

    // ── Parse errors ─────────────────────────────────────────────────────────

    #[test]
    fn parse_err_relative_path() {
        assert!(
            RuntimeCapability::parse_str("fs.read(relative/path)").is_err(),
            "relative path must be rejected"
        );
    }

    #[test]
    fn parse_err_dotdot_path() {
        assert!(
            RuntimeCapability::parse_str("fs.read(/a/../b)").is_err(),
            "`..` components must be rejected"
        );
    }

    /// Malformed verb prefix (missing closing paren) must NOT fall through to
    /// the bare-name `Tool` branch — it must be an error.
    #[test]
    fn parse_err_malformed_fs_read_not_tool() {
        let input = "fs.read(/proj";
        let result = RuntimeCapability::parse_str(input);
        assert!(
            result.is_err(),
            "malformed fs.read (missing `)`) must be Err, not Tool"
        );
        match result {
            Err(Error::CapabilityParse { raw, .. }) => {
                // The raw field must echo back the input string verbatim.
                assert_eq!(raw, input);
            }
            other => panic!("expected CapabilityParse error for malformed fs.read, got {other:?}"),
        }
    }

    #[test]
    fn parse_err_net_connect_url_scheme() {
        assert!(
            RuntimeCapability::parse_str("net.connect(http://x)").is_err(),
            "URL with scheme must be rejected"
        );
    }

    #[test]
    fn parse_err_net_connect_port_notation() {
        assert!(
            RuntimeCapability::parse_str("net.connect(host:443)").is_err(),
            "port notation must be rejected"
        );
    }

    #[test]
    fn parse_err_net_connect_empty_domain() {
        assert!(
            RuntimeCapability::parse_str("net.connect()").is_err(),
            "empty domain must be rejected"
        );
    }

    #[test]
    fn parse_err_tool_empty_name() {
        assert!(
            RuntimeCapability::parse_str("tool:").is_err(),
            "empty tool name must be rejected"
        );
    }

    #[test]
    fn parse_err_empty_string() {
        assert!(
            RuntimeCapability::parse_str("").is_err(),
            "empty string must be rejected"
        );
    }

    #[test]
    fn parse_err_whitespace_only() {
        assert!(
            RuntimeCapability::parse_str("  ").is_err(),
            "whitespace-only string must be rejected"
        );
    }

    // ── Attenuation — path containment ───────────────────────────────────────

    #[test]
    fn fs_read_permits_subdirectory() {
        let parent = RuntimeCapability::FsRead(PathBuf::from("/proj"));
        let child = RuntimeCapability::FsRead(PathBuf::from("/proj/sub"));
        assert!(parent.permits(&child));
    }

    #[test]
    fn fs_read_permits_equal_path() {
        let cap = RuntimeCapability::FsRead(PathBuf::from("/proj"));
        assert!(cap.permits(&cap));
    }

    #[test]
    fn fs_read_does_not_permit_broader_path() {
        let parent = RuntimeCapability::FsRead(PathBuf::from("/proj"));
        let child = RuntimeCapability::FsRead(PathBuf::from("/"));
        assert!(
            !parent.permits(&child),
            "child broader than parent must be denied"
        );
    }

    /// Critical prefix-string trap: `/proj-extra` must NOT be contained in `/proj`.
    #[test]
    fn fs_read_does_not_permit_sibling_with_shared_string_prefix() {
        let parent = RuntimeCapability::FsRead(PathBuf::from("/proj"));
        let trap = RuntimeCapability::FsRead(PathBuf::from("/proj-extra"));
        assert!(
            !parent.permits(&trap),
            "/proj-extra must NOT be permitted by /proj (prefix-string trap)"
        );
    }

    /// Verbs are orthogonal: an `fs.write` grant must NOT permit an `fs.read`.
    #[test]
    fn fs_write_does_not_permit_fs_read() {
        let write_grant = RuntimeCapability::FsWrite(PathBuf::from("/proj"));
        let read_request = RuntimeCapability::FsRead(PathBuf::from("/proj/sub"));
        assert!(
            !write_grant.permits(&read_request),
            "write grant must not permit read (verbs are orthogonal)"
        );
    }

    /// Verbs are orthogonal: an `fs.read` grant must NOT permit an `fs.write`.
    #[test]
    fn fs_read_does_not_permit_fs_write() {
        let read_grant = RuntimeCapability::FsRead(PathBuf::from("/proj"));
        let write_request = RuntimeCapability::FsWrite(PathBuf::from("/proj/sub"));
        assert!(
            !read_grant.permits(&write_request),
            "read grant must not permit write (verbs are orthogonal)"
        );
    }

    // ── Attenuation — domain sub-domain matching ──────────────────────────────

    #[test]
    fn net_connect_permits_subdomain() {
        let parent = RuntimeCapability::NetConnect("example.com".to_owned());
        let child = RuntimeCapability::NetConnect("api.example.com".to_owned());
        assert!(parent.permits(&child));
    }

    #[test]
    fn net_connect_permits_equal_domain() {
        let cap = RuntimeCapability::NetConnect("example.com".to_owned());
        assert!(cap.permits(&cap));
    }

    /// Critical suffix trap: `evil-example.com` must NOT match `example.com`.
    #[test]
    fn net_connect_does_not_permit_suffix_trap() {
        let parent = RuntimeCapability::NetConnect("example.com".to_owned());
        let trap = RuntimeCapability::NetConnect("evil-example.com".to_owned());
        assert!(
            !parent.permits(&trap),
            "evil-example.com must NOT be permitted by example.com (suffix trap)"
        );
    }

    #[test]
    fn net_connect_does_not_permit_tld_only() {
        let parent = RuntimeCapability::NetConnect("example.com".to_owned());
        let tld = RuntimeCapability::NetConnect("com".to_owned());
        assert!(
            !parent.permits(&tld),
            "bare TLD must not be permitted by example.com"
        );
    }

    // ── Attenuation — other verbs ─────────────────────────────────────────────

    #[test]
    fn proc_spawn_permits_proc_spawn() {
        assert!(RuntimeCapability::ProcSpawn.permits(&RuntimeCapability::ProcSpawn));
    }

    #[test]
    fn proc_spawn_does_not_permit_pty_open() {
        assert!(
            !RuntimeCapability::ProcSpawn.permits(&RuntimeCapability::PtyOpen),
            "ProcSpawn must not permit PtyOpen"
        );
    }

    #[test]
    fn pty_open_permits_pty_open() {
        assert!(RuntimeCapability::PtyOpen.permits(&RuntimeCapability::PtyOpen));
    }

    #[test]
    fn tool_git_permits_tool_git() {
        let cap = RuntimeCapability::Tool("git".to_owned());
        assert!(cap.permits(&cap));
    }

    #[test]
    fn tool_git_does_not_permit_tool_gitx() {
        let parent = RuntimeCapability::Tool("git".to_owned());
        let other = RuntimeCapability::Tool("gitx".to_owned());
        assert!(
            !parent.permits(&other),
            "Tool(\"git\") must not permit Tool(\"gitx\")"
        );
    }

    // ── CapabilitySet ─────────────────────────────────────────────────────────

    #[test]
    fn capability_set_permits_via_any_grant() {
        let set = CapabilitySet::new(vec![
            RuntimeCapability::FsRead(PathBuf::from("/proj")),
            RuntimeCapability::ProcSpawn,
        ]);

        // Permitted by FsRead grant
        assert!(set.permits(&RuntimeCapability::FsRead(PathBuf::from("/proj/a"))));
        // Permitted by ProcSpawn grant
        assert!(set.permits(&RuntimeCapability::ProcSpawn));
        // Not permitted by either grant
        assert!(
            !set.permits(&RuntimeCapability::FsWrite(PathBuf::from("/proj"))),
            "FsWrite not in set"
        );
    }

    #[test]
    fn capability_set_from_wire_parses_all() {
        let wire = vec![
            axp_proto::Capability("fs.read(/proj)".to_owned()),
            axp_proto::Capability("proc.spawn".to_owned()),
        ];
        let set = CapabilitySet::from_wire(&wire).unwrap();
        assert_eq!(set.grants().len(), 2);
    }

    #[test]
    fn capability_set_from_wire_errors_on_malformed() {
        let wire = vec![
            axp_proto::Capability("fs.read(/proj)".to_owned()),
            axp_proto::Capability("".to_owned()), // malformed
        ];
        assert!(
            CapabilitySet::from_wire(&wire).is_err(),
            "malformed entry must cause from_wire to fail"
        );
    }

    #[test]
    fn capability_set_empty_denies_all() {
        let set = CapabilitySet::default();
        assert!(!set.permits(&RuntimeCapability::ProcSpawn));
        assert!(!set.permits(&RuntimeCapability::FsRead(PathBuf::from("/"))));
    }
}
