#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;

use super::env::{DiscoveredEnvironment, discover_python_environment};
use super::extract::{extract_class_attribute, extract_class_info, module_defines_top_level};
use super::import_resolver::{ImportResolver, find_module_file};
use super::model::{
    ClassAttributeInfo, DefinitionInfo, FunctionSignature, ResolveError, SourceRange,
};
use super::resolve::{base_class_name, resolve_base_class};
use super::stub_mapping::definition_with_source;
use super::typeshed::{BUILTINS_MODULE, is_runtime_builtin_name, is_stubbed_pypi_package};
use ruff_db::files::{File, FileRootKind};
use ruff_db::system::{SystemPath, SystemPathBuf};
use ruff_python_ast::PythonVersion;
use salsa::Durability;
use tracing::debug;
use ty_module_resolver::{
    ModuleName, ResolverEnvironment, ResolverFile, SearchPath, SearchPathSettings,
    UseDefaultStrategy, file_to_module, resolve_module_confident, system_module_search_paths,
};
use ty_python_core::Program;
use ty_python_core::platform::PythonPlatform;
use ty_python_semantic::PythonVersionWithSource;

/// Salsa input for Python environment configuration.
///
/// In the LSP backend, this is created during `initialize()` alongside the
/// analysis database, using the workspace root from LSP init params (or the
/// server's current directory) plus any configured Python interpreter.
///
/// This input participates in the cache key for Python definition lookups, so
/// changing its values invalidates cached module-resolution results. Per-file
/// invalidation for watched Python source and `.pth` files is handled
/// separately via `ruff_db::files::File::sync_path` in the LSP backend, which
/// also bumps the parent directory when a file is created or deleted.
#[salsa::input]
pub struct PythonConfig {
    /// Workspace root directory path.
    pub workspace_root: SystemPathBuf,

    /// Configured Python interpreter path.
    pub interpreter: Option<SystemPathBuf>,
}

impl PythonConfig {
    /// Create a config with salsa `Durability::NEVER_CHANGE` so that queries reading it
    /// never need revalidating.
    pub fn frozen(
        db: &dyn salsa::Database,
        workspace_root: SystemPathBuf,
        interpreter: Option<SystemPathBuf>,
    ) -> Self {
        Self::builder(workspace_root, interpreter)
            .durability(Durability::NEVER_CHANGE)
            .new(db)
    }
}

/// Interned target string for cache key deduplication.
///
/// Salsa interns these so that repeated lookups for the same `_target_`
/// value (e.g., "myapp.models.MyModel") share a single allocation
/// and use cheap identity comparison as the cache key.
#[salsa::interned]
pub struct TargetString {
    pub value: String,
}

/// The salsa key for a class: the file it is defined in and its name.
#[salsa::interned]
pub(super) struct ClassKey {
    #[returns(copy)]
    pub file: File,
    pub class: String,
}

/// An interned dotted module name, such as `pkg.models`.
#[salsa::interned]
pub(super) struct ModuleRef {
    pub name: String,
}

/// An interned bare symbol or attribute name.
#[salsa::interned]
pub(super) struct SymbolName {
    pub value: String,
}

/// Bases that exist only for the type system: they carry no constructor,
/// docstring or attributes of their own, so MRO walks skip them.
const TYPE_ONLY_BASES: &[&str] = &["object", "ABC", "Protocol", "Generic"];

/// Successfully resolved Python definition data.
///
/// `definition_info` and `file` come from the typing view, which prefers a
/// stub, so signatures are the ones a type checker sees. When `file` is a stub,
/// `source` locates the runtime implementation behind it, and
/// `definition_info` already has its docstring if the stub had none.
/// See [`super::stub_mapping`].
#[derive(Clone)]
pub struct ResolvedDefinition {
    pub definition_info: DefinitionInfo,
    pub file: File,
    pub source: Option<SourceDefinition>,
    pub module_name: String,
    pub symbol_name: String,
}

/// Where the runtime implementation behind a stub definition is: one range
/// per definition of the name in `file`, such as each overload.
#[derive(Clone, Debug)]
pub struct SourceDefinition {
    pub file: File,
    pub ranges: Vec<SourceRange>,
}

/// Cached result of `extract_definition_info`.
///
/// Uses Arc + pointer-based equality (same pattern as `ParsedYaml`)
/// to satisfy salsa's Update requirements without expensive deep comparison.
#[derive(Clone)]
pub struct DefinitionResult {
    inner: Arc<Result<ResolvedDefinition, ResolveError>>,
}

impl DefinitionResult {
    fn from_result(result: Result<ResolvedDefinition, ResolveError>) -> Self {
        Self {
            inner: Arc::new(result),
        }
    }

    /// Get the cached result.
    pub fn get(&self) -> Result<&ResolvedDefinition, &ResolveError> {
        self.inner.as_ref().as_ref()
    }
}

impl PartialEq for DefinitionResult {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for DefinitionResult {}

/// Cached Python environment discovery for a `PythonConfig`.
///
/// Recomputed only when `PythonConfig`'s `workspace_root` or `interpreter`
/// change. Also registers each site-packages directory as a `SearchPath` file
/// root so library files get `Durability::HIGH`: project-file edits don't
/// invalidate library-derived memos.
#[salsa::tracked(returns(ref))]
pub(super) fn discovered_environment(
    db: &dyn ruff_db::Db,
    config: PythonConfig,
) -> DiscoveredEnvironment {
    let workspace_root = config.workspace_root(db);
    let interpreter = config.interpreter(db);
    let discovered =
        discover_python_environment(workspace_root, interpreter.as_deref()).unwrap_or_default();
    for path in discovered
        .site_packages
        .iter()
        .chain(discovered.real_stdlib.as_ref())
    {
        db.files()
            .try_add_root(db, path.as_path(), FileRootKind::SearchPath);
    }
    discovered
}

/// The module-resolution environment for a `PythonConfig`: the workspace as
/// the first-party root, the discovered site-packages and real stdlib, and the
/// interpreter's Python version.
///
/// Editable installs are not part of this value. `ty_module_resolver` reads
/// the `.pth` files lazily through salsa, so creating, editing or deleting one
/// invalidates only the resolutions that depend on it.
#[salsa::tracked(returns(copy))]
pub(crate) fn resolver_environment(
    db: &dyn ty_module_resolver::Db,
    config: PythonConfig,
) -> ResolverEnvironment<'_> {
    let discovered = discovered_environment(db, config);
    let cwd = db.system().current_directory();
    let workspace_root = SystemPath::absolute(config.workspace_root(db), cwd);
    db.files()
        .try_add_root(db, &workspace_root, FileRootKind::Project);
    resolver_environment_from_parts(
        db,
        vec![workspace_root],
        discovered.site_packages.clone(),
        discovered.real_stdlib.clone(),
        discovered.python_version.version,
    )
}

/// The `ty_python_core::Program` for a `PythonConfig`. It wraps
/// [`resolver_environment`], so ty resolves modules over the same search paths and
/// parses files under the same `PythonFile` keys as hydrust does.
#[salsa::tracked(returns(copy))]
pub(crate) fn program(db: &dyn ty_python_core::Db, config: PythonConfig) -> Program<'_> {
    Program::new(
        db,
        PythonPlatform::default(),
        resolver_environment(db, config),
    )
}

pub(crate) fn python_version_with_source(
    db: &dyn ruff_db::Db,
    config: PythonConfig,
) -> &PythonVersionWithSource {
    &discovered_environment(db, config).python_version
}

/// Build a resolver environment from explicit roots. Directories that do not
/// exist are skipped.
///
/// `real_stdlib` is the interpreter's stdlib directory. ty searches it only in
/// runtime mode, in place of the typeshed stubs.
pub(super) fn resolver_environment_from_parts(
    db: &dyn ty_module_resolver::Db,
    src_roots: Vec<SystemPathBuf>,
    site_packages: Vec<SystemPathBuf>,
    real_stdlib: Option<SystemPathBuf>,
    python_version: PythonVersion,
) -> ResolverEnvironment<'_> {
    let settings = SearchPathSettings {
        src_roots,
        site_packages_paths: site_packages,
        real_stdlib_path: real_stdlib,
        ..SearchPathSettings::empty()
    };
    let Ok(search_paths) =
        settings.to_search_paths(db.system(), db.vendored(), &UseDefaultStrategy);
    ResolverEnvironment::new(db, python_version, search_paths)
}

/// A resolver environment whose first-party roots are `roots`, in order, with
/// no site-packages. Relative roots are taken against the database's current
/// directory.
#[cfg(test)]
pub(super) fn resolver_environment_for_roots<'db>(
    db: &'db dyn ty_module_resolver::Db,
    roots: &[PathBuf],
) -> ResolverEnvironment<'db> {
    let cwd = db.system().current_directory();
    let src_roots = roots
        .iter()
        .map(|root| SystemPath::from_std_path(root).expect("search root is UTF-8"))
        .map(|root| SystemPath::absolute(root, cwd))
        .collect();
    resolver_environment_from_parts(db, src_roots, Vec::new(), None, PythonVersion::latest_ty())
}

/// [`resolved_module_file`] for a module name given as a string.
pub(super) fn resolve_module_by_name<'db>(
    db: &'db dyn ty_module_resolver::Db,
    module: &str,
    environment: ResolverEnvironment<'db>,
) -> Option<File> {
    resolved_module_file(db, ModuleRef::new(db, module.to_string()), environment)
}

