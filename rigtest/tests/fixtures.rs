//! Integration tests for the `#[fixture]` macro and its by-parameter-name
//! injection into `#[testcase]` functions.
//!
//! Two axes are exercised:
//!
//! - Macro-generated wiring: fixtures registered as `#[testcase]` parameters
//!   are set up, injected by value, and torn down. These run through the same
//!   registration machinery as the rest of the suite; the assertions live in
//!   the test bodies (the registry only records that they registered).
//! - The generated fixture struct's inherent methods are also called directly
//!   from a plain `#[tokio::test]`, without the subprocess harness, to assert
//!   the returned value and the LIFO teardown order precisely.

// The teardown functions in this file only record an event; they are `async`
// because `#[fixture(teardown = ...)]` requires an `async fn(...)`.
#![allow(clippy::unused_async)]

use std::sync::Arc;
use std::sync::Mutex;

use rigtest::registry::RIG_TEST_CASES;
use rigtest::{fixture, testcase, TestContext};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// A fixture that returns a value. The test below asserts it receives exactly
// this value.
#[fixture]
async fn answer(_ctx: Arc<TestContext>) -> Result<u32, BoxError> {
    Ok(42)
}

// A no-parameter fixture — the generated setup accepts (and ignores) ctx.
#[fixture]
async fn greeting() -> Result<String, BoxError> {
    Ok("hello".to_string())
}

#[testcase]
async fn receives_fixture_value(_ctx: Arc<TestContext>, answer: u32) -> Result<(), BoxError> {
    assert_eq!(answer, 42);
    Ok(())
}

#[testcase]
async fn receives_two_fixtures(
    _ctx: Arc<TestContext>,
    answer: u32,
    greeting: String,
) -> Result<(), BoxError> {
    assert_eq!(answer, 42);
    assert_eq!(greeting, "hello");
    Ok(())
}

// A fixture defined in another module, using the fully-qualified attribute
// form. `pub` visibility lets it be imported by tests elsewhere.
mod other_module {
    use std::sync::Arc;

    use rigtest::TestContext;

    use super::BoxError;

    #[rigtest::fixture]
    pub async fn remote_fixture(_ctx: Arc<TestContext>) -> Result<&'static str, BoxError> {
        Ok("from other module")
    }
}

use other_module::remote_fixture;

#[testcase]
async fn uses_cross_module_fixture(
    _ctx: Arc<TestContext>,
    remote_fixture: &'static str,
) -> Result<(), BoxError> {
    assert_eq!(remote_fixture, "from other module");
    Ok(())
}

// Fixtures compose with `#[case]` parametrization: the fixture value is
// injected into every generated case alongside the per-case value.
#[testcase]
#[case(1)]
#[case(2)]
async fn case_with_fixture(
    _ctx: Arc<TestContext>,
    #[case] n: u32,
    answer: u32,
) -> Result<(), BoxError> {
    assert_eq!(answer, 42, "fixture injected into every case");
    assert!(n == 1 || n == 2);
    Ok(())
}

// Fixtures also compose with `#[values]`: the fixture is injected into every
// generated combination alongside the per-combination value.
#[testcase]
async fn values_with_fixture(
    _ctx: Arc<TestContext>,
    #[values(10u32, 20u32)] n: u32,
    answer: u32,
) -> Result<(), BoxError> {
    assert_eq!(answer, 42, "fixture injected into every combination");
    assert!(n == 10 || n == 20);
    Ok(())
}

#[test]
fn fixture_tests_register() {
    for name in [
        "receives_fixture_value",
        "receives_two_fixtures",
        "uses_cross_module_fixture",
        // One registration per `#[case]` row, each with the fixture wired in.
        "case_with_fixture::case_1",
        "case_with_fixture::case_2",
        // One registration per `#[values]` combination, fixture wired in.
        "values_with_fixture::case_1_10u32",
        "values_with_fixture::case_2_20u32",
    ] {
        let count = RIG_TEST_CASES.iter().filter(|tc| tc.name == name).count();
        assert_eq!(count, 1, "{name} must register exactly once");
    }
}

// --- Direct-call tests (no subprocess harness) ---------------------------

// Records setup/teardown events so the LIFO ordering can be asserted.
static EVENTS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

struct First;
struct Second;

#[fixture(teardown = teardown_first)]
async fn first(_ctx: Arc<TestContext>) -> Result<First, BoxError> {
    EVENTS.lock().unwrap().push("setup_first");
    Ok(First)
}

