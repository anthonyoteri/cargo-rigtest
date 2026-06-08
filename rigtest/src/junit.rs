//! `JUnit` XML reporter.
//!
//! A [`TestEventReporter`] implementation that accumulates suite-level state
//! during a run and serializes a `JUnit` XML document (compatible with
//! cargo-nextest's schema) when the suite finishes.
//!
//! In `cargo rig run --reporter junit` mode each child test binary writes its
//! own complete document to `RIGTEST_JUNIT_OUTPUT_PATH`, and the parent merges
//! the per-binary parts into the final `target/rigtest/junit.xml`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, FixedOffset, Utc};
use quick_junit::{
    FlakyOrRerun, NonSuccessKind, Property, Report, TestCase, TestCaseStatus, TestRerun, TestSuite,
};

use crate::reporter::{Outcome, TestEventReporter, TestRef};

/// Suite-level metadata embedded in the produced XML as properties.
#[derive(Clone)]
pub(crate) struct JunitConfig {
    /// File the document will be written to when `finish()` is called.
    pub output_path: PathBuf,
    /// Name of the `<testsuite>` element — derived from the test-binary name.
    pub suite_name: String,
    /// Random seed for the run, recorded as the `rigtest.seed` property.
    pub seed: u64,
}

/// Mutable run-state accumulated as lifecycle events fire.
#[derive(Default)]
struct State {
    suite_started_at: Option<DateTime<FixedOffset>>,
    /// In-flight test cases keyed by `(module, name)` so two tests in
    /// different modules with the same bare function name cannot collide.
    /// Populated on `test_started`, drained on the terminal event.
    in_flight: HashMap<(String, String), CaseProgress>,
    /// Cases whose terminal event has fired.
    finalized: Vec<TestCase>,
}

struct CaseProgress {
    classname: String,
    /// Failed attempts before the terminal event, in order.
    reruns: Vec<AttemptRecord>,
}

struct AttemptRecord {
    outcome: Outcome,
    reason: String,
    stdout: String,
    stderr: String,
    duration: Duration,
}

pub(crate) struct JunitReporter {
    config: JunitConfig,
    state: Mutex<State>,
}

impl JunitReporter {
    pub(crate) fn new(config: JunitConfig) -> Self {
        let state = State {
            suite_started_at: Some(now_offset()),
            ..State::default()
        };
        Self {
            config,
            state: Mutex::new(state),
        }
    }

    /// Build the in-memory `Report` reflecting the current state. Visible to
    /// tests so they can assert on the structured output without parsing XML.
    fn build_report(&self, suite_time: Duration) -> Report {
        let state = lock(&self.state);

        let mut suite = TestSuite::new(self.config.suite_name.clone());
        if let Some(ts) = state.suite_started_at {
            suite.set_timestamp(ts);
        }
        suite.set_time(suite_time);
        suite.add_property(Property::new("rigtest.seed", self.config.seed.to_string()));
        suite.add_property(Property::new("rigtest.version", env!("CARGO_PKG_VERSION")));
        let hostname = gethostname::gethostname().to_string_lossy().into_owned();
        suite.extra.insert("hostname".into(), hostname.into());

        for case in state.finalized.iter().cloned() {
            suite.add_test_case(case);
        }
        // Any test that started but never reached a terminal event is
        // surfaced as a synthetic <error> case so a child-side crash mid-run
        // does not silently vanish from the JUnit report.
        for ((_module, name), progress) in &state.in_flight {
            let mut status = TestCaseStatus::non_success(NonSuccessKind::Error);
            status.set_message("test did not report a terminal event");
            status.set_type("crash");
            let mut case = TestCase::new(name.clone(), status);
            case.set_classname(progress.classname.clone());
            case.set_time(Duration::ZERO);
            suite.add_test_case(case);
        }

        let mut report = Report::new(self.config.suite_name.clone());
        report.add_test_suite(suite);
        report
    }

