# Threading model

How hydra-lsp uses threads, and why it is set up this way.

Read this before changing any thread count, the concurrency level, or where a
request handler does its work. Symbols are named rather than cited by line, so
`rg` is the way to find them; the constants and functions this describes all live
in `src/backend.rs` unless another file is given.

## The shape

Three layers, often confused with each other. They are separate limits and only
the third is about threads.

```text
  stdin ──> tower-lsp decodes, builds a handler future per message
              │
              │  queued up to 100 deep (tower-lsp MESSAGE_QUEUE_SIZE)
              ▼
            buffer_unordered(8)          <- MAX_CONCURRENT_REQUESTS
              │   at most 8 handlers in flight, interleaved
              │   cooperatively on ONE thread
              │                            the outbox task shares this thread
              │                            ┌──────────────┐
              ├───────────────────────────>│ client outbox│──> stdout
              │   log messages,            └──────────────┘
              │   diagnostics, outgoing
              │   requests (never awaited)
              ▼
            handler awaits a pool job
              │
        ┌─────┴─────┐
        ▼           ▼
   latency pool  worker pool            <- rayon, 2 + 3 by default
   (2 threads)   (3 threads)               real OS threads, real parallelism
                                           fewer on a machine with < 6 CPUs
```

1. **One runtime thread.** `serve()` in `src/main.rs` builds
   `tokio::runtime::Builder::new_current_thread()`.
2. **Eight concurrent handlers.** `MAX_CONCURRENT_REQUESTS` in
   `src/backend.rs`, passed to `Server::concurrency_level`. Note the
   integration harness (`tests/common/mod.rs`) does *not* set this, so those
   tests run at tower-lsp's default of 4.
3. **Up to five pool threads.** 2 latency + 3 worker, from `pool_sizes` in
   `src/backend.rs`, overridable through the `numThreads` setting and trimmed on
   a small machine — see *The default is capped by the CPU count* below.
4. **One outbox task.** `src/outbox.rs`. Not a thread and not a handler — it
   holds no lock and does no analysis, it only moves finished messages out to
   the client.

Total OS threads at rest: 1 runtime + up to 5 rayon + however many tokio's
blocking pool spawns for `stdin`/`stdout` (typically 2). The outbox adds none —
on a current-thread runtime a spawned task shares the thread that is already
there. The runtime thread is the process's main thread, not a spawned one:
`block_on` runs the server on the thread that called it.

`numThreads` is that whole count, runtime thread included, so the pools get
`numThreads - 1`. See *`numThreads` counts the runtime thread* below.

## Why there is only one runtime thread

`tower_lsp::Server::serve` never calls `tokio::spawn`. The crate aims to be
executor agnostic, so instead of spawning it builds three futures — read stdin,
write stdout, and a `buffer_unordered` over in-flight handlers — and finishes
with `join!(print_output, read_input, process_server_tasks)`. That is a single
task. `Runtime::block_on` drives it on the calling thread.

hydra-lsp spawns exactly one task of its own, the client outbox. Real work
still goes to rayon and comes back through a `oneshot`; the outbox never runs
any.

So extra tokio worker threads have nothing to run. `#[tokio::main]` (one worker
per core) and `worker_threads(2)` were both tried, and both only added threads
that sat idle. The outbox does not change that: it is parked on an empty channel
almost all the time.

**The consequence that matters.** Handlers are concurrent but not parallel.
Anything that blocks without awaiting — above all taking `Session::db`, a
`parking_lot::Mutex` — stalls stdin, stdout, and all other in-flight requests
until it returns. Tokio cannot steal past a blocked `parking_lot` lock, and here
there is no other thread to steal to anyway. This is why work belongs on the
pools.

The outbox shares that fate — it cannot run while the thread is blocked either,
so messages queued during a long `compute_diagnostics` go out in a burst
afterwards. What it buys is that *sending* is not part of the stall: handing a
message over is a channel push that returns immediately, where awaiting `Client`
directly would add the client's read speed to the time the lock is held.

## Why the concurrency level is set explicitly

