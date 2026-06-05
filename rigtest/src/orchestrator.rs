use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use rand::seq::SliceRandom as _;
use rand::RngExt as _;
use rand::SeedableRng as _;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::protocol::SubprocessOutcome;
use crate::registry::{RIG_GLOBAL_SETUP, RIG_GLOBAL_TEARDOWN, RIG_TEST_CASES};
use crate::reporter::Reporter;
use crate::scheduler::RuntimeArgs;
use crate::subprocess::{OsSubprocessRunner, SpawnRequest, SubprocessRunner};

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

#[derive(Clone, Copy)]
enum Outcome {
    Passed,
    Skipped,
    Failed,
}

/// Run a test with retries, returning the final outcome and updating the
/// reporter.
async fn run_test<R: SubprocessRunner>(
    runner: &R,
    reporter: &Reporter,
    tc: &crate::registry::TestCase,
    state_var: &str,
    state_json: &str,
) -> (Outcome, Duration) {
    let pb = reporter.test_started(tc.name);
    let test_start = Instant::now();
    let max_attempts = tc.retries + 1;

    for attempt in 1..=max_attempts {
        let outcome = runner
            .run(SpawnRequest {
                test_name: tc.name,
                state_var,
                state_json,
                timeout: tc.timeout,
            })
            .await;

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

async fn dispatch_cases<R: SubprocessRunner>(
    runner: Arc<R>,
    reporter: &Arc<Reporter>,
    state_var: String,
    state_json: String,
    semaphore: Arc<Semaphore>,
    parallel_cases: Vec<&'static crate::registry::TestCase>,
    serial_cases: Vec<&'static crate::registry::TestCase>,
) -> (usize, usize) {
    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut join_set: JoinSet<Outcome> = JoinSet::new();

    for tc in parallel_cases {
        let runner = Arc::clone(&runner);
        let reporter = Arc::clone(reporter);
        let semaphore = Arc::clone(&semaphore);
        let state_var = state_var.clone();
        let state_json = state_json.clone();

        join_set.spawn(async move {
            let _permit = semaphore
                .acquire()
                .await
                .expect("semaphore should not be closed");
            let (outcome, _) = run_test(&*runner, &reporter, tc, &state_var, &state_json).await;
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
        let (outcome, _) = run_test(&*runner, reporter, tc, &state_var, &state_json).await;
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

    let runner = Arc::new(OsSubprocessRunner::new(exe, args.no_capture));

    let suite_start = Instant::now();

    let (serial_cases, parallel_cases): (Vec<_>, Vec<_>) =
        cases.into_iter().partition(|tc| tc.serial);

    let (passed, skipped) = dispatch_cases(
        runner,
        &reporter,
        state_var,
        state_json,
        semaphore,
        parallel_cases,
        serial_cases,
    )
    .await;

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
    use std::sync::Mutex;

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

    // ── Subprocess seam demo tests ───────────────────────────────────────
    //
    // These prove the SubprocessRunner trait is a real seam: the
    // orchestration logic (retry loop) is now driveable end-to-end with a
    // fake runner — no OS processes spawned.

    /// Test double that returns a pre-programmed queue of outcomes and records
    /// every call. Multiple queued outcomes per test exercise the retry path.
    struct FakeRunner {
        queue: Mutex<Vec<SubprocessOutcome>>,
        calls: Mutex<u32>,
    }

    impl FakeRunner {
        fn new(outcomes: Vec<SubprocessOutcome>) -> Self {
            Self {
                queue: Mutex::new(outcomes),
                calls: Mutex::new(0),
            }
        }

        fn call_count(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    impl SubprocessRunner for FakeRunner {
        async fn run(&self, _req: SpawnRequest<'_>) -> anyhow::Result<SubprocessOutcome> {
            *self.calls.lock().unwrap() += 1;
            Ok(self.queue.lock().unwrap().remove(0))
        }
    }

    fn case_with_retries(name: &'static str, retries: u32) -> TestCase {
        let mut tc = make_case(name);
        tc.retries = retries;
        tc
    }

    #[tokio::test]
    async fn retry_loop_succeeds_after_one_failure() {
        let runner = FakeRunner::new(vec![
            SubprocessOutcome::Failed {
                reason: "transient".into(),
                stdout: String::new(),
                stderr: String::new(),
            },
            SubprocessOutcome::Passed,
        ]);
        let tc = case_with_retries("flaky", 1);
        let reporter = Reporter::new();

        let (outcome, _) = run_test(&runner, &reporter, &tc, "X", "{}").await;

        assert!(matches!(outcome, Outcome::Passed));
        assert_eq!(runner.call_count(), 2);
    }

    #[tokio::test]
    async fn skip_does_not_retry() {
        let runner = FakeRunner::new(vec![SubprocessOutcome::Skipped("nope".into())]);
        let tc = case_with_retries("skipper", 3);
        let reporter = Reporter::new();

        let (outcome, _) = run_test(&runner, &reporter, &tc, "X", "{}").await;

        assert!(matches!(outcome, Outcome::Skipped));
        assert_eq!(runner.call_count(), 1);
    }

    #[tokio::test]
    async fn retry_exhausts_and_reports_failure() {
        let mk_fail = || SubprocessOutcome::Failed {
            reason: "boom".into(),
            stdout: String::new(),
            stderr: String::new(),
        };
        let runner = FakeRunner::new(vec![mk_fail(), mk_fail(), mk_fail()]);
        let tc = case_with_retries("always_fails", 2);
        let reporter = Reporter::new();

        let (outcome, _) = run_test(&runner, &reporter, &tc, "X", "{}").await;

        assert!(matches!(outcome, Outcome::Failed));
        assert_eq!(runner.call_count(), 3); // initial + 2 retries
    }
}
