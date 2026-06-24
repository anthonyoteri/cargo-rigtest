#![warn(clippy::pedantic)]

mod junit;

use std::process::Command;
use std::process::Stdio;

use anyhow::anyhow;
use anyhow::Context;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ReporterKind {
    Junit,
}

#[derive(Parser)]
#[command(name = "cargo", bin_name = "cargo")]
struct Cargo {
    #[command(subcommand)]
    subcommand: CargoSubcommand,
}

#[derive(Subcommand)]
enum CargoSubcommand {
    /// Rust infrastructure and acceptance test runner
    Rigtest(RigArgs),
}

#[derive(Parser, Debug)]
struct RigArgs {
    #[command(subcommand)]
    command: RigCommand,
}

#[derive(Subcommand, Debug)]
enum RigCommand {
    /// Run the acceptance test suite
    Run(RunArgs),
}

#[derive(Parser, Debug)]
#[allow(clippy::struct_excessive_bools)] // CLI flags; not state to model.
struct RunArgs {
    /// Maximum parallel test jobs [default: number of CPUs]
    #[arg(short, long)]
    jobs: Option<usize>,

    /// Seed for randomized test order
    #[arg(long)]
    seed: Option<u64>,

    /// Only run tests whose name contains FILTER
    #[arg(short, long)]
    filter: Option<String>,

    /// Only run tests tagged with one of TAGS. Repeat the flag and/or pass a
    /// comma-separated list — both forms union together. Combined with
    /// `--not-tag` and `--filter` using AND.
    #[arg(long = "tag", value_name = "TAGS", value_delimiter = ',', action = clap::ArgAction::Append)]
    tag: Vec<String>,

    /// Exclude tests tagged with any of TAGS. Repeat the flag and/or pass a
    /// comma-separated list — both forms union together.
    #[arg(long = "not-tag", value_name = "TAGS", value_delimiter = ',', action = clap::ArgAction::Append)]
    not_tag: Vec<String>,

    /// Package containing the test targets
    #[arg(short, long)]
    package: Option<String>,

    /// Only run these test targets. May be specified multiple times.
    /// By default all rig test targets are run.
    #[arg(long, num_args = 1..)]
    test: Vec<String>,

    /// Show test output in real time rather than capturing it.
    #[arg(long)]
    no_capture: bool,

    /// Reporter to use for run output. Pass `junit` to emit
    /// `target/rigtest/junit.xml` alongside the live console output.
    #[arg(long, value_enum, value_name = "REPORTER")]
    reporter: Option<ReporterKind>,

    /// Override every test's declared retry count for this run. Set to 0
    /// to disable retries entirely (strict mode). Leaves any declared
    /// `retry_on_error` matcher in force: only failures the matcher
    /// accepts consume an attempt.
    #[arg(long, value_name = "N")]
    retries: Option<usize>,

    /// Skip the preflight phase entirely. Use sparingly — preflight exists
    /// to catch missing environment dependencies *before* tests run.
    #[arg(long)]
    no_preflight: bool,

    /// Run the preflight phase, print the readiness table, and exit
    /// without running `#[global_setup]` or any tests. Exits 0 when every
    /// declared probe passes, 2 when any probe fails. When no
    /// `#[preflight]` is declared the binary prints `no preflight
    /// declared` and exits 0.
    #[arg(long)]
    preflight_only: bool,

    /// Treat preflight failures as warnings rather than aborting the
    /// suite. The readiness table and `JUnit` preflight testsuite still
    /// show the failures; the final exit code reflects only the test
    /// phase. Combine with `--reporter junit` to publish probe results
    /// to CI dashboards regardless of suite outcome.
    #[arg(long)]
    continue_on_preflight_failure: bool,
}

fn main() -> anyhow::Result<()> {
    let Cargo {
        subcommand: CargoSubcommand::Rigtest(rig_args),
    } = Cargo::parse();

    match rig_args.command {
        RigCommand::Run(run_args) => run(&run_args),
    }
}

