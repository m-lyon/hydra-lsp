//! The two rayon pools: how they are sized, how they are built, and how work is
//! handed to them. `docs/threading-model.md` explains the sizing.

use salsa::CancellationToken;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};
use tokio::sync::oneshot;
use tower_lsp::jsonrpc::{Error, Result};
use tower_lsp::lsp_types::MessageType;

use super::HydraLspBackend;

/// Outcome of a closure run on a rayon pool via [`spawn_on_pool`].
///
/// Two failure modes are kept distinct because callers treat them differently:
/// - `Cancelled` is the *expected* consequence of a concurrent write: salsa
///   throws `salsa::Cancelled` (via `panic::resume_unwind`) to abandon a query
///   whose revision was superseded. Callers should silently drop the result;
///   the superseding edit will recompute it.
/// - `Panicked` is a genuine bug worth logging.
#[derive(Debug)]
pub(crate) enum PoolOutcome<R> {
    Completed(R),
    /// Salsa cancelled the query: a concurrent write bumped the revision, or
    /// the request was dropped.
    Cancelled,
    /// The closure panicked for a non-cancellation reason. Carries a
    /// best-effort message extracted from the panic payload.
    Panicked(String),
}

impl<R> PoolOutcome<R> {
    /// The outcome a [`spawn_on_pool`] receiver resolved to, with a dropped
    /// sender read as `Cancelled`.
    ///
    /// The sender is only dropped unsent when the pool is not built yet (or has
    /// shut down), and every caller treats that the way it treats a superseded
    /// revision: the request is stale.
    pub(crate) fn from_received(
        received: std::result::Result<Self, oneshot::error::RecvError>,
    ) -> Self {
        received.unwrap_or(PoolOutcome::Cancelled)
    }
}

/// Best-effort human-readable message from a panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Bridge a rayon thread-pool task to an async tokio context.
///
/// Spawns `f` on `pool` and returns a [`PoolJob`] that resolves when the task
/// finishes. Unlike `tokio::task::spawn_blocking`, the closure runs on a
/// caller-chosen rayon pool, which lets us separate latency-sensitive work
/// (hover, goto_definition) from background work (diagnostics).
///
/// `f` queries through `db`, which should be a snapshot of its own: dropping the
/// job before it resolves cancels that handle's [`CancellationToken`], so a
/// request tower-lsp abandons (on `$/cancelRequest`) stops at salsa's next
/// cancellation. `f` runs inside one `attach` because salsa clears the token
/// whenever the outermost attached scope exits.
///
/// `f` is run inside `catch_unwind` and its result is delivered as a
/// [`PoolOutcome`], distinguishing normal completion, salsa cancellation, and a
/// genuine panic. Without it a salsa `Cancelled` unwind would propagate into rayon and
/// abort the process, since these pools register no `panic_handler`.
///
/// The borrow of `pool` ends immediately after `pool.spawn` returns — before
/// any `.await` suspension — so callers do not need to hold a reference across
/// yield points.
pub(super) fn spawn_on_pool<Db, F, R>(
    pool: &OnceLock<rayon::ThreadPool>,
    db: Db,
    f: F,
) -> PoolJob<R>
where
    Db: salsa::Database + Send + 'static,
    F: FnOnce(&Db) -> R + Send + 'static,
    R: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    let job = PoolJob {
        rx,
        cancel: Some(db.cancellation_token()),
    };
    let Some(pool) = pool.get() else {
        // Sender dropped on return; the job resolves to RecvError.
        return job;
    };
    pool.spawn(move || {
        let run = || db.attach(f);
        let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
            Ok(r) => PoolOutcome::Completed(r),
            Err(payload) => {
                if payload.downcast_ref::<salsa::Cancelled>().is_some() {
                    PoolOutcome::Cancelled
                } else {
                    PoolOutcome::Panicked(panic_message(&*payload))
                }
            }
        };
        let _ = tx.send(outcome);
    });
    job
}

