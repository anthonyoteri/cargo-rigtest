use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use rand::seq::SliceRandom as _;
use rand::RngExt as _;
use rand::SeedableRng as _;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::junit::{JunitConfig, JunitReporter};
use crate::preflight_runner::{run_preflight, PreflightReport};
use crate::registry::{
    RIG_DEFAULT_TIMEOUT, RIG_GLOBAL_SETUP, RIG_GLOBAL_TEARDOWN, RIG_PREFLIGHT, RIG_TEST_CASES,
};
use crate::reporter::{MultiReporter, Reporter, TestEventReporter, TestRef};
use crate::schedule::{Phase, PlannedCase, Schedule};
use crate::scheduler::RuntimeArgs;
use crate::subprocess::{OsSubprocessRunner, SubprocessRunner};

/// Sentinel error returned by [`run`] when the preflight phase aborts the
/// suite. `run_main` downcasts this to translate the abort into exit code
/// `2` (distinct from the `1` used for test failures).
#[derive(Debug)]
pub(crate) struct PreflightAbort(pub String);

impl std::fmt::Display for PreflightAbort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PreflightAbort {}

/// Result of [`handle_preflight_phase`] — either continue into the test
/// phase or bail with a populated [`PreflightAbort`].
enum ControlFlow {
    Continue,
    Abort(anyhow::Error),
}

/// Run preflight as part of a normal suite run, plumb results into the
/// reporter, and decide whether the run should continue. Encapsulated so
/// `run` stays under the pedantic 100-line ceiling.
async fn handle_preflight_phase(
    args: &RuntimeArgs,
    reporter: &Arc<MultiReporter>,
) -> anyhow::Result<ControlFlow> {
    let report = if args.no_preflight {
        PreflightReport::none()
    } else {
        match run_preflight().await {
            Ok(report) => report,
            Err(e) => return Err(anyhow::Error::new(PreflightAbort(e.to_string()))),
        }
    };

    if report.verdict.declared && !report.results.is_empty() {
        reporter.preflight_recorded(&report.results);
    }

    if !report.verdict.passed && !args.continue_on_preflight_failure {
        // Still let the JUnit reporter flush — otherwise the preflight
        // testsuite we just recorded would never reach disk.
        let _ = reporter.finish(0, 0, 0, Duration::ZERO);
        return Ok(ControlFlow::Abort(anyhow::Error::new(PreflightAbort(
            format_abort_message(report.verdict.failed_count, RIG_TEST_CASES.len()),
        ))));
    }

    Ok(ControlFlow::Continue)
}

/// Handle `--preflight-only`: run the readiness check (when one is
/// declared) and exit with the right code. Skips both `#[global_setup]`
/// and the test phase by design.
async fn handle_preflight_only(args: &RuntimeArgs) -> anyhow::Result<()> {
    if args.no_preflight || RIG_PREFLIGHT.is_empty() {
        println!("no preflight declared");
        return Ok(());
    }
    let report = run_preflight()
        .await
        .map_err(|e| anyhow::Error::new(PreflightAbort(e.to_string())))?;
    if report.verdict.passed {
        return Ok(());
    }
    Err(anyhow::Error::new(PreflightAbort(format_abort_message(
        report.verdict.failed_count,
        RIG_TEST_CASES.len(),
    ))))
}

