//! Diagnostics for a hydra document

use std::collections::HashSet;
use tower_lsp::lsp_types::*;

use super::HydraLspBackend;
use super::pools::PoolOutcome;
use crate::database::HydraDatabase;
use crate::diagnostics;
use crate::python::cache::PythonConfig;
use crate::rules::DiagnosticRule;
use crate::yaml_cache::{self, DocumentInput};

/// What to do with the result of the diagnostics validation task.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DiagAction {
    /// Publish these diagnostics to the client.
    Publish(Vec<Diagnostic>),
    /// Do nothing — the round was superseded (cancelled) or the receiver was
    /// dropped. Leaves any previously published diagnostics intact.
    Skip,
    /// The task panicked; log the message and publish nothing.
    LogPanic(String),
}

/// Map a diagnostics task outcome to a [`DiagAction`]. Pure so it can be tested
/// without a live `Client` or rayon pool.
pub(super) fn classify_diag_outcome(
    outcome: std::result::Result<
        PoolOutcome<Vec<Diagnostic>>,
        tokio::sync::oneshot::error::RecvError,
    >,
) -> DiagAction {
    match PoolOutcome::from_received(outcome) {
        PoolOutcome::Completed(diagnostics) => DiagAction::Publish(diagnostics),
        PoolOutcome::Cancelled => DiagAction::Skip,
        PoolOutcome::Panicked(msg) => DiagAction::LogPanic(msg),
    }
}

impl HydraLspBackend {
    /// Clear any diagnostics previously published for `uri`.
    fn clear_diagnostics(&self, uri: &Url) {
        self.outbox
            .publish_diagnostics(uri.clone(), Vec::new(), None);
    }

    /// Clear diagnostics only when the client relies on push.
    ///
    /// Pull clients own their diagnostic state (they stop requesting a closed
    /// document), so there is nothing for the server to clear.
    pub(super) fn clear_diagnostics_if_needed(&self, uri: &Url) {
        if self.negotiated().pull_diagnostics {
            return;
        }
        self.clear_diagnostics(uri);
    }

    /// Publish diagnostics only when the client relies on push.
    ///
    /// Pull clients fetch diagnostics via `textDocument/diagnostic`; pushing to
    /// them would be redundant, so this is a no-op for them.
    pub(super) fn publish_diagnostics_if_needed(&self, uri: &Url) {
        if self.negotiated().pull_diagnostics {
            return;
        }
        self.publish_diagnostics_for_document(uri);
    }

    /// After a document is opened or edited: publish its diagnostics if it is a
    /// Hydra file, or clear any previously published if it is not. Both only for
    /// a push client, and only while diagnostics are enabled. `context` names
    /// the caller in the no-session warning.
    pub(super) fn refresh_after_edit(
        &self,
        uri: &Url,
        input: DocumentInput,
        context: &'static str,
    ) {
        // Cached salsa check, so this is cheap on the warm path.
        let is_hydra = self
            .with_session(context, |s| {
                let db = s.db.lock();
                yaml_cache::is_hydra_file(&*db, input)
            })
            .unwrap_or(false);
        if self.settings.read().features.diagnostics {
            if is_hydra {
                self.publish_diagnostics_if_needed(uri);
            } else {
                self.clear_diagnostics_if_needed(uri);
            }
        }
    }

    /// Compute the diagnostics for `input` from the current database state.
    ///
    /// Returns `None` when there is no session. The parse and validation run
    /// while holding the db lock so no concurrent write can bump the salsa
    /// revision mid-compute — the same synchronous model ty uses for its push
    /// handler, which is why no cancellation (and therefore no retry) is
    /// possible here.
    fn compute_diagnostics(&self, input: DocumentInput) -> Option<Vec<Diagnostic>> {
        let disabled_rules = self.settings.read().disabled_rules.clone();
        self.with_session("compute diagnostics", |s| {
            // Hold the lock across the whole compute: with no interleaved
            // `set_text`, the revision can't move under us, so `parsed_yaml`
            // and `validate_document` never observe a `salsa::Cancelled`.
            let db = s.db.lock();
            validate_input(&db, input, &disabled_rules, s.python_config)
        })
    }

