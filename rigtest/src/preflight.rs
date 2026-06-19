//! Preflight: pre-suite verification of declared external dependencies.
//!
//! A [`Preflight`] declares a list of [`Probe`]s that the coordinator runs
//! once, before any test subprocess is spawned, to verify that the
//! environment the suite needs is actually reachable. Each probe either
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
//!         .dns("api_dns", "example.com")
//! }
//! ```
//!
//! Only `fn() -> Preflight` is accepted in this release; the
//! profile-aware 1-arg form is planned for a later release.
//!
//! See `CONTEXT.md` for the canonical vocabulary (probe, primitive,
//! preflight, coordinator).

use std::fmt;
use std::future::Future;
use std::ops::RangeInclusive;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::registry::BoxError;

/// Default timeout applied to every TCP probe unless overridden via
/// [`Preflight::timeout`].
pub const DEFAULT_TCP_TIMEOUT: Duration = Duration::from_secs(1);

/// Default timeout applied to every DNS probe unless overridden via
/// [`Preflight::timeout`].
pub const DEFAULT_DNS_TIMEOUT: Duration = Duration::from_millis(500);

/// Default timeout applied to every HTTP probe unless overridden via
/// [`Preflight::timeout`].
pub const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Default timeout applied to every SSH probe unless overridden via
/// [`Preflight::timeout`].
pub const DEFAULT_SSH_TIMEOUT: Duration = Duration::from_secs(10);

/// Default timeout applied to every custom probe unless overridden via
/// [`Preflight::timeout`].
pub const DEFAULT_CUSTOM_TIMEOUT: Duration = Duration::from_secs(5);

/// Default acceptable status range for HTTP probes — every standard
/// success status (`200..=299`). Override with [`Preflight::expect_status`].
pub const DEFAULT_HTTP_OK_STATUS: RangeInclusive<u16> = 200..=299;

/// Default remote command run by SSH probes. `true` is portable across
/// every Unix shell and exits 0; we check the exit status, not the
/// command's output.
pub const DEFAULT_SSH_COMMAND: &str = "true";

/// What HTTP status codes count as a passing probe.
///
/// Constructed from a single `u16` (exact match) or a `RangeInclusive<u16>`
/// (any code in the inclusive range) via `Into`. Used as the argument to
/// [`Preflight::expect_status`].
///
/// Variants may be added in future releases. The `#[non_exhaustive]`
/// attribute prevents external code from constructing this enum directly —
/// use the `Into` impls.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ExpectStatus {
    /// The probe passes when the response status equals this code exactly.
    Exact(u16),
    /// The probe passes when the response status is contained in this
    /// inclusive range.
    Range(RangeInclusive<u16>),
}

impl ExpectStatus {
    /// Returns `true` when `status` satisfies this expectation.
    #[must_use]
    pub fn matches(&self, status: u16) -> bool {
        match self {
            Self::Exact(want) => *want == status,
            Self::Range(range) => range.contains(&status),
        }
    }
}

impl fmt::Display for ExpectStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(s) => write!(f, "{s}"),
            Self::Range(r) => write!(f, "{}..={}", r.start(), r.end()),
        }
    }
}

impl From<u16> for ExpectStatus {
    fn from(s: u16) -> Self {
        Self::Exact(s)
    }
}

impl From<RangeInclusive<u16>> for ExpectStatus {
    fn from(r: RangeInclusive<u16>) -> Self {
        Self::Range(r)
    }
}

/// Async factory stored by a [`ProbeKind::Custom`] probe.
///
/// Each invocation produces a fresh future that resolves to `Ok(())` on
/// success or a boxed error on failure. The factory is kept behind an
/// `Arc` so [`Probe`] stays cheap to move without forcing the closure to
/// be `Clone`.
pub type CustomProbeFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send>> + Send + Sync>;

