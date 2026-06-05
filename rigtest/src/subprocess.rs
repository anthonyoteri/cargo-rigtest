//! Subprocess execution seam.
//!
//! The orchestrator runs each test case through a [`SubprocessRunner`]. The
//! production implementation, [`OsSubprocessRunner`], spawns a real test
//! binary; tests can substitute a stub that returns canned outcomes without
//! touching the OS.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::anyhow;

use crate::protocol::{self, SubprocessOutcome};

/// Per-call parameters for a single subprocess invocation.
pub(crate) struct SpawnRequest<'a> {
    pub test_name: &'a str,
    pub state_var: &'a str,
    pub state_json: &'a str,
    pub timeout: Option<Duration>,
}

/// Runs one test case as a subprocess and returns its outcome.
///
/// `Send + Sync + 'static` so a runner can be shared via `Arc` into tasks
/// spawned on a `JoinSet` for parallel dispatch. The returned future is
/// required to be `Send` so it can be awaited inside those tasks.
pub(crate) trait SubprocessRunner: Send + Sync + 'static {
    fn run(
        &self,
        req: SpawnRequest<'_>,
    ) -> impl std::future::Future<Output = anyhow::Result<SubprocessOutcome>> + Send;
}

/// Production runner: spawns the test binary as a child process.
pub(crate) struct OsSubprocessRunner {
    exe: PathBuf,
    no_capture: bool,
}

impl OsSubprocessRunner {
    pub(crate) fn new(exe: PathBuf, no_capture: bool) -> Self {
        Self { exe, no_capture }
    }
}

impl SubprocessRunner for OsSubprocessRunner {
    async fn run(&self, req: SpawnRequest<'_>) -> anyhow::Result<SubprocessOutcome> {
        let mut cmd = tokio::process::Command::new(&self.exe);
        cmd.arg("--run-single")
            .arg(req.test_name)
            .arg("--state-env-var")
            .arg(req.state_var)
            .env(req.state_var, req.state_json);

        if self.no_capture {
            spawn_no_capture(cmd, req.timeout).await
        } else {
            spawn_captured(cmd, req.timeout).await
        }
    }
}

// ── private helpers ──────────────────────────────────────────────────────

/// Grace period between SIGTERM and SIGKILL when a test times out.
const KILL_GRACE_PERIOD: Duration = Duration::from_secs(5);

/// Send SIGTERM and wait up to `KILL_GRACE_PERIOD` for the process to exit,
/// then send SIGKILL if it is still running.
///
/// On non-Unix platforms SIGTERM is not available, so this falls straight
/// through to a hard kill.
async fn graceful_kill(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // SAFETY: kill(2) is safe to call with a valid pid and signal number.
            unsafe { libc::kill(pid.cast_signed(), libc::SIGTERM) };
        }

        tokio::select! {
            _ = child.wait() => return,
            () = tokio::time::sleep(KILL_GRACE_PERIOD) => {}
        }
    }

    let _ = child.kill().await;
}

enum WaitOutcome {
    Exited(std::process::ExitStatus),
    TimedOut(Duration),
}

/// Wait for `child` to exit, killing it gracefully if `timeout` elapses.
async fn wait_or_timeout(
    child: &mut tokio::process::Child,
    timeout: Option<Duration>,
) -> anyhow::Result<WaitOutcome> {
    match timeout {
        Some(dur) => tokio::select! {
            r = child.wait() => r.map(WaitOutcome::Exited).map_err(|e| anyhow!("{e}")),
            () = tokio::time::sleep(dur) => {
                graceful_kill(child).await;
                Ok(WaitOutcome::TimedOut(dur))
            }
        },
        None => child
            .wait()
            .await
            .map(WaitOutcome::Exited)
            .map_err(|e| anyhow!("{e}")),
    }
}

async fn drain_pipe<R>(handle: Option<R>) -> String
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;
    let Some(mut h) = handle else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let _ = h.read_to_end(&mut bytes).await;
    if bytes.is_empty() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Spawn in no-capture mode: stdout inherited, stderr piped for skip-reason
/// extraction. Stderr is replayed to the terminal on failure so it is not lost.
async fn spawn_no_capture(
    mut cmd: tokio::process::Command,
    timeout: Option<Duration>,
) -> anyhow::Result<SubprocessOutcome> {
    let mut child = cmd
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("failed to spawn test subprocess: {e}"))?;

    let status = match wait_or_timeout(&mut child, timeout).await? {
        WaitOutcome::TimedOut(dur) => return Ok(SubprocessOutcome::TimedOut(dur)),
        WaitOutcome::Exited(s) => s,
    };

    let stderr = drain_pipe(child.stderr.take()).await;

    match status.code() {
        Some(0) => Ok(SubprocessOutcome::Passed),
        Some(c) if c == protocol::SKIP_EXIT_CODE => Ok(SubprocessOutcome::Skipped(
            protocol::decode_skip_reason(&stderr),
        )),
        code => {
            // Stderr was not inherited, so replay it so the user can see it.
            eprint!("{stderr}");
            Ok(SubprocessOutcome::Failed {
                reason: protocol::exit_code_reason(code),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }
}

/// Spawn in capture mode: both stdout and stderr piped, printed only on
/// failure.
async fn spawn_captured(
    mut cmd: tokio::process::Command,
    timeout: Option<Duration>,
) -> anyhow::Result<SubprocessOutcome> {
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("failed to spawn test subprocess: {e}"))?;

    let status = match wait_or_timeout(&mut child, timeout).await? {
        WaitOutcome::TimedOut(dur) => return Ok(SubprocessOutcome::TimedOut(dur)),
        WaitOutcome::Exited(s) => s,
    };

    let (stdout, stderr) = tokio::join!(
        drain_pipe(child.stdout.take()),
        drain_pipe(child.stderr.take())
    );

    Ok(protocol::decode_outcome(status.code(), stdout, stderr))
}
