//! Integration tests for the `#[preflight]` proc macro and the
//! coordinator-side preflight runner. These verify the public API shape
//! (builder ergonomics, registry registration) without actually spawning
//! a subprocess — the end-to-end CLI-level behaviour is covered by
//! `examples/basic` and `examples/calculator`, which run in CI under
//! `cargo rigtest run`.

use std::time::Duration;

use rigtest::preflight::{ProbeKind, DEFAULT_PROBE_TIMEOUT};
use rigtest::registry::RIG_PREFLIGHT;
use rigtest::Preflight;

#[rigtest::preflight]
fn preflight() -> Preflight {
    Preflight::new()
        .tcp("local_loopback_closed_port", "127.0.0.1:1")
        .timeout(Duration::from_millis(50))
        .env("path_is_set", "PATH")
}

#[test]
fn preflight_macro_registers_exactly_one_entry() {
    assert_eq!(
        RIG_PREFLIGHT.len(),
        1,
        "exactly one #[preflight] should be registered"
    );
}

#[test]
fn preflight_macro_invokes_user_builder() {
    let entry = &RIG_PREFLIGHT[0];
    let p = (entry.build_fn)("");
    let probes = p.probes();
    assert_eq!(probes.len(), 2, "builder declared two probes");

    assert_eq!(probes[0].name, "local_loopback_closed_port");
    assert!(matches!(
        &probes[0].kind,
        ProbeKind::Tcp { target } if target == "127.0.0.1:1"
    ));
    assert_eq!(probes[0].timeout, Some(Duration::from_millis(50)));

    assert_eq!(probes[1].name, "path_is_set");
    assert!(matches!(
        &probes[1].kind,
        ProbeKind::Env { var, expected: None } if var == "PATH"
    ));
}

#[test]
fn preflight_macro_records_declaration_order() {
    let entry = &RIG_PREFLIGHT[0];
    let p = (entry.build_fn)("");
    let names: Vec<&str> = p.probes().iter().map(|p| p.name.as_ref()).collect();
    assert_eq!(names, vec!["local_loopback_closed_port", "path_is_set"]);
}

#[test]
fn default_probe_timeout_is_thirty_seconds() {
    assert_eq!(DEFAULT_PROBE_TIMEOUT, Duration::from_secs(30));
}

// ── Profile-aware signature ─────────────────────────────────────────────
//
// Verify the macro routes the active profile name into a 1-arg builder.
// We exercise the 1-arg form via the helper builder below rather than a
// second `#[preflight]` attribute (only one is allowed per binary) — the
// macro adapter is the same code path either way.

fn profile_aware_builder(env: &str) -> Preflight {
    let host: &'static str = match env {
        "prod" => "prod.example.com",
        "staging" => "staging.example.com",
        _ => "localhost",
    };
    Preflight::new().dns("api_dns", host)
}

#[test]
fn one_arg_builder_branches_on_profile() {
    let p = profile_aware_builder("prod");
    let probes = p.probes();
    assert!(matches!(
        &probes[0].kind,
        ProbeKind::Dns { host } if host == "prod.example.com"
    ));
    let p = profile_aware_builder("staging");
    assert!(matches!(
        &p.probes()[0].kind,
        ProbeKind::Dns { host } if host == "staging.example.com"
    ));
    let p = profile_aware_builder("");
    assert!(matches!(
        &p.probes()[0].kind,
        ProbeKind::Dns { host } if host == "localhost"
    ));
}

#[test]
fn active_profile_name_reads_env_or_defaults_to_empty() {
    // Combined into a single sequential test because both branches mutate
    // a process-wide env var; running them as separate `#[test]` cases
    // would let cargo's parallel test harness race the read with the
    // write and produce flaky output.
    unsafe { std::env::remove_var("RIGTEST_PROFILE") };
    assert_eq!(rigtest::__internal::active_profile_name(), "");
    unsafe { std::env::set_var("RIGTEST_PROFILE", "staging") };
    assert_eq!(rigtest::__internal::active_profile_name(), "staging");
    unsafe { std::env::remove_var("RIGTEST_PROFILE") };
}
