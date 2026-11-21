//! The analysis session: the salsa database behind its lock, snapshots of it for
//! the pools, and the per-document inputs.

use ruff_db::system::SystemPathBuf;
use salsa::Setter;
use tower_lsp::jsonrpc::{Error, Result};
use tower_lsp::lsp_types::Url;

use super::HydraLspBackend;
use crate::database::HydraDatabase;
use crate::python::cache::{self as python_cache, PythonConfig};
use crate::yaml_cache::{self, DocumentInput, ParsedYaml};

/// Initialized analysis state owned by `HydraLspBackend`.
///
/// `db` lives behind a `Mutex` rather than an `RwLock` because `HydraDatabase`
/// (via `salsa::Storage`) is `Send` but not `Sync` — salsa's storage uses
/// interior mutability that is not safe to share by `&T` across threads.
/// The Mutex is taken only briefly: read handlers acquire it long enough to
/// clone a snapshot (an Arc-share of cached data, microseconds), and writers
/// acquire it long enough to mutate a salsa input. The expensive work runs
/// on independent snapshots outside the lock — see `Session::snapshot`.
///
pub(super) struct Session {
    pub(super) db: parking_lot::Mutex<HydraDatabase>,
    pub(super) python_config: PythonConfig,
}

impl Session {
    /// Cheap, read-only snapshot of the database for concurrent worker use.
    ///
    /// Holds the db lock only long enough to clone the salsa storage (an
    /// Arc-share of cached data — `HydraDatabase: Clone` via
    /// `salsa::Storage`). The returned `SessionSnapshot` is `Send` and
    /// survives independently of the write side; if a writer modifies an
    /// input after the snapshot is taken, salsa cancels in-flight reads on
    /// the snapshot at the next query boundary.
    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            db: self.db.lock().clone(),
            python_config: self.python_config,
        }
    }
}

/// Cheap, send-safe view of the analysis state. Produced by
/// `Session::snapshot`; consumed by handlers that hand the database to
/// `tokio::task::spawn_blocking`.
#[derive(Clone)]
pub(super) struct SessionSnapshot {
    pub(super) db: HydraDatabase,
    pub(super) python_config: PythonConfig,
}

impl HydraLspBackend {
    fn fallback_workspace_root() -> Result<String> {
        let failed = |reason: String| Error {
            code: Error::internal_error().code,
            message: format!("failed to determine workspace root from current directory: {reason}")
                .into(),
            data: None,
        };
        std::env::current_dir()
            .map_err(|error| failed(error.to_string()))?
            .into_os_string()
            .into_string()
            .map_err(|path| {
                failed(format!(
                    "non-utf8 path {}",
                    std::path::PathBuf::from(path).display()
                ))
            })
    }

    pub(super) fn initialize_session(
        &self,
        workspace_root: Option<String>,
        interpreter: Option<String>,
    ) -> Result<()> {
        let workspace_root = match workspace_root {
            Some(workspace_root) => workspace_root,
            None => Self::fallback_workspace_root()?,
        };
        let workspace_root = SystemPathBuf::from(workspace_root);
        let db = HydraDatabase::new(&workspace_root);
        let python_config =
            PythonConfig::frozen(&db, workspace_root, interpreter.map(SystemPathBuf::from));
        *self.session.write() = Some(Session {
            db: parking_lot::Mutex::new(db),
            python_config,
        });
        Ok(())
    }

    /// Take a cheap snapshot of the analysis state for use on a worker thread.
    ///
    /// Returns `None` (and logs) when the session has not yet been built by
    /// `initialize`. Holds the session read lock and the inner db lock only
    /// long enough to clone the salsa storage; the returned snapshot is
    /// `Send` and can be passed to `tokio::task::spawn_blocking` without
    /// forcing other handlers to wait and run sequentially.
    pub(super) fn snapshot(&self) -> Option<SessionSnapshot> {
        self.with_session("snapshot", Session::snapshot)
    }

    /// Run `f` with a borrow of the initialized session, returning `None`
    /// (and logging a warning that names `context`) when the session has
    /// not yet been built by `initialize`.
    ///
    /// The outer `RwLock` guard is held for the duration of `f`, but `f`
    /// only sees `&Session` — the inner db lock is acquired by `f` itself, so
    /// concurrent callers with different access patterns do not serialize on the
    /// outer lock.
    pub(super) fn with_session<T>(
        &self,
        context: &'static str,
        f: impl FnOnce(&Session) -> T,
    ) -> Option<T> {
        let session = self.session.read();
        if session.is_none() {
            // Drop the guard before logging so a slow tracing subscriber
            // (file write, stderr lock contention) does not stall every
            // other handler that needs the session.
            drop(session);
            tracing::warn!(context, "session not initialized; ignoring");
            return None;
        }
        Some(f(session.as_ref().expect("checked Some above")))
    }

