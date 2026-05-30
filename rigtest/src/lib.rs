#![warn(clippy::pedantic)]
//! Runtime library for the [`cargo-rigtest`] acceptance-testing framework.
//!
//! This crate provides the attributes, context, and entry point needed to write
//! tests that run against a live, deployed system — a staging environment, a
//! real database, a running service. Tests are compiled into a standard Cargo
//! test binary (with `harness = false`) and driven by the `cargo rigtest` CLI,
//! which runs each test case in its own subprocess for process-level isolation.
//!
//! # Getting started
//!
//! Add `rigtest` to your dev-dependencies and declare a test target with
//! `harness = false`:
//!
//! ```toml
//! # Cargo.toml
//! [dev-dependencies]
//! rigtest = "0.1"
//!
//! [[test]]
//! name = "acceptance"
//! path = "tests/acceptance.rs"
//! harness = false
//! ```
//!
//! A minimal test file:
//!
//! ```no_run
//! use std::sync::Arc;
//! use rigtest::prelude::*;
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Serialize, Deserialize)]
//! struct State { base_url: String }
//!
//! #[global_setup]
//! async fn setup() -> State {
//!     State {
//!         base_url: std::env::var("BASE_URL")
//!             .unwrap_or_else(|_| "http://localhost:8080".into()),
//!     }
//! }
//!
//! #[global_teardown]
//! async fn teardown(_state: State) {}
//!
//! #[testcase]
//! async fn homepage_is_up(ctx: Arc<TestContext>) -> Result<(), rigtest::Error> {
//!     let state = ctx.global_data.downcast_ref::<State>().unwrap();
//!     // make assertions against state.base_url…
//!     Ok(())
//! }
//!
//! fn main() { rigtest::run_main(); }
//! ```
//!
//! Run the suite with:
//!
//! ```text
//! cargo rigtest run
//! ```
//!
//! # The testing model
//!
//! `cargo-rigtest` separates orchestration from execution. The coordinator
//! (run by `cargo rigtest run`) calls [`#[global_setup]`][`global_setup`]
//! once to produce shared state, then spawns each test case as an independent
//! subprocess. Each subprocess deserializes the global state, runs its test
//! function, and exits. When all tests have finished the coordinator calls
//! [`#[global_teardown]`][`global_teardown`].
//!
//! Because every test is a separate process:
//!
//! - A panic, crash, or `process::exit` in one test cannot affect others.
//! - Tests run in parallel by default (configurable with `--jobs`).
//! - Any resource a test opens lives only for the lifetime of that subprocess.
//!
//! # Attributes
//!
//! ## `#[testcase]`
//!
//! Registers an async function as a test case. The function must accept
//! `Arc<`[`TestContext`]`>` and return `Result<(), `[`Error`]`>`:
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use rigtest::TestContext;
//! # use rigtest::testcase;
//! #[testcase]
//! async fn my_test(ctx: Arc<TestContext>) -> Result<(), rigtest::Error> {
//!     Ok(())
//! }
//! ```
//!
//! Optional flags can be combined in any order:
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use rigtest::TestContext;
//! # use rigtest::testcase;
//! #[testcase(serial, timeout = std::time::Duration::from_secs(30), retries = 2)]
//! async fn careful_test(ctx: Arc<TestContext>) -> Result<(), rigtest::Error> {
//!     Ok(())
//! }
//! ```
//!
//! | Flag | Description |
//! |------|-------------|
//! | `serial` | Run this test exclusively — no other test runs concurrently |
//! | `timeout = <Duration>` | Terminate the subprocess if it runs too long |
//! | `retries = <N>` | Retry a failing test up to N additional times |
//!
//! ## `#[global_setup]`
//!
//! Runs once before any test in the suite. Returns a value that is serialized
//! and passed to every test subprocess as the global state. At most one may be
//! defined.
//!
//! ```no_run
//! # use serde::{Serialize, Deserialize};
//! # use rigtest::global_setup;
//! # #[derive(Serialize, Deserialize)]
//! # struct MyState { db_url: String }
//! #[global_setup]
//! async fn setup() -> MyState {
//!     MyState { db_url: std::env::var("DATABASE_URL").unwrap() }
//! }
//! ```
//!
//! The return type must implement [`serde::Serialize`] and
//! [`serde::Deserialize`] — the value crosses a process boundary and must
//! survive serialization. Store configuration (URLs, ports, credentials,
//! identifiers) rather than live resources (connection pools, file
//! descriptors, socket handles).
//!
//! ## `#[global_teardown]`
//!
//! Runs once after all tests finish. Receives the deserialized state produced
//! by `#[global_setup]`. At most one may be defined.
//!
//! ```no_run
//! # use serde::{Serialize, Deserialize};
//! # use rigtest::global_teardown;
//! # #[derive(Serialize, Deserialize)]
//! # struct MyState { db_url: String }
//! #[global_teardown]
//! async fn teardown(state: MyState) {
//!     println!("cleaning up {}", state.db_url);
//! }
//! ```
//!
//! Because `#[global_teardown]` runs in the coordinator process — outside any
//! test subprocess — it is the right place to clean up resources that must be
//! released regardless of how individual tests finish, including tests that
//! time out.
//!
//! # Test context
//!
//! Every test receives an `Arc<`[`TestContext`]`>`. It exposes:
//!
//! - **[`global_data`][TestContext::global_data]** — the deserialized global
//!   state from `#[global_setup]`, accessed via `downcast_ref`.
//! - **[`setup`][TestContext::setup] / [`teardown`][TestContext::teardown]** —
//!   async closures for per-test resource lifecycle. Failures are labelled
//!   `"setup failed:"` or `"teardown failed:"` in the report so the phase is
//!   unambiguous.
//! - **`client`** — a shared `reqwest::Client` when the `http-client`
//!   feature is enabled.
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use serde::{Serialize, Deserialize};
//! # use rigtest::{TestContext, testcase, Error};
//! # #[derive(Serialize, Deserialize)]
//! # struct State { db_url: String }
//! # struct Conn;
//! # impl Conn {
//! #     async fn insert(&mut self, _: &str) -> Result<(), Error> { Ok(()) }
//! #     async fn count(&self) -> Result<usize, Error> { Ok(1) }
//! #     async fn rollback(self) -> Result<(), Error> { Ok(()) }
//! # }
//! # async fn db_connect(_: &str) -> Result<Conn, Error> { Ok(Conn) }
//! #[testcase]
//! async fn creates_record(ctx: Arc<TestContext>) -> Result<(), rigtest::Error> {
//!     let mut conn = ctx.setup(|global| async move {
//!         let state = global.downcast_ref::<State>().unwrap();
//!         db_connect(&state.db_url).await
//!     }).await?;
//!
//!     conn.insert("hello").await?;
//!     assert_eq!(conn.count().await?, 1);
//!
//!     ctx.teardown(|_global| async move {
//!         conn.rollback().await?;
//!         Ok(())
//!     }).await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! # Skipping tests
//!
//! Use [`skip!`] to bail out of a test at runtime with an optional reason:
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use rigtest::{TestContext, testcase};
//! #[testcase]
//! async fn requires_db(ctx: Arc<TestContext>) -> Result<(), rigtest::Error> {
//!     if std::env::var("DATABASE_URL").is_err() {
//!         rigtest::skip!("DATABASE_URL not set");
//!     }
//!     // …
//!     Ok(())
//! }
//! ```
//!
//! Skipped tests appear as `SKIP` in the summary and do not count as failures.
//!
//! # Feature flags
//!
//! | Feature | Description |
//! |---------|-------------|
//! | `http-client` | Adds a shared `reqwest::Client` as `ctx.client`. Omit this feature if you prefer to construct your own HTTP client. |
//!
//! # Entry point
//!
//! Every test binary must call [`run_main`] from `fn main()`. This is the
//! hook that lets the `cargo rigtest` coordinator drive the binary as either
//! an orchestrator or a single-test subprocess depending on how it was
//! invoked.
//!
//! ```no_run
//! # #![allow(clippy::needless_doctest_main)]
//! fn main() {
//!     rigtest::run_main();
//! }
//! ```
//!
//! [`cargo-rigtest`]: https://crates.io/crates/cargo-rigtest

