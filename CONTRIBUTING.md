# Contributing to cargo-rigtest

Thank you for your interest in contributing.

## Getting started

1. Fork the repository and clone your fork.
2. Install a stable Rust toolchain (MSRV 1.87). `rustup` is recommended.
3. Build and test:

```
cargo test --workspace
```

4. Check lints and formatting:

```
cargo clippy --workspace --all-targets -- -W clippy::pedantic -D warnings
cargo fmt --all --check
```

## Making changes

- Open an issue before starting significant work so we can discuss the approach.
- Keep commits focused; one logical change per commit.
- Follow [Conventional Commits](https://www.conventionalcommits.org/) — this is enforced by CI.
  Common types: `feat`, `fix`, `refactor`, `docs`, `test`, `ci`, `chore`.
- Add or update tests for any behavior changes.
- All CI checks must pass before a PR can be merged.

## Commit style

```
feat: add --timeout flag to cargo rigtest run
fix: skip reason missing in --no-capture mode
docs: document global_data downcast pattern
```

Breaking changes must include a `!` after the type and a `BREAKING CHANGE:` footer:

```
feat!: rename ctx.state to ctx.global_data

BREAKING CHANGE: TestContext.state has been renamed to TestContext.global_data.
```

## Crate layout

| Crate | Purpose |
|-------|---------|
| `cargo-rigtest` | CLI plugin (`cargo rigtest run`) |
| `rigtest` | Runtime library — what users add to `[dev-dependencies]` |
| `rigtest-macros` | Proc macros (`#[testcase]`, `#[global_setup]`, `#[global_teardown]`) |

## Running the examples

```
cargo test -p calculator
cargo test -p rigtest-example-basic
```

## Releasing

Releases are managed with [cocogitto](https://docs.cocogitto.io/). The release workflow runs via GitHub Actions on `workflow_dispatch`. See `cog.toml` for configuration.

## AI-assisted contributions

AI tools (Claude, Copilot, Cursor, and similar) are welcome here. The only firm requirement is that a human is in the loop: someone has to shape the change, read every line, and stand behind it on review.

To keep the review queue honest, contributors self-categorize PRs with one of two repository labels. Either, both, or neither may apply:

- **`AI Assisted`** — you used an AI tool to help with the change, but the work is primarily yours. You drove the design, reviewed the output, and would defend the code in review on your own. Treated identically to any other PR.
- **`AI Generated`** — the change was substantially produced by an AI agent with minimal human shaping. These are evaluated honestly on their merits; expect a higher bar, since there's less human judgement baked in.

Disclosure is for transparency, not stigma. An honest `AI Generated` label is fine; what isn't fine is using AI heavily and not saying so. Undisclosed AI use that's discovered later will be treated as a process violation.

## License

By contributing, you agree that your contributions will be dual-licensed under MIT OR Apache-2.0, matching the project license.
