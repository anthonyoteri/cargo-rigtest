use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use rand::seq::SliceRandom as _;
use rand::RngExt as _;
use rand::SeedableRng as _;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::junit::{JunitConfig, JunitReporter};
use crate::protocol::SubprocessOutcome;
use crate::registry::{RIG_GLOBAL_SETUP, RIG_GLOBAL_TEARDOWN, RIG_TEST_CASES};
use crate::reporter::{
    MultiReporter, Outcome as ReportOutcome, Reporter, TestEventReporter, TestRef,
};
use crate::scheduler::RuntimeArgs;
use crate::subprocess::{OsSubprocessRunner, SpawnRequest, SubprocessRunner};

fn classify_failed(stderr: &str) -> ReportOutcome {
    if stderr.contains("panicked at") {
        ReportOutcome::Panic
    } else {
        ReportOutcome::Assertion
    }
}

fn test_ref(tc: &crate::registry::TestCase) -> TestRef<'_> {
    TestRef {
        name: tc.name,
        module: tc.module,
        file: tc.file,
    }
}

/// Build the reporter stack from CLI args. Always includes the live console
/// [`Reporter`]; `--reporter junit` adds a [`JunitReporter`] alongside it.
fn build_reporter(args: &RuntimeArgs, seed: u64) -> anyhow::Result<MultiReporter> {
    let mut reporters: Vec<Box<dyn TestEventReporter>> = vec![Box::new(Reporter::new())];

    if let Some(name) = args.reporter.as_deref() {
        match name {
            "junit" => {
                let config = resolve_junit_config(seed)?;
                reporters.push(Box::new(JunitReporter::new(config)));
            }
            other => {
                return Err(anyhow!(
                    "cargo-rigtest: unknown --reporter '{other}' (expected 'junit')"
                ));
            }
        }
    }

    Ok(MultiReporter::new(reporters))
}

/// Strip a trailing cargo hash suffix (e.g. `acceptance-9dbf02a2431e03ff`)
/// from a binary stem. Cargo's metadata hash is always 16 ASCII hex chars —
/// gating on that length prevents mis-stripping legitimate names that happen
/// to end in hex (e.g. `my-test-cafe`).
fn strip_hash_suffix(stem: &str) -> &str {
    if let Some(idx) = stem.rfind('-') {
        let tail = &stem[idx + 1..];
        if tail.len() == 16 && tail.chars().all(|c| c.is_ascii_hexdigit()) {
            return &stem[..idx];
        }
    }
    stem
}

#[cfg(test)]
mod tests_strip_hash {
    use super::strip_hash_suffix;

    #[test]
    fn strips_16_char_hex_suffix() {
        assert_eq!(
            strip_hash_suffix("acceptance-9dbf02a2431e03ff"),
            "acceptance"
        );
    }

    #[test]
    fn preserves_short_hex_tail() {
        assert_eq!(strip_hash_suffix("my-test-cafe"), "my-test-cafe");
    }

    #[test]
    fn preserves_non_hex_tail() {
        assert_eq!(strip_hash_suffix("my-test-foobar"), "my-test-foobar");
    }

    #[test]
    fn preserves_stem_without_dash() {
        assert_eq!(strip_hash_suffix("acceptance"), "acceptance");
    }
}

fn resolve_junit_config(seed: u64) -> anyhow::Result<JunitConfig> {
    let exe =
        std::env::current_exe().map_err(|e| anyhow!("failed to find current executable: {e}"))?;
    let raw_stem = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("rigtest");
    let binary_stem = strip_hash_suffix(raw_stem).to_string();

    let output_path = match std::env::var("RIGTEST_JUNIT_OUTPUT_PATH").ok() {
        Some(p) => std::path::PathBuf::from(p),
        None => default_junit_output_path(&exe),
    };

    // When the parent invokes us it passes the target name verbatim so the
    // suite element matches the human-readable name even if the part file
    // is keyed by a unique executable stem. Fall back to deriving from the
    // current executable for direct-invocation use cases.
    let suite_name = std::env::var("RIGTEST_JUNIT_SUITE_NAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or(binary_stem);

    Ok(JunitConfig {
        output_path,
        suite_name,
        seed,
    })
}

/// Default to `<target>/rigtest/junit.xml` resolved by walking up from the
/// current exe to the `target` directory cargo built it into.
fn default_junit_output_path(exe: &std::path::Path) -> std::path::PathBuf {
    let target_dir = exe
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "target"))
        .map(std::path::Path::to_path_buf);

    target_dir
        .unwrap_or_else(|| std::path::PathBuf::from("target"))
        .join("rigtest")
        .join("junit.xml")
}

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

