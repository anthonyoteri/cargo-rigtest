use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use console::style;
use indicatif::MultiProgress;
use indicatif::ProgressBar;
use indicatif::ProgressDrawTarget;
use indicatif::ProgressStyle;

/// Renders the per-test pass line. When `retries > 0` the test is flaky —
/// the status word is `FLAKY` (yellow) instead of `PASS` (green) so a
/// scan of a CI log distinguishes a clean pass from one that needed
/// retries.
fn render_pass_line(name: &str, duration: Duration, retries: usize) -> String {
    let status = if retries > 0 {
        style("FLAKY").yellow().bold()
    } else {
        style("PASS").green().bold()
    };
    format!("{} [{:.3}s] {}", status, duration.as_secs_f64(), name)
}

/// Builds the `passed` segment of the run-summary line. When `flaky > 0` it
/// surfaces as a parenthetical of `passed`, per CONTEXT.md (Flaky entry) —
/// e.g. `12 passed (2 flaky)`. When zero, the output is byte-identical to
/// the pre-flaky format so existing log scrapers keep parsing.
fn passed_label(passed: usize, flaky: usize) -> String {
    if flaky > 0 {
        format!("{passed} passed ({flaky} flaky)")
    } else {
        format!("{passed} passed")
    }
}

fn indent(s: &str) -> String {
    s.lines()
        .fold(String::with_capacity(s.len()), |mut acc, line| {
            if !acc.is_empty() {
                acc.push('\n');
            }
            acc.push_str("  ");
            acc.push_str(line);
            acc
        })
}

/// Identifies a test case in reporter callbacks.
///
/// Carries enough information to render either a terse leaf name (as the live
/// console reporter does) or a fully-qualified path (as the `JUnit` reporter
/// does for the `classname` attribute).
#[derive(Clone, Copy, Debug)]
pub(crate) struct TestRef<'a> {
    pub name: &'a str,
    pub module: &'a str,
    /// Reserved for future use by reporters that link to source.
    #[allow(dead_code)]
    pub file: &'a str,
}

/// How a test failed. The trait surface distinguishes the four cases so
/// reporters can render them appropriately — the `JUnit` reporter maps
/// `Assertion`/`Panic` to `<failure>` and `Timeout`/`Crash` to `<error>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Test function returned `Err(_)` from its body.
    Assertion,
    /// Test function panicked.
    Panic,
    /// Subprocess exceeded its `#[testcase(timeout = ...)]` budget.
    Timeout,
    /// Subprocess could not be spawned or its IPC channel failed.
    Crash,
}