#[doc(hidden)]
pub extern crate linkme as __linkme;
#[doc(hidden)]
pub extern crate serde_json as __serde_json;

pub mod context;
pub mod registry;
pub mod reporter;
pub mod scheduler;

pub use context::TestContext;
pub use rigtest_macros::{global_setup, global_teardown, testcase};
pub use scheduler::RuntimeArgs;

/// Convenient glob import for test files.
///
/// ```no_run
/// use rigtest::prelude::*;
/// ```
///
/// Brings into scope: [`TestContext`] and the attribute macros [`testcase`],
/// [`global_setup`], and [`global_teardown`].
pub mod prelude {
    pub use crate::TestContext;
    pub use rigtest_macros::{global_setup, global_teardown, testcase};
}

/// Convenience alias for the error type used by test functions, setup, and
/// teardown closures. Equivalent to `Box<dyn std::error::Error + Send + Sync>`.
pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Marker error returned by the [`skip!`] macro to signal that a test should be
/// skipped rather than failed.
///
/// You will not typically construct this directly — use [`skip!`] instead.
/// The runtime inspects the error type of a failing test to distinguish a skip
/// from a genuine failure and records it as `SKIP` in the report.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use rigtest::{testcase, TestContext};
///
/// #[testcase]
/// async fn requires_linux(ctx: Arc<TestContext>) -> Result<(), rigtest::Error> {
///     if !cfg!(target_os = "linux") {
///         rigtest::skip!("this test only runs on Linux");
///     }
///     // Linux-specific assertions…
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub struct Skip(
    /// The human-readable reason displayed next to `SKIP` in the test report.
    /// Empty when the test is skipped without a message.
    pub String,
);

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Skip {}

