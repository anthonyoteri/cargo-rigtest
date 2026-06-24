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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use console::style;
use futures::future::join_all;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use tokio::net::{lookup_host, TcpStream};
use tokio::time::timeout;

use crate::preflight::{CustomProbeFn, Probe, ProbeKind};
use crate::registry::{PreflightEntry, RIG_PREFLIGHT};

/// Outcome of a single probe.
#[derive(Debug, Clone)]
pub(crate) enum ProbeOutcome {
    Passed,
    Failed(String),
}

#[derive(Debug)]
pub(crate) struct ProbeResult {
    /// The probe definition the result corresponds to. Kept so future
    /// extensions (e.g. surfacing the probe `type` separately in the
    /// `JUnit` testcase) can attach without a struct-shape change.
    #[allow(dead_code)]
    pub probe: Probe,
    pub outcome: ProbeOutcome,
    pub elapsed: Duration,
    /// Display name produced by the tiered disambiguation algorithm —
    /// unique within the run. Used uniformly in the human-readable output
    /// and the `JUnit` `<testcase name=...>` attribute.
    pub display_name: String,
}

/// The full structured result of a preflight phase. Returned by
/// [`run_preflight`] so the orchestrator can both render results and emit
/// a `JUnit` `<testsuite name="preflight">` element with the same data.
pub(crate) struct PreflightOutcome {
    /// `true` when at least one probe was declared (i.e. a `#[preflight]`
    /// was present and contained probes). When `false` the orchestrator
    /// must not emit a synthetic `JUnit` testsuite — there is nothing to
    /// report.
    pub declared: bool,
    /// Per-probe results in declaration order. Empty when no probes were
    /// declared.
    pub results: Vec<ProbeResult>,
    /// `true` when every declared probe passed (or no probes were
    /// declared). `false` when at least one probe failed.
    pub passed: bool,
}

impl PreflightOutcome {
    /// A no-op outcome representing "preflight did not run" (no
    /// `#[preflight]` declared, or `--no-preflight` set). Distinguishable
    /// from a passing preflight by `declared == false`.
    pub(crate) fn none() -> Self {
        Self {
            declared: false,
            results: Vec::new(),
            passed: true,
        }
    }
}

/// Run the preflight phase, if one is declared.
///
/// Returns a [`PreflightOutcome`] in every non-error case — the caller
/// decides whether to abort (default), continue with failures recorded
/// (`--continue-on-preflight-failure`), or print the table and exit
/// (`--preflight-only`). Returns `Err` only when the suite's preflight is
/// structurally invalid (e.g. duplicate `name+type+target`, a `custom`
/// probe collision, or more than one `#[preflight]` per binary) — those
/// are the operator-error cases that must abort regardless of any flag.
pub(crate) async fn run_preflight() -> anyhow::Result<PreflightOutcome> {
    if RIG_PREFLIGHT.is_empty() {
        return Ok(PreflightOutcome::none());
    }
    // `RIG_PREFLIGHT.len() <= 1` is asserted in the orchestrator before we
    // are called; index 0 is sound here.
    let entry: &PreflightEntry = &RIG_PREFLIGHT[0];
    let profile = crate::__internal::active_profile_name();
    let preflight = (entry.build_fn)(&profile);
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
        // result line so the operator can see the phase ran. Treat as
        // "declared" so callers that key behavior off declaration (e.g.
        // `--preflight-only`'s success message) still get the same path
        // as a passing run.
        println_via(&multi, is_tty, "preflight result: 0 passed");
        return Ok(PreflightOutcome {
            declared: true,
            results: Vec::new(),
            passed: true,
        });
    }

    // Resolve display names through the full tiered disambiguation tree.
    // Errors here are structural ("two custom probes with the same name",
    // "exact duplicate probe"); they're operator mistakes that must abort
    // before any probe is executed regardless of `--continue-on-...`.
    let display_names = disambiguate_names(&probes)?;

    let spinners = build_spinners(&multi, &display_names, is_tty);

    let multi = Arc::new(multi);

    // Fire every probe concurrently. Each task finalizes its own spinner
    // (lines therefore appear in completion order) so a slow probe never
    // blocks fast ones from being marked done in the live output.
    let suite_start = Instant::now();
    let futures =
        probes
            .into_iter()
            .zip(spinners)
            .zip(display_names)
            .map(|((p, pb), display_name)| {
                let multi = Arc::clone(&multi);
                async move {
                    let start = Instant::now();
                    let outcome = run_probe(&p).await;
                    let elapsed = start.elapsed();
                    let line = render_probe_line(&display_name, &outcome, elapsed);
                    finalize_spinner(&multi, &pb, is_tty, &line);
                    ProbeResult {
                        probe: p,
                        outcome,
                        elapsed,
                        display_name,
                    }
                }
            });
    let results: Vec<ProbeResult> = join_all(futures).await;
    let total_elapsed = suite_start.elapsed();

    let passed = results
        .iter()
        .filter(|r| matches!(r.outcome, ProbeOutcome::Passed))
        .count();
    let failed_count = results
        .iter()
        .filter(|r| matches!(r.outcome, ProbeOutcome::Failed(_)))
        .count();

    if failed_count == 0 {
        println_via(
            &multi,
            is_tty,
            &format!(
                "preflight result: {passed} passed [{:.2}s]",
                total_elapsed.as_secs_f64()
            ),
        );
        return Ok(PreflightOutcome {
            declared: true,
            results,
            passed: true,
        });
    }

    render_readiness_table(&multi, is_tty, &results);
    Ok(PreflightOutcome {
        declared: true,
        results,
        passed: false,
    })
}

