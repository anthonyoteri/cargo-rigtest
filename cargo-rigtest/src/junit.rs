//! Aggregate per-binary `JUnit` XML parts into a single document.
//!
//! Each rig test binary writes its complete `<testsuites>` document to a part
//! file inside `target/rigtest/parts/`. After all children have run, the
//! parent reads every part, merges the inner `<testsuite>` elements into a
//! single document at `target/rigtest/junit.xml`, and synthesizes an error
//! `<testsuite>` for any expected binary that did not produce results.
//!
//! [`JunitCoordinator`] owns the lifecycle: prepare the `parts/` directory
//! before the run, attach `--reporter junit` + the per-target env vars to
//! each child `Command`, and aggregate + atomically write the final XML
//! once every binary has exited.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context};
use quick_junit::{NonSuccessKind, Report, TestCase, TestCaseStatus, TestSuite};

use crate::ReporterKind;

/// Owns the optional `JUnit` lifecycle for one `cargo rig run` invocation.
///
/// Construct via [`Self::for_run`]; returns `None` when `--reporter junit`
/// is absent so the caller can pattern-match without sprinkling
/// `if args.reporter == Some(_)` checks across the run path.
pub(crate) struct JunitCoordinator {
    parts_dir: PathBuf,
    final_path: PathBuf,
}

impl JunitCoordinator {
    /// `Some(coordinator)` when the operator requested the `junit` reporter;
    /// `None` otherwise. Cleaning the parts directory happens here so a
    /// crashed previous run can't leak stale suites into the next aggregate.
    pub(crate) fn for_run(reporter: Option<ReporterKind>) -> anyhow::Result<Option<Self>> {
        match reporter {
            Some(ReporterKind::Junit) => Ok(Some(Self::prepare()?)),
            None => Ok(None),
        }
    }

    fn prepare() -> anyhow::Result<Self> {
        let target = cargo_target_dir().context("failed to resolve cargo target directory")?;
        let parts_dir = target.join("rigtest").join("parts");
        let final_path = target.join("rigtest").join("junit.xml");

        if parts_dir.exists() {
            std::fs::remove_dir_all(&parts_dir)
                .with_context(|| format!("failed to clean {}", parts_dir.display()))?;
        }
        std::fs::create_dir_all(&parts_dir)
            .with_context(|| format!("failed to create {}", parts_dir.display()))?;

        Ok(Self {
            parts_dir,
            final_path,
        })
    }

    /// Attach `--reporter junit` plus the per-target env vars (output path
    /// and suite name) to a test-binary `Command` about to be spawned.
    pub(crate) fn attach_to(&self, cmd: &mut Command, name: &str, executable: &str) {
        cmd.args(["--reporter", "junit"]);
        cmd.env("RIGTEST_JUNIT_OUTPUT_PATH", self.part_for(executable));
        cmd.env("RIGTEST_JUNIT_SUITE_NAME", name);
    }

    /// Collect every expected part file, aggregate them into one `Report`,
    /// and atomically write the result to `<target>/rigtest/junit.xml`.
    /// Logs the final path on stdout.
    pub(crate) fn finalize(&self, targets: &[(String, String)]) -> anyhow::Result<()> {
        let expected: Vec<(&str, PathBuf)> = targets
            .iter()
            .map(|(n, exe)| (n.as_str(), self.part_for(exe)))
            .collect();
        let report = aggregate(&expected);
        write_aggregate(&self.final_path, &report)?;
        println!(
            "cargo-rigtest: JUnit XML written to {}",
            self.final_path.display()
        );
        Ok(())
    }

    /// Part file path keyed by the executable's filename stem — which cargo
    /// includes a unique hash in — so two workspace crates with the same
    /// `[[test]]` target name do not collide on the same part file.
    fn part_for(&self, executable: &str) -> PathBuf {
        let stem = Path::new(executable)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("part");
        self.parts_dir.join(format!("{stem}.xml"))
    }
}

fn cargo_target_dir() -> anyhow::Result<PathBuf> {
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
    Ok(PathBuf::from(dir))
}

fn write_aggregate(path: &Path, report: &Report) -> anyhow::Result<()> {
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

/// Aggregate per-binary part files into a single `Report`.
///
/// `expected` is the full list of `(target_name, part_path)` pairs that were
/// supposed to run. The target name is what the produced `<testsuite name>`
/// will carry — the part file's inner suite name is rewritten on ingest so
/// the aggregate's grouping always reflects the parent's view of the world.
///
/// Any expected binary without a corresponding part file gets a synthetic
/// suite containing a single error `<testcase>` so the consumer can see that
/// the binary failed to publish results. Part files whose XML does not parse
/// are reported on stderr and treated the same as missing.
pub fn aggregate(expected: &[(&str, std::path::PathBuf)]) -> Report {
    let mut report = Report::new("cargo-rigtest");

    for (target_name, path) in expected {
        if !path.exists() {
            report.add_test_suite(synthetic_missing_suite(target_name));
            continue;
        }
        let xml = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "cargo-rigtest: failed to read JUnit part {}: {e}",
                    path.display()
                );
                report.add_test_suite(synthetic_missing_suite(target_name));
                continue;
            }
        };
        let parsed = match Report::deserialize_from_str(&xml) {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "cargo-rigtest: failed to parse JUnit part {}: {e}",
                    path.display()
                );
                report.add_test_suite(synthetic_missing_suite(target_name));
                continue;
            }
        };
        let mut found = false;
        for mut suite in parsed.test_suites {
            // Defensive: ensure the aggregate's <testsuite name> always
            // matches the parent's view of the target name, regardless of
            // what the child wrote — except for the synthetic preflight
            // testsuite, which is identified by its literal name and must
            // remain `preflight` in the aggregate (CI dashboards key off
            // that name to surface readiness checks separately).
            if suite.name.as_str() != "preflight" {
                suite.name = (*target_name).into();
            }
            report.add_test_suite(suite);
            found = true;
        }
        if !found {
            report.add_test_suite(synthetic_missing_suite(target_name));
        }
    }

    report
}