/// Skip the current test with an optional reason.
///
/// Immediately returns a [`Skip`] error from the enclosing test function.
/// The runtime records the test as `SKIP` rather than `FAIL` and displays the
/// reason next to the test name in the report.
///
/// # Forms
///
/// - `skip!("reason")` — skip with a message (any value that implements [`ToString`]).
/// - `skip!()` — skip with no message.
///
/// # Examples
///
/// Skip when an environment variable is absent:
///
/// ```no_run
/// use std::sync::Arc;
/// use rigtest::{testcase, TestContext};
///
/// #[testcase]
/// async fn requires_db(ctx: Arc<TestContext>) -> Result<(), rigtest::Error> {
///     if std::env::var("DB_URL").is_err() {
///         rigtest::skip!("DB_URL not set");
///     }
///     // database assertions…
///     Ok(())
/// }
/// ```
///
/// Skip unconditionally (no message):
///
/// ```no_run
/// # use std::sync::Arc;
/// # use rigtest::{testcase, TestContext};
/// #[testcase]
/// async fn not_yet_implemented(_ctx: Arc<TestContext>) -> Result<(), rigtest::Error> {
///     rigtest::skip!();
/// }
/// ```
#[macro_export]
macro_rules! skip {
    ($reason:expr) => {
        return Err(Box::new($crate::Skip($reason.to_string())))
    };
    () => {
        return Err(Box::new($crate::Skip(String::new())))
    };
}

/// Flush stdout and stderr then exit. Using `std::process::exit` directly
/// skips Rust's normal teardown, leaving buffered output unwritten.
pub(crate) fn flush_and_exit(code: i32) -> ! {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}

/// Entry point for test binaries using cargo-rigtest.
/// Call this from `main()` in a `[[test]]` target with `harness = false`.
///
/// # Panics
///
/// Panics if the Tokio multi-thread runtime cannot be initialized.
pub fn run_main() -> ! {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let result = runtime.block_on(async {
        let args = <RuntimeArgs as clap::Parser>::parse();
        if args.rig_probe {
            flush_and_exit(0);
        }
        scheduler::run_suite(args).await
    });

    match result {
        Ok(()) => flush_and_exit(0),
        Err(e) => {
            eprintln!("error: {e}");
            flush_and_exit(1);
        }
    }
}
