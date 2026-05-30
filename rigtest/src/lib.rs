#![warn(clippy::pedantic)]

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
/// ```ignore
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
/// ```ignore
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
/// ```ignore
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
/// ```ignore
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
