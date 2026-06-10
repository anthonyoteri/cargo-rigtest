# cargo-rigtest

[![CI](https://github.com/anthonyoteri/cargo-rigtest/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/anthonyoteri/cargo-rigtest/actions/workflows/ci.yml)
[![rigtest on crates.io](https://img.shields.io/crates/v/rigtest.svg?label=rigtest)](https://crates.io/crates/rigtest)
[![cargo-rigtest on crates.io](https://img.shields.io/crates/v/cargo-rigtest.svg?label=cargo-rigtest)](https://crates.io/crates/cargo-rigtest)
[![docs.rs](https://img.shields.io/docsrs/rigtest?label=docs.rs)](https://docs.rs/rigtest)
[![MSRV: 1.87](https://img.shields.io/badge/rustc-1.87+-orange.svg)](https://blog.rust-lang.org/2025/05/15/Rust-1.87.0.html)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/anthonyoteri/cargo-rigtest#license)

A Cargo plugin for infrastructure and acceptance testing in Rust.

cargo-rigtest runs each test in its own subprocess, giving you
process-level isolation, parallel execution, structured output, and
first-class support for shared infrastructure setup — without the overhead
of spinning up a full test harness.

---

## Overview

Most Rust projects have two test layers covered: unit tests with `#[test]`,
and integration tests that compile and run a local binary. The third layer
— *acceptance tests against a deployed system* — is almost always handled
by reaching for another tool: pytest, Postman, shell scripts.

`cargo-rigtest` closes that gap. It offers the kind of process-level
isolation and parallel execution you'd expect from a modern Rust test
runner, and pairs that with the lifecycle hooks and clients that
acceptance testing against a real system actually needs:

| Feature                          | cargo-test | cargo-nextest | cargo-rigtest |
|----------------------------------|:----------:|:-------------:|:-------------:|
| Process isolation per test       |     —      |       ✓       |       ✓       |
| Captured output on failure       |     —      |       ✓       |       ✓       |
| Per-test timeout / retries       |     —      |     ✓¹       |       ✓       |
| Global setup / teardown          |     —      |       —       |       ✓       |
| Per-test setup / teardown        |     —      |       —       |       ✓       |
| Built-in HTTP client             |     —      |       —       |       ✓       |
| Built-in SSH client              |     —      |       —       |       ✓       |

¹ via config file

The motivating case: a service is deployed to staging; before promoting
to production you want to verify the signup flow completes, authenticated
requests are accepted, and data persists correctly. The binary is already
running. There's nothing to mock. These are acceptance tests — written
in Rust, living inside your Cargo workspace, type-checked by the same
compiler as the rest of your code. Local binary tests still work fine;
deployed systems are just where rigtest is sharpest.

---

## Features

cargo-rigtest is built around a few core ideas: tests that can't interfere
with each other, infrastructure that's set up once and shared cleanly, and
output that tells you exactly what failed without making you dig through
logs.

- **Process isolation** — each test runs in its own subprocess; a panic
  or crash cannot affect other tests
- **Parallel execution** — tests run concurrently by default, configurable
  with `--jobs`
- **Global setup & teardown** — `#[global_setup]` and `#[global_teardown]`
  provision and clean up shared infrastructure once per suite
- **Per-test lifecycle** — `TestContext` provides scoped `setup` and
  `teardown` hooks for resources that belong to a single test
- **Serial, timeout, and retry** — opt individual tests into exclusive
  execution, a hard time limit, or automatic retries
- **Captured output** — held per test and printed only on failure,
  nextest-style
- **Runtime skip** — `rigtest::skip!("reason")` lets a test opt out
  gracefully at runtime

---

## Installation

Getting started is two steps: install the `cargo rigtest` command, then
add the `rigtest` library to your project.

### Install the CLI

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

### Add the library

```toml
[dev-dependencies]
rigtest = "0.1"
```

If your tests make HTTP calls, enable the `http-client` feature to get a
shared `reqwest::Client` via `ctx.client().await?` in every test:

```toml
[dev-dependencies]
rigtest = { version = "0.1", features = ["http-client"] }
```

You can also make HTTP calls without this feature — just bring your own
client library and construct it in your tests.

---

## Quick start

### 1. Add a test target

In your `Cargo.toml`, add a `[[test]]` section with `harness = false`:

```toml
[[test]]
name = "acceptance"
path = "tests/acceptance.rs"
harness = false
```

### 2. Write the test file

```rust
use std::sync::Arc;
use rigtest::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct State {
    base_url: String,
}

#[global_setup]
async fn setup() -> State {
    State { base_url: "http://localhost:8080".to_string() }
}

#[global_teardown]
async fn teardown(state: State) {
    println!("releasing resources for {}", state.base_url);
}

#[testcase]
async fn homepage_returns_200(
    ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = ctx.global::<State>();
    // ctx.client() requires the `http-client` feature
    let resp = ctx.client().await?.get(&state.base_url).send().await?;
    assert_eq!(resp.status(), 200);
    Ok(())
}

fn main() {
    rigtest::run_main();
}
```

### 3. Run

```
cargo rigtest run
```

---

## Test attributes

### `#[testcase]`

Registers an async function as a test case. The function must have this
signature:

```rust
async fn name(ctx: Arc<TestContext>) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
```

Optional flags can be combined in any order:

```rust
#[testcase(serial, timeout = std::time::Duration::from_secs(30), retries = 2)]
```

| Flag | Description |
|------|-------------|
| `serial` | Run this test exclusively — no other test runs concurrently with it |
| `timeout = <Duration>` | Hard-kill the test subprocess and report failure if it exceeds the duration |
| `retries = <N>` | Retry a failing test up to N additional times before reporting failure |

> **Note on timeout and teardown:** when a timeout fires, the subprocess is
> terminated — on Linux and macOS a graceful signal is sent first, with a
> short window for the process to exit cleanly before a hard kill follows;
> on Windows the process is hard-killed immediately. Either way, teardown
> registered with `ctx.teardown(...)` will not run. Resources that must be
> released regardless of outcome should be handled in
> `#[global_teardown]`, which runs outside the test subprocess.

### `#[global_setup]`

Runs once before any test in the suite. The return value is serialized and
passed to each test subprocess. At most one may be defined.

```rust
#[global_setup]
async fn setup() -> MyState {
    MyState { db_url: std::env::var("DATABASE_URL").unwrap() }
}
```

The return type must implement `serde::Serialize` and `serde::Deserialize`
— the state is serialized to cross the process boundary into each test
subprocess. This means it can only hold serializable values: URLs, ports,
credentials, identifiers. Live resources such as connection pools, file
descriptors, and socket handles cannot survive the round-trip — store the
configuration needed to recreate them instead.

### `#[global_teardown]`

Runs once after all tests finish. Receives the value produced by
`#[global_setup]`. At most one may be defined.

```rust
#[global_teardown]
async fn teardown(state: MyState) {
    MyDb::connect(&state.db_url).await.unwrap().drop_schema().await;
}
```

---

## Per-test setup and teardown

`TestContext` provides `setup` and `teardown` hooks for resources with a
clear per-test lifecycle. The `global` argument in both closures is the
deserialized state from `#[global_setup]` — use it to create live
resources within the test.

```rust
#[testcase]
async fn creates_a_record(
    ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = ctx
        .setup(|global| async move {
            let state = global.downcast_ref::<State>().unwrap();
            db::connect(&state.db_url).await
        })
        .await?;  // failure reported as "setup failed: ..."

    conn.insert("hello").await?;
    assert_eq!(conn.count().await?, 1);

    ctx.teardown(|_global| async move {
        conn.rollback().await?;
        Ok(())
    })
    .await?;  // failure reported as "teardown failed: ..."

    Ok(())
}
```

---

## Skipping tests

Use `rigtest::skip!` to skip a test at runtime with an optional reason:

```rust
#[testcase]
async fn requires_live_database(
    _ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if std::env::var("DATABASE_URL").is_err() {
        rigtest::skip!("DATABASE_URL not set");
    }
    // ...
    Ok(())
}
```

Skipped tests appear in the summary as `SKIP` and do not count as failures.

---

## HTTP client

Enable the `http-client` feature for a built-in `reqwest::Client` accessible
via `ctx.client().await?`. See [`examples/http-client`](examples/http-client)
for a working example including custom TLS configuration.

---

## SSH client

> **Unix only.** The `ssh-client` feature depends on [`openssh`](https://crates.io/crates/openssh),
> which delegates to the system `ssh` binary via Unix pipes. It does not
> compile on Windows or other non-Unix targets.

Enable the `ssh-client` feature for cached SSH sessions accessible via
`ctx.ssh(destination).await?`:

```toml
[dev-dependencies]
rigtest = { version = "0.1", features = ["ssh-client"] }
```

`ctx.ssh("user@host")` returns an `Arc<openssh::Session>` connected to the
given destination. Sessions are cached by destination string within the test
subprocess — repeated calls to the same host reuse the existing connection,
avoiding expensive reconnects over high-latency tunnels.

The `rigtest::ssh!` convenience macro runs a shell command in one line:

```rust
let output = rigtest::ssh!(ctx, "deploy@staging.example.com", "systemctl status app").output().await?;
assert!(output.status.success());
```

An optional destination-aware configurator can be registered to customise
the connection — for example to accept self-signed host keys in a CI
environment or to select a non-default identity file:

```rust
fn configure_ssh(
    _destination: &str,
    mut builder: rigtest::openssh::SessionBuilder,
) -> Result<rigtest::openssh::SessionBuilder, rigtest::Error> {
    builder.known_hosts_check(rigtest::openssh::KnownHosts::Accept);
    Ok(builder)
}

#[rigtest::main(ssh_client = configure_ssh)]
fn main() {}
```

The configurator receives the destination string so different hosts can
receive different configuration. Omitting the configurator uses the
`openssh` defaults, which inherit your SSH agent and `~/.ssh/config`
automatically.

See [`examples/ssh-client`](examples/ssh-client) for a complete working
example. Set `SSH_HOST=user@yourhost` before running, or leave it unset
to default to `localhost`.

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

In a Jenkins pipeline, point the `junit` step at the file after the run:

```groovy
sh 'cargo rigtest run --reporter junit'
junit 'target/rigtest/junit.xml'
```

---

## Running tests

```
cargo rigtest run [OPTIONS]
```

| Flag | Description |
|------|-------------|
| `--jobs <N>` | Maximum parallel test jobs (default: number of CPUs) |
| `--seed <N>` | Fix the random order seed for reproducible runs |
| `--filter <STRING>` | Only run tests whose name contains STRING |
| `--test <NAME>` | Only run the named test target (repeatable: `--test a --test b`) |
| `--package <NAME>` | Package containing the test targets |
| `--no-capture` | Print test output in real time instead of capturing it (implies `--jobs 1`) |
| `--reporter <KIND>` | Additional reporter to run alongside the console. `junit` emits `target/rigtest/junit.xml` (see above) |

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

## Crate layout

| Crate | Description |
|-------|-------------|
| `cargo-rigtest` | The `cargo rigtest` CLI plugin |
| `rigtest` | Runtime library — add this to `[dev-dependencies]` |

---

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