/// A job handed to a pool by [`spawn_on_pool`]. Awaiting it gives the job's
/// outcome; dropping it first cancels the job.
pub(super) struct PoolJob<R> {
    rx: oneshot::Receiver<PoolOutcome<R>>,
    /// Taken once the job resolves, so a finished job is not cancelled on drop.
    cancel: Option<CancellationToken>,
}

impl<R> Future for PoolJob<R> {
    type Output = std::result::Result<PoolOutcome<R>, oneshot::error::RecvError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let received = std::task::ready!(Pin::new(&mut self.rx).poll(cx));
        self.cancel = None;
        Poll::Ready(received)
    }
}

impl<R> Drop for PoolJob<R> {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
    }
}

impl HydraLspBackend {
    /// Build the latency and worker pools for a `numThreads` override (`None`
    /// for the server default) and log the split. Called once, from
    /// `initialize`; fails only when the OS refuses to create the threads.
    pub(super) fn build_pools(&self, num_threads: Option<usize>) -> Result<()> {
        // Build the two rayon pools exactly once, when `numThreads` is known.
        let (latency, worker) = pool_sizes(num_threads);
        match (
            rayon::ThreadPoolBuilder::new()
                .num_threads(latency)
                .thread_name(|i| format!("hydra-latency-{i}"))
                .build(),
            rayon::ThreadPoolBuilder::new()
                .num_threads(worker)
                .thread_name(|i| format!("hydra-worker-{i}"))
                .build(),
        ) {
            (Ok(latency_pool), Ok(worker_pool)) => {
                // `set` cannot fail: `initialize` is the sole writer, once.
                let _ = self.latency_pool.set(latency_pool);
                let _ = self.worker_pool.set(worker_pool);
                let total = latency + worker + RUNTIME_THREADS;
                let note = match num_threads {
                    None => cpu_limited_default_note(total),
                    Some(_) => oversubscription_note(total),
                };
                self.outbox.log(
                    MessageType::INFO,
                    format!(
                        "Thread pools configured: {latency} latency + {worker} worker, \
                         plus the protocol loop ({total} threads in all){note}"
                    ),
                );
                Ok(())
            }
            _ => {
                let msg = format!(
                    "Failed to build thread pools ({latency} latency + {worker} worker); \
                     the OS refused thread creation"
                );
                self.outbox.log(MessageType::ERROR, &msg);
                let mut err = Error::internal_error();
                err.message = msg.into();
                Err(err)
            }
        }
    }
}

/// Thread counts for the latency and worker pools, given the `numThreads`
/// setting (`None` when the client did not set it).
///
/// `numThreads` is every thread the server runs, not every thread it hands to
/// the pools: the async runtime's thread is one of them, so the pools get
/// `numThreads - RUNTIME_THREADS`. The runtime's thread is not itself
/// configurable: it is fixed at startup (see `serve` in `server.rs`), because the
/// setting only arrives with `initialize`, by which time the runtime is running.
/// It is the process's main thread rather than a spawned one, and the client
/// outbox shares it rather than adding another.
///
/// So `numThreads` has no meaning below [`MIN_NUM_THREADS`] — one runtime thread
/// plus one per pool — and [`clamp_num_threads`] lifts anything lower, with a
/// warning, before this is called.
///
/// Every branch keeps both counts >= 1: rayon reads `num_threads(0)` as
/// "auto-detect from core count", which would silently over-allocate.
///
/// Neither count can be used beyond [`MAX_CONCURRENT_REQUESTS`]: each handler
/// hands off one job to one pool, so the number of pool jobs running at once is
/// capped by the number of handlers tower-lsp will run at once. That is what
/// [`MAX_NUM_THREADS`] clamps the setting to.
///
/// The *shape* of the default is fixed rather than derived from the core count,
/// because neither pool's useful width scales with cores:
///
/// - Latency serves one keystroke's worth of overlapping requests.
///   `semanticTokens/full` and `signatureHelp` fire together on an edit, and
///   they are badly matched in cost — token extraction is nearly free once the
///   document is parsed, while signature help can sit in a cold
///   `cached_definition_info` reading and parsing Python. Two threads stop the
///   cheap one queueing behind the expensive one. A third would need a third
///   concurrent request, and that means hover, which happens when the pointer
///   rests rather than while typing.
///
/// - Worker serves the one real fan-out in the server: after a watched `.py`
///   file changes, every open Hydra document is re-validated. That fan-out is
///   capped well below the open-document count, because documents sharing a
///   `_target_` share one `cached_definition_info` key and salsa blocks the
///   duplicate callers rather than computing twice. Three threads rather than
///   two because this pool absorbs whole-workspace bursts while the latency
///   pool only ever has one cursor position to answer for, and because the
///   jobs are long enough that a queued document waits noticeably.
///
/// Its *size* is capped by the machine, though. Five threads is the width the
/// server can use, not a promise the hardware can run them, and someone who has
/// not set `numThreads` is asking for a sensible default rather than for that
/// width — so the default is trimmed to fit the CPUs on hand.
fn pool_sizes(num_threads: Option<usize>) -> (usize, usize) {
    match num_threads {
        // `initialize` already maps the client's 0 sentinel to `None`; matching
        // it here too keeps the "never return 0" guarantee a property of this
        // function rather than of its one caller.
        None | Some(0) => split_pool_total(default_pool_total()),
        Some(n) => split_pool_total(n.saturating_sub(RUNTIME_THREADS)),
    }
}

