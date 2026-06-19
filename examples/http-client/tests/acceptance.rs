use rigtest::{global_setup, global_teardown, preflight, testcase, Preflight, TestContext};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// A preflight that demonstrates `.dns(...)` — the deterministic, no-port
// DNS probe. We resolve `localhost`, which is guaranteed to resolve on
// every supported platform without a working DNS resolver, so this
// example links and runs in CI without external dependencies.
//
// A real-world HTTP-tested suite would typically also include a
// `.http("api", "https://staging.example.com/health")` probe, which
// reuses the `configure_client` configurator declared below — see
// `rigtest/README.md` for the full primitive table.
#[preflight]
fn checks() -> Preflight {
    Preflight::new().dns("dns_localhost", "localhost")
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