tower-lsp defaults `concurrency_level` to 4. Every handler that does real work
hands exactly one job to one pool, so:

> in-flight pool jobs ≤ in-flight handlers

At the default of 4, with 5 pool threads, part of the pool sizing is unreachable
and `numThreads` above 4 does nothing at all: the queue that fills under load is
tower-lsp's admission gate, not rayon's.

8 covers the 5 default pool threads plus a couple of notification handlers
(`did_change`, `did_open`, `did_save`) alongside them. It is not a thread count;
these are futures on the one runtime thread, so raising it costs memory for
pending futures and nothing else.

Note it limits handlers *started*, not messages *read*. tower-lsp builds each
handler future as it decodes and queues it 100 deep, so stdin keeps draining
well past 8.

## Where each request actually runs

| Handler | Where the work happens |
| --- | --- |
| `hover` | `spawn_definition_lookup` → **latency pool** |
| `signature_help` | `spawn_definition_lookup` → **latency pool** |
| `goto_definition` | `spawn_definition_lookup` → **latency pool** |
| `semantic_tokens_full` | snapshot → **latency pool** |
| `diagnostic` (pull) | snapshot → **worker pool** |
| `completion` | **inline**, but mostly stubbed — see gaps |
| `did_change` | **inline**: input update, then `is_hydra_file` under the lock |
| `publish_diagnostics_if_needed` | **inline** via `compute_diagnostics` — deliberate, see below |

`spawn_definition_lookup` is the only latency-pool call site besides semantic
tokens. Pull `diagnostic` is the only worker-pool call site.

`HydraLspBackend::with_session` takes a *synchronous* closure — it runs inline on
the caller's thread. It is not an escape hatch to a pool.
`HydraLspBackend::snapshot` is what you want when work is going to a pool.

## Three structural facts worth internalising

**The db mutex is the real serializer.** `Session::db` is a single
`parking_lot::Mutex`. Pools give you parallelism only for the stretch of a job
that is not holding it. Adding threads does not help work that is queued on the
lock.

**The pools isolate threads, not work.** Both pools reach
`python_cache::cached_definition_info`. When two requests want the same key,
salsa blocks one until the other finishes, regardless of which pool they came
from. Separating the pools stops a diagnostics burst from occupying every thread
and leaving a hover waiting; it does not stop them contending on shared queries.

**Salsa cancellation is coarse.** A write bumps the revision and in-flight
queries unwind with `salsa::Cancelled`. `spawn_on_pool` catches that and reports
`PoolOutcome::Cancelled`; handlers turn it into
`ContentModified` (or `ServerCancelled` with `retrigger_request` for pull
diagnostics) so the client re-asks. During fast typing the pools are therefore
bursty — a lot of started-and-abandoned work.

## Design decisions, and the alternatives rejected

### Fixed default instead of scaling with cores

The default is 2 + 3 however large the machine is. Scaling with cores is the
obvious alternative and was how it started: the worker pool defaulted to
`available_parallelism() - 2` and the `numThreads` clamp allowed up to
`available_parallelism() * 8`, so a 16-core machine got 14 worker threads by
default and could be asked for 128. Almost all of them never ran.

Neither pool's useful width grows with cores:

- **Latency = 2.** `semanticTokens/full` and `signatureHelp` fire together on an
  edit and are badly matched in cost — token extraction is nearly free once the
  document is parsed, while signature help can sit in a cold
  `cached_definition_info` reading and parsing Python. Two threads stop the cheap
  one queueing behind the expensive one. A third would need a third concurrent
  request, and in practice that means hover, which happens when the pointer
  rests rather than while typing.
- **Worker = 3.** The one real fan-out is re-validating every open Hydra
  document after a watched `.py` file changes. That fan-out is already well below
  the open-document count, because documents sharing a `_target_` share one
  `cached_definition_info` key. Three rather than two because this pool absorbs
  whole-workspace bursts while the latency pool only ever has one cursor position
  to answer for.

Reference point: ruff and ty cap their background pool at
`min(available_parallelism, 4)` and run a single fmt thread
(`ty_server/src/lib.rs`, `ty_server/src/server/schedule.rs`).

