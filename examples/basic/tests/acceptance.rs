use rigtest::{global_setup, global_teardown, preflight, testcase, Preflight, TestContext};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// A preflight that verifies the environment carries the basics every
// shell-launched test depends on. `HOME` is set in every supported CI
// environment, so this probe passes deterministically.
#[preflight]
fn checks() -> Preflight {
    Preflight::new().env("home_is_set", "HOME")
}

#[derive(Serialize, Deserialize)]
pub struct SharedState {
    pub base_url: String,
}

#[global_setup]
async fn setup() -> SharedState {
    SharedState {
        base_url: "http://localhost:8080".to_string(),
    }
}

#[global_teardown]
async fn teardown(state: SharedState) {
    println!("teardown: releasing resources for {}", state.base_url);
}

#[testcase]
async fn simple_computation(
    _ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let result = (1..=10).sum::<i32>();
    assert_eq!(result, 55, "sum of 1..=10 should be 55");
    Ok(())
}

#[testcase]
async fn accesses_global_data(
    ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = ctx.global::<SharedState>();
    assert!(
        state.base_url.starts_with("http"),
        "base_url should be an HTTP URL, got: {}",
        state.base_url,
    );
    Ok(())
}

#[testcase]
async fn async_sleep(
    _ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    Ok(())
}

#[testcase]
async fn builds_http_request(
    ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let request = ctx
        .client()
        .await?
        .get("http://example.invalid/health")
        .build();
    assert!(request.is_ok(), "building a GET request should succeed");
    Ok(())
}

// A parametrized testcase. Each `#[case(...)]` row is registered as its
// own test (`role_is_recognised::case_1`, `::case_2_viewer`, `::case_3`)
// and is reported and filterable independently.
#[testcase]
#[case("alice", "admin")]
#[case::viewer("bob", "viewer")]
#[case("carol", "admin")]
async fn role_is_recognised(
    _ctx: Arc<TestContext>,
    #[case] user: &str,
    #[case] expected_role: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    assert!(!user.is_empty(), "user name should not be empty");
    assert!(
        matches!(expected_role, "admin" | "viewer"),
        "unexpected role: {expected_role}",
    );
    Ok(())
}

#[rigtest::main]
fn main() {}