/// The kind of a [`Probe`]. Each variant corresponds to a builder method on
/// [`Preflight`] (a "primitive" in the project's vocabulary).
///
/// Variants may be added in future releases. The `#[non_exhaustive]`
/// attribute prevents external code from constructing this enum directly —
/// use [`Preflight`]'s builder methods.
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
    /// A DNS resolution probe — passes when `host` resolves to at least
    /// one A or AAAA record within the probe's timeout. The host is a
    /// bare DNS name with no port (e.g. `"example.com"`); ports are out
    /// of scope for DNS and belong on [`ProbeKind::Tcp`].
    Dns {
        /// Hostname to resolve, e.g. `"example.com"`.
        host: &'static str,
    },
    /// An HTTP GET probe — passes when the response status satisfies
    /// [`expect`] (default: `200..=299`). Reuses the user's registered
    /// `#[rigtest::main(http_client = …)]` configurator when present so a
    /// passing probe predicts that real tests can talk to the endpoint.
    ///
    /// [`expect`]: ProbeKind::Http::expect
    #[cfg(feature = "http-client")]
    Http {
        /// Fully qualified URL, e.g. `"https://example.com/health"`.
        url: &'static str,
        /// Status codes that count as a passing probe.
        expect: ExpectStatus,
    },
    /// An SSH connect-and-exec probe — passes when a session to `dest`
    /// is established and the remote `command` exits 0. Reuses the
    /// user's registered `#[rigtest::main(ssh_client = …)]` configurator
    /// when present.
    #[cfg(all(feature = "ssh-client", unix))]
    Ssh {
        /// SSH destination string, e.g. `"deploy@bastion"`. Any value
        /// accepted by the `ssh` binary works.
        dest: &'static str,
        /// Remote command to execute. Defaults to
        /// [`DEFAULT_SSH_COMMAND`] (`"true"`); override with
        /// [`Preflight::command`].
        command: &'static str,
    },
    /// A user-supplied probe — the escape hatch for checks the named
    /// primitives don't cover. Passes when the closure's returned future
    /// resolves to `Ok(())`.
    Custom {
        /// The async factory. See [`CustomProbeFn`].
        run: CustomProbeFn,
    },
}

impl fmt::Debug for ProbeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp { target } => f.debug_struct("Tcp").field("target", target).finish(),
            Self::Env { var, expected } => f
                .debug_struct("Env")
                .field("var", var)
                .field("expected", expected)
                .finish(),
            Self::Dns { host } => f.debug_struct("Dns").field("host", host).finish(),
            #[cfg(feature = "http-client")]
            Self::Http { url, expect } => f
                .debug_struct("Http")
                .field("url", url)
                .field("expect", expect)
                .finish(),
            #[cfg(all(feature = "ssh-client", unix))]
            Self::Ssh { dest, command } => f
                .debug_struct("Ssh")
                .field("dest", dest)
                .field("command", command)
                .finish(),
            Self::Custom { .. } => f.debug_struct("Custom").finish_non_exhaustive(),
        }
    }
}

/// A single declared check. Carries the display name, the kind-specific
/// configuration, and a per-probe timeout (currently observed by every
/// asynchronous primitive; environment probes evaluate synchronously and
/// ignore it).
///
/// Fields may be added in future releases. The `#[non_exhaustive]`
/// attribute prevents external code from constructing this struct via
/// struct-literal syntax — use [`Preflight`]'s builder methods.
#[derive(Debug)]
#[non_exhaustive]
pub struct Probe {
    /// Display name, as it appears in the readiness output and in
    /// duplicate-name diagnostics.
    pub name: &'static str,
    /// Kind-specific configuration. See [`ProbeKind`].
    pub kind: ProbeKind,
    /// Per-probe timeout. Each primitive carries its own sensible default;
    /// environment probes carry one for diagnostic uniformity but evaluate
    /// synchronously and so never observe it.
    pub timeout: Duration,
}

