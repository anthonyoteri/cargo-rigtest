# rigtest-macros

[![rigtest-macros on crates.io](https://img.shields.io/crates/v/rigtest-macros.svg?label=rigtest-macros)](https://crates.io/crates/rigtest-macros)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/anthonyoteri/cargo-rigtest#license)

Procedural macros (`#[testcase]`, `#[global_setup]`, `#[global_teardown]`,
`#[rigtest::main]`) for the
[`cargo-rigtest`](https://github.com/anthonyoteri/cargo-rigtest) test
framework.

This crate is an implementation detail. The macros it defines are
re-exported from the [`rigtest`](https://crates.io/crates/rigtest) crate,
which is what users should depend on. Depend on `rigtest-macros` directly
only if you are building tooling on top of the macros themselves.

Licensed under MIT OR Apache-2.0.
