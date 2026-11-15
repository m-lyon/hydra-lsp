//! The analysis session: the salsa database behind its lock, snapshots of it for
//! the pools, and the per-document inputs.

use ruff_db::Db;
use ruff_db::files::Files;
use ruff_db::system::SystemPath;
use salsa::Setter;
use std::collections::HashSet;
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

    /// Bump the enclosing site-packages `FileRoot` revision for each changed
    /// directory so `parse_pth_files` re-scans it — but only when the directory
    /// is actually a registered library root (i.e. one of the resolver's
    /// site-packages search paths).
    ///
    /// Returns the number of directories actually bumped.
    pub(super) fn bump_site_packages_pth_roots(
        &self,
        directories: &HashSet<std::path::PathBuf>,
    ) -> usize {
        if directories.is_empty() {
            return 0;
        }
        let mut db = self.db.lock();
        let mut bumped = 0usize;
        for directory in directories {
            let Some(sys_path) = SystemPath::from_std_path(directory) else {
                continue;
            };
            // Only registered roots have a revision to bump; `root()` returns
            // `None` for directories that aren't library search paths, so this
            // naturally gates on site-packages membership.
            if db.files().root(&*db, sys_path).is_some() {
                Files::touch_root(&mut *db, sys_path);
                bumped += 1;
            }
        }
        bumped
    }
}

/// Cheap, send-safe view of the analysis state. Produced by
/// `Session::snapshot`; consumed by handlers that hand the database to
/// `tokio::task::spawn_blocking`.
#[derive(Clone)]
pub struct SessionSnapshot {
    pub db: HydraDatabase,
    pub python_config: PythonConfig,
}

