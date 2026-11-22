//! Mapping a definition in a stub to the runtime implementation it stands for.
//!
//! Signatures come from the stub, as a type checker reads them. Go-to-definition
//! and, when the stub has none, the docstring come from the implementation, as
//! in ty's own hover and go-to-definition. That covers a typeshed stub and the
//! interpreter's stdlib source behind it, a `.pyi` beside its `.py`, and a
//! `<name>-stubs` package and the package it stubs.
//!
//! The mapping itself is ty's [`map_stub_definition`]. It needs the stub's
//! definition as a ty [`Definition`], so this finds the statement hydrust
//! extracted and asks ty's semantic index for its definition.

use ruff_db::files::File;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::{line_index, source_text};
use ruff_python_ast::visitor::source_order::{self, SourceOrderVisitor, TraversalSignal};
use ruff_python_ast::{self as ast, AnyNodeRef};
use ruff_source_file::{OneIndexed, PositionEncoding, SourceLocation};
use ruff_text_size::{Ranged, TextSize};
use ty_module_resolver::{ResolverEnvironment, ResolverFile, stub_file_to_real_module};
use ty_python_core::platform::PythonPlatform;
use ty_python_core::{Program, ProgramFile, semantic_index};
use ty_python_semantic::{ResolvedDefinition as TyDefinition, map_stub_definition};

use super::cache::{
    ClassKey, ResolvedDefinition, SourceDefinition, SymbolName, class_defining_method,
};
use super::extract::{extract_class_info, extract_method_info, source_range};
use super::model::{DefinitionInfo, MethodInfo, ResolveError, SourcePosition};
use super::resolve::extract_definition_info;

/// [`extract_definition_info`], with the definition's runtime implementation
/// when it was found in a stub.
pub(crate) fn definition_with_source<'db>(
    db: &'db dyn ty_python_semantic::Db,
    target: &str,
    environment: ResolverEnvironment<'db>,
) -> Result<ResolvedDefinition, ResolveError> {
    let mut definition = extract_definition_info(db, target, environment)?;
    if let Some((source, docstring)) = implementation(db, &definition, environment) {
        if let Some(docstring) = docstring {
            definition.definition_info.fill_missing_docstring(docstring);
        }
        definition.source = Some(source);
    }
    Ok(definition)
}

type Implementation = (SourceDefinition, Option<String>);

/// Where `definition`'s implementation is, and its docstring, if any. `None`
/// when `definition` is not in a stub or nothing on disk implements it, as for
/// a module compiled into the interpreter such as `builtins`.
fn implementation(
    db: &dyn ty_python_semantic::Db,
    definition: &ResolvedDefinition,
    environment: ResolverEnvironment<'_>,
) -> Option<Implementation> {
    let stub = definition.file;
    if !stub.is_stub(db) {
        return None;
    }
    let start = definition.definition_info.range().start;
    map_definition_at(db, stub, start, environment).or_else(|| match &definition.definition_info {
        DefinitionInfo::Method(method) => inherited_implementation(db, stub, method, environment),
        _ => None,
    })
}

/// The implementation of a method the stub declares on its class but the
/// implementation inherits from a base: the method as the implementation's
/// class finds it in its MRO.
fn inherited_implementation(
    db: &dyn ty_python_semantic::Db,
    stub: File,
    method: &MethodInfo,
    environment: ResolverEnvironment<'_>,
) -> Option<Implementation> {
    let python_version = environment.python_version(db);
    let class = extract_class_info(db, stub, &method.class_name, python_version)?;
    let (class_source, _) = map_definition_at(db, stub, class.range.start, environment)?;
    let defining = class_defining_method(
        db,
        ClassKey::new(db, class_source.file, method.class_name.as_str()),
        SymbolName::new(db, method.method_name.clone()),
        environment,
    )?;
    let file = defining.file(db);
    let found = extract_method_info(
        db,
        file,
        defining.class(db).as_str(),
        &method.method_name,
        python_version,
    )?;
    // A base imported from a module with a stub resolves to that stub.
    if file.is_stub(db) {
        return map_definition_at(db, file, found.signature.range.start, environment);
    }
    let source = SourceDefinition {
        file,
        ranges: vec![found.signature.range],
    };
    Some((source, found.signature.docstring))
}

