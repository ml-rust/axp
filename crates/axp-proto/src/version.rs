//! Protocol version constant for the AXP wire format.

/// The current AXP protocol version string.
pub const PROTOCOL_VERSION: &str = "0.0.0";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_is_non_empty() {
        assert!(!PROTOCOL_VERSION.is_empty());
    }
}