/// Format the abort message printed when preflight failures stop a suite.
/// Lives here because the Coordinator owns the abort policy; the runner
/// no longer needs to know what an abort message looks like or how many
/// tests are in the registry.
fn format_abort_message(failed_count: usize, tests_total: usize) -> String {
    format!(
        "{failed_count} probe{plural} failed — aborting suite ({tests_total} tests not run)",
        plural = if failed_count == 1 { "" } else { "s" },
    )
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

/// Run a planned test with retries, returning the final outcome and updating
/// the reporter.
///
/// The timeout and retry budget are already resolved on the [`PlannedCase`]
/// (see [`Schedule::plan`]); the test's `retry_on_error` matcher (if any) is
/// left in force regardless. When a matcher is in force, panics, timeouts, and
/// subprocess crashes are not retried — only failures whose typed `Err(_)`
/// matched the pattern are retried.
async fn run_test<R: SubprocessRunner, P: TestEventReporter>(
    runner: &R,
    reporter: &P,
    pc: &PlannedCase,
    state_var: &str,
    state_json: &str,
) -> (Outcome, Duration) {
    let tc = pc.case;
    let tref = test_ref(tc);
    reporter.test_started(tref);
    let test_start = Instant::now();
    let max_attempts = pc.max_attempts;
    let mut attempt_start = Instant::now();

    for attempt in 1..=max_attempts {
        let raw = runner.run(tc.name, state_var, state_json, pc.timeout).await;

        let is_last = attempt == max_attempts;
        let duration = test_start.elapsed();
        let attempt_duration = attempt_start.elapsed();

        match crate::retry::plan(raw, tc) {
            crate::retry::AttemptPlan::Passed => {
                reporter.test_passed(tref, duration);
                return (Outcome::Passed, duration);
            }
            crate::retry::AttemptPlan::Skipped { reason } => {
                reporter.test_skipped(tref, duration, &reason);
                return (Outcome::Skipped, duration);
            }
            crate::retry::AttemptPlan::Failed {
                kind,
                reason,
                stdout,
                stderr,
                retryable,
            } => {
                if !is_last && retryable {
                    reporter.test_retrying(
                        tref,
                        attempt,
                        max_attempts,
                        kind,
                        &reason,
                        &stdout,
                        &stderr,
                        attempt_duration,
                    );
                } else {
                    reporter.test_failed(tref, duration, kind, &reason, &stdout, &stderr);
                    return (Outcome::Failed, duration);
                }
            }
        }

        attempt_start = Instant::now();
    }

    unreachable!()
}

/// Execute a [`Schedule`]: run the parallel phase concurrently, then the
/// exclusive phase sequentially. Returns `(passed, skipped)`; the caller
/// derives `failed = total - passed - skipped`.
async fn execute<R: SubprocessRunner, P: TestEventReporter>(
    schedule: Schedule,
    runner: Arc<R>,
    reporter: Arc<P>,
    state_var: String,
    state_json: String,
) -> (usize, usize) {
    let mut passed = 0usize;
    let mut skipped = 0usize;

    for phase in schedule.phases {
        match phase {
            Phase::Parallel { cases, cap, groups } => {
                let (p, s) = run_parallel_phase(
                    &runner,
                    &reporter,
                    &state_var,
                    &state_json,
                    cases,
                    cap,
                    &groups,
                )
                .await;
                passed += p;
                skipped += s;
            }
            Phase::Exclusive { cases } => {
                for pc in cases {
                    let (outcome, _) =
                        run_test(&*runner, &*reporter, &pc, &state_var, &state_json).await;
                    tally(outcome, &mut passed, &mut skipped);
                }
            }
        }
    }

    (passed, skipped)
}

fn tally(outcome: Outcome, passed: &mut usize, skipped: &mut usize) {
    match outcome {
        Outcome::Passed => *passed += 1,
        Outcome::Skipped => *skipped += 1,
        Outcome::Failed => {}
    }
}

/// Run the parallel phase on a `JoinSet`: each task acquires the global cap
/// permit, then its serial-group permit (if any), so same-group cases never
/// overlap while different groups stay concurrent.
async fn run_parallel_phase<R: SubprocessRunner, P: TestEventReporter>(
    runner: &Arc<R>,
    reporter: &Arc<P>,
    state_var: &str,
    state_json: &str,
    cases: Vec<PlannedCase>,
    cap: usize,
    groups: &[&'static str],
) -> (usize, usize) {
    let semaphore = Arc::new(Semaphore::new(cap));
    let group_locks: std::collections::HashMap<&'static str, Arc<Semaphore>> = groups
        .iter()
        .map(|g| (*g, Arc::new(Semaphore::new(1))))
        .collect();

    let mut join_set: JoinSet<Outcome> = JoinSet::new();
    for pc in cases {
        let runner = Arc::clone(runner);
        let reporter = Arc::clone(reporter);
        let semaphore = Arc::clone(&semaphore);
        let group_lock = pc.serial_group.map(|g| Arc::clone(&group_locks[g]));
        let state_var = state_var.to_owned();
        let state_json = state_json.to_owned();

        join_set.spawn(async move {
            let _permit = semaphore
                .acquire()
                .await
                .expect("semaphore should not be closed");
            let _group_permit = match &group_lock {
                Some(lock) => Some(
                    lock.acquire()
                        .await
                        .expect("group semaphore should not be closed"),
                ),
                None => None,
            };
            let (outcome, _) = run_test(&*runner, &*reporter, &pc, &state_var, &state_json).await;
            outcome
        });
    }

    let mut passed = 0usize;
    let mut skipped = 0usize;
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(outcome) => tally(outcome, &mut passed, &mut skipped),
            Err(e) => eprintln!("cargo-rigtest: task join error: {e}"),
        }
    }

    (passed, skipped)
}

