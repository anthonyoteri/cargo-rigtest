use anyhow::anyhow;
use futures::FutureExt as _;

use crate::context::TestContext;
use crate::protocol;
use crate::registry::{RIG_GLOBAL_SETUP, RIG_TEST_CASES};

/// Deserialize global state, run exactly one named test, and exit.
///
/// Called in subprocess mode when `--run-single` is present.
pub(crate) async fn run_single(test_name: &str, state_var: Option<&str>) -> anyhow::Result<()> {
    let global_data: Box<dyn std::any::Any + Send + Sync> = if let Some(var) = state_var {
        let json = std::env::var(var).unwrap_or_default();
        // Remove the env var so it is not visible to the test function or any
        // child processes it might spawn.
        //
        // SAFETY: single-threaded at this point — the Tokio runtime has not
        // yet spawned any threads, and no other threads read this variable.
        unsafe { std::env::remove_var(var) };

        if let Some(entry) = RIG_GLOBAL_SETUP.first() {
            (entry.deserialize_fn)(&json)
        } else {
            Box::new(())
        }
    } else {
        Box::new(())
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
            Err(anyhow!("{e}"))
        }
        Err(_) => Err(anyhow!("panicked")),
    }
}
