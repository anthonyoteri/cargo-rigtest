use std::sync::Arc;
use std::time::Instant;

use anyhow::anyhow;
use rand::seq::SliceRandom as _;
use rand::RngExt as _;
use rand::SeedableRng as _;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::protocol::{self, SubprocessOutcome};
use crate::registry::{RIG_GLOBAL_SETUP, RIG_GLOBAL_TEARDOWN, RIG_TEST_CASES};
use crate::reporter::Reporter;
use crate::scheduler::RuntimeArgs;

fn default_jobs() -> usize {
    std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
}

fn apply_filter<'a>(
    cases: &[&'a crate::registry::TestCase],
    filter: Option<&str>,
) -> Vec<&'a crate::registry::TestCase> {
    cases
        .iter()
        .filter(|tc| filter.is_none_or(|f| tc.name.contains(f)))
        .copied()
        .collect()
}

struct RunConfig {
    exe: std::path::PathBuf,
    state_var: String,
    state_json: String,
    no_capture: bool,
}

#[derive(Clone, Copy)]
enum Outcome {
    Passed,
    Skipped,
    Failed,
}

/// Grace period between SIGTERM and SIGKILL when a test times out.
const KILL_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(5);

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
    TimedOut(std::time::Duration),
}

/// Wait for `child` to exit, killing it gracefully if `timeout` elapses.
async fn wait_or_timeout(
    child: &mut tokio::process::Child,
    timeout: Option<std::time::Duration>,
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
    timeout: Option<std::time::Duration>,
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
    timeout: Option<std::time::Duration>,
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

/// Run a single test subprocess, respecting timeout and capture settings.
async fn spawn_test_process(
    exe: &std::path::Path,
    test_name: &str,
    state_var: &str,
    state_json: &str,
    no_capture: bool,
    timeout: Option<std::time::Duration>,
) -> anyhow::Result<SubprocessOutcome> {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("--run-single")
        .arg(test_name)
        .arg("--state-env-var")
        .arg(state_var)
        .env(state_var, state_json);

    if no_capture {
        spawn_no_capture(cmd, timeout).await
    } else {
        spawn_captured(cmd, timeout).await
    }
}

/// Run a test with retries, returning the final outcome and updating the
/// reporter.
async fn run_test(
    reporter: &Reporter,
    exe: &std::path::Path,
    tc: &crate::registry::TestCase,
    state_var: &str,
    state_json: &str,
    no_capture: bool,
) -> (Outcome, std::time::Duration) {
    let pb = reporter.test_started(tc.name);
    let test_start = Instant::now();
    let max_attempts = tc.retries + 1;

    for attempt in 1..=max_attempts {
        let outcome =
            spawn_test_process(exe, tc.name, state_var, state_json, no_capture, tc.timeout).await;

        let is_last = attempt == max_attempts;
        let duration = test_start.elapsed();

        match outcome {
            Ok(SubprocessOutcome::Passed) => {
                reporter.test_passed(&pb, tc.name, duration);
                return (Outcome::Passed, duration);
            }
            Ok(SubprocessOutcome::Skipped(reason)) => {
                reporter.test_skipped(&pb, tc.name, duration, &reason);
                return (Outcome::Skipped, duration);
            }
            Ok(SubprocessOutcome::Failed {
                reason,
                stdout,
                stderr,
            }) => {
                if is_last {
                    reporter.test_failed(&pb, tc.name, duration, &reason, &stdout, &stderr);
                    return (Outcome::Failed, duration);
                }
                reporter.test_retrying(tc.name, attempt, max_attempts, &reason);
            }
            Ok(SubprocessOutcome::TimedOut(dur)) => {
                let reason = format!("timed out after {:.1}s", dur.as_secs_f64());
                if is_last {
                    reporter.test_failed(&pb, tc.name, duration, &reason, "", "");
                    return (Outcome::Failed, duration);
                }
                reporter.test_retrying(tc.name, attempt, max_attempts, &reason);
            }
            Err(e) => {
                if is_last {
                    reporter.test_failed(&pb, tc.name, duration, &e.to_string(), "", "");
                    return (Outcome::Failed, duration);
                }
                reporter.test_retrying(tc.name, attempt, max_attempts, &e.to_string());
            }
        }
    }

    unreachable!()
}

async fn dispatch_cases(
    reporter: &Arc<Reporter>,
    cfg: &RunConfig,
    semaphore: Arc<Semaphore>,
    parallel_cases: Vec<&'static crate::registry::TestCase>,
    serial_cases: Vec<&'static crate::registry::TestCase>,
) -> (usize, usize) {
    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut join_set: JoinSet<Outcome> = JoinSet::new();

    for tc in parallel_cases {
        let reporter = Arc::clone(reporter);
        let semaphore = Arc::clone(&semaphore);
        let exe = cfg.exe.clone();
        let state_var = cfg.state_var.clone();
        let state_json = cfg.state_json.clone();
        let no_capture = cfg.no_capture;

        join_set.spawn(async move {
            let _permit = semaphore
                .acquire()
                .await
                .expect("semaphore should not be closed");
            let (outcome, _) =
                run_test(&reporter, &exe, tc, &state_var, &state_json, no_capture).await;
            outcome
        });
    }

    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Outcome::Passed) => passed += 1,
            Ok(Outcome::Skipped) => skipped += 1,
            Ok(Outcome::Failed) => {}
            Err(e) => eprintln!("cargo-rigtest: task join error: {e}"),
        }
    }

    for tc in serial_cases {
        let (outcome, _) = run_test(
            reporter,
            &cfg.exe,
            tc,
            &cfg.state_var,
            &cfg.state_json,
            cfg.no_capture,
        )
        .await;
        match outcome {
            Outcome::Passed => passed += 1,
            Outcome::Skipped => skipped += 1,
            Outcome::Failed => {}
        }
    }

    (passed, skipped)
}

