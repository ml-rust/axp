//! Opaque protocol identifiers.
use serde::{Deserialize, Serialize};

/// Opaque session identifier (e.g. "s_91"). Treat the inner string as opaque — do not parse it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    /// Borrow the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Opaque job identifier (e.g. "j_5"). Treat the inner string as opaque.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobId(pub String);

impl JobId {
    /// Borrow the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_serializes_as_bare_string() {
        let id = SessionId("s_91".into());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""s_91""#);
    }

    #[test]
    fn session_id_round_trips() {
        let id = SessionId("s_91".into());
        let json = serde_json::to_string(&id).unwrap();
        let id2: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn job_id_serializes_as_bare_string() {
        let id = JobId("j_5".into());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""j_5""#);
    }

    #[test]
    fn job_id_round_trips() {
        let id = JobId("j_5".into());
        let json = serde_json::to_string(&id).unwrap();
        let id2: JobId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, id2);
    }
}
