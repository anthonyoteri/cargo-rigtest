# 0004 — Retry matcher evaluated subprocess-side against typed error

The `retry_on_error` pattern attached to `#[testcase]` is evaluated inside
the test subprocess, against the user's typed `Err(_)` value, *before*
the error is boxed and serialized across the subprocess boundary. The
macro requires the test function to return `Result<(), ConcreteType>`
(a named error type) when `retry_on_error` is set, and emits a
compile-time error pointing at the signature when this constraint is
not met. The subprocess emits a `retry_eligible: bool` hint in the
failure variant of the wire protocol; the coordinator reads only that
boolean when deciding whether to retry.

## Considered alternatives

- **Trait-object match against `&dyn Error`** in the subprocess.
  Rejected because `dyn Error` does not allow pattern matching on
  variants; the only escape is `downcast_ref::<MyError>()` inside a
  guard, which defeats the purpose of "use `matches!` syntax."
- **String / regex match against the error's `Display` output**,
  evaluated coordinator-side. Rejected because typed pattern matching
  is what the syntax promised — degrading it to string matching keeps
  the syntax but loses every guarantee (variants, guards, exhaustiveness).
- **Serialize a structured error representation across the boundary**
  so the coordinator can match. Rejected because it expands the wire
  protocol from a boolean to a full error-type schema for a single
  decision-point (retry or not).
