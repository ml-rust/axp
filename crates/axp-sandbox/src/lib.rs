//! AXP sandbox backends. The Linux backend enforces a [`SandboxPolicy`] via
//! Landlock (+ seccomp in a later unit) applied to a child process; other
//! platforms are no-ops or refusals. (Enforcement is added incrementally —
//! this unit is the scaffolding.)
pub use axp_proto::EnforcementTier;

mod error;
mod policy;
mod probe;

pub use error::{Result, SandboxError};
pub use policy::SandboxPolicy;
pub use probe::landlock_available;
