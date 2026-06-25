//! Real seccomp-bpf enforcement tests for `axp-sandbox`.
//!
//! These spawn a child under a `KernelLsm` policy and assert the kernel reports
//! a seccomp filter is active (`/proc/self/status` Seccomp mode 2). They skip
//! gracefully when Landlock is unavailable, since `KernelLsm` applies Landlock
//! and seccomp together, so they are safe in any CI.
use axp_sandbox::{EnforcementTier, SandboxPolicy};
use tempfile::tempdir;

/// Read the `Seccomp:` mode the child reports for itself from
/// `/proc/self/status`. The field is `0` (disabled), `1` (strict), or `2`
/// (filter). Returns the parsed mode from the child's stdout.
fn parse_seccomp_mode(stdout: &[u8]) -> Option<u32> {
    let text = String::from_utf8_lossy(stdout);
    for line in text.lines() {
        // Match the mode line exactly, not `Seccomp_filters:` on newer kernels.
        if let Some(rest) = line.strip_prefix("Seccomp:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// A child spawned under `KernelLsm` must observe seccomp mode 2 (a BPF filter
/// is installed). The child reads `/proc/self/status`, which the baseline
/// system allowlist (`/proc`, `/bin`, `/usr`, libs) lets it run under Landlock.
#[tokio::test]
async fn seccomp_filter_active_under_kernel_lsm() {
    if !axp_sandbox::landlock_available() {
        eprintln!("skipping: Landlock unavailable (KernelLsm needs it)");
        return;
    }

    let work = tempdir().expect("create work dir");
    let policy = SandboxPolicy::from_parts(
        EnforcementTier::KernelLsm,
        vec![work.path().to_path_buf()],
        vec![],
        true,
        false,
    );

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg("grep '^Seccomp:' /proc/self/status");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    policy.apply(&mut cmd).expect("apply policy");

    let out = cmd.output().await.expect("spawn child under KernelLsm");
    assert!(
        out.status.success(),
        "child should run under the sandbox, status={:?}",
        out.status
    );
    assert_eq!(
        parse_seccomp_mode(&out.stdout),
        Some(2),
        "expected seccomp mode 2 (filter active), got: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// The `DevNone` tier installs no seccomp filter: the child reports mode 0.
///
/// Linux-only: `/proc/self/status` does not exist on other platforms.
#[tokio::test]
async fn dev_none_has_no_seccomp_filter() {
    if !cfg!(target_os = "linux") {
        eprintln!("skipping: /proc/self/status is Linux-only");
        return;
    }
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg("grep '^Seccomp:' /proc/self/status");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    SandboxPolicy::dev_none()
        .apply(&mut cmd)
        .expect("apply dev_none");

    let out = cmd.output().await.expect("spawn child");
    assert!(
        out.status.success(),
        "child should run, status={:?}",
        out.status
    );
    assert_eq!(
        parse_seccomp_mode(&out.stdout),
        Some(0),
        "dev_none must not install a filter, got: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}