/// Builder for a list of [`Probe`]s declared by a `#[preflight]` function.
///
/// Use [`Preflight::new`] to start the chain, then call one of the probe
/// constructors ([`tcp`][Self::tcp], [`env`][Self::env],
/// [`dns`][Self::dns], [`http`][Self::http], [`ssh`][Self::ssh],
/// [`custom`][Self::custom]), optionally followed by an adjustment
/// ([`timeout`][Self::timeout], [`equals`][Self::equals],
/// [`expect_status`][Self::expect_status], [`command`][Self::command])
/// which acts on the most-recently-added probe. Each method returns
/// `Preflight` so the chain reads naturally.
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
    /// probe's timeout. The default timeout is [`DEFAULT_TCP_TIMEOUT`]
    /// (one second); override per-probe with
    /// [`timeout`][Self::timeout].
    pub fn tcp(mut self, name: &'static str, target: &'static str) -> Self {
        self.probes.push(Probe {
            name,
            kind: ProbeKind::Tcp { target },
            timeout: DEFAULT_TCP_TIMEOUT,
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
            timeout: DEFAULT_TCP_TIMEOUT,
        });
        self
    }

    /// Adds a DNS resolution probe.
    ///
    /// `host` is a bare DNS name with **no port** (for example
    /// `"example.com"`). The probe passes when the name resolves to at
    /// least one A or AAAA record within the probe's timeout. The default
    /// timeout is [`DEFAULT_DNS_TIMEOUT`] (500 ms); override per-probe
    /// with [`timeout`][Self::timeout].
    pub fn dns(mut self, name: &'static str, host: &'static str) -> Self {
        self.probes.push(Probe {
            name,
            kind: ProbeKind::Dns { host },
            timeout: DEFAULT_DNS_TIMEOUT,
        });
        self
    }

    /// Adds an HTTP GET probe.
    ///
    /// The probe passes when a `GET url` returns a status in
    /// [`DEFAULT_HTTP_OK_STATUS`] (`200..=299`). Override the acceptable
    /// status with [`expect_status`][Self::expect_status], and the
    /// timeout (default [`DEFAULT_HTTP_TIMEOUT`], 5 s) with
    /// [`timeout`][Self::timeout].
    ///
    /// If the suite registers an HTTP configurator via
    /// `#[rigtest::main(http_client = …)]`, the probe applies it to a
    /// fresh [`reqwest::ClientBuilder`] so it talks to the endpoint the
    /// same way real tests will. A configurator that returns `Err` causes
    /// the probe to fail with the configurator's error attached; other
    /// probes still run.
    #[cfg(feature = "http-client")]
    pub fn http(mut self, name: &'static str, url: &'static str) -> Self {
        self.probes.push(Probe {
            name,
            kind: ProbeKind::Http {
                url,
                expect: ExpectStatus::Range(DEFAULT_HTTP_OK_STATUS),
            },
            timeout: DEFAULT_HTTP_TIMEOUT,
        });
        self
    }

    /// Adds an SSH connect-and-exec probe.
    ///
    /// `dest` is any value accepted by the `ssh` binary — for example
    /// `"user@host"`, `"host"`, or a `~/.ssh/config` alias. The probe
    /// passes when a session establishes and the remote command (default
    /// [`DEFAULT_SSH_COMMAND`], `"true"`) exits 0. Override the command
    /// with [`command`][Self::command] and the timeout (default
    /// [`DEFAULT_SSH_TIMEOUT`], 10 s) with [`timeout`][Self::timeout].
    ///
    /// If the suite registers an SSH configurator via
    /// `#[rigtest::main(ssh_client = …)]`, the probe applies it to a
    /// fresh [`openssh::SessionBuilder`]. A configurator that returns
    /// `Err` causes the probe to fail with the configurator's error
    /// attached; other probes still run.
    ///
    /// # Platform support
    ///
    /// Available only on Unix; [`openssh`] is a Unix-only dependency.
    #[cfg(all(feature = "ssh-client", unix))]
    pub fn ssh(mut self, name: &'static str, dest: &'static str) -> Self {
        self.probes.push(Probe {
            name,
            kind: ProbeKind::Ssh {
                dest,
                command: DEFAULT_SSH_COMMAND,
            },
            timeout: DEFAULT_SSH_TIMEOUT,
        });
        self
    }

    /// Adds a user-supplied probe — the escape hatch for checks the
    /// six named primitives don't cover.
    ///
    /// `factory` is invoked once when the probe runs and must return a
    /// `Send` future that resolves to `Ok(())` on success or to an error
    /// on failure. The default timeout is [`DEFAULT_CUSTOM_TIMEOUT`]
    /// (5 s); override with [`timeout`][Self::timeout].
    ///
    /// ```ignore
    /// use rigtest::Preflight;
    /// Preflight::new().custom("mx_record", || async move {
    ///     // … any async check …
    ///     Ok(())
    /// });
    /// ```
    pub fn custom<F, Fut>(mut self, name: &'static str, factory: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), BoxError>> + Send + 'static,
    {
        let run: CustomProbeFn = Arc::new(move || {
            let fut = factory();
            Box::pin(fut) as Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send>>
        });
        self.probes.push(Probe {
            name,
            kind: ProbeKind::Custom { run },
            timeout: DEFAULT_CUSTOM_TIMEOUT,
        });
        self
    }

    /// Overrides the timeout of the most recently added probe.
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

    /// Overrides the acceptable response status of the most recently
    /// added HTTP probe. Accepts a single status (`200`) or an inclusive
    /// range (`200..=204`); the impl uses `Into<ExpectStatus>`. Has no
    /// effect when the most recent probe is not an HTTP probe.
    #[cfg(feature = "http-client")]
    pub fn expect_status<S: Into<ExpectStatus>>(mut self, status: S) -> Self {
        if let Some(last) = self.probes.last_mut() {
            if let ProbeKind::Http { expect, .. } = &mut last.kind {
                *expect = status.into();
            }
        }
        self
    }

    /// Overrides the remote command of the most recently added SSH
    /// probe. The probe still requires exit status 0; only the command
    /// run on the remote host changes. Has no effect when the most
    /// recent probe is not an SSH probe.
    #[cfg(all(feature = "ssh-client", unix))]
    pub fn command(mut self, command: &'static str) -> Self {
        if let Some(last) = self.probes.last_mut() {
            if let ProbeKind::Ssh { command: c, .. } = &mut last.kind {
                *c = command;
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
        assert_eq!(probes[0].timeout, DEFAULT_TCP_TIMEOUT);
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
            _ => panic!("expected env probe"),
        }
    }

    #[test]
    fn equals_pins_expected_value() {
        let p = Preflight::new().env("mode", "MODE").equals("prod");
        match p.probes()[0].kind {
            ProbeKind::Env { expected, .. } => assert_eq!(expected, Some("prod")),
            _ => panic!("expected env probe"),
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
        assert_eq!(p.probes()[1].timeout, DEFAULT_TCP_TIMEOUT);
    }

    #[test]
    fn dns_probe_records_default_timeout() {
        let p = Preflight::new().dns("api_dns", "example.com");
        let probes = p.probes();
        assert_eq!(probes.len(), 1);
        assert_eq!(probes[0].name, "api_dns");
        assert!(matches!(
            probes[0].kind,
            ProbeKind::Dns {
                host: "example.com"
            }
        ));
        assert_eq!(probes[0].timeout, DEFAULT_DNS_TIMEOUT);
    }

    #[test]
    fn each_primitive_carries_its_documented_default_timeout() {
        // A regression guard for the documented defaults — every named
        // primitive must use the constant that the README references.
        let mut p = Preflight::new()
            .tcp("tcp", "1.2.3.4:1")
            .env("env", "X")
            .dns("dns", "example.com");
        assert_eq!(p.probes()[0].timeout, DEFAULT_TCP_TIMEOUT);
        assert_eq!(p.probes()[2].timeout, DEFAULT_DNS_TIMEOUT);
        p = p.custom("custom", || async { Ok(()) });
        assert_eq!(p.probes()[3].timeout, DEFAULT_CUSTOM_TIMEOUT);

        #[cfg(feature = "http-client")]
        {
            let p = p.http("http", "http://example.com/");
            assert_eq!(p.probes()[4].timeout, DEFAULT_HTTP_TIMEOUT);

            #[cfg(all(feature = "ssh-client", unix))]
            {
                let p = p.ssh("ssh", "deploy@bastion");
                assert_eq!(p.probes()[5].timeout, DEFAULT_SSH_TIMEOUT);
            }
        }
    }

    #[test]
    fn custom_probe_records_default_timeout() {
        let p = Preflight::new().custom("noop", || async { Ok(()) });
        let probes = p.probes();
        assert_eq!(probes.len(), 1);
        assert_eq!(probes[0].name, "noop");
        assert!(matches!(probes[0].kind, ProbeKind::Custom { .. }));
        assert_eq!(probes[0].timeout, DEFAULT_CUSTOM_TIMEOUT);
    }

    #[cfg(feature = "http-client")]
    #[test]
    fn http_probe_defaults_to_2xx_range() {
        let p = Preflight::new().http("api", "https://example.com/health");
        match &p.probes()[0].kind {
            ProbeKind::Http { url, expect } => {
                assert_eq!(*url, "https://example.com/health");
                assert!(matches!(expect, ExpectStatus::Range(r) if *r == (200..=299)));
            }
            _ => panic!("expected http probe"),
        }
        assert_eq!(p.probes()[0].timeout, DEFAULT_HTTP_TIMEOUT);
    }

    #[cfg(feature = "http-client")]
    #[test]
    fn expect_status_accepts_single_code() {
        let p = Preflight::new()
            .http("api", "https://example.com/health")
            .expect_status(204_u16);
        match &p.probes()[0].kind {
            ProbeKind::Http { expect, .. } => {
                assert!(matches!(expect, ExpectStatus::Exact(204)));
            }
            _ => panic!("expected http probe"),
        }
    }

    #[cfg(feature = "http-client")]
    #[test]
    fn expect_status_accepts_inclusive_range() {
        let p = Preflight::new()
            .http("api", "https://example.com/health")
            .expect_status(200_u16..=204);
        match &p.probes()[0].kind {
            ProbeKind::Http { expect, .. } => {
                assert!(matches!(expect, ExpectStatus::Range(r) if *r == (200..=204)));
            }
            _ => panic!("expected http probe"),
        }
    }

    #[cfg(feature = "http-client")]
    #[test]
    fn expect_status_on_non_http_probe_is_no_op() {
        let p = Preflight::new()
            .tcp("api", "127.0.0.1:1")
            .expect_status(200_u16);
        assert!(matches!(p.probes()[0].kind, ProbeKind::Tcp { .. }));
    }

    #[test]
    fn expect_status_matches_exact_and_range() {
        let exact: ExpectStatus = 204_u16.into();
        assert!(exact.matches(204));
        assert!(!exact.matches(205));
        let range: ExpectStatus = (200_u16..=204).into();
        assert!(range.matches(200));
        assert!(range.matches(204));
        assert!(!range.matches(205));
    }

    #[cfg(all(feature = "ssh-client", unix))]
    #[test]
    fn ssh_probe_records_default_command_and_timeout() {
        let p = Preflight::new().ssh("bastion", "deploy@bastion");
        match &p.probes()[0].kind {
            ProbeKind::Ssh { dest, command } => {
                assert_eq!(*dest, "deploy@bastion");
                assert_eq!(*command, DEFAULT_SSH_COMMAND);
            }
            _ => panic!("expected ssh probe"),
        }
        assert_eq!(p.probes()[0].timeout, DEFAULT_SSH_TIMEOUT);
    }

    #[cfg(all(feature = "ssh-client", unix))]
    #[test]
    fn command_overrides_last_ssh_probe() {
        let p = Preflight::new()
            .ssh("bastion", "deploy@bastion")
            .command("uname -s");
        match &p.probes()[0].kind {
            ProbeKind::Ssh { command, .. } => assert_eq!(*command, "uname -s"),
            _ => panic!("expected ssh probe"),
        }
    }

    #[cfg(all(feature = "ssh-client", unix))]
    #[test]
    fn command_on_non_ssh_probe_is_no_op() {
        let p = Preflight::new()
            .tcp("api", "127.0.0.1:1")
            .command("uname -s");
        assert!(matches!(p.probes()[0].kind, ProbeKind::Tcp { .. }));
    }
}