/// Receives lifecycle events as the orchestrator runs a test suite.
///
/// Implementations include the live TTY [`Reporter`], a [`NullReporter`] that
/// silently discards events, and a [`RecordingReporter`] that captures the
/// event sequence for assertions in tests.
///
/// `Send + Sync + 'static` so a single reporter can be shared via `Arc`
/// across `JoinSet`-spawned tasks for parallel dispatch.
pub(crate) trait TestEventReporter: Send + Sync + 'static {
    fn test_started(&self, test: TestRef<'_>);
    fn test_passed(&self, test: TestRef<'_>, duration: Duration);
    fn test_skipped(&self, test: TestRef<'_>, duration: Duration, reason: &str);
    fn test_failed(
        &self,
        test: TestRef<'_>,
        duration: Duration,
        outcome: Outcome,
        reason: &str,
        stdout: &str,
        stderr: &str,
    );
    #[allow(clippy::too_many_arguments)]
    fn test_retrying(
        &self,
        test: TestRef<'_>,
        attempt: u32,
        max_attempts: u32,
        outcome: Outcome,
        reason: &str,
        stdout: &str,
        stderr: &str,
        duration: Duration,
    );
    fn print_phase(&self, label: &str);
    /// Called once with the structured per-probe results when a preflight
    /// phase ran (whether passed or failed). Default no-op so the live
    /// console reporter — which already streams probe lines directly —
    /// doesn't need to do anything.
    fn preflight_recorded(&self, _results: &[crate::preflight_runner::ProbeResult]) {}
    /// Called once at the end of the run. Returning `Err` causes the test
    /// binary to exit non-zero so a CI consumer can tell that an artifact
    /// the reporter promised (e.g. the `JUnit` XML file) was not produced.
    fn finish(
        &self,
        passed: usize,
        skipped: usize,
        total: usize,
        elapsed: Duration,
    ) -> anyhow::Result<()>;
}

/// Nextest-style reporter. In a TTY it shows live spinners per running test;
/// in a non-TTY environment (CI, piped output) it falls back to plain lines
/// so output is never silently dropped.
pub(crate) struct Reporter {
    multi: MultiProgress,
    is_tty: bool,
    /// Active spinners keyed by test name. The orchestrator drives lifecycle
    /// events by name; `Reporter` owns the per-test `ProgressBar` so callers
    /// never have to thread a handle through.
    spinners: Mutex<HashMap<String, ProgressBar>>,
    /// Per-test retry counts keyed by `(module, name)`. Incremented on every
    /// `test_retrying`; consulted in `test_passed` to decide whether to
    /// render `FLAKY` (retries > 0) or `PASS`. A run-wide tally of flaky
    /// passes is also tracked so the summary can surface a `(N flaky)`
    /// parenthetical without changing the trait signature.
    retry_counts: Mutex<HashMap<(String, String), usize>>,
    flaky_passed: Mutex<usize>,
}

impl Default for Reporter {
    fn default() -> Self {
        Self::new()
    }
}

impl Reporter {
    /// Creates a new `Reporter`.
    ///
    /// Inspects whether stderr is a TTY and configures the draw target
    /// accordingly: in a TTY environment animated [`indicatif`] spinners are
    /// used; in non-TTY environments (CI, piped output) the reporter falls back
    /// to plain lines printed directly to stderr so no output is silently
    /// dropped.
    pub(crate) fn new() -> Self {
        let is_tty = console::Term::stderr().is_term();
        let target = if is_tty {
            ProgressDrawTarget::stderr()
        } else {
            ProgressDrawTarget::hidden()
        };
        Self {
            multi: MultiProgress::with_draw_target(target),
            is_tty,
            spinners: Mutex::new(HashMap::new()),
            retry_counts: Mutex::new(HashMap::new()),
            flaky_passed: Mutex::new(0),
        }
    }

    fn retry_key(test: TestRef<'_>) -> (String, String) {
        (test.module.to_string(), test.name.to_string())
    }

    fn record_retry(&self, test: TestRef<'_>) {
        let mut counts = self.retry_counts.lock().expect("retry_counts mutex");
        *counts.entry(Self::retry_key(test)).or_insert(0) += 1;
    }

    /// Removes and returns the recorded retry count for a test. Called by
    /// terminal events (`test_passed` / `test_failed`) so a re-run of the
    /// same test name in a different module never picks up a stale count.
    fn take_retries(&self, test: TestRef<'_>) -> usize {
        self.retry_counts
            .lock()
            .expect("retry_counts mutex")
            .remove(&Self::retry_key(test))
            .unwrap_or(0)
    }

    fn take_spinner(&self, name: &str) -> Option<ProgressBar> {
        self.spinners.lock().expect("spinners mutex").remove(name)
    }

    fn finalize_spinner(&self, name: &str, line: &str) {
        if self.is_tty {
            // println flushes synchronously through the draw thread;
            // finish_and_clear then removes the spinner. Reversing this
            // order would let the spinner be torn down before the line is
            // drawn and race with MultiProgress's draw thread.
            self.multi.println(line).ok();
            if let Some(pb) = self.take_spinner(name) {
                pb.finish_and_clear();
            }
        } else {
            // Drop the spinner anyway to free the slot in the map. In
            // non-TTY mode `pb` is hidden so finish_and_clear is a no-op.
            let _ = self.take_spinner(name);
            eprintln!("{line}");
        }
    }

    fn print_captured(&self, label: &str, content: &str) {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return;
        }
        let rule = style(format!("── {label} ")).dim().to_string();
        let output = format!("\n  {rule}\n{}\n", indent(trimmed));
        if self.is_tty {
            self.multi.println(&output).ok();
        } else {
            eprintln!("{output}");
        }
    }
}

impl TestEventReporter for Reporter {
    fn test_started(&self, test: TestRef<'_>) {
        if !self.is_tty {
            return;
        }
        let pb = self.multi.add(ProgressBar::new_spinner());
        pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}").expect("valid spinner template"),
        );
        pb.set_message(format!("{} {}", style("RUNNING").cyan().bold(), test.name));
        pb.enable_steady_tick(Duration::from_millis(80));
        self.spinners
            .lock()
            .expect("spinners mutex")
            .insert(test.name.to_string(), pb);
    }

    fn test_passed(&self, test: TestRef<'_>, duration: Duration) {
        let retries = self.take_retries(test);
        if retries > 0 {
            *self.flaky_passed.lock().expect("flaky_passed mutex") += 1;
        }
        let line = render_pass_line(test.name, duration, retries);
        self.finalize_spinner(test.name, &line);
    }

    fn test_skipped(&self, test: TestRef<'_>, duration: Duration, reason: &str) {
        let mut line = format!(
            "{} [{:.3}s] {}",
            style("SKIP").dim().bold(),
            duration.as_secs_f64(),
            test.name,
        );
        if !reason.is_empty() {
            use std::fmt::Write as _;
            let _ = write!(line, ": {reason}");
        }
        self.finalize_spinner(test.name, &line);
    }

    fn test_failed(
        &self,
        test: TestRef<'_>,
        duration: Duration,
        _outcome: Outcome,
        reason: &str,
        stdout: &str,
        stderr: &str,
    ) {
        // Drop any recorded retry count so a later test reusing the same
        // (module, name) keys can't inherit stale state. Failed tests stay
        // in the `failed` bucket regardless of how many retries they used,
        // so the value itself is discarded.
        let _ = self.take_retries(test);
        let header = format!(
            "{} [{:.3}s] {}: {}",
            style("FAIL").red().bold(),
            duration.as_secs_f64(),
            test.name,
            reason,
        );
        self.finalize_spinner(test.name, &header);
        self.print_captured("stdout", stdout);
        self.print_captured("stderr", stderr);
    }

    fn test_retrying(
        &self,
        test: TestRef<'_>,
        attempt: u32,
        max_attempts: u32,
        _outcome: Outcome,
        reason: &str,
        _stdout: &str,
        _stderr: &str,
        _duration: Duration,
    ) {
        self.record_retry(test);
        let line = format!(
            "{} [{}/{}] {}: {}",
            style("RETRY").yellow().bold(),
            attempt,
            max_attempts,
            test.name,
            reason,
        );
        if self.is_tty {
            self.multi.println(&line).ok();
        } else {
            eprintln!("{line}");
        }
    }

    fn print_phase(&self, label: &str) {
        let line = format!("{} {}", style("──").dim(), style(label).dim().bold());
        if self.is_tty {
            self.multi.println(&line).ok();
        } else {
            eprintln!("{line}");
        }
    }

    fn finish(
        &self,
        passed: usize,
        skipped: usize,
        total: usize,
        elapsed: Duration,
    ) -> anyhow::Result<()> {
        let failed = total - passed - skipped;
        let separator = style("─".repeat(60)).dim().to_string();

        // Flaky tally is a parenthetical of `passed` (per CONTEXT.md /
        // PRD Q3.B2), not a third bucket. When zero, the summary format
        // is byte-identical to the pre-flaky output.
        let flaky = *self.flaky_passed.lock().expect("flaky_passed mutex");
        let mut parts = vec![style(passed_label(passed, flaky))
            .green()
            .bold()
            .to_string()];
        if skipped > 0 {
            parts.push(style(format!("{skipped} skipped")).dim().bold().to_string());
        }
        if failed > 0 {
            parts.push(style(format!("{failed} failed")).red().bold().to_string());
        }

        let counts = parts.join(", ");
        let summary = format!(
            "{:>12} [{:.2}s] {total} tests run: {counts}",
            if failed == 0 {
                style("Summary").green().bold().to_string()
            } else {
                style("Summary").red().bold().to_string()
            },
            elapsed.as_secs_f64(),
        );

        if self.is_tty {
            self.multi.println(&separator).ok();
            self.multi.println(&summary).ok();
        } else {
            eprintln!("{separator}");
            eprintln!("{summary}");
        }
        Ok(())
    }
}

