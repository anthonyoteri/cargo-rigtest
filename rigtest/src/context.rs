use std::any::Any;
use std::future::Future;
use std::sync::Arc;

use futures::FutureExt;

/// Shared context passed to every test function.
pub struct TestContext {
    /// Data produced by `#[global_setup]`, available to all tests.
    pub global_data: Arc<Box<dyn Any + Send + Sync>>,
    /// A reusable HTTP client for tests that make network calls.
    /// Available when the `http-client` feature is enabled.
    #[cfg(feature = "http-client")]
    pub client: reqwest::Client,
}

impl TestContext {
    /// Creates a new [`TestContext`] wrapping `global_data` produced by `#[global_setup]`.
    ///
    /// This is called by the cargo-rigtest coordinator inside each test subprocess
    /// before invoking the test function. Test authors receive an already-constructed
    /// `Arc<TestContext>` as the argument to their test function and do not call
    /// this directly.
    #[must_use]
    pub fn new(global_data: Box<dyn Any + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            global_data: Arc::new(global_data),
            #[cfg(feature = "http-client")]
            client: reqwest::Client::new(),
        })
    }

    /// Run per-test setup logic and return its result.
    ///
    /// The closure receives `global` — the deserialized global state produced
    /// by `#[global_setup]`. It contains only serializable configuration
    /// (URLs, ports, credentials); use it to create a live resource. The
    /// value returned from this closure (`T`) lives entirely within the test
    /// subprocess and may be any type — it is never serialized.
    ///
    /// On error or panic the test fails with a `"setup failed:"` / `"setup
    /// panicked"` prefix so the phase is unambiguous in the report.
    ///
    /// ```ignore
    /// let conn = ctx.setup(|global| async move {
    ///     let cfg = global.downcast_ref::<Config>().unwrap();
    ///     MyDb::connect(&cfg.db_url).await   // ? works naturally
    /// }).await?;
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the closure returns an error or if it panics.
    pub async fn setup<F, Fut, T>(&self, f: F) -> Result<T, crate::Error>
    where
        F: FnOnce(Arc<Box<dyn Any + Send + Sync>>) -> Fut,
        Fut: Future<Output = Result<T, crate::Error>>,
    {
        match std::panic::AssertUnwindSafe(f(Arc::clone(&self.global_data)))
            .catch_unwind()
            .await
        {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => Err(format!("setup failed: {e}").into()),
            Err(_) => Err(Box::from("setup panicked")),
        }
    }

    /// Run per-test teardown logic.
    ///
    /// The closure receives the same `Arc` to global setup data and returns
    /// `Result<(), rig::Error>`. Failures are reported as `"teardown
    /// failed:"` / `"teardown panicked"`, distinct from failures in the test
    /// body.
    ///
    /// ```ignore
    /// ctx.teardown(|global| async move {
    ///     let cfg = global.downcast_ref::<Config>().unwrap();
    ///     conn.release_back_to(&cfg.pool).await   // ? works naturally
    /// }).await?;
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the closure returns an error or if it panics.
    ///
    /// # Teardown and timeout
    ///
    /// If the test is killed by a `timeout` set on `#[testcase]`, this
    /// closure will **not** run — the entire subprocess is hard-killed before
    /// it has a chance to execute. Resources that must be released regardless
    /// of outcome should use RAII guards (e.g. `Drop` impls) or be managed in
    /// `#[global_teardown]`.
    pub async fn teardown<F, Fut>(&self, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(Arc<Box<dyn Any + Send + Sync>>) -> Fut,
        Fut: Future<Output = Result<(), crate::Error>>,
    {
        match std::panic::AssertUnwindSafe(f(Arc::clone(&self.global_data)))
            .catch_unwind()
            .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("teardown failed: {e}").into()),
            Err(_) => Err(Box::from("teardown panicked")),
        }
    }
}