/// Convenience for `PreflightOutcome::failed` callers that want the same
/// abort message the runner used to produce. Kept as a free function (not
/// a method) so callers can compose it with extra context like the test
/// count without forcing the runner to know about test counts.
pub(crate) fn format_abort_message(failed_count: usize, tests_total: usize) -> String {
    format!(
        "{failed_count} probe{plural} failed — aborting suite ({tests_total} tests not run)",
        plural = if failed_count == 1 { "" } else { "s" },
    )
}

/// Pre-create one spinner per probe in declaration order so the TTY
/// layout is deterministic regardless of which probe finishes first.
fn build_spinners(
    multi: &MultiProgress,
    display_names: &[String],
    is_tty: bool,
) -> Vec<ProgressBar> {
    let spinner_style =
        ProgressStyle::with_template("{spinner:.cyan} {msg}").expect("valid spinner template");
    display_names
        .iter()
        .map(|name| {
            let pb = multi.add(ProgressBar::new_spinner());
            pb.set_style(spinner_style.clone());
            pb.set_message(format!("{} {}", style("RUNNING").cyan().bold(), name));
            if is_tty {
                pb.enable_steady_tick(Duration::from_millis(80));
            }
            pb
        })
        .collect()
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
            ProbeOutcome::Passed => (style("PASS").green().bold().to_string(), String::new()),
            ProbeOutcome::Failed(reason) => (
                style("FAIL").red().bold().to_string(),
                format!(": {reason}"),
            ),
        };
        println_via(
            multi,
            is_tty,
            &format!(
                "  {status} [{:.3}s] {}{detail}",
                r.elapsed.as_secs_f64(),
                r.display_name,
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
        // teardown does not race with MultiProgress's draw thread. Fall
        // back to stderr if the MultiProgress write fails so an operator
        // never silently loses readiness output (e.g. broken pipe in CI).
        if multi.println(line).is_err() {
            eprintln!("{line}");
        }
        pb.finish_and_clear();
    } else {
        eprintln!("{line}");
    }
}

fn println_via(multi: &MultiProgress, is_tty: bool, line: &str) {
    if is_tty {
        if multi.println(line).is_err() {
            eprintln!("{line}");
        }
    } else {
        eprintln!("{line}");
    }
}

