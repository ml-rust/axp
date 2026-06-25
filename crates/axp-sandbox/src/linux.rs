//! Linux Landlock enforcement. Builds a Landlock ruleset in the PARENT (where
//! allocation is safe) and installs a `pre_exec` hook that calls `restrict_self`
//! in the CHILD after fork / before exec, confining only the child (never the daemon).
//!
//! # Safety
//! The `pre_exec` closure runs post-fork, pre-exec in the child, so it must be
//! async-signal-safe. It performs ONLY the `restrict_self` syscalls (prctl +
//! landlock_restrict_self) on a ruleset fully built before the fork, and it is
//! allocation-free on every path: `restrict_self` returns a small `Copy` status
//! (no heap), and the error paths return an `io::Error` built from a bare
//! `ErrorKind` (no formatted or boxed message). It holds no locks and touches no
//! shared state.
use std::path::Path;

use landlock::{
    ABI, AccessFs, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreated, RulesetCreatedAttr,
};

use crate::{SandboxError, SandboxPolicy};

/// Baseline system paths granted read+execute so programs (shells, coreutils,
/// shared libraries, the dynamic loader) can actually load and run inside the
/// sandbox. Best-effort: any path that does not exist on this host is skipped.
const BASELINE_READ_PATHS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/etc",
    "/dev/null",
    "/dev/zero",
    "/dev/urandom",
    "/proc",
];

/// Add a single `PathBeneath` rule to the ruleset.
///
/// `required` controls failure mode when the path cannot be opened:
/// - `true` (policy-granted paths): fail closed with [`SandboxError::Apply`].
/// - `false` (baseline best-effort paths): skip silently (systems vary).
fn add_path_rule(
    ruleset: RulesetCreated,
    path: &Path,
    rights: BitFlags<AccessFs>,
    required: bool,
) -> Result<RulesetCreated, SandboxError> {
    let path_fd = match PathFd::new(path) {
        Ok(fd) => fd,
        Err(e) => {
            if required {
                return Err(SandboxError::Apply {
                    reason: format!(
                        "cannot open granted path {} for landlock: {e}",
                        path.display()
                    ),
                });
            }
            // Baseline path absent on this host: skip it.
            return Ok(ruleset);
        }
    };
    // Landlock rejects directory-only rights (e.g. `ReadDir`) applied to a
    // non-directory path. Mask the requested rights to those valid for THIS
    // path's file type: a directory keeps the full set (the kernel applies the
    // appropriate subset to each entry beneath it); a regular/special file
    // (e.g. `/dev/null`) keeps only the file-applicable rights. This only
    // narrows the rights granted on this path — it never widens access.
    let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
    let effective = if is_dir {
        rights
    } else {
        rights & (AccessFs::Execute | AccessFs::ReadFile | AccessFs::WriteFile)
    };
    ruleset
        .add_rule(PathBeneath::new(path_fd, effective))
        .map_err(|e| SandboxError::Apply {
            reason: format!("landlock add_rule for {} failed: {e}", path.display()),
        })
}

/// Build a Landlock ruleset confining `cmd` to `policy`'s allowed paths and
/// install it via a `pre_exec` hook that runs in the child after fork.
///
/// The ruleset is fully constructed in the parent (where allocation is safe);
/// only `restrict_self` runs in the child. Returns [`SandboxError::Unavailable`]
/// (no silent downgrade) if Landlock is not supported by the running kernel.
pub(crate) fn enforce(
    policy: &SandboxPolicy,
    cmd: &mut tokio::process::Command,
) -> Result<(), SandboxError> {
    // Availability gate: fail loudly rather than silently no-op.
    if !crate::landlock_available() {
        return Err(SandboxError::Unavailable {
            tier: policy.tier,
            reason: "Landlock not supported by this kernel".into(),
        });
    }

    // Rights at the V1 ABI floor. `from_read` covers read + execute + readdir
    // (what is needed to read files and to load/execute programs); `from_write`
    // covers write/create/remove. landlock 0.4.
    let read_rights = AccessFs::from_read(ABI::V1);
    let write_rights = AccessFs::from_write(ABI::V1);

    // Build the ruleset in the parent. It must `handle_access` the UNION of all
    // rights it governs. `HardRequirement` => fail rather than silently weaken.
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(read_rights | write_rights)
        .map_err(|e| SandboxError::Apply {
            reason: format!("landlock handle_access failed: {e}"),
        })?
        .create()
        .map_err(|e| SandboxError::Apply {
            reason: format!("landlock ruleset create failed: {e}"),
        })?;

    // Baseline system read+execute allowlist so the child can load and exec.
    for raw in BASELINE_READ_PATHS {
        ruleset = add_path_rule(ruleset, Path::new(raw), read_rights, false)?;
    }

    // Policy-granted paths are required: a granted path that cannot be opened is
    // a loud, fail-closed error. Verbs are orthogonal — read paths get read
    // rights, write paths get write rights only.
    for path in &policy.fs_read {
        ruleset = add_path_rule(ruleset, path, read_rights, true)?;
    }
    for path in &policy.fs_write {
        ruleset = add_path_rule(ruleset, path, write_rights, true)?;
    }

    // `restrict_self` consumes the `RulesetCreated` by value, but `pre_exec`
    // takes an `FnMut`, so move it into an `Option` and `take()` it on first run.
    let mut ruleset = Some(ruleset);

    // SAFETY: the closure only calls restrict_self (prctl + landlock_restrict_self)
    // on a ruleset built in the parent before fork; it holds no locks, touches no
    // shared state, and allocates nothing on any path — the error arms return an
    // `io::Error` from a bare `ErrorKind` (no formatted message), so it stays
    // async-signal-safe even when restrict_self fails.
    unsafe {
        cmd.pre_exec(move || match ruleset.take() {
            Some(r) => match r.restrict_self() {
                Ok(_) => Ok(()),
                Err(_) => Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            },
            // Unreachable in practice (the hook runs once per spawn); fail closed
            // without allocating or panicking (a post-fork panic is signal-unsafe).
            None => Err(std::io::Error::from(std::io::ErrorKind::Other)),
        });
    }
    Ok(())
}