/// Run the full test suite (coordinator path).
///
/// # Errors
///
/// Returns an error if any test fails or if the current executable path
/// cannot be determined.
///
/// # Panics
///
/// Panics if more than one `#[global_setup]` or `#[global_teardown]` function
/// is registered.
pub(crate) async fn run(args: RuntimeArgs) -> anyhow::Result<()> {
    assert!(
        RIG_GLOBAL_SETUP.len() <= 1,
        "cargo-rigtest: at most one #[global_setup] function may be defined, found {}",
        RIG_GLOBAL_SETUP.len()
    );
    assert!(
        RIG_GLOBAL_TEARDOWN.len() <= 1,
        "cargo-rigtest: at most one #[global_teardown] function may be defined, found {}",
        RIG_GLOBAL_TEARDOWN.len()
    );
    #[cfg(feature = "http-client")]
    assert!(
        crate::registry::RIG_HTTP_CLIENT_CONFIGURATOR.len() <= 1,
        "cargo-rigtest: at most one #[rigtest::main(http_client = …)] may be defined, found {}",
        crate::registry::RIG_HTTP_CLIENT_CONFIGURATOR.len()
    );
    #[cfg(all(feature = "ssh-client", unix))]
    assert!(
        crate::registry::RIG_SSH_CLIENT_CONFIGURATOR.len() <= 1,
        "cargo-rigtest: at most one #[rigtest::main(ssh_client = …)] may be defined, found {}",
        crate::registry::RIG_SSH_CLIENT_CONFIGURATOR.len()
    );

    let reporter = Arc::new(Reporter::new());

    let global_setup = RIG_GLOBAL_SETUP.first();

    let global_data: Box<dyn std::any::Any + Send + Sync> = if let Some(entry) = global_setup {
        reporter.print_phase("global setup");
        (entry.setup_fn)().await
    } else {
        Box::new(())
    };

    let mut rng = rand::rng();

    let state_var = format!("RIG_STATE_{:016x}", rng.random::<u64>());
    let state_json: String = if let Some(entry) = global_setup {
        (entry.serialize_fn)(&*global_data)
    } else {
        String::new()
    };

    let cases_refs: Vec<&'static crate::registry::TestCase> = RIG_TEST_CASES.iter().collect();
    let mut cases = apply_filter(&cases_refs, args.filter.as_deref());

    let seed = args.seed.unwrap_or_else(|| rng.random::<u64>());
    println!("cargo-rigtest: running with seed {seed}");

    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(seed);
    cases.shuffle(&mut rng);

    let total = cases.len();
    let jobs = if args.no_capture {
        1
    } else {
        args.jobs.unwrap_or_else(default_jobs)
    };
    let semaphore = Arc::new(Semaphore::new(jobs));

    let exe =
        std::env::current_exe().map_err(|e| anyhow!("failed to find current executable: {e}"))?;

    let cfg = RunConfig {
        exe,
        state_var,
        state_json,
        no_capture: args.no_capture,
    };

    let suite_start = Instant::now();

    let (serial_cases, parallel_cases): (Vec<_>, Vec<_>) =
        cases.into_iter().partition(|tc| tc.serial);

    let (passed, skipped) =
        dispatch_cases(&reporter, &cfg, semaphore, parallel_cases, serial_cases).await;

    let elapsed = suite_start.elapsed();
    reporter.finish(passed, skipped, total, elapsed);

    if let Some(entry) = RIG_GLOBAL_TEARDOWN.first() {
        reporter.print_phase("global teardown");
        (entry.teardown_fn)(global_data).await;
    }

    let failed = total - passed - skipped;
    if failed > 0 {
        Err(anyhow!("Test suite failed: {passed}/{total} passed"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::TestContext;
    use crate::registry::{BoxFuture, TestCase};
    use std::sync::Arc;

    fn make_case(name: &'static str) -> TestCase {
        TestCase {
            name,
            module: "test_module",
            file: "test.rs",
            serial: false,
            timeout: None,
            retries: 0,
            test_fn: |_ctx: Arc<TestContext>| -> BoxFuture<
                'static,
                Result<(), Box<dyn std::error::Error + Send + Sync>>,
            > { Box::pin(async { Ok(()) }) },
        }
    }

    #[test]
    fn filter_none_returns_all() {
        let cases = [make_case("foo"), make_case("bar"), make_case("baz")];
        let refs: Vec<&TestCase> = cases.iter().collect();
        assert_eq!(apply_filter(&refs, None).len(), 3);
    }

    #[test]
    fn filter_matches_substring() {
        let cases = [
            make_case("test_login"),
            make_case("test_logout"),
            make_case("health_check"),
        ];
        let refs: Vec<&TestCase> = cases.iter().collect();
        let filtered = apply_filter(&refs, Some("test_"));
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|tc| tc.name.contains("test_")));
    }

    #[test]
    fn filter_no_match_returns_empty() {
        let cases = [make_case("foo"), make_case("bar")];
        let refs: Vec<&TestCase> = cases.iter().collect();
        assert_eq!(apply_filter(&refs, Some("xyz")).len(), 0);
    }
}
