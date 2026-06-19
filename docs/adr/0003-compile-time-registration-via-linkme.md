# 0003 — Compile-time test registration via `linkme` distributed slices

Tests, global setup, global teardown, and client configurators are
registered via `linkme::distributed_slice` static slices populated by
the proc macros. Test binaries declare `#[rigtest::main] fn main() {}`
— the macro generates the coordinator entry point — and the
coordinator iterates the slices at startup to discover everything.
The author never lists their tests in a registration call.

## Considered alternatives

- **`inventory` crate.** Smaller API but relies on `ctor`-style
  initialization tricks that have rough edges on some platforms.
  `linkme` integrates with the linker directly and is supported on
  every platform rigtest already targets
  (Linux/macOS/Windows × stable/nightly/MSRV).
- **Runtime registration inside the user's `main()`.** Would force
  every test binary to enumerate its tests by name. The current
  `#[rigtest::main] fn main() {}` is one line of boilerplate
  independent of how many tests the binary has.