    /// Serialize the current state to the configured output path. Returns the
    /// XML string for inspection in tests.
    pub(crate) fn write(&self, suite_time: Duration) -> std::io::Result<String> {
        let report = self.build_report(suite_time);
        let xml = report
            .to_string()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        if let Some(parent) = self.config.output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.config.output_path, &xml)?;
        Ok(xml)
    }

    fn record_attempt(&self, test: TestRef<'_>, attempt: AttemptRecord) {
        let key = key_for(test);
        let mut state = lock(&self.state);
        if let Some(case) = state.in_flight.get_mut(&key) {
            case.reruns.push(attempt);
        }
    }

    fn take_progress(&self, test: TestRef<'_>) -> Option<CaseProgress> {
        let key = key_for(test);
        let mut state = lock(&self.state);
        state.in_flight.remove(&key)
    }

    fn push_finalized(&self, case: TestCase) {
        let mut state = lock(&self.state);
        state.finalized.push(case);
    }
}

/// Acquire `state` while tolerating mutex poisoning. A panic inside a
/// reporter callback would otherwise poison the lock and prevent `finish()`
/// from writing the `JUnit` document — losing exactly the failure information
/// CI consumers need.
fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn key_for(test: TestRef<'_>) -> (String, String) {
    (test.module.to_string(), test.name.to_string())
}

fn now_offset() -> DateTime<FixedOffset> {
    let now: DateTime<Utc> = Utc::now();
    now.with_timezone(&FixedOffset::east_opt(0).expect("UTC offset"))
}

/// Rewrite a Rust module path so `JUnit` consumers (Jenkins, GitLab) build a
/// navigable tree from it. `module::path::here` → `module.path.here`.
fn classname_from_module(module: &str) -> String {
    module.replace("::", ".")
}

fn non_success_kind(outcome: Outcome) -> NonSuccessKind {
    match outcome {
        Outcome::Assertion | Outcome::Panic => NonSuccessKind::Failure,
        Outcome::Timeout | Outcome::Crash => NonSuccessKind::Error,
    }
}

fn outcome_type(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Assertion => "assertion",
        Outcome::Panic => "panic",
        Outcome::Timeout => "timeout",
        Outcome::Crash => "crash",
    }
}

fn make_rerun(attempt: &AttemptRecord) -> TestRerun {
    let mut rerun = TestRerun::new(non_success_kind(attempt.outcome));
    rerun.set_time(attempt.duration);
    rerun.set_message(attempt.reason.clone());
    rerun.set_type(outcome_type(attempt.outcome));
    if !attempt.stdout.is_empty() {
        rerun.set_system_out_lossy(attempt.stdout.as_bytes());
    }
    if !attempt.stderr.is_empty() {
        rerun.set_system_err_lossy(attempt.stderr.as_bytes());
    }
    rerun
}

impl TestEventReporter for JunitReporter {
    fn test_started(&self, test: TestRef<'_>) {
        let mut state = lock(&self.state);
        state.in_flight.insert(
            key_for(test),
            CaseProgress {
                classname: classname_from_module(test.module),
                reruns: Vec::new(),
            },
        );
    }

    fn test_passed(&self, test: TestRef<'_>, duration: Duration) {
        let progress = self.take_progress(test);
        let mut case = TestCase::new(test.name, TestCaseStatus::success());
        if let Some(p) = &progress {
            case.set_classname(p.classname.clone());
        }
        case.set_time(duration);
        if let Some(p) = progress {
            // The test passed eventually — earlier failed attempts become
            // <flakyFailure>/<flakyError> children.
            if !p.reruns.is_empty() {
                if let TestCaseStatus::Success { flaky_runs, .. } = &mut case.status {
                    for attempt in &p.reruns {
                        flaky_runs.push(make_rerun(attempt));
                    }
                }
            }
        }
        self.push_finalized(case);
    }

    fn test_skipped(&self, test: TestRef<'_>, duration: Duration, reason: &str) {
        let progress = self.take_progress(test);
        let mut status = TestCaseStatus::skipped();
        if !reason.is_empty() {
            status.set_message(reason);
        }
        let mut case = TestCase::new(test.name, status);
        if let Some(p) = progress {
            case.set_classname(p.classname);
        }
        case.set_time(duration);
        self.push_finalized(case);
    }