#[salsa::tracked(returns(copy), lru = 1024)]
pub(super) fn resolved_module_file<'db>(
    db: &'db dyn ty_module_resolver::Db,
    module: ModuleRef<'db>,
    environment: ResolverEnvironment<'db>,
) -> Option<File> {
    let module_path_str = module.name(db);
    let module_name = ModuleName::new(module_path_str)?;
    let module = resolve_module_confident(db, environment, &module_name)?;

    // Every other stdlib module resolves to its stub, which signatures come
    // from. `stub_mapping` finds the implementation for docs and navigation.
    if is_stubbed_pypi_package(module_path_str)
        && module
            .search_path(db)
            .is_some_and(SearchPath::is_standard_library)
    {
        return system_module_search_paths(db, environment)
            .find_map(|root| find_module_file(db, &root.join(module_path_str)));
    }

    module.file(db)
}

/// Whether the runtime module `module` exposes `symbol`, according to its
/// vendored stub.
#[salsa::tracked(returns(copy))]
pub(super) fn vendored_module_exposes<'db>(
    db: &'db dyn ty_module_resolver::Db,
    module: ModuleRef<'db>,
    symbol: SymbolName<'db>,
    environment: ResolverEnvironment<'db>,
) -> bool {
    let symbol = symbol.value(db);
    if symbol.is_empty() || symbol.contains('.') || !is_runtime_builtin_name(symbol) {
        return false;
    }
    let Some(stub) = resolved_module_file(db, module, environment) else {
        return false;
    };
    if stub.path(db).as_vendored_path().is_none() {
        return false;
    }

    module_defines_top_level(db, stub, symbol, environment.python_version(db))
}

/// Whether `name` is a symbol the runtime `builtins` module exposes.
pub(crate) fn is_builtin_symbol<'db>(
    db: &'db dyn ty_module_resolver::Db,
    name: &str,
    environment: ResolverEnvironment<'db>,
) -> bool {
    vendored_module_exposes(
        db,
        ModuleRef::new(db, BUILTINS_MODULE.to_string()),
        SymbolName::new(db, name.to_string()),
        environment,
    )
}

/// Cached extraction of Python definition info for a `_target_` string, with
/// the runtime implementation behind it when it resolved to a stub.
///
/// Wraps `definition_with_source` with salsa caching.
/// The result is cached and only recomputed when:
/// - The target string changes (different `_target_` value)
/// - The Python configuration changes (interpreter, workspace root, or
///   site-packages `.pth` state handles)
///
/// Uses `lru=1024` to bound memory for workspaces with many distinct targets.
/// Hydra projects often reference many `_target_` strings (e.g. one per config),
/// so the bound is set well above typical workspace sizes to avoid thrashing.
#[salsa::tracked(returns(clone), no_eq, lru = 1024)]
pub fn resolved_definition<'db>(
    db: &'db dyn ty_python_semantic::Db,
    config: PythonConfig,
    target: TargetString<'db>,
) -> DefinitionResult {
    let target_str = target.value(db);
    debug!(
        target = target_str,
        "resolved_definition: executing (cache miss)"
    );

    let environment = resolver_environment(db, config);
    let mut result = definition_with_source(db, target_str, environment);

    // A stub declares names its module does not have at run time, such as the
    // typevars its annotations use. With no runtime source that defines the
    // name, a private one is taken to be one of those.
    if let Ok(def) = &result
        && def.source.is_none()
        && let Some(module) = vendored_module_name(db, def.file, environment)
        && let Some(root_symbol) = target_str
            .strip_prefix(&module)
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|rest| rest.split('.').next())
        && !vendored_module_exposes(
            db,
            ModuleRef::new(db, module.clone()),
            SymbolName::new(db, root_symbol.to_string()),
            environment,
        )
    {
        result = Err(ResolveError::UnresolvedSymbol {
            module,
            symbol: root_symbol.to_string(),
        });
    }

    DefinitionResult::from_result(result)
}

/// The dotted name of the module the vendored stub `file` stands for, such as
/// `builtins` for `stdlib/builtins.pyi`. `None` for a file on disk.
///
/// Lets a caller that has only a resolved file recover which module it came
/// from, rather than assuming the target's own prefix names it.
fn vendored_module_name(
    db: &dyn ty_module_resolver::Db,
    file: File,
    environment: ResolverEnvironment<'_>,
) -> Option<String> {
    file.path(db).as_vendored_path()?;
    let module = file_to_module(db, ResolverFile::new(db, file, environment))?;
    Some(module.name(db).to_string())
}

/// Cached docstring + `__init__` resolution for a class, walking its MRO.
///
/// Salsa memoises each `(class_key, environment)` pair, so parent classes
/// shared across different child classes are resolved at most once per revision.
/// The own class properties take priority; parent properties fill in only what
/// is missing.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ClassMembers {
    docstring: Option<String>,
    init_signature: Option<FunctionSignature>,
    new_signature: Option<FunctionSignature>,
    all_bases_resolved: bool,
}

#[derive(Clone, Debug)]
pub(super) struct InheritedClassMembers {
    inner: Arc<ClassMembers>,
}

impl InheritedClassMembers {
    fn new(
        docstring: Option<String>,
        init_signature: Option<FunctionSignature>,
        new_signature: Option<FunctionSignature>,
        all_bases_resolved: bool,
    ) -> Self {
        Self {
            inner: Arc::new(ClassMembers {
                docstring,
                init_signature,
                new_signature,
                all_bases_resolved,
            }),
        }
    }

    /// Resolved docstring — from the class itself or the nearest ancestor that has one.
    pub fn docstring(&self) -> Option<&String> {
        self.inner.docstring.as_ref()
    }

    /// Resolved `__init__` signature — from the class itself or the nearest ancestor.
    pub fn init_signature(&self) -> Option<&FunctionSignature> {
        self.inner.init_signature.as_ref()
    }

    /// Resolved `__new__` signature — from the class itself or the nearest ancestor.
    ///
    /// Only meaningful when [`init_signature`](Self::init_signature) is `None`; see
    /// [`ClassInfo::new_signature`](crate::python::model::ClassInfo::new_signature).
    pub fn new_signature(&self) -> Option<&FunctionSignature> {
        self.inner.new_signature.as_ref()
    }

    /// Whether every base class along the walked MRO was found.
    pub fn all_bases_resolved(&self) -> bool {
        self.inner.all_bases_resolved
    }

    /// Fill each `None` slot from these docs.
    pub(super) fn fill_missing(
        &self,
        docstring: &mut Option<String>,
        init: &mut Option<FunctionSignature>,
        new_signature: &mut Option<FunctionSignature>,
    ) {
        fill(docstring, self.docstring());
        fill(init, self.init_signature());
        fill(new_signature, self.new_signature());
    }
}

fn fill<T: Clone>(slot: &mut Option<T>, from: Option<&T>) {
    if slot.is_none() {
        *slot = from.cloned();
    }
}

/// The key of each base class in `base_classes` that the MRO walks visit, or
/// `None` for a base that did not resolve.
fn parent_keys<'db, 'a>(
    db: &'db dyn ty_module_resolver::Db,
    file: File,
    base_classes: &'a [String],
    environment: ResolverEnvironment<'db>,
) -> impl Iterator<Item = Option<ClassKey<'db>>> + 'a
where
    'db: 'a,
{
    base_classes
        .iter()
        .filter(|base| !TYPE_ONLY_BASES.contains(&base_class_name(base)))
        .map(move |base| {
            let base = resolve_base_class(db, file, base, environment)?;
            Some(ClassKey::new(db, base.file, base.name))
        })
}

impl PartialEq for InheritedClassMembers {
    /// Value equality (deep-compares the resolved docstring and `__init__`).
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}
impl Eq for InheritedClassMembers {}

/// A class attribute and the class in the MRO that assigns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct InheritedAttribute {
    pub info: ClassAttributeInfo,
    pub file: File,
    pub class: String,
}

/// Cached result of looking up a class attribute by walking the MRO.
///
/// `None` inner value means the attribute was not found on the class or any parent.
#[derive(Clone, Debug)]
pub(super) struct ClassAttributeLookup {
    inner: Arc<Option<InheritedAttribute>>,
}

impl ClassAttributeLookup {
    fn found(info: ClassAttributeInfo, file: File, class: String) -> Self {
        Self {
            inner: Arc::new(Some(InheritedAttribute { info, file, class })),
        }
    }

    fn not_found() -> Self {
        Self {
            inner: Arc::new(None),
        }
    }

    /// The attribute, if the class or a parent assigns it.
    pub fn get(&self) -> Option<&InheritedAttribute> {
        self.inner.as_ref().as_ref()
    }
}

impl PartialEq for ClassAttributeLookup {
    /// Value equality (deep-compares the resolved attribute, file path, and class).
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}
impl Eq for ClassAttributeLookup {}

