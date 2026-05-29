# cargo-rigtest

[![CI](https://github.com/anthonyoteri/cargo-rigtest/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/anthonyoteri/cargo-rigtest/actions/workflows/ci.yml)
[![rigtest on crates.io](https://img.shields.io/crates/v/rigtest.svg?label=rigtest)](https://crates.io/crates/rigtest)
[![cargo-rigtest on crates.io](https://img.shields.io/crates/v/cargo-rigtest.svg?label=cargo-rigtest)](https://crates.io/crates/cargo-rigtest)
[![docs.rs](https://img.shields.io/docsrs/rigtest?label=docs.rs)](https://docs.rs/rigtest)
[![MSRV: 1.87](https://img.shields.io/badge/rustc-1.87+-orange.svg)](https://blog.rust-lang.org/2025/05/15/Rust-1.87.0.html)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/anthonyoteri/cargo-rigtest#license)

A Cargo plugin for infrastructure and acceptance testing in Rust.

cargo-rigtest runs each test in its own subprocess, giving you process-level isolation, parallel execution, structured output, and first-class support for shared infrastructure setup — without the overhead of spinning up a full test harness.

---

## Overview

cargo-rigtest is designed for the layer of tests that sit above unit tests: API integration tests, service smoke tests, environment verification, end-to-end workflows. It is not a replacement for `#[test]` — it is a complement to it.

Key properties:

- Each test runs in its own subprocess — a panic or crash in one test cannot affect others
- Tests run in parallel by default, with a configurable concurrency limit
- A single `#[global_setup]` / `#[global_teardown]` pair provisions and tears down shared infrastructure once per suite
- Per-test setup and teardown hooks are available via `TestContext`
- Tests can be marked `serial`, given a `timeout`, or configured to `retry` on failure
- Output is captured per-test and printed only on failure, nextest-style
- A `rigtest::skip!` macro lets tests opt out at runtime with a reason

---

## Installation

Install the Cargo plugin:

```
cargo install cargo-rigtest
```

Add the runtime library to your project:

```toml
[dev-dependencies]
rigtest = "0.1"
```

### Optional features

| Feature | Description |
|---------|-------------|
| `http-client` | Adds a pre-built `reqwest::Client` as `ctx.client`, available to every test. Enable it if your tests make HTTP calls and you want a shared client without constructing one manually. |

```toml
[dev-dependencies]
rigtest = { version = "0.1", features = ["http-client"] }
```

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
    let state = ctx.global_data.downcast_ref::<State>().unwrap();
    // ctx.client requires the `http-client` feature
    let resp = ctx.client.get(&state.base_url).send().await?;
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

Registers an async function as a test case. The function must have this signature:

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

> **Note on timeout and teardown:** when a timeout fires the subprocess is hard-killed. Any
> teardown registered with `ctx.teardown(...)` will not run. Resources that must be
> released regardless of outcome should use RAII guards or `#[global_teardown]`.

### `#[global_setup]`

Runs once before any test in the suite. The return value is serialized and passed to each test subprocess via a temporary environment variable. At most one may be defined.

```rust
#[global_setup]
async fn setup() -> MyState {
    MyState { db_url: std::env::var("DATABASE_URL").unwrap() }
}
```

The return type must implement `serde::Serialize` and `serde::Deserialize` — it is serialized to JSON and forwarded to each test subprocess via a temporary environment variable. Store configuration values (URLs, ports, credentials) rather than live handles.

### `#[global_teardown]`

Runs once after all tests finish. Receives the value produced by `#[global_setup]`. At most one may be defined.

```rust
#[global_teardown]
async fn teardown(state: MyState) {
    // state is the deserialized form of what setup returned — connection
    // handles and other live resources cannot survive the round-trip.
    // Use the serializable fields (e.g. a URL or ID) to reconnect and clean up.
    MyDb::connect(&state.db_url).await.unwrap().drop_schema().await;
}
```

> **Note:** The global state type must implement `serde::Serialize` and
> `serde::Deserialize` because it is serialized to JSON and passed to each
> test subprocess via a temporary environment variable. Live resources such
> as connection pools, file descriptors, and socket handles cannot be
> serialized — store the configuration needed to recreate them instead (a
> URL, a path, a port number).

---

## Per-test setup and teardown

`TestContext` provides `setup` and `teardown` hooks for resources with a clear per-test lifecycle.

The `global` argument passed to both closures is the deserialized global state — the same value produced by `#[global_setup]` and subject to the same constraint: it contains only serializable configuration (URLs, ports, credentials), not live handles. Use it to *create* a live resource; the resource itself lives entirely within the test subprocess and is not subject to any serialization requirement.

```rust
#[testcase]
async fn creates_a_record(
    ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // `global` holds deserialized config — use it to open a live connection.
    // The returned `conn` is a live resource that exists only in this subprocess.
    let mut conn = ctx
        .setup(|global| async move {
            let state = global.downcast_ref::<State>().unwrap();
            db::connect(&state.db_url).await
        })
        .await?;  // failure reported as "setup failed: ..."

    conn.insert("hello").await?;
    assert_eq!(conn.count().await?, 1);

    // `conn` is moved into the teardown closure — still a live resource here.
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

The seed is printed at the start of every run so a failing order can be reproduced exactly:

```
cargo rigtest run --seed 12345678
```

---

## Output

cargo-rigtest produces nextest-style output. In a TTY, running tests show live spinners; results are printed as they complete:

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

If a package has more than one rigtest test target, all of them are discovered and run in sequence automatically:

```
cargo rigtest run                          # run all rigtest targets
cargo rigtest run --test smoke             # run one
cargo rigtest run --test smoke --test e2e  # run two
```

cargo-rigtest identifies rigtest test targets automatically and ignores any other `harness = false` binaries in the package.

---

## Crate layout

| Crate | Description |
|-------|-------------|
| `cargo-rigtest` | The `cargo rigtest` CLI plugin |
| `rigtest` | Runtime library — add this to `[dev-dependencies]` |

---

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