/// Resolve display names for every probe through the four-tier
/// disambiguation tree:
///
/// - **Tier 1** — name unique → use `name`.
/// - **Tier 2** — same name across different probe *types* → `name(type)`.
/// - **Tier 3** — same name within the same type → `name(type[target])`,
///   where `target` is the operator-visible parameter (host:port for TCP,
///   host for DNS, URL for HTTP, dest for SSH, variable name for env).
///   `custom` probes have no inspectable target — a collision among
///   `custom` probes with the same name is an operator mistake and
///   returns an error.
/// - **Tier 4** — name *and* type *and* target all identical → genuine
///   duplicate, returns an error.
///
/// Returns a `Vec<String>` aligned 1:1 with `probes` so the caller can
/// use the same names for both human-readable output and `JUnit`
/// `<testcase name=...>` attributes.
pub(crate) fn disambiguate_names(probes: &[Probe]) -> anyhow::Result<Vec<String>> {
    // Count occurrences of every (name, type) and (name, type, target)
    // triple up front so the per-probe decision is a few lookups.
    let mut by_name: HashMap<&str, usize> = HashMap::new();
    let mut by_name_type: HashMap<(&str, &'static str), usize> = HashMap::new();
    let mut by_name_type_target: HashMap<(&str, &'static str, String), usize> = HashMap::new();

    for p in probes {
        let name = p.name.as_ref();
        *by_name.entry(name).or_default() += 1;
        let ty = p.kind.type_tag();
        *by_name_type.entry((name, ty)).or_default() += 1;
        if let Some(t) = p.kind.natural_target() {
            *by_name_type_target
                .entry((name, ty, t.to_string()))
                .or_default() += 1;
        }
    }

    let mut out = Vec::with_capacity(probes.len());
    for p in probes {
        let name = p.name.as_ref();
        let ty = p.kind.type_tag();
        let name_count = by_name.get(name).copied().unwrap_or(0);
        if name_count == 1 {
            out.push(name.to_string());
            continue;
        }
        let type_count = by_name_type.get(&(name, ty)).copied().unwrap_or(0);
        if type_count == 1 {
            out.push(format!("{name}({ty})"));
            continue;
        }
        // Tier 3+ — within-type collision. `custom` has no target so the
        // only remedy is renaming; surface that explicitly.
        let Some(target) = p.kind.natural_target() else {
            return Err(anyhow!(
                "duplicate custom probe {name:?} — give each custom probe a unique name",
            ));
        };
        let triple_count = by_name_type_target
            .get(&(name, ty, target.to_string()))
            .copied()
            .unwrap_or(0);
        if triple_count == 1 {
            out.push(format!("{name}({ty}[{target}])"));
            continue;
        }
        // Tier 4 — name + type + target all identical. Genuine duplicate.
        return Err(anyhow!("duplicate probe: {name}({ty}[{target}])"));
    }
    Ok(out)
}

/// Dispatch a probe and apply the per-probe timeout.
///
/// Centralising the `tokio::time::timeout` wrapper here keeps every
/// `run_X` helper focused on the actual check. `Some(d)` wraps the
/// helper in `timeout(d, …)` and reports a timeout failure on elapse;
/// `None` runs the helper to completion with no framework-imposed
/// deadline (used by `custom`).
async fn run_probe(p: &Probe) -> ProbeOutcome {
    // Env probes are synchronous — they never await, so the timeout
    // wrapper would just measure dispatch overhead. Handle them
    // separately so the `Some`/`None` branch below stays about the
    // async primitives.
    if let ProbeKind::Env { var, expected } = &p.kind {
        return run_env(var, expected.as_deref());
    }
    let fut = run_async_probe(p);
    match p.timeout {
        Some(d) => match timeout(d, fut).await {
            Ok(outcome) => outcome,
            Err(_) => ProbeOutcome::Failed(format!(
                "{} timed out after {:.3}s",
                probe_kind_label(&p.kind),
                d.as_secs_f64()
            )),
        },
        None => fut.await,
    }
}

async fn run_async_probe(p: &Probe) -> ProbeOutcome {
    match &p.kind {
        ProbeKind::Tcp { target } => run_tcp(target).await,
        ProbeKind::Env { .. } => {
            unreachable!("env probes are dispatched synchronously in run_probe")
        }
        ProbeKind::Dns { host } => run_dns(host).await,
        #[cfg(feature = "http-client")]
        ProbeKind::Http { url, expect } => run_http(url, expect, p.timeout).await,
        #[cfg(all(feature = "ssh-client", unix))]
        ProbeKind::Ssh { dest, command } => run_ssh(dest, command).await,
        ProbeKind::Custom { run } => run_custom(run).await,
    }
}

fn probe_kind_label(kind: &ProbeKind) -> String {
    match kind {
        ProbeKind::Tcp { target } => format!("connect to {target}"),
        ProbeKind::Env { var, .. } => format!("env {var}"),
        ProbeKind::Dns { host } => format!("resolving {host}"),
        #[cfg(feature = "http-client")]
        ProbeKind::Http { url, .. } => format!("GET {url}"),
        #[cfg(all(feature = "ssh-client", unix))]
        ProbeKind::Ssh { dest, .. } => format!("ssh {dest}"),
        ProbeKind::Custom { .. } => "custom probe".to_string(),
    }
}

async fn run_tcp(target: &str) -> ProbeOutcome {
    match TcpStream::connect(target).await {
        Ok(_stream) => ProbeOutcome::Passed,
        Err(e) => ProbeOutcome::Failed(format!("connect to {target} failed: {e}")),
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

/// Resolve `host` via the system resolver. `tokio::net::lookup_host` wants
/// a `host:port` argument so we tack on `:0`; the port plays no role —
/// we only consult whether at least one address came back.
async fn run_dns(host: &str) -> ProbeOutcome {
    let target = format!("{host}:0");
    match lookup_host(target).await {
        Ok(mut iter) => {
            if iter.next().is_some() {
                ProbeOutcome::Passed
            } else {
                ProbeOutcome::Failed(format!("{host} resolved to no addresses"))
            }
        }
        Err(e) => ProbeOutcome::Failed(format!("resolving {host} failed: {e}")),
    }
}

#[cfg(feature = "http-client")]
type HttpConfigurator =
    fn(reqwest::ClientBuilder) -> Result<reqwest::ClientBuilder, crate::registry::BoxError>;

#[cfg(feature = "http-client")]
async fn run_http(
    url: &str,
    expect: &crate::preflight::ExpectStatus,
    deadline: Option<Duration>,
) -> ProbeOutcome {
    let configurator: Option<HttpConfigurator> = crate::registry::RIG_HTTP_CLIENT_CONFIGURATOR
        .first()
        .map(|e| e.configure_fn);
    run_http_with(url, expect, deadline, configurator).await
}

/// Internal: shared between the real probe and the unit tests, so the
/// configurator-failure branch is testable without injecting into the
/// linkme distributed slice (which is fixed at link time).
///
/// The outer `tokio::time::timeout` wrapper lives in [`run_probe`]; this
/// function only configures `reqwest`'s own per-request timeout so the
/// HTTP stack can tear down cleanly before the outer guard fires.
#[cfg(feature = "http-client")]
async fn run_http_with(
    url: &str,
    expect: &crate::preflight::ExpectStatus,
    deadline: Option<Duration>,
    configurator: Option<HttpConfigurator>,
) -> ProbeOutcome {
    // Build the client through the same configurator the live tests use,
    // so a passing probe predicts that real tests can talk to the
    // endpoint. A configurator that errors makes only this probe fail —
    // other probes still run.
    let mut builder = reqwest::ClientBuilder::new();
    if let Some(d) = deadline {
        builder = builder.timeout(d);
    }
    if let Some(configure_fn) = configurator {
        match configure_fn(builder) {
            Ok(b) => builder = b,
            Err(e) => return ProbeOutcome::Failed(format!("configurator failed: {e}")),
        }
    }
    let client = match builder.build() {
        Ok(c) => c,
        Err(e) => return ProbeOutcome::Failed(format!("building http client failed: {e}")),
    };
    match client.get(url).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if expect.matches(status) {
                ProbeOutcome::Passed
            } else {
                ProbeOutcome::Failed(format!("GET {url} returned {status} (expected {expect})"))
            }
        }
        Err(e) => ProbeOutcome::Failed(format!("GET {url} failed: {e}")),
    }
}

#[cfg(all(feature = "ssh-client", unix))]
type SshConfigurator =
    fn(&str, openssh::SessionBuilder) -> Result<openssh::SessionBuilder, crate::registry::BoxError>;

#[cfg(all(feature = "ssh-client", unix))]
async fn run_ssh(dest: &str, command: &str) -> ProbeOutcome {
    let configurator: Option<SshConfigurator> = crate::registry::RIG_SSH_CLIENT_CONFIGURATOR
        .first()
        .map(|e| e.configure_fn);
    run_ssh_with(dest, command, configurator).await
}

/// Internal: shared between the real probe and the unit tests, so the
/// configurator-failure branch is testable without injecting into the
/// linkme distributed slice (which is fixed at link time).
#[cfg(all(feature = "ssh-client", unix))]
async fn run_ssh_with(
    dest: &str,
    command: &str,
    configurator: Option<SshConfigurator>,
) -> ProbeOutcome {
    let mut builder = openssh::SessionBuilder::default();
    if let Some(configure_fn) = configurator {
        match configure_fn(dest, builder) {
            Ok(b) => builder = b,
            Err(e) => return ProbeOutcome::Failed(format!("configurator failed: {e}")),
        }
    }
    let session = match builder.connect(dest).await {
        Ok(s) => s,
        Err(e) => return ProbeOutcome::Failed(format!("ssh {dest}: connect failed: {e}")),
    };
    let status = match session.command("sh").arg("-c").arg(command).status().await {
        Ok(s) => s,
        Err(e) => {
            return ProbeOutcome::Failed(format!("ssh {dest}: running {command:?} failed: {e}"));
        }
    };
    // Best-effort close; failure to close cleanly is not a probe
    // failure — the connect+exec already succeeded.
    let _ = session.close().await;
    if status.success() {
        ProbeOutcome::Passed
    } else {
        ProbeOutcome::Failed(format!(
            "ssh {dest}: remote command {command:?} exited with {status}"
        ))
    }
}

async fn run_custom(run: &CustomProbeFn) -> ProbeOutcome {
    match (run)().await {
        Ok(()) => ProbeOutcome::Passed,
        Err(e) => ProbeOutcome::Failed(format!("{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preflight::Preflight;

    // ── Disambiguation tier-by-tier coverage ─────────────────────────────
    //
    // The disambiguator decides how each probe is named in both the human
    // readiness table and the JUnit `<testcase name=...>` attribute, so we
    // exercise every tier explicitly: a regression here changes the names
    // CI dashboards expect across both surfaces.

    #[test]
    fn tier1_unique_names_pass_through_verbatim() {
        let p = Preflight::new().tcp("a", "1.2.3.4:1").env("b", "X");
        let names = disambiguate_names(&p.into_probes()).unwrap();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn tier2_collision_across_types_appends_type_tag() {
        let p = Preflight::new().tcp("api", "1.2.3.4:1").env("api", "X");
        let names = disambiguate_names(&p.into_probes()).unwrap();
        assert_eq!(names, vec!["api(tcp)".to_string(), "api(env)".to_string()]);
    }

    #[test]
    fn tier3_collision_within_tcp_appends_target() {
        let p = Preflight::new()
            .tcp("api", "1.2.3.4:1")
            .tcp("api", "1.2.3.4:2");
        let names = disambiguate_names(&p.into_probes()).unwrap();
        assert_eq!(
            names,
            vec![
                "api(tcp[1.2.3.4:1])".to_string(),
                "api(tcp[1.2.3.4:2])".to_string(),
            ],
        );
    }

    #[test]
    fn tier3_collision_within_dns_appends_host() {
        let p = Preflight::new()
            .dns("dns", "a.example.com")
            .dns("dns", "b.example.com");
        let names = disambiguate_names(&p.into_probes()).unwrap();
        assert_eq!(
            names,
            vec![
                "dns(dns[a.example.com])".to_string(),
                "dns(dns[b.example.com])".to_string(),
            ],
        );
    }

    #[test]
    fn tier3_collision_within_env_appends_var() {
        let p = Preflight::new().env("e", "HOME").env("e", "PATH");
        let names = disambiguate_names(&p.into_probes()).unwrap();
        assert_eq!(
            names,
            vec!["e(env[HOME])".to_string(), "e(env[PATH])".to_string()],
        );
    }

    #[cfg(feature = "http-client")]
    #[test]
    fn tier3_collision_within_http_appends_url() {
        let p = Preflight::new()
            .http("api", "http://a.example.com/")
            .http("api", "http://b.example.com/");
        let names = disambiguate_names(&p.into_probes()).unwrap();
        assert_eq!(
            names,
            vec![
                "api(http[http://a.example.com/])".to_string(),
                "api(http[http://b.example.com/])".to_string(),
            ],
        );
    }

    #[cfg(all(feature = "ssh-client", unix))]
    #[test]
    fn tier3_collision_within_ssh_appends_dest() {
        let p = Preflight::new()
            .ssh("ssh", "deploy@a")
            .ssh("ssh", "deploy@b");
        let names = disambiguate_names(&p.into_probes()).unwrap();
        assert_eq!(
            names,
            vec![
                "ssh(ssh[deploy@a])".to_string(),
                "ssh(ssh[deploy@b])".to_string(),
            ],
        );
    }

    #[test]
    fn tier3_custom_collision_is_an_error() {
        // `custom` probes have no inspectable target — the only fix is a
        // rename, which the message must say explicitly.
        let p = Preflight::new()
            .custom("c", || async { Ok(()) })
            .custom("c", || async { Ok(()) });
        let err = disambiguate_names(&p.into_probes())
            .expect_err("two custom probes with the same name must error");
        let msg = err.to_string();
        assert!(msg.contains("duplicate custom probe"), "got {msg}");
        assert!(msg.contains("unique name"), "got {msg}");
    }

    #[test]
    fn tier4_identical_name_type_target_is_an_error() {
        let p = Preflight::new()
            .tcp("api", "1.2.3.4:1")
            .tcp("api", "1.2.3.4:1");
        let err = disambiguate_names(&p.into_probes())
            .expect_err("two probes with identical name+type+target must error");
        let msg = err.to_string();
        assert!(msg.contains("duplicate probe"), "got {msg}");
        assert!(msg.contains("api(tcp[1.2.3.4:1])"), "got {msg}");
    }

    #[test]
    fn three_way_name_collision_across_types_all_disambiguated() {
        let p = Preflight::new()
            .tcp("x", "1.2.3.4:1")
            .env("x", "X")
            .dns("x", "example.com");
        let names = disambiguate_names(&p.into_probes()).unwrap();
        assert_eq!(
            names,
            vec![
                "x(tcp)".to_string(),
                "x(env)".to_string(),
                "x(dns)".to_string(),
            ],
        );
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
        let outcome = run_tcp("127.0.0.1:1").await;
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
        let outcome = run_tcp(&target).await;
        assert!(
            matches!(outcome, ProbeOutcome::Passed),
            "expected pass, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn dns_probe_passes_for_localhost() {
        // `localhost` resolves on every supported platform without
        // requiring a working DNS resolver — it's served from the hosts
        // file / NSS / WSAQuerySvc local table.
        let outcome = run_dns("localhost").await;
        assert!(
            matches!(outcome, ProbeOutcome::Passed),
            "expected pass, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn dns_probe_fails_for_invalid_tld() {
        // `.invalid` is reserved by RFC 2606 and guaranteed never to
        // resolve. The system resolver returns NXDOMAIN promptly.
        let outcome = run_dns("nonexistent.rigtest.invalid").await;
        assert!(
            matches!(outcome, ProbeOutcome::Failed(_)),
            "expected fail, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn custom_probe_pass_path() {
        use crate::preflight::Preflight;
        let preflight = Preflight::new().custom("ok", || async { Ok(()) });
        let probes = preflight.into_probes();
        let p = &probes[0];
        let outcome = run_probe(p).await;
        assert!(matches!(outcome, ProbeOutcome::Passed));
    }

    #[tokio::test]
    async fn custom_probe_fail_path() {
        use crate::preflight::Preflight;
        let preflight = Preflight::new().custom("bad", || async { Err("nope".into()) });
        let probes = preflight.into_probes();
        let p = &probes[0];
        let outcome = run_probe(p).await;
        match outcome {
            ProbeOutcome::Failed(msg) => assert!(msg.contains("nope")),
            ProbeOutcome::Passed => panic!("expected fail"),
        }
    }

    #[tokio::test]
    async fn probes_run_concurrently_not_sequentially() {
        // Five slow custom probes; if they ran sequentially the total
        // would be ~5 * 100ms = 500ms. With concurrent execution it
        // should be ~100ms. We assert a generous bound (250ms) to keep
        // the test robust against CI scheduler noise.
        use crate::preflight::Preflight;
        let preflight = (0..5).fold(Preflight::new(), |p, i| {
            let name: &'static str = Box::leak(format!("slow_{i}").into_boxed_str());
            p.custom(name, || async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(())
            })
        });
        let probes = preflight.into_probes();
        let start = Instant::now();
        let futures = probes.iter().map(run_probe);
        let outcomes = futures::future::join_all(futures).await;
        let elapsed = start.elapsed();
        assert!(
            outcomes.iter().all(|o| matches!(o, ProbeOutcome::Passed)),
            "all five probes should pass"
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "five 100ms probes ran in {elapsed:?}; concurrency is broken",
        );
    }

    #[tokio::test]
    async fn custom_probe_timeout_path() {
        use crate::preflight::Preflight;
        let preflight = Preflight::new()
            .custom("slow", || async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(())
            })
            .timeout(Duration::from_millis(50));
        let probes = preflight.into_probes();
        let p = &probes[0];
        let outcome = run_probe(p).await;
        match outcome {
            ProbeOutcome::Failed(msg) => assert!(msg.contains("timed out")),
            ProbeOutcome::Passed => panic!("expected timeout"),
        }
    }

    #[cfg(feature = "http-client")]
    #[tokio::test]
    async fn http_probe_passes_against_local_server() {
        use crate::preflight::ExpectStatus;
        let server = spawn_oneshot_http(204).await;
        let url: &'static str = Box::leak(format!("http://{}/", server.addr).into_boxed_str());
        let outcome = run_http(
            url,
            &ExpectStatus::Range(200..=299),
            Some(Duration::from_secs(2)),
        )
        .await;
        assert!(
            matches!(outcome, ProbeOutcome::Passed),
            "expected pass, got {outcome:?}"
        );
    }

    #[cfg(feature = "http-client")]
    #[tokio::test]
    async fn http_probe_fails_on_status_mismatch() {
        use crate::preflight::ExpectStatus;
        let server = spawn_oneshot_http(500).await;
        let url: &'static str = Box::leak(format!("http://{}/", server.addr).into_boxed_str());
        let outcome = run_http(
            url,
            &ExpectStatus::Range(200..=299),
            Some(Duration::from_secs(2)),
        )
        .await;
        match outcome {
            ProbeOutcome::Failed(msg) => {
                assert!(msg.contains("500"), "expected 500 in message, got {msg:?}");
            }
            ProbeOutcome::Passed => panic!("expected fail"),
        }
    }

    #[cfg(feature = "http-client")]
    #[tokio::test]
    async fn http_probe_exact_status_match() {
        use crate::preflight::ExpectStatus;
        let server = spawn_oneshot_http(204).await;
        let url: &'static str = Box::leak(format!("http://{}/", server.addr).into_boxed_str());
        let outcome = run_http(url, &ExpectStatus::Exact(204), Some(Duration::from_secs(2))).await;
        assert!(matches!(outcome, ProbeOutcome::Passed));
    }

    #[cfg(feature = "http-client")]
    #[tokio::test]
    async fn http_probe_fails_on_connect_refused() {
        use crate::preflight::ExpectStatus;
        // Bind, capture port, drop — that port is now unbound and
        // refuses connections.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url: &'static str = Box::leak(format!("http://127.0.0.1:{port}/").into_boxed_str());
        let outcome = run_http(
            url,
            &ExpectStatus::Range(200..=299),
            Some(Duration::from_millis(500)),
        )
        .await;
        assert!(
            matches!(outcome, ProbeOutcome::Failed(_)),
            "expected fail, got {outcome:?}"
        );
    }

    #[cfg(feature = "http-client")]
    struct OneshotHttpServer {
        addr: std::net::SocketAddr,
    }

    /// Minimal HTTP/1.0 server that accepts exactly one connection, reads
    /// until the request headers terminate, and writes a single status
    /// line + `Content-Length: 0` body. Avoids pulling in `axum` or
    /// `hyper` for what's effectively a one-line response.
    #[cfg(feature = "http-client")]
    async fn spawn_oneshot_http(status: u16) -> OneshotHttpServer {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Drain request headers — we don't care what the client
            // sends, only that the connection completes.
            let mut buf = [0u8; 1024];
            // Read once; reqwest sends the full GET in a single packet
            // for these tests so a single read is sufficient.
            let _ = stream.read(&mut buf).await;
            let reason = match status {
                200 => "OK",
                204 => "No Content",
                500 => "Internal Server Error",
                _ => "Status",
            };
            let response = format!(
                "HTTP/1.0 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
        OneshotHttpServer { addr }
    }

    #[cfg(feature = "http-client")]
    #[tokio::test]
    async fn http_probe_fails_on_invalid_url() {
        use crate::preflight::ExpectStatus;
        let outcome = run_http(
            "not-a-valid-url",
            &ExpectStatus::Range(200..=299),
            Some(Duration::from_millis(500)),
        )
        .await;
        assert!(
            matches!(outcome, ProbeOutcome::Failed(_)),
            "expected fail, got {outcome:?}"
        );
    }

    #[cfg(feature = "http-client")]
    #[tokio::test]
    async fn http_probe_configurator_error_is_recorded_as_probe_failure() {
        // Inject a failing configurator and confirm it surfaces as a
        // probe failure with the configurator's error attached — not a
        // panic, and not a silent pass.
        use crate::preflight::ExpectStatus;
        fn failing_configurator(
            _b: reqwest::ClientBuilder,
        ) -> Result<reqwest::ClientBuilder, crate::registry::BoxError> {
            Err("simulated configurator failure".into())
        }
        let outcome = run_http_with(
            "http://127.0.0.1:1/",
            &ExpectStatus::Range(200..=299),
            Some(Duration::from_millis(100)),
            Some(failing_configurator),
        )
        .await;
        match outcome {
            ProbeOutcome::Failed(msg) => {
                assert!(msg.contains("configurator failed"), "got {msg:?}");
                assert!(
                    msg.contains("simulated configurator failure"),
                    "got {msg:?}"
                );
            }
            ProbeOutcome::Passed => panic!("expected configurator failure to fail the probe"),
        }
    }

    #[cfg(feature = "http-client")]
    #[tokio::test]
    async fn http_probe_configurator_error_does_not_short_circuit_other_probes() {
        // Two probes: the first runs with a failing configurator, the
        // second is a passing TCP probe. Both should produce outcomes —
        // the failing configurator must not prevent the other probe
        // from running.
        use crate::preflight::ExpectStatus;
        fn failing_configurator(
            _b: reqwest::ClientBuilder,
        ) -> Result<reqwest::ClientBuilder, crate::registry::BoxError> {
            Err("boom".into())
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let tcp_target = format!("127.0.0.1:{}", addr.port());
        let (http_outcome, tcp_outcome) = tokio::join!(
            run_http_with(
                "http://127.0.0.1:1/",
                &ExpectStatus::Range(200..=299),
                Some(Duration::from_millis(100)),
                Some(failing_configurator),
            ),
            run_tcp(&tcp_target),
        );
        assert!(matches!(http_outcome, ProbeOutcome::Failed(_)));
        assert!(matches!(tcp_outcome, ProbeOutcome::Passed));
    }

    #[cfg(all(feature = "ssh-client", unix))]
    #[tokio::test]
    async fn ssh_probe_configurator_error_is_recorded_as_probe_failure() {
        fn failing_configurator(
            _dest: &str,
            _b: openssh::SessionBuilder,
        ) -> Result<openssh::SessionBuilder, crate::registry::BoxError> {
            Err("simulated ssh configurator failure".into())
        }
        let outcome = run_ssh_with("deploy@127.0.0.1", "true", Some(failing_configurator)).await;
        match outcome {
            ProbeOutcome::Failed(msg) => {
                assert!(msg.contains("configurator failed"), "got {msg:?}");
                assert!(
                    msg.contains("simulated ssh configurator failure"),
                    "got {msg:?}"
                );
            }
            ProbeOutcome::Passed => panic!("expected configurator failure to fail the probe"),
        }
    }
}