/// Divide a total across the two pools, keeping both >= 1.
///
/// Latency is filled first and capped at its default: an override is asking for
/// more analysis throughput, and the second latency thread is already enough for
/// the requests one keystroke produces. Shared by both branches of
/// [`pool_sizes`], so a clamped default splits exactly as the equivalent
/// explicit `numThreads` would.
fn split_pool_total(total: usize) -> (usize, usize) {
    match total {
        // Below three there is nothing to divide; one each is the floor.
        0..=2 => (1, 1),
        n => (
            DEFAULT_LATENCY_THREADS,
            n.saturating_sub(DEFAULT_LATENCY_THREADS),
        ),
    }
}

/// Total pool threads when `numThreads` is unset: [`DEFAULT_POOL_THREADS`],
/// trimmed to leave one of the CPUs the OS reports for the runtime thread.
///
/// `None` means the OS would not say how many there are, which is no reason to
/// shrink.
fn default_pool_total() -> usize {
    match available_cpus() {
        Some(available) => DEFAULT_POOL_THREADS.min(available.saturating_sub(RUNTIME_THREADS)),
        None => DEFAULT_POOL_THREADS,
    }
}

/// CPUs this process may use, or `None` when the OS will not say.
///
/// The single place the count is read, so that a test can stand a number in for
/// it. Pool sizing and both `initialize` notes have to agree about how many CPUs
/// there are, and checking them against whatever the test machine happens to
/// report would only check the arithmetic against itself.
fn available_cpus() -> Option<usize> {
    #[cfg(test)]
    {
        if let Some(mocked) = tests::mocked_cpus() {
            return mocked;
        }
    }
    std::thread::available_parallelism().ok().map(|n| n.get())
}

/// Clamp a raw `numThreads` into the range the server can use, and say so when
/// it had to move.
///
/// `Ok(None)` is the 0 sentinel: "let the server decide". Otherwise the value is
/// held between [`MIN_NUM_THREADS`] and [`MAX_NUM_THREADS`], and the second
/// element is a message for the `initialize` log when it was out of range. A
/// number that quietly becomes a different number is the kind of thing someone
/// only notices as "the setting does nothing".
pub(super) fn clamp_num_threads(raw: u64) -> (Option<usize>, Option<String>) {
    if raw == 0 {
        return (None, None);
    }
    let requested = usize::try_from(raw).unwrap_or(MAX_NUM_THREADS);
    let clamped = requested.clamp(MIN_NUM_THREADS, MAX_NUM_THREADS);
    if clamped == requested {
        return (Some(clamped), None);
    }
    let why = if requested < MIN_NUM_THREADS {
        format!(
            "the minimum is {MIN_NUM_THREADS}: one thread runs the protocol \
             loop, and each of the two analysis pools needs at least one"
        )
    } else {
        format!(
            "the maximum is {MAX_NUM_THREADS}: beyond that the extra threads \
             cannot be reached, because only {MAX_CONCURRENT_REQUESTS} requests \
             run at once to feed them"
        )
    };
    (
        Some(clamped),
        Some(format!(
            "numThreads {requested} is out of range and was treated as \
             {clamped} — {why}. Use 0 for the server default."
        )),
    )
}

