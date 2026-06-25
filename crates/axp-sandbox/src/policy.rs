//! Backend-agnostic sandbox policy.
use crate::{EnforcementTier, SandboxError};
use std::path::PathBuf;

/// A neutral, backend-agnostic description of what a sandboxed child may do.
///
/// Built by the runtime (which owns the capability model) from a session's
/// workspace + capability set + tier, then handed to [`SandboxPolicy::apply`]
/// to configure the child process. This type deliberately holds only plain
/// primitives (paths + flags + tier) so `axp-sandbox` need not depend on the
/// runtime's capability types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// The enforcement tier requested for this job.
    pub tier: EnforcementTier,
    /// Absolute, canonical paths the child may READ (subtree access).
    pub fs_read: Vec<PathBuf>,
    /// Absolute, canonical paths the child may WRITE (subtree access).
    pub fs_write: Vec<PathBuf>,
    /// Whether the child may spawn further processes.
    pub allow_proc_spawn: bool,
    /// Whether the child may create network sockets (coarse; domain allowlisting
    /// via the egress proxy is a later unit).
    pub allow_network: bool,
}

impl SandboxPolicy {
    /// Construct a policy from already-decomposed primitives.
    pub fn from_parts(
        tier: EnforcementTier,
        fs_read: Vec<PathBuf>,
        fs_write: Vec<PathBuf>,
        allow_proc_spawn: bool,
        allow_network: bool,
    ) -> Self {
        Self {
            tier,
            fs_read,
            fs_write,
            allow_proc_spawn,
            allow_network,
        }
    }

    /// A do-nothing policy at the `DevNone` tier (no enforcement).
    pub fn dev_none() -> Self {
        Self {
            tier: EnforcementTier::DevNone,
            fs_read: Vec::new(),
            fs_write: Vec::new(),
            allow_proc_spawn: true,
            allow_network: true,
        }
    }

    /// Apply this policy to a child command before it is spawned.
    ///
    /// - `DevNone` → no-op, `Ok(())`.
    /// - `KernelLsm` → on Linux, installs real Landlock filesystem enforcement on
    ///   `cmd` via a `pre_exec` hook (see `crate::linux::enforce`). If Landlock is
    ///   unavailable it returns `Err(Unavailable)` — it does NOT silently no-op,
    ///   which would be false security. On non-Linux it returns `Err(Unavailable)`.
    /// - `Container` / `ProcessToken` → `Err(Unavailable)` (not supported on this backend).
    pub fn apply(&self, cmd: &mut tokio::process::Command) -> Result<(), SandboxError> {
        match self.tier {
            EnforcementTier::DevNone => Ok(()),
            EnforcementTier::KernelLsm => {
                #[cfg(target_os = "linux")]
                {
                    crate::linux::enforce(self, cmd)
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = cmd;
                    Err(SandboxError::Unavailable {
                        tier: self.tier,
                        reason: "kernel-lsm enforcement requires Linux".into(),
                    })
                }
            }
            EnforcementTier::Container | EnforcementTier::ProcessToken => {
                Err(SandboxError::Unavailable {
                    tier: self.tier,
                    reason: "tier not supported by the Linux backend".into(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parts_round_trips_fields() {
        let read = vec![PathBuf::from("/ws/src")];
        let write = vec![PathBuf::from("/ws/out")];
        let policy = SandboxPolicy::from_parts(
            EnforcementTier::KernelLsm,
            read.clone(),
            write.clone(),
            true,
            false,
        );
        assert_eq!(policy.tier, EnforcementTier::KernelLsm);
        assert_eq!(policy.fs_read, read);
        assert_eq!(policy.fs_write, write);
        assert!(policy.allow_proc_spawn);
        assert!(!policy.allow_network);
    }

    #[test]
    fn dev_none_has_dev_none_tier() {
        let policy = SandboxPolicy::dev_none();
        assert_eq!(policy.tier, EnforcementTier::DevNone);
    }

    #[test]
    fn apply_dev_none_is_ok() {
        let mut cmd = tokio::process::Command::new("true");
        let policy = SandboxPolicy::dev_none();
        assert!(policy.apply(&mut cmd).is_ok());
    }

    #[test]
    fn apply_kernel_lsm_matches_landlock_availability() {
        let mut cmd = tokio::process::Command::new("true");
        let policy = SandboxPolicy::from_parts(
            EnforcementTier::KernelLsm,
            Vec::new(),
            Vec::new(),
            false,
            false,
        );
        let result = policy.apply(&mut cmd);
        if cfg!(target_os = "linux") && crate::landlock_available() {
            // Landlock present: enforcement is installed, so apply succeeds.
            assert!(
                result.is_ok(),
                "expected Ok with Landlock available, got {result:?}"
            );
        } else {
            // No Landlock (or non-Linux): must fail loudly, never silently no-op.
            match result {
                Err(SandboxError::Unavailable { tier, .. }) => {
                    assert_eq!(tier, EnforcementTier::KernelLsm);
                }
                other => panic!("expected Unavailable, got {other:?}"),
            }
        }
    }

    #[test]
    fn apply_container_is_unavailable() {
        let mut cmd = tokio::process::Command::new("true");
        let policy = SandboxPolicy::from_parts(
            EnforcementTier::Container,
            Vec::new(),
            Vec::new(),
            false,
            false,
        );
        assert!(matches!(
            policy.apply(&mut cmd),
            Err(SandboxError::Unavailable { .. })
        ));
    }

    #[test]
    fn apply_process_token_is_unavailable() {
        let mut cmd = tokio::process::Command::new("true");
        let policy = SandboxPolicy::from_parts(
            EnforcementTier::ProcessToken,
            Vec::new(),
            Vec::new(),
            false,
            false,
        );
        assert!(matches!(
            policy.apply(&mut cmd),
            Err(SandboxError::Unavailable { .. })
        ));
    }
}
