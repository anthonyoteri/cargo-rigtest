use anyhow::anyhow;
use futures::FutureExt as _;

use crate::context::TestContext;
use crate::protocol;
use crate::registry::{RIG_GLOBAL_SETUP, RIG_TEST_CASES};
use crate::state::StateHandoff;

/// Deserialize global state, run exactly one named test, and exit.
///
/// Called in subprocess mode when `--run-single` is present.
pub(crate) async fn run_single(test_name: &str, state_var: Option<&str>) -> anyhow::Result<()> {
    let global_data: Box<dyn std::any::Any + Send + Sync> = match state_var {
        Some(var) => StateHandoff::load(var, RIG_GLOBAL_SETUP.first()),
        None => Box::new(()),
    };

    let tc = RIG_TEST_CASES
        .iter()
        .find(|tc| tc.name == test_name)
        .ok_or_else(|| anyhow!("cargo-rigtest: no test named '{test_name}'"))?;

    let ctx = TestContext::new(global_data)
        .map_err(|e| anyhow!("failed to configure HTTP client: {e}"))?;

    let result = std::panic::AssertUnwindSafe((tc.test_fn)(ctx))
        .catch_unwind()
        .await;

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            if e.downcast_ref::<crate::Skip>().is_some() {
                eprintln!("{}", protocol::encode_skip(&e.to_string()));
                crate::flush_and_exit(protocol::SKIP_EXIT_CODE);
            }
            if e.downcast_ref::<crate::NotRetryEligible>().is_some() {
                // Print the underlying message before exiting so the
                // coordinator's captured stderr still carries the human
                // failure reason; only the eligibility hint is encoded in
                // the exit code.
                eprintln!("{e}");
                crate::flush_and_exit(protocol::FAIL_NOT_RETRYABLE_EXIT_CODE);
            }
            Err(anyhow!("{e}"))
        }
        Err(_) => Err(anyhow!("panicked")),
    }
}
