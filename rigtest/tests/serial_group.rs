//! Integration tests for the `serial` / `serial = "group"` flags on
//! `#[testcase]`. Like the parametrization tests, these live in the runtime
//! crate because verifying the generated registrations requires inspecting
//! `rigtest::registry::RIG_TEST_CASES`.

use std::sync::Arc;

use rigtest::registry::RIG_TEST_CASES;
use rigtest::{testcase, TestContext};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[testcase(serial)]
async fn bare_serial_case(_ctx: Arc<TestContext>) -> Result<(), BoxError> {
    Ok(())
}

#[testcase(serial = "db")]
async fn grouped_serial_case(_ctx: Arc<TestContext>) -> Result<(), BoxError> {
    Ok(())
}

fn find(name: &str) -> &'static rigtest::registry::TestCase {
    RIG_TEST_CASES
        .iter()
        .find(|tc| tc.name == name)
        .expect("case registered")
}

#[test]
fn bare_serial_sets_exclusive_flag_and_no_group() {
    let tc = find("bare_serial_case");
    assert!(tc.serial);
    assert_eq!(tc.serial_group, None);
}

#[test]
fn named_serial_sets_group_and_is_not_exclusive() {
    let tc = find("grouped_serial_case");
    assert!(!tc.serial);
    assert_eq!(tc.serial_group, Some("db"));
}
