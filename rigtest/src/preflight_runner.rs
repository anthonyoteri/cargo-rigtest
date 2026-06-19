//! Coordinator-side execution and rendering for the preflight phase.
//!
//! Runs all declared probes concurrently. In a TTY, each probe owns a live
//! [`indicatif::ProgressBar`] spinner so the operator sees activity while
//! probes are in flight. Spinners are added to the [`MultiProgress`] in
//! declaration order and finalized as their probes complete; the final
//! per-probe `PASS`/`FAIL` lines therefore appear in the order each probe
//! *finished*, while the persistent readiness table printed on failure is
//! always in declaration order. In a non-TTY environment (CI, piped
//! output) the spinners are hidden and only the lines are emitted.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use console::style;
use futures::future::join_all;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::preflight::{Probe, ProbeKind};
use crate::registry::{PreflightEntry, RIG_PREFLIGHT};

/// Outcome of a single probe.
#[derive(Debug)]
pub(crate) enum ProbeOutcome {
    Passed,
    Failed(String),
}

#[derive(Debug)]
pub(crate) struct ProbeResult {
    pub probe: Probe,
    pub outcome: ProbeOutcome,
    pub elapsed: Duration,
}

/// Run the preflight phase, if one is declared. Returns `Ok(())` when no
/// preflight is declared, when every probe passes, or when the user passed
/// `--no-preflight`. Returns `Err` with a formatted abort message when one
/// or more probes fail (so the caller can exit with status 2) or when the
/// suite's preflight is structurally invalid (duplicate names, more than
/// one `#[preflight]` per binary).
///
/// `tests_total` is the number of tests that would otherwise have run; the
/// abort message reports it so operators see the cost of the failed
/// preflight at a glance.
pub(crate) async fn run_preflight(tests_total: usize) -> anyhow::Result<()> {
    if RIG_PREFLIGHT.is_empty() {
        return Ok(());
    }
    assert!(
        RIG_PREFLIGHT.len() <= 1,
        "cargo-rigtest: at most one #[preflight] function may be defined, found {}",
        RIG_PREFLIGHT.len()
    );

    let entry: &PreflightEntry = &RIG_PREFLIGHT[0];
    let preflight = (entry.build_fn)();
    let probes = preflight.into_probes();

    let is_tty = console::Term::stderr().is_term();
    let multi = MultiProgress::with_draw_target(if is_tty {
        ProgressDrawTarget::stderr()
    } else {
        ProgressDrawTarget::hidden()
    });

    // Phase header. Print via the MultiProgress so it stays above the
    // spinner area in a TTY rather than interleaving with redraws.
    println_via(
        &multi,
        is_tty,
        &format!("{} {}", style("──").dim(), style("preflight").dim().bold()),
    );

    if probes.is_empty() {
        // A `#[preflight]` that declares no probes is legal — render the
        // result line so the operator can see the phase ran.
        println_via(&multi, is_tty, "preflight result: 0 passed");
        return Ok(());
    }

    // Tier-1 disambiguation: error if any name collides. Done before any
    // probe runs so an operator's mistake never wastes a second of probe
    // budget.
    if let Some(duplicate) = first_duplicate_name(&probes) {
        return Err(anyhow!(
            "cargo-rigtest: duplicate probe name {duplicate:?} in #[preflight] \
             (tier-1 auto-disambiguation requires every name be unique; \
             rename one of the colliding probes)"
        ));
    }

    // Pre-create one spinner per probe in declaration order so the TTY
    // layout is deterministic regardless of which probe finishes first.
    let spinner_style =
        ProgressStyle::with_template("{spinner:.cyan} {msg}").expect("valid spinner template");
    let spinners: Vec<ProgressBar> = probes
        .iter()
        .map(|p| {
            let pb = multi.add(ProgressBar::new_spinner());
            pb.set_style(spinner_style.clone());
            pb.set_message(format!("{} {}", style("RUNNING").cyan().bold(), p.name));
            if is_tty {
                pb.enable_steady_tick(Duration::from_millis(80));
            }
            pb
        })
        .collect();

    let multi = Arc::new(multi);

    // Fire every probe concurrently. Each task finalizes its own spinner
    // (lines therefore appear in completion order) so a slow probe never
    // blocks fast ones from being marked done in the live output.
    let suite_start = Instant::now();
    let futures = probes.into_iter().zip(spinners).map(|(p, pb)| {
        let multi = Arc::clone(&multi);
        async move {
            let start = Instant::now();
            let outcome = run_probe(&p).await;
            let elapsed = start.elapsed();
            let line = render_probe_line(p.name, &outcome, elapsed);
            finalize_spinner(&multi, &pb, is_tty, &line);
            ProbeResult {
                probe: p,
                outcome,
                elapsed,
            }
        }
    });
    let results: Vec<ProbeResult> = join_all(futures).await;
    let total_elapsed = suite_start.elapsed();

    let passed = results
        .iter()
        .filter(|r| matches!(r.outcome, ProbeOutcome::Passed))
        .count();
    let failed: Vec<&ProbeResult> = results
        .iter()
        .filter(|r| matches!(r.outcome, ProbeOutcome::Failed(_)))
        .collect();

    if failed.is_empty() {
        println_via(
            &multi,
            is_tty,
            &format!(
                "preflight result: {passed} passed [{:.2}s]",
                total_elapsed.as_secs_f64()
            ),
        );
        return Ok(());
    }

    render_readiness_table(&multi, is_tty, &results);
    let failed = failed.len();
    Err(anyhow!(
        "{failed} probe{plural} failed — aborting suite ({tests_total} tests not run)",
        plural = if failed == 1 { "" } else { "s" },
    ))
}

