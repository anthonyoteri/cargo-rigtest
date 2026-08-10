//! Pure scheduling plan: turns a filtered list of test cases into an ordered
//! sequence of execution [`Phase`]s with every per-case decision already
//! resolved.
//!
//! The plan is deliberately free of `tokio` and any I/O. It is a value the
//! executor consumes and tests can assert on directly — the "serial runs
//! alone against everything" invariant is encoded as data (the `Exclusive`
//! phase always follows the `Parallel` phase) rather than as control flow.
//!
//! Folding `effective_timeout` and the `--retries` override into
//! [`PlannedCase`] means those precedence rules are applied exactly once, at
//! plan time, instead of being re-derived on every attempt.

use std::time::Duration;

use crate::registry::TestCase;

/// A test case with every scheduling decision already resolved.
///
/// `timeout` and `max_attempts` are the final values the executor hands to the
/// subprocess runner — no further precedence logic is applied downstream.
#[derive(Clone, Copy)]
pub(crate) struct PlannedCase {
    /// The registered case; the executor needs it for the retry matcher, the
    /// reporter test-ref, and the subprocess test name.
    pub case: &'static TestCase,
    /// Resolved timeout: per-case `timeout` beats the suite default, and
    /// `no_timeout` forces `None`.
    pub timeout: Option<Duration>,
    /// Total attempts (`retries + 1`), after applying any `--retries` override.
    pub max_attempts: u32,
    /// The case's serial group, if any — carried so the executor can build the
    /// per-group mutual-exclusion locks.
    pub serial_group: Option<&'static str>,
}

impl PlannedCase {
    fn resolve(
        case: &'static TestCase,
        retries_override: Option<usize>,
        suite_default: Option<Duration>,
    ) -> Self {
        let effective_retries =
            retries_override.map_or(case.retries, |n| u32::try_from(n).unwrap_or(u32::MAX));
        Self {
            case,
            timeout: effective_timeout(case, suite_default),
            max_attempts: effective_retries.saturating_add(1),
            serial_group: case.serial_group,
        }
    }
}

// Identity-plus-resolution equality: two planned cases are equal when they
// wrap the same registered case and resolved to the same schedule decisions.
impl PartialEq for PlannedCase {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.case, other.case)
            && self.timeout == other.timeout
            && self.max_attempts == other.max_attempts
            && self.serial_group == other.serial_group
    }
}

impl std::fmt::Debug for PlannedCase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlannedCase")
            .field("name", &self.case.name)
            .field("timeout", &self.timeout)
            .field("max_attempts", &self.max_attempts)
            .field("serial_group", &self.serial_group)
            .finish()
    }
}

/// One execution phase. `Parallel` cases run concurrently under a global cap
/// and per-group locks; `Exclusive` cases (bare `serial`) run one at a time,
/// alone against everything else.
#[derive(Debug, PartialEq)]
pub(crate) enum Phase {
    Parallel {
        cases: Vec<PlannedCase>,
        cap: usize,
        /// Distinct serial-group names present among `cases`, sorted for a
        /// deterministic plan value.
        groups: Vec<&'static str>,
    },
    Exclusive {
        cases: Vec<PlannedCase>,
    },
}

/// An ordered execution plan. The `Parallel` phase always precedes the
/// `Exclusive` phase; that ordering is the serial-runs-alone invariant.
#[derive(Debug, PartialEq)]
pub(crate) struct Schedule {
    pub phases: Vec<Phase>,
}

impl Schedule {
    /// Partition `cases` into the parallel and exclusive phases, resolving each
    /// case's timeout and retry budget. `cap` is the global concurrency limit
    /// for the parallel phase.
    pub fn plan(
        cases: Vec<&'static TestCase>,
        cap: usize,
        retries_override: Option<usize>,
        suite_default: Option<Duration>,
    ) -> Self {
        let (serial, parallel): (Vec<_>, Vec<_>) = cases.into_iter().partition(|tc| tc.serial);

        let resolve = |tc| PlannedCase::resolve(tc, retries_override, suite_default);
        let parallel_cases: Vec<PlannedCase> = parallel.into_iter().map(resolve).collect();
        let exclusive_cases: Vec<PlannedCase> = serial.into_iter().map(resolve).collect();

        let mut groups: Vec<&'static str> = parallel_cases
            .iter()
            .filter_map(|pc| pc.serial_group)
            .collect();
        groups.sort_unstable();
        groups.dedup();

        Self {
            phases: vec![
                Phase::Parallel {
                    cases: parallel_cases,
                    cap,
                    groups,
                },
                Phase::Exclusive {
                    cases: exclusive_cases,
                },
            ],
        }
    }
}

