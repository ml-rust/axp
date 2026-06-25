//! Linux seccomp-bpf syscall filtering. Builds a seccomp filter and compiles it
//! to a BPF program in the PARENT (where allocation is safe), then installs a
//! `pre_exec` hook that calls `apply_filter` in the CHILD after fork / before
//! exec, confining only the child (never the daemon).
//!
//! # Policy
//! Default-ALLOW with a targeted DENY set: every syscall NOT listed is allowed
//! (`mismatch_action = Allow`), and the listed syscalls return `-EPERM`
//! (`match_action = Errno(EPERM)`). `-EPERM` is a graceful, debuggable,
//! Docker-style denial (NOT `KillProcess`), so a program that probes e.g.
//! io_uring sees a clean error and can fall back instead of being killed.
//!
//! Denied syscalls: `ptrace`, `process_vm_readv`, `process_vm_writev` (block
//! cross-process memory inspection / debugging of other processes), and
//! `io_uring_setup`, `io_uring_enter`, `io_uring_register` (block the io_uring
//! ring, a recurring sandbox-escape surface that bypasses syscall auditing).
//!
//! # Scope (explicit, not hidden)
//! This filter does NOT gate `allow_proc_spawn` or `allow_network`: `execve`,
//! `clone`, and `socket` are intentionally NOT in the deny set. The child must
//! `execve` its own program, and gating `clone` would break threads. Those
//! capabilities remain unenforced-by-seccomp here and are handled by a later
//! unit / tier.
//!
//! # Safety
//! The `pre_exec` closure runs post-fork, pre-exec in the child, so it must be
//! async-signal-safe. It performs ONLY the `apply_filter` syscalls (prctl +
//! seccomp) on a BPF program fully built before the fork, and it is
//! allocation-free on every path: it borrows the already-built `prog` (moved
//! into the closure) and on error returns an `io::Error` built from a bare
//! `ErrorKind` (no formatted or boxed message). It holds no locks and touches
//! no shared state.
use seccompiler::{
    BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch, apply_filter,
};
use std::collections::BTreeMap;

use crate::SandboxError;

/// Syscalls denied (returned `-EPERM`) by the seccomp filter. Each becomes an
/// empty-rule map entry, i.e. it matches unconditionally and gets `match_action`.
//  `libc::SYS_*` are `c_long` — `i64` on 64-bit targets (where the cast is a
//  no-op) but `i32` on 32-bit, where widening to the `BTreeMap<i64, _>` key type
//  is required. The cast is therefore portability, not redundancy.
#[allow(clippy::unnecessary_cast)]
fn denied_syscalls() -> [i64; 6] {
    [
        libc::SYS_ptrace as i64,
        libc::SYS_process_vm_readv as i64,
        libc::SYS_process_vm_writev as i64,
        libc::SYS_io_uring_setup as i64,
        libc::SYS_io_uring_enter as i64,
        libc::SYS_io_uring_register as i64,
    ]
}

/// Build a seccomp-bpf filter denying a fixed set of syscalls and install it via
/// a `pre_exec` hook that runs in the child after fork.
///
/// The filter is fully compiled to a [`BpfProgram`] in the parent (where
/// allocation is safe); only `apply_filter` runs in the child. The deny set is
/// policy-independent, so this takes only `cmd`. Returns [`SandboxError::Apply`]
/// if the filter cannot be built/compiled or the target arch is unknown.
pub(crate) fn apply(cmd: &mut tokio::process::Command) -> Result<(), SandboxError> {
    // Resolve the target architecture in the parent. Failure means seccompiler
    // does not know this host's arch — fail loudly.
    let target_arch: TargetArch =
        std::env::consts::ARCH
            .try_into()
            .map_err(|e| SandboxError::Apply {
                reason: format!(
                    "seccomp unsupported target arch {}: {e:?}",
                    std::env::consts::ARCH
                ),
            })?;

    // Each denied syscall is an empty-rule entry: it matches unconditionally and
    // therefore receives `match_action` (Errno EPERM).
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for syscall in denied_syscalls() {
        rules.insert(syscall, vec![]);
    }

    // mismatch_action (not in map) = Allow; match_action (in map) = -EPERM. The
    // two actions MUST differ (the constructor validates this) and they do.
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        target_arch,
    )
    .map_err(|e| SandboxError::Apply {
        reason: format!("seccomp filter build failed: {e}"),
    })?;

    // Compile to a BPF program in the parent (allocation happens here).
    let prog: BpfProgram = filter.try_into().map_err(|e| SandboxError::Apply {
        reason: format!("seccomp BPF compile failed: {e}"),
    })?;

    // SAFETY: the closure only calls apply_filter (prctl + seccomp) on a BPF
    // program built in the parent before fork; it borrows the already-compiled
    // `prog` (moved into the closure), holds no locks, touches no shared state,
    // and allocates nothing on any path — the error arm returns an `io::Error`
    // from a bare `ErrorKind` (no formatted message), so it stays
    // async-signal-safe even when apply_filter fails.
    unsafe {
        cmd.pre_exec(move || match apply_filter(&prog) {
            Ok(()) => Ok(()),
            Err(_) => Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On this Linux host the filter must build and the `pre_exec` hook must
    /// install without error for a fresh command (no child is spawned here).
    #[test]
    fn apply_installs_filter_ok() {
        let mut cmd = tokio::process::Command::new("true");
        assert!(
            apply(&mut cmd).is_ok(),
            "seccomp filter build + hook install should succeed on Linux"
        );
    }

    /// The deny set is six distinct syscalls (no accidental dup or omission).
    /// The specific identities live in `denied_syscalls`; re-listing them here
    /// would only mirror that constant, so we pin the invariants instead.
    #[test]
    fn denied_set_is_six_distinct_syscalls() {
        let mut denied = denied_syscalls();
        assert_eq!(denied.len(), 6);
        denied.sort_unstable();
        for pair in denied.windows(2) {
            assert_ne!(pair[0], pair[1], "duplicate syscall in deny set");
        }
    }
}
