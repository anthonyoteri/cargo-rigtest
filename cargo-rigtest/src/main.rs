#![warn(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]

use std::process::Command;
use std::process::Stdio;

use anyhow::anyhow;
use anyhow::Context;
use clap::Parser;
use clap::Subcommand;

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
}

fn main() -> anyhow::Result<()> {
    let Cargo {
        subcommand: CargoSubcommand::Rigtest(rig_args),
    } = Cargo::parse();

    match rig_args.command {
        RigCommand::Run(run_args) => run(&run_args),
    }
}

fn run(args: &RunArgs) -> anyhow::Result<()> {
    // Build the test binary (or binaries) without running them.
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

    // Filter to binaries that respond to --rig-probe, skipping any other
    // harness=false test runners that happen to be in the package.
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

    // If the user specified --test, narrow to those targets.
    let targets: Vec<_> = if args.test.is_empty() {
        rig_targets
    } else {
        // Validate that every requested name exists.
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

    let mut last_code = 0i32;

    for (name, executable) in &targets {
        println!("cargo-rigtest: running '{name}'");

        let mut test_cmd = Command::new(executable);

        if let Some(jobs) = args.jobs {
            test_cmd.args(["--jobs", &jobs.to_string()]);
        }
        if let Some(seed) = args.seed {
            test_cmd.args(["--seed", &seed.to_string()]);
        }
        if let Some(ref filter) = args.filter {
            test_cmd.args(["--filter", filter]);
        }
        if args.no_capture {
            test_cmd.arg("--no-capture");
        }

        let status = test_cmd
            .status()
            .with_context(|| format!("failed to execute test binary: {executable}"))?;

        let code = status.code().unwrap_or(1);
        if code != 0 {
            last_code = code;
        }
    }

    std::process::exit(last_code);
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

/// Search the JSON-lines output from `cargo test --no-run --message-format=json`
/// for all `compiler-artifact` entries whose target kind is "test".
/// Returns a list of `(name, executable_path)` pairs in the order they appear.
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
