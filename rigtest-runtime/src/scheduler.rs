use std::sync::Arc;
use std::time::Instant;

use anyhow::anyhow;
use clap::Parser;
use futures::FutureExt;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::context::TestContext;
use crate::registry::RIG_GLOBAL_SETUP;
use crate::registry::RIG_GLOBAL_TEARDOWN;
use crate::registry::RIG_TEST_CASES;
use crate::reporter::Reporter;

fn default_jobs() -> usize {
    std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
}

/// Arguments forwarded from `cargo rig run` into the test binary.
#[derive(Parser, Debug)]
#[command(about = "Run the cargo-rigtest acceptance test suite")]
pub struct RuntimeArgs {
    /// Maximum number of parallel test jobs [default: number of CPUs].
    #[arg(short, long)]
    pub jobs: Option<usize>,

    /// Seed for randomized test order. Printed on every run so failures are
    /// reproducible.
    #[arg(long)]
    pub seed: Option<u64>,

    /// Only run tests whose name contains FILTER.
    #[arg(short, long)]
    pub filter: Option<String>,

    /// Show test output in real time rather than capturing it.
    #[arg(long)]
    pub no_capture: bool,

    // ── Internal flags used in subprocess (single-test) mode ─────────────────
    // Hidden from `--help`; set by the coordinator when spawning per-test
    // subprocesses.
    /// Run exactly one named test case and exit. Used internally.
    #[arg(long, hide = true)]
    pub run_single: Option<String>,

    /// Name of the env var holding the serialized global state. Used internally.
    #[arg(long, hide = true)]
    pub state_env_var: Option<String>,

    /// Exit immediately with code 0. Used by cargo-rigtest to confirm this binary
    /// is a rig test runner before attempting to run it.
    #[arg(long, hide = true)]
    pub rig_probe: bool,
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

/// Run the full test suite described by `args`.
pub async fn run_suite(args: RuntimeArgs) -> anyhow::Result<()> {
    // ── Single-test (subprocess) mode ────────────────────────────────────────
    if let Some(ref test_name) = args.run_single {
        return run_single_test(test_name, args.state_env_var.as_deref()).await;
    }

    // ── Coordinator mode ─────────────────────────────────────────────────────

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

    // Create reporter early so it can frame setup/teardown output too.
    let reporter = Arc::new(Reporter::new());

    // Run global setup.
    let global_data: Box<dyn std::any::Any + Send + Sync> =
        if let Some(entry) = RIG_GLOBAL_SETUP.first() {
            reporter.print_phase("global setup");
            let data = (entry.setup_fn)().await;
            data
        } else {
            Box::new(())
        };

    // Serialize state for subprocess handoff, choosing a randomized env var
    // name so it is not guessable by other processes on the same host.
    let state_var = format!("RIG_STATE_{:016x}", {
        use rand::RngCore;
        rand::thread_rng().next_u64()
    });
    let state_json: String = if let Some(entry) = RIG_GLOBAL_SETUP.first() {
        (entry.serialize_fn)(&global_data)
    } else {
        String::new()
    };

    // Collect and optionally filter test cases.
    let cases_refs: Vec<&'static crate::registry::TestCase> = RIG_TEST_CASES.iter().collect();
    let mut cases = apply_filter(&cases_refs, args.filter.as_deref());

    // Choose seed and shuffle.
    let seed = args.seed.unwrap_or_else(|| {
        use rand::RngCore;
        rand::thread_rng().next_u64()
    });
    println!("cargo-rigtest: running with seed {seed}");

    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(seed);
    cases.shuffle(&mut rng);

    let total = cases.len();
    let jobs = match (args.no_capture, args.jobs) {
        (true, None) => 1,
        (true, Some(n)) => {
            eprintln!("cargo-rigtest: warning: --no-capture with --jobs={n} may interleave output");
            n
        }
        (false, n) => n.unwrap_or_else(default_jobs),
    };
    let semaphore = Arc::new(Semaphore::new(jobs));

    let exe =
        std::env::current_exe().map_err(|e| anyhow!("failed to find current executable: {e}"))?;

    let suite_start = Instant::now();

    // Partition into parallel and serial. Serial tests run after all parallel
    // tests finish so they never share the executor with another test.
    let (serial_cases, parallel_cases): (Vec<_>, Vec<_>) =
        cases.into_iter().partition(|tc| tc.serial);

    let mut passed = 0usize;
    let mut skipped = 0usize;

    // ── Parallel phase ───────────────────────────────────────────────────────
    let mut join_set: JoinSet<Outcome> = JoinSet::new();

    for tc in parallel_cases {
        let reporter = Arc::clone(&reporter);
        let semaphore = Arc::clone(&semaphore);
        let exe = exe.clone();
        let state_var = state_var.clone();
        let state_json = state_json.clone();
        let no_capture = args.no_capture;

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

    // ── Serial phase ─────────────────────────────────────────────────────────
    for tc in serial_cases {
        let (outcome, _) = run_test(
            &reporter,
            &exe,
            tc,
            &state_var,
            &state_json,
            args.no_capture,
        )
        .await;
        match outcome {
            Outcome::Passed => passed += 1,
            Outcome::Skipped => skipped += 1,
            Outcome::Failed => {}
        }
    }

    let elapsed = suite_start.elapsed();
    reporter.finish(passed, skipped, total, elapsed);

    // Run global teardown.
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

#[derive(Clone, Copy)]
enum Outcome {
    Passed,
    Skipped,
    Failed,
}

enum ProcessOutcome {
    Passed,
    Skipped {
        reason: String,
    },
    Failed {
        reason: String,
        stdout: String,
        stderr: String,
    },
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
        // Send SIGTERM so destructors and signal handlers can run.
        if let Some(pid) = child.id() {
            // SAFETY: kill(2) is safe to call with a valid pid and signal number.
            unsafe { libc::kill(pid.cast_signed(), libc::SIGTERM) };
        }

        // Give the process a chance to exit on its own.
        tokio::select! {
            _ = child.wait() => return,
            () = tokio::time::sleep(KILL_GRACE_PERIOD) => {}
        }
    }

    // Grace period elapsed (or non-Unix): hard kill.
    let _ = child.kill().await;
}

/// Run a single test subprocess, respecting timeout and capture settings.
async fn spawn_test_process(
    exe: &std::path::Path,
    test_name: &str,
    state_var: &str,
    state_json: &str,
    no_capture: bool,
    timeout: Option<std::time::Duration>,
) -> anyhow::Result<ProcessOutcome> {
    use tokio::io::AsyncReadExt;
    use tokio::process::Command;

    let mut cmd = Command::new(exe);
    cmd.arg("--run-single")
        .arg(test_name)
        .arg("--state-env-var")
        .arg(state_var)
        .env(state_var, state_json);

    if no_capture {
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("failed to spawn test subprocess: {e}"))?;

        let status = match timeout {
            Some(dur) => tokio::select! {
                r = child.wait() => r.map_err(|e| anyhow!("{e}"))?,
                () = tokio::time::sleep(dur) => {
                    graceful_kill(&mut child).await;
                    return Ok(ProcessOutcome::Failed {
                        reason: format!("timed out after {:.1}s", dur.as_secs_f64()),
                        stdout: String::new(),
                        stderr: String::new(),
                    });
                }
            },
            None => child.wait().await.map_err(|e| anyhow!("{e}"))?,
        };

        return match status.code() {
            Some(0) => Ok(ProcessOutcome::Passed),
            Some(2) => Ok(ProcessOutcome::Skipped {
                reason: String::new(),
            }),
            code => Ok(ProcessOutcome::Failed {
                reason: format!("exited with code {}", code.unwrap_or(-1)),
                stdout: String::new(),
                stderr: String::new(),
            }),
        };
    }

    // Capture mode: pipe stdout/stderr, read them after the process exits.
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("failed to spawn test subprocess: {e}"))?;