    /// Publish diagnostics for a document (push fallback for non-pull clients).
    ///
    /// Computes synchronously under the db lock (see [`compute_diagnostics`])
    /// and publishes the result. Because the compute can't be cancelled, there
    /// is no retry loop: a single pass always reflects the newest revision.
    ///
    /// [`compute_diagnostics`]: Self::compute_diagnostics
    fn publish_diagnostics_for_document(&self, uri: &Url) {
        // Callers (`did_open`, `did_change`) always create the input first, so a
        // missing entry is a programmer error rather than a runtime concern.
        // Copy the input out so the shard guard drops before the db lock is
        // taken (see the `document_inputs` lock order).
        let Some(input) = self.document_inputs.get(uri).map(|entry| *entry) else {
            debug_assert!(
                false,
                "publish_diagnostics_for_document called for {} with no DocumentInput; \
                 did_open/did_change should always create one first",
                uri
            );
            tracing::warn!(%uri, "no DocumentInput for diagnostics; skipping");
            return;
        };

        if let Some(diagnostics) = self.compute_diagnostics(input) {
            self.outbox
                .publish_diagnostics(uri.clone(), diagnostics, None);
        }
    }
}

/// Parse `input` and validate it: the diagnostics for one document, shared by
/// the push path (under the db lock) and the pull path (on a snapshot, on the
/// worker pool). The caller decides whether the document is a Hydra file.
pub(super) fn validate_input(
    db: &HydraDatabase,
    input: DocumentInput,
    disabled_rules: &HashSet<DiagnosticRule>,
    python_config: PythonConfig,
) -> Vec<Diagnostic> {
    let parsed_yaml = yaml_cache::parsed_yaml(db, input);
    match parsed_yaml.result() {
        Ok(content) => diagnostics::validate_document(content, disabled_rules, db, python_config),
        // A document that fails to parse has no suppression comments to read,
        // so `disabledRules` is the only way to silence this one.
        Err(_) if disabled_rules.contains(&DiagnosticRule::YamlSyntaxError) => Vec::new(),
        Err(e) => vec![yaml_syntax_error_diagnostic(e)],
    }
}

/// Build the single diagnostic shown for a YAML document that failed to parse.
fn yaml_syntax_error_diagnostic(msg: &str) -> Diagnostic {
    diagnostics::create_diagnostic(
        0,
        0,
        0,
        DiagnosticSeverity::ERROR,
        Some(DiagnosticRule::YamlSyntaxError),
        format!("YAML syntax error: {}", msg),
    )
}

/// An empty `Full` diagnostic report — nothing to show for this document.
pub(super) fn empty_full_report() -> DocumentDiagnosticReportResult {
    DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(
        RelatedFullDocumentDiagnosticReport::default(),
    ))
}

/// A stable `result_id` for a set of diagnostics, or `None` when there are none.
///
/// `lsp_types::Diagnostic` isn't `Hash`, so we hash a stable JSON serialization.
/// The client echoes this id back as `previous_result_id` on the next pull; an
/// unchanged id lets us answer `Unchanged` and skip re-sending the items.
fn diagnostics_result_id(diags: &[Diagnostic]) -> Option<String> {
    if diags.is_empty() {
        return None;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // Serialization is deterministic for a given value, so equal diagnostic
    // lists hash equally. Fall back to a length-based id if serialization ever
    // fails (it shouldn't for well-formed diagnostics).
    match serde_json::to_vec(diags) {
        Ok(bytes) => bytes.hash(&mut hasher),
        Err(_) => diags.len().hash(&mut hasher),
    }
    Some(format!("{:016x}", hasher.finish()))
}

/// Build a pull-diagnostic report, returning `Unchanged` when the freshly
/// computed `result_id` matches the client's `previous_result_id`.
pub(super) fn build_diagnostic_report(
    diagnostics: Vec<Diagnostic>,
    previous_result_id: Option<&str>,
) -> DocumentDiagnosticReportResult {
    let result_id = diagnostics_result_id(&diagnostics);
    let report = match &result_id {
        Some(new_id) if Some(new_id.as_str()) == previous_result_id => {
            DocumentDiagnosticReport::Unchanged(RelatedUnchangedDocumentDiagnosticReport {
                related_documents: None,
                unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                    result_id: new_id.clone(),
                },
            })
        }
        _ => DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
            related_documents: None,
            full_document_diagnostic_report: FullDocumentDiagnosticReport {
                result_id,
                items: diagnostics,
            },
        }),
    };
    DocumentDiagnosticReportResult::Report(report)
}