/// Filter `cases` to those matching `--tag` (if any) and not matching any
/// `--not-tag`. Both sets are deduplicated and compared case-sensitively
/// against each test's `tags` slice. Empty inputs are no-ops.
fn apply_tag_filter<'a>(
    cases: &[&'a crate::registry::TestCase],
    include: &std::collections::HashSet<&str>,
    exclude: &std::collections::HashSet<&str>,
) -> Vec<&'a crate::registry::TestCase> {
    cases
        .iter()
        .filter(|tc| {
            let included = include.is_empty() || tc.tags.iter().any(|t| include.contains(t));
            let excluded = !exclude.is_empty() && tc.tags.iter().any(|t| exclude.contains(t));
            included && !excluded
        })
        .copied()
        .collect()
}

/// Convert a list of CLI-supplied tag values into a deduplicated set,
/// stripping empty entries that result from inputs like `--tag smoke,,fast`.
fn tag_set(values: &[String]) -> std::collections::HashSet<&str> {
    values
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
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
async fn run_test<R: SubprocessRunner, P: TestEventReporter>(
    runner: &R,
    reporter: &P,
    tc: &crate::registry::TestCase,
    state_var: &str,
    state_json: &str,
) -> (Outcome, Duration) {
    let tref = test_ref(tc);
    reporter.test_started(tref);
    let test_start = Instant::now();
    let max_attempts = tc.retries + 1;
    let mut attempt_start = Instant::now();

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
        let attempt_duration = attempt_start.elapsed();

        match outcome {
            Ok(SubprocessOutcome::Passed) => {
                reporter.test_passed(tref, duration);
                return (Outcome::Passed, duration);
            }
            Ok(SubprocessOutcome::Skipped(reason)) => {
                reporter.test_skipped(tref, duration, &reason);
                return (Outcome::Skipped, duration);
            }
            Ok(SubprocessOutcome::Failed {
                reason,
                stdout,
                stderr,
            }) => {
                let report_outcome = classify_failed(&stderr);
                if is_last {
                    reporter.test_failed(tref, duration, report_outcome, &reason, &stdout, &stderr);
                    return (Outcome::Failed, duration);
                }
                reporter.test_retrying(
                    tref,
                    attempt,
                    max_attempts,
                    report_outcome,
                    &reason,
                    &stdout,
                    &stderr,
                    attempt_duration,
                );
            }
            Ok(SubprocessOutcome::TimedOut(dur)) => {
                let reason = format!("timed out after {:.1}s", dur.as_secs_f64());
                if is_last {
                    reporter.test_failed(tref, duration, ReportOutcome::Timeout, &reason, "", "");
                    return (Outcome::Failed, duration);
                }
                reporter.test_retrying(
                    tref,
                    attempt,
                    max_attempts,
                    ReportOutcome::Timeout,
                    &reason,
                    "",
                    "",
                    attempt_duration,
                );
            }
            Err(e) => {
                if is_last {
                    reporter.test_failed(
                        tref,
                        duration,
                        ReportOutcome::Crash,
                        &e.to_string(),
                        "",
                        "",
                    );
                    return (Outcome::Failed, duration);
                }
                reporter.test_retrying(
                    tref,
                    attempt,
                    max_attempts,
                    ReportOutcome::Crash,
                    &e.to_string(),
                    "",
                    "",
                    attempt_duration,
                );
            }
        }

        attempt_start = Instant::now();
    }

    unreachable!()
}

