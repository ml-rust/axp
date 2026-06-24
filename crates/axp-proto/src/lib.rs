//! `axp-proto` — AXP wire-protocol payload types.
//!
//! This crate contains pure serde data types (no IO, no async) that define the
//! AXP protocol wire format. It is the lowest-level crate in the workspace and
//! has no internal AXP dependencies.
//!
//! All public types are re-exported at the crate root so they are reachable as
//! `axp_proto::TypeName`.

mod capability;
mod discovery;
mod ids;
mod job;
mod session;
mod tier;
mod version;

pub use capability::*;
pub use discovery::*;
pub use ids::*;
pub use job::*;
pub use session::*;
pub use tier::*;
pub use version::PROTOCOL_VERSION;
