# cargo-rigtest

Acceptance-test framework for Rust. Terms below are canonical — code, docs,
and operator-facing output should use these words and nothing else when they
apply.

Sections and the terms within them are listed alphabetically.

## Language

**Configurator**:
A user-supplied function registered via
`#[rigtest::main(http_client = …)]` or `#[rigtest::main(ssh_client = …)]`
that customises the framework's HTTP or SSH client before use. Reused
by `http` and `ssh` probes so a passing probe predicts that real tests
can connect with the same configuration.
_Avoid_: client builder, customiser, hook.

**Coordinator**:
The parent rigtest process that loads the registry, runs preflight,
schedules tests, and spawns subprocesses. Holds global setup state
between tests and runs global teardown.
_Avoid_: parent, runner, harness, scheduler.

**Preflight**:
The phase that runs once in the coordinator, before any test subprocess
is spawned, to verify the suite's declared external dependencies. Also
the name of the attribute (`#[preflight]`) and the output section header
that reports its results.
_Avoid_: setup, health check, readiness check, smoke test.

**Primitive**:
The kind of a probe. The six v1 primitives are `http`, `tcp`, `ssh`,
`dns`, `env`, and `custom`. Each is a distinct builder method on
`Preflight`.
_Avoid_: probe type, check kind, family.

**Probe**:
A single check declared inside a `Preflight` builder. A probe either
**passes** or **fails** — never "reaches" or "succeeds."
_Avoid_: test, assertion, check, dependency, reachability test.

**Subprocess**:
A child process spawned by the coordinator to run exactly one test.
Crash isolation, deterministic per-test state, and OS-level resource
teardown are direct consequences of running each test in its own
subprocess.
_Avoid_: worker, child, slave, test process.
