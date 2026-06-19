use clap::Parser;

/// Arguments forwarded from `cargo rig run` into the test binary.
///
/// Fields may be added in future releases. The `#[non_exhaustive]` attribute
/// prevents external code from constructing this struct via struct literal
/// syntax — use [`clap::Parser`] to parse arguments from a command line.
#[derive(Parser, Debug)]
#[command(about = "Run the cargo-rigtest acceptance test suite")]
#[non_exhaustive]
pub struct RuntimeArgs {
    /// Maximum number of parallel test jobs [default: number of CPUs].
    #[arg(short, long)]
    pub jobs: Option<usize>,

    /// Seed for randomized test order. Printed on every run so failures are
    /// reproducible.
    #[arg(long)]
    pub seed: Option<u64>,

    /// Only run tests whose name contains FILTER.
    #[arg(short, long)]
    pub filter: Option<String>,

    /// Only run tests tagged with one of TAGS. Repeat the flag and/or pass a
    /// comma-separated list — both forms union together. Combined with
    /// `--not-tag` and `--filter` using AND.
    #[arg(long = "tag", value_name = "TAGS", value_delimiter = ',', action = clap::ArgAction::Append)]
    pub tag: Vec<String>,

    /// Exclude tests tagged with any of TAGS. Repeat the flag and/or pass a
    /// comma-separated list — both forms union together.
    #[arg(long = "not-tag", value_name = "TAGS", value_delimiter = ',', action = clap::ArgAction::Append)]
    pub not_tag: Vec<String>,

    /// Show test output in real time rather than capturing it.
    #[arg(long)]
    pub no_capture: bool,

    /// Reporter(s) to use for run output. Pass `junit` to additionally emit a
    /// `JUnit` XML document at `target/rigtest/junit.xml` (or the path in
    /// `RIGTEST_JUNIT_OUTPUT_PATH` when set by the parent).
    #[arg(long, value_name = "REPORTER")]
    pub reporter: Option<String>,

    // ── Internal flags used in subprocess (single-test) mode ─────────────────
    // Hidden from `--help`; set by the coordinator when spawning per-test
    // subprocesses.
    /// Run exactly one named test case and exit. Used internally.
    #[arg(long, hide = true)]
    pub run_single: Option<String>,

    /// Name of the env var holding the serialized global state. Used internally.
    #[arg(long, hide = true)]
    pub state_env_var: Option<String>,

    /// Exit immediately with code 0. Used by cargo-rigtest to confirm this binary
    /// is a rig test runner before attempting to run it.
    #[arg(long, hide = true)]
    pub rig_probe: bool,

    /// Respond with an empty test list and exit 0. Satisfies the discovery
    /// protocol used by `cargo nextest` and similar tools.
    #[arg(long, hide = true)]
    pub list: bool,

    /// Accepted and ignored so that tools passing `--format terse` (nextest)
    /// do not cause a parse error.
    #[arg(long, hide = true)]
    pub format: Option<String>,
}

/// Dispatch to either the coordinator or subprocess path based on the parsed
/// arguments.
///
/// # Errors
///
/// Returns an error if any test fails or if the current executable path
/// cannot be determined.
pub async fn run_suite(args: RuntimeArgs) -> anyhow::Result<()> {
    if let Some(ref test_name) = args.run_single {
        return crate::runner::run_single(test_name, args.state_env_var.as_deref()).await;
    }
    crate::orchestrator::run(args).await
}
