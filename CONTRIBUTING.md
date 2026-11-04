# Contributing to tierstore

Thank you for helping. This page is the short version; [AGENTS.md](AGENTS.md) holds the full rules every change is reviewed
against, and each rule there has an id (for example T1, R3) that reviewers cite.

By taking part you agree to the [Code of Conduct](CODE_OF_CONDUCT.md). The project is licensed under
[Apache 2.0](LICENSE), and your contribution is under the same licence.

## Report a problem or ask for a feature

Open an issue with the bug or feature template. To report a security problem, do not open an issue: use
[private vulnerability reporting](https://github.com/komira-ai/tierstore/security/advisories/new).

## Build and test

The toolchain is pinned in `rust-toolchain.toml`. From the repository root:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Workflow and `AGENTS.md` checks are tests of the `xtask` crate (`cargo test -p xtask`). Dependency policy is checked by
cargo-deny, as CI runs it: `cargo deny --all-features check bans licenses sources`. Advisories are not part of the pull request
check: they are reported separately and never block a pull request. See [docs/ci-and-release.md](docs/ci-and-release.md).

## Coverage

CI measures every crate of the workspace with `cargo-llvm-cov` and fails a crate below 99.00% of its lines or 99.00% of its
branches, with no exclusions. Do not hide code from measurement and do not lower a floor. Add tests that assert behaviour,
including failure and boundary cases, until the uncovered lines and branches are covered.

## Tests

- A bug fix comes with a test that fails without the fix.
- Tests are hermetic and deterministic: no network, no fixed ports or paths, no shared state, and no assertions on elapsed
  time (inject a clock or wait on an explicit event).
- Write tests in Rust, in the crate they test.

## Code

- No new `unsafe`, and no new `unwrap()` or `expect(` in production paths. Libraries use `thiserror` enums; `anyhow` is for
  binaries.
- Configuration is passed as flags or config keys, not environment variables. A new tunable number is a config key with a
  default.
- Stored and wire formats change additively only: a new optional field, never a rename, removal or new required field.
- Keep files under 1000 lines.
- Docs and comments describe only what the code does; change them in the same pull request as the behaviour.

## Commits and pull requests

- One subject per pull request. Split refactors from behaviour changes.
- Make a branch from `main` and open the pull request against `main`. Do not rewrite pushed history: add a new commit that
  says what it corrects, and bring in `main` with a merge.
- Commit messages cite issues as `Refs komira-ai/tierstore#<n>`, never `Fixes`, `Closes` or `Resolves`.
- Commit under your own identity. If a coding agent wrote part of a commit, end the message with a `Co-authored-by:` trailer
  naming it. A `Signed-off-by:` line (`git commit -s`) is welcome and is not required.
- The title says concretely what changes. The pull request template lists what the description needs: the user-facing
  change first, then the layers with the reason, compatibility, and each test by name with the behaviour it proves.
- Commit no secrets, endpoints, logs or scratch files.