#[allow(clippy::too_many_lines)]
fn run(args: &RunArgs) -> anyhow::Result<()> {
    let mut build_cmd = Command::new("cargo");
    build_cmd
        .arg("test")
        .arg("--no-run")
        .arg("--message-format=json");

    if let Some(ref pkg) = args.package {
        build_cmd.args(["--package", pkg]);
    }

    for name in &args.test {
        build_cmd.args(["--test", name]);
    }

    build_cmd.stdout(Stdio::piped());
    build_cmd.stderr(Stdio::inherit());

    let output = build_cmd
        .output()
        .context("failed to spawn `cargo test --no-run`")?;

    if !output.status.success() {
        return Err(anyhow!(
            "cargo test --no-run failed with exit code {}",
            output
                .status
                .code()
                .map_or_else(|| "unknown".to_string(), |c| c.to_string()),
        ));
    }

    let stdout = String::from_utf8(output.stdout).context("cargo output was not valid UTF-8")?;

    let all_targets = find_all_test_executables(&stdout);

    let rig_targets: Vec<_> = all_targets
        .into_iter()
        .filter(|(_, exe)| is_rig_binary(exe))
        .collect();

    if rig_targets.is_empty() {
        return Err(anyhow!(
            "no rigtest test targets found. \
             Make sure at least one [[test]] target in Cargo.toml has \
             harness = false and calls rigtest::run_main()."
        ));
    }

    let targets: Vec<_> = if args.test.is_empty() {
        rig_targets
    } else {
        let unknown: Vec<_> = args
            .test
            .iter()
            .filter(|name| !rig_targets.iter().any(|(n, _)| n == *name))
            .collect();
        if !unknown.is_empty() {
            return Err(anyhow!(
                "unknown rig test target(s): {}. \
                 Run without --test to run all rig targets.",
                unknown
                    .iter()
                    .map(|s| format!("'{s}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        rig_targets
            .into_iter()
            .filter(|(n, _)| args.test.contains(n))
            .collect()
    };

    let junit = match args.reporter {
        Some(ReporterKind::Junit) => Some(prepare_junit_paths()?),
        None => None,
    };

    let mut last_code = 0i32;

    for (name, executable) in &targets {
        println!("cargo-rigtest: running '{name}'");

        let mut test_cmd = Command::new(executable);
        test_cmd.env("CARGO_RIGTEST", "1");

        if let Some(jobs) = args.jobs {
            test_cmd.args(["--jobs", &jobs.to_string()]);
        }
        if let Some(seed) = args.seed {
            test_cmd.args(["--seed", &seed.to_string()]);
        }
        if let Some(ref filter) = args.filter {
            test_cmd.args(["--filter", filter]);
        }
        for tag in &args.tag {
            test_cmd.args(["--tag", tag]);
        }
        for tag in &args.not_tag {
            test_cmd.args(["--not-tag", tag]);
        }
        if args.no_capture {
            test_cmd.arg("--no-capture");
        }
        if let Some(retries) = args.retries {
            test_cmd.args(["--retries", &retries.to_string()]);
        }
        if args.no_preflight {
            test_cmd.arg("--no-preflight");
        }
        if args.preflight_only {
            test_cmd.arg("--preflight-only");
        }
        if args.continue_on_preflight_failure {
            test_cmd.arg("--continue-on-preflight-failure");
        }
        if let Some(paths) = &junit {
            test_cmd.args(["--reporter", "junit"]);
            test_cmd.env("RIGTEST_JUNIT_OUTPUT_PATH", paths.part_for(executable));
            test_cmd.env("RIGTEST_JUNIT_SUITE_NAME", name);
        }

        let status = test_cmd
            .status()
            .with_context(|| format!("failed to execute test binary: {executable}"))?;

        let code = status.code().unwrap_or(1);
        if code != 0 {
            last_code = code;
        }
    }

    if let Some(paths) = &junit {
        let expected: Vec<(&str, std::path::PathBuf)> = targets
            .iter()
            .map(|(n, exe)| (n.as_str(), paths.part_for(exe)))
            .collect();
        let report = junit::aggregate(&expected);
        write_aggregate(&paths.final_path, &report)?;
        println!(
            "cargo-rigtest: JUnit XML written to {}",
            paths.final_path.display()
        );
    }

    std::process::exit(last_code);
}

struct JunitPaths {
    parts_dir: std::path::PathBuf,
    final_path: std::path::PathBuf,
}

impl JunitPaths {
    /// Part file path keyed by the executable's filename stem — which
    /// cargo includes a unique hash in — so two workspace crates with the
    /// same `[[test]]` target name do not collide on the same part file.
    fn part_for(&self, executable: &str) -> std::path::PathBuf {
        let stem = std::path::Path::new(executable)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("part");
        self.parts_dir.join(format!("{stem}.xml"))
    }
}

/// Resolve the cargo target directory (honoring `CARGO_TARGET_DIR`),
/// clean any pre-existing `rigtest/parts/` so stale results never leak into
/// the aggregate, and return the well-known paths used during the run.
fn prepare_junit_paths() -> anyhow::Result<JunitPaths> {
    let target = cargo_target_dir().context("failed to resolve cargo target directory")?;
    let parts_dir = target.join("rigtest").join("parts");
    let final_path = target.join("rigtest").join("junit.xml");

    if parts_dir.exists() {
        std::fs::remove_dir_all(&parts_dir)
            .with_context(|| format!("failed to clean {}", parts_dir.display()))?;
    }
    std::fs::create_dir_all(&parts_dir)
        .with_context(|| format!("failed to create {}", parts_dir.display()))?;

    Ok(JunitPaths {
        parts_dir,
        final_path,
    })
}

fn cargo_target_dir() -> anyhow::Result<std::path::PathBuf> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version=1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .context("failed to spawn `cargo metadata`")?;
    if !output.status.success() {
        return Err(anyhow!("cargo metadata failed"));
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("cargo metadata produced invalid JSON")?;
    let dir = value
        .get("target_directory")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("cargo metadata missing 'target_directory'"))?;
    Ok(std::path::PathBuf::from(dir))
}

fn write_aggregate(path: &std::path::Path, report: &quick_junit::Report) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let xml = report
        .to_string()
        .context("failed to serialize aggregated JUnit XML")?;
    // Atomic write: a crash during serialize/write leaves the previous
    // aggregate in place rather than a half-written file pretending to be
    // current.
    let tmp = path.with_extension("xml.tmp");
    std::fs::write(&tmp, xml).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename into {}", path.display()))?;
    Ok(())
}

/// Returns true if `exe` is a rig test binary (responds to --rig-probe with exit 0).
fn is_rig_binary(exe: &str) -> bool {
    Command::new(exe)
        .arg("--rig-probe")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Parse `cargo test --no-run --message-format=json` output and return all
/// test-target executables.
///
/// `json_output` is the raw stdout from the `cargo test --no-run
/// --message-format=json` invocation — a newline-delimited sequence of JSON
/// objects. Each object is inspected for `"reason": "compiler-artifact"` with
/// a `"target"` whose `"kind"` array contains `"test"`.
///
/// Returns a `Vec` of `(name, executable_path)` pairs in the order they appear
/// in the input. Malformed lines and non-test artifacts are silently ignored.
#[must_use]
pub fn find_all_test_executables(json_output: &str) -> Vec<(String, String)> {
    let mut results = Vec::new();

    for line in json_output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if value.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }

        let Some(target) = value.get("target") else {
            continue;
        };

        let kind_is_test = target
            .get("kind")
            .and_then(|k| k.as_array())
            .is_some_and(|arr| arr.iter().any(|k| k.as_str() == Some("test")));

        if !kind_is_test {
            continue;
        }

        let name = match target.get("name").and_then(|n| n.as_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        if let Some(exe) = value.get("executable").and_then(|e| e.as_str()) {
            results.push((name, exe.to_string()));
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_artifact(name: &str, kind: &str, executable: &str) -> String {
        serde_json::json!({
            "reason": "compiler-artifact",
            "target": { "name": name, "kind": [kind] },
            "executable": executable
        })
        .to_string()
    }

    fn sample_non_artifact() -> String {
        serde_json::json!({ "reason": "build-script-executed" }).to_string()
    }

    #[test]
    fn finds_single_test_artifact() {
        let json = [
            sample_non_artifact(),
            sample_artifact("acceptance", "test", "/tmp/acceptance-abc"),
        ]
        .join("\n");
        assert_eq!(
            find_all_test_executables(&json),
            vec![("acceptance".to_string(), "/tmp/acceptance-abc".to_string())],
        );
    }

    #[test]
    fn finds_multiple_test_artifacts() {
        let json = [
            sample_artifact("suite_a", "test", "/tmp/suite_a"),
            sample_artifact("suite_b", "test", "/tmp/suite_b"),
        ]
        .join("\n");
        assert_eq!(
            find_all_test_executables(&json),
            vec![
                ("suite_a".to_string(), "/tmp/suite_a".to_string()),
                ("suite_b".to_string(), "/tmp/suite_b".to_string()),
            ],
        );
    }

    #[test]
    fn ignores_non_test_artifacts() {
        let json = sample_artifact("acceptance", "bin", "/tmp/acceptance-abc");
        assert!(find_all_test_executables(&json).is_empty());
    }

    #[test]
    fn returns_empty_for_empty_input() {
        assert!(find_all_test_executables("").is_empty());
    }

    #[test]
    fn handles_garbage_lines_gracefully() {
        let json = "not json at all\n{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"acceptance\",\"kind\":[\"test\"]},\"executable\":\"/tmp/x\"}";
        assert_eq!(
            find_all_test_executables(json),
            vec![("acceptance".to_string(), "/tmp/x".to_string())],
        );
    }
}
