//! Sandbox error type.
use crate::EnforcementTier;

/// Errors from configuring or applying a sandbox policy.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SandboxError {
    /// The requested enforcement tier cannot be honored on this host
    /// (unsupported tier, kernel too old, Landlock unavailable). The engine must
    /// FAIL the job rather than silently downgrade.
    #[error("sandbox tier {tier:?} unavailable: {reason}")]
    Unavailable {
        /// The tier that could not be honored.
        tier: EnforcementTier,
        /// Human-readable reason.
        reason: String,
    },
    /// Applying the sandbox to the child process failed.
    #[error("failed to apply sandbox: {reason}")]
    Apply {
        /// Human-readable reason.
        reason: String,
    },
}

/// Convenience alias.
pub type Result<T, E = SandboxError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_display_contains_tier_and_reason() {
        let err = SandboxError::Unavailable {
            tier: EnforcementTier::KernelLsm,
            reason: "kernel too old".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("KernelLsm"), "missing tier: {msg}");
        assert!(msg.contains("kernel too old"), "missing reason: {msg}");
    }

    #[test]
    fn apply_display_contains_reason() {
        let err = SandboxError::Apply {
            reason: "pre_exec failed".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("pre_exec failed"), "missing reason: {msg}");
    }
}
