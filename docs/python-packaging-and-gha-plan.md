# Python packaging and GitHub Action plan

Ship the `hydrust` CLI as an installable Python package, and give users a
supported way to run it in CI. Modelled on how ruff and ty do it
(`code_reference/ruff`, `code_reference/ruff-action`).

**Status legend:** `TODO` · `IN PROGRESS` · `DONE` · `DROPPED`

| Phase | Scope | Status |
| ----- | ----- | ------ |
| 1 | CLI rename, `check` subcommand, multi-path, `github` output format | DONE |
| 2 | `pyproject.toml`, maturin config, `python/hydrust/` shim | TODO |
| 3 | Wheel build + PyPI publish workflows | TODO |
| 4 | Documented CI snippet | TODO |
| 5 | `hydrust-action` (deferred) | TODO |

Phase 2 onward assumes the one-binary change in `server-subcommand-plan.md`,
which lands first: `hydrust check` and `hydrust server` on a single bin, and the
crate renamed to `hydrust`.

## Decisions

These were settled up front; the rest of the plan assumes them.

- **PyPI distribution name is `hydrust`.** `hydra-lsp` on PyPI is taken by an
  unrelated project (`Retsediv/hydra-lsp`). `hydrust`, `hydrust-check` and
  `hydra-check` were all free; `hydrust` is the intended final name, and the
  distribution name is the one thing that is expensive to change later.
- **Rename the binary and add the `check` subcommand before publishing.**
  Nothing is on PyPI yet, so this is free now and a breaking change later.
- **Extend the CLI rather than working around it in the action.** Multi-path
  input and a `github` output format live in the tool, so local runs and CI
  behave the same. ty does the same thing (`--output-format github`).
- **Document the CI snippet first; build the action afterwards.** Ruff's own
  docs lead with a plain `pip install ruff` workflow and treat `ruff-action` as
  the alternative; ty ships no action at all.
- **The wheel ships the language server too.** Settled later than the rest, in
  `server-subcommand-plan.md`: there is one binary, so `pip install hydrust`
  gives you `hydrust server` as well as `hydrust check`. Ruff and ty both do
  this. It is also what makes the VS Code extension's default
  `importStrategy: fromEnvironment` sound — a `hydrust` found on PATH is always
  server-capable. Building a CLI-only wheel would break that, so don't.

## What ruff and ty actually do

Findings from `code_reference/`, recorded so the reasoning survives.

**One binary, server as a subcommand.** Ruff has a single `ruff` bin with
`Server(ServerCommand)` in its subcommand enum (`crates/ruff/src/args.rs:151`);
ty has `Check` / `Server` / `Version` on one `ty` bin
(`crates/ty/src/args.rs:37`). `ruff-lsp` used to be a separate Python package
and was deprecated once `ruff server` shipped. Their direction of travel was
two artifacts to one.

**Packaging.** A root `pyproject.toml` with `build-backend = "maturin"`,
`bindings = "bin"`, `python-source = "python"` and `manifest-path` pointing at
the bin crate. No PyO3. `python/ruff/__init__.py` provides `find_ruff_bin()`
and `__main__.py` makes `python -m ruff` work.

**Build/publish.** `build-binaries.yml` runs one `PyO3/maturin-action` job per
target plus an sdist job that installs and smoke-tests the tarball.
`publish-pypi.yml` runs `uv publish` under PyPI trusted publishing. Both are
wired into cargo-dist via `dist-workspace.toml` (`build-local-artifacts =
false`, `local-artifacts-jobs`, `publish-jobs`) so binaries compile once.

