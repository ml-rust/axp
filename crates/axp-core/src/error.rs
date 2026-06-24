//! Crate-wide error type and `Result` alias for `axp-core`.

/// The top-level error type for `axp-core`.
///
/// Marked `#[non_exhaustive]` so future variants can be added without a breaking
/// change to downstream `match` arms.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A code path that has not yet been implemented.
    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),

    /// The given path could not be used as a workspace root.
    #[error("invalid workspace path `{path}`: {reason}")]
    InvalidWorkspace {
        /// The path that was rejected.
        path: std::path::PathBuf,
        /// Human-readable reason for the rejection.
        reason: String,
    },

    /// A capability string failed to parse.
    #[error("capability parse error `{raw}`: {reason}")]
    CapabilityParse {
        /// The raw string that was rejected.
        raw: String,
        /// Human-readable reason for the rejection.
        reason: String,
    },

    /// A session lookup failed because no session with that id exists.
    #[error("session not found: {0:?}")]
    SessionNotFound(axp_proto::SessionId),

    /// A capability lookup failed because no capability with that name is registered.
    #[error("capability not found: `{name}`")]
    CapabilityNotFound {
        /// The capability name that was not found.
        name: String,
    },

    /// A provider registration was rejected because a provider with that id already exists.
    #[error("duplicate provider: `{id}`")]
    DuplicateProvider {
        /// The provider id that already exists.
        id: String,
    },

    /// A provider registration was rejected because two capabilities share the same local name.
    #[error("duplicate capability `{name}` within provider `{provider}`")]
    DuplicateCapability {
        /// The provider id that contains the duplicate.
        provider: String,
        /// The local capability name that appears more than once.
        name: String,
    },

    /// A capability description failed the quality gate during provider registration.
    #[error("description quality check failed for `{provider}:{capability}`: {reason}")]
    DescriptionQuality {
        /// The provider id whose capability failed the check.
        provider: String,
        /// The capability name whose description failed.
        capability: String,
        /// Human-readable reason for the failure.
        reason: String,
    },
}

/// A `Result` alias that defaults the error type to [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_implemented_display() {
        let err = Error::NotImplemented("foo::bar");
        assert_eq!(err.to_string(), "not yet implemented: foo::bar");
    }
}