    let status = match timeout {
        Some(dur) => tokio::select! {
            r = child.wait() => r.map_err(|e| anyhow!("{e}"))?,
            () = tokio::time::sleep(dur) => {
                graceful_kill(&mut child).await;
                return Ok(ProcessOutcome::Failed {
                    reason: format!("timed out after {:.1}s", dur.as_secs_f64()),
                    stdout: String::new(),
                    stderr: String::new(),
                });
            }
        },
        None => child.wait().await.map_err(|e| anyhow!("{e}"))?,
    };

    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    if let Some(mut h) = child.stdout.take() {
        let _ = h.read_to_end(&mut stdout_bytes).await;
    }
    if let Some(mut h) = child.stderr.take() {
        let _ = h.read_to_end(&mut stderr_bytes).await;
    }

    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();

    match status.code() {
        Some(0) => Ok(ProcessOutcome::Passed),
        Some(2) => {
            let reason = stderr
                .lines()
                .find_map(|l| l.strip_prefix("cargo-rigtest-skip: "))
                .unwrap_or("")
                .to_string();
            Ok(ProcessOutcome::Skipped { reason })
        }
        code => {
            let reason = if stderr.trim().is_empty() {
                format!("exited with code {}", code.unwrap_or(-1))
            } else {
                stderr.trim().to_string()
            };
            Ok(ProcessOutcome::Failed {
                reason,
                stdout,
                stderr: String::new(),
            })
        }
    }
}

/// Run a test with retries, returning the final outcome and updating the reporter.
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
            Ok(ProcessOutcome::Passed) => {
                reporter.test_passed(&pb, tc.name, duration);
                return (Outcome::Passed, duration);
            }
            Ok(ProcessOutcome::Skipped { reason }) => {
                reporter.test_skipped(&pb, tc.name, duration, &reason);
                return (Outcome::Skipped, duration);
            }
            Ok(ProcessOutcome::Failed {
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

/// Single-test mode: deserialize global state, run one test, exit.
async fn run_single_test(test_name: &str, state_var: Option<&str>) -> anyhow::Result<()> {
    // Deserialize state and immediately clear the env var.
    let global_data: Box<dyn std::any::Any + Send + Sync> = if let Some(var) = state_var {
        let json = std::env::var(var).unwrap_or_default();
        std::env::remove_var(var);

        if let Some(entry) = RIG_GLOBAL_SETUP.first() {
            (entry.deserialize_fn)(&json)
        } else {
            Box::new(())
        }
    } else {
        Box::new(())
    };

    let tc = RIG_TEST_CASES
        .iter()
        .find(|tc| tc.name == test_name)
        .ok_or_else(|| anyhow!("cargo-rigtest: no test named '{test_name}'"))?;

    let ctx = TestContext::new(global_data);

    let result = std::panic::AssertUnwindSafe((tc.test_fn)(ctx))
        .catch_unwind()
        .await;

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            if e.downcast_ref::<crate::Skip>().is_some() {
                eprintln!("cargo-rigtest-skip: {e}");
                crate::flush_and_exit(2);
            }
            Err(anyhow!("{e}"))
        }
        Err(_) => Err(anyhow!("panicked")),
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
