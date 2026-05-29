#![warn(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::unused_async)]
#![allow(clippy::too_many_lines)]

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

/// Marker error returned by the `skip!` macro to signal that a test should be
/// skipped rather than failed.
#[derive(Debug)]
pub struct Skip(pub String);

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Skip {}

/// Skip the current test with an optional reason.
///
/// ```ignore
/// #[testcase]
/// async fn my_test(_ctx: Arc<TestContext>) -> Result<(), rigtest::Error> {
///     if std::env::var("DB_URL").is_err() {
///         rigtest::skip!("DB_URL not set");
///     }
///     Ok(())
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