fn synthetic_missing_suite(binary: &str) -> TestSuite {
    let mut suite = TestSuite::new(binary);
    let mut status = TestCaseStatus::non_success(NonSuccessKind::Error);
    status.set_message("test binary did not produce results");
    status.set_type("missing");
    let mut case = TestCase::new("did_not_run", status);
    case.set_classname(binary);
    case.set_time(std::time::Duration::ZERO);
    suite.add_test_case(case);
    suite
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn write_part(dir: &Path, file_stem: &str, suite_name: &str, case_name: &str) {
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="{suite_name}">
  <testsuite name="{suite_name}" tests="1">
    <testcase classname="acceptance" name="{case_name}" time="0.001"/>
  </testsuite>
</testsuites>
"#
        );
        fs::write(dir.join(format!("{file_stem}.xml")), xml).unwrap();
    }

    #[test]
    fn merges_present_parts_into_single_report() {
        let tmp = TempDir::new().unwrap();
        write_part(tmp.path(), "alpha-abc", "alpha", "one");
        write_part(tmp.path(), "beta-def", "beta", "two");
        let expected = vec![
            ("alpha", tmp.path().join("alpha-abc.xml")),
            ("beta", tmp.path().join("beta-def.xml")),
        ];

        let report = aggregate(&expected);
        assert_eq!(report.test_suites.len(), 2);
        let names: Vec<&str> = report.test_suites.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    #[test]
    fn two_targets_sharing_name_each_get_own_suite() {
        let tmp = TempDir::new().unwrap();
        write_part(tmp.path(), "acceptance-abc", "acceptance", "one");
        write_part(tmp.path(), "acceptance-def", "different_name", "two");
        let expected = vec![
            ("acceptance", tmp.path().join("acceptance-abc.xml")),
            ("acceptance", tmp.path().join("acceptance-def.xml")),
        ];

        let report = aggregate(&expected);
        assert_eq!(report.test_suites.len(), 2);
        // Both renamed to "acceptance" but distinct content.
        assert!(report
            .test_suites
            .iter()
            .all(|s| s.name.as_str() == "acceptance"));
        let case_names: Vec<&str> = report
            .test_suites
            .iter()
            .flat_map(|s| s.test_cases.iter().map(|c| c.name.as_str()))
            .collect();
        assert!(case_names.contains(&"one"));
        assert!(case_names.contains(&"two"));
    }

    #[test]
    fn missing_part_gets_synthetic_error_suite() {
        let tmp = TempDir::new().unwrap();
        write_part(tmp.path(), "alpha", "alpha", "one");
        let expected = vec![
            ("alpha", tmp.path().join("alpha.xml")),
            ("beta", tmp.path().join("beta.xml")),
        ];

        let report = aggregate(&expected);
        assert_eq!(report.test_suites.len(), 2);
        let beta = report
            .test_suites
            .iter()
            .find(|s| s.name.as_str() == "beta")
            .expect("synthetic beta suite present");
        assert_eq!(beta.test_cases.len(), 1);
        let case = &beta.test_cases[0];
        assert!(matches!(
            case.status,
            TestCaseStatus::NonSuccess {
                kind: NonSuccessKind::Error,
                ..
            }
        ));
        assert_eq!(case.time, Some(std::time::Duration::ZERO));
    }

    #[test]
    fn malformed_part_falls_back_to_synthetic_suite() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("broken.xml"), "<not valid xml").unwrap();
        let expected = vec![("broken", tmp.path().join("broken.xml"))];

        let report = aggregate(&expected);
        assert_eq!(report.test_suites.len(), 1);
        let suite = &report.test_suites[0];
        assert_eq!(suite.name.as_str(), "broken");
        assert!(matches!(
            suite.test_cases[0].status,
            TestCaseStatus::NonSuccess {
                kind: NonSuccessKind::Error,
                ..
            }
        ));
    }

    #[test]
    fn inner_suite_name_is_rewritten_to_expected() {
        let tmp = TempDir::new().unwrap();
        write_part(tmp.path(), "alpha-abc", "completely_different", "one");
        let expected = vec![("alpha", tmp.path().join("alpha-abc.xml"))];

        let report = aggregate(&expected);
        assert_eq!(report.test_suites.len(), 1);
        assert_eq!(report.test_suites[0].name.as_str(), "alpha");
    }

    #[test]
    fn empty_expected_list_produces_empty_report() {
        let report = aggregate(&[]);
        assert!(report.test_suites.is_empty());
    }

    #[test]
    fn synthetic_preflight_suite_keeps_its_name() {
        // A test binary that runs preflight emits *two* suites in one
        // part file — `preflight` and the regular test suite. The
        // aggregator must leave `preflight` untouched so CI dashboards
        // can key off the literal name; renaming both to the target
        // would erase the distinction between probe and test results.
        let tmp = TempDir::new().unwrap();
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="acceptance">
  <testsuite name="preflight" tests="1">
    <testcase classname="preflight" name="api" time="0.001"/>
  </testsuite>
  <testsuite name="acceptance" tests="1">
    <testcase classname="acceptance" name="some_test" time="0.002"/>
  </testsuite>
</testsuites>
"#;
        fs::write(tmp.path().join("acceptance.xml"), xml).unwrap();
        let expected = vec![("acceptance", tmp.path().join("acceptance.xml"))];

        let report = aggregate(&expected);
        let names: Vec<&str> = report.test_suites.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["preflight", "acceptance"]);
    }
}
