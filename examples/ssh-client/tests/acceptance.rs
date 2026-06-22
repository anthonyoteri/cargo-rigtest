use rigtest::{global_setup, global_teardown, preflight, testcase, Preflight, TestContext};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[cfg(unix)]
use rigtest::openssh::KnownHosts;

// A preflight for an SSH-tested suite. We demonstrate the SSH probe
// behind an opt-in env var (`RIGTEST_SSH_HOST`) so the example links
// and runs in CI without a live SSH host: the `.env(...)` baseline
// probe passes deterministically; the `.ssh(...)` probe runs only when
// the operator opts in.
//
// In a real preflight you'd typically declare the `.ssh(...)` probe
// unconditionally — see `rigtest/README.md` for the full primitive
// table and the `command(...)` / `timeout(...)` overrides.
#[cfg(unix)]
#[preflight]
fn checks() -> Preflight {
    let preflight = Preflight::new().env("path_is_set", "PATH");
    if let Ok(host) = std::env::var("RIGTEST_SSH_HOST") {
        preflight.ssh("ssh_bastion", host).command("true")
    } else {
        preflight
    }
}

#[cfg(not(unix))]
#[preflight]
fn checks() -> Preflight {
    Preflight::new().env("path_is_set", "PATH")
}

#[derive(Serialize, Deserialize)]
pub struct SharedState {
    pub host: String,
}

#[global_setup]
async fn setup() -> SharedState {
    SharedState {
        host: std::env::var("SSH_HOST").unwrap_or_else(|_| "localhost".to_string()),
    }
}

#[global_teardown]
async fn teardown(_state: SharedState) {}

/// Configure SSH connections to accept unknown host keys.
///
/// In a CI or local test environment the target host may not be in
/// `~/.ssh/known_hosts`.  This configurator accepts any host key so tests
/// can run without manual key approval.  Do not use this against untrusted
/// hosts.
#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn configure_ssh(
    _destination: &str,
    mut builder: rigtest::openssh::SessionBuilder,
) -> Result<rigtest::openssh::SessionBuilder, rigtest::Error> {
    builder.known_hosts_check(KnownHosts::Accept);
    Ok(builder)
}

#[cfg(unix)]
#[testcase]
async fn runs_remote_command(
    ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = ctx.global::<SharedState>();

    let session = ctx.ssh(&state.host).await?;
    let output = session
        .command("sh")
        .arg("-c")
        .arg("echo hello")
        .output()
        .await?;
    assert!(output.status.success(), "remote command should succeed");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    Ok(())
}

#[cfg(unix)]
#[testcase]
async fn ssh_macro_convenience(
    ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = ctx.global::<SharedState>();

    let output = rigtest::ssh!(ctx, &state.host, "uname -s").output().await?;
    assert!(output.status.success(), "uname should succeed");
    Ok(())
}

#[cfg(unix)]
#[testcase]
async fn reuses_cached_session(
    ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = ctx.global::<SharedState>();

    let s1 = ctx.ssh(&state.host).await?;
    let s2 = ctx.ssh(&state.host).await?;
    assert!(
        std::sync::Arc::ptr_eq(&s1, &s2),
        "second call should return the same cached session"
    );
    Ok(())
}

#[cfg(unix)]
#[testcase]
async fn ssh_error_on_nonexistent_host(
    ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // .invalid is an IANA-reserved TLD (RFC 2606) guaranteed never to resolve;
    // the ssh binary fails immediately on DNS lookup failure.
    let result = ctx.ssh("nonexistent.rigtest.invalid").await;
    assert!(
        result.is_err(),
        "connecting to a nonexistent host should return an error"
    );
    Ok(())
}

#[cfg(unix)]
#[rigtest::main(ssh_client = configure_ssh)]
fn main() {}

#[cfg(not(unix))]
#[rigtest::main]
fn main() {}