/// Threads in the latency pool: hover, signature help, go-to-definition and
/// semantic tokens. See [`pool_sizes`] for why this is 2, and why an override
/// adds to the worker pool rather than to this one.
const DEFAULT_LATENCY_THREADS: usize = 2;

/// Threads in the worker pool: diagnostics validation. See [`pool_sizes`].
const DEFAULT_WORKER_THREADS: usize = 3;

/// Total pool threads the server will ask for by default, before the CPU count
/// is taken into account. See [`default_pool_total`].
const DEFAULT_POOL_THREADS: usize = DEFAULT_LATENCY_THREADS + DEFAULT_WORKER_THREADS;

/// Threads that are not in either pool: the one the async runtime runs on.
///
/// Named rather than written as `1` because it is the difference between the
/// user-facing `numThreads` and the number the pools divide up, and that
/// subtraction appears in several places.
const RUNTIME_THREADS: usize = 1;

/// What `numThreads` would have to be to ask for the untrimmed default.
const DEFAULT_NUM_THREADS: usize = DEFAULT_POOL_THREADS + RUNTIME_THREADS;

/// How many LSP messages `tower_lsp` will have handlers in flight for at once,
/// passed to `Server::concurrency_level` in `server.rs`.
///
/// This, not the pool sizes, is what caps parallel analysis. Every handler that
/// does real work hands exactly one job to a rayon pool and awaits it, so
/// in-flight pool jobs can never exceed in-flight handlers. tower-lsp's own
/// default is 4, which is below the 5 pool threads [`pool_sizes`] hands out by
/// default — leaving it there would make part of that sizing unreachable.
///
/// Eight covers the five default pool threads plus a couple of notification
/// handlers (`did_change`, `did_open`, `did_save`) running alongside them. It is
/// not a thread count: these are futures interleaved on the one runtime thread,
/// so raising it costs a little memory for pending futures and nothing else.
///
/// Note this limits handlers *started*, not messages read. tower-lsp builds each
/// handler future as it decodes the message and queues it 100 deep, so stdin
/// keeps draining well past this number.
pub(crate) const MAX_CONCURRENT_REQUESTS: usize = 8;

/// Most threads the two pools can be given between them.
///
/// Above this the pools are unreachable rather than merely oversubscribed:
/// filling `n` worker threads needs `n` concurrent diagnostic pulls, and only
/// [`MAX_CONCURRENT_REQUESTS`] handlers exist at a time, so beyond
/// `MAX_CONCURRENT_REQUESTS + DEFAULT_LATENCY_THREADS` no message mix can start
/// them.
const MAX_POOL_THREADS: usize = MAX_CONCURRENT_REQUESTS + DEFAULT_LATENCY_THREADS;

/// Upper bound on the `numThreads` setting: [`MAX_POOL_THREADS`] plus the
/// runtime thread the setting also counts.
///
/// Deliberately not the core count. Clamping an explicit setting to
/// `available_parallelism()` was considered and rejected. It reports what is
/// available *now* — it honours cgroup quotas and CPU affinity — so in a two-CPU
/// container it would silently turn an explicit `numThreads: 6` into `(1, 1)`,
/// on exactly the machine where someone was trying to tune, with no way to say
/// otherwise. A clamp should reject numbers that are meaningless, not numbers
/// that are merely suboptimal for the hardware; over-provisioning is reported by
/// [`oversubscription_note`] in the `initialize` log instead.
///
/// The unset default *is* trimmed to the CPU count ([`default_pool_total`]),
/// which is not in tension with the above: nobody asked for that number, so
/// there is no intent to override.
const MAX_NUM_THREADS: usize = MAX_POOL_THREADS + RUNTIME_THREADS;

