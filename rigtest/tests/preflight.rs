//! Integration tests for the `#[preflight]` proc macro and the
//! coordinator-side preflight runner. These verify the public API shape
//! (builder ergonomics, registry registration) without actually spawning
//! a subprocess — the end-to-end CLI-level behaviour is covered by
//! `examples/basic` and `examples/calculator`, which run in CI under
//! `cargo rigtest run`.

use std::time::Duration;

use rigtest::preflight::{ProbeKind, DEFAULT_TCP_TIMEOUT};
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
    let p = (entry.build_fn)();
    let probes = p.probes();
    assert_eq!(probes.len(), 2, "builder declared two probes");

    assert_eq!(probes[0].name, "local_loopback_closed_port");
    assert!(matches!(
        probes[0].kind,
        ProbeKind::Tcp {
            target: "127.0.0.1:1"
        }
    ));
    assert_eq!(probes[0].timeout, Duration::from_millis(50));

    assert_eq!(probes[1].name, "path_is_set");
    assert!(matches!(
        probes[1].kind,
        ProbeKind::Env {
            var: "PATH",
            expected: None
        }
    ));
}

#[test]
fn preflight_macro_records_declaration_order() {
    let entry = &RIG_PREFLIGHT[0];
    let p = (entry.build_fn)();
    let names: Vec<&str> = p.probes().iter().map(|p| p.name).collect();
    assert_eq!(names, vec!["local_loopback_closed_port", "path_is_set"]);
}

#[test]
fn default_tcp_timeout_is_one_second() {
    assert_eq!(DEFAULT_TCP_TIMEOUT, Duration::from_secs(1));
}
