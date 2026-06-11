# AGENTS.md

## Don't
- Add `Signed-off-by`. Humans only.
- Add `Co-Authored-By: Claude` or any AI attribution.
- Use `--no-verify`.
- Force-push to `main`.
- Bump versions or edit `CHANGELOG.md`. `cog bump` regenerates both.
- Run `cog bump` or trigger a release without explicit ask.
- Open PRs without `Closes #N`.

## Do
- Run `cargo fmt && cargo clippy --workspace --all-targets --all-features -- -W clippy::pedantic -D warnings` before push.
- Apply `AI Assisted` (human-authored, AI helped) or `AI Generated` (AI-authored, minimal human investment) when opening a PR.
- `git fetch && git pull origin main` before branching.
- Reference the issue in PR body (`Closes #N`).