    /// Python search-path roots that live outside the workspace folder(s).
    ///
    /// These are the site-packages and `.pth`-target directories the resolver
    /// reads from but that a workspace-relative watcher glob would miss (see
    /// `initialized`). Taken from the resolver's own search paths, so each
    /// watched root equals the path the resolver interns under. Nested paths
    /// are collapsed so a parent `**` watcher isn't duplicated by one of its
    /// children.
    pub(super) fn out_of_workspace_search_roots(&self) -> Vec<std::path::PathBuf> {
        self.with_session("out_of_workspace_search_roots", |s| {
            let db = s.db.lock();
            let workspace = s
                .python_config
                .workspace_root(&*db)
                .as_std_path()
                .to_path_buf();
            let environment = python_cache::resolver_environment(&*db, s.python_config);
            let roots: Vec<ruff_db::system::SystemPathBuf> =
                ty_module_resolver::system_module_search_paths(&*db, environment)
                    // Drop anything under the workspace (already covered by
                    // the workspace-relative glob).
                    .filter(|path| !path.as_std_path().starts_with(&workspace))
                    .map(ToOwned::to_owned)
                    .collect();
            ruff_db::system::deduplicate_nested_paths(roots)
                .map(|path| path.as_std_path().to_path_buf())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
    }

    /// Look up the cached `parsed_yaml` result for a URI, but only if the
    /// document is a Hydra file.
    ///
    /// Checks the cheap `is_hydra_file` salsa query first; if the document
    /// is not a Hydra file we skip the YAML parse entirely. Returns `None` when:
    /// - The URI has no `DocumentInput` (notification not yet seen).
    /// - The session is not initialized.
    /// - The document is not a Hydra file.
    ///
    /// On the warm path for a Hydra file, both salsa calls are O(1) cache
    /// hits. Used by every per-keystroke handler so a hover/etc. on an
    /// already-parsed document does no YAML re-parsing.
    pub(super) fn cached_parsed_yaml(
        &self,
        uri: &Url,
        context: &'static str,
    ) -> Option<ParsedYaml> {
        let input = self.document_input(uri)?;
        self.with_session(context, |s| {
            let db = s.db.lock();
            if yaml_cache::is_hydra_file(&*db, input) {
                Some(yaml_cache::parsed_yaml(&*db, input))
            } else {
                None
            }
        })
        .flatten()
    }

    /// The `DocumentInput` for `uri`. The shard guard is released before this
    /// returns, so a caller can take the db lock next without inverting the
    /// lock order `get_or_create_input` uses.
    pub(super) fn document_input(&self, uri: &Url) -> Option<DocumentInput> {
        self.document_inputs.get(uri).map(|entry| *entry)
    }

    /// Get or create a `DocumentInput` for a given URI.
    ///
    /// Holds the dashmap entry guard across the salsa write so that concurrent
    /// callers for the same URI cannot both observe a vacant entry and create
    /// distinct inputs (which would orphan one in salsa storage and let stale
    /// text leak through). Lock order: dashmap shard → `Session::db.lock()`.
    ///
    /// Returns `None` when the session is not yet initialized; callers
    /// (notification handlers) should treat that as a no-op. `context` names
    /// the caller in the no-session warning.
    pub(super) fn get_or_create_input(
        &self,
        uri: &Url,
        text: &str,
        version: i32,
        context: &'static str,
    ) -> Option<DocumentInput> {
        self.with_session(context, |s| match self.document_inputs.entry(uri.clone()) {
            dashmap::Entry::Occupied(occ) => {
                let input = *occ.get();
                let mut db = s.db.lock();
                input.set_text(&mut *db).to(text.to_string());
                input.set_version(&mut *db).to(version);
                input
            }
            dashmap::Entry::Vacant(vac) => {
                let db = s.db.lock();
                let input = DocumentInput::new(&*db, text.to_string(), version);
                vac.insert(input);
                input
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_db::system::SystemPath;

    /// Build a minimal on-disk venv so `discover_python_environment` returns a
    /// real site-packages directory. Discovery uses a real `OsSystem`, not the
    /// db's system, so this must be an actual filesystem layout. The returned
    /// `TempDir` must be kept alive for the venv to persist.
    fn session_with_real_venv() -> (Session, std::path::PathBuf, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let venv = tmp.path().to_path_buf();
        let site_packages = crate::python::test_support::write_venv(&venv, "3.11");

        let db = HydraDatabase::new(SystemPath::new(venv.to_str().unwrap()));
        let venv_path = SystemPathBuf::from(venv.to_string_lossy().into_owned());
        let python_config = PythonConfig::frozen(&db, venv_path.clone(), Some(venv_path));
        // Discovery canonicalizes the sys.prefix (e.g. macOS `/var` →
        // `/private/var`), so return the canonical path: simulated watch
        // events then name the same directory the resolver listed, exactly as
        // real events do.
        let site_packages = crate::python::test_support::canonicalize(&site_packages);
        let session = Session {
            db: parking_lot::Mutex::new(db),
            python_config,
        };
        (session, site_packages, tmp)
    }

    /// Syncing a created or deleted `.pth`, as `did_change_watched_files`
    /// does, is enough for the search paths to pick up the change.
    #[test]
    fn syncing_a_pth_file_refreshes_search_paths() {
        use ruff_db::files::File;

        let (session, site_packages, tmp) = session_with_real_venv();
        let editable = crate::python::test_support::canonicalize(tmp.path()).join("editable_src");
        std::fs::create_dir_all(&editable).unwrap();
        let pth = site_packages.join("editable.pth");
        let pth_sys = SystemPath::from_std_path(&pth).unwrap();

        let has_editable = |session: &Session| {
            let db = session.db.lock();
            let environment = python_cache::resolver_environment(&*db, session.python_config);
            ty_module_resolver::editable_search_paths(&*db, environment)
                .any(|path| path.as_std_path() == editable)
        };
        assert!(!has_editable(&session));

        std::fs::write(&pth, format!("{}\n", editable.display())).unwrap();
        File::sync_path(&mut *session.db.lock(), pth_sys);
        assert!(has_editable(&session), "a created .pth must be picked up");

        std::fs::remove_file(&pth).unwrap();
        File::sync_path(&mut *session.db.lock(), pth_sys);
        assert!(!has_editable(&session), "a deleted .pth must be dropped");
    }
}