    fn test_failed(
        &self,
        test: TestRef<'_>,
        duration: Duration,
        outcome: Outcome,
        reason: &str,
        stdout: &str,
        stderr: &str,
    ) {
        let progress = self.take_progress(test);
        let kind = non_success_kind(outcome);
        let mut status = TestCaseStatus::non_success(kind);
        status.set_message(reason);
        status.set_type(outcome_type(outcome));
        let has_reruns = progress.as_ref().is_some_and(|p| !p.reruns.is_empty());
        if has_reruns {
            if let TestCaseStatus::NonSuccess { reruns, .. } = &mut status {
                if let Some(p) = &progress {
                    for attempt in &p.reruns {
                        reruns.runs.push(make_rerun(attempt));
                    }
                }
            }
            status.set_rerun_kind(FlakyOrRerun::Rerun);
        }
        let mut case = TestCase::new(test.name, status);
        if let Some(p) = progress {
            case.set_classname(p.classname);
        }
        case.set_time(duration);
        if !stdout.is_empty() {
            case.set_system_out_lossy(stdout.as_bytes());
        }
        if !stderr.is_empty() {
            case.set_system_err_lossy(stderr.as_bytes());
        }
        self.push_finalized(case);
    }

    fn test_retrying(
        &self,
        test: TestRef<'_>,
        _attempt: u32,
        _max_attempts: u32,
        outcome: Outcome,
        reason: &str,
        stdout: &str,
        stderr: &str,
        duration: Duration,
    ) {
        self.record_attempt(
            test,
            AttemptRecord {
                outcome,
                reason: reason.to_string(),
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
                duration,
            },
        );
    }

    fn print_phase(&self, _label: &str) {}

