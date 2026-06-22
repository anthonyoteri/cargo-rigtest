# cargo-rigtest

[![cargo-rigtest on crates.io](https://img.shields.io/crates/v/cargo-rigtest.svg?label=cargo-rigtest)](https://crates.io/crates/cargo-rigtest)
[![MSRV: 1.87](https://img.shields.io/badge/rustc-1.87+-orange.svg)](https://blog.rust-lang.org/2025/05/15/Rust-1.87.0.html)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/anthonyoteri/cargo-rigtest#license)

Cargo subcommand for running infrastructure and acceptance tests built with
the [`rigtest`](https://crates.io/crates/rigtest) library.

`cargo-rigtest` discovers `harness = false` test targets that link the
`rigtest` runtime, launches each test in its own subprocess, and reports
results in a nextest-style console (with an optional JUnit XML reporter for
CI). The test code itself — attributes, lifecycle hooks, `TestContext` —
lives in the [`rigtest`](https://crates.io/crates/rigtest) crate.

---

## Install

**From crates.io** (builds from source — requires a Rust toolchain):

```
cargo install cargo-rigtest
```

**Pre-built binaries** are available for macOS, Linux, and Windows on the
[releases page](https://github.com/anthonyoteri/cargo-rigtest/releases).
macOS and Linux releases are `.tar.gz` archives — extract and place
`cargo-rigtest` somewhere on your `PATH`. The Windows release is a plain
`.exe` — download it, rename it if desired, and place it on your `PATH`.

> **macOS note:** The release binaries are ad-hoc signed but not notarized
> or Developer ID signed. Gatekeeper may block the binary on first launch
> with a security warning. You can bypass this by right-clicking the binary
> in Finder and choosing **Open**, or by running
> `xattr -d com.apple.quarantine /path/to/cargo-rigtest` in your terminal.
> The Homebrew method below handles this automatically and is the
> recommended install path on macOS.

**Homebrew** (macOS and Linux):

```
brew tap anthonyoteri/tap
brew install cargo-rigtest
```

Once installed, `cargo rigtest` is available as a Cargo subcommand:

```
cargo rigtest run
```

You also need the [`rigtest`](https://crates.io/crates/rigtest) library
added to your project's `[dev-dependencies]` for the CLI to have something
to discover and run.

---

## `cargo rigtest run`

```
cargo rigtest run [OPTIONS]
```

| Flag | Description |
|------|-------------|
| `--jobs <N>` | Maximum parallel test jobs (default: number of CPUs) |
| `--seed <N>` | Fix the random order seed for reproducible runs |
| `--filter <STRING>` | Only run tests whose name contains STRING |
| `--tag <TAGS>` | Only run tests with matching tags (see [Tags and filtering](#tags-and-filtering)) |
| `--not-tag <TAGS>` | Exclude tests with matching tags |
| `--test <NAME>` | Only run the named test target (repeatable: `--test a --test b`) |
| `--package <NAME>` | Package containing the test targets |
| `--no-capture` | Print test output in real time instead of capturing it (implies `--jobs 1`) |
| `--no-preflight` | Skip the preflight phase entirely (see [Preflight](#preflight)) |
| `--preflight-only` | Run only the preflight phase and exit |
| `--continue-on-preflight-failure` | Don't abort on preflight failure; tests still run |
| `--reporter <KIND>` | Additional reporter alongside the console (see [JUnit XML output](#junit-xml-output)) |

The seed is printed at the start of every run so a failing order can be
reproduced exactly:

```
cargo rigtest run --seed 12345678
```

---

## Output

cargo-rigtest produces nextest-style output. In a TTY, running tests show
live spinners; results are printed as they complete:

```
── global setup
PASS [0.142s] homepage_returns_200
SKIP [0.031s] requires_live_database: DATABASE_URL not set
FAIL [0.089s] creates_a_record: assertion failed at tests/acceptance.rs:42

  ── stdout
  created record with id 99
  expected count 1, got 2

────────────────────────────────────────────────────────────
     Summary [0.21s] 3 tests run: 1 passed, 1 skipped, 1 failed
── global teardown
```

In CI or piped output, spinners are replaced with plain lines so no output is lost.

---

## Multiple test targets

If a package has more than one rigtest test target, all of them are
discovered and run in sequence automatically:

```
cargo rigtest run                          # run all rigtest targets
cargo rigtest run --test smoke             # run one
cargo rigtest run --test smoke --test e2e  # run two
```

cargo-rigtest identifies rigtest test targets automatically and ignores any
other `harness = false` binaries in the package.

---

## Tags and filtering

Tests can carry string tags via the `#[testcase(tags = […])]` attribute
(see [`rigtest`](https://crates.io/crates/rigtest)). The `--tag` and
`--not-tag` flags subset the suite by those tags at the command line.

```
cargo rigtest run --tag smoke                  # tests tagged "smoke"
cargo rigtest run --tag smoke,regression       # union: tagged either
cargo rigtest run --tag smoke --tag regression # equivalent: repeat the flag
cargo rigtest run --not-tag slow               # exclude "slow"
cargo rigtest run --tag smoke --not-tag slow   # smoke AND NOT slow
```

`--tag`, `--not-tag`, and `--filter` compose with AND: a test must match
the name filter, match at least one `--tag` (when any are given), and
match none of the `--not-tag` values. If nothing matches, the run exits
cleanly with a zero-result summary — the same behaviour as a `--filter`
that matches no tests.

---

## Preflight

A test binary can declare a `#[preflight]` function that runs once in the
coordinator, before `#[global_setup]` and before any test subprocess is
spawned, to verify the suite's declared external dependencies. See the
[`rigtest`](https://crates.io/crates/rigtest) crate for the macro and the
builder API.

If any declared probe fails, the readiness table is printed and the
coordinator aborts the suite **before** `#[global_setup]` runs — neither
setup nor teardown executes when preflight fails, so an unprepared
environment cannot leave half-initialised state behind.

### Exit codes

| Code | Meaning |
|------|---------|
| `0` | All tests passed (or no tests matched the filter) |
| `1` | At least one test failed |
| `2` | The preflight phase aborted the suite (one or more probes failed). Distinct from `1` so CI can distinguish "the environment isn't ready" from "the tests genuinely failed". |

### Skipping preflight

Pass `--no-preflight` to skip the phase entirely for one run:

```
cargo rigtest run --no-preflight
```

This is intended for local debugging, not CI: preflight exists
specifically to catch missing environment dependencies *before* tests run.

### Running preflight by itself

Pass `--preflight-only` to run the readiness check and exit without
running tests — useful as a fast pre-deployment smoke check:

```
cargo rigtest run --preflight-only
```

Exits `0` when every probe passes and `2` when any probe fails. When
the suite declares no `#[preflight]`, prints `no preflight declared`
and exits `0`.

### Continuing past preflight failures

Pass `--continue-on-preflight-failure` to demote probe failures to
warnings. The readiness table still prints; tests still run; the final
exit code reflects only the test phase. Useful when you want to publish
a JUnit report that shows both probe and test results regardless of
suite outcome — combine with `--reporter junit`.

---

## JUnit XML output

For CI systems that consume JUnit reports — Jenkins, GitLab CI, Buildkite,
CircleCI, and others — pass `--reporter junit`:

```
cargo rigtest run --reporter junit
```

This writes `target/rigtest/junit.xml` alongside the normal live console
output. The document uses the standard JUnit schema with the
`<flakyFailure>` and `<rerunFailure>` extensions for retried tests, so
existing JUnit-based integrations consume it without changes.

When a `#[preflight]` is declared, a synthetic `<testsuite
name="preflight">` element is emitted *before* the regular test
testsuite. Each probe becomes one `<testcase>` whose `name` attribute
uses the disambiguated form (see the rigtest README for the tiered
scheme); failed probes carry a `<failure>` child with the probe's error
message. With `--no-preflight` or no `#[preflight]` declared the
synthetic testsuite is absent — CI consumers can distinguish "preflight
did not run" from "preflight ran and everything passed".

In a Jenkins pipeline, point the `junit` step at the file after the run:

```groovy
sh 'cargo rigtest run --reporter junit'
junit 'target/rigtest/junit.xml'
```

---

## License

Licensed under either of [Apache License, Version 2.0][apache] or
[MIT license][mit] at your option.

[apache]: https://github.com/anthonyoteri/cargo-rigtest/blob/main/LICENSE-APACHE
[mit]: https://github.com/anthonyoteri/cargo-rigtest/blob/main/LICENSE-MIT
