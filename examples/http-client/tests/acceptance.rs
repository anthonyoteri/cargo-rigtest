use rigtest::{global_setup, global_teardown, preflight, testcase, Preflight, TestContext};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// A profile-aware preflight demonstrating both `.dns(...)` and the
// 1-arg `fn(env: &str) -> Preflight` signature: the probe target
// switches based on the active profile name supplied by the framework.
// Pre-#47 the profile name comes from the `RIGTEST_PROFILE` env var,
// defaulting to the empty string — so the default branch resolves
// `localhost`, guaranteed to resolve on every supported platform.
//
// A real suite would typically also add a `.http("api", ...)` probe
// here, which would reuse the `configure_client` configurator declared
// below — see `rigtest/README.md` for the full primitive table.
#[preflight]
fn checks(env: &str) -> Preflight {
    let host: &'static str = match env {
        "prod" => "prod.example.com",
        "staging" => "staging.example.com",
        _ => "localhost",
    };
    Preflight::new().dns("dns_target", host)
}

#[derive(Serialize, Deserialize)]
pub struct SharedState {
    pub base_url: String,
}

#[global_setup]
async fn setup() -> SharedState {
    SharedState {
        base_url: "https://localhost:8443".to_string(),
    }
}

#[global_teardown]
async fn teardown(_state: SharedState) {}

/// Configure the shared HTTP client to accept self-signed TLS certificates.
///
/// This is useful when running acceptance tests against a local or staging
/// environment that uses a self-signed certificate.  Never use this in
/// production or against untrusted hosts.
#[allow(clippy::unnecessary_wraps)] // Result required by the http_client fn pointer signature
fn configure_client(
    builder: reqwest::ClientBuilder,
) -> Result<reqwest::ClientBuilder, rigtest::Error> {
    Ok(builder.danger_accept_invalid_certs(true))
}

#[testcase]
async fn builds_request_to_https_endpoint(
    ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = ctx.global::<SharedState>();

    let url = format!("{}/health", state.base_url);
    let request = ctx.client().await?.get(&url).build();
    assert!(request.is_ok(), "building a GET request should succeed");
    Ok(())
}

#[rigtest::main(http_client = configure_client)]
fn main() {}
