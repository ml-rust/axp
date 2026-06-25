//! AXP sandbox backends. The Linux backend enforces a [`SandboxPolicy`] via
//! Landlock filesystem confinement plus a seccomp-bpf syscall filter applied to
//! a child process; other platforms are no-ops or refusals.
//!
//! # `unsafe` policy
//! This is the ONE crate in the project permitted to use `unsafe`. All `unsafe`
//! is confined to two `pre_exec` post-fork hooks installed on the child: one
//! installs Landlock confinement (`restrict_self`, see `linux`) and one installs
//! the seccomp-bpf filter (`apply_filter`, see `seccomp`). Both are kept minimal
//! and contained in their Linux modules; each closure performs only syscalls on
//! state built before the fork (a Landlock ruleset / a compiled BPF program),
//! holds no locks, and allocates nothing.
pub use axp_proto::EnforcementTier;

mod error;
#[cfg(target_os = "linux")]
mod linux;
mod policy;
mod probe;
#[cfg(target_os = "linux")]
mod seccomp;

pub use error::{Result, SandboxError};
pub use policy::SandboxPolicy;
pub use probe::landlock_available;