/// Print the readiness table (every probe, status, timing, error) in
/// declaration order on the failure path so the operator can see the
/// whole picture in one place even when many probes failed at once.
fn render_readiness_table(multi: &MultiProgress, is_tty: bool, results: &[ProbeResult]) {
    let separator = style("─".repeat(60)).dim().to_string();
    println_via(multi, is_tty, &separator);
    println_via(multi, is_tty, "preflight readiness:");
    for r in results {
        let (status, detail) = match &r.outcome {
            ProbeOutcome::Passed => (style("pass").green().bold().to_string(), String::new()),
            ProbeOutcome::Failed(reason) => (
                style("fail").red().bold().to_string(),
                format!(": {reason}"),
            ),
        };
        println_via(
            multi,
            is_tty,
            &format!(
                "  {status} [{:.3}s] {}{detail}",
                r.elapsed.as_secs_f64(),
                r.probe.name,
            ),
        );
    }
    println_via(multi, is_tty, &separator);
}

fn render_probe_line(name: &str, outcome: &ProbeOutcome, elapsed: Duration) -> String {
    match outcome {
        ProbeOutcome::Passed => format!(
            "{} [{:.3}s] {name}",
            style("PASS").green().bold(),
            elapsed.as_secs_f64(),
        ),
        ProbeOutcome::Failed(reason) => format!(
            "{} [{:.3}s] {name}: {reason}",
            style("FAIL").red().bold(),
            elapsed.as_secs_f64(),
        ),
    }
}

fn finalize_spinner(multi: &MultiProgress, pb: &ProgressBar, is_tty: bool, line: &str) {
    if is_tty {
        // Order matters: emit the persistent line first so the spinner
        // teardown does not race with MultiProgress's draw thread.
        multi.println(line).ok();
        pb.finish_and_clear();
    } else {
        eprintln!("{line}");
    }
}

fn println_via(multi: &MultiProgress, is_tty: bool, line: &str) {
    if is_tty {
        multi.println(line).ok();
    } else {
        eprintln!("{line}");
    }
}

fn first_duplicate_name(probes: &[Probe]) -> Option<&'static str> {
    let mut seen: HashSet<&'static str> = HashSet::with_capacity(probes.len());
    for p in probes {
        if !seen.insert(p.name) {
            return Some(p.name);
        }
    }
    None
}

async fn run_probe(p: &Probe) -> ProbeOutcome {
    match &p.kind {
        ProbeKind::Tcp { target } => run_tcp(target, p.timeout).await,
        ProbeKind::Env { var, expected } => run_env(var, *expected),
    }
}