/// Cached docstring + `__init__` resolution for one class, including its MRO.
///
/// The query calls `extract_class_info` for the class itself and, for any missing property,
/// recursively invokes itself for each parent (MRO order, left-to-right).
/// Salsa memoises every node independently, so a grandparent shared by two
/// sibling classes is resolved only once per revision.
///
/// `cycle_result` handles circular inheritance (invalid Python) gracefully by
/// returning empty docs rather than panicking.
#[salsa::tracked(returns(clone), lru = 512, cycle_result = inherited_class_members_cycle)]
pub(super) fn inherited_class_members<'db>(
    db: &'db dyn ty_module_resolver::Db,
    class_key: ClassKey<'db>,
    environment: ResolverEnvironment<'db>,
) -> InheritedClassMembers {
    let file = class_key.file(db);
    let class_name = class_key.class(db).as_str();
    let python_version = environment.python_version(db);

    let class_info = match extract_class_info(db, file, class_name, python_version) {
        Some(class_info) => class_info,
        None => {
            return match ImportResolver::new(db, environment).resolve_symbol(file, class_name) {
                Some(resolved) => {
                    let resolved_key = ClassKey::new(db, resolved.file, resolved.name);
                    if resolved_key == class_key {
                        return InheritedClassMembers::new(None, None, None, false);
                    }
                    inherited_class_members(db, resolved_key, environment)
                }
                None => InheritedClassMembers::new(None, None, None, false),
            };
        }
    };

    let mut docstring = class_info.docstring;
    let mut init = class_info.init_signature;
    let mut new_signature = class_info.new_signature;

    if docstring.is_some() && init.is_some() {
        return InheritedClassMembers::new(docstring, init, new_signature, true);
    }

    // Tracks whether the walk saw the whole hierarchy. Only meaningful when it
    // finds no `__init__`.
    let mut all_bases_resolved = true;

    for parent_key in parent_keys(db, file, &class_info.base_classes, environment) {
        let Some(parent_key) = parent_key else {
            all_bases_resolved = false;
            continue;
        };
        let parent_docs = inherited_class_members(db, parent_key, environment);
        all_bases_resolved &= parent_docs.all_bases_resolved();
        parent_docs.fill_missing(&mut docstring, &mut init, &mut new_signature);
        if docstring.is_some() && init.is_some() {
            break;
        }
    }

    InheritedClassMembers::new(docstring, init, new_signature, all_bases_resolved)
}

fn inherited_class_members_cycle(
    _db: &dyn ty_module_resolver::Db,
    _id: salsa::Id,
    _class_key: ClassKey,
    _environment: ResolverEnvironment,
) -> InheritedClassMembers {
    InheritedClassMembers::new(None, None, None, true)
}

/// Cached class-attribute lookup, walking the MRO when not found directly.
///
/// Each `(class_key, attribute_key, environment)` triple is memoised, so
/// shared parent classes are looked up at most once per revision.
///
/// `cycle_result` handles circular inheritance gracefully.
#[salsa::tracked(returns(clone), lru = 512, cycle_result = class_parent_attribute_cycle)]
pub(super) fn class_parent_attribute<'db>(
    db: &'db dyn ty_module_resolver::Db,
    class_key: ClassKey<'db>,
    attribute_key: SymbolName<'db>,
    environment: ResolverEnvironment<'db>,
) -> ClassAttributeLookup {
    let file = class_key.file(db);
    let class_name = class_key.class(db).as_str();
    let attribute_name = attribute_key.value(db);
    let python_version = environment.python_version(db);

    if let Some(attr_info) =
        extract_class_attribute(db, file, class_name, attribute_name, python_version)
    {
        return ClassAttributeLookup::found(attr_info, file, class_name.to_string());
    }

    let Some(class_info) = extract_class_info(db, file, class_name, python_version) else {
        return ClassAttributeLookup::not_found();
    };

    for parent_key in parent_keys(db, file, &class_info.base_classes, environment).flatten() {
        let result = class_parent_attribute(db, parent_key, attribute_key, environment);
        if result.get().is_some() {
            return result;
        }
    }

    ClassAttributeLookup::not_found()
}

