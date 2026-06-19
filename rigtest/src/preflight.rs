//! Preflight: pre-suite verification of declared external dependencies.
//!
//! A [`Preflight`] declares a list of [`Probe`]s that the coordinator runs
//! once, before any test subprocess is spawned, to verify that the
//! suite's declared external dependencies are available. Each probe either
//! **passes** or **fails**; if any probe fails the suite aborts before
//! `#[global_setup]` runs and the coordinator exits with status `2`.
//!
//! Users declare a preflight by attaching the `#[preflight]` attribute to a
//! free function that returns a `Preflight`:
//!
//! ```ignore
//! use rigtest::Preflight;
//! use std::time::Duration;
//!
//! #[rigtest::preflight]
//! fn preflight() -> Preflight {
//!     Preflight::new()
//!         .tcp("api", "127.0.0.1:8080")
//!         .timeout(Duration::from_millis(500))
//!         .env("home_is_set", "HOME")
//! }
//! ```
//!
//! `#[preflight]` accepts `fn() -> Preflight`.
//!
//! See `CONTEXT.md` for the canonical vocabulary (probe, primitive,
//! preflight, coordinator).

use std::time::Duration;

/// Default timeout applied to every probe whose primitive has a meaningful
/// upper bound unless overridden via [`Preflight::timeout`]. Generous on
/// purpose — the library cannot know whether the operator is running
/// against a fast local network, a cross-continent VPN, or a satellite
/// link. Operators with tighter requirements call `.timeout(...)` to
/// tighten; the default exists to keep preflight from looking flaky in
/// environments the framework cannot survey.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// The kind of a [`Probe`]. Each variant corresponds to a builder method on
/// [`Preflight`] (a "primitive" in the project's vocabulary).
///
/// Variants may be added in future releases. The `#[non_exhaustive]`
/// attribute prevents external code from constructing this enum directly —
/// use [`Preflight`]'s builder methods.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ProbeKind {
    /// A TCP connect probe — passes when a TCP connection to `target`
    /// (a `host:port` string) is established within the probe's timeout.
    Tcp {
        /// `host:port` target string, e.g. `"127.0.0.1:8080"`.
        target: &'static str,
    },
    /// An environment-variable probe — passes when the named variable is
    /// set and either non-empty (default) or equals an explicit value
    /// (after [`Preflight::equals`]).
    Env {
        /// The environment variable to inspect, e.g. `"HOME"`.
        var: &'static str,
        /// When `Some`, the probe passes only if the variable's value
        /// equals this string exactly. When `None`, the probe passes if
        /// the variable is set and its value is non-empty.
        expected: Option<&'static str>,
    },
}

/// A single declared check. Carries the display name, the kind-specific
/// configuration, and a per-probe timeout.
///
/// Fields may be added in future releases. The `#[non_exhaustive]`
/// attribute prevents external code from constructing this struct via
/// struct-literal syntax — use [`Preflight`]'s builder methods.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Probe {
    /// Display name, as it appears in the readiness output and in
    /// duplicate-name diagnostics.
    pub name: &'static str,
    /// Kind-specific configuration. See [`ProbeKind`].
    pub kind: ProbeKind,
    /// Per-probe timeout. Defaults to [`DEFAULT_PROBE_TIMEOUT`]. TCP
    /// probes observe it as the connect deadline; environment probes
    /// evaluate synchronously and so never observe it.
    pub timeout: Duration,
}

/// Builder for a list of [`Probe`]s declared by a `#[preflight]` function.
///
/// Use [`Preflight::new`] to start the chain, then call [`tcp`][Self::tcp]
/// or [`env`][Self::env] to add probes, optionally followed by
/// [`timeout`][Self::timeout] / [`equals`][Self::equals] which adjust the
/// most-recently-added probe. Each method returns `Preflight` so the chain
/// reads naturally.
///
/// Fields may be added in future releases. The `#[non_exhaustive]`
/// attribute prevents external code from constructing this struct via
/// struct-literal syntax — use [`Preflight::new`].
#[derive(Debug, Default)]
#[non_exhaustive]
#[must_use]
pub struct Preflight {
    probes: Vec<Probe>,
}

impl Preflight {
    /// Creates an empty `Preflight`.
    pub fn new() -> Self {
        Self { probes: Vec::new() }
    }

    /// Adds a TCP connect probe.
    ///
    /// The probe passes when a TCP connection to `target` (a `host:port`
    /// string such as `"127.0.0.1:8080"`) is established within the
    /// probe's timeout. The default is
    /// [`DEFAULT_PROBE_TIMEOUT`] (30 seconds — generous to keep preflight
    /// from looking flaky in environments the framework cannot survey).
    /// Override per-probe with [`timeout`][Self::timeout].
    pub fn tcp(mut self, name: &'static str, target: &'static str) -> Self {
        self.probes.push(Probe {
            name,
            kind: ProbeKind::Tcp { target },
            timeout: DEFAULT_PROBE_TIMEOUT,
        });
        self
    }

