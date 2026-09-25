# Review instructions

hydrust is a language server and CLI checker (`hydrust check`) for Hydra YAML
configs, written in Rust and shipped as GitHub release archives and PyPI wheels.
It runs on a single developer's machine or in their CI, on their own code. The
hydrust-vscode extension is its main client and treats the capabilities block,
README and CHANGELOG as a contract. Reviews are fed into an automated fix loop,
so every finding you raise will probably be implemented. Raise only findings
that are worth the code they will add.

## Proportionality comes first

- Only flag defects that this diff introduces or makes worse. Do not audit
  surrounding code the diff did not touch.
- Weigh each finding against the complexity its fix would add. If a fix for an
  unlikely edge case would add more than a few lines, or a new flag, thread, or
  CI job, recommend documenting the limitation instead.
- When reviewing a commit that addresses earlier review findings, judge whether
  the fix is proportionate to the problem. Unnecessary complexity added by a fix
  is itself a finding, and simplifying or reverting it is a valid
  recommendation.
- Do not raise a new edge case inside code that exists only to handle another
  edge case, unless it causes a crash, wrong output, or a broken release.
- Do not repeat a finding that a comment on an earlier review has marked as
  intentional or won't-fix.
- When a finding depends on how a third-party library, tool, or service behaves
  (path resolution, a CLI flag, PyPI or GitHub Actions semantics), say so and
  name the source. Do not assert that behaviour from memory as fact.
- A review with no findings is a good outcome. Do not pad it.

## Out of scope

Do not flag:

- Attackers who are other local users, or malicious config files and Python
  sources. The user is analysing their own project.
- Formatting and lint issues that `cargo fmt` and `cargo clippy` catch.
- Missing tests for behaviour the diff does not claim, and hypothetical future
  changes ("someone might later add a `println!` here").
- Duplicated constants, unless they have already drifted apart.
- Release-readiness items that can only be checked on the next real release.
  Mention them in the summary instead of as findings.

## Worth flagging

These repeatedly turned out to be real:

- **Docs that contradict the code.** When a diff touches README, CHANGELOG or
  `docs/` alongside code, check each claim against the code. Flag a change to a
  documented output field, exit code or capability with no CHANGELOG entry.
  Comment and docstring mistakes are fine to report at Low.
- **Failures that come out green.** The CLI's exit code is its product. Flag an
  error or an empty input that still exits 0, and CI or release steps that pass
  when their check did not run: unexpanded globs, string-equality gates that
  skip silently, `upload-artifact` left at `if-no-files-found: warn`.
- **Output formats that disagree.** `pretty`, `compact`, `json` and `github`
  must derive shared values (severity, columns, codes) from one place, not
  re-derive them or parse another format's output.
- **Guarantees nothing tests.** Flag a missing test only when the diff states a
  guarantee (README or CHANGELOG, or a comment explaining a non-default
  setting). Also flag assertions that pass when the code under test does
  nothing, such as a substring found in unrelated help text or a count that
  holds at zero.
- **Release-only logic.** Workflow code that first runs on a real release (jq
  over the dist plan, tag and version checks, publishing) is risky because
  PyPI versions cannot be re-uploaded. Flag it when a pull request never
  exercises it.
- **Paths and platforms.** CI runs on Ubuntu only, but Windows and macOS are
  release targets. Watch for `canonicalize()` changing reported paths through
  symlinks, Windows `\\?\` prefixes breaking `strip_prefix`, and tests that need
  Unix-only filenames without `#[cfg(unix)]`.
- **Compatibility rules in AGENTS.md.** Never remove or repurpose a settings key
  or feature name quietly, and bump `HYDRUST_PROTOCOL_VERSION` only when an
  existing entry changes meaning.
