//! Integration tests for the `#[testcase(retry_on_error = …)]` matcher.
//!
//! These tests verify the macro accepts a real Rust pattern and registers
//! a [`TestCase`] with `retry_on_error_set: true`. The orchestrator-level
//! retry semantics are covered in `rigtest::orchestrator::tests`; here we
//! only assert on what the macro emits.

use std::sync::Arc;

use rigtest::registry::RIG_TEST_CASES;
use rigtest::{testcase, TestContext};

#[derive(Debug)]
#[allow(dead_code)] // The variants exist only to be referenced inside the
                    // `retry_on_error = …` pattern fed into `matches!`; the test bodies in
                    // this file never construct an `Err(_)` so the analyzer flags them.
enum DemoError {
    Transient,
    Fatal(String),
}

impl std::fmt::Display for DemoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient => f.write_str("transient"),
            Self::Fatal(msg) => write!(f, "fatal: {msg}"),
        }
    }
}

impl std::error::Error for DemoError {}

// A test with `retry_on_error` set against a single variant — the
// simplest shape. The pattern is spliced verbatim into a `matches!`
// arm by the macro; the test body never runs in this harness, only the
// macro-generated registration is inspected.
#[testcase(retries = 2, retry_on_error = DemoError::Transient)]
async fn retries_on_transient(_ctx: Arc<TestContext>) -> Result<(), DemoError> {
    Ok(())
}

// Alternative patterns and `if` guards are part of the surface area.
#[testcase(retry_on_error = DemoError::Transient | DemoError::Fatal(_))]
async fn retries_on_either_variant(_ctx: Arc<TestContext>) -> Result<(), DemoError> {
    Ok(())
}

#[test]
fn matcher_sets_retry_on_error_set_flag() {
    let tc = RIG_TEST_CASES
        .iter()
        .find(|tc| tc.name == "retries_on_transient")
        .expect("retries_on_transient must be registered");
    assert!(
        tc.retry_on_error_set,
        "retry_on_error = … must register retry_on_error_set: true"
    );
    assert_eq!(tc.retries, 2);
}

#[test]
fn alternatives_and_guards_compile_into_registration() {
    let tc = RIG_TEST_CASES
        .iter()
        .find(|tc| tc.name == "retries_on_either_variant")
        .expect("retries_on_either_variant must be registered");
    assert!(tc.retry_on_error_set);
}

#[test]
fn no_matcher_leaves_flag_false() {
    // A plain `#[testcase]` in the same binary as a control — `retry_on_error_set`
    // must remain `false` when the attribute is absent.
    let plain = RIG_TEST_CASES
        .iter()
        .find(|tc| tc.name == "plain_no_matcher")
        .expect("plain_no_matcher must be registered");
    assert!(!plain.retry_on_error_set);
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[testcase]
async fn plain_no_matcher(_ctx: Arc<TestContext>) -> Result<(), BoxError> {
    Ok(())
}