/// The LSP `ServerCancelled` error for a superseded pull-diagnostic round.
///
/// `retrigger_request: true` asks the client to re-pull, which replaces any
/// server-side retry loop with demand-driven retry.
pub(super) fn server_cancelled_error() -> tower_lsp::jsonrpc::Error {
    tower_lsp::jsonrpc::Error {
        code: tower_lsp::jsonrpc::ErrorCode::ServerError(-32802), // LSP ServerCancelled
        message: "diagnostics superseded by a newer revision".into(),
        data: serde_json::to_value(DiagnosticServerCancellationData {
            retrigger_request: true,
        })
        .ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::pools::{PoolOutcome, single_thread_pool, spawn_on_pool};
    use crate::database::tests::TestDb;
    use salsa::Setter;
    use std::time::Duration;

    // --- Test B: classify_diag_outcome (pure) ---

    #[test]
    fn classify_publishes_on_completion() {
        let diags: Vec<Diagnostic> = vec![Diagnostic::default()];
        assert_eq!(
            classify_diag_outcome(Ok(PoolOutcome::Completed(diags.clone()))),
            DiagAction::Publish(diags)
        );
    }

    #[test]
    fn classify_skips_on_cancellation() {
        assert_eq!(
            classify_diag_outcome(Ok(PoolOutcome::Cancelled)),
            DiagAction::Skip
        );
    }

    #[test]
    fn classify_skips_on_recv_error() {
        // A dropped sender is the only way to obtain a real RecvError.
        let (tx, rx) = tokio::sync::oneshot::channel::<PoolOutcome<Vec<Diagnostic>>>();
        drop(tx);
        let err = rx.blocking_recv().unwrap_err();
        assert_eq!(classify_diag_outcome(Err(err)), DiagAction::Skip);
    }

    #[test]
    fn classify_logs_on_panic() {
        assert_eq!(
            classify_diag_outcome(Ok(PoolOutcome::Panicked("kaboom".to_string()))),
            DiagAction::LogPanic("kaboom".to_string())
        );
    }

    // --- Test C2: parsed_yaml on the diagnostics route is cancel-safe ---

    /// Fix-guard (not a red-first repro): the diagnostics publisher must run its
    /// `yaml_cache::parsed_yaml` call *inside* `spawn_on_pool`, so a concurrent
    /// write that cancels it mid-flight surfaces as `PoolOutcome::Cancelled`
    /// (→ `DiagAction::Skip`) instead of an uncaught `salsa::Cancelled` panic on
    /// the task. This drives the exact parse the fix moves onto the pool:
    /// `parsed_yaml` on a cloned snapshot, cancelled by `set_text(&mut db)` on
    /// another thread. Because every salsa `fetch` begins with a cancellation
    /// check, looping `parsed_yaml` reliably observes the pending flag the
    /// blocked writer sets, exactly as the checkpoint loop in
    /// `pools::tests::salsa_cancellation_is_caught_and_classified`.
    #[tokio::test]
    async fn parsed_yaml_cancellation_on_pool_classifies_as_skip() {
        let pool = single_thread_pool();

        let mut db = TestDb::new();
        let input = DocumentInput::new(&db, "# @hydra\nmodel:\n  _target_: a.B\n".to_string(), 1);
        // Snapshot shares storage with `db`; a `&mut db` write cancels queries
        // running on this handle. `DocumentInput` is a `Copy` salsa id, so the
        // move-closure copy below leaves `input` usable by the writer thread.
        let snapshot = db.clone();

        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel::<()>();

        let rx = spawn_on_pool(&pool, move || {
            started_tx.send(()).expect("main receiver dropped");
            proceed_rx.recv().expect("main sender dropped");
            // Re-run the diagnostics-route parse until the concurrent writer
            // sets the cancellation flag; the next `fetch` then unwinds with
            // `salsa::Cancelled`. The bound + outer timeout keep a
            // mis-choreographed race from hanging.
            for _ in 0..100_000_000_u64 {
                let _ = yaml_cache::parsed_yaml(&snapshot, input);
                std::thread::yield_now();
            }
            panic!("cancellation flag was never observed");
        });

        started_rx.recv().expect("worker never started");
        // `set_text(&mut db)` blocks in salsa's cancel_others until the snapshot
        // handle is released, so it must run on its own thread.
        let writer = std::thread::spawn(move || {
            input
                .set_text(&mut db)
                .to("# @hydra\nmodel:\n  _target_: c.D\n".to_string());
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
        // The publisher relies on this exact classification to skip (not clear)
        // diagnostics for a superseded round.
        assert_eq!(classify_diag_outcome(Ok(outcome)), DiagAction::Skip);
        writer.join().expect("writer thread panicked");
    }

    // --- Pull-diagnostic report helpers ---

    fn sample_diagnostic(message: &str) -> Diagnostic {
        Diagnostic {
            message: message.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn diagnostics_result_id_is_none_for_empty_and_stable_for_equal() {
        assert_eq!(diagnostics_result_id(&[]), None);

        let a = vec![sample_diagnostic("boom")];
        let b = vec![sample_diagnostic("boom")];
        let c = vec![sample_diagnostic("different")];
        assert_eq!(diagnostics_result_id(&a), diagnostics_result_id(&b));
        assert_ne!(diagnostics_result_id(&a), diagnostics_result_id(&c));
    }

    #[test]
    fn build_diagnostic_report_full_then_unchanged() {
        let diags = vec![sample_diagnostic("boom")];
        let id = diagnostics_result_id(&diags).expect("non-empty diags have an id");

        // No previous id → Full report carrying the items and the fresh id.
        match build_diagnostic_report(diags.clone(), None) {
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(full)) => {
                assert_eq!(full.full_document_diagnostic_report.items, diags);
                assert_eq!(
                    full.full_document_diagnostic_report.result_id,
                    Some(id.clone())
                );
            }
            other => panic!("expected Full, got {other:?}"),
        }

        // Matching previous id → Unchanged, echoing the id, dropping the items.
        match build_diagnostic_report(diags, Some(&id)) {
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(u)) => {
                assert_eq!(u.unchanged_document_diagnostic_report.result_id, id);
            }
            other => panic!("expected Unchanged, got {other:?}"),
        }
    }

    #[test]
    fn empty_diagnostics_always_report_full() {
        // An empty set has no result_id, so even a repeat pull stays Full (never
        // Unchanged), correctly clearing any prior diagnostics on the client.
        match build_diagnostic_report(Vec::new(), Some("stale-id")) {
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(full)) => {
                assert!(full.full_document_diagnostic_report.items.is_empty());
                assert_eq!(full.full_document_diagnostic_report.result_id, None);
            }
            other => panic!("expected Full, got {other:?}"),
        }
    }

    #[test]
    fn server_cancelled_error_uses_lsp_code_and_retrigger() {
        let err = server_cancelled_error();
        assert!(matches!(
            err.code,
            tower_lsp::jsonrpc::ErrorCode::ServerError(-32802)
        ));
        let data = err.data.expect("cancellation carries data");
        let cancellation: DiagnosticServerCancellationData =
            serde_json::from_value(data).expect("valid cancellation data");
        assert!(cancellation.retrigger_request);
    }
}
