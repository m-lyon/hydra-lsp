# One `hydrust` binary

Collapse the two binaries into one — `hydrust check` and `hydrust server` — and
rename the crate to match, so the bin, the crate, the release archives and the
PyPI distribution all agree. Revisits the "out of scope" call recorded in
`python-packaging-and-gha-plan.md`; that plan's phase 2 has been updated to
depend on this one rather than on the `--no-default-features` wheel build.

Spans two repositories: `hydra-lsp` (server) and `hydra-lsp-vscode` (client).
The **repository** names do not change; see Decisions.

**Status legend:** `TODO` · `IN PROGRESS` · `DONE` · `DROPPED`

| Phase | Repo | Scope | Status |
| ----- | ---- | ----- | ------ |
| A | vscode | Version-keyed archive and executable names, `server` arg, PATH candidates | DONE |
| B | hydra-lsp | `hydrust server`, crate rename, delete the `server` feature | TODO |

Phase A ships first and works against every server released so far, so there is
no window in which client and server disagree.

## Why revisit

The `server` feature (`Cargo.toml:26-28`) is an empty marker: no
`cfg(feature = "server")` exists anywhere in `src/`, and `src/lib.rs` compiles
`backend` unconditionally. `--no-default-features` therefore drops no
dependency and saves no compile time. Its only two jobs were to give maturin a
single `[[bin]]` to auto-detect (the open question left at the end of
`python-packaging-and-gha-plan.md`'s phase 1) and to keep the second binary out
of the wheel. One binary makes the first disappear and turns the second into a
different question — should `pip install hydrust` include the language server —
which ruff and ty both answer yes.

Answering yes is what makes the extension's default `importStrategy:
fromEnvironment` sound: a `hydrust` found on PATH is always server-capable.

## Decisions

- **`args: ['server']` is unconditional.** No version branch, so a wrong
  pre-launch version guess can never stop the server starting. Every released
  server tolerates it: pre-0.4.0 never parses argv at all (`git show
  v0.3.0:src/main.rs`), and v0.4.0 falls through to the `_ =>` arm in
  `handle_args` (`src/main.rs:41`). Verified by running it — exit 0, stdout
  clean, one stderr note.
- **Rename the crate to `hydrust` in the same release.** dist names archives
  after the crate, so v0.5.0 ships `hydrust-<target>.tar.xz`. That is what makes
  the change degrade gracefully for an extension that has not updated: it scans
  for an asset it recognises and skips any release without one
  (`download.ts:218-229`), so it quietly stays on v0.4.0 and keeps working
  instead of failing. It also merges two breaking changes into one — a rename
  was already anticipated (`python-packaging-and-gha-plan.md`, "Naming") — and the
  code cost is small: 21 references to `hydra_lsp` across 7 files, one of which
  (`src/main.rs`) is being deleted anyway.
- **Do not rename the GitHub repository.** `getLatestVersion` hits
  `/repos/m-lyon/hydra-lsp/releases` with a bare `https.get` that does not
  follow redirects (unlike `downloadFile`, which handles 301/302 at
  `download.ts:44-49`). A repo rename returns a 301 whose body is not a JSON
  array, so every client's `latest` resolution would break at once — old and
  new. Separate change, separate release, and it needs redirect handling in the
  client first.
- **No deprecated `hydra-lsp` alias binary.** It would have to be a second
  `[[bin]]`, which means a new feature flag plus dist config to keep it out of
  the wheel — reintroducing exactly what this plan deletes. The crate rename
  covers the same exposure for free.
- **Publish phase A and let it propagate before tagging server v0.5.0.** The
  extension is published to the marketplace
  (`hydra-lsp-vscode/.github/workflows/release.yml`), where auto-update is on by
  default. Cheap insurance on top of the graceful path above.
- **The capability handshake does not change.** See below.
- **`serverInfo.name` becomes `"hydrust"`**, and nothing may key identity off
  it. Identity questions belong to `capabilities.experimental.hydrust`.

## Compatibility: what changes and what must not

### Server handshake — no change

- **Do not bump `HYDRUST_PROTOCOL_VERSION`** (`src/backend.rs:156`). Its own
  doc comment (`src/backend.rs:146-155`) says to bump only when a field already
  in the block changes meaning. The rename changes no field's meaning, and
  clients that never look at the executable name are unaffected.
- `SUPPORTED_SETTINGS`, `DiagnosticRule::all_codes()` and `SUPPORTED_FEATURES`
  are all untouched by the rename.
- **Do not add an "invocation" field to the block.** The block is only readable
  from an `InitializeResult`, which only exists once the binary has already been
  launched correctly. Everything the rename breaks is decided before that.
- `serverInfo.name` (`src/backend.rs:1037`) changes `"hydra-lsp"` → `"hydrust"`.
  Safe: the client reads only `serverInfo.version` (`compatTable.ts:358-362`).

### Client compat table — one new table

The existing tables (`SETTING_COMPAT`, `RULE_COMPAT`, `FEATURE_COMPAT`) all
answer "what does this server understand", sourced from the handshake with the
version table as fallback. The rename raises a question none of them can answer:
**what do I download and execute**. That is pre-launch, so it can only come from
a version table, and it deserves to be a first-class one rather than an `if`
buried in `constants.ts`.

Add to `compatTable.ts`, with the same evidence-comment convention:

```ts
/**
 * First release in which the crate, the binary and the release archive are all
 * called `hydrust`, and the language server is `hydrust server`.
 */
export const UNIFIED_BINARY_VERSION: ServerVersion = ver(0, 5, 0);

/** Archive and executable basenames for a given server version. */
export function archiveName(version: ServerVersion | undefined): string;
export function serverExecutableName(version: ServerVersion | undefined): string;
```

Rules:

- `undefined` or `< v0.5.0` → `hydra-lsp` for both. Unknown means old: every
  archive up to v0.4.0 is named `hydra-lsp-<target>` and contains a `hydra-lsp`
  executable, confirmed against the v0.4.0 `dist-manifest.json`.
- `>= v0.5.0` → `hydrust` for both.
- Arguments are always `['server']`, per the decision above. Record why next to
  the constant, because it looks like an oversight otherwise.

This is the `semver.lte` shape `ruff-action` uses for the same problem
(`python-packaging-and-gha-plan.md`, "Naming").

### Client: `BINARY_NAME` is three different things

`constants.ts:8` currently feeds three consumers that stop agreeing after the
rename. Split it:

| New name | Kind | Consumers |
| -------- | ---- | --------- |
| `archiveName(version)` | version-keyed | `getArchiveDirectoryName` (`constants.ts:85`), `getDownloadUrl` (`constants.ts:68`) |
| `serverExecutableName(version)` | version-keyed | `getExecutablePath` (`constants.ts:105`) |
| `PATH_CANDIDATES` | `['hydrust', 'hydra-lsp']` | the `which` call in `server.ts:46` |
| `DISPLAY_NAME` | `'hydrust'` | log lines in `compat.ts` and `compatTable.ts` |

`getPlatformInfo()` currently bakes `executableName` into its return value with
no version to hand, and `getArchiveDirectoryName` takes only a `PlatformInfo`.
Both need the version threaded in. Every caller that matters already has a
concrete resolved tag, never `'latest'` — `ensureServer` (`download.ts:294-304,
427`) and `findExistingExecutable` (`download.ts:477`) — so the lookup is always
answerable.

**The one place with no version to key off** is `getLatestVersion`
(`download.ts:187`), which is scanning *for* the version. It must accept either
asset name while scanning, then derive the download URL from the tag it
resolved. Deriving rather than remembering which name matched keeps one source
of truth, at the cost of trusting the table — acceptable, since the table
decides what a given tag is called by definition.

The per-version cache layout keeps working across the cutover unchanged:
`bundled/libs/0.4.0/hydra-lsp-<target>/hydra-lsp` and
`bundled/libs/0.5.0/hydrust-<target>/hydrust` coexist, because
`findExistingExecutable` resolves both names per directory entry.

`PATH_CANDIDATES` prefers the new name and falls back to the old one. It needs
two entries where `find_hydrust_bin()` needs only `hydrust`, because the
extension has to keep working with a `hydra-lsp` a user installed before the
rename, whereas nothing has ever been published to PyPI.

### The invariant that keeps PATH lookup sound

> There is exactly one `hydrust` binary and it always has the `server`
> subcommand.

`importStrategy` defaults to `fromEnvironment` (`package.json:46`) and
`server.ts:44-54` only falls through to the bundled binary when `which` finds
nothing — a `hydrust` on PATH that cannot serve would be an unrecoverable
startup failure, not a fallback. Deleting the `server` feature is what
guarantees this. Anyone reintroducing a CLI-only build breaks the extension's
default configuration; say so in a comment where the feature used to be, and
pin it with the `hydrust server --help` smoke test in
`python-packaging-and-gha-plan.md` phase 3.

## Phase A — VS Code extension

Ships first. Forward-compatible with every existing server, so it can be
released and adopted before the server changes.

- [x] `compatTable.ts`: add `UNIFIED_BINARY_VERSION`, `archiveName()` and
      `serverExecutableName()` with evidence comments. Also carries
      `DISPLAY_NAME`, `LEGACY_BINARY_NAME`, `BINARY_NAME_CANDIDATES` and
      `SERVER_ARGS`, so nothing about naming lives in `constants.ts` any more
      and the two files do not have to import each other.
- [ ] `compatTable.ts:39`: the docstring guessed `--version` prints
      `'hydrust-server 0.4.0'`. The guess is gone, but the real string is still
      to be filled in once phase B settles it.
- [x] `constants.ts`: retire `BINARY_NAME` per the table above; thread `version`
      through `getArchiveDirectoryName`, `getDownloadUrl`, `getChecksumUrl` and
      `getExecutablePath`; drop `executableName` from `PlatformInfo`. It gains
      `executableSuffix` instead ('.exe' or nothing), which is the only part of
      the executable name that is a platform question rather than a version one.
- [x] `download.ts:187-229`: accept either asset name while scanning releases,
      and report which tag was chosen in the failure message so a mismatch is
      diagnosable. A matched asset whose name disagrees with what the table
      derives for that tag is logged as a warning, since the download that
      follows is about to 404.
- [x] `server.ts:112-118`: `args: ['server']`, with a comment recording why it
      is safe unconditionally.
- [x] `server.ts:46`: search `PATH_CANDIDATES` in order.
- [x] `package.json:47-56`: `importStrategy` descriptions say "hydra-lsp
      executable"; the `disabledRules` markdown at `:78` cites hydra-lsp
      versions. Both are user-visible. Every other `requires hydra-lsp vX.Y.Z`
      note had the same problem and was changed with them.
- [x] `test/oob/contract.test.ts:46`: hardcodes `target/debug/hydra-lsp`; accept
      either binary and pass `server` when it is `hydrust`. `ci.yml` pinned
      `$HYDRA_LSP_BINARY` to the same hardcoded path, so it no longer sets it
      and lets the suite find either name.
- [x] Unit tests: `archiveName`/`serverExecutableName` at v0.4.0, v0.5.0 and
      undefined; `getExecutablePath` over a mixed `bundled/libs` tree holding a
      0.4.0 and a 0.5.0 install; `findExistingExecutable` picking the newest
      across that mix; `getLatestVersion` choosing a `hydrust-`-named release
      over an older `hydra-lsp-`-named one, and vice versa when only the old one
      exists.

## Phase B — Server

### Subcommand

- [ ] Move `serve()` out of `src/main.rs` into the library (`src/server.rs`,
      re-exported), so a bin is a one-line call. It owns its own
      `tracing_subscriber` init writing to stderr with ANSI off — `hydrust
      check` initialises tracing differently (`src/cli.rs`, ANSI on, verbosity
      from `--verbosity`), and only one init can win per process, so each
      subcommand must do its own.
- [ ] `src/cli.rs`: add `Server(ServerCommand)` to the `Command` enum.
- [ ] Preserve today's tolerance of unrecognised arguments
      (`src/main.rs:22-28`: an editor may append its own transport flag such as
      `--stdio`, and refusing to start is worse than ignoring it). A bare clap
      subcommand would exit 2 instead, so `ServerCommand` needs a hidden
      `trailing_var_arg` + `allow_hyphen_values` catch-all that is ignored, and
      the note about it goes to stderr, never stdout.
- [ ] Delete `[[bin]] hydra-lsp`, `src/main.rs`, `[features]` and
      `required-features` from `Cargo.toml`, along with the comment at
      `Cargo.toml:14-16`. Replace with a comment recording the PATH invariant
      above.
- [ ] `src/backend.rs:1037`: `serverInfo.name` → `"hydrust"`.

### Crate rename

- [ ] `Cargo.toml:2`: `name = "hydrust"`. Leave `repository` pointing at
      `m-lyon/hydra-lsp`.
- [ ] `Cargo.toml:10-12`: the `[lib]` section can go entirely — with the package
      named `hydrust`, both `name` and `path = "src/lib.rs"` are the defaults.
      Keep the explicit `[[bin]] name = "hydrust", path = "src/cli.rs"`, since
      the bin source is not `src/main.rs`.
- [ ] `use hydra_lsp::` → `use hydrust::` — 21 references across
      `src/cli.rs` and `tests/{capabilities,semantic_tokens,encoding_failure_modes,
      textrange_conversion,common/mod}.rs`.
- [ ] Re-run `dist generate` and confirm `.github/workflows/release.yml` still
      builds; neither workflow currently hardcodes the name.
- [ ] Confirm the v0.5.0 `dist-manifest.json` lists exactly one executable named
      `hydrust` per `hydrust-<target>` archive. This is the artefact phase A's
      tables are written against.

### Tests and docs

- [ ] `tests/server_cli.rs`: `CARGO_BIN_EXE_hydra-lsp` → `CARGO_BIN_EXE_hydrust`
      with a `server` argument; the assertions at `:33` and `:53` expect
      `hydra-lsp X.Y.Z` and `Usage: hydra-lsp`. Keep the stdout-cleanliness test
      — it is the one that matters most here, since `hydrust check` writes to
      stdout freely and now shares a process with the LSP transport.
- [ ] New test: `hydrust server --some-unknown-flag` still starts the LSP loop
      and writes nothing to stdout.
- [ ] `hydrust --version` must stay parseable by `parseServerVersion`
      (`compatTable.ts:43`) — it matches the first `\d+.\d+(.\d+)?` in the
      output, so any clap default is fine. Decide whether to set
      `propagate_version` and record what `hydrust server --version` prints, for
      phase A's docstring.
- [ ] In the client repo, flip the candidate order in
      `test/oob/contract.test.ts`'s `requireBinary` back to `hydrust` first.
      Phase A left `hydra-lsp` first because until this phase lands `cargo
      build` produces both and only `hydra-lsp` is the server. Afterwards
      nothing builds a `hydra-lsp`, so a leftover one in `target/debug` would
      be picked up silently and the suite would check the wrong binary.
- [ ] README, CHANGELOG and `.github/copilot-instructions.md`.
- [ ] CHANGELOG: breaking change note covering both renames, and the
      `hydrust.serverVersion` pin as the escape hatch.

## Packaging

Not a phase here. `python-packaging-and-gha-plan.md` has been updated in place:
its phase 2 now depends on phase B above, builds with default features, and
carries the server in the wheel.

## What an extension older than phase A does

Nothing bad, which is the point of the crate rename.

`getLatestVersion` walks releases newest-first and takes the first one carrying
an asset named exactly `hydra-lsp-<target>.<ext>`, skipping any release that
lacks it (`download.ts:218-229`). v0.5.0's assets are `hydrust-<target>.<ext>`,
so a stale client does not match them, resolves v0.4.0 instead, and downloads
and runs it normally. No error, no fallback path, no cached-binary requirement.

The user stays on v0.4.0 until they update the extension. Worth one line in the
phase B release notes so nobody wonders why a new server release did not appear.

Without the crate rename this would instead have been: `ensureServer` throws
"Executable not found after extraction" (`download.ts:329`), caught at
`server.ts:61-73`, falling back to a cached binary if one exists and failing to
start if not — plus a wasted download and extract on every activation, since
`needsDownload` keeps saying yes. Recorded here because it is the failure mode
to re-check if the archive naming is ever revisited.
