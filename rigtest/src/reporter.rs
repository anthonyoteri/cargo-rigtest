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

/// Nextest-style reporter. In a TTY it shows live spinners per running test;
/// in a non-TTY environment (CI, piped output) it falls back to plain lines
/// so output is never silently dropped.
pub struct Reporter {
    multi: MultiProgress,
    is_tty: bool,
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
    #[must_use]
    pub fn new() -> Self {
        let is_tty = console::Term::stderr().is_term();
        let target = if is_tty {
            ProgressDrawTarget::stderr()
        } else {
            ProgressDrawTarget::hidden()
        };
        Self {
            multi: MultiProgress::with_draw_target(target),
            is_tty,
        }
    }

    /// Begin tracking a running test named `name`.
    ///
    /// In TTY mode this adds an animated spinner to the [`MultiProgress`]
    /// display. In non-TTY mode it returns a hidden, no-op [`ProgressBar`].
    /// Either way the returned handle must be passed to [`test_passed`],
    /// [`test_skipped`], or [`test_failed`] to finalise the line.
    ///
    /// [`test_passed`]: Reporter::test_passed
    /// [`test_skipped`]: Reporter::test_skipped
    /// [`test_failed`]: Reporter::test_failed
    ///
    /// # Panics
    ///
    /// Panics if the internal spinner template string is malformed (it is a
    /// compile-time constant, so this cannot happen in practice).
    #[must_use]
    pub fn test_started(&self, name: &str) -> ProgressBar {
        if self.is_tty {
            let pb = self.multi.add(ProgressBar::new_spinner());
            pb.set_style(
                ProgressStyle::with_template("{spinner:.cyan} {msg}")
                    .expect("valid spinner template"),
            );
            pb.set_message(format!("{} {}", style("RUNNING").cyan().bold(), name));
            pb.enable_steady_tick(Duration::from_millis(80));
            pb
        } else {
            ProgressBar::hidden()
        }
    }

    /// Finalise the progress line for `name` as `PASS`.
    ///
    /// `pb` is the spinner handle returned by [`test_started`]. It is cleared
    /// after the final line is printed. `duration` is the wall-clock time the
    /// test took from start to finish.
    ///
    /// [`test_started`]: Reporter::test_started
    pub fn test_passed(&self, pb: &ProgressBar, name: &str, duration: Duration) {
        let line = format!(
            "{} [{:.3}s] {}",
            style("PASS").green().bold(),
            duration.as_secs_f64(),
            name,
        );
        if self.is_tty {
            // println flushes synchronously through the draw thread; finish_and_clear
            // then removes the spinner. Using finish_with_message instead would hand
            // the line to the async draw thread and race with MultiProgress drop.
            self.multi.println(&line).ok();
            pb.finish_and_clear();
        } else {
            eprintln!("{line}");
        }
    }

    /// Print a `RETRY` notification while the test is still in progress.
    ///
    /// `name` is the test name. `attempt` is the attempt number (1-based) that
    /// just failed. `max_attempts` is the total number of attempts allowed
    /// (initial attempt + retries). `reason` is the failure message for the
    /// attempt being retried. The spinner for the test is not finalised and
    /// continues to animate.
    pub fn test_retrying(&self, name: &str, attempt: u32, max_attempts: u32, reason: &str) {
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

    /// Finalise the progress line for `name` as `SKIP`.
    ///
    /// `pb` is the spinner handle returned by [`test_started`]. `duration` is
    /// the wall-clock time elapsed before the test signalled a skip. `reason`
    /// is the human-readable skip message; it is appended to the output line
    /// after a colon and is omitted when empty.
    ///
    /// [`test_started`]: Reporter::test_started
    pub fn test_skipped(&self, pb: &ProgressBar, name: &str, duration: Duration, reason: &str) {
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
        if self.is_tty {
            self.multi.println(&line).ok();
            pb.finish_and_clear();
        } else {
            eprintln!("{line}");
        }
    }

    /// Finalise the progress line for `name` as `FAIL` and print any captured output.
    ///
    /// `pb` is the spinner handle returned by [`test_started`]. `duration` is
    /// the elapsed wall-clock time. `reason` is a short description of the
    /// failure (e.g. `"exited with code 1"` or `"timed out after 5.0s"`).
    /// `stdout` and `stderr` are the captured output from the test subprocess;
    /// non-empty sections are printed indented below the failure line.
    ///
    /// [`test_started`]: Reporter::test_started
    pub fn test_failed(
        &self,
        pb: &ProgressBar,
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
        if self.is_tty {
            self.multi.println(&header).ok();
            pb.finish_and_clear();
        } else {
            eprintln!("{header}");
        }
        self.print_captured("stdout", stdout);
        self.print_captured("stderr", stderr);
    }

    /// Print a dimmed phase header to mark a global-lifecycle boundary.
    ///
    /// `label` is the phase name (e.g. `"global setup"`, `"global teardown"`).
    /// Output produced by the phase appears on the lines that follow this
    /// header, interleaved naturally with the terminal or log stream.
    pub fn print_phase(&self, label: &str) {
        let line = format!("{} {}", style("──").dim(), style(label).dim().bold());
        if self.is_tty {
            self.multi.println(&line).ok();
        } else {
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

    /// Print the final test-run summary line.
    ///
    /// `passed` and `skipped` are the counts of tests with those outcomes.
    /// `total` is the total number of tests that were run; `failed` is derived
    /// as `total - passed - skipped`. `elapsed` is the wall-clock duration of
    /// the entire suite from start to finish.
    pub fn finish(&self, passed: usize, skipped: usize, total: usize, elapsed: Duration) {
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
