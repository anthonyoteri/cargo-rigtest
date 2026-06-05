//! Wire protocol between the coordinator process and test subprocesses.
//!
//! Both sides import from here so the contract is defined exactly once:
//! exit codes, the skip-reason stderr encoding, and the result type.

/// Exit code written by a subprocess to signal a skipped test.
pub(crate) const SKIP_EXIT_CODE: i32 = 2;

/// Prefix written to stderr by a skipped test so the coordinator can extract
/// the reason string.
const SKIP_PREFIX: &str = "cargo-rigtest-skip: ";

/// The decoded result of running one test subprocess.
#[derive(Clone)]
pub(crate) enum SubprocessOutcome {
    Passed,
    Skipped(String),
    Failed {
        reason: String,
        stdout: String,
        stderr: String,
    },
    TimedOut(std::time::Duration),
}

/// Encode a skip reason for writing to stderr inside a test subprocess.
pub(crate) fn encode_skip(reason: &str) -> String {
    format!("{SKIP_PREFIX}{reason}")
}

/// Extract the skip reason from subprocess stderr, or return an empty string.
pub(crate) fn decode_skip_reason(stderr: &str) -> String {
    stderr
        .lines()
        .find_map(|l| l.strip_prefix(SKIP_PREFIX))
        .unwrap_or("")
        .to_string()
}

pub(crate) fn exit_code_reason(code: Option<i32>) -> String {
    format!("exited with code {}", code.unwrap_or(-1))
}

/// Interpret a subprocess exit code and captured output into a
/// [`SubprocessOutcome`].
///
/// Does not handle the timed-out case; callers that detect a timeout should
/// return [`SubprocessOutcome::TimedOut`] directly.
pub(crate) fn decode_outcome(
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
) -> SubprocessOutcome {
    match exit_code {
        Some(0) => SubprocessOutcome::Passed,
        Some(c) if c == SKIP_EXIT_CODE => SubprocessOutcome::Skipped(decode_skip_reason(&stderr)),
        code => SubprocessOutcome::Failed {
            reason: exit_code_reason(code),
            stdout,
            stderr,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_skip_adds_prefix() {
        assert_eq!(encode_skip("no db"), "cargo-rigtest-skip: no db");
    }

    #[test]
    fn encode_skip_empty_reason() {
        assert_eq!(encode_skip(""), "cargo-rigtest-skip: ");
    }

    #[test]
    fn decode_outcome_pass() {
        assert!(matches!(
            decode_outcome(Some(0), String::new(), String::new()),
            SubprocessOutcome::Passed
        ));
    }

    #[test]
    fn decode_outcome_skip_extracts_reason() {
        let stderr = format!("{SKIP_PREFIX}needs network");
        let outcome = decode_outcome(Some(SKIP_EXIT_CODE), String::new(), stderr);
        assert!(matches!(outcome, SubprocessOutcome::Skipped(r) if r == "needs network"));
    }

    #[test]
    fn decode_outcome_skip_reason_on_second_line() {
        let stderr = format!("some noise\n{SKIP_PREFIX}real reason\nmore noise");
        let outcome = decode_outcome(Some(SKIP_EXIT_CODE), String::new(), stderr);
        assert!(matches!(outcome, SubprocessOutcome::Skipped(r) if r == "real reason"));
    }

    #[test]
    fn decode_outcome_fail_preserves_output() {
        let outcome = decode_outcome(Some(1), "out".into(), "err".into());
        assert!(matches!(
            outcome,
            SubprocessOutcome::Failed { stdout, stderr, .. }
            if stdout == "out" && stderr == "err"
        ));
    }

    #[test]
    fn decode_outcome_unknown_exit_code() {
        let outcome = decode_outcome(None, String::new(), String::new());
        assert!(matches!(
            outcome,
            SubprocessOutcome::Failed { reason, .. }
            if reason.contains("-1")
        ));
    }
}
