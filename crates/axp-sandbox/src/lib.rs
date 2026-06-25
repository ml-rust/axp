//! AXP sandbox backends. The Linux backend enforces a [`SandboxPolicy`] via
//! Landlock (+ seccomp in a later unit) applied to a child process; other
//! platforms are no-ops or refusals.
//!
//! # `unsafe` policy
//! This is the ONE crate in the project permitted to use `unsafe`. The only
//! `unsafe` is the `pre_exec` post-fork hook used to install Landlock
//! confinement on the child (see [`linux`]). It is kept minimal and fully
//! contained in the Linux module; the closure performs only the
//! `restrict_self` syscalls on a ruleset built before the fork.
pub use axp_proto::EnforcementTier;

mod error;
#[cfg(target_os = "linux")]
mod linux;
mod policy;
mod probe;

pub use error::{Result, SandboxError};
pub use policy::SandboxPolicy;
pub use probe::landlock_available;
