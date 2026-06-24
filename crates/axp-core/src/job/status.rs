//! Runtime job status type and conversions to/from the wire representation.

use axp_proto::JobStatusProto;

/// Runtime job status (mirrors `axp_proto::JobStatusProto`).
///
/// This type lives in the core crate and is the authoritative in-process
/// representation of a job's lifecycle state.  It converts losslessly to and
/// from [`JobStatusProto`] for serialisation on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    /// Created, not yet started.
    Pending,
    /// Process is running.
    Running,
    /// Process exited with this code.
    Exited { code: i32 },
    /// Process was killed (signal/cancel).
    Killed,
    /// Job failed before/around execution (capability denial, spawn error, buffer overflow, …).
    Failed { reason: String },
}

impl JobStatus {
    /// Returns `true` if the job has reached a terminal state (no further transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobStatus::Exited { .. } | JobStatus::Killed | JobStatus::Failed { .. }
        )
    }
}

impl From<JobStatus> for JobStatusProto {
    fn from(s: JobStatus) -> Self {
        match s {
            JobStatus::Pending => JobStatusProto::Pending,
            JobStatus::Running => JobStatusProto::Running,
            JobStatus::Exited { code } => JobStatusProto::Exited { code },
            JobStatus::Killed => JobStatusProto::Killed,
            JobStatus::Failed { reason } => JobStatusProto::Failed { reason },
        }
    }
}

impl From<JobStatusProto> for JobStatus {
    fn from(p: JobStatusProto) -> Self {
        match p {
            JobStatusProto::Pending => JobStatus::Pending,
            JobStatusProto::Running => JobStatus::Running,
            JobStatusProto::Exited { code } => JobStatus::Exited { code },
            JobStatusProto::Killed => JobStatus::Killed,
            JobStatusProto::Failed { reason } => JobStatus::Failed { reason },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_terminal ───────────────────────────────────────────────────────────

    #[test]
    fn pending_is_not_terminal() {
        assert!(!JobStatus::Pending.is_terminal());
    }

    #[test]
    fn running_is_not_terminal() {
        assert!(!JobStatus::Running.is_terminal());
    }

    #[test]
    fn exited_is_terminal() {
        assert!(JobStatus::Exited { code: 0 }.is_terminal());
    }

    #[test]
    fn killed_is_terminal() {
        assert!(JobStatus::Killed.is_terminal());
    }

    #[test]
    fn failed_is_terminal() {
        assert!(
            JobStatus::Failed {
                reason: "oops".into()
            }
            .is_terminal()
        );
    }

    // ── round-trip JobStatus → proto → JobStatus ──────────────────────────────

    fn round_trip(s: JobStatus) -> JobStatus {
        let proto: JobStatusProto = s.into();
        proto.into()
    }

    #[test]
    fn pending_round_trips() {
        assert_eq!(round_trip(JobStatus::Pending), JobStatus::Pending);
    }

    #[test]
    fn running_round_trips() {
        assert_eq!(round_trip(JobStatus::Running), JobStatus::Running);
    }

    #[test]
    fn exited_round_trips() {
        let orig = JobStatus::Exited { code: 42 };
        assert_eq!(round_trip(orig.clone()), orig);
    }

    #[test]
    fn killed_round_trips() {
        assert_eq!(round_trip(JobStatus::Killed), JobStatus::Killed);
    }

    #[test]
    fn failed_round_trips() {
        let orig = JobStatus::Failed {
            reason: "bad".into(),
        };
        assert_eq!(round_trip(orig.clone()), orig);
    }
}
