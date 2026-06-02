use std::any::Any;
use std::future::Future;
use std::sync::Arc;

use futures::FutureExt;
#[cfg(feature = "http-client")]
use tokio::sync::OnceCell;

/// Shared context passed to every test function.
pub struct TestContext {
    /// Data produced by `#[global_setup]`, available to all tests.
    pub global_data: Arc<Box<dyn Any + Send + Sync>>,
    /// Lazily-initialized HTTP client. Built on first call to [`Self::client`].
    #[cfg(feature = "http-client")]
    client: OnceCell<reqwest::Client>,
    /// SSH sessions cached by destination string. Unix only.
    #[cfg(all(feature = "ssh-client", unix))]
    ssh_sessions:
        tokio::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<openssh::Session>>>,
}

impl TestContext {
    /// Creates a new [`TestContext`] wrapping `global_data` produced by `#[global_setup]`.
    ///
    /// This is called by the cargo-rigtest coordinator inside each test subprocess
    /// before invoking the test function. Test authors receive an already-constructed
    /// `Arc<TestContext>` as the argument to their test function and do not call
    /// this directly.
    ///
    /// # Errors
    ///
    /// Returns an error if an internal invariant is violated during construction.
    pub fn new(global_data: Box<dyn Any + Send + Sync>) -> Result<Arc<Self>, crate::Error> {
        Ok(Arc::new(Self {
            global_data: Arc::new(global_data),
            #[cfg(feature = "http-client")]
            client: OnceCell::new(),
            #[cfg(all(feature = "ssh-client", unix))]
            ssh_sessions: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }))
    }

    /// Returns a reference to the shared HTTP client, constructing it on first call.
    ///
    /// The client is built lazily: tests that skip or never make network calls
    /// pay no TLS initialization cost. To customize the client (e.g. to accept
    /// self-signed certificates), register a configurator via
    /// `#[rigtest::main(http_client = your_fn)]` — see the
    /// [`http-client` example](https://github.com/anthonyoteri/cargo-rigtest/tree/main/examples/http-client)
    /// for a complete example.
    ///
    /// # Errors
    ///
    /// Returns an error if the registered `http_client` configurator returns an
    /// error, or if `reqwest::ClientBuilder::build` fails.
    #[cfg(feature = "http-client")]
    pub async fn client(&self) -> Result<&reqwest::Client, crate::Error> {
        self.client
            .get_or_try_init(|| async {
                let builder = reqwest::ClientBuilder::new();
                let builder =
                    if let Some(entry) = crate::registry::RIG_HTTP_CLIENT_CONFIGURATOR.first() {
                        (entry.configure_fn)(builder)?
                    } else {
                        builder
                    };
                Ok(builder.build()?)
            })
            .await
    }

    /// Returns a cached SSH session for `destination`, connecting on first call.
    ///
    /// `destination` may be any value accepted by the `ssh` binary — for example
    /// `"user@host"`, `"host"`, or an alias defined in `~/.ssh/config`. Sessions
    /// are cached by destination string within this subprocess; a second call with
    /// the same destination returns the existing session without reconnecting.
    ///
    /// To customize the connection (e.g. override known-hosts checking or select
    /// a non-default identity file), register a configurator via
    /// `#[rigtest::main(ssh_client = your_fn)]`.
    ///
    /// # Platform support
    ///
    /// This method is only available on Unix. The underlying [`openssh`] crate
    /// delegates to the system `ssh` binary, which is a Unix-only dependency.
    /// The `ssh-client` feature has no effect on non-Unix targets.
    ///
    /// # Errors
    ///
    /// Returns an error if the registered `ssh_client` configurator returns an
    /// error, or if the underlying SSH connection cannot be established.
    #[cfg(all(feature = "ssh-client", unix))]
    pub async fn ssh(
        &self,
        destination: &str,
    ) -> Result<std::sync::Arc<openssh::Session>, crate::Error> {
        let mut sessions = self.ssh_sessions.lock().await;
        if let Some(session) = sessions.get(destination) {
            return Ok(std::sync::Arc::clone(session));
        }
        let builder = openssh::SessionBuilder::default();
        let builder = if let Some(entry) = crate::registry::RIG_SSH_CLIENT_CONFIGURATOR.first() {
            (entry.configure_fn)(destination, builder)?
        } else {
            builder
        };
        let session = builder.connect(destination).await?;
        let arc = std::sync::Arc::new(session);
        sessions.insert(destination.to_string(), std::sync::Arc::clone(&arc));
        Ok(arc)
    }

    /// Returns a reference to the global setup state, downcasting to `T`.
    ///
    /// This is a typed shorthand for
    /// `ctx.global_data.downcast_ref::<T>().expect(…)`. The `global_data`
    /// field remains public so existing code using `downcast_ref` directly
    /// continues to compile unchanged.
    ///
    /// # Panics
    ///
    /// Panics if the stored global state is not of type `T`. This indicates a
    /// programming error — the type passed to `global` must match the type
    /// returned by `#[global_setup]`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use rigtest::{TestContext, testcase};
    /// # #[derive(serde::Serialize, serde::Deserialize)]
    /// # struct State { base_url: String }
    /// #[testcase]
    /// async fn homepage_is_up(ctx: Arc<TestContext>) -> Result<(), rigtest::Error> {
    ///     let state = ctx.global::<State>();
    ///     // use state.base_url…
    ///     Ok(())
    /// }
    /// ```
    #[must_use]
    pub fn global<T: Any + Send + Sync + 'static>(&self) -> &T {
        self.global_data
            .downcast_ref::<T>()
            .expect("global_data type mismatch: the type passed to ctx.global::<T>() must match the type returned by #[global_setup]")
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
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use rigtest::{TestContext, Error};
    /// # struct Config { db_url: String }
    /// # struct Conn;
    /// # async fn db_connect(_: &str) -> Result<Conn, Error> { Ok(Conn) }
    /// # async fn example(ctx: Arc<TestContext>) -> Result<(), Error> {
    /// let conn = ctx.setup(|global| async move {
    ///     let cfg = global.downcast_ref::<Config>().unwrap();
    ///     db_connect(&cfg.db_url).await
    /// }).await?;
    /// # Ok(())
    /// # }
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
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use rigtest::{TestContext, Error};
    /// # struct Config { pool: String }
    /// # struct Conn;
    /// # impl Conn {
    /// #     async fn release_back_to(self, _: &str) -> Result<(), Error> { Ok(()) }
    /// # }
    /// # async fn example(ctx: Arc<TestContext>, conn: Conn) -> Result<(), Error> {
    /// ctx.teardown(|global| async move {
    ///     let cfg = global.downcast_ref::<Config>().unwrap();
    ///     conn.release_back_to(&cfg.pool).await
    /// }).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the closure returns an error or if it panics.
    ///
    /// # Teardown and timeout
    ///
    /// If the test is killed by a `timeout` set on `#[testcase]`, this
    /// closure will **not** run — the entire subprocess is terminated before
    /// it has a chance to execute. Resources that must be released regardless
    /// of outcome should be managed in `#[global_teardown]`, which runs in
    /// the coordinator process outside the killed subprocess.
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
