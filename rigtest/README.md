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
| `retries = <N>` | Retry a failing test up to N additional times before reporting failure. A test that fails on one or more attempts but ultimately passes is rendered as `FLAKY` in the console (and counted in the `(N flaky)` summary parenthetical) — the run still exits `0`. |
| `retry_on_error = <pat>` | Only retry when the typed `Err(_)` matches the pattern (same syntax as `matches!`); requires a concrete error type — see below |
| `tags = ["smoke", …]` | Attach string tags for use with the `--tag` / `--not-tag` CLI filters |

The `retry_on_error` matcher pattern-matches the test's typed `Err(_)`
value with the same syntax as the standard library's `matches!` macro —
including alternatives with `|` and `if` guards. When a matcher is set
the test must return `Result<(), ConcreteType>` (a named error type, not
`Box<dyn Error + Send + Sync>` / `rigtest::Error`); the compiler enforces
this and emits a message pointing at the signature. Panics, timeouts,
and subprocess kills are never retried when a matcher is in force.

```rust
#[derive(Debug)]
enum MyError {
    Network(String),
    Timeout,
    Assertion(String),
}
// impl std::fmt::Display and std::error::Error ...

#[testcase(retries = 3, retry_on_error = MyError::Network(_) | MyError::Timeout)]
async fn deploys_eventually(_ctx: Arc<TestContext>) -> Result<(), MyError> {
    // Retried up to 3 times on Network or Timeout; assertion failures
    // surface immediately on the first attempt.
    Ok(())
}
```

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

### `#[preflight]`

Runs once in the coordinator, before `#[global_setup]` and before any test
subprocess is spawned, to verify that the external dependencies the suite
declares are available. Each declared probe either **passes** or
**fails**; if any probe fails the coordinator prints a readiness table,
exits with status `2`, and skips both `#[global_setup]` and
`#[global_teardown]`.

At most one `#[preflight]` may be defined per test binary.

```rust
use rigtest::Preflight;
use std::time::Duration;

#[rigtest::preflight]
fn preflight() -> Preflight {
    Preflight::new()
        .tcp("api", "127.0.0.1:8080")
        .timeout(Duration::from_millis(500))
        .env("home_is_set", "HOME")
        .dns("api_dns", "example.com")
}
```

`#[preflight]` accepts two signatures:

```rust
fn checks() -> Preflight { /* ... */ }
fn checks(env: &str) -> Preflight { /* ... */ }
```

In the 1-arg form the framework supplies the active profile name as the
`&str` argument so a single declaration can branch on environment:

```rust
#[rigtest::preflight]
fn checks(env: &str) -> Preflight {
    match env {
        "prod" => Preflight::new().http("api", "https://api.prod.example.com/health"),
        _ => Preflight::new().http("api", "https://api.staging.example.com/health"),
    }
}
```

The profile is sourced from the `RIGTEST_PROFILE` environment variable,
defaulting to the empty string when unset. The parameter type must be
exactly `&str` — `String`, `&String`, `Cow<'_, str>`, more than one
parameter, `async fn`, and return types other than `Preflight` are
rejected at compile time with an actionable message.

#### Probe primitives

| Primitive | Builder method | Default timeout | Passes when |
|-----------|----------------|-----------------|-------------|
| TCP | `Preflight::tcp(name, "host:port")` | 30 seconds | A TCP connection to the target establishes within the timeout |
| Env | `Preflight::env(name, "VAR")` | n/a (synchronous) | The variable is set and non-empty (default) — or, with `.equals(value)`, equals `value` exactly |
| DNS | `Preflight::dns(name, "host")` | 30 seconds | The host resolves to at least one A or AAAA record. `host` is a bare DNS name — no port |
| HTTP¹ | `Preflight::http(name, "url")` | 30 seconds | A `GET url` returns a status in `200..=299` (default) — or in the range/value supplied via `.expect_status(...)` |
| SSH²  | `Preflight::ssh(name, "dest")` | 30 seconds | An SSH session establishes and the remote command (default `true`, override with `.command("...")`) exits 0 |
| Custom | `Preflight::custom(name, \|\| async { ... })` | none (no framework-imposed deadline) | The async closure resolves to `Ok(())` |

¹ Requires the `http-client` feature. The probe reuses the user's
`#[rigtest::main(http_client = …)]` configurator when present so a
passing probe predicts the same client configuration the live tests
will use. A configurator returning `Err` fails only that probe — other
probes still run.

² Requires the `ssh-client` feature and a Unix target. Same configurator
reuse and failure semantics as HTTP, using
`#[rigtest::main(ssh_client = …)]`.

Every builder method accepts both string literals and owned strings
(`String`, `format!(...)`, `Cow<'static, str>`) — the signature is
`impl Into<Cow<'static, str>>`. Literals stay zero-allocation
(`Cow::Borrowed`); owned strings cross over as `Cow::Owned` so probe
targets can be constructed dynamically without manual lifetime juggling.

#### Chained adjustments

`.timeout(d)` overrides the per-probe timeout of the most recently added
probe. For asynchronous probes (`tcp`, `dns`, `http`, `ssh`, `custom`)
the override controls the connect/resolve/request/exec deadline; for env
probes the value is recorded but not observed (the check is synchronous).

`.equals(value)` upgrades the most recently added env probe from "set and
non-empty" to "equals `value` exactly". No-op on probes of any other
kind.

`.expect_status(s)` overrides the acceptable HTTP status of the most
recently added HTTP probe. Accepts either a single `u16` (`.expect_status(204)`)
or an inclusive range (`.expect_status(200..=204)`). No-op on probes of
any other kind.

`.command(s)` overrides the remote command run by the most recently added
SSH probe. The probe still requires exit status 0; only the command run
on the remote host changes. No-op on probes of any other kind.

#### Auto-disambiguation

Probe names are resolved through a four-tier scheme so the same `name`
can appear more than once when it remains unambiguous in context. The
resolved name shows up in both the human readiness table and the JUnit
`<testcase name=...>` attribute:

- **Tier 1** — name unique → use `name` verbatim.
- **Tier 2** — same name across different probe *types* → `name(type)`
  (e.g. `api(tcp)` vs `api(http)`).
- **Tier 3** — same name within the same type → `name(type[target])`
  using the probe's natural target (`host:port` for TCP, `host` for
  DNS, `url` for HTTP, `dest` for SSH, variable name for env).
- **Tier 4** — name, type, and target all identical → genuine
  duplicate, reported as a startup error before any probe runs.

`custom` probes have no inspectable target — colliding `custom` names
must be renamed; the framework reports that explicitly.

> **Skipping preflight.** Pass `--no-preflight` to `cargo rigtest run` to
> skip the entire phase for one run. This is intended for local debugging,
> not CI: preflight exists specifically to catch missing environment
> dependencies *before* tests run.

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

An optional destination-aware configurator can be registered to customize
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
