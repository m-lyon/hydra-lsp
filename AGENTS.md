# Hydrust

## Common commands

```bash
# Test the project
cargo test

# Run linting checks
cargo clippy
```

## Adding a feature

**A settings key** — parse it in `initialize`, then add it to `CORE_SETTINGS` (or
to the `feature_toggles!` list for an on/off switch, which registers the key for
you). Update the counts in [tests/capabilities.rs](tests/capabilities.rs). In the
extension: declare it in `package.json`, send it from `startServer`, add a
`SETTING_COMPAT` entry.

**A diagnostic rule** — add it to `diagnostic_rules!` in
[src/diagnostics.rs](src/diagnostics.rs) and it is advertised automatically. In
the extension, add a `RULE_COMPAT` entry; for a *rename*, record the old code as
`previousCode` so the client can rewrite it for older servers, as
`invalid-target` → `invalid-hydra-parameter` did in v0.3.0.

**A behaviour the client must know about** — only needed when it depends on a
client capability, or the client has to branch on it. Read the client capability
in `initialize` and store the flag, add a field to `NegotiatedFeatures` and a
`(name, gate)` pair to `SUPPORTED_FEATURES`, and gate the behaviour on that same
flag so the advertised name is never a promise the session will not keep. Cover
it both ways in [tests/capabilities.rs](tests/capabilities.rs). In the extension,
add a `FEATURE_COMPAT` entry; names become `hydrust.supports.<name>` context keys.

Anything the server always does and can advertise through a standard LSP
capability field needs none of this.

### Rules

- Bump `HYDRUST_PROTOCOL_VERSION` only when something already in the block
  changes meaning: a key that starts doing something different, a repurposed
  feature name, a field that changes type. Additions never need a bump.
- Never remove or repurpose a name quietly — a client may still send it.
- Unknown settings keys stay ignored, never rejected.
- `features` is always an array, even when empty; clients test membership on it.
- Bump the crate version and add a CHANGELOG entry. The client's fallback table
  is keyed on release versions.

### Checking it

`cargo test --test capabilities` covers the block's shape. In the extension repo,
`npm run test:contract` checks the client against a real running binary, and
`npm run test:table-audit` re-verifies the fallback table against tagged sources.

## Threading

The server runs its analysis on two `rayon` pools — a latency pool for hover,
completion and semantic tokens, and a worker pool for diagnostics — alongside a
single-threaded tokio runtime that handles the protocol itself. The `numThreads`
setting is the total across all three, and defaults to a size the server picks to
fit the machine.

[docs/threading-model.md](docs/threading-model.md) is the reference for this:
where each request handler does its work, why the counts are what they are, and
which alternatives were tried and rejected. Read it before changing a thread
count, the concurrency level, or where a handler runs.
