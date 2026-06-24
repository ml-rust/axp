//! Sandbox enforcement tier — the guarantee a session declares.

/// The sandbox guarantee a session declares.
///
/// This is a placeholder enum; real OS-specific tiers land with the Linux
/// backend unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnforcementTier {
    /// No sandboxing — process runs with full ambient permissions.
    None,
    /// OS-level sandboxing — the precise mechanism depends on the host OS.
    Os,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforcement_tier_debug() {
        assert_eq!(format!("{:?}", EnforcementTier::None), "None");
        assert_eq!(format!("{:?}", EnforcementTier::Os), "Os");
    }

    #[test]
    fn enforcement_tier_equality() {
        assert_eq!(EnforcementTier::None, EnforcementTier::None);
        assert_ne!(EnforcementTier::None, EnforcementTier::Os);
    }
}