### `numThreads` counts the runtime thread

`numThreads` is every thread the server runs, not every thread it hands to the
pools, so `pool_sizes` divides `numThreads - RUNTIME_THREADS`. The runtime thread
is a real thread on a real core; someone setting `numThreads: 4` on a four-core
box means "use this machine", and four pool threads *plus* the runtime thread
would be one more than they asked for.

The consequences:

- The usable range is 0 (the sentinel) and `3..=11`. Below `MIN_NUM_THREADS` = 3
  there is nothing left to divide once the runtime thread is taken out, and
  neither pool may be empty.
- `MAX_NUM_THREADS` = 11, one above the pool ceiling of 10, so the top of the
  range still reaches `MAX_POOL_THREADS`.
- The untrimmed default is `numThreads: 6`, not 5.
- The `initialize` log reports the total including the runtime thread, so it can
  be read straight against the setting.

`clamp_num_threads` holds the value in range and returns a warning when it had to
move it, which `initialize` logs at `WARNING`. An out-of-range value is not a
hard error: an older client that still offers 1 or 2 should get a working server
and an explanation, not a refusal.

Client-side, `package.json` makes `hydrust.numThreads` an `enum` of exactly those
values rather than `minimum`/`maximum`, because a range cannot express the hole
at 1 and 2. That turns the setting into a dropdown, and `enumDescriptions` spells
out the split each value produces.

### The default is capped by the CPU count

The *shape* of the default does not scale with cores, per the section above; its
*size* is still bounded by them. `default_pool_total_for` gives
`min(5, available_parallelism - 1)`, and `split_pool_total` divides that with the
same rule an explicit `numThreads` gets, so the equivalence in the invariants
section below still holds.

Two things this is not:

- **Not scaling with cores.** The default never grows past 5. This only stops it
  exceeding what the machine has.
- **Not a clamp on an explicit `numThreads`.** That is the next section, and it
  is still rejected. Nobody asked for the default number, so trimming it
  overrides no intent; trimming a number someone typed does.

The reserved CPU is for the runtime thread. It is idle almost all the time, but
it is the thread that reads stdin, and if analysis occupies every core it is what
gets preempted — the symptom being the editor waiting on a request the server has
not read yet. The case that prompted this: a two-CPU CI box ran five analysis
threads on two cores with the reader competing against all five.

Floors win over the cap. One CPU leaves a budget of zero, but rayon reads
`num_threads(0)` as "one per core", so `split_pool_total` still returns `(1, 1)`.
On a one-CPU machine the pools are oversubscribed by design.

Reduction is logged: `cpu_limited_default_note` appends the nominal default, the
CPU count, and a pointer to `numThreads` — otherwise a quietly halved pool is
indistinguishable from a slow server.

### `numThreads` is clamped to 11, not to the CPU count

`MAX_POOL_THREADS = MAX_CONCURRENT_REQUESTS + DEFAULT_LATENCY_THREADS` = 10, and
`MAX_NUM_THREADS` is that plus the runtime thread. Filling `n` worker threads
needs `n` concurrent diagnostic pulls, and only 8 handlers exist at a time, so
beyond 10 pool threads no message mix can start them.

Clamping an explicit setting to `available_parallelism()` was considered and
rejected:

- It reports what is available *now* — it honours cgroup quotas and CPU
  affinity. In a two-CPU container it would turn an explicit `numThreads: 6` into
  `(1, 1)`, on exactly the machine where someone was trying to tune, and leave
  them no way to say otherwise.
- The setting's whole purpose is to override what the server would pick, and
  what the server picks is already CPU-aware (previous section). A clamp that
  reimposes the same bound makes the override inert in exactly the case someone
  reaches for it.

The principle: a clamp should reject numbers that are *meaningless*, not numbers
that are merely *suboptimal for the hardware*. Over-provisioning is reported
instead — `oversubscription_note` appends a line to the `initialize` log when the
total exceeds the CPU count.

### `numThreads: 0` means "server decides"

