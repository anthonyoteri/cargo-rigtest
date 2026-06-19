# cargo-rigtest

[![CI](https://github.com/anthonyoteri/cargo-rigtest/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/anthonyoteri/cargo-rigtest/actions/workflows/ci.yml)
[![rigtest on crates.io](https://img.shields.io/crates/v/rigtest.svg?label=rigtest)](https://crates.io/crates/rigtest)
[![cargo-rigtest on crates.io](https://img.shields.io/crates/v/cargo-rigtest.svg?label=cargo-rigtest)](https://crates.io/crates/cargo-rigtest)
[![docs.rs](https://img.shields.io/docsrs/rigtest?label=docs.rs)](https://docs.rs/rigtest)
[![MSRV: 1.87](https://img.shields.io/badge/rustc-1.87+-orange.svg)](https://blog.rust-lang.org/2025/05/15/Rust-1.87.0.html)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

A Cargo plugin for infrastructure and acceptance testing in Rust.

cargo-rigtest runs each test in its own subprocess, giving you
process-level isolation, parallel execution, structured output, and
first-class support for shared infrastructure setup — written in Rust,
living inside your Cargo workspace, type-checked by the same compiler as
the rest of your code.

| Feature                          | cargo-test | cargo-nextest | cargo-rigtest |
|----------------------------------|:----------:|:-------------:|:-------------:|
| Process isolation per test       |     —      |       ✓       |       ✓       |
| Captured output on failure       |     —      |       ✓       |       ✓       |
| Per-test timeout / retries       |     —      |      ✓¹       |       ✓       |
| Tag-based test filtering         |     —      |       —       |       ✓       |
| Parametrized cases (`#[case]`)   |     —      |       —       |       ✓       |
| Global setup / teardown          |     —      |       —       |       ✓       |
| Per-test setup / teardown        |     —      |       —       |       ✓       |
| Preflight environment checks     |     —      |       —       |       ✓       |
| JUnit XML reporter               |     —      |      ✓¹       |       ✓       |
| Built-in HTTP client             |     —      |       —       |       ✓       |
| Built-in SSH client              |     —      |       —       |       ✓       |

¹ via config file

---

## Quick start

Install the CLI:

```
cargo install cargo-rigtest
```

Add the library to your project:

```toml
[dev-dependencies]
rigtest = "0.4"

[[test]]
name = "acceptance"
path = "tests/acceptance.rs"
harness = false
```

Write `tests/acceptance.rs`:

```rust
use std::sync::Arc;
use rigtest::prelude::*;

#[testcase]
async fn it_works(
    _ctx: Arc<TestContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    assert_eq!(2 + 2, 4);
    Ok(())
}

fn main() {
    rigtest::run_main();
}
```

Run:

```
cargo rigtest run
```

See the per-crate READMEs below for the full attribute reference, CLI
flags, JUnit output, and the HTTP/SSH client integrations.

---

## Crates

| Crate | Purpose | README |
|-------|---------|--------|
| [`rigtest`](https://crates.io/crates/rigtest) | Runtime library — add to `[dev-dependencies]`. Attributes, `TestContext`, HTTP/SSH clients. | [`rigtest/README.md`](rigtest/README.md) |
| [`cargo-rigtest`](https://crates.io/crates/cargo-rigtest) | The `cargo rigtest` CLI plugin. Install methods, command reference, CI integration. | [`cargo-rigtest/README.md`](cargo-rigtest/README.md) |
| [`rigtest-macros`](https://crates.io/crates/rigtest-macros) | Internal proc-macro crate. Re-exported through `rigtest` — depend on `rigtest` instead. | [`rigtest-macros/README.md`](rigtest-macros/README.md) |

Worked examples live in [`examples/`](examples).

---

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
