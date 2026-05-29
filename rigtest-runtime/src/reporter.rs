use std::time::Duration;

use console::style;

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}
use indicatif::MultiProgress;
use indicatif::ProgressBar;
use indicatif::ProgressDrawTarget;
use indicatif::ProgressStyle;

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

    /// Add a spinner for a newly started test. Returns a handle used by
    /// `test_passed` / `test_failed` to finalise the line.
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

    /// Finalise a test line as PASS.
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

    /// Print a retry notification. The spinner keeps running; the test is not
    /// yet finished.
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

    /// Finalise a test line as SKIP.
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

    /// Finalise a test line as FAIL, optionally printing captured output.
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

    /// Print a labeled phase header (e.g. "global setup", "global teardown").
    /// Output from the phase flows naturally after this line.
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

    /// Print the final summary.
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
