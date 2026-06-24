//! Capability grant strings.
use serde::{Deserialize, Serialize};

/// A capability grant, as the AXP spec defines it: a string grammar such as
/// `fs.read(/proj)`, `fs.write(/proj)`, `net.connect(api.github.com)`, `proc.spawn`,
/// or a named tool/skill. This crate carries it verbatim; structured parsing into OS
/// policy is the sandbox layer's responsibility.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Capability(pub String);

impl Capability {
    /// Borrow the grant as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_serializes_as_bare_string() {
        let cap = Capability("fs.read(/proj)".into());
        let json = serde_json::to_string(&cap).unwrap();
        assert_eq!(json, r#""fs.read(/proj)""#);
    }

    #[test]
    fn capability_round_trips() {
        let cap = Capability("fs.read(/proj)".into());
        let json = serde_json::to_string(&cap).unwrap();
        let cap2: Capability = serde_json::from_str(&json).unwrap();
        assert_eq!(cap, cap2);
    }
}