    fn finish(
        &self,
        _passed: usize,
        _skipped: usize,
        _total: usize,
        elapsed: Duration,
    ) -> anyhow::Result<()> {
        self.write(elapsed).map_err(|e| {
            anyhow::anyhow!(
                "failed to write JUnit XML to {}: {e}",
                self.config.output_path.display()
            )
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cfg(tmp: &TempDir) -> JunitConfig {
        JunitConfig {
            output_path: tmp.path().join("junit.xml"),
            suite_name: "acceptance".into(),
            seed: 42,
        }
    }

    fn tref<'a>(name: &'a str, module: &'a str) -> TestRef<'a> {
        TestRef {
            name,
            module,
            file: "tests/acceptance.rs",
        }
    }

    #[test]
    fn passed_case_is_recorded_as_success() {
        let tmp = TempDir::new().unwrap();
        let r = JunitReporter::new(cfg(&tmp));
        r.test_started(tref("hello", "acceptance::smoke"));
        r.test_passed(
            tref("hello", "acceptance::smoke"),
            Duration::from_millis(50),
        );
        let xml = r.write(Duration::from_secs(1)).unwrap();
        assert!(xml.contains("name=\"hello\""), "xml was: {xml}");
        assert!(xml.contains("classname=\"acceptance.smoke\""));
        assert!(xml.contains("name=\"acceptance\"")); // suite name
        assert!(!xml.contains("<failure"));
        assert!(!xml.contains("<error"));
    }

    #[test]
    fn failed_assertion_emits_failure_element() {
        let tmp = TempDir::new().unwrap();
        let r = JunitReporter::new(cfg(&tmp));
        let t = tref("broken", "acceptance");
        r.test_started(t);
        r.test_failed(
            t,
            Duration::from_millis(10),
            Outcome::Assertion,
            "expected 1 got 2",
            "captured stdout",
            "captured stderr",
        );
        let xml = r.write(Duration::from_secs(1)).unwrap();
        assert!(xml.contains("<failure"), "xml: {xml}");
        assert!(xml.contains("expected 1 got 2"));
        assert!(xml.contains("captured stdout"));
        assert!(xml.contains("captured stderr"));
        assert!(!xml.contains("<error "));
    }

    #[test]
    fn timeout_emits_error_element() {
        let tmp = TempDir::new().unwrap();
        let r = JunitReporter::new(cfg(&tmp));
        let t = tref("slow", "acceptance");
        r.test_started(t);
        r.test_failed(
            t,
            Duration::from_secs(30),
            Outcome::Timeout,
            "timed out after 30.0s",
            "",
            "",
        );
        let xml = r.write(Duration::from_secs(31)).unwrap();
        assert!(xml.contains("<error"), "xml: {xml}");
        assert!(xml.contains("timed out"));
        assert!(!xml.contains("<failure "));
    }

    #[test]
    fn crash_emits_error_element() {
        let tmp = TempDir::new().unwrap();
        let r = JunitReporter::new(cfg(&tmp));
        let t = tref("missing", "acceptance");
        r.test_started(t);
        r.test_failed(
            t,
            Duration::from_millis(1),
            Outcome::Crash,
            "spawn failed",
            "",
            "",
        );
        let xml = r.write(Duration::from_secs(1)).unwrap();
        assert!(xml.contains("<error"));
    }

    #[test]
    fn skipped_case_emits_skipped_element_with_reason() {
        let tmp = TempDir::new().unwrap();
        let r = JunitReporter::new(cfg(&tmp));
        let t = tref("conditional", "acceptance");
        r.test_started(t);
        r.test_skipped(t, Duration::from_millis(1), "DB_URL not set");
        let xml = r.write(Duration::from_secs(1)).unwrap();
        assert!(xml.contains("<skipped"));
        assert!(xml.contains("DB_URL not set"));
    }

    #[test]
    fn passing_after_retry_records_flaky_rerun() {
        let tmp = TempDir::new().unwrap();
        let r = JunitReporter::new(cfg(&tmp));
        let t = tref("flaky", "acceptance");
        r.test_started(t);
        r.test_retrying(
            t,
            1,
            2,
            Outcome::Assertion,
            "first failure",
            "out-1",
            "err-1",
            Duration::from_millis(5),
        );
        r.test_passed(t, Duration::from_millis(15));
        let xml = r.write(Duration::from_secs(1)).unwrap();
        // Final case is success but carries a flaky rerun child.
        assert!(xml.contains("flakyFailure"), "xml: {xml}");
        assert!(xml.contains("first failure"));
    }

    #[test]
    fn failing_after_retry_records_rerun_failure() {
        let tmp = TempDir::new().unwrap();
        let r = JunitReporter::new(cfg(&tmp));
        let t = tref("always_broken", "acceptance");
        r.test_started(t);
        r.test_retrying(
            t,
            1,
            2,
            Outcome::Assertion,
            "attempt 1 failed",
            "",
            "",
            Duration::from_millis(5),
        );
        r.test_failed(
            t,
            Duration::from_millis(15),
            Outcome::Assertion,
            "final failure",
            "",
            "",
        );
        let xml = r.write(Duration::from_secs(1)).unwrap();
        assert!(xml.contains("rerunFailure"), "xml: {xml}");
        assert!(xml.contains("attempt 1 failed"));
        assert!(xml.contains("final failure"));
    }

    #[test]
    fn suite_includes_seed_and_version_properties() {
        let tmp = TempDir::new().unwrap();
        let r = JunitReporter::new(cfg(&tmp));
        r.test_started(tref("t", "acceptance"));
        r.test_passed(tref("t", "acceptance"), Duration::from_millis(1));
        let xml = r.write(Duration::from_secs(1)).unwrap();
        assert!(xml.contains("rigtest.seed"));
        assert!(xml.contains("\"42\""));
        assert!(xml.contains("rigtest.version"));
    }

    #[test]
    fn same_name_in_different_modules_does_not_collide() {
        let tmp = TempDir::new().unwrap();
        let r = JunitReporter::new(cfg(&tmp));
        let a = tref("shared", "acceptance::unit");
        let b = tref("shared", "acceptance::integration");
        r.test_started(a);
        r.test_started(b);
        r.test_passed(a, Duration::from_millis(5));
        r.test_failed(
            b,
            Duration::from_millis(7),
            Outcome::Assertion,
            "boom",
            "",
            "",
        );
        let xml = r.write(Duration::from_secs(1)).unwrap();
        assert!(xml.contains("classname=\"acceptance.unit\""));
        assert!(xml.contains("classname=\"acceptance.integration\""));
        // Exactly one passes, exactly one fails — collision would lose one.
        assert_eq!(xml.matches("<failure").count(), 1);
    }

    #[test]
    fn in_flight_test_at_finish_emits_synthetic_error() {
        let tmp = TempDir::new().unwrap();
        let r = JunitReporter::new(cfg(&tmp));
        r.test_started(tref("never_finished", "acceptance"));
        let xml = r.write(Duration::from_secs(1)).unwrap();
        assert!(xml.contains("name=\"never_finished\""));
        assert!(xml.contains("<error"));
        assert!(xml.contains("did not report a terminal event"));
    }

    #[test]
    fn finish_returns_error_when_output_path_unwritable() {
        // Point the output at a directory that exists as a regular file —
        // create_dir_all on its parent will fail because the "parent" is
        // not a directory.
        let tmp = TempDir::new().unwrap();
        let blocker = tmp.path().join("not_a_dir");
        std::fs::write(&blocker, b"sentinel").unwrap();
        let config = JunitConfig {
            output_path: blocker.join("nested/junit.xml"),
            suite_name: "acceptance".into(),
            seed: 1,
        };
        let r = JunitReporter::new(config);
        r.test_started(tref("t", "acceptance"));
        r.test_passed(tref("t", "acceptance"), Duration::from_millis(1));
        let result = r.finish(1, 0, 1, Duration::from_secs(1));
        assert!(result.is_err(), "expected finish to surface write failure");
    }

    #[test]
    fn poisoned_state_still_writes_xml() {
        // Simulate a previous callback panicking under the lock: poison
        // the mutex by panicking inside a closure that holds the guard,
        // then verify subsequent reporter calls still work and finish()
        // writes the XML.
        let tmp = TempDir::new().unwrap();
        let r = JunitReporter::new(cfg(&tmp));
        r.test_started(tref("ok", "acceptance"));
        let r_ref = std::sync::Arc::new(r);
        let r_clone = std::sync::Arc::clone(&r_ref);
        let _ = std::thread::spawn(move || {
            let _guard = r_clone.state.lock().unwrap();
            panic!("simulated panic while holding state lock");
        })
        .join();
        // Mutex is now poisoned. The reporter should still function.
        r_ref.test_passed(tref("ok", "acceptance"), Duration::from_millis(1));
        let xml = r_ref.write(Duration::from_secs(1)).unwrap();
        assert!(xml.contains("name=\"ok\""));
    }

    #[test]
    fn rerun_kind_only_set_when_reruns_present() {
        let tmp = TempDir::new().unwrap();
        let r = JunitReporter::new(cfg(&tmp));
        let t = tref("plain_failure", "acceptance");
        r.test_started(t);
        r.test_failed(
            t,
            Duration::from_millis(10),
            Outcome::Assertion,
            "boom",
            "",
            "",
        );
        let xml = r.write(Duration::from_secs(1)).unwrap();
        assert!(!xml.contains("rerunFailure"), "xml: {xml}");
        assert!(!xml.contains("flakyFailure"));
    }

    #[test]
    fn suite_has_hostname_attribute() {
        let tmp = TempDir::new().unwrap();
        let r = JunitReporter::new(cfg(&tmp));
        r.test_started(tref("t", "acceptance"));
        r.test_passed(tref("t", "acceptance"), Duration::from_millis(1));
        let xml = r.write(Duration::from_secs(1)).unwrap();
        assert!(xml.contains("hostname="), "xml: {xml}");
    }

    #[test]
    fn finish_writes_file_to_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested/dir/junit.xml");
        let config = JunitConfig {
            output_path: path.clone(),
            suite_name: "acceptance".into(),
            seed: 1,
        };
        let r = JunitReporter::new(config);
        r.test_started(tref("only", "acceptance"));
        r.test_passed(tref("only", "acceptance"), Duration::from_millis(1));
        r.finish(1, 0, 1, Duration::from_secs(1)).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("name=\"only\""));
    }
}