/// [`map_stub_definition`] for the `def` or `class` statement at `start` in
/// the stub `stub`.
fn map_definition_at(
    db: &dyn ty_python_semantic::Db,
    stub: File,
    start: SourcePosition,
    environment: ResolverEnvironment<'_>,
) -> Option<Implementation> {
    // Checked first so a stub with no implementation, such as `builtins.pyi`,
    // is never given a semantic index.
    stub_file_to_real_module(db, ResolverFile::new(db, stub, environment))?;

    // The same program `cache::program` builds for the session, so this
    // shares its parses and semantic indexes.
    let program = Program::new(db, PythonPlatform::default(), environment);
    let program_file = ProgramFile::new(db, stub, program);
    let parsed = parsed_module(db, program_file.python_file(db)).load(db);
    let start = text_offset(db, stub, start);
    let stub_definition = match definition_statement_at(&parsed, start)? {
        DefinitionStatement::Function(function) => {
            semantic_index(db, program_file).expect_single_definition(function)
        }
        DefinitionStatement::Class(class) => {
            semantic_index(db, program_file).expect_single_definition(class)
        }
    };

    let implementations =
        map_stub_definition(db, &TyDefinition::Definition(stub_definition), None)?;
    let file = implementations.first()?.focus_range(db).file();
    let mut ranges = Vec::new();
    let mut docstring = None;
    for implementation in implementations
        .iter()
        .filter(|implementation| implementation.focus_range(db).file() == file)
    {
        if let Some(definition) = implementation.definition() {
            let parsed = parsed_module(db, definition.python_file(db)).load(db);
            ranges.push(source_range(
                definition.full_range(db, &parsed).range(),
                source_text(db, file).as_str(),
            ));
        }
        docstring = docstring
            .or_else(|| implementation.docstring(db))
            .or_else(|| implementation.implementation_docstring(db));
    }
    if ranges.is_empty() {
        return None;
    }
    Some((SourceDefinition { file, ranges }, docstring))
}

/// The byte offset of `position`, a zero-based line and UTF-16 column, in
/// `file`.
fn text_offset(db: &dyn ruff_db::Db, file: File, position: SourcePosition) -> TextSize {
    let location = SourceLocation {
        line: OneIndexed::from_zero_indexed(position.line as usize),
        character_offset: OneIndexed::from_zero_indexed(position.column as usize),
    };
    line_index(db, file).offset(
        location,
        source_text(db, file).as_str(),
        PositionEncoding::Utf16,
    )
}

enum DefinitionStatement<'a> {
    Function(&'a ast::StmtFunctionDef),
    Class(&'a ast::StmtClassDef),
}

/// The `def` or `class` statement starting at `start`, at any depth: stubs
/// nest definitions in classes and in `if sys.version_info` branches.
fn definition_statement_at(
    parsed: &ParsedModuleRef,
    start: TextSize,
) -> Option<DefinitionStatement<'_>> {
    struct Finder<'a> {
        start: TextSize,
        found: Option<DefinitionStatement<'a>>,
    }

    impl<'a> SourceOrderVisitor<'a> for Finder<'a> {
        fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
            if self.found.is_some() || !node.range().contains_inclusive(self.start) {
                return TraversalSignal::Skip;
            }
            if node.start() == self.start {
                match node {
                    AnyNodeRef::StmtFunctionDef(function) => {
                        self.found = Some(DefinitionStatement::Function(function));
                    }
                    AnyNodeRef::StmtClassDef(class) => {
                        self.found = Some(DefinitionStatement::Class(class));
                    }
                    _ => {}
                }
            }
            TraversalSignal::Traverse
        }
    }

    let mut finder = Finder { start, found: None };
    source_order::walk_body(&mut finder, parsed.suite());
    finder.found
}
