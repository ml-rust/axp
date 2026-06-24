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
