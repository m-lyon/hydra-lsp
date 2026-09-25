# Review instructions

hydrust is a language server and CLI checker for Hydra YAML configs, written in Rust. It runs on a single developer's machine or in their CI, on their own code. The hydrust-vscode extension is its main client and treats the capabilities block, README and CHANGELOG as a contract. Reviews are often fed into an automated fix loop, so every finding you raise will probably be implemented. Raise only findings that are worth the code they will add.

## Proportionality comes first

- Weigh each finding against the complexity its fix would add. If a fix for an unlikely edge case would add more than a few lines, or a new flag, thread, or CI job, recommend documenting the limitation instead.
- When reviewing a commit that addresses earlier review findings, judge whether the fix is proportionate to the problem. Unnecessary complexity added by a fix is itself a finding, and simplifying or reverting it is a valid recommendation.
- Do not repeat a finding that a comment on an earlier review has marked as intentional or won't-fix.
- When a finding depends on how a third-party library, tool, or service behaves (path resolution, a CLI flag, PyPI or GitHub Actions semantics), say so and name the source. Do not assert that behaviour from memory as fact.
- A review with no findings is a good outcome. Do not pad it.

## Out of scope

Do not flag:

- Attackers who are other local users, or malicious config files and Python sources. The user is analysing their own project.
- Missing tests for behaviour the diff does not claim, and hypothetical future changes ("someone might later add a `println!` here").