**Action.** `ruff-action` is TypeScript bundled by esbuild into a committed
`dist/ruff-action/index.cjs`, `using: node24`. It downloads GitHub Release
archives (or Astral's CDN mirror), never PyPI. Of ~7,700 lines in `src/`, 6,577
are a generated checksum table; the rest is mostly an ndjson versions manifest,
version discovery from `pyproject.toml`/`requirements.txt`/`uv.lock`, and
historical artifact-naming compatibility. The reusable core is roughly 150
lines: detect platform, resolve version, download, tool-cache, `addPath`,
`exec`.

## Naming

The crate is renamed to `hydrust` in v0.5.0 (`server-subcommand-plan.md`), so
the distribution name, module name, crate name, bin name and release-archive
prefix are all one word from the first PyPI release. What still has to hold:

- The PyPI distribution name and the Python module name are both `hydrust` and
  never change. This is the one name that is expensive to change later.
- **The GitHub repository stays `m-lyon/hydra-lsp`.** Renaming it breaks the VS
  Code extension's release lookup, which does not follow redirects; see the
  decision in `server-subcommand-plan.md`.
- dist names archives after the **crate**, not the bin — confirmed against
  `hydra-lsp-vscode/src/common/constants.ts:84`. So archives are
  `hydrust-<target>.tar.xz` from v0.5.0 and `hydra-lsp-<target>.tar.xz` before
  it, and anything downloading them must key the prefix off the version. That is
  the pattern `ruff-action` uses (`semver.lte(version, "v0.4.10")` in
  `src/download/download-version.ts`) and what the extension does in
  `compatTable.ts`; phase 5 needs the same from day one.

## Phase 1 — CLI · DONE

Prerequisite for everything else.

- [x] `Cargo.toml`: rename `[[bin]] hydra-check` to `hydrust`.
- [x] `Cargo.toml`: add `[features] default = ["server"]` and
      `required-features = ["server"]` on the `hydra-lsp` bin, so wheels can
      build the CLI alone via `--no-default-features`. Verified:
      `cargo build --no-default-features --bins --message-format=json` emits
      only `hydrust`, the default build emits both.
- [x] `src/cli.rs`: `#[command(name = "hydrust")]`, move the existing arguments
      into a `Check` subcommand. Clean break — nothing is published.
- [x] Accept multiple paths: files and directories. Walk directories with
      `ignore::WalkBuilder` for `.yaml`/`.yml`, honouring `.gitignore` as ruff
      does. Aggregate exit codes across files: 0 clean, 1 diagnostics, 2 error.
- [x] Add `Github` to the `OutputFormat` enum, emitting
      `::error file=,line=,col=,endLine=,endColumn=,title=::` / `::warning ...`.
- [x] Adjust the pretty and compact formats for multi-file runs — both currently
      assume a single file.
- [x] Update the `hydra-check` references across `README.md`, `CHANGELOG.md`,
      `.github/copilot-instructions.md` and `Cargo.toml`. `tests/server_cli.rs`
      uses `CARGO_BIN_EXE_hydra-lsp` and is unaffected by the feature gate
      because `cargo test` builds with default features.
- [x] `tests/check_cli.rs`: 12 integration tests over the CLI surface CI depends
      on — exit codes, directory walking, `.gitignore`, path deduplication, the
      `github` annotation shape, and single-document JSON.

Decided during implementation, beyond what was planned above:

- **`--format` became `--output-format`**, matching ruff and ty. No alias:
  nothing is published, so there is nobody to stay compatible with.
- **`WalkBuilder::require_git(false)`.** By default `ignore` only applies
  `.gitignore` inside a git checkout, which would make the set of checked files
  depend on whether `.git` exists — wrong for a tarball or a container build.
- **Paths are printed relative to the current directory** in every format, not
  just `github`. GitHub silently drops an annotation whose `file=` is absolute,
  so this is a correctness requirement rather than cosmetics.
- **One `HydraDatabase` and `PythonConfig` across the whole run**, so module
  resolution done for the first file is reused by the rest.

Left for phase 2 at the time: verifying maturin's behaviour on a crate with two
`[[bin]]` targets. The docs only say bin bindings auto-detect "only if there is
only a binary target" and say nothing about the multi-bin case; the sandbox
blocked the throwaway test. **Resolved by deleting the second binary** —
`server-subcommand-plan.md` collapses both into one `hydrust` bin and removes
the `server` feature, so maturin's single-binary auto-detection applies. The
feature was an empty marker anyway: no `cfg(feature = "server")` ever existed.

## Phase 2 — Python package

Depends on `server-subcommand-plan.md` phase B, which supplies the single
`hydrust` bin and the crate rename.

- [ ] Root `pyproject.toml`: maturin backend, `name = "hydrust"`,
      `dynamic = ["version"]` so the version comes from `Cargo.toml` rather than
      drifting. Ruff hardcodes its version and maintains a bump script; not
      worth it here.
- [ ] `[tool.maturin]`: `bindings = "bin"`, `python-source = "python"`,
      `module-name = "hydrust"`, `strip = true`, and an `exclude` for test
      fixtures.
- [ ] `python/hydrust/__init__.py`: port `find_ruff_bin()` — the scripts-dir,
      user-scheme, `pip install --target` and pip-build-env lookups — looking
      for `hydrust`. No candidate list: nothing has been published under another
      name, and the `hydra-check` bin never shipped.
- [ ] `python/hydrust/__main__.py` so `python -m hydrust` works.
- [ ] Build with default features and confirm the wheel contains exactly one
      script, `hydrust`, which answers both `check` and `server`.
- [ ] Check the wheel size after stripping. Unstripped the two pre-merge
      binaries were ~16.7 MB and ~20.3 MB, but they overlap almost entirely —
      both link the same salsa/ruff/ty analysis code — so expect roughly the
      larger of the two rather than the sum, and less than that stripped.

## Phase 3 — Build and publish

- [ ] `build-wheels.yml`: an sdist job that installs the tarball and smoke-tests
      `hydrust check --help`, `hydrust server --help` and
      `python -m hydrust --help`, plus one `PyO3/maturin-action` job per target
      for the six targets already in `dist-workspace.toml`. The `server` line is
      what pins the invariant above: a wheel whose `hydrust` cannot serve breaks
      the VS Code extension's default configuration.
- [ ] `publish-pypi.yml`: `uv publish`, `environment: release`,
      `id-token: write`, PyPI trusted publishing.
- [ ] Trigger both on dist's existing tag pattern
      (`'**[0-9]+.[0-9]+.[0-9]+*'`), independent of the dist workflow.
- [ ] Out of repo: register `hydrust` on PyPI as a pending trusted publisher,
      and create the `release` GitHub environment.

Known risk: cross-compiling the ruff/salsa dependency tree for
`aarch64-unknown-linux-gnu` and `x86_64-unknown-linux-musl` inside
maturin-action's containers. Fallbacks are zig or a native ARM runner.

Deferred optimisation: running independently of dist compiles each target twice
per release. Folding the wheel build into dist via `build-local-artifacts =
false` + `local-artifacts-jobs` fixes that, at the cost of matching dist's
expected artifact names exactly. Worth doing once the CI cost is measured.

## Phase 4 — Documented CI snippet

- [ ] README section with a workflow using
      `uvx hydrust@<version> check --output-format github .`.

## Phase 5 — `hydrust-action` (deferred)

The repo exists at `m-lyon/hydrust-action` with only a LICENSE and README.
Build it once the CLI surface has settled, judged against the phase 4 baseline.
Shape, if it happens: the ~150-line core of `ruff-action` — platform detect,
version resolve, download the dist `.tar.xz` from GitHub Releases, tool-cache,
PATH, exec. Skip the mirror, the checksum table and the ndjson manifest. Keep
every name in one `constants.ts` and key the archive prefix off the version
from day one.
