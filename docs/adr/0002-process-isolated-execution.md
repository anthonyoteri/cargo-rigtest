# 0002 — Process-isolated execution with custom test harness

Each test runs in its own subprocess spawned from the same compiled
test binary. The binary uses `harness = false` so rigtest owns the
entry point and dispatches on a `--run-single` flag — the parent
process is the coordinator that schedules, the child process is the
single test that runs. This buys crash isolation, deterministic
per-test state, and clean OS-level resource teardown; the cost is
per-test process-spawn overhead and a non-standard `Cargo.toml`
requirement.

## Considered alternatives

- **Thread-per-test (libtest, nextest default).** Rejected because a
  panic in one test can corrupt thread-local or static state shared
  with other tests; OS-level resources (file handles, sockets, child
  processes) aren't reliably released between tests; and a segfault
  in one test takes the whole runner down.
- **`harness = true` with a libtest wrapper.** Rejected because
  libtest doesn't expose the lifecycle hooks rigtest needs:
  coordinator/subprocess split, the env-var state-handoff protocol,
  and parallel scheduling against a custom registry.
