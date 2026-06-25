//! Landlock availability detection.
//!
//! A SAFE probe (no `unsafe`) that asks the running kernel whether it supports
//! the Landlock ABI floor AXP requires for `KernelLsm` enforcement. Used by the
//! engine to decide up front whether a `KernelLsm` job can be honored, so it can
//! FAIL rather than silently downgrade.

/// Returns true if the running kernel supports the Landlock ABI floor AXP
/// requires for `KernelLsm` enforcement. Always false on non-Linux.
#[cfg(target_os = "linux")]
pub fn landlock_available() -> bool {
    use landlock::{ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr};

    // Probe by building a ruleset at the required ABI floor (V1) under
    // `HardRequirement`: on a kernel that lacks Landlock the `create` syscall
    // fails, so a successful `create()` is a clean "supported at the floor"
    // signal. We deliberately do NOT call `restrict_self()` — creating the
    // ruleset fd asks the kernel whether the floor is supported without confining
    // this process.
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V1))
        .and_then(|r| r.create())
        .is_ok()
}

/// Returns true if the running kernel supports the Landlock ABI floor AXP
/// requires for `KernelLsm` enforcement. Always false on non-Linux.
#[cfg(not(target_os = "linux"))]
pub fn landlock_available() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landlock_available_returns_a_bool_without_panicking() {
        // The value is environment-dependent: on non-Linux it must be false; on
        // Linux it depends on the kernel/config, so we only assert it returns.
        let supported = landlock_available();
        #[cfg(not(target_os = "linux"))]
        assert!(!supported);
        #[cfg(target_os = "linux")]
        let _ = supported;
    }
}