async fn dispatch_cases<R: SubprocessRunner, P: TestEventReporter>(
    runner: Arc<R>,
    reporter: Arc<P>,
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
        let reporter = Arc::clone(&reporter);
        let semaphore = Arc::clone(&semaphore);
        let state_var = state_var.clone();
        let state_json = state_json.clone();

        join_set.spawn(async move {
            let _permit = semaphore
                .acquire()
                .await
                .expect("semaphore should not be closed");
            let (outcome, _) = run_test(&*runner, &*reporter, tc, &state_var, &state_json).await;
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
        let (outcome, _) = run_test(&*runner, &*reporter, tc, &state_var, &state_json).await;
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

    let mut rng = rand::rng();
    let seed = args.seed.unwrap_or_else(|| rng.random::<u64>());

    let reporter = Arc::new(build_reporter(&args, seed)?);

    let global_setup = RIG_GLOBAL_SETUP.first();

    let global_data: Box<dyn std::any::Any + Send + Sync> = if let Some(entry) = global_setup {
        reporter.print_phase("global setup");
        (entry.setup_fn)().await
    } else {
        Box::new(())
    };

    let state_var = format!("RIG_STATE_{:016x}", rng.random::<u64>());
    let state_json: String = if let Some(entry) = global_setup {
        (entry.serialize_fn)(&*global_data)
    } else {
        String::new()
    };

    let cases_refs: Vec<&'static crate::registry::TestCase> = RIG_TEST_CASES.iter().collect();
    let name_filtered = apply_filter(&cases_refs, args.filter.as_deref());
    let include_tags = tag_set(&args.tag);
    let exclude_tags = tag_set(&args.not_tag);
    let mut cases = apply_tag_filter(&name_filtered, &include_tags, &exclude_tags);

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
        Arc::clone(&reporter),
        state_var,
        state_json,
        semaphore,
        parallel_cases,
        serial_cases,
    )
    .await;

    let elapsed = suite_start.elapsed();
    let finish_result = reporter.finish(passed, skipped, total, elapsed);

    if let Some(entry) = RIG_GLOBAL_TEARDOWN.first() {
        reporter.print_phase("global teardown");
        (entry.teardown_fn)(global_data).await;
    }

    let failed = total - passed - skipped;
    if failed > 0 {
        Err(anyhow!("Test suite failed: {passed}/{total} passed"))
    } else {
        // Surface a reporter (e.g. JUnit XML) write error as the run's
        // exit so a CI consumer that promised an artifact gets a hard fail
        // rather than a misleading green.
        finish_result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::TestContext;
    use crate::registry::{BoxFuture, TestCase};
    use crate::reporter::{Event, NullReporter, RecordingReporter};
    use std::sync::Mutex;

    fn make_case(name: &'static str) -> TestCase {
        TestCase {
            name,
            module: "test_module",
            file: "test.rs",
            serial: false,
            timeout: None,
            retries: 0,
            tags: &[],
            test_fn: |_ctx: Arc<TestContext>| -> BoxFuture<
                'static,
                Result<(), Box<dyn std::error::Error + Send + Sync>>,
            > { Box::pin(async { Ok(()) }) },
        }
    }

    fn case_with_tags(name: &'static str, tags: &'static [&'static str]) -> TestCase {
        let mut tc = make_case(name);
        tc.tags = tags;
        tc
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

    // ── Tag filter ──────────────────────────────────────────────────────

    #[test]
    fn tag_filter_with_empty_sets_returns_all() {
        let cases = [
            case_with_tags("a", &["smoke"]),
            case_with_tags("b", &[]),
            case_with_tags("c", &["regression"]),
        ];
        let refs: Vec<&TestCase> = cases.iter().collect();
        let include = std::collections::HashSet::new();
        let exclude = std::collections::HashSet::new();
        assert_eq!(apply_tag_filter(&refs, &include, &exclude).len(), 3);
    }

    #[test]
    fn tag_filter_include_keeps_tests_matching_any_tag() {
        let cases = [
            case_with_tags("smoke_only", &["smoke"]),
            case_with_tags("regression_only", &["regression"]),
            case_with_tags("both", &["smoke", "regression"]),
            case_with_tags("untagged", &[]),
        ];
        let refs: Vec<&TestCase> = cases.iter().collect();
        let include: std::collections::HashSet<&str> = ["smoke"].into_iter().collect();
        let exclude = std::collections::HashSet::new();
        let filtered = apply_tag_filter(&refs, &include, &exclude);
        let names: Vec<&str> = filtered.iter().map(|tc| tc.name).collect();
        assert_eq!(names, vec!["smoke_only", "both"]);
    }

    #[test]
    fn tag_filter_include_multiple_unions() {
        let cases = [
            case_with_tags("smoke_only", &["smoke"]),
            case_with_tags("regression_only", &["regression"]),
            case_with_tags("slow_only", &["slow"]),
        ];
        let refs: Vec<&TestCase> = cases.iter().collect();
        let include: std::collections::HashSet<&str> =
            ["smoke", "regression"].into_iter().collect();
        let exclude = std::collections::HashSet::new();
        let filtered = apply_tag_filter(&refs, &include, &exclude);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn tag_filter_exclude_drops_tests_matching_any_tag() {
        let cases = [
            case_with_tags("fast", &["smoke"]),
            case_with_tags("slow_smoke", &["smoke", "slow"]),
            case_with_tags("untagged", &[]),
        ];
        let refs: Vec<&TestCase> = cases.iter().collect();
        let include = std::collections::HashSet::new();
        let exclude: std::collections::HashSet<&str> = ["slow"].into_iter().collect();
        let filtered = apply_tag_filter(&refs, &include, &exclude);
        let names: Vec<&str> = filtered.iter().map(|tc| tc.name).collect();
        assert_eq!(names, vec!["fast", "untagged"]);
    }

    #[test]
    fn tag_filter_include_and_exclude_compose_with_and() {
        let cases = [
            case_with_tags("smoke_fast", &["smoke"]),
            case_with_tags("smoke_slow", &["smoke", "slow"]),
            case_with_tags("regression_fast", &["regression"]),
        ];
        let refs: Vec<&TestCase> = cases.iter().collect();
        let include: std::collections::HashSet<&str> = ["smoke"].into_iter().collect();
        let exclude: std::collections::HashSet<&str> = ["slow"].into_iter().collect();
        let filtered = apply_tag_filter(&refs, &include, &exclude);
        let names: Vec<&str> = filtered.iter().map(|tc| tc.name).collect();
        assert_eq!(names, vec!["smoke_fast"]);
    }

    #[test]
    fn tag_set_dedupes_and_drops_empty() {
        let values = vec![
            "smoke".to_string(),
            String::new(),
            "smoke".to_string(),
            "regression".to_string(),
        ];
        let set = tag_set(&values);
        assert_eq!(set.len(), 2);
        assert!(set.contains("smoke"));
        assert!(set.contains("regression"));
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
        let reporter = NullReporter;

        let (outcome, _) = run_test(&runner, &reporter, &tc, "X", "{}").await;

        assert!(matches!(outcome, Outcome::Passed));
        assert_eq!(runner.call_count(), 2);
    }

    #[tokio::test]
    async fn skip_does_not_retry() {
        let runner = FakeRunner::new(vec![SubprocessOutcome::Skipped("nope".into())]);
        let tc = case_with_retries("skipper", 3);
        let reporter = NullReporter;

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
        let reporter = NullReporter;

        let (outcome, _) = run_test(&runner, &reporter, &tc, "X", "{}").await;

        assert!(matches!(outcome, Outcome::Failed));
        assert_eq!(runner.call_count(), 3); // initial + 2 retries
    }

    // ── Reporter seam: assert on the event sequence ──────────────────────

    #[test]
    fn classify_failed_detects_panic_marker() {
        let panic = "thread 'main' panicked at src/lib.rs:1\n";
        assert!(matches!(classify_failed(panic), ReportOutcome::Panic));
        assert!(matches!(
            classify_failed("error: boom"),
            ReportOutcome::Assertion
        ));
        assert!(matches!(classify_failed(""), ReportOutcome::Assertion));
    }

    #[tokio::test]
    async fn retry_emits_retrying_event_before_passed() {
        let runner = FakeRunner::new(vec![
            SubprocessOutcome::Failed {
                reason: "first failure".into(),
                stdout: String::new(),
                stderr: String::new(),
            },
            SubprocessOutcome::Passed,
        ]);
        let tc = case_with_retries("flaky", 1);
        let reporter = RecordingReporter::new();

        let (outcome, _) = run_test(&runner, &reporter, &tc, "X", "{}").await;

        assert!(matches!(outcome, Outcome::Passed));
        let events = reporter.events();
        assert!(matches!(events[0], Event::Started(ref n) if n == "flaky"));
        assert!(
            matches!(events[1], Event::Retrying(ref n, 1, 2, _, _) if n == "flaky"),
            "expected Retrying(flaky, 1/2) at index 1, got {:?}",
            events[1]
        );
        assert!(matches!(events[2], Event::Passed(ref n) if n == "flaky"));
        assert_eq!(events.len(), 3);
    }

    // ── Dispatch tests: serial/parallel ordering, semaphore cap, counts ──

    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn leaked_case(name: &'static str, serial: bool) -> &'static TestCase {
        let mut tc = make_case(name);
        tc.serial = serial;
        Box::leak(Box::new(tc))
    }

    /// Returns a pre-programmed outcome per test name; otherwise Passed.
    struct ByNameRunner {
        outcomes: HashMap<&'static str, SubprocessOutcome>,
    }

    impl SubprocessRunner for ByNameRunner {
        async fn run(&self, req: SpawnRequest<'_>) -> anyhow::Result<SubprocessOutcome> {
            Ok(self
                .outcomes
                .get(req.test_name)
                .cloned()
                .unwrap_or(SubprocessOutcome::Passed))
        }
    }

    #[tokio::test]
    async fn dispatch_counts_pass_skip_fail_correctly() {
        let mut outcomes = HashMap::new();
        outcomes.insert("a", SubprocessOutcome::Passed);
        outcomes.insert("b", SubprocessOutcome::Skipped("nope".into()));
        outcomes.insert(
            "c",
            SubprocessOutcome::Failed {
                reason: "boom".into(),
                stdout: String::new(),
                stderr: String::new(),
            },
        );
        outcomes.insert("d", SubprocessOutcome::Passed);
        outcomes.insert("e", SubprocessOutcome::Passed);
        let runner = Arc::new(ByNameRunner { outcomes });
        let reporter = Arc::new(NullReporter);
        let semaphore = Arc::new(Semaphore::new(4));

        let cases: Vec<&'static TestCase> = ["a", "b", "c", "d", "e"]
            .into_iter()
            .map(|n| leaked_case(n, false))
            .collect();

        let (passed, skipped) = dispatch_cases(
            runner,
            reporter,
            "X".into(),
            "{}".into(),
            semaphore,
            cases,
            Vec::new(),
        )
        .await;

        assert_eq!(passed, 3);
        assert_eq!(skipped, 1);
        // failed = total - passed - skipped = 5 - 3 - 1 = 1
    }

    #[tokio::test]
    async fn dispatch_runs_serial_cases_after_all_parallel() {
        let runner = Arc::new(ByNameRunner {
            outcomes: HashMap::new(),
        });
        let reporter = Arc::new(RecordingReporter::new());
        let semaphore = Arc::new(Semaphore::new(2));

        let parallel = vec![
            leaked_case("p1", false),
            leaked_case("p2", false),
            leaked_case("p3", false),
        ];
        let serial = vec![leaked_case("s1", true), leaked_case("s2", true)];

        let _ = dispatch_cases(
            Arc::clone(&runner),
            Arc::clone(&reporter),
            "X".into(),
            "{}".into(),
            semaphore,
            parallel,
            serial,
        )
        .await;

        let events = reporter.events();
        let started: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                Event::Started(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        // Every "p*" must come before every "s*".
        let last_parallel_idx = started
            .iter()
            .rposition(|n| n.starts_with('p'))
            .expect("at least one parallel started");
        let first_serial_idx = started
            .iter()
            .position(|n| n.starts_with('s'))
            .expect("at least one serial started");
        assert!(
            last_parallel_idx < first_serial_idx,
            "expected all parallel cases to start before any serial case, got started order: {started:?}"
        );
    }

    /// Runner that records the maximum number of concurrent in-flight calls
    /// observed at any point.
    struct ConcurrencyRunner {
        active: AtomicUsize,
        max_observed: AtomicUsize,
    }

    impl ConcurrencyRunner {
        fn new() -> Self {
            Self {
                active: AtomicUsize::new(0),
                max_observed: AtomicUsize::new(0),
            }
        }
    }

    impl SubprocessRunner for ConcurrencyRunner {
        async fn run(&self, _req: SpawnRequest<'_>) -> anyhow::Result<SubprocessOutcome> {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_observed.fetch_max(now, Ordering::SeqCst);
            // Yield so other tasks can interleave and bump `active` if they
            // are allowed to.
            tokio::time::sleep(Duration::from_millis(10)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(SubprocessOutcome::Passed)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dispatch_respects_semaphore_cap() {
        let runner = Arc::new(ConcurrencyRunner::new());
        let reporter = Arc::new(NullReporter);
        let semaphore = Arc::new(Semaphore::new(2));

        let cases: Vec<&'static TestCase> = (0..10)
            .map(|i| {
                let name: &'static str = Box::leak(format!("t{i}").into_boxed_str());
                leaked_case(name, false)
            })
            .collect();

        let _ = dispatch_cases(
            Arc::clone(&runner),
            reporter,
            "X".into(),
            "{}".into(),
            semaphore,
            cases,
            Vec::new(),
        )
        .await;

        let max = runner.max_observed.load(Ordering::SeqCst);
        assert!(
            max <= 2,
            "semaphore cap of 2 violated: max concurrent was {max}"
        );
        assert!(
            max >= 1,
            "expected some concurrency to be observed, got {max}"
        );
    }
}