/// Assert that every "at most one" registration slice holds zero or one
/// entry. Any violation is a build-time wiring mistake (two `#[global_setup]`
/// functions, two `default_timeout` declarations, …) that must fail loudly.
fn assert_singleton_registrations() {
    assert!(
        RIG_PREFLIGHT.len() <= 1,
        "cargo-rigtest: at most one #[preflight] function may be defined, found {}",
        RIG_PREFLIGHT.len()
    );
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
    assert!(
        RIG_DEFAULT_TIMEOUT.len() <= 1,
        "cargo-rigtest: at most one #[rigtest::main(default_timeout = …)] may be defined, found {}",
        RIG_DEFAULT_TIMEOUT.len()
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
/// Panics if more than one singleton registration is declared — that is,
/// more than one `#[preflight]`, `#[global_setup]`, `#[global_teardown]`,
/// `default_timeout`, or client configurator.
pub(crate) async fn run(args: RuntimeArgs) -> anyhow::Result<()> {
    assert_singleton_registrations();

    let mut rng = rand::rng();
    let seed = args.seed.unwrap_or_else(|| rng.random::<u64>());

    let reporter = Arc::new(build_reporter(&args, seed)?);

    if args.preflight_only {
        return handle_preflight_only(&args).await;
    }

    if let ControlFlow::Abort(err) = handle_preflight_phase(&args, &reporter).await? {
        return Err(err);
    }

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
    let cap = if args.no_capture {
        1
    } else {
        args.jobs.unwrap_or_else(default_jobs)
    };

    let exe =
        std::env::current_exe().map_err(|e| anyhow!("failed to find current executable: {e}"))?;

    let runner = Arc::new(OsSubprocessRunner::new(exe, args.no_capture));

    let suite_start = Instant::now();

    let retries_override = args.retries;
    let suite_default_timeout = RIG_DEFAULT_TIMEOUT.first().map(|e| e.timeout);
    let schedule = Schedule::plan(cases, cap, retries_override, suite_default_timeout);

    let (passed, skipped) = execute(
        schedule,
        runner,
        Arc::clone(&reporter),
        state_var,
        state_json,
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
    use crate::protocol::SubprocessOutcome;
    use crate::registry::{BoxFuture, TestCase};
    use crate::reporter::{Event, NullReporter, RecordingReporter};
    use std::sync::Mutex;

    fn make_case(name: &'static str) -> TestCase {
        TestCase::new(
            name,
            "test_module",
            "test.rs",
            false,
            None,
            None,
            false,
            0,
            false,
            &[],
            |_ctx: Arc<TestContext>| -> BoxFuture<
                'static,
                Result<(), Box<dyn std::error::Error + Send + Sync>>,
            > { Box::pin(async { Ok(()) }) },
        )
    }

    fn case_with_tags(name: &'static str, tags: &'static [&'static str]) -> TestCase {
        let mut tc = make_case(name);
        tc.tags = tags;
        tc
    }

    // Effective-timeout precedence now lives in the pure plan; see the
    // `schedule::tests` timeout_* tests.

    /// Wrap an owned test case in a `PlannedCase`, resolving its retry budget
    /// the same way `Schedule::plan` does. These retry tests never set a
    /// per-case timeout, so `timeout` resolves to `None`. Leaks the case for
    /// the `'static` bound the executor requires — acceptable in tests.
    fn plan_case(tc: TestCase, retries_override: Option<usize>) -> PlannedCase {
        let effective =
            retries_override.map_or(tc.retries, |n| u32::try_from(n).unwrap_or(u32::MAX));
        PlannedCase {
            timeout: tc.timeout,
            max_attempts: effective.saturating_add(1),
            serial_group: tc.serial_group,
            case: Box::leak(Box::new(tc)),
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
        async fn run(
            &self,
            _test_name: &str,
            _state_var: &str,
            _state_json: &str,
            _timeout: Option<Duration>,
        ) -> anyhow::Result<SubprocessOutcome> {
            *self.calls.lock().unwrap() += 1;
            Ok(self.queue.lock().unwrap().remove(0))
        }
    }

    fn case_with_retries(name: &'static str, retries: u32) -> TestCase {
        let mut tc = make_case(name);
        tc.retries = retries;
        tc
    }

    fn mk_failed(reason: &str, retry_eligible: bool) -> SubprocessOutcome {
        SubprocessOutcome::Failed {
            reason: reason.into(),
            stdout: String::new(),
            stderr: String::new(),
            retry_eligible,
        }
    }

    fn case_with_retry_on_error(name: &'static str, retries: u32) -> TestCase {
        let mut tc = case_with_retries(name, retries);
        tc.retry_on_error_set = true;
        tc
    }

    #[tokio::test]
    async fn retry_loop_succeeds_after_one_failure() {
        let runner = FakeRunner::new(vec![
            mk_failed("transient", true),
            SubprocessOutcome::Passed,
        ]);
        let tc = case_with_retries("flaky", 1);
        let reporter = NullReporter;

        let (outcome, _) = run_test(&runner, &reporter, &plan_case(tc, None), "X", "{}").await;

        assert!(matches!(outcome, Outcome::Passed));
        assert_eq!(runner.call_count(), 2);
    }

    #[tokio::test]
    async fn skip_does_not_retry() {
        let runner = FakeRunner::new(vec![SubprocessOutcome::Skipped("nope".into())]);
        let tc = case_with_retries("skipper", 3);
        let reporter = NullReporter;

        let (outcome, _) = run_test(&runner, &reporter, &plan_case(tc, None), "X", "{}").await;

        assert!(matches!(outcome, Outcome::Skipped));
        assert_eq!(runner.call_count(), 1);
    }

    #[tokio::test]
    async fn retry_exhausts_and_reports_failure() {
        let runner = FakeRunner::new(vec![
            mk_failed("boom", true),
            mk_failed("boom", true),
            mk_failed("boom", true),
        ]);
        let tc = case_with_retries("always_fails", 2);
        let reporter = NullReporter;

        let (outcome, _) = run_test(&runner, &reporter, &plan_case(tc, None), "X", "{}").await;

        assert!(matches!(outcome, Outcome::Failed));
        assert_eq!(runner.call_count(), 3); // initial + 2 retries
    }

    // ── retry_on_error matcher: subprocess-side eligibility hint ──

    #[tokio::test]
    async fn non_retry_eligible_failure_fails_immediately() {
        let runner = FakeRunner::new(vec![mk_failed("assertion failure", false)]);
        let tc = case_with_retry_on_error("strict_matcher", 5);
        let reporter = NullReporter;

        let (outcome, _) = run_test(&runner, &reporter, &plan_case(tc, None), "X", "{}").await;

        assert!(matches!(outcome, Outcome::Failed));
        assert_eq!(
            runner.call_count(),
            1,
            "non-matching error must not consume a retry attempt"
        );
    }

    #[tokio::test]
    async fn retry_eligible_failure_with_matcher_retries() {
        let runner = FakeRunner::new(vec![
            mk_failed("transient", true),
            SubprocessOutcome::Passed,
        ]);
        let tc = case_with_retry_on_error("flaky_with_matcher", 2);
        let reporter = NullReporter;

        let (outcome, _) = run_test(&runner, &reporter, &plan_case(tc, None), "X", "{}").await;

        assert!(matches!(outcome, Outcome::Passed));
        assert_eq!(runner.call_count(), 2);
    }

    #[tokio::test]
    async fn timeout_with_matcher_does_not_retry() {
        let runner = FakeRunner::new(vec![SubprocessOutcome::TimedOut(Duration::from_secs(1))]);
        let tc = case_with_retry_on_error("times_out_matcher", 3);
        let reporter = NullReporter;

        let (outcome, _) = run_test(&runner, &reporter, &plan_case(tc, None), "X", "{}").await;

        assert!(matches!(outcome, Outcome::Failed));
        assert_eq!(
            runner.call_count(),
            1,
            "timeout with retry_on_error set must not consume a retry"
        );
    }

    #[tokio::test]
    async fn timeout_without_matcher_retries() {
        let runner = FakeRunner::new(vec![
            SubprocessOutcome::TimedOut(Duration::from_secs(1)),
            SubprocessOutcome::Passed,
        ]);
        let tc = case_with_retries("times_out_then_passes", 2);
        let reporter = NullReporter;

        let (outcome, _) = run_test(&runner, &reporter, &plan_case(tc, None), "X", "{}").await;

        assert!(matches!(outcome, Outcome::Passed));
        assert_eq!(runner.call_count(), 2);
    }

    #[tokio::test]
    async fn cli_override_replaces_declared_retry_count() {
        let runner = FakeRunner::new(vec![
            mk_failed("transient", true),
            mk_failed("transient", true),
            SubprocessOutcome::Passed,
        ]);
        // Declared `retries = 0` but CLI override bumps to 5.
        let tc = case_with_retries("override_total", 0);
        let reporter = NullReporter;

        let (outcome, _) = run_test(&runner, &reporter, &plan_case(tc, Some(5)), "X", "{}").await;

        assert!(matches!(outcome, Outcome::Passed));
        assert_eq!(runner.call_count(), 3);
    }

    #[tokio::test]
    async fn cli_override_zero_disables_declared_retries() {
        let runner = FakeRunner::new(vec![mk_failed("boom", true)]);
        // Declared `retries = 3` but `--retries 0` forces strict mode.
        let tc = case_with_retries("strict_run", 3);
        let reporter = NullReporter;

        let (outcome, _) = run_test(&runner, &reporter, &plan_case(tc, Some(0)), "X", "{}").await;

        assert!(matches!(outcome, Outcome::Failed));
        assert_eq!(runner.call_count(), 1);
    }

    #[tokio::test]
    async fn cli_override_leaves_matcher_in_force() {
        let runner = FakeRunner::new(vec![mk_failed("not matching", false)]);
        let tc = case_with_retry_on_error("override_keeps_matcher", 0);
        let reporter = NullReporter;

        // Even with the operator bumping to 10 retries, a non-matching
        // error must still fail-fast — the override replaces the count
        // but not the matcher.
        let (outcome, _) = run_test(&runner, &reporter, &plan_case(tc, Some(10)), "X", "{}").await;

        assert!(matches!(outcome, Outcome::Failed));
        assert_eq!(runner.call_count(), 1);
    }

    // ── Reporter seam: assert on the event sequence ──────────────────────

    #[tokio::test]
    async fn retry_emits_retrying_event_before_passed() {
        let runner = FakeRunner::new(vec![
            mk_failed("first failure", true),
            SubprocessOutcome::Passed,
        ]);
        let tc = case_with_retries("flaky", 1);
        let reporter = RecordingReporter::new();

        let (outcome, _) = run_test(&runner, &reporter, &plan_case(tc, None), "X", "{}").await;

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

    fn leaked_group_case(name: &'static str, group: &'static str) -> &'static TestCase {
        let mut tc = make_case(name);
        tc.serial_group = Some(group);
        Box::leak(Box::new(tc))
    }

    /// Returns a pre-programmed outcome per test name; otherwise Passed.
    struct ByNameRunner {
        outcomes: HashMap<&'static str, SubprocessOutcome>,
    }

    impl SubprocessRunner for ByNameRunner {
        async fn run(
            &self,
            test_name: &str,
            _state_var: &str,
            _state_json: &str,
            _timeout: Option<Duration>,
        ) -> anyhow::Result<SubprocessOutcome> {
            Ok(self
                .outcomes
                .get(test_name)
                .cloned()
                .unwrap_or(SubprocessOutcome::Passed))
        }
    }

    #[tokio::test]
    async fn dispatch_counts_pass_skip_fail_correctly() {
        let mut outcomes = HashMap::new();
        outcomes.insert("a", SubprocessOutcome::Passed);
        outcomes.insert("b", SubprocessOutcome::Skipped("nope".into()));
        outcomes.insert("c", mk_failed("boom", true));
        outcomes.insert("d", SubprocessOutcome::Passed);
        outcomes.insert("e", SubprocessOutcome::Passed);
        let runner = Arc::new(ByNameRunner { outcomes });
        let reporter = Arc::new(NullReporter);

        let cases: Vec<&'static TestCase> = ["a", "b", "c", "d", "e"]
            .into_iter()
            .map(|n| leaked_case(n, false))
            .collect();
        let schedule = Schedule::plan(cases, 4, None, None);

        let (passed, skipped) = execute(schedule, runner, reporter, "X".into(), "{}".into()).await;

        assert_eq!(passed, 3);
        assert_eq!(skipped, 1);
        // failed = total - passed - skipped = 5 - 3 - 1 = 1
    }

    // Serial-runs-after-parallel is now a data invariant of the plan (the
    // `Exclusive` phase always follows `Parallel`); see
    // `schedule::tests::exclusive_phase_follows_parallel_phase`. `execute`
    // just iterates the phases in order, so no live-tokio test is needed.

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
        async fn run(
            &self,
            _test_name: &str,
            _state_var: &str,
            _state_json: &str,
            _timeout: Option<Duration>,
        ) -> anyhow::Result<SubprocessOutcome> {
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

        let cases: Vec<&'static TestCase> = (0..10)
            .map(|i| {
                let name: &'static str = Box::leak(format!("t{i}").into_boxed_str());
                leaked_case(name, false)
            })
            .collect();
        let schedule = Schedule::plan(cases, 2, None, None);

        let _ = execute(
            schedule,
            Arc::clone(&runner),
            reporter,
            "X".into(),
            "{}".into(),
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

    /// Runner keyed by a test-name → group map. Tracks the max concurrency
    /// observed *within* each group so a same-group overlap is detectable.
    struct GroupConcurrencyRunner {
        groups: HashMap<&'static str, &'static str>,
        active: Mutex<HashMap<&'static str, usize>>,
        per_group_max: Mutex<HashMap<&'static str, usize>>,
        overall_max: AtomicUsize,
        overall_active: AtomicUsize,
        /// Optional rendezvous: when set, every task blocks here after
        /// recording its in-flight count, so a cross-group concurrency
        /// assertion is deterministic rather than timing-based. Left `None`
        /// for the mutual-exclusion test, where a barrier would deadlock
        /// (same-group tasks can never be in flight together).
        rendezvous: Option<Arc<tokio::sync::Barrier>>,
    }

    impl GroupConcurrencyRunner {
        fn new(groups: HashMap<&'static str, &'static str>) -> Self {
            Self {
                groups,
                active: Mutex::new(HashMap::new()),
                per_group_max: Mutex::new(HashMap::new()),
                overall_max: AtomicUsize::new(0),
                overall_active: AtomicUsize::new(0),
                rendezvous: None,
            }
        }

        fn with_rendezvous(groups: HashMap<&'static str, &'static str>, parties: usize) -> Self {
            let mut runner = Self::new(groups);
            runner.rendezvous = Some(Arc::new(tokio::sync::Barrier::new(parties)));
            runner
        }
    }

    impl SubprocessRunner for GroupConcurrencyRunner {
        async fn run(
            &self,
            test_name: &str,
            _state_var: &str,
            _state_json: &str,
            _timeout: Option<Duration>,
        ) -> anyhow::Result<SubprocessOutcome> {
            let group = *self.groups.get(test_name).expect("known test name");
            {
                let mut active = self.active.lock().unwrap();
                let n = active.entry(group).or_insert(0);
                *n += 1;
                let now = *n;
                let mut maxes = self.per_group_max.lock().unwrap();
                let m = maxes.entry(group).or_insert(0);
                *m = (*m).max(now);
            }
            let overall = self.overall_active.fetch_add(1, Ordering::SeqCst) + 1;
            self.overall_max.fetch_max(overall, Ordering::SeqCst);
            // With a rendezvous, every task must arrive before any proceeds, so
            // cross-group overlap is provable; otherwise sleep so a same-group
            // sibling has a real chance to overlap if the lock were broken.
            if let Some(barrier) = &self.rendezvous {
                barrier.wait().await;
            } else {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            self.overall_active.fetch_sub(1, Ordering::SeqCst);
            *self.active.lock().unwrap().get_mut(group).unwrap() -= 1;
            Ok(SubprocessOutcome::Passed)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dispatch_serial_group_is_mutually_exclusive() {
        let groups: HashMap<&str, &str> = [("db1", "db"), ("db2", "db")].into_iter().collect();
        let runner = Arc::new(GroupConcurrencyRunner::new(groups));
        let reporter = Arc::new(NullReporter);

        // Cap high enough that only the group lock can serialize them.
        let cases = vec![
            leaked_group_case("db1", "db"),
            leaked_group_case("db2", "db"),
        ];
        let schedule = Schedule::plan(cases, 4, None, None);

        let _ = execute(
            schedule,
            Arc::clone(&runner),
            reporter,
            "X".into(),
            "{}".into(),
        )
        .await;

        let db_max = *runner.per_group_max.lock().unwrap().get("db").unwrap();
        assert_eq!(db_max, 1, "same-group cases must never overlap");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dispatch_different_groups_run_concurrently() {
        // The plan lists distinct group names, but the runtime guarantee —
        // distinct groups map to distinct locks and can therefore overlap —
        // lives in `run_parallel_phase`'s lock-map construction, which the pure
        // plan tests do not exercise. This guards that layer: a regression that
        // shared one lock across groups (or looked up the wrong one) would
        // over-serialize and fail the `overall_max >= 2` assertion.
        let groups: HashMap<&str, &str> = [("a1", "a"), ("b1", "b")].into_iter().collect();
        // Two parties rendezvous: both must be in flight to pass the barrier,
        // so `overall_max == 2` is deterministic, not timing-dependent.
        let runner = Arc::new(GroupConcurrencyRunner::with_rendezvous(groups, 2));
        let reporter = Arc::new(NullReporter);

        let cases = vec![leaked_group_case("a1", "a"), leaked_group_case("b1", "b")];
        let schedule = Schedule::plan(cases, 4, None, None);

        let _ = execute(
            schedule,
            Arc::clone(&runner),
            reporter,
            "X".into(),
            "{}".into(),
        )
        .await;

        let overall = runner.overall_max.load(Ordering::SeqCst);
        assert!(
            overall >= 2,
            "different groups should run concurrently, got max {overall}"
        );
    }
}