async fn teardown_first(_ctx: Arc<TestContext>) -> Result<(), BoxError> {
    EVENTS.lock().unwrap().push("teardown_first");
    Ok(())
}

#[fixture(teardown = teardown_second)]
async fn second(_ctx: Arc<TestContext>) -> Result<Second, BoxError> {
    EVENTS.lock().unwrap().push("setup_second");
    Ok(Second)
}

async fn teardown_second(_ctx: Arc<TestContext>) -> Result<(), BoxError> {
    EVENTS.lock().unwrap().push("teardown_second");
    Ok(())
}

fn ctx() -> Arc<TestContext> {
    TestContext::new(Box::new(())).expect("build TestContext")
}

#[tokio::test]
async fn setup_returns_value_and_teardown_runs() {
    let value = answer::__rigtest_fixture_setup(ctx())
        .await
        .expect("setup ok");
    assert_eq!(value, 42);

    // The default (no-op) teardown runs without error.
    answer::__rigtest_fixture_teardown(ctx())
        .await
        .expect("teardown ok");
}

#[tokio::test]
async fn two_fixtures_tear_down_lifo() {
    EVENTS.lock().unwrap().clear();

    // Set up left-to-right: first, then second.
    let _a = first::__rigtest_fixture_setup(ctx()).await.expect("setup");
    let _b = second::__rigtest_fixture_setup(ctx()).await.expect("setup");

    // Tear down LIFO: second, then first.
    second::__rigtest_fixture_teardown(ctx())
        .await
        .expect("teardown");
    first::__rigtest_fixture_teardown(ctx())
        .await
        .expect("teardown");

    let events = EVENTS.lock().unwrap().clone();
    assert_eq!(
        events,
        vec![
            "setup_first",
            "setup_second",
            "teardown_second",
            "teardown_first",
        ]
    );
}

// --- End-to-end wrapper test (invokes the generated test_fn directly) -----
//
// The `#[testcase]` bodies above only *register*; they run under the real
// subprocess harness. To exercise the generated setup -> body -> LIFO-teardown
// wrapper without a subprocess, grab the registered `test_fn` and call it.

static WRAP_EVENTS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

#[fixture(teardown = wrap_a_teardown)]
async fn wrap_a(_ctx: Arc<TestContext>) -> Result<i32, BoxError> {
    WRAP_EVENTS.lock().unwrap().push("setup_a");
    Ok(1)
}

async fn wrap_a_teardown(_ctx: Arc<TestContext>) -> Result<(), BoxError> {
    WRAP_EVENTS.lock().unwrap().push("teardown_a");
    Ok(())
}

#[fixture(teardown = wrap_b_teardown)]
async fn wrap_b(_ctx: Arc<TestContext>) -> Result<i32, BoxError> {
    WRAP_EVENTS.lock().unwrap().push("setup_b");
    Ok(2)
}

async fn wrap_b_teardown(_ctx: Arc<TestContext>) -> Result<(), BoxError> {
    WRAP_EVENTS.lock().unwrap().push("teardown_b");
    Ok(())
}

#[testcase]
async fn wrap_target(_ctx: Arc<TestContext>, wrap_a: i32, wrap_b: i32) -> Result<(), BoxError> {
    WRAP_EVENTS.lock().unwrap().push("body");
    assert_eq!(wrap_a, 1);
    assert_eq!(wrap_b, 2);
    Ok(())
}

#[tokio::test]
async fn generated_wrapper_orders_setup_body_teardown() {
    WRAP_EVENTS.lock().unwrap().clear();

    let tc = RIG_TEST_CASES
        .iter()
        .find(|tc| tc.name == "wrap_target")
        .expect("wrap_target registered");

    let result = (tc.test_fn)(ctx()).await;
    assert!(result.is_ok(), "wrapper returned {result:?}");

    let events = WRAP_EVENTS.lock().unwrap().clone();
    assert_eq!(
        events,
        vec!["setup_a", "setup_b", "body", "teardown_b", "teardown_a"],
        "fixtures set up left-to-right, torn down LIFO around the body",
    );
}

#[tokio::test]
async fn cross_module_fixture_wrapper_runs() {
    let tc = RIG_TEST_CASES
        .iter()
        .find(|tc| tc.name == "uses_cross_module_fixture")
        .expect("uses_cross_module_fixture registered");
    let result = (tc.test_fn)(ctx()).await;
    assert!(result.is_ok(), "wrapper returned {result:?}");
}