/// Fans out lifecycle events to several reporters in registration order.
///
/// Used when `--reporter junit` is active to drive both the live console
/// [`Reporter`] and a [`JunitReporter`][crate::junit::JunitReporter] from the
/// same orchestrator dispatch.
pub(crate) struct MultiReporter {
    reporters: Vec<Box<dyn TestEventReporter>>,
}

impl MultiReporter {
    pub(crate) fn new(reporters: Vec<Box<dyn TestEventReporter>>) -> Self {
        Self { reporters }
    }
}

impl TestEventReporter for MultiReporter {
    fn test_started(&self, test: TestRef<'_>) {
        for r in &self.reporters {
            r.test_started(test);
        }
    }
    fn test_passed(&self, test: TestRef<'_>, duration: Duration) {
        for r in &self.reporters {
            r.test_passed(test, duration);
        }
    }
    fn test_skipped(&self, test: TestRef<'_>, duration: Duration, reason: &str) {
        for r in &self.reporters {
            r.test_skipped(test, duration, reason);
        }
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
        for r in &self.reporters {
            r.test_failed(test, duration, outcome, reason, stdout, stderr);
        }
    }
    fn test_retrying(
        &self,
        test: TestRef<'_>,
        attempt: u32,
        max_attempts: u32,
        outcome: Outcome,
        reason: &str,
        stdout: &str,
        stderr: &str,
        duration: Duration,
    ) {
        for r in &self.reporters {
            r.test_retrying(
                test,
                attempt,
                max_attempts,
                outcome,
                reason,
                stdout,
                stderr,
                duration,
            );
        }
    }
    fn print_phase(&self, label: &str) {
        for r in &self.reporters {
            r.print_phase(label);
        }
    }
    fn preflight_recorded(&self, results: &[crate::preflight_runner::ProbeResult]) {
        for r in &self.reporters {
            r.preflight_recorded(results);
        }
    }
    fn finish(
        &self,
        passed: usize,
        skipped: usize,
        total: usize,
        elapsed: Duration,
    ) -> anyhow::Result<()> {
        // Drive every reporter even if one fails so the live console summary
        // still prints. Return the first error so a JUnit write failure is
        // surfaced as a non-zero exit even when the console reporter
        // afterwards returns Ok.
        let mut first_err: Option<anyhow::Error> = None;
        for r in &self.reporters {
            if let Err(e) = r.finish(passed, skipped, total, elapsed) {
                first_err.get_or_insert(e);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

// ── Test doubles ─────────────────────────────────────────────────────────

/// A reporter that discards every event. Useful in tests that exercise the
/// orchestrator's behavior without needing to observe reporter output.
#[cfg(test)]
pub(crate) struct NullReporter;

#[cfg(test)]
impl TestEventReporter for NullReporter {
    fn test_started(&self, _test: TestRef<'_>) {}
    fn test_passed(&self, _test: TestRef<'_>, _duration: Duration) {}
    fn test_skipped(&self, _test: TestRef<'_>, _duration: Duration, _reason: &str) {}
    fn test_failed(
        &self,
        _test: TestRef<'_>,
        _duration: Duration,
        _outcome: Outcome,
        _reason: &str,
        _stdout: &str,
        _stderr: &str,
    ) {
    }
    fn test_retrying(
        &self,
        _test: TestRef<'_>,
        _attempt: u32,
        _max_attempts: u32,
        _outcome: Outcome,
        _reason: &str,
        _stdout: &str,
        _stderr: &str,
        _duration: Duration,
    ) {
    }
    fn print_phase(&self, _label: &str) {}
    fn finish(
        &self,
        _passed: usize,
        _skipped: usize,
        _total: usize,
        _elapsed: Duration,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Captures the sequence of events for assertions in tests.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    Started(String),
    Passed(String),
    Skipped(String, String),
    Failed(String, Outcome, String),
    Retrying(String, u32, u32, Outcome, String),
    Phase(String),
    Finished(usize, usize, usize),
}

#[cfg(test)]
pub(crate) struct RecordingReporter {
    events: Mutex<Vec<Event>>,
}

#[cfg(test)]
impl RecordingReporter {
    pub(crate) fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn events(&self) -> Vec<Event> {
        self.events.lock().expect("events mutex").clone()
    }
}

#[cfg(test)]
impl TestEventReporter for RecordingReporter {
    fn test_started(&self, test: TestRef<'_>) {
        self.events
            .lock()
            .expect("events mutex")
            .push(Event::Started(test.name.to_string()));
    }
    fn test_passed(&self, test: TestRef<'_>, _duration: Duration) {
        self.events
            .lock()
            .expect("events mutex")
            .push(Event::Passed(test.name.to_string()));
    }
    fn test_skipped(&self, test: TestRef<'_>, _duration: Duration, reason: &str) {
        self.events
            .lock()
            .expect("events mutex")
            .push(Event::Skipped(test.name.to_string(), reason.to_string()));
    }
    fn test_failed(
        &self,
        test: TestRef<'_>,
        _duration: Duration,
        outcome: Outcome,
        reason: &str,
        _stdout: &str,
        _stderr: &str,
    ) {
        self.events
            .lock()
            .expect("events mutex")
            .push(Event::Failed(
                test.name.to_string(),
                outcome,
                reason.to_string(),
            ));
    }
    fn test_retrying(
        &self,
        test: TestRef<'_>,
        attempt: u32,
        max_attempts: u32,
        outcome: Outcome,
        reason: &str,
        _stdout: &str,
        _stderr: &str,
        _duration: Duration,
    ) {
        self.events
            .lock()
            .expect("events mutex")
            .push(Event::Retrying(
                test.name.to_string(),
                attempt,
                max_attempts,
                outcome,
                reason.to_string(),
            ));
    }
    fn print_phase(&self, label: &str) {
        self.events
            .lock()
            .expect("events mutex")
            .push(Event::Phase(label.to_string()));
    }
    fn finish(
        &self,
        passed: usize,
        skipped: usize,
        total: usize,
        _elapsed: Duration,
    ) -> anyhow::Result<()> {
        self.events
            .lock()
            .expect("events mutex")
            .push(Event::Finished(passed, skipped, total));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tref<'a>(name: &'a str, module: &'a str) -> TestRef<'a> {
        TestRef {
            name,
            module,
            file: "tests/acceptance.rs",
        }
    }

    fn strip_ansi(s: &str) -> String {
        console::strip_ansi_codes(s).into_owned()
    }

    #[test]
    fn pass_line_renders_pass_when_no_retries() {
        let line = strip_ansi(&render_pass_line("homepage", Duration::from_millis(142), 0));
        assert_eq!(line, "PASS [0.142s] homepage");
    }

    #[test]
    fn pass_line_renders_flaky_when_any_retries() {
        let line = strip_ansi(&render_pass_line("homepage", Duration::from_millis(142), 1));
        assert_eq!(line, "FLAKY [0.142s] homepage");
    }

    #[test]
    fn passed_label_omits_parenthetical_when_no_flaky() {
        assert_eq!(passed_label(12, 0), "12 passed");
    }

    #[test]
    fn passed_label_adds_parenthetical_when_flaky_present() {
        assert_eq!(passed_label(12, 2), "12 passed (2 flaky)");
    }

    // End-to-end through the live reporter: feed retry + pass events and
    // confirm the run-wide flaky counter increments. Drives `take_retries`
    // alongside the render helpers so the wiring between them is exercised
    // in one test, not just the pure functions in isolation.
    #[test]
    fn retry_then_pass_marks_test_flaky() {
        let r = Reporter::new();
        let t = tref("homepage", "acceptance");
        r.test_started(t);
        r.test_retrying(
            t,
            1,
            2,
            Outcome::Assertion,
            "transient",
            "",
            "",
            Duration::from_millis(10),
        );
        r.test_passed(t, Duration::from_millis(20));
        assert_eq!(*r.flaky_passed.lock().unwrap(), 1);
        // Retry state is consumed at the terminal event so a follow-up run
        // of the same key never inherits stale counts.
        assert!(r
            .retry_counts
            .lock()
            .unwrap()
            .get(&(t.module.to_string(), t.name.to_string()))
            .is_none());
    }

    #[test]
    fn pass_with_no_retries_is_not_flaky() {
        let r = Reporter::new();
        let t = tref("homepage", "acceptance");
        r.test_started(t);
        r.test_passed(t, Duration::from_millis(20));
        assert_eq!(*r.flaky_passed.lock().unwrap(), 0);
    }

    // A failed test consumes any pending retry count so a later test
    // reusing the same key cannot inherit it. The failure itself is
    // bucketed in `failed`, not `flaky`.
    #[test]
    fn fail_clears_pending_retry_count() {
        let r = Reporter::new();
        let t = tref("homepage", "acceptance");
        r.test_started(t);
        r.test_retrying(
            t,
            1,
            2,
            Outcome::Assertion,
            "still broken",
            "",
            "",
            Duration::from_millis(10),
        );
        r.test_failed(
            t,
            Duration::from_millis(20),
            Outcome::Assertion,
            "final fail",
            "",
            "",
        );
        assert_eq!(*r.flaky_passed.lock().unwrap(), 0);
        assert!(r.retry_counts.lock().unwrap().is_empty());
    }
}
