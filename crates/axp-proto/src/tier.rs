//! Sandbox enforcement tier.
use serde::{Deserialize, Serialize};

/// The sandbox enforcement guarantee a session runs under.
///
/// A session declares its tier so a client KNOWS the guarantee it is getting and can
/// refuse to run sensitive work under a weaker tier; the host cannot silently downgrade it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnforcementTier {
    /// Kernel-enforced isolation via an LSM (Linux Landlock/seccomp, macOS Seatbelt).
    KernelLsm,
    /// Container/namespace isolation.
    Container,
    /// OS process-token / restricted-token isolation (e.g. Windows AppContainer).
    ProcessToken,
    /// No enforcement — development only.
    DevNone,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_lsm_serializes_to_kebab_case() {
        let json = serde_json::to_string(&EnforcementTier::KernelLsm).unwrap();
        assert_eq!(json, r#""kernel-lsm""#);
    }

    #[test]
    fn container_serializes_to_kebab_case() {
        let json = serde_json::to_string(&EnforcementTier::Container).unwrap();
        assert_eq!(json, r#""container""#);
    }

    #[test]
    fn process_token_serializes_to_kebab_case() {
        let json = serde_json::to_string(&EnforcementTier::ProcessToken).unwrap();
        assert_eq!(json, r#""process-token""#);
    }

    #[test]
    fn dev_none_serializes_to_kebab_case() {
        let json = serde_json::to_string(&EnforcementTier::DevNone).unwrap();
        assert_eq!(json, r#""dev-none""#);
    }

    #[test]
    fn all_variants_round_trip() {
        let variants = [
            EnforcementTier::KernelLsm,
            EnforcementTier::Container,
            EnforcementTier::ProcessToken,
            EnforcementTier::DevNone,
        ];
        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let decoded: EnforcementTier = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, decoded);
        }
    }
}
