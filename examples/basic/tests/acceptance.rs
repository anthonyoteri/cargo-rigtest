// Test functions must be `async` for the framework's BoxFuture signature.
#![allow(clippy::unused_async)]

use rigtest::{global_setup, global_teardown, testcase, TestContext};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Shared state created during global setup and passed to every test.
#[derive(Serialize, Deserialize)]
pub struct SharedState {
    pub base_url: String,
}

/// Set up shared state once before the suite runs.
#[global_setup]
async fn setup() -> SharedState {
    SharedState {
        base_url: "http://localhost:8080".to_string(),
    }
}

/// Tear down shared state after the suite finishes.
#[global_teardown]
async fn teardown(state: SharedState) {
    println!("teardown: releasing resources for {}", state.base_url);
}

/// A test that always passes by validating a simple computation.
#[testcase]
async fn simple_computation(
    _ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let result = (1..=10).sum::<i32>();
    assert_eq!(result, 55, "sum of 1..=10 should be 55");
    Ok(())
}

/// A test that accesses `ctx.global_data` and downcasts to `SharedState`.
#[testcase]
async fn accesses_global_data(
    ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = ctx
        .global_data
        .downcast_ref::<SharedState>()
        .expect("global_data should be SharedState");
    assert!(
        state.base_url.starts_with("http"),
        "base_url should be an HTTP URL, got: {}",
        state.base_url,
    );
    Ok(())
}

/// A test that demonstrates async work with a brief sleep.
#[testcase]
async fn async_sleep(
    _ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    Ok(())
}

/// A test that builds an HTTP request without sending it (no real server needed).
#[testcase]
async fn builds_http_request(
    ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let request = ctx.client.get("http://example.invalid/health").build();
    assert!(request.is_ok(), "building a GET request should succeed");
    Ok(())
}

fn main() {
    rigtest::run_main();
}