/// Lower bound on the `numThreads` setting, 0 aside: the runtime thread plus one
/// per pool. Anything less cannot be honoured, since neither pool may be empty.
const MIN_NUM_THREADS: usize = RUNTIME_THREADS + 2;

/// The default split has to be both reachable and expressible as a setting
/// value, which is what the two clamps above are for. Checked here rather than
/// in a test so that changing one constant without the others cannot compile.
const _: () = {
    assert!(DEFAULT_POOL_THREADS <= MAX_CONCURRENT_REQUESTS);
    assert!(DEFAULT_POOL_THREADS <= MAX_POOL_THREADS);
    assert!(MIN_NUM_THREADS <= DEFAULT_NUM_THREADS && DEFAULT_NUM_THREADS <= MAX_NUM_THREADS);
};

/// A trailing note for the `initialize` log when an explicit `numThreads` asks
/// for more threads than the machine reports CPUs, or the empty string when it
/// does not. `total` counts the runtime thread, as `numThreads` does.
///
/// [`MAX_NUM_THREADS`] deliberately does not clamp an explicit setting to
/// `available_parallelism`, so a `numThreads` that outruns the hardware is
/// honoured. Saying so in the log is how someone tuning the setting finds out it
/// went too far — otherwise the symptom is that raising it stops changing
/// anything, with nothing to explain why. Only reported for an oversubscribing
/// total, so the common case stays quiet.
fn oversubscription_note(total: usize) -> String {
    // `None` means the OS would not tell us; that is not worth reporting on.
    let Some(available) = available_cpus() else {
        return String::new();
    };
    if total <= available {
        return String::new();
    }
    format!(
        " — above the {available} CPUs available here, so these threads will \
         share cores rather than run in parallel"
    )
}

/// A trailing note for the `initialize` log when the unset default came out
/// below [`DEFAULT_NUM_THREADS`] because of the CPU count, or the empty string
/// when it did not. `total` counts the runtime thread, as `numThreads` does.
///
/// The counterpart to [`oversubscription_note`], and there for the same reason:
/// a silently reduced pool looks identical to a slow server. It also points at
/// the way out, since the reduction only applies while `numThreads` is unset.
fn cpu_limited_default_note(total: usize) -> String {
    if total >= DEFAULT_NUM_THREADS {
        return String::new();
    }
    // Unreachable via `initialize` — a total below the default means the CPU
    // count is what trimmed it, so the OS did answer — but the note has nothing
    // to name without a count.
    let Some(available) = available_cpus() else {
        return String::new();
    };
    format!(
        " — the usual default is {DEFAULT_NUM_THREADS}, reduced here to fit the \
         {available} CPUs available; set numThreads to override"
    )
}