async fn run_tcp(target: &str, deadline: Duration) -> ProbeOutcome {
    match timeout(deadline, TcpStream::connect(target)).await {
        Ok(Ok(_stream)) => ProbeOutcome::Passed,
        Ok(Err(e)) => ProbeOutcome::Failed(format!("connect to {target} failed: {e}")),
        Err(_) => ProbeOutcome::Failed(format!(
            "connect to {target} timed out after {:.3}s",
            deadline.as_secs_f64()
        )),
    }
}

fn run_env(var: &str, expected: Option<&str>) -> ProbeOutcome {
    match std::env::var(var) {
        Ok(actual) => match expected {
            Some(want) if actual == want => ProbeOutcome::Passed,
            Some(want) => {
                ProbeOutcome::Failed(format!("{var}={actual:?} does not equal expected {want:?}"))
            }
            None if actual.is_empty() => ProbeOutcome::Failed(format!("{var} is set but empty")),
            None => ProbeOutcome::Passed,
        },
        Err(std::env::VarError::NotPresent) => ProbeOutcome::Failed(format!("{var} is not set")),
        Err(std::env::VarError::NotUnicode(_)) => {
            ProbeOutcome::Failed(format!("{var} is set but is not valid UTF-8"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preflight::Preflight;

    #[test]
    fn duplicate_detection_finds_first_collision() {
        let p = Preflight::new().tcp("api", "1.2.3.4:1").env("api", "X");
        assert_eq!(first_duplicate_name(&p.into_probes()), Some("api"));
    }

    #[test]
    fn duplicate_detection_returns_none_for_unique() {
        let p = Preflight::new().tcp("a", "1.2.3.4:1").env("b", "X");
        assert!(first_duplicate_name(&p.into_probes()).is_none());
    }

    #[tokio::test]
    async fn env_probe_passes_when_var_set_and_nonempty() {
        // SAFETY: tests do not run in parallel against the same env var
        // (we use a uniquely-named var per test). Setting env in tests is
        // a known footgun across threads, but tokio::test serializes the
        // task on its own runtime and this var is not read elsewhere.
        unsafe { std::env::set_var("RIGTEST_PREFLIGHT_TEST_ENV_PASS", "yes") };
        let outcome = run_env("RIGTEST_PREFLIGHT_TEST_ENV_PASS", None);
        assert!(matches!(outcome, ProbeOutcome::Passed));
    }

    #[tokio::test]
    async fn env_probe_fails_when_var_unset() {
        unsafe { std::env::remove_var("RIGTEST_PREFLIGHT_TEST_ENV_UNSET") };
        let outcome = run_env("RIGTEST_PREFLIGHT_TEST_ENV_UNSET", None);
        assert!(matches!(outcome, ProbeOutcome::Failed(_)));
    }

    #[tokio::test]
    async fn env_probe_equals_matches_exact_value() {
        unsafe { std::env::set_var("RIGTEST_PREFLIGHT_TEST_ENV_EQ", "prod") };
        assert!(matches!(
            run_env("RIGTEST_PREFLIGHT_TEST_ENV_EQ", Some("prod")),
            ProbeOutcome::Passed
        ));
        assert!(matches!(
            run_env("RIGTEST_PREFLIGHT_TEST_ENV_EQ", Some("staging")),
            ProbeOutcome::Failed(_)
        ));
    }

    #[tokio::test]
    async fn tcp_probe_fails_against_closed_port() {
        // Port 1 is privileged on every platform we target; we expect a
        // connect failure within a few milliseconds.
        let outcome = run_tcp("127.0.0.1:1", Duration::from_millis(250)).await;
        assert!(matches!(outcome, ProbeOutcome::Failed(_)));
    }

    #[tokio::test]
    async fn tcp_probe_passes_against_listening_port() {
        // Bind a fresh listener on a kernel-assigned port and probe it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let target = format!("127.0.0.1:{}", addr.port());
        // Accept-in-the-background so the connect can complete.
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let outcome = run_tcp(&target, Duration::from_secs(1)).await;
        assert!(
            matches!(outcome, ProbeOutcome::Passed),
            "expected pass, got {outcome:?}"
        );
    }
}