impl HydraLspBackend {
    fn fallback_workspace_root() -> Result<String> {
        std::env::current_dir()
            .map_err(|error| Error {
                code: Error::internal_error().code,
                message: format!(
                    "failed to determine workspace root from current directory: {}",
                    error
                )
                .into(),
                data: None,
            })?
            .into_os_string()
            .into_string()
            .map_err(|path| Error {
                code: Error::internal_error().code,
                message: format!(
                    "failed to determine workspace root from current directory: non-utf8 path {}",
                    std::path::PathBuf::from(path).display()
                )
                .into(),
                data: None,
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
        let cwd = SystemPath::new(&workspace_root);
        let db = HydraDatabase::new(cwd);
        let python_config = PythonConfig::new(&db, Some(workspace_root), interpreter);
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
    /// `initialized`). Shares the exact `search_paths_for_config` list the
    /// resolver uses, so each watched root equals the path the resolver interns
    /// under. Nested paths are collapsed so a parent `**` watcher isn't
    /// duplicated by one of its children.
    pub(super) fn out_of_workspace_search_roots(&self) -> Vec<std::path::PathBuf> {
        self.with_session("computing watched search paths", |s| {
            let db = s.db.lock();
            let workspace = s
                .python_config
                .workspace_root(&*db)
                .as_deref()
                .map(std::path::PathBuf::from);
            let roots: Vec<ruff_db::system::SystemPathBuf> =
                python_cache::search_paths_for_config(&*db, s.python_config)
                    .iter()
                    // Drop the relative "." entry and anything under the
                    // workspace (already covered by the workspace-relative glob).
                    .filter(|path| path.is_absolute())
                    .filter(|path| workspace.as_ref().is_none_or(|w| !path.starts_with(w)))
                    .filter_map(|path| {
                        ruff_db::system::SystemPathBuf::from_path_buf(path.clone()).ok()
                    })
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
        let input = *self.document_inputs.get(uri)?;
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

    /// Get or create a `DocumentInput` for a given URI.
    ///
    /// Holds the dashmap entry guard across the salsa write so that concurrent
    /// callers for the same URI cannot both observe a vacant entry and create
    /// distinct inputs (which would orphan one in salsa storage and let stale
    /// text leak through). Lock order: dashmap shard → `Session::db.lock()`.
    ///
    /// Returns `None` when the session is not yet initialized; callers
    /// (notification handlers) should treat that as a no-op.
    pub(super) fn get_or_create_input(
        &self,
        uri: &Url,
        text: &str,
        version: i32,
    ) -> Option<DocumentInput> {
        self.with_session("opening documents", |s| {
            match self.document_inputs.entry(uri.clone()) {
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
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Test D: site-packages gating in bump_site_packages_pth_roots ---

    /// Build a minimal on-disk venv so `discover_python_environment` returns a
    /// real site-packages directory, and register its `FileRoot`s exactly as
    /// production does (via `python_cache::site_packages_paths`). Discovery uses
    /// a real `OsSystem` (not the db's system), so this must be an actual
    /// filesystem layout, not an in-memory one — it mirrors `python::env`'s
    /// in-memory `create_mock_venv`. The returned `TempDir` must be kept alive
    /// for the venv to persist.
    fn session_with_real_venv() -> (Session, std::path::PathBuf, tempfile::TempDir) {
        use std::fs;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let venv = tmp.path().to_path_buf();
        let version = "3.11";
        let (exe, site_packages) = if cfg!(target_os = "windows") {
            (
                venv.join(r"Scripts\python.exe"),
                venv.join(r"Lib\site-packages"),
            )
        } else {
            (
                venv.join("bin/python"),
                venv.join(format!("lib/python{version}/site-packages")),
            )
        };
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, "").unwrap();
        fs::create_dir_all(&site_packages).unwrap();
        fs::write(
            venv.join("pyvenv.cfg"),
            format!("home = {}\nversion = {version}\n", venv.display()),
        )
        .unwrap();

        let db = HydraDatabase::new(SystemPath::new(venv.to_str().unwrap()));
        let python_config = PythonConfig::new(
            &db,
            Some(venv.to_string_lossy().into_owned()),
            Some(venv.to_string_lossy().into_owned()),
        );
        // Register the discovered site-packages directories as `FileRoot`s,
        // exactly as the resolver does on its first query. Without this the
        // event side has no root to bump. Discovery canonicalizes the sys.prefix
        // (e.g. macOS `/var` → `/private/var`), so the registered root path is
        // the canonical one — return that so the simulated watch events route to
        // it lexically, exactly as real events (fired under the canonical root
        // the watcher registered) do.
        python_cache::site_packages_paths(&db, python_config);
        let site_packages = std::fs::canonicalize(&site_packages).expect("canonicalize venv");
        let session = Session {
            db: parking_lot::Mutex::new(db),
            python_config,
        };
        (session, site_packages, tmp)
    }

    /// A `.pth` change in the real site-packages dir bumps its root; a change in
    /// an unrelated directory in the same batch is skipped. Unrelated `.pth`
    /// files have no registered root, so they never trigger invalidation.
    #[test]
    fn bump_tracks_only_registered_site_packages_roots() {
        let (session, site_packages, _tmp) = session_with_real_venv();
        let unrelated = site_packages.parent().unwrap().join("not-site-packages");
        std::fs::create_dir_all(&unrelated).unwrap();

        let mut dirs = HashSet::new();
        dirs.insert(site_packages);
        dirs.insert(unrelated);

        let bumped = session.bump_site_packages_pth_roots(&dirs);
        assert_eq!(bumped, 1, "only the registered site-packages root bumps");
    }

    /// A `.pth` change with no registered root bumps nothing — an unrelated
    /// `foo.pth` anywhere in the watched tree is inert.
    #[test]
    fn bump_skips_unrelated_dir_entirely() {
        let (session, site_packages, _tmp) = session_with_real_venv();
        let unrelated = site_packages.parent().unwrap().join("scratch");
        std::fs::create_dir_all(&unrelated).unwrap();

        let mut dirs = HashSet::new();
        dirs.insert(unrelated);

        assert_eq!(session.bump_site_packages_pth_roots(&dirs), 0);
    }

    /// Repeated `.pth` events for the same real dir bump the root revision each
    /// time so `parse_pth_files` re-scans.
    #[test]
    fn bump_bumps_root_revision_across_repeated_events() {
        let (session, site_packages, _tmp) = session_with_real_venv();
        let mut dirs = HashSet::new();
        dirs.insert(site_packages.clone());

        let sys_path = SystemPath::from_std_path(&site_packages).unwrap();
        let rev0 = {
            let db = session.db.lock();
            db.files().root(&*db, sys_path).unwrap().revision(&*db)
        };

        assert_eq!(session.bump_site_packages_pth_roots(&dirs), 1);
        let rev1 = {
            let db = session.db.lock();
            db.files().root(&*db, sys_path).unwrap().revision(&*db)
        };

        assert_eq!(session.bump_site_packages_pth_roots(&dirs), 1);
        let rev2 = {
            let db = session.db.lock();
            db.files().root(&*db, sys_path).unwrap().revision(&*db)
        };

        assert!(
            rev1 != rev0 && rev2 != rev1,
            "each event must bump the root revision so the directory scan is recomputed"
        );
    }

    /// Routing is lexical, so a candidate path differing from the registered
    /// root only by a trailing separator must still match (`SystemPath::absolute`
    /// strips the trailing separator before the prefix-tree lookup). Regression
    /// guard for the path symmetry the `.pth` invalidation relies on.
    #[test]
    fn bump_matches_site_packages_despite_trailing_separator() {
        let (session, site_packages, _tmp) = session_with_real_venv();
        let mut with_sep = site_packages.into_os_string();
        with_sep.push(std::path::MAIN_SEPARATOR.to_string());

        let mut dirs = HashSet::new();
        dirs.insert(std::path::PathBuf::from(with_sep));

        assert_eq!(
            session.bump_site_packages_pth_roots(&dirs),
            1,
            "a trailing-separator variant of the site-packages path must still match"
        );
    }
}
