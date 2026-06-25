//! Real Landlock filesystem confinement tests for `axp-sandbox`.
//!
//! These spawn a child under a `KernelLsm` policy and assert the kernel actually
//! denies reads outside the granted workspace. They skip gracefully when
//! Landlock is unavailable on the host, so they are safe in any CI.
use std::fs;

use axp_sandbox::{EnforcementTier, SandboxPolicy};
use tempfile::tempdir;

/// A child confined to a workspace must be denied reading a file OUTSIDE it,
/// but allowed to read a file INSIDE it (positive control proves the baseline
/// system allowlist lets `cat` and its loader run).
#[tokio::test]
async fn confines_child_to_workspace() {
    if !axp_sandbox::landlock_available() {
        eprintln!("skipping: Landlock unavailable");
        return;
    }

    let work = tempdir().expect("create work dir");
    let outside = tempdir().expect("create outside dir");

    let secret = outside.path().join("secret.txt");
    fs::write(&secret, "TOPSECRET").expect("write secret");
    let allowed = work.path().join("allowed.txt");
    fs::write(&allowed, "OK").expect("write allowed");

    // Workspace is readable; nothing else is granted.
    let policy = SandboxPolicy::from_parts(
        EnforcementTier::KernelLsm,
        vec![work.path().to_path_buf()],
        vec![],
        true,
        false,
    );

    // Negative: reading the outside path must fail and leak no secret bytes.
    {
        let mut cmd = tokio::process::Command::new("cat");
        cmd.arg(&secret);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());
        policy.apply(&mut cmd).expect("apply");
        let out = cmd.output().await.expect("spawn cat outside");
        assert!(
            !out.status.success(),
            "expected cat of outside path to FAIL under Landlock, status={:?}",
            out.status
        );
        assert!(
            !out.stdout
                .windows(b"TOPSECRET".len())
                .any(|w| w == b"TOPSECRET"),
            "secret leaked through Landlock confinement: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    // Positive control: reading the granted workspace path must succeed.
    {
        let mut cmd = tokio::process::Command::new("cat");
        cmd.arg(&allowed);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());
        policy.apply(&mut cmd).expect("apply");
        let out = cmd.output().await.expect("spawn cat inside");
        assert!(
            out.status.success(),
            "expected cat of granted workspace path to SUCCEED, status={:?} stderr-suppressed",
            out.status
        );
        assert!(
            out.stdout.windows(b"OK".len()).any(|w| w == b"OK"),
            "expected workspace file contents, got: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

/// The `DevNone` tier applies no enforcement: a child can read anywhere.
#[tokio::test]
async fn dev_none_does_not_confine() {
    let outside = tempdir().expect("create outside dir");
    let secret = outside.path().join("secret.txt");
    fs::write(&secret, "TOPSECRET").expect("write secret");

    let mut cmd = tokio::process::Command::new("cat");
    cmd.arg(&secret);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    SandboxPolicy::dev_none()
        .apply(&mut cmd)
        .expect("apply dev_none");

    let out = cmd.output().await.expect("spawn cat");
    assert!(
        out.status.success(),
        "dev_none must not confine, status={:?}",
        out.status
    );
    assert!(
        out.stdout
            .windows(b"TOPSECRET".len())
            .any(|w| w == b"TOPSECRET"),
        "expected unconfined read to see the secret, got: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}
