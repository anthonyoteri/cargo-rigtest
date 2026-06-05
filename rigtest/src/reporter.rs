use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use console::style;
use indicatif::MultiProgress;
use indicatif::ProgressBar;
use indicatif::ProgressDrawTarget;
use indicatif::ProgressStyle;

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

/// Receives lifecycle events as the orchestrator runs a test suite.
///
/// Implementations include the live TTY [`Reporter`], a [`NullReporter`] that
/// silently discards events, and a [`RecordingReporter`] that captures the
/// event sequence for assertions in tests.
///
/// `Send + Sync + 'static` so a single reporter can be shared via `Arc`
/// across `JoinSet`-spawned tasks for parallel dispatch.
pub(crate) trait TestEventReporter: Send + Sync + 'static {
    fn test_started(&self, name: &str);
    fn test_passed(&self, name: &str, duration: Duration);
    fn test_skipped(&self, name: &str, duration: Duration, reason: &str);
    fn test_failed(&self, name: &str, duration: Duration, reason: &str, stdout: &str, stderr: &str);
    fn test_retrying(&self, name: &str, attempt: u32, max_attempts: u32, reason: &str);
    fn print_phase(&self, label: &str);
    fn finish(&self, passed: usize, skipped: usize, total: usize, elapsed: Duration);
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
        }
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
    fn test_started(&self, name: &str) {
        if !self.is_tty {
            return;
        }
        let pb = self.multi.add(ProgressBar::new_spinner());
        pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}").expect("valid spinner template"),
        );
        pb.set_message(format!("{} {}", style("RUNNING").cyan().bold(), name));
        pb.enable_steady_tick(Duration::from_millis(80));
        self.spinners
            .lock()
            .expect("spinners mutex")
            .insert(name.to_string(), pb);
    }

    fn test_passed(&self, name: &str, duration: Duration) {
        let line = format!(
            "{} [{:.3}s] {}",
            style("PASS").green().bold(),
            duration.as_secs_f64(),
            name,
        );
        self.finalize_spinner(name, &line);
    }

    fn test_skipped(&self, name: &str, duration: Duration, reason: &str) {
        let mut line = format!(
            "{} [{:.3}s] {}",
            style("SKIP").yellow().bold(),
            duration.as_secs_f64(),
            name,
        );
        if !reason.is_empty() {
            use std::fmt::Write as _;
            let _ = write!(line, ": {reason}");
        }
        self.finalize_spinner(name, &line);
    }

    fn test_failed(
        &self,
        name: &str,
        duration: Duration,
        reason: &str,
        stdout: &str,
        stderr: &str,
    ) {
        let header = format!(
            "{} [{:.3}s] {}: {}",
            style("FAIL").red().bold(),
            duration.as_secs_f64(),
            name,
            reason,
        );
        self.finalize_spinner(name, &header);
        self.print_captured("stdout", stdout);
        self.print_captured("stderr", stderr);
    }

    fn test_retrying(&self, name: &str, attempt: u32, max_attempts: u32, reason: &str) {
        let line = format!(
            "{} [{}/{}] {}: {}",
            style("RETRY").yellow().bold(),
            attempt,
            max_attempts,
            name,
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

    fn finish(&self, passed: usize, skipped: usize, total: usize, elapsed: Duration) {
        let failed = total - passed - skipped;
        let separator = style("─".repeat(60)).dim().to_string();

        let mut parts = vec![style(format!("{passed} passed")).green().bold().to_string()];
        if skipped > 0 {
            parts.push(
                style(format!("{skipped} skipped"))
                    .yellow()
                    .bold()
                    .to_string(),
            );
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
    }
}

// ── Test doubles ─────────────────────────────────────────────────────────

/// A reporter that discards every event. Useful in tests that exercise the
/// orchestrator's behaviour without needing to observe reporter output.
#[cfg(test)]
pub(crate) struct NullReporter;

#[cfg(test)]
impl TestEventReporter for NullReporter {
    fn test_started(&self, _name: &str) {}
    fn test_passed(&self, _name: &str, _duration: Duration) {}
    fn test_skipped(&self, _name: &str, _duration: Duration, _reason: &str) {}
    fn test_failed(
        &self,
        _name: &str,
        _duration: Duration,
        _reason: &str,
        _stdout: &str,
        _stderr: &str,
    ) {
    }
    fn test_retrying(&self, _name: &str, _attempt: u32, _max_attempts: u32, _reason: &str) {}
    fn print_phase(&self, _label: &str) {}
    fn finish(&self, _passed: usize, _skipped: usize, _total: usize, _elapsed: Duration) {}
}

/// Captures the sequence of events for assertions in tests.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    Started(String),
    Passed(String),
    Skipped(String, String),
    Failed(String, String),
    Retrying(String, u32, u32, String),
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
    fn test_started(&self, name: &str) {
        self.events
            .lock()
            .expect("events mutex")
            .push(Event::Started(name.to_string()));
    }
    fn test_passed(&self, name: &str, _duration: Duration) {
        self.events
            .lock()
            .expect("events mutex")
            .push(Event::Passed(name.to_string()));
    }
    fn test_skipped(&self, name: &str, _duration: Duration, reason: &str) {
        self.events
            .lock()
            .expect("events mutex")
            .push(Event::Skipped(name.to_string(), reason.to_string()));
    }
    fn test_failed(
        &self,
        name: &str,
        _duration: Duration,
        reason: &str,
        _stdout: &str,
        _stderr: &str,
    ) {
        self.events
            .lock()
            .expect("events mutex")
            .push(Event::Failed(name.to_string(), reason.to_string()));
    }
    fn test_retrying(&self, name: &str, attempt: u32, max_attempts: u32, reason: &str) {
        self.events
            .lock()
            .expect("events mutex")
            .push(Event::Retrying(
                name.to_string(),
                attempt,
                max_attempts,
                reason.to_string(),
            ));
    }
    fn print_phase(&self, label: &str) {
        self.events
            .lock()
            .expect("events mutex")
            .push(Event::Phase(label.to_string()));
    }
    fn finish(&self, passed: usize, skipped: usize, total: usize, _elapsed: Duration) {
        self.events
            .lock()
            .expect("events mutex")
            .push(Event::Finished(passed, skipped, total));
    }
}
