//! Retry policy: classifies one subprocess attempt's raw outcome into an
//! [`AttemptPlan`] the orchestrator loop can dispatch without re-deriving
//! eligibility rules.
//!
//! The matcher itself evaluates subprocess-side (ADR-0004); this module owns
//! the coordinator-side combining logic that decides whether a `Failed`,
//! `TimedOut`, or `Err` attempt is eligible for another try given the test's
//! `retry_on_error` declaration and the failure's panic-vs-assertion shape.

use crate::protocol::SubprocessOutcome;
use crate::registry::TestCase;
use crate::reporter::Outcome as ReportOutcome;

/// One attempt's classification. The orchestrator loop pattern-matches this
/// to fire the correct reporter event and decide whether to loop again.
pub(crate) enum AttemptPlan {
    Passed,
    Skipped {
        reason: String,
    },
    Failed {
        kind: ReportOutcome,
        reason: String,
        stdout: String,
        stderr: String,
        /// `true` when the failure is eligible for another attempt under
        /// the test's `retry_on_error` policy. Independent of whether
        /// attempts remain — the loop combines this with `is_last`.
        retryable: bool,
    },
}

/// Classify one attempt's raw subprocess result.
///
/// Folds every coordinator-side retry decision into one place:
///
/// - `Passed` / `Skipped` are never retryable.
/// - `Failed` with `retry_eligible == false` (matcher mismatch) is terminal.
/// - `Failed` carrying a panic in stderr is terminal when a `retry_on_error`
///   matcher is in force, retryable otherwise.
/// - `TimedOut` and spawn-side `Err` ("framework failures") are terminal
///   when a matcher is in force, retryable otherwise.
pub(crate) fn plan(raw: anyhow::Result<SubprocessOutcome>, tc: &TestCase) -> AttemptPlan {
    let matcher_in_force = tc.retry_on_error_set;
    match raw {
        Ok(SubprocessOutcome::Passed) => AttemptPlan::Passed,
        Ok(SubprocessOutcome::Skipped(reason)) => AttemptPlan::Skipped { reason },
        Ok(SubprocessOutcome::Failed {
            reason,
            stdout,
            stderr,
            retry_eligible,
        }) => {
            let kind = classify_failed(&stderr);
            let panic_with_matcher = matches!(kind, ReportOutcome::Panic) && matcher_in_force;
            let retryable = retry_eligible && !panic_with_matcher;
            AttemptPlan::Failed {
                kind,
                reason,
                stdout,
                stderr,
                retryable,
            }
        }
        Ok(SubprocessOutcome::TimedOut(dur)) => AttemptPlan::Failed {
            kind: ReportOutcome::Timeout,
            reason: format!("timed out after {:.1}s", dur.as_secs_f64()),
            stdout: String::new(),
            stderr: String::new(),
            retryable: !matcher_in_force,
        },
        Err(e) => AttemptPlan::Failed {
            kind: ReportOutcome::Crash,
            reason: e.to_string(),
            stdout: String::new(),
            stderr: String::new(),
            retryable: !matcher_in_force,
        },
    }
}

fn classify_failed(stderr: &str) -> ReportOutcome {
    if stderr.contains("panicked at") {
        ReportOutcome::Panic
    } else {
        ReportOutcome::Assertion
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{BoxError, TestCase, TestFn};
    use std::pin::Pin;
    use std::sync::Arc;

    fn noop_test_fn(
        _: Arc<crate::context::TestContext>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), BoxError>> + Send + 'static>> {
        Box::pin(async { Ok(()) })
    }

    fn case(retry_on_error_set: bool) -> TestCase {
        let test_fn: TestFn = noop_test_fn;
        TestCase {
            name: "t",
            module: "m",
            file: "f",
            serial: false,
            timeout: None,
            retries: 0,
            retry_on_error_set,
            tags: &[],
            test_fn,
        }
    }

    fn failed(retry_eligible: bool, stderr: &str) -> SubprocessOutcome {
        SubprocessOutcome::Failed {
            reason: "boom".into(),
            stdout: String::new(),
            stderr: stderr.into(),
            retry_eligible,
        }
    }

    #[test]
    fn passed_is_terminal_pass() {
        assert!(matches!(
            plan(Ok(SubprocessOutcome::Passed), &case(false)),
            AttemptPlan::Passed
        ));
    }

    #[test]
    fn skipped_carries_reason() {
        let p = plan(Ok(SubprocessOutcome::Skipped("nope".into())), &case(false));
        assert!(matches!(p, AttemptPlan::Skipped { reason } if reason == "nope"));
    }

    #[test]
    fn failed_assertion_retryable_without_matcher() {
        let p = plan(Ok(failed(true, "")), &case(false));
        assert!(matches!(
            p,
            AttemptPlan::Failed {
                kind: ReportOutcome::Assertion,
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn failed_matcher_mismatch_never_retries() {
        // retry_eligible == false comes from the subprocess when the
        // matcher rejected the typed Err; the policy must honor that
        // regardless of matcher_in_force.
        let p = plan(Ok(failed(false, "")), &case(true));
        assert!(matches!(
            p,
            AttemptPlan::Failed {
                retryable: false,
                ..
            }
        ));
    }

    #[test]
    fn failed_panic_retryable_without_matcher() {
        let p = plan(
            Ok(failed(true, "thread 'main' panicked at src/x.rs:1")),
            &case(false),
        );
        assert!(matches!(
            p,
            AttemptPlan::Failed {
                kind: ReportOutcome::Panic,
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn failed_panic_terminal_with_matcher() {
        let p = plan(
            Ok(failed(true, "thread 'main' panicked at src/x.rs:1")),
            &case(true),
        );
        assert!(matches!(
            p,
            AttemptPlan::Failed {
                kind: ReportOutcome::Panic,
                retryable: false,
                ..
            }
        ));
    }

    #[test]
    fn timed_out_retryable_without_matcher() {
        let p = plan(
            Ok(SubprocessOutcome::TimedOut(std::time::Duration::from_secs(
                3,
            ))),
            &case(false),
        );
        assert!(matches!(
            p,
            AttemptPlan::Failed {
                kind: ReportOutcome::Timeout,
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn timed_out_terminal_with_matcher() {
        let p = plan(
            Ok(SubprocessOutcome::TimedOut(std::time::Duration::from_secs(
                3,
            ))),
            &case(true),
        );
        assert!(matches!(
            p,
            AttemptPlan::Failed {
                kind: ReportOutcome::Timeout,
                retryable: false,
                ..
            }
        ));
    }

    #[test]
    fn spawn_error_retryable_without_matcher() {
        let p = plan(Err(anyhow::anyhow!("boom")), &case(false));
        assert!(matches!(
            p,
            AttemptPlan::Failed {
                kind: ReportOutcome::Crash,
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn spawn_error_terminal_with_matcher() {
        let p = plan(Err(anyhow::anyhow!("boom")), &case(true));
        assert!(matches!(
            p,
            AttemptPlan::Failed {
                kind: ReportOutcome::Crash,
                retryable: false,
                ..
            }
        ));
    }
}