fn class_parent_attribute_cycle(
    _db: &dyn ty_module_resolver::Db,
    _id: salsa::Id,
    _class_key: ClassKey,
    _attribute_key: SymbolName,
    _environment: ResolverEnvironment,
) -> ClassAttributeLookup {
    ClassAttributeLookup::not_found()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::HydraDatabase;
    use crate::database::tests::TestDb;
    use crate::python::model::ParameterKind;
    use crate::python::test_support::{
        canonicalize, filesystem_root, get_simple_test_dir, sig_for, sig_param, system_file,
        system_path, test_db, write_file, write_venv,
    };
    use ruff_db::Db;
    use ruff_db::files::{FilePath, system_path_to_file};
    use ruff_db::vendored::VendoredPathBuf;
    use salsa::Setter;
    use std::path::Path;

    /// The path of the vendored `builtins` stub.
    fn builtins_stub() -> FilePath {
        vendored("stdlib/builtins.pyi")
    }

    /// The path of the vendored typeshed file at `path`.
    fn vendored(path: &str) -> FilePath {
        VendoredPathBuf::from(path).into()
    }

    /// The path of the file at `path` on disk.
    fn on_disk(path: &Path) -> FilePath {
        system_path(path).into()
    }

    #[test]
    fn test_class_key_is_shared_across_path_spellings() {
        use ruff_db::system::DbWithWritableSystem;

        let mut db = TestDb::new();
        db.write_file("/work/models.py", "").expect("write models");
        let file = |path: &str| system_path_to_file(&db, path).expect("file exists");

        let key = ClassKey::new(&db, file("/work/pkg/../models.py"), "Model");
        assert_eq!(
            key.file(&db).path(&db),
            &FilePath::system("/work/models.py")
        );
        assert_eq!(key.class(&db), "Model");
        assert!(key == ClassKey::new(&db, file("/work/./models.py"), "Model"));
    }

    /// The editable roots the `.pth` files in `site_packages` list.
    fn editables(db: &HydraDatabase, site_packages: &Path) -> Vec<PathBuf> {
        let environment = resolver_environment_from_parts(
            db,
            Vec::new(),
            vec![SystemPathBuf::from_path_buf(site_packages.to_path_buf()).unwrap()],
            None,
            PythonVersion::latest_ty(),
        );
        ty_module_resolver::editable_search_paths(db, environment)
            .map(|path| path.as_std_path().to_path_buf())
            .collect()
    }

    #[test]
    fn test_discovered_environment_cache_hit() {
        let db = test_db();
        let examples = get_simple_test_dir();
        let config = PythonConfig::frozen(&db, system_path(&examples), None);
        let result1 = discovered_environment(&db, config);
        let result2 = discovered_environment(&db, config);
        // Pointer equality: salsa returns the same value on a cache hit.
        assert!(std::ptr::eq(result1 as *const _, result2 as *const _));
    }

    #[test]
    fn test_discovered_environment_invalidation_on_interpreter_change() {
        let mut db = test_db();
        let examples = get_simple_test_dir();
        // Mutable on purpose: production configs are frozen, but the queries
        // must still recompute if a config ever changes.
        let config = PythonConfig::new(&db, system_path(&examples), None);
        let _ = discovered_environment(&db, config);

        // Changing the interpreter invalidates the memo: a nonexistent
        // interpreter fails discovery, so the result is empty.
        config
            .set_interpreter(&mut db)
            .to(Some(SystemPathBuf::from("/nonexistent/python")));
        assert_eq!(
            discovered_environment(&db, config),
            &DiscoveredEnvironment::default()
        );
    }

    #[test]
    fn test_resolver_environment_puts_the_workspace_root_first() {
        let db = test_db();
        let examples = get_simple_test_dir();
        let config = PythonConfig::frozen(&db, system_path(&examples), None);
        let environment = resolver_environment(&db, config);
        let first = ty_module_resolver::system_module_search_paths(&db, environment).next();
        assert_eq!(
            first.map(SystemPath::as_std_path),
            Some(examples.as_path()),
            "workspace root should be the first search path"
        );
    }

    /// Without a discoverable version the parser falls back to the latest one.
    #[test]
    fn test_resolver_environment_defaults_to_the_latest_version() {
        let db = test_db();
        let config = PythonConfig::frozen(
            &db,
            system_path(&get_simple_test_dir()),
            Some(SystemPathBuf::from("/nonexistent/python")),
        );
        assert_eq!(
            resolver_environment(&db, config).python_version(&db),
            PythonVersion::latest_ty()
        );
        assert_eq!(
            python_version_with_source(&db, config).source,
            ty_python_semantic::PythonVersionSource::Default
        );
    }

    #[test]
    fn test_resolver_environment_uses_the_venv_version() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let venv = canonicalize(tmp.path());
        write_venv(&venv, "3.9.18");

        let db = test_db();
        let venv = system_path(&venv);
        let config = PythonConfig::frozen(&db, venv.clone(), Some(venv));
        assert_eq!(
            resolver_environment(&db, config).python_version(&db),
            PythonVersion::PY39
        );
    }

    /// ty must parse under the version hydrust parses under, or each file is
    /// parsed twice, under two `PythonFile` keys.
    #[test]
    fn test_program_uses_the_resolver_environment() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let venv = canonicalize(tmp.path());
        write_venv(&venv, "3.9.18");
        let module = venv.join("module.py");
        write_file(&module, "x = 1\n");

        let mut db = test_db();
        let venv = system_path(&venv);
        let config = PythonConfig::frozen(&db, venv.clone(), Some(venv));
        db.set_python_config(config);

        let environment = resolver_environment(&db, config);
        assert_eq!(program(&db, config).resolver_environment(&db), environment);
        let file = ruff_db::files::system_path_to_file(&db, system_path(&module)).unwrap();
        assert_eq!(
            ty_python_semantic::Db::program_file(&db, file).python_file(&db),
            ruff_db::PythonFile::new(&db, file, environment.python_version(&db))
        );

        let version = ty_python_semantic::Db::python_version_with_source(&db, file);
        assert_eq!(version.version, PythonVersion::PY39);
        assert!(
            matches!(
                version.source,
                ty_python_semantic::PythonVersionSource::PyvenvCfgFile(_)
            ),
            "the version should come from pyvenv.cfg, got {version:?}"
        );
    }

    #[test]
    fn test_python_config_fields() {
        let db = TestDb::new();
        let config = PythonConfig::frozen(&db, SystemPathBuf::from("/workspace"), None);
        assert_eq!(
            config.workspace_root(&db).as_path(),
            SystemPath::new("/workspace")
        );
        assert_eq!(config.interpreter(&db).as_deref(), None);
    }

    #[test]
    #[should_panic]
    fn test_frozen_python_config_rejects_setters() {
        let mut db = TestDb::new();
        let config = PythonConfig::frozen(&db, SystemPathBuf::from("/workspace"), None);
        config
            .set_interpreter(&mut db)
            .to(Some(SystemPathBuf::from("/usr/bin/python3")));
    }

    #[test]
    fn test_target_string_interning() {
        let db = TestDb::new();
        let t1 = TargetString::new(&db, "my.Module".to_string());
        let t2 = TargetString::new(&db, "my.Module".to_string());
        let t3 = TargetString::new(&db, "other.Module".to_string());

        // Same string → same interned ID
        assert!(t1 == t2);
        // Different string → different ID
        assert!(t1 != t3);
        // Value round-trips
        assert_eq!(t1.value(&db), "my.Module");
    }

    #[test]
    fn test_cached_definition_info_valid_target() {
        let db = test_db();
        let workspace = get_simple_test_dir();
        let config = PythonConfig::frozen(&db, system_path(&workspace), None);
        let target = TargetString::new(&db, "my_module.DataLoader".to_string());
        let result = resolved_definition(&db, config, target);

        let def = result.get().expect("should resolve DataLoader");
        assert_eq!(def.symbol_name, "DataLoader");
        assert!(matches!(def.definition_info, DefinitionInfo::Class(_)));
    }

    #[test]
    fn test_cached_definition_info_invalid_target() {
        let db = TestDb::new();
        let workspace = get_simple_test_dir();
        let config = PythonConfig::frozen(&db, system_path(&workspace), None);
        let target = TargetString::new(&db, "nonexistent.Module".to_string());
        let result = resolved_definition(&db, config, target);
        assert!(result.get().is_err());
    }

    #[test]
    fn test_cached_definition_info_cache_hit() {
        let db = test_db();
        let workspace = get_simple_test_dir();
        let config = PythonConfig::frozen(&db, system_path(&workspace), None);
        let target = TargetString::new(&db, "my_module.DataLoader".to_string());

        let result1 = resolved_definition(&db, config, target);
        let result2 = resolved_definition(&db, config, target);

        // Same input, no changes — should be pointer-equal (cache hit)
        assert!(result1 == result2);
    }

    #[test]
    fn test_cached_definition_info_function_target() {
        let db = test_db();
        let workspace = get_simple_test_dir();
        let config = PythonConfig::frozen(&db, system_path(&workspace), None);
        let target = TargetString::new(&db, "my_module.create_model".to_string());
        let result = resolved_definition(&db, config, target);

        let def = result.get().expect("should resolve create_model");
        assert_eq!(def.symbol_name, "create_model");
        assert!(matches!(def.definition_info, DefinitionInfo::Function(_)));
    }

    #[test]
    fn test_cached_definition_info_invalidation_on_config_change() {
        let mut db = test_db();
        let workspace = get_simple_test_dir();
        // Mutable on purpose: production configs are frozen, but the queries
        // must still recompute if a config ever changes.
        let config = PythonConfig::new(&db, system_path(&workspace), None);

        let result1 = {
            let target = TargetString::new(&db, "my_module.DataLoader".to_string());
            resolved_definition(&db, config, target)
        };
        assert!(result1.get().is_ok());

        // Change workspace root — invalidates all cached lookups
        config
            .set_workspace_root(&mut db)
            .to(filesystem_root().join("nonexistent/path"));

        let result2 = {
            let target = TargetString::new(&db, "my_module.DataLoader".to_string());
            resolved_definition(&db, config, target)
        };
        // Result pointer should differ (recomputed, not from cache)
        assert!(result1 != result2);
    }

    #[test]
    fn test_cached_definition_info_invalidation_on_file_sync() {
        use filetime::{FileTime, set_file_mtime};
        use ruff_db::files::File;
        use std::fs;
        use tempfile::TempDir;

        // Build a temporary workspace so we can actually modify the file
        // backing the resolved target. `File::sync_path` is intentionally a
        // no-op when on-disk metadata has not changed (matches the LSP path:
        // the file watcher only fires for real changes).
        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path();
        let py_file = workspace.join("watched_module.py");
        fs::write(
            &py_file,
            "class WatchedClass:\n    \"\"\"v1\"\"\"\n    pass\n",
        )
        .expect("write v1");
        // Pin the initial mtime so the post-write bump below is unambiguous,
        // even on filesystems with second-resolution mtimes (network mounts,
        // older HFS+, ext3) where relying on the wall clock is flaky.
        set_file_mtime(&py_file, FileTime::from_unix_time(1_000_000, 0))
            .expect("set initial mtime");

        let mut db = test_db();
        let config = PythonConfig::frozen(&db, system_path(workspace), None);
        let result1 = {
            let target = TargetString::new(&db, "watched_module.WatchedClass".to_string());
            resolved_definition(&db, config, target)
        };
        assert!(result1.get().is_ok(), "should resolve WatchedClass");

        // Modify the file on disk and force the mtime to a known-newer value
        // so `sync_path` definitely sees a change. This models what the LSP
        // does when did_change_watched_files notifies us of a real change.
        fs::write(
            &py_file,
            "class WatchedClass:\n    \"\"\"v2\"\"\"\n    pass\n",
        )
        .expect("write v2");
        set_file_mtime(&py_file, FileTime::from_unix_time(2_000_000, 0))
            .expect("set updated mtime");

        let sys_path = SystemPath::from_std_path(&py_file).unwrap();
        File::sync_path(&mut db, sys_path);

        let result2 = {
            let target = TargetString::new(&db, "watched_module.WatchedClass".to_string());
            resolved_definition(&db, config, target)
        };
        assert!(
            result1 != result2,
            "memo should be invalidated after sync_path picks up the disk change"
        );
    }

    #[test]
    fn test_cached_definition_info_invalidation_on_pth_file_sync() {
        use filetime::{FileTime, set_file_mtime};
        use ruff_db::files::File;
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let tmp_path = canonicalize(tmp.path());
        let site_packages = tmp_path.join("site-packages");
        let editable_src = tmp_path.join("src");
        fs::create_dir_all(&site_packages).expect("create site-packages dir");
        fs::create_dir_all(&editable_src).expect("create editable src dir");

        let pth_file = site_packages.join("editable_package.pth");
        fs::write(&pth_file, "../src\n").expect("write initial pth");
        set_file_mtime(&pth_file, FileTime::from_unix_time(1_000_000, 0))
            .expect("set initial pth mtime");

        let mut db = test_db();
        let result1 = editables(&db, &site_packages);
        assert_eq!(result1.len(), 1);
        assert_eq!(canonicalize(&result1[0]), canonicalize(&editable_src),);

        fs::write(&pth_file, "../missing\n").expect("write updated pth");
        set_file_mtime(&pth_file, FileTime::from_unix_time(2_000_000, 0))
            .expect("set updated pth mtime");

        let sys_path = SystemPath::from_std_path(&pth_file).unwrap();
        File::sync_path(&mut db, sys_path);

        let result2 = editables(&db, &site_packages);
        assert!(result2.is_empty());
    }

    #[test]
    fn test_site_packages_pth_root_invalidation_on_file_create() {
        use ruff_db::files::File;
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let tmp_path = canonicalize(tmp.path());
        let site_packages = tmp_path.join("site-packages");
        let editable_src = tmp_path.join("src");
        fs::create_dir_all(&site_packages).expect("create site-packages dir");
        fs::create_dir_all(&editable_src).expect("create editable src dir");

        let mut db = test_db();
        let site_packages_sys = SystemPath::from_std_path(&site_packages).unwrap();

        // Register the site-packages directory as a `FileRoot`, exactly as
        // `discovered_environment` does in production.
        db.files()
            .try_add_root(&db, site_packages_sys, FileRootKind::SearchPath);

        let result1 = editables(&db, &site_packages);
        assert!(result1.is_empty());

        // Create a `.pth` file and sync it, exactly as
        // `did_change_watched_files` does on a `.pth` CREATE event. Syncing a
        // new file bumps its parent directory, which the scan depends on.
        let pth_file = site_packages.join("editable_package.pth");
        fs::write(&pth_file, "../src\n").expect("write pth");
        File::sync_path(&mut db, SystemPath::from_std_path(&pth_file).unwrap());

        let result2 = editables(&db, &site_packages);
        assert_eq!(result2.len(), 1);
        assert_eq!(canonicalize(&result2[0]), canonicalize(&editable_src),);
    }

    /// An editable install of the workspace itself must not turn workspace
    /// files into a high-durability `SearchPath` root: `resolver_environment`
    /// registers the workspace as a `Project` root first, so ty skips it.
    #[test]
    fn test_editable_self_install_keeps_the_workspace_a_project_root() {
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let tmp_path = canonicalize(tmp.path());
        let workspace = tmp_path.join("workspace");
        let src = workspace.join("src");
        let site_packages = tmp_path.join("site-packages");
        fs::create_dir_all(src.join("pkg")).expect("create workspace src");
        fs::create_dir_all(&site_packages).expect("create site-packages dir");
        fs::write(src.join("pkg").join("__init__.py"), "").expect("write module");
        fs::write(
            site_packages.join("_workspace.pth"),
            format!("{}\n", src.display()),
        )
        .expect("write pth");

        let db = test_db();
        let config = PythonConfig::frozen(&db, system_path(&workspace), None);
        let _ = resolver_environment(&db, config);

        // A lookup that misses the static paths makes ty compute the dynamic
        // ones, which is where it registers roots for editable installs.
        let workspace_sys = SystemPathBuf::from_path_buf(workspace.clone()).unwrap();
        let site_packages_sys = SystemPathBuf::from_path_buf(site_packages.clone()).unwrap();
        let environment = resolver_environment_from_parts(
            &db,
            vec![workspace_sys],
            vec![site_packages_sys],
            None,
            PythonVersion::latest_ty(),
        );
        let missing = ModuleName::new("hydra_lsp_no_such_module").unwrap();
        assert!(resolve_module_confident(&db, environment, &missing).is_none());

        let module = SystemPathBuf::from_path_buf(src.join("pkg").join("__init__.py")).unwrap();
        let root = db
            .files()
            .root(&db, &module)
            .expect("workspace file has a root");
        assert_eq!(root.kind_at_time_of_creation(&db), FileRootKind::Project);
    }

    #[test]
    fn test_cached_definition_result_equality() {
        // Two separately-created results should not be pointer-equal
        let err = || Err(ResolveError::InvalidFormat("err".into()));
        let r1 = DefinitionResult::from_result(err());
        let r2 = DefinitionResult::from_result(err());
        assert!(r1 != r2);

        // Cloned result should be pointer-equal
        let r3 = r1.clone();
        assert!(r1 == r3);
    }

    // ---- Backdating regression tests for the recursive MRO queries ----
    //
    // `inherited_class_members` / `class_parent_attribute` are recursive, so each query
    // is its own memoised dependent. Their result types must compare by *value*
    // or salsa can never backdate, and any edit to a file in an MRO chain re-runs the
    // entire chain even when the resolved result is identical. These tests lock in
    // value equality.

    /// Minimal `FunctionSignature` with `param_count` positional parameters.
    fn test_sig(name: &str, param_count: usize) -> FunctionSignature {
        let parameters = (0..param_count)
            .map(|i| sig_param(&format!("p{i}"), ParameterKind::PositionalOrKeyword))
            .collect();
        sig_for(name, parameters)
    }

    #[test]
    fn test_class_parent_docs_value_equality() {
        // Two independently-allocated results with identical contents must be
        // equal so salsa can backdate. Under the old `Arc::ptr_eq` impl these
        // were always `!=`.
        let a = InheritedClassMembers::new(
            Some("doc".to_string()),
            Some(test_sig("__init__", 1)),
            None,
            true,
        );
        let b = InheritedClassMembers::new(
            Some("doc".to_string()),
            Some(test_sig("__init__", 1)),
            None,
            true,
        );
        assert_eq!(a, b, "equal contents must compare equal (value equality)");

        // Differing contents must compare unequal — guards against false
        // negatives (failing to invalidate on a real change).
        let diff_doc = InheritedClassMembers::new(
            Some("other".to_string()),
            Some(test_sig("__init__", 1)),
            None,
            true,
        );
        assert_ne!(a, diff_doc, "different docstring must compare unequal");
        let diff_init = InheritedClassMembers::new(
            Some("doc".to_string()),
            Some(test_sig("__init__", 2)),
            None,
            true,
        );
        assert_ne!(
            a, diff_init,
            "different __init__ signature must compare unequal"
        );
    }

    #[test]
    fn test_cached_class_attribute_value_equality() {
        use ruff_db::system::DbWithWritableSystem;

        let mut db = TestDb::new();
        db.write_file("/p.py", "").expect("write p");
        db.write_file("/q.py", "").expect("write q");
        let p = system_path_to_file(&db, "/p.py").expect("p exists");
        let q = system_path_to_file(&db, "/q.py").expect("q exists");
        let attr = |v: &str| ClassAttributeInfo {
            name: "factory".to_string(),
            value: v.to_string(),
        };
        let a = ClassAttributeLookup::found(attr("Foo"), p, "C".to_string());
        let b = ClassAttributeLookup::found(attr("Foo"), p, "C".to_string());
        assert_eq!(a, b, "equal contents must compare equal (value equality)");

        // Each component participates in equality (no false negatives).
        let diff_value = ClassAttributeLookup::found(attr("Bar"), p, "C".to_string());
        assert_ne!(
            a, diff_value,
            "different attribute value must compare unequal"
        );
        let diff_file = ClassAttributeLookup::found(attr("Foo"), q, "C".to_string());
        assert_ne!(a, diff_file, "different file must compare unequal");

        let n1 = ClassAttributeLookup::not_found();
        let n2 = ClassAttributeLookup::not_found();
        assert_eq!(n1, n2, "two not-found results must compare equal");
        assert_ne!(a, n1, "found vs not-found must compare unequal");
    }

    /// Sync `path` as the LSP client reports it on a `did_change_watched_files`
    /// event: by its lexical path, not resolving symlinks.
    fn sync_lexical(db: &mut HydraDatabase, path: &Path) {
        File::sync_path(db, &system_path(path));
    }

    /// Write a 3-file `Child -> Parent -> GrandParent` workspace and return the
    /// file paths. `gp_body` is the body of the GrandParent class file.
    fn write_mro_workspace(workspace: &Path, gp_body: &str) -> (PathBuf, PathBuf, PathBuf) {
        use filetime::{FileTime, set_file_mtime};
        use std::fs;

        let gp = workspace.join("gp.py");
        fs::write(&gp, gp_body).expect("write gp");
        let parent = workspace.join("parent.py");
        fs::write(
            &parent,
            "from gp import GrandParent\n\n\nclass Parent(GrandParent):\n    pass\n",
        )
        .expect("write parent");
        let child = workspace.join("child.py");
        fs::write(
            &child,
            "from parent import Parent\n\n\nclass Child(Parent):\n    pass\n",
        )
        .expect("write child");
        for f in [&gp, &parent, &child] {
            set_file_mtime(f, FileTime::from_unix_time(1_000_000, 0)).expect("set mtime");
        }
        (gp, parent, child)
    }

    #[test]
    fn test_class_parent_docs_backdates_on_irrelevant_ancestor_edit() {
        use filetime::{FileTime, set_file_mtime};
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path();
        let gp_v1 = "class GrandParent:\n    \"\"\"gp docstring\"\"\"\n    def __init__(self, a):\n        pass\n";
        let (gp, _parent, child) = write_mro_workspace(workspace, gp_v1);

        let mut db = test_db();

        let r1 = {
            let sp = resolver_environment_for_roots(&db, &[workspace.to_path_buf()]);
            let child_key = ClassKey::new(&db, system_file(&db, &child), "Child");
            inherited_class_members(&db, child_key, sp)
        };
        // Prove the MRO walk really resolved through to GrandParent — otherwise
        // a trivially-empty result could make this test pass for the wrong reason.
        assert_eq!(
            r1.docstring().map(String::as_str),
            Some("gp docstring"),
            "Child must inherit GrandParent's docstring via the MRO"
        );
        assert_eq!(
            r1.init_signature().map(|s| s.parameters.len()),
            Some(2),
            "Child must inherit GrandParent's __init__(self, a) — 2 params incl. self"
        );

        // Append an unrelated function AFTER the class so GrandParent's own
        // docstring and __init__ spans are byte-for-byte unchanged.
        fs::write(&gp, format!("{gp_v1}\n\ndef unrelated():\n    pass\n")).expect("write gp v2");
        set_file_mtime(&gp, FileTime::from_unix_time(2_000_000, 0)).expect("bump gp mtime");
        sync_lexical(&mut db, &gp);

        let r2 = {
            let sp = resolver_environment_for_roots(&db, &[workspace.to_path_buf()]);
            let child_key = ClassKey::new(&db, system_file(&db, &child), "Child");
            inherited_class_members(&db, child_key, sp)
        };

        // Backdating: GrandParent re-executed but produced a value-equal result,
        // so its `changed_at` did not advance and the Parent/Child memos were
        // never re-run — the Child result is the *same* cached Arc.
        assert!(
            Arc::ptr_eq(&r1.inner, &r2.inner),
            "an irrelevant ancestor edit must backdate; the Child result must \
             not be recomputed (this fails under Arc::ptr_eq equality)"
        );
    }

    #[test]
    fn test_class_parent_docs_invalidates_on_relevant_ancestor_edit() {
        use filetime::{FileTime, set_file_mtime};
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path();
        let (gp, _parent, child) = write_mro_workspace(
            workspace,
            "class GrandParent:\n    \"\"\"gp docstring\"\"\"\n    def __init__(self, a):\n        pass\n",
        );

        let mut db = test_db();
        let r1 = {
            let sp = resolver_environment_for_roots(&db, &[workspace.to_path_buf()]);
            let child_key = ClassKey::new(&db, system_file(&db, &child), "Child");
            inherited_class_members(&db, child_key, sp)
        };
        assert_eq!(r1.init_signature().map(|s| s.parameters.len()), Some(2));

        // Genuinely change GrandParent's __init__ signature: the inherited
        // result MUST update (no false negative).
        fs::write(
            &gp,
            "class GrandParent:\n    \"\"\"gp docstring\"\"\"\n    def __init__(self, a, b):\n        pass\n",
        )
        .expect("write gp v2");
        set_file_mtime(&gp, FileTime::from_unix_time(2_000_000, 0)).expect("bump gp mtime");
        sync_lexical(&mut db, &gp);

        let r2 = {
            let sp = resolver_environment_for_roots(&db, &[workspace.to_path_buf()]);
            let child_key = ClassKey::new(&db, system_file(&db, &child), "Child");
            inherited_class_members(&db, child_key, sp)
        };
        assert_eq!(
            r2.init_signature().map(|s| s.parameters.len()),
            Some(3),
            "a real __init__ change must propagate through the MRO"
        );
        assert!(r1 != r2, "result must differ after a meaningful change");
    }

    #[test]
    fn test_class_parent_attribute_backdates_on_irrelevant_ancestor_edit() {
        use filetime::{FileTime, set_file_mtime};
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path();
        let gp_v1 = "class GrandParent:\n    factory = SomeFactory\n";
        let (gp, _parent, child) = write_mro_workspace(workspace, gp_v1);

        let mut db = test_db();
        let r1 = {
            let sp = resolver_environment_for_roots(&db, &[workspace.to_path_buf()]);
            let child_key = ClassKey::new(&db, system_file(&db, &child), "Child");
            let attr_key = SymbolName::new(&db, "factory".to_string());
            class_parent_attribute(&db, child_key, attr_key, sp)
        };
        assert_eq!(
            r1.get().map(|a| a.info.value.as_str()),
            Some("SomeFactory"),
            "Child must inherit GrandParent's `factory` attribute via the MRO"
        );

        // Append unrelated content; the `factory` assignment is unchanged.
        fs::write(&gp, format!("{gp_v1}\n\ndef unrelated():\n    pass\n")).expect("write gp v2");
        set_file_mtime(&gp, FileTime::from_unix_time(2_000_000, 0)).expect("bump gp mtime");
        sync_lexical(&mut db, &gp);

        let r2 = {
            let sp = resolver_environment_for_roots(&db, &[workspace.to_path_buf()]);
            let child_key = ClassKey::new(&db, system_file(&db, &child), "Child");
            let attr_key = SymbolName::new(&db, "factory".to_string());
            class_parent_attribute(&db, child_key, attr_key, sp)
        };
        assert!(
            Arc::ptr_eq(&r1.inner, &r2.inner),
            "an irrelevant ancestor edit must backdate the attribute lookup \
             (this fails under Arc::ptr_eq equality)"
        );
    }

    #[test]
    fn test_class_parent_attribute_invalidates_on_relevant_ancestor_edit() {
        use filetime::{FileTime, set_file_mtime};
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path();
        let (gp, _parent, child) =
            write_mro_workspace(workspace, "class GrandParent:\n    factory = SomeFactory\n");

        let mut db = test_db();
        let r1 = {
            let sp = resolver_environment_for_roots(&db, &[workspace.to_path_buf()]);
            let child_key = ClassKey::new(&db, system_file(&db, &child), "Child");
            let attr_key = SymbolName::new(&db, "factory".to_string());
            class_parent_attribute(&db, child_key, attr_key, sp)
        };
        assert_eq!(r1.get().map(|a| a.info.value.as_str()), Some("SomeFactory"));

        // Change the attribute's value: the inherited result MUST update.
        fs::write(&gp, "class GrandParent:\n    factory = OtherFactory\n").expect("write gp v2");
        set_file_mtime(&gp, FileTime::from_unix_time(2_000_000, 0)).expect("bump gp mtime");
        sync_lexical(&mut db, &gp);

        let r2 = {
            let sp = resolver_environment_for_roots(&db, &[workspace.to_path_buf()]);
            let child_key = ClassKey::new(&db, system_file(&db, &child), "Child");
            let attr_key = SymbolName::new(&db, "factory".to_string());
            class_parent_attribute(&db, child_key, attr_key, sp)
        };
        assert_eq!(
            r2.get().map(|a| a.info.value.as_str()),
            Some("OtherFactory"),
            "a real attribute change must propagate through the MRO"
        );
        assert!(r1 != r2, "result must differ after a meaningful change");
    }

    /// When the workspace is reached through a symlinked path (the path the LSP client
    /// reports), a real ancestor edit must still invalidate the MRO memo.
    #[cfg(unix)]
    #[test]
    fn test_class_parent_docs_invalidates_via_symlinked_workspace() {
        use filetime::{FileTime, set_file_mtime};
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let real_ws = tmp.path().join("real");
        fs::create_dir_all(&real_ws).expect("create real workspace");
        let (gp, _parent, _child) = write_mro_workspace(
            &real_ws,
            "class GrandParent:\n    \"\"\"gp docstring\"\"\"\n    def __init__(self, a):\n        pass\n",
        );

        // Access the same tree through a symlink — this is the lexical path the
        // editor/client would report, distinct from the canonical `real/` path.
        let link_ws = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_ws, &link_ws).expect("create workspace symlink");
        // Sanity: the symlink path really is non-canonical.
        assert_ne!(
            link_ws,
            link_ws.canonicalize().expect("canonicalize link"),
            "symlink path must differ from its canonical target for this test to be meaningful"
        );

        let mut db = test_db();
        let child_via_link = link_ws.join("child.py");
        let query = |db: &HydraDatabase| {
            let sp = resolver_environment_for_roots(db, std::slice::from_ref(&link_ws));
            let child_key = ClassKey::new(db, system_file(db, &child_via_link), "Child");
            inherited_class_members(db, child_key, sp)
        };

        let r1 = query(&db);
        assert_eq!(
            r1.init_signature().map(|s| s.parameters.len()),
            Some(2),
            "Child must inherit GrandParent's __init__(self, a) via the symlinked workspace"
        );

        // Genuinely change GrandParent's __init__, then sync the *lexical*
        // symlink path (as the client reports on a watch event).
        fs::write(
            &gp,
            "class GrandParent:\n    \"\"\"gp docstring\"\"\"\n    def __init__(self, a, b):\n        pass\n",
        )
        .expect("write gp v2");
        set_file_mtime(&gp, FileTime::from_unix_time(2_000_000, 0)).expect("bump gp mtime");
        sync_lexical(&mut db, &link_ws.join("gp.py"));

        let r2 = query(&db);
        assert_eq!(
            r2.init_signature().map(|s| s.parameters.len()),
            Some(3),
            "a real __init__ change must invalidate the MRO memo even when the \
             workspace is reached through a symlink"
        );
    }

    #[test]
    fn test_resolve_module_invalidation_on_file_create() {
        use ruff_db::files::File;
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path();

        let mut db = test_db();
        let search_paths = vec![workspace.to_path_buf()];

        // First lookup: the module file does not exist yet.
        let result1 = { resolve_on(&db, &search_paths, "new_module") };
        assert!(
            result1.is_none(),
            "module should not resolve before creation"
        );

        // Create the module file on disk and notify salsa, as the LSP does on a
        // did_change_watched_files CREATE event.
        // Sync the same (lexical, un-canonicalized) path the resolver probes
        // and that the LSP passes to `File::sync_path`.
        let py_file = workspace.join("new_module.py");
        fs::write(&py_file, "class NewClass:\n    pass\n").expect("write module");
        File::sync_path(&mut db, SystemPath::from_std_path(&py_file).unwrap());

        // Second lookup: the create must now be observed.
        let result2 = { resolve_on(&db, &search_paths, "new_module") };
        assert!(
            result2
                .as_ref()
                .and_then(FilePath::as_system_path)
                .is_some_and(|p| p.ends_with("new_module.py")),
            "module must resolve after creation + sync_path, got {result2:?}"
        );
    }

    /// The module name reaching this query is the `_target_` string a user
    /// typed into their YAML, so segments such as `..` or an absolute path must
    /// not make it probe files outside the search roots.
    #[test]
    fn test_resolve_module_cached_cannot_escape_the_search_roots() {
        use ruff_db::system::DbWithWritableSystem;

        // Fixed in-memory paths: a real temp directory can carry a dot in its
        // prefix, which would split the absolute name below into parts and
        // quietly make this test prove nothing.
        let mut db = TestDb::new();
        db.write_file("/root/__init__.py", "class Thing:\n    pass\n")
            .expect("write root init");
        db.write_file("/root/inside.py", "class Thing:\n    pass\n")
            .expect("write inside");
        db.write_file("/outside.py", "class Thing:\n    pass\n")
            .expect("write outside");
        db.write_file("/root/pkg/sub.py", "class Thing:\n    pass\n")
            .expect("write sub");

        let search_paths = vec![PathBuf::from("/root")];
        let resolve = |module: &str| resolve_on(&db, &search_paths, module);

        // Positive control: a guard that rejected everything would fail here.
        assert_eq!(
            resolve("inside"),
            Some(FilePath::system("/root/inside.py")),
            "a plain module name must still resolve"
        );

        // `/outside.py` exists, so this fails without the guard.
        assert_eq!(
            resolve("/outside"),
            None,
            "an absolute name must not replace the search root"
        );

        // `_target_: ".Thing"` splits to an empty module name, and
        // `/root/__init__.py` exists, so this fails without the dot-only guard.
        assert_eq!(
            resolve(""),
            None,
            "an empty name must not resolve to the search root itself"
        );
        assert_eq!(resolve("."), None, "nor must a dot-only name");

        // A leading or doubled dot leaves an empty part, which must not be
        // skipped: without the guard `..inside` would reach `/root/inside.py`
        // and `pkg..sub` would reach `/root/pkg/sub.py`, both of which exist.
        assert_eq!(
            resolve("..inside"),
            None,
            "a leading-dot name must not resolve as if the dots were absent"
        );
        // Positive control for the doubled-dot case: the file it would reach
        // must really be reachable under its correct name.
        assert_eq!(
            resolve("pkg.sub"),
            Some(FilePath::system("/root/pkg/sub.py")),
            "the nested module must resolve under its correct name"
        );
        assert_eq!(
            resolve("pkg..sub"),
            None,
            "nor must a name with a doubled dot"
        );
    }

    // ==================== vendored typeshed resolution ====================

    /// Resolve `module` against `search_paths` on a fresh database.
    fn resolve_on(
        db: &dyn ty_module_resolver::Db,
        search_paths: &[PathBuf],
        module: &str,
    ) -> Option<FilePath> {
        resolve_module_by_name(db, module, resolver_environment_for_roots(db, search_paths))
            .map(|file| file.path(db).clone())
    }

    #[test]
    fn test_builtins_resolves_to_the_vendored_stub() {
        let db = TestDb::new();
        assert_eq!(
            resolve_on(&db, &[PathBuf::from("/root")], "builtins"),
            Some(builtins_stub()),
        );
    }

    #[test]
    fn test_stdlib_modules_resolve_to_their_vendored_stubs() {
        let db = TestDb::new();
        assert_eq!(
            resolve_on(&db, &[PathBuf::from("/root")], "datetime"),
            Some(vendored("stdlib/datetime.pyi")),
        );
    }

    /// `builtins` is compiled into the interpreter, so Python never imports a
    /// `builtins.py` from the search path, and neither does resolution.
    #[test]
    fn test_workspace_builtins_does_not_shadow_the_vendored_stub() {
        use ruff_db::system::DbWithWritableSystem;

        let mut db = TestDb::new();
        db.write_file("/root/builtins.py", "def len(obj):\n    pass\n")
            .expect("write shadowing builtins");

        assert_eq!(
            resolve_on(&db, &[PathBuf::from("/root")], "builtins"),
            Some(builtins_stub()),
        );
    }

    #[test]
    fn test_is_builtin_symbol() {
        let db = TestDb::new();
        let environment = resolver_environment_for_roots(&db, &[]);
        let check = |name: &str| is_builtin_symbol(&db, name, environment);

        assert!(check("len"), "a builtin function");
        assert!(check("dict"), "a builtin class");
        assert!(!check("not_a_builtin"));
        assert!(!check("count"), "a method of `list`, not a builtin");
        assert!(!check(""));
        assert!(!check("builtins.len"), "a dotted name is not a bare name");
    }

    // ==================== resolution order ====================
    //
    // Pins where a module name lands when more than one search root could
    // answer it, in the shape production builds: a workspace, a site-packages
    // directory and the editable installs its `.pth` files list.

    /// A workspace, a site-packages directory and an interpreter's stdlib
    /// directory on disk, each starting empty.
    struct Layout {
        _tmp: tempfile::TempDir,
        workspace: PathBuf,
        site_packages: PathBuf,
        stdlib: PathBuf,
    }

    impl Layout {
        fn new() -> Self {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let root = canonicalize(tmp.path());
            let workspace = root.join("workspace");
            let site_packages = root.join("venv").join("site-packages");
            let stdlib = root.join("python").join("lib");
            std::fs::create_dir_all(&workspace).expect("create workspace");
            std::fs::create_dir_all(&site_packages).expect("create site-packages");
            std::fs::create_dir_all(&stdlib).expect("create stdlib");
            Self {
                _tmp: tmp,
                workspace,
                site_packages,
                stdlib,
            }
        }

        fn db(&self) -> HydraDatabase {
            HydraDatabase::new(SystemPath::from_std_path(&self.workspace).unwrap())
        }

        fn environment<'db>(&self, db: &'db HydraDatabase) -> ResolverEnvironment<'db> {
            resolver_environment_from_parts(
                db,
                vec![system_path(&self.workspace)],
                vec![system_path(&self.site_packages)],
                Some(system_path(&self.stdlib)),
                PythonVersion::latest_ty(),
            )
        }

        fn resolve(&self, module: &str) -> Option<FilePath> {
            let db = self.db();
            resolve_module_by_name(&db, module, self.environment(&db))
                .map(|file| file.path(&db).clone())
        }

        /// The definition `target` resolves to, with the path of its typing
        /// file and of its runtime source, if any.
        fn definition(&self, target: &str) -> (DefinitionInfo, FilePath, Option<FilePath>) {
            let db = self.db();
            let def = definition_with_source(&db, target, self.environment(&db))
                .unwrap_or_else(|err| panic!("{target} should resolve: {err}"));
            let source = def
                .source
                .as_ref()
                .map(|source| source.file.path(&db).clone());
            (def.definition_info, def.file.path(&db).clone(), source)
        }
    }

    fn docstring(info: &DefinitionInfo) -> Option<&str> {
        match info {
            DefinitionInfo::Function(sig) => sig.docstring.as_deref(),
            DefinitionInfo::Class(class) => class.docstring.as_deref(),
            DefinitionInfo::Method(method) => method.signature.docstring.as_deref(),
        }
    }

    #[test]
    fn test_workspace_module_shadows_site_packages() {
        let layout = Layout::new();
        write_file(&layout.workspace.join("shared.py"), "");
        write_file(&layout.site_packages.join("shared.py"), "");

        assert_eq!(
            layout.resolve("shared"),
            Some(on_disk(&layout.workspace.join("shared.py")))
        );
    }

    /// The workspace is ahead of the stdlib on `sys.path`, so a first-party
    /// module named like a stdlib module is what Python imports.
    #[test]
    fn test_workspace_module_shadows_a_stdlib_name() {
        let layout = Layout::new();
        write_file(&layout.workspace.join("json.py"), "");

        assert_eq!(
            layout.resolve("json"),
            Some(on_disk(&layout.workspace.join("json.py")))
        );
    }

    /// The stdlib is ahead of site-packages on `sys.path`, so Python imports
    /// the stdlib `enum`, not the copy in site-packages.
    #[test]
    fn test_site_packages_module_named_like_the_stdlib() {
        let layout = Layout::new();
        write_file(&layout.site_packages.join("enum.py"), "");

        assert_eq!(layout.resolve("enum"), Some(vendored("stdlib/enum.pyi")));
    }

    /// ty always answers `typing_extensions` from its stdlib stubs, but Hydra
    /// imports the installed copy.
    #[test]
    fn test_site_packages_typing_extensions() {
        let layout = Layout::new();
        write_file(&layout.site_packages.join("typing_extensions.py"), "");

        assert_eq!(
            layout.resolve("typing_extensions"),
            Some(on_disk(&layout.site_packages.join("typing_extensions.py")))
        );
    }

    /// Without an installed copy Hydra cannot import `typing_extensions`, even
    /// though the stdlib stubs include it.
    #[test]
    fn test_missing_typing_extensions_is_unresolved() {
        let layout = Layout::new();
        assert_eq!(layout.resolve("typing_extensions"), None);
    }

    #[test]
    fn test_builtins_without_a_shadow_is_the_vendored_stub() {
        let layout = Layout::new();
        assert_eq!(layout.resolve("builtins"), Some(builtins_stub()));
    }

    /// Every stdlib module resolves from its stub, even when the interpreter's
    /// stdlib has no source to offer, as when it was not found.
    #[test]
    fn test_stdlib_module_without_source_resolves_to_its_stub() {
        let layout = Layout::new();
        assert_eq!(
            layout.resolve("collections"),
            Some(vendored("stdlib/collections/__init__.pyi"))
        );
        let (_, _, source) = layout.definition("collections.OrderedDict");
        assert_eq!(source, None);
    }

    /// A stdlib module with a source still resolves to its stub, which
    /// signatures come from.
    #[test]
    fn test_stdlib_module_with_source_resolves_to_its_stub() {
        let layout = Layout::new();
        write_file(&layout.stdlib.join("collections").join("__init__.py"), "");

        assert_eq!(
            layout.resolve("collections"),
            Some(vendored("stdlib/collections/__init__.pyi"))
        );
    }

    /// A module compiled into the interpreter has no source to find, but every
    /// interpreter of that version can import it.
    #[test]
    fn test_compiled_in_stdlib_module_resolves_without_source() {
        let layout = Layout::new();
        assert_eq!(
            layout.resolve("itertools"),
            Some(vendored("stdlib/itertools.pyi"))
        );
    }

    #[test]
    fn test_stdlib_definition_takes_docs_and_location_from_source() {
        let layout = Layout::new();
        let source = layout.stdlib.join("collections").join("__init__.py");
        write_file(
            &source,
            "class OrderedDict(dict):\n    'Dictionary that remembers insertion order'\n",
        );

        let (info, typing, source_path) = layout.definition("collections.OrderedDict");
        assert_eq!(typing, vendored("stdlib/collections/__init__.pyi"));
        assert_eq!(source_path, Some(on_disk(&source)));
        assert_eq!(
            docstring(&info),
            Some("Dictionary that remembers insertion order")
        );
    }

    /// The stub hides private names, but with a source to confirm it a private
    /// stdlib name is one Hydra can import.
    #[test]
    fn test_private_stdlib_name_with_source_resolves() {
        let layout = Layout::new();
        write_file(
            &layout.stdlib.join("collections").join("__init__.py"),
            "class _OrderedDictKeysView:\n    pass\n",
        );

        let (_, _, source) = layout.definition("collections._OrderedDictKeysView");
        assert!(source.is_some());
    }

    /// The runtime module can re-export the definition, as `pathlib` does from
    /// `pathlib._local` from Python 3.13. ty resolves that import against the
    /// stubs, and with no `pathlib/_local.pyi` there it stops at the import,
    /// as ty's own go-to-definition does.
    #[test]
    fn test_stdlib_source_stops_at_a_runtime_only_reexport() {
        let layout = Layout::new();
        let pathlib = layout.stdlib.join("pathlib");
        write_file(&pathlib.join("__init__.py"), "from ._local import Path\n");
        write_file(
            &pathlib.join("_local.py"),
            "class Path:\n    'PurePath subclass that can make system calls.'\n",
        );

        let (_, _, source) = layout.definition("pathlib.Path");
        assert_eq!(source, Some(on_disk(&pathlib.join("__init__.py"))));
    }

    #[test]
    fn test_stub_method_takes_docs_from_source() {
        let layout = Layout::new();
        let source = layout.workspace.join("typed.py");
        write_file(
            &source,
            "class Path:\n    @classmethod\n    def home(cls):\n        'Return the home directory.'\n",
        );
        write_file(
            &layout.workspace.join("typed.pyi"),
            "class Path:\n    @classmethod\n    def home(cls) -> Path: ...\n",
        );

        let (info, _, source_path) = layout.definition("typed.Path.home");
        assert_eq!(source_path, Some(on_disk(&source)));
        assert_eq!(docstring(&info), Some("Return the home directory."));
    }

    /// A stub's own docstring wins over the source's, as in ty's hover.
    #[test]
    fn test_stub_docstring_wins_over_source() {
        let layout = Layout::new();
        write_file(
            &layout.workspace.join("typed.py"),
            "def f(x):\n    'From the source.'\n",
        );
        write_file(
            &layout.workspace.join("typed.pyi"),
            "def f(x: int) -> int:\n    'From the stub.'\n",
        );

        let (info, _, source) = layout.definition("typed.f");
        assert!(source.is_some());
        assert_eq!(docstring(&info), Some("From the stub."));
    }

    /// A stub-only module has no source, so its definition keeps the stub.
    #[test]
    fn test_compiled_in_definition_has_no_source() {
        let layout = Layout::new();
        let (_, typing, source) = layout.definition("builtins.len");
        assert_eq!(typing, builtins_stub());
        assert_eq!(source, None);
    }

    #[test]
    fn test_editable_install_resolves_after_site_packages() {
        let layout = Layout::new();
        let editable = layout.site_packages.parent().unwrap().join("editable_src");
        write_file(&editable.join("editable_pkg").join("__init__.py"), "");
        write_file(&layout.site_packages.join("shadowed.py"), "");
        write_file(&editable.join("shadowed.py"), "");
        write_file(
            &layout.site_packages.join("_editable.pth"),
            &format!("{}\n", editable.display()),
        );

        assert_eq!(
            layout.resolve("editable_pkg"),
            Some(on_disk(&editable.join("editable_pkg").join("__init__.py")))
        );
        assert_eq!(
            layout.resolve("shadowed"),
            Some(on_disk(&layout.site_packages.join("shadowed.py")))
        );
    }

    #[test]
    fn test_namespace_package_spanning_two_roots() {
        let layout = Layout::new();
        write_file(&layout.workspace.join("ns").join("first.py"), "");
        write_file(&layout.site_packages.join("ns").join("second.py"), "");

        assert_eq!(
            layout.resolve("ns.first"),
            Some(on_disk(&layout.workspace.join("ns").join("first.py")))
        );
        assert_eq!(
            layout.resolve("ns.second"),
            Some(on_disk(&layout.site_packages.join("ns").join("second.py")))
        );
        // A namespace package has no file of its own to point at.
        assert_eq!(layout.resolve("ns"), None);
    }

    /// A regular package anywhere on the path hides the namespace portions of
    /// the same name, so Python never looks inside the workspace `pkg/` here.
    #[test]
    fn test_regular_package_in_a_later_root_and_namespace_portion_earlier() {
        let layout = Layout::new();
        write_file(&layout.workspace.join("pkg").join("sub.py"), "");
        write_file(&layout.site_packages.join("pkg").join("__init__.py"), "");

        assert_eq!(
            layout.resolve("pkg"),
            Some(on_disk(
                &layout.site_packages.join("pkg").join("__init__.py")
            ))
        );
        assert_eq!(layout.resolve("pkg.sub"), None);
    }

    /// A PEP 561 `<name>-stubs` package is preferred over `<name>` itself, as
    /// a `.pyi` is over the `.py` beside it.
    #[test]
    fn test_stub_only_package() {
        let layout = Layout::new();
        write_file(&layout.site_packages.join("acme").join("__init__.py"), "");
        write_file(
            &layout.site_packages.join("acme-stubs").join("__init__.pyi"),
            "",
        );

        assert_eq!(
            layout.resolve("acme"),
            Some(on_disk(
                &layout.site_packages.join("acme-stubs").join("__init__.pyi")
            ))
        );
    }

    #[test]
    fn test_package_shadows_module_of_the_same_name() {
        let layout = Layout::new();
        write_file(&layout.workspace.join("thing.py"), "");
        write_file(&layout.workspace.join("thing").join("__init__.py"), "");

        assert_eq!(
            layout.resolve("thing"),
            Some(on_disk(&layout.workspace.join("thing").join("__init__.py")))
        );
    }

    /// Types come from a `<name>-stubs` package, docstrings and navigation from
    /// the package it stubs.
    #[test]
    fn test_stub_package_definition_takes_docs_from_the_package() {
        let layout = Layout::new();
        let package = layout.site_packages.join("acme").join("__init__.py");
        write_file(&package, "def run(x):\n    'Run it.'\n");
        let stub = layout.site_packages.join("acme-stubs").join("__init__.pyi");
        write_file(&stub, "def run(x: int) -> None: ...\n");

        let (info, typing, source) = layout.definition("acme.run");
        assert_eq!(typing, on_disk(&stub));
        assert_eq!(source, Some(on_disk(&package)));
        let DefinitionInfo::Function(sig) = info else {
            panic!("run is a function");
        };
        assert_eq!(sig.docstring.as_deref(), Some("Run it."));
        assert_eq!(
            sig.parameters[0].type_annotation.as_deref(),
            Some("int"),
            "the signature comes from the stub"
        );
    }

    #[test]
    fn test_stub_next_to_source_takes_docs_from_source() {
        let layout = Layout::new();
        let source = layout.workspace.join("typed.py");
        write_file(&source, "def f(x):\n    'Documented.'\n");
        write_file(
            &layout.workspace.join("typed.pyi"),
            "def f(x: int) -> int: ...\n",
        );

        let (info, _, source_path) = layout.definition("typed.f");
        assert_eq!(source_path, Some(on_disk(&source)));
        assert_eq!(docstring(&info), Some("Documented."));
    }

    /// A stub whose source keeps its own docstring keeps that one.
    #[test]
    fn test_stub_docstring_is_kept_without_one_in_source() {
        let layout = Layout::new();
        write_file(&layout.workspace.join("typed.py"), "def f(x):\n    pass\n");
        write_file(
            &layout.workspace.join("typed.pyi"),
            "def f(x: int) -> int:\n    'From the stub.'\n",
        );

        let (info, _, _) = layout.definition("typed.f");
        assert_eq!(docstring(&info), Some("From the stub."));
    }

    #[test]
    fn test_stub_next_to_source_is_preferred() {
        let layout = Layout::new();
        write_file(&layout.workspace.join("typed.py"), "");
        write_file(&layout.workspace.join("typed.pyi"), "");

        assert_eq!(
            layout.resolve("typed"),
            Some(on_disk(&layout.workspace.join("typed.pyi")))
        );
    }

    // ==================== resolved_module_file tests ====================

    #[test]
    fn test_resolve_module_simple() {
        let examples_dir = get_simple_test_dir();
        let db = test_db();
        let search_paths = vec![examples_dir.clone(), PathBuf::from(".")];
        let result = resolve_on(&db, &search_paths, "my_module");
        assert!(result.is_some());
        assert!(
            result
                .unwrap()
                .as_system_path()
                .unwrap()
                .ends_with("my_module.py")
        );
    }

    #[test]
    fn test_resolve_module_package() {
        let examples_dir = get_simple_test_dir();
        let db = test_db();
        let search_paths = vec![examples_dir.clone(), PathBuf::from(".")];
        let result = resolve_on(&db, &search_paths, "test_package");
        assert!(result.is_some());
        assert!(
            result
                .unwrap()
                .as_system_path()
                .unwrap()
                .ends_with("__init__.py")
        );
    }

    #[test]
    fn test_resolve_module_submodule() {
        let examples_dir = get_simple_test_dir();
        let db = test_db();
        let search_paths = vec![examples_dir.clone(), PathBuf::from(".")];
        let result = resolve_on(&db, &search_paths, "test_package.submodule");
        assert!(result.is_some());
        assert!(
            result
                .unwrap()
                .as_system_path()
                .unwrap()
                .ends_with("submodule.py")
        );
    }

    #[test]
    fn test_resolve_module_nonexistent() {
        let examples_dir = get_simple_test_dir();
        let db = test_db();
        let search_paths = vec![examples_dir.clone(), PathBuf::from(".")];
        let result = resolve_on(&db, &search_paths, "nonexistent_module");
        assert!(result.is_none());
    }
}