`clamp_num_threads` maps `0` to `None`, and `pool_sizes` matches `Some(0)` as
well, so the sentinel is honoured by the sizing functions rather than by their
one caller.

Worth stating explicitly because getting it wrong is silent and affects
everybody. The VS Code extension sends `0` whenever the user has not set the
option (`hydra-lsp-vscode/src/common/settings.ts`), and `package.json` documents
`0` as "let the server decide". An earlier version clamped it to `1` instead,
which hit the `0..=2 => (1, 1)` floor in `split_pool_total` — so **every default
VS Code session ran a 1 + 1 split.**

### `semantic_tokens_full` uses snapshot + pool

It runs on a snapshot on the latency pool — not the worker pool, because
highlighting is on the render path for each keystroke.

Running it inline under `s.db.lock()` is the obvious alternative and was how it
started. The `semantic_tokens` query depends on `parsed_yaml`, so the first
request after an edit pays a full YAML parse; holding the lock across that blocks
the next `did_change` write, which is what every other handler waits behind.

A request superseded mid-flight returns `ContentModified` rather than
`Ok(None)`, because `Ok(None)` would blank the client's highlighting until some
later edit happened to produce a clean round.

### `compute_diagnostics` deliberately stays inline

`compute_diagnostics` holds `db.lock()` across the whole `validate_document` run.
That is intentional: with no interleaved `set_text`, the revision cannot move
underneath it, so it never observes `salsa::Cancelled`. Moving it to the worker
pool would make it cancellable, and the push path has no client-side retrigger to
recover with.

**Do not "fix" this without solving the cancellation story first.** It is the
one place that knowingly blocks the runtime thread.

### Handlers never await the client

Every message out — `window/logMessage`, `publishDiagnostics`,
`registerCapability`, `workspace/diagnostic/refresh` — goes onto the outbox
queue (`src/outbox.rs`) and is awaited by its drain task instead. There is
deliberately no `Client` field on `HydraLspBackend`, so a handler cannot go
around it.

This is not a tidiness rule, it is a liveness one. `tower_lsp::Server::serve`
aborts the stream feeding stdout the moment stdin reaches end of file, and an
aborted stream is never polled again. Its server-to-client queue holds one
message, so the *second* notification a handler sends waits for space nobody
will free; an outgoing request is worse, because no reply can arrive once the
stdin reader has stopped. Either way the handler never finishes, `serve()`
never returns, and the process stays alive doing nothing.

That is a real failure, not a theoretical one. An `initialize` that logged two
messages hung about 1 run in 150 under load on Linux — 5 wedged processes in
800 — which is what `test_no_arguments_speaks_lsp_on_stdout` hit in CI. The same
soak over the outbox is clean across 4000 runs. `tests/server_cli.rs` guards the
regression with `EXIT_TIMEOUT`, so a recurrence fails in 30 seconds instead of
hanging the job.

The drain task can still get stuck this way, and that is the point: it holds no
lock, nothing waits on it, and dropping the runtime drops it, so the process
exits regardless.

Two consequences worth knowing:

- The queue is unbounded, so a slow client grows it rather than throttling
  handlers. `ty_server` makes the same trade. The alternative is letting the
  client's read speed set the pace of edits.
- Ordering is FIFO across *all* message kinds, which is what keeps a
  `clear_diagnostics` from overtaking the `publish_diagnostics` before it.
  Anything added to `ClientCall` inherits that; do not add a second queue.

`shutdown` waits for the queue to empty, bounded by `FLUSH_TIMEOUT`, so the
tail of the log survives a clean exit without a hung client delaying it.

## Invariants

Enforced at compile time by the `const` block next to `MAX_POOL_THREADS`:

```text
DEFAULT_POOL_THREADS <= MAX_CONCURRENT_REQUESTS
DEFAULT_POOL_THREADS <= MAX_POOL_THREADS
MIN_NUM_THREADS <= DEFAULT_NUM_THREADS <= MAX_NUM_THREADS
```