/// Compute the timeout that actually applies to `tc`.
///
/// Precedence: a per-case explicit `timeout` wins over the suite-wide
/// `suite_default`; `no_timeout` forces no timeout even when a default is set.
/// With neither in play, the suite default (if any) applies.
fn effective_timeout(tc: &TestCase, suite_default: Option<Duration>) -> Option<Duration> {
    if tc.no_timeout {
        None
    } else {
        tc.timeout.or(suite_default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::TestContext;
    use crate::registry::BoxFuture;
    use std::sync::Arc;

    fn make_case(name: &'static str) -> TestCase {
        TestCase::new(
            name,
            "test_module",
            "test.rs",
            false,
            None,
            None,
            false,
            0,
            false,
            &[],
            |_ctx: Arc<TestContext>| -> BoxFuture<
                'static,
                Result<(), Box<dyn std::error::Error + Send + Sync>>,
            > { Box::pin(async { Ok(()) }) },
        )
    }

    fn leak(tc: TestCase) -> &'static TestCase {
        Box::leak(Box::new(tc))
    }

    fn parallel_phase(schedule: &Schedule) -> (&[PlannedCase], usize, &[&'static str]) {
        match &schedule.phases[0] {
            Phase::Parallel { cases, cap, groups } => (cases, *cap, groups),
            Phase::Exclusive { .. } => panic!("phase 0 must be Parallel"),
        }
    }

    fn exclusive_phase(schedule: &Schedule) -> &[PlannedCase] {
        match &schedule.phases[1] {
            Phase::Exclusive { cases } => cases,
            Phase::Parallel { .. } => panic!("phase 1 must be Exclusive"),
        }
    }

    // ── phase ordering: serial runs after parallel ──────────────────────

    #[test]
    fn exclusive_phase_follows_parallel_phase() {
        let mut serial = make_case("s1");
        serial.serial = true;
        let cases = vec![leak(make_case("p1")), leak(serial), leak(make_case("p2"))];

        let schedule = Schedule::plan(cases, 4, None, None);

        let (parallel, _, _) = parallel_phase(&schedule);
        let exclusive = exclusive_phase(&schedule);
        let parallel_names: Vec<&str> = parallel.iter().map(|pc| pc.case.name).collect();
        let exclusive_names: Vec<&str> = exclusive.iter().map(|pc| pc.case.name).collect();
        assert_eq!(parallel_names, vec!["p1", "p2"]);
        assert_eq!(exclusive_names, vec!["s1"]);
    }

    // ── group membership on the parallel phase ──────────────────────────

    #[test]
    fn parallel_phase_reports_distinct_sorted_groups() {
        let mk = |name, group| {
            let mut tc = make_case(name);
            tc.serial_group = Some(group);
            leak(tc)
        };
        let cases = vec![
            mk("b1", "beta"),
            mk("a1", "alpha"),
            mk("a2", "alpha"),
            leak(make_case("plain")),
        ];

        let schedule = Schedule::plan(cases, 4, None, None);

        let (_, _, groups) = parallel_phase(&schedule);
        assert_eq!(groups, ["alpha", "beta"]);
    }

    #[test]
    fn planned_case_carries_its_group() {
        let mut tc = make_case("g");
        tc.serial_group = Some("db");
        let schedule = Schedule::plan(vec![leak(tc)], 4, None, None);
        let (cases, _, _) = parallel_phase(&schedule);
        assert_eq!(cases[0].serial_group, Some("db"));
    }

    // ── resolved effective timeout: all four precedence cases ───────────

    fn planned_timeout(tc: TestCase, suite_default: Option<Duration>) -> Option<Duration> {
        let schedule = Schedule::plan(vec![leak(tc)], 4, None, suite_default);
        parallel_phase(&schedule).0[0].timeout
    }

    #[test]
    fn timeout_uses_suite_default_when_no_per_case() {
        let default = Some(Duration::from_secs(5));
        assert_eq!(planned_timeout(make_case("t"), default), default);
    }

    #[test]
    fn timeout_per_case_wins_over_default() {
        let mut tc = make_case("t");
        tc.timeout = Some(Duration::from_secs(1));
        assert_eq!(
            planned_timeout(tc, Some(Duration::from_secs(5))),
            Some(Duration::from_secs(1))
        );
    }

    #[test]
    fn timeout_no_timeout_forces_none() {
        let mut tc = make_case("t");
        tc.no_timeout = true;
        assert_eq!(planned_timeout(tc, Some(Duration::from_secs(5))), None);
    }

    #[test]
    fn timeout_none_when_nothing_set() {
        assert_eq!(planned_timeout(make_case("t"), None), None);
    }

    // ── resolved retry budget incl. --retries override ──────────────────

    fn planned_attempts(tc: TestCase, retries_override: Option<usize>) -> u32 {
        let schedule = Schedule::plan(vec![leak(tc)], 4, retries_override, None);
        parallel_phase(&schedule).0[0].max_attempts
    }

    #[test]
    fn attempts_default_to_declared_retries_plus_one() {
        let mut tc = make_case("t");
        tc.retries = 2;
        assert_eq!(planned_attempts(tc, None), 3);
    }

    #[test]
    fn attempts_override_replaces_declared_count() {
        let mut tc = make_case("t");
        tc.retries = 0;
        assert_eq!(planned_attempts(tc, Some(5)), 6);
    }

    #[test]
    fn attempts_override_zero_forces_single_attempt() {
        let mut tc = make_case("t");
        tc.retries = 3;
        assert_eq!(planned_attempts(tc, Some(0)), 1);
    }

    // ── mixed-suite characterization: assert the full Schedule value ────

    #[test]
    fn mixed_suite_plan_is_fully_characterized() {
        let bare = {
            let mut tc = make_case("bare_serial");
            tc.serial = true;
            leak(tc)
        };
        let a1 = {
            let mut tc = make_case("a1");
            tc.serial_group = Some("grp");
            leak(tc)
        };
        let a2 = {
            let mut tc = make_case("a2");
            tc.serial_group = Some("grp");
            leak(tc)
        };
        let other = {
            let mut tc = make_case("other");
            tc.serial_group = Some("zzz");
            leak(tc)
        };
        let plain = leak(make_case("plain"));
        let timed = {
            let mut tc = make_case("timed");
            tc.timeout = Some(Duration::from_secs(1));
            leak(tc)
        };

        let suite_default = Some(Duration::from_secs(5));
        let cases = vec![bare, a1, a2, other, plain, timed];

        let schedule = Schedule::plan(cases, 4, None, suite_default);

        let expected = Schedule {
            phases: vec![
                Phase::Parallel {
                    cap: 4,
                    groups: vec!["grp", "zzz"],
                    cases: vec![
                        PlannedCase {
                            case: a1,
                            timeout: suite_default,
                            max_attempts: 1,
                            serial_group: Some("grp"),
                        },
                        PlannedCase {
                            case: a2,
                            timeout: suite_default,
                            max_attempts: 1,
                            serial_group: Some("grp"),
                        },
                        PlannedCase {
                            case: other,
                            timeout: suite_default,
                            max_attempts: 1,
                            serial_group: Some("zzz"),
                        },
                        PlannedCase {
                            case: plain,
                            timeout: suite_default,
                            max_attempts: 1,
                            serial_group: None,
                        },
                        PlannedCase {
                            case: timed,
                            timeout: Some(Duration::from_secs(1)),
                            max_attempts: 1,
                            serial_group: None,
                        },
                    ],
                },
                Phase::Exclusive {
                    cases: vec![PlannedCase {
                        case: bare,
                        timeout: suite_default,
                        max_attempts: 1,
                        serial_group: None,
                    }],
                },
            ],
        };

        assert_eq!(schedule, expected);
    }
}