    /// Adds an environment-variable probe.
    ///
    /// The default check passes when `var` is set in the coordinator's
    /// environment and its value is non-empty. Use
    /// [`equals`][Self::equals] to require a specific value.
    pub fn env(mut self, name: &'static str, var: &'static str) -> Self {
        self.probes.push(Probe {
            name,
            kind: ProbeKind::Env {
                var,
                expected: None,
            },
            timeout: DEFAULT_PROBE_TIMEOUT,
        });
        self
    }

    /// Overrides the timeout of the most recently added probe to `d`.
    ///
    /// Has no effect when no probe has been added yet, and no effect on
    /// the *evaluation* of environment probes (they are synchronous), but
    /// the value is still recorded so a future async `env` semantic can
    /// observe it without an API break.
    pub fn timeout(mut self, d: Duration) -> Self {
        if let Some(last) = self.probes.last_mut() {
            last.timeout = d;
        }
        self
    }

    /// Requires the most recently added environment probe's variable to
    /// equal `value` exactly. Has no effect when the most recent probe
    /// is not an environment probe, or when no probe has been added yet.
    pub fn equals(mut self, value: &'static str) -> Self {
        if let Some(last) = self.probes.last_mut() {
            if let ProbeKind::Env { expected, .. } = &mut last.kind {
                *expected = Some(value);
            }
        }
        self
    }

    /// Consumes the builder and returns the declared probes in
    /// declaration order. Intended for the framework runtime; not part of
    /// the stable public API.
    #[doc(hidden)]
    #[must_use]
    pub fn into_probes(self) -> Vec<Probe> {
        self.probes
    }

    /// Returns the declared probes without consuming the builder.
    /// Intended for tests; not part of the stable public API.
    #[doc(hidden)]
    #[must_use]
    pub fn probes(&self) -> &[Probe] {
        &self.probes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_builder() {
        let p = Preflight::new();
        assert!(p.probes().is_empty());
    }

    #[test]
    fn tcp_probe_records_default_timeout() {
        let p = Preflight::new().tcp("api", "127.0.0.1:1");
        let probes = p.probes();
        assert_eq!(probes.len(), 1);
        assert_eq!(probes[0].name, "api");
        assert!(matches!(
            probes[0].kind,
            ProbeKind::Tcp {
                target: "127.0.0.1:1"
            }
        ));
        assert_eq!(probes[0].timeout, DEFAULT_PROBE_TIMEOUT);
    }

    #[test]
    fn timeout_overrides_last_probe() {
        let p = Preflight::new()
            .tcp("api", "127.0.0.1:1")
            .timeout(Duration::from_millis(250));
        assert_eq!(p.probes()[0].timeout, Duration::from_millis(250));
    }

    #[test]
    fn env_probe_defaults_to_non_empty_check() {
        let p = Preflight::new().env("home", "HOME");
        let probes = p.probes();
        assert_eq!(probes.len(), 1);
        match probes[0].kind {
            ProbeKind::Env { var, expected } => {
                assert_eq!(var, "HOME");
                assert!(expected.is_none());
            }
            ProbeKind::Tcp { .. } => panic!("expected env probe"),
        }
    }

    #[test]
    fn equals_pins_expected_value() {
        let p = Preflight::new().env("mode", "MODE").equals("prod");
        match p.probes()[0].kind {
            ProbeKind::Env { expected, .. } => assert_eq!(expected, Some("prod")),
            ProbeKind::Tcp { .. } => panic!("expected env probe"),
        }
    }

    #[test]
    fn equals_on_tcp_probe_is_no_op() {
        // Calling `.equals(...)` after a `.tcp(...)` is meaningless but
        // must not corrupt state — the probe stays a TCP probe.
        let p = Preflight::new().tcp("api", "127.0.0.1:1").equals("ignored");
        assert!(matches!(p.probes()[0].kind, ProbeKind::Tcp { .. }));
    }

    #[test]
    fn chain_records_probes_in_declaration_order() {
        let p = Preflight::new()
            .tcp("a", "1.2.3.4:1")
            .env("b", "X")
            .tcp("c", "1.2.3.4:2");
        let names: Vec<&str> = p.probes().iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn timeout_after_chain_only_affects_just_added_probe() {
        let p = Preflight::new()
            .tcp("a", "1.2.3.4:1")
            .timeout(Duration::from_millis(100))
            .tcp("b", "1.2.3.4:2");
        assert_eq!(p.probes()[0].timeout, Duration::from_millis(100));
        assert_eq!(p.probes()[1].timeout, DEFAULT_PROBE_TIMEOUT);
    }
}