/// A one-thread pool for tests here and in `diag` that run work through
/// [`spawn_on_pool`].
#[cfg(test)]
pub(super) fn single_thread_pool() -> OnceLock<rayon::ThreadPool> {
    let cell = OnceLock::new();
    let _ = cell.set(
        rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("failed to build test pool"),
    );
    cell
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::tests::TestDb;
    use crate::yaml_cache::{self, DocumentInput};
    use salsa::{Database as _, Setter};
    use std::cell::Cell;
    use std::time::Duration;

    thread_local! {
        /// What [`available_cpus`] should report inside [`with_cpus`]: the outer
        /// `Option` is whether a test is standing in for the OS at all, the
        /// inner one is the count it reports.
        static MOCK_CPUS: Cell<Option<Option<usize>>> = const { Cell::new(None) };
    }

    /// The count [`available_cpus`] should use in place of the OS, if any.
    pub(super) fn mocked_cpus() -> Option<Option<usize>> {
        MOCK_CPUS.get()
    }

    /// Run `body` as if the machine had `cpus` CPUs — `None` for an OS that will
    /// not say. Thread-local, so tests running in parallel do not see each
    /// other's stand-in.
    fn with_cpus<T>(cpus: Option<usize>, body: impl FnOnce() -> T) -> T {
        MOCK_CPUS.set(Some(cpus));
        let out = body();
        MOCK_CPUS.set(None);
        out
    }

    // --- Test A: spawn_on_pool contract ---

    #[tokio::test]
    async fn spawn_on_pool_completes_normally() {
        let pool = single_thread_pool();
        let outcome = spawn_on_pool(&pool, TestDb::new(), |_| 42_i32)
            .await
            .expect("sender dropped");
        assert!(matches!(outcome, PoolOutcome::Completed(42)));
    }

    #[tokio::test]
    async fn spawn_on_pool_captures_panic_without_aborting() {
        // Pre-fix, a panic in the closure would reach rayon (no panic_handler)
        // and abort the whole test process. Reaching the assertion at all is
        // the regression guard; the message check confirms payload capture.
        let pool = single_thread_pool();
        let outcome = spawn_on_pool(&pool, TestDb::new(), |_| -> i32 { panic!("boom-42") })
            .await
            .expect("sender dropped");
        match outcome {
            PoolOutcome::Panicked(msg) => assert!(
                msg.contains("boom-42"),
                "expected panic message to contain payload, got {msg:?}"
            ),
            other => panic!("expected Panicked, got {other:?}"),
        }
    }

    // --- Test C: real salsa cancellation is classified as Cancelled ---

    #[tokio::test]
    async fn salsa_cancellation_is_caught_and_classified() {
        let pool = single_thread_pool();

        let mut db = TestDb::new();
        let input = DocumentInput::new(&db, "v1".to_string(), 1);
        // Snapshot shares storage with `db`; a `&mut db` write cancels queries
        // running on this handle.
        let snapshot = db.clone();

        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel::<()>();

        let rx = spawn_on_pool(&pool, snapshot, move |db| {
            started_tx.send(()).expect("main receiver dropped");
            proceed_rx.recv().expect("main sender dropped");
            // Poll the cancellation checkpoint until the concurrent write sets
            // the flag; this unwinds with salsa::Cancelled once observed. The
            // bound + outer timeout keep a mis-choreographed race from hanging.
            for _ in 0..100_000_000_u64 {
                db.unwind_if_revision_cancelled();
                std::thread::yield_now();
            }
            panic!("cancellation flag was never observed");
        });

        started_rx.recv().expect("worker never started");
        // `set_text(&mut db)` blocks in salsa's cancel_others until the snapshot
        // handle is released, so it must run on its own thread.
        let writer = std::thread::spawn(move || {
            input.set_text(&mut db).to("v2".to_string());
        });
        proceed_tx.send(()).expect("worker receiver dropped");

        let outcome = tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .expect("timed out waiting for cancellation")
            .expect("sender dropped");
        assert!(
            matches!(outcome, PoolOutcome::Cancelled),
            "expected Cancelled, got {outcome:?}"
        );
        writer.join().expect("writer thread panicked");
    }

    // --- Dropping a PoolJob cancels it ---

    #[tokio::test]
    async fn dropping_the_job_cancels_its_query() {
        let pool = single_thread_pool();
        let db = TestDb::new();
        let input = DocumentInput::new(&db, "# @hydra\nmodel: {}\n".to_string(), 1);
        let snapshot = db.clone();

        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (unwound_tx, unwound_rx) = std::sync::mpsc::channel();

        let job = spawn_on_pool(&pool, snapshot, move |db| {
            started_tx.send(()).expect("main receiver dropped");
            let caught = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
                for _ in 0..100_000_000_u64 {
                    let _ = yaml_cache::parsed_yaml(db, input);
                    std::thread::yield_now();
                }
            }));
            unwound_tx.send(caught).expect("main receiver dropped");
        });

        started_rx.recv().expect("worker never started");
        // What tower-lsp does to a handler future on `$/cancelRequest`. No
        // write happens, so only the token can stop the loop.
        drop(job);
        let caught = unwound_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the job was not cancelled");
        assert!(
            matches!(caught, Err(salsa::Cancelled::Local)),
            "expected a local cancellation, got {caught:?}"
        );
        // The token belongs to the snapshot, so the original handle still runs.
        let _ = yaml_cache::parsed_yaml(&db, input);
    }

    #[tokio::test]
    async fn a_finished_job_is_not_cancelled_on_drop() {
        let pool = single_thread_pool();
        let db = TestDb::new();
        let cancel = db.cancellation_token();
        let outcome = spawn_on_pool(&pool, db, |_| ())
            .await
            .expect("sender dropped");
        assert!(matches!(outcome, PoolOutcome::Completed(())));
        assert!(!cancel.is_cancelled());
    }

    // --- Pool sizing ---

    #[test]
    fn default_pool_total_is_two_latency_and_three_worker_given_the_cores() {
        // The nominal default, on any machine with a core to spare for each of
        // the five plus the runtime thread.
        with_cpus(Some(6), || {
            assert_eq!(split_pool_total(default_pool_total()), (2, 3));
        });
        with_cpus(Some(64), || {
            assert_eq!(split_pool_total(default_pool_total()), (2, 3));
        });
        // An OS that will not report a count is no reason to shrink.
        with_cpus(None, || {
            assert_eq!(default_pool_total(), DEFAULT_POOL_THREADS);
        });
    }

    #[test]
    fn default_pool_total_leaves_a_cpu_for_the_runtime_thread() {
        // The point of the clamp: pools plus the runtime thread fit the machine.
        for available in 2..=DEFAULT_POOL_THREADS {
            with_cpus(Some(available), || {
                assert_eq!(
                    default_pool_total(),
                    available - RUNTIME_THREADS,
                    "{available} CPUs should leave one for the runtime thread"
                );
            });
        }
    }

    #[test]
    fn default_pool_total_still_gives_each_pool_a_thread_on_one_cpu() {
        // A single CPU leaves no budget at all, but a pool of zero means "one
        // per core" to rayon, so the floor has to win over the clamp.
        with_cpus(Some(1), || {
            assert_eq!(default_pool_total(), 0);
            assert_eq!(split_pool_total(default_pool_total()), (1, 1));
        });
    }

    #[test]
    fn pool_sizes_default_never_exceeds_the_nominal_default() {
        // Whatever this machine reports, the unset default is between the floor
        // and the nominal 2 + 3 — it may shrink to fit, never grow.
        let (latency, worker) = pool_sizes(None);
        assert!(latency >= 1 && worker >= 1);
        assert!(latency <= DEFAULT_LATENCY_THREADS);
        assert!(latency + worker <= DEFAULT_POOL_THREADS);
    }

    #[test]
    fn pool_sizes_default_matches_the_equivalent_explicit_total() {
        // Whatever the default came out as on this machine, it stays
        // expressible through the setting: `numThreads` counts the runtime
        // thread as well as the pools, so asking for that total by name gives
        // the same split. If the two ever diverge, one of them is describing the
        // wrong split.
        let (latency, worker) = pool_sizes(None);
        assert_eq!(
            pool_sizes(Some(latency + worker + RUNTIME_THREADS)),
            (latency, worker)
        );
    }

    #[test]
    fn pool_sizes_treats_zero_as_unset() {
        // The VS Code extension sends `numThreads: 0` whenever the user has not
        // set the option, and package.json documents 0 as "let the server
        // decide". Clamping it up to 1 instead would silently give the default
        // session a 1 + 1 split.
        assert_eq!(pool_sizes(Some(0)), pool_sizes(None));
    }

    #[test]
    fn pool_sizes_stays_within_the_request_concurrency_limit() {
        // The whole point of MAX_POOL_THREADS: at the ceiling the pools must
        // still be startable, which means no more worker threads than there are
        // handler slots to launch diagnostic pulls from. MAX_NUM_THREADS is that
        // plus the runtime thread, so the top of the setting's range reaches it
        // exactly.
        let (latency, worker) = pool_sizes(Some(MAX_NUM_THREADS));
        assert_eq!(latency + worker, MAX_POOL_THREADS);
        assert!(
            worker <= MAX_CONCURRENT_REQUESTS,
            "{worker} worker threads need {worker} concurrent pulls, but only \
             {MAX_CONCURRENT_REQUESTS} handlers run at once"
        );
    }

    #[test]
    fn oversubscription_note_is_quiet_at_or_below_the_cpu_count() {
        with_cpus(Some(8), || {
            // A total the machine can run must add nothing to the log line.
            assert_eq!(oversubscription_note(8), "");
            // One over is oversubscribed, and has to say so.
            assert!(oversubscription_note(9).contains("8 CPUs available here"));
        });
        // Nothing to compare against, so nothing to report.
        with_cpus(None, || assert_eq!(oversubscription_note(usize::MAX), ""));
    }

    #[test]
    fn cpu_limited_default_note_only_fires_when_the_default_was_trimmed() {
        // Both cases go through the same arithmetic as the `initialize` log,
        // which reports the two pool sizes rather than the pre-split total.
        let logged_total = || {
            let (latency, worker) = split_pool_total(default_pool_total());
            latency + worker + RUNTIME_THREADS
        };
        // A machine roomy enough for the nominal default logs nothing extra.
        with_cpus(Some(8), || {
            assert_eq!(cpu_limited_default_note(logged_total()), "");
        });
        // A two-CPU box gets one thread per pool and has to say why.
        with_cpus(Some(2), || {
            let note = cpu_limited_default_note(logged_total());
            assert!(note.contains("2 CPUs available"), "got: {note:?}");
            assert!(note.contains("numThreads"), "got: {note:?}");
        });
    }

    #[test]
    fn pool_sizes_honours_the_override_as_a_total_including_the_runtime_thread() {
        // Across the whole settable range, latency stays at 2 and the remainder
        // is worker, so the pools plus the runtime thread add up to exactly what
        // the user asked for.
        for total in MIN_NUM_THREADS..=MAX_NUM_THREADS {
            let (latency, worker) = pool_sizes(Some(total));
            assert_eq!(
                latency + worker + RUNTIME_THREADS,
                total,
                "numThreads={total} should split exactly"
            );
            assert!(latency <= DEFAULT_LATENCY_THREADS);
        }
    }

    #[test]
    fn pool_sizes_never_returns_zero() {
        // rayon reads num_threads(0) as "auto-detect from core count", so a
        // zero here would silently spawn a pool per core — the opposite of
        // what someone setting a low numThreads is asking for. `initialize`
        // never passes anything below MIN_NUM_THREADS, but the guarantee belongs
        // to this function rather than to its caller.
        for total in 0..=MAX_NUM_THREADS {
            let (latency, worker) = pool_sizes(Some(total));
            assert!(latency >= 1, "numThreads={total} gave {latency} latency");
            assert!(worker >= 1, "numThreads={total} gave {worker} worker");
        }
    }

    #[test]
    fn clamp_num_threads_passes_the_usable_range_through_silently() {
        assert_eq!(clamp_num_threads(0), (None, None));
        for n in MIN_NUM_THREADS..=MAX_NUM_THREADS {
            assert_eq!(clamp_num_threads(n as u64), (Some(n), None));
        }
    }

    #[test]
    fn clamp_num_threads_warns_about_a_total_below_the_minimum() {
        // 1 and 2 leave nothing, or not enough, once the protocol loop's thread
        // is taken out. They are settable in older clients, so the server has to
        // say what it did rather than silently run a different configuration.
        for n in 1..MIN_NUM_THREADS as u64 {
            let (clamped, warning) = clamp_num_threads(n);
            assert_eq!(clamped, Some(MIN_NUM_THREADS));
            let warning = warning.expect("a value below the minimum has to be reported");
            assert!(warning.contains(&format!("numThreads {n}")), "{warning}");
            assert!(
                warning.contains(&format!("the minimum is {MIN_NUM_THREADS}")),
                "{warning}"
            );
        }
    }

    #[test]
    fn clamp_num_threads_warns_about_a_total_above_the_maximum() {
        for n in [MAX_NUM_THREADS as u64 + 1, u64::MAX] {
            let (clamped, warning) = clamp_num_threads(n);
            assert_eq!(clamped, Some(MAX_NUM_THREADS));
            assert!(
                warning
                    .expect("a value above the maximum has to be reported")
                    .contains(&format!("the maximum is {MAX_NUM_THREADS}"))
            );
        }
    }
}
