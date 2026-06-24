//! Runtime workspace type.
//!
//! A [`Workspace`] is a canonical, absolute directory path that serves as the
//! root of an agent's filesystem access scope.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// The root directory of an agent's isolated workspace.
///
/// The path stored inside is always absolute and canonical (symlinks resolved).
/// Construction via [`Workspace::new`] performs the canonicalization; all other
/// methods can be called infallibly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Create a new `Workspace` from `path`.
    ///
    /// The path is resolved with [`std::fs::canonicalize`] so that symlinks are
    /// expanded and the stored value is always absolute.  If the path does not
    /// exist or the calling process lacks the necessary permissions, an
    /// [`Error::InvalidWorkspace`] is returned rather than panicking.
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let original = path.as_ref().to_path_buf();
        let canonical = std::fs::canonicalize(&original).map_err(|e| Error::InvalidWorkspace {
            path: original.clone(),
            reason: e.to_string(),
        })?;

        // Defensive check: canonicalize should always produce an absolute path,
        // but verify to catch hypothetical platform quirks.
        if !canonical.is_absolute() {
            return Err(Error::InvalidWorkspace {
                path: original,
                reason: "canonicalized path is not absolute".to_owned(),
            });
        }

        Ok(Self { root: canonical })
    }

    /// The canonical, absolute path to the workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns `true` if `path` is inside (or equal to) this workspace root.
    ///
    /// Uses [`Path::starts_with`] which matches on full path components, so
    /// `/proj-extra` is NOT considered to be inside `/proj`.
    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn new_succeeds_for_existing_dir() {
        let dir = make_dir();
        let ws = Workspace::new(dir.path()).expect("workspace from existing dir");
        assert!(ws.root().is_absolute());
    }

    #[test]
    fn root_matches_canonical_path() {
        let dir = make_dir();
        let ws = Workspace::new(dir.path()).unwrap();
        // tempfile may itself return a symlinked path on some systems; canonical
        // should match canonical.
        let expected = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(ws.root(), expected);
    }

    #[test]
    fn contains_root_itself() {
        let dir = make_dir();
        let ws = Workspace::new(dir.path()).unwrap();
        assert!(ws.contains(ws.root()));
    }

    #[test]
    fn contains_subdirectory() {
        let dir = make_dir();
        let ws = Workspace::new(dir.path()).unwrap();
        let sub = ws.root().join("subdir").join("nested");
        assert!(ws.contains(&sub));
    }

    #[test]
    fn does_not_contain_unrelated_path() {
        let dir = make_dir();
        let ws = Workspace::new(dir.path()).unwrap();
        assert!(!ws.contains(Path::new("/etc/passwd")));
    }

    #[test]
    fn does_not_contain_sibling_with_shared_prefix() {
        // /tmp/axpXXX is the workspace root; /tmp/axpXXX-extra must NOT be
        // considered contained — the prefix-string trap.
        let dir = make_dir();
        let ws = Workspace::new(dir.path()).unwrap();
        // Build a sibling path that shares the directory name as a string prefix
        // but is a different component.
        let sibling_name = format!(
            "{}-extra",
            dir.path().file_name().unwrap().to_string_lossy()
        );
        let sibling = dir.path().parent().unwrap().join(sibling_name);
        assert!(!ws.contains(&sibling));
    }

    #[test]
    fn new_on_nonexistent_path_returns_err() {
        let result = Workspace::new("/this/path/does/not/exist/axp_test_12345");
        assert!(
            result.is_err(),
            "expected Err for non-existent path, got Ok"
        );
        match result {
            Err(Error::InvalidWorkspace { .. }) => {}
            other => panic!("expected InvalidWorkspace, got {other:?}"),
        }
    }
}
