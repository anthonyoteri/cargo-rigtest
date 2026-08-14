//! Integration tests for the `#[values(...)]` cartesian parametrization
//! supported by `#[testcase]`.
//!
//! Like `parametrize.rs`, these live in the runtime crate because verifying
//! the generated registrations requires inspecting
//! `rigtest::registry::RIG_TEST_CASES`, which is only populated when the
//! macro emits code referencing the `rigtest` runtime.

use std::sync::Arc;

use rigtest::registry::RIG_TEST_CASES;
use rigtest::{TestContext, testcase};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// A single `#[values]` param expands to one case per value.
#[testcase]
async fn single_values(
    _ctx: Arc<TestContext>,
    #[values("a", "b", "c")] tag: &str,
) -> Result<(), BoxError> {
    assert!(!tag.is_empty());
    Ok(())
}

// Two `#[values]` params expand to their cartesian product (3 × 2 = 6),
// with the last param varying fastest.
#[testcase]
async fn method_status(
    _ctx: Arc<TestContext>,
    #[values("GET", "POST", "PUT")] method: &str,
    #[values(200, 404)] status: u16,
) -> Result<(), BoxError> {
    assert!(!method.is_empty());
    assert!(status >= 200);
    Ok(())
}

// `#[case]` rows compose with `#[values]`: case rows are the outermost
// dimension (2 rows × 2 values = 4 cases).
#[testcase]
#[case::lo(1u32)]
#[case::hi(10u32)]
async fn case_times_values(
    _ctx: Arc<TestContext>,
    #[case] base: u32,
    #[values("x", "y")] tag: &str,
) -> Result<(), BoxError> {
    assert!(base >= 1);
    assert!(!tag.is_empty());
    Ok(())
}

fn names_starting_with(prefix: &str) -> Vec<&'static str> {
    RIG_TEST_CASES
        .iter()
        .filter(|tc| tc.name.starts_with(prefix))
        .map(|tc| tc.name)
        .collect()
}

#[test]
fn single_values_registers_one_case_per_value() {
    let names = names_starting_with("single_values");
    assert!(
        names.contains(&"single_values::case_1_a"),
        "names: {names:?}"
    );
    assert!(
        names.contains(&"single_values::case_2_b"),
        "names: {names:?}"
    );
    assert!(
        names.contains(&"single_values::case_3_c"),
        "names: {names:?}"
    );
    assert_eq!(names.len(), 3, "names: {names:?}");
}

#[test]
fn values_product_registers_every_combination() {
    let names = names_starting_with("method_status");
    for expected in [
        "method_status::case_1_GET_200",
        "method_status::case_2_GET_404",
        "method_status::case_3_POST_200",
        "method_status::case_4_POST_404",
        "method_status::case_5_PUT_200",
        "method_status::case_6_PUT_404",
    ] {
        assert!(
            names.contains(&expected),
            "missing {expected}; names: {names:?}"
        );
    }
    assert_eq!(names.len(), 6, "names: {names:?}");
}

#[test]
fn case_and_values_compose_as_a_product() {
    let names = names_starting_with("case_times_values");
    for expected in [
        "case_times_values::case_1_lo_x",
        "case_times_values::case_2_lo_y",
        "case_times_values::case_3_hi_x",
        "case_times_values::case_4_hi_y",
    ] {
        assert!(
            names.contains(&expected),
            "missing {expected}; names: {names:?}"
        );
    }
    assert_eq!(names.len(), 4, "names: {names:?}");
}

#[test]
fn generated_values_cases_inherit_metadata() {
    let case = RIG_TEST_CASES
        .iter()
        .find(|tc| tc.name == "method_status::case_1_GET_200")
        .expect("case_1 registered");
    assert!(case.module.contains("values"), "module: {}", case.module);
    assert!(case.file.ends_with("values.rs"), "file: {}", case.file);
    assert!(!case.serial);
    assert!(case.timeout.is_none());
    assert_eq!(case.retries, 0);
}
