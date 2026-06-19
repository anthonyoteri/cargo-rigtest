# 0001 — Preflight profile parameter

The `#[preflight]` macro accepts either `fn() -> Preflight` or
`fn(env: &str) -> Preflight`; the 1-arg form receives the active
profile name as `&str`. Pre-#47, the value comes from the
`RIGTEST_PROFILE` env var (defaulting to `""`); post-#47 the framework
supplies it directly.

## Considered alternatives

- **Profile-blind.** Users branch via `env::var(...)` from inside the
  function body. Rejected because the ergonomics are bad enough that
  every profile-aware preflight ends up with the same boilerplate.
- **Structured `&Profile` parameter.** Rejected because the profiles
  feature (#47) hasn't designed its type yet; committing to a shape
  now would be premature, and richer profile data is still accessible
  via free functions when it exists.
