# rigtest

[![rigtest on crates.io](https://img.shields.io/crates/v/rigtest.svg?label=rigtest)](https://crates.io/crates/rigtest)
[![docs.rs](https://img.shields.io/docsrs/rigtest?label=docs.rs)](https://docs.rs/rigtest)
[![MSRV: 1.87](https://img.shields.io/badge/rustc-1.87+-orange.svg)](https://blog.rust-lang.org/2025/05/15/Rust-1.87.0.html)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/anthonyoteri/cargo-rigtest#license)

Runtime library for [`cargo-rigtest`](https://crates.io/crates/cargo-rigtest),
a Cargo plugin for infrastructure and acceptance testing in Rust.

`rigtest` is the `[dev-dependencies]` half of the framework. It provides the
attributes (`#[testcase]`, `#[global_setup]`, `#[global_teardown]`), the
`TestContext` runtime, the optional HTTP and SSH clients, and the
`run_main` entry point that your test binary calls. You run those tests
with the `cargo rigtest` CLI, distributed as the separate
[`cargo-rigtest`](https://crates.io/crates/cargo-rigtest) crate.

---

## Add to your project

```toml
[dev-dependencies]
rigtest = "0.4"
```

If your tests make HTTP calls, enable the `http-client` feature to get a
shared `reqwest::Client` via `ctx.client().await?` in every test:

```toml
[dev-dependencies]
rigtest = { version = "0.4", features = ["http-client"] }
```

You can also make HTTP calls without this feature — just bring your own
client library and construct it in your tests.

In `Cargo.toml`, declare the test target with `harness = false` so rigtest
owns the binary's `main`:

```toml
[[test]]
name = "acceptance"
path = "tests/acceptance.rs"
harness = false
```

---

## A first test

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

Run it with `cargo rigtest run` (see the
[`cargo-rigtest`](https://crates.io/crates/cargo-rigtest) crate for the
CLI).

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
| `tags = ["smoke", …]` | Attach string tags for use with the `--tag` / `--not-tag` CLI filters |

> **Note on timeout and teardown:** when a timeout fires, the subprocess is
> terminated — on Linux and macOS a graceful signal is sent first, with a
> short window for the process to exit cleanly before a hard kill follows;
> on Windows the process is hard-killed immediately. Either way, teardown
> registered with `ctx.teardown(...)` will not run. Resources that must be
> released regardless of outcome should be handled in
> `#[global_teardown]`, which runs outside the test subprocess.

#### Parametrized cases (`#[case]`)

A test can be expanded into a table of cases by stacking one or more
`#[case(...)]` attributes above the function and tagging the parameters
that vary per row with `#[case]`. Each row is registered as its own
`TestCase`, runs in its own subprocess, and shows up as a distinct row
in the runner output and the JUnit report.

```rust
#[testcase]
#[case("alice", "admin")]
#[case::viewer("bob", "viewer")]
async fn user_has_expected_role(
    _ctx: Arc<TestContext>,
    #[case] user: &str,
    #[case] expected_role: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // ...
    Ok(())
}
```

The example above registers two tests, named
`user_has_expected_role::case_1` and
`user_has_expected_role::case_2_viewer`. Unlabelled rows use the
`case_<N>` form; the optional `#[case::label(...)]` form appends
`_<label>` for readability. The `<N>` prefix is always present so
duplicate labels (or duplicate values) can never collide.

All flags on `#[testcase]` (`serial`, `timeout`, `retries`, `tags`)
apply identically to every generated row. Non-`#[case]` parameters
(typically `ctx: Arc<TestContext>`) are wired in as usual; only
`#[case]`-tagged parameters receive per-row values.

#### Tags

Tag a test with one or more string labels, then subset the suite at the
command line via the `cargo rigtest run --tag` / `--not-tag` flags.
Pre-promotion smoke runs, nightly regressions, and "skip the slow stuff"
become declarative rather than name-pattern hacks.

```rust
#[testcase(tags = ["smoke"])]
async fn login_works(_ctx: Arc<TestContext>) -> Result<(), rigtest::Error> { Ok(()) }

#[testcase(tags = ["smoke", "regression"])]
async fn checkout_completes(_ctx: Arc<TestContext>) -> Result<(), rigtest::Error> { Ok(()) }

#[testcase(tags = ["slow"])]
async fn full_migration_replay(_ctx: Arc<TestContext>) -> Result<(), rigtest::Error> { Ok(()) }
```

See the [`cargo-rigtest`](https://crates.io/crates/cargo-rigtest) crate
for the matching CLI flags and how they compose with `--filter`.

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
via `ctx.client().await?`. See
[`examples/http-client`](https://github.com/anthonyoteri/cargo-rigtest/tree/main/examples/http-client)
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
rigtest = { version = "0.4", features = ["ssh-client"] }
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

See
[`examples/ssh-client`](https://github.com/anthonyoteri/cargo-rigtest/tree/main/examples/ssh-client)
for a complete working example. Set `SSH_HOST=user@yourhost` before
running, or leave it unset to default to `localhost`.

---

## License

Licensed under either of [Apache License, Version 2.0][apache] or
[MIT license][mit] at your option.

[apache]: https://github.com/anthonyoteri/cargo-rigtest/blob/main/LICENSE-APACHE
[mit]: https://github.com/anthonyoteri/cargo-rigtest/blob/main/LICENSE-MIT