The default split must be reachable (enough handler slots to start every pool
thread) and expressible as a setting value (so `numThreads: 6` reproduces the
untrimmed default). Expressibility survives the CPU cap because both branches of
`pool_sizes` divide their total through `split_pool_total` — asserted for
whatever this machine produces by
`pool_sizes_default_matches_the_equivalent_explicit_total`.

Not enforced but true: raising `numThreads` requires raising
`MAX_CONCURRENT_REQUESTS` to match, or the extra threads are unreachable.
`MAX_POOL_THREADS` is derived from it so this mostly takes care of itself.

Client-side, `package.json` lists the settable values as an `enum` on
`hydrust.numThreads` so the settings UI rejects out-of-range values rather than
letting the server silently clamp. **If `MIN_NUM_THREADS` or `MAX_NUM_THREADS`
changes, that list, its `enumDescriptions`, and the setting description have to
change with it**, along with the evidence string in
`hydra-lsp-vscode/src/common/compatTable.ts`.

## Known gaps

Each of these is a real limitation that is left in place on purpose. Change one
only with the reason it exists in hand.

**The push path cannot fill the worker pool.** After watched Python files
change, `did_change_watched_files` ends in a sequential `for` loop of
`publish_diagnostics_if_needed(&uri)`. One document at a time, on the runtime
thread, via the inline `compute_diagnostics`. If the third worker thread looks
idle in profiling, this is why — the pool is only reached from *pull*
diagnostics. Fixing it properly means solving the `compute_diagnostics`
cancellation question above.

The loop ends each iteration with an explicit `tokio::task::yield_now().await`.
Publishing awaits nothing now that it goes through the outbox, so without that
the whole refresh runs before stdin is read again. Do not delete it as redundant
— nothing else in the loop body yields.

**`completion` runs inline.** Currently fine, because it is mostly stubbed:
`TargetValue` and `ParameterKey` return `Ok(None)` with TODOs, and
`ParameterValue` returns static lists for `_partial_` / `_recursive_` /
`_convert_`. It is cheap partly because `DocumentInput::text` is
`#[returns(ref)]`, so reading it is a borrow rather than a full clone. **When
completion is actually implemented it must move to the snapshot + pool pattern**
— the in-code TODOs say so, and this is the note backing them.

**Ordering is safe, but only by accident of placement.** `buffer_unordered`
polls futures for the first time in arrival order, and `did_change` calls
`get_or_create_input` synchronously before its first `.await`. So the salsa
input is updated before any later-arriving handler is first polled, and a hover
arriving after an edit cannot be answered against the pre-edit revision.
Completion order is unordered; the *input update* is not. If anything is ever
inserted before that call, or an `.await` added ahead of it, that property is
lost silently.

Routing the client through the outbox makes this sturdier by accident:
`did_change` has no `.await` at all before the input update, since the log
message and the publish are both plain calls. That is less to trip over, but it
is still an accident of placement, so the warning still stands.

## Where this lives in the code

In `hydra-lsp`:

- `src/main.rs` — `serve()`: the `current_thread` runtime and
  `concurrency_level`.
- `src/backend.rs` — the pool-sizing constants and the `const` invariant block;
  `pool_sizes`, `split_pool_total`, `default_pool_total`/`_for`,
  `clamp_num_threads`, `oversubscription_note`, `cpu_limited_default_note`;
  `initialize`, which is the only place the pools are built; `snapshot`,
  `with_session`, `spawn_on_pool`, `spawn_definition_lookup`;
  `compute_diagnostics` and the publish helpers. Unit tests for all of the sizing
  logic are in the same file.
- `src/outbox.rs` — the outbound queue and its drain task.
- `tests/server_cli.rs` — `EXIT_TIMEOUT`, the guard against the outbox hang
  coming back.

In `hydra-lsp-vscode`:

- `package.json` — `hydrust.numThreads`: the `enum` of settable values (`0`,
  `3..=11`) and its `enumDescriptions`.
- `src/common/settings.ts` — where the `0` sentinel is sent from.
- `src/common/compatTable.ts` — the compatibility evidence string for
  `numThreads`.
