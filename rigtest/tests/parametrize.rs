//! Integration tests for the `#[case]` table-driven parametrization
//! supported by `#[testcase]`.
//!
//! These tests live in the runtime crate (rather than `rigtest-macros`)
//! because verifying the generated registrations requires inspecting
//! `rigtest::registry::RIG_TEST_CASES`, which is only populated when the
//! macro emits code referencing the `rigtest` runtime.

use std::sync::Arc;

use rigtest::registry::RIG_TEST_CASES;
use rigtest::{testcase, TestContext};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// A non-parametrized testcase must continue to register exactly one
// entry under its function name — the historical behaviour.
#[testcase]
async fn plain_testcase_still_registers(_ctx: Arc<TestContext>) -> Result<(), BoxError> {
    Ok(())
}

// Two stacked `#[case(...)]` rows with no labels — generated names are
// `<fn>::case_1` and `<fn>::case_2`.
#[testcase]
#[case(1u32, 2u32, 3u32)]
#[case(10u32, 20u32, 30u32)]
async fn adds_pair(
    _ctx: Arc<TestContext>,
    #[case] a: u32,
    #[case] b: u32,
    #[case] sum: u32,
) -> Result<(), BoxError> {
    assert_eq!(a + b, sum);
    Ok(())
}

// Mix of labelled and unlabelled rows — labels suffix the generated name.
#[testcase]
#[case::small("alice")]
#[case("bob")]
#[case::with_dash_label("carol")]
async fn name_is_non_empty(_ctx: Arc<TestContext>, #[case] user: &str) -> Result<(), BoxError> {
    assert!(!user.is_empty());
    Ok(())
}

#[test]
fn plain_testcase_registers_exactly_once() {
    let count = RIG_TEST_CASES
        .iter()
        .filter(|tc| tc.name == "plain_testcase_still_registers")
        .count();
    assert_eq!(count, 1);
}

#[test]
fn parametrized_rows_register_one_case_per_row() {
    let names: Vec<&str> = RIG_TEST_CASES
        .iter()
        .filter(|tc| tc.name.starts_with("adds_pair"))
        .map(|tc| tc.name)
        .collect();
    assert!(names.contains(&"adds_pair::case_1"), "names: {names:?}");
    assert!(names.contains(&"adds_pair::case_2"), "names: {names:?}");
    assert_eq!(names.len(), 2, "names: {names:?}");
}

#[test]
fn labelled_rows_produce_indexed_suffixed_names() {
    let names: Vec<&str> = RIG_TEST_CASES
        .iter()
        .filter(|tc| tc.name.starts_with("name_is_non_empty"))
        .map(|tc| tc.name)
        .collect();
    assert!(names.contains(&"name_is_non_empty::case_1_small"));
    assert!(names.contains(&"name_is_non_empty::case_2"));
    assert!(names.contains(&"name_is_non_empty::case_3_with_dash_label"));
    assert_eq!(names.len(), 3, "names: {names:?}");
}

#[test]
fn generated_cases_inherit_module_and_file_metadata() {
    let case = RIG_TEST_CASES
        .iter()
        .find(|tc| tc.name == "adds_pair::case_1")
        .expect("case_1 registered");
    assert!(
        case.module.contains("parametrize"),
        "module: {}",
        case.module
    );
    assert!(case.file.ends_with("parametrize.rs"), "file: {}", case.file);
    assert!(!case.serial);
    assert!(case.timeout.is_none());
    assert_eq!(case.retries, 0);
}
