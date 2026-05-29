use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Boxed error type returned from test functions.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Signature of a registered test function.
pub type TestFn = fn(Arc<crate::context::TestContext>) -> BoxFuture<'static, Result<(), BoxError>>;

/// A single test case, registered via `#[testcase]`.
pub struct TestCase {
    pub name: &'static str,
    pub module: &'static str,
    pub file: &'static str,
    /// When true, this test must not run concurrently with any other test.
    pub serial: bool,
    /// Kill the subprocess and fail the test if it exceeds this duration.
    pub timeout: Option<std::time::Duration>,
    /// Retry a failed test up to this many additional times before reporting failure.
    pub retries: u32,
    /// The test function, receiving a shared `TestContext`.
    pub test_fn: TestFn,
}

// linkme requires Sync
unsafe impl Sync for TestCase {}

/// A global setup function, registered via `#[global_setup]`.
pub struct GlobalSetupEntry {
    pub setup_fn: fn() -> BoxFuture<'static, Box<dyn std::any::Any + Send + Sync>>,
    /// Serialize the state value to a JSON string for subprocess handoff.
    pub serialize_fn: fn(&Box<dyn std::any::Any + Send + Sync>) -> String,
    /// Deserialize state from the JSON string produced by `serialize_fn`.
    pub deserialize_fn: fn(&str) -> Box<dyn std::any::Any + Send + Sync>,
}

unsafe impl Sync for GlobalSetupEntry {}

/// A global teardown function, registered via `#[global_teardown]`.
pub struct GlobalTeardownEntry {
    pub teardown_fn: fn(Box<dyn std::any::Any + Send + Sync>) -> BoxFuture<'static, ()>,
}

unsafe impl Sync for GlobalTeardownEntry {}

/// All test cases discovered at compile time.
#[linkme::distributed_slice]
pub static RIG_TEST_CASES: [TestCase];

/// At most one global setup function.
#[linkme::distributed_slice]
pub static RIG_GLOBAL_SETUP: [GlobalSetupEntry];

/// At most one global teardown function.
#[linkme::distributed_slice]
pub static RIG_GLOBAL_TEARDOWN: [GlobalTeardownEntry];
