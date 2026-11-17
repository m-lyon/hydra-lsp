# Contributing to Hydrust

Thank you for your interest in Hydrust. This page explains how to send a change.

## Pick an issue

- Look at the [open issues](https://github.com/m-lyon/hydra-lsp/issues). Issues
  labelled `small` need the least work.
- Comment on the issue before you start, so two people do not do the same work.
- For a large change, or one with no issue, open an issue first and agree on the
  design.

## Send a change

1. Fork the repository and clone your fork.
2. Create a branch from `main`.
3. Make the change, and add or update the tests for it.
4. Run the checks below. CI runs the same checks.
5. Push the branch to your fork and open a pull request against `main`.

Outside contributors do not get write access. A maintainer must approve each CI
run on a pull request from a fork, so the checks start after a short wait.
Pull requests are squash-merged.

## Checks

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Many tests compare output with [insta](https://insta.rs/) snapshots. If you
change output on purpose, review the new snapshots with `cargo insta review`
(install it with `cargo install cargo-insta`). Do not accept snapshot changes you
did not expect. [tests/README.md](tests/README.md) explains the test layout.

## Before you open the pull request

- Add an entry under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md) for any
  change a user can see. Do not change the version in `Cargo.toml`. A maintainer
  bumps it when they cut a release.
- For a new settings key, diagnostic rule, or client-visible feature, follow
  **Adding a feature** in [AGENTS.md](AGENTS.md). The VSCode extension depends on
  these names, so the rules there also apply to you.
- Before you change thread counts or where a request handler runs, read
  [docs/threading-model.md](docs/threading-model.md).

## Cut a release

Maintainers only.

1. Choose the version. Bump the minor version if a change in `[Unreleased]` is
   breaking. Otherwise, bump the patch version.
2. In [CHANGELOG.md](CHANGELOG.md), rename `## [Unreleased]` to the new version,
   for example `## [0.7.1]`. Add a new, empty `## [Unreleased]` section above it.
3. Set the same version in `Cargo.toml`, then run `cargo check` to update
   `Cargo.lock`.
4. Commit these changes and merge them to `main`.
5. Tag the merge commit with the version and push the tag, for example
   `git tag v0.7.1 && git push origin v0.7.1`. The tag starts the release
   workflow.

## Security problems

Do not report a security problem in a public issue. See [SECURITY.md](SECURITY.md).
