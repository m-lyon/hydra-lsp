//! Spike for #81: type-check `_target_` calls through the `ty_python_semantic::embed` facade.
//!
//! Values are typed crudely here (YAML 1.2 scalars, no interpolations, no `_convert_`); the real
//! value typing is #81's step 3.

use std::path::Path;
use std::time::Instant;

use ruff_db::system::SystemPathBuf;
use ty_module_resolver::ModuleName;
use ty_python_semantic::ProgramEnvironment;
use ty_python_semantic::embed::{
    Arg, CallCheck, check_call, check_partial_call, dict_of_str_unknown, float_instance,
    int_literal, list_of_unknown, module_member, str_literal,
};
use ty_python_semantic::types::Type;

use super::cache::{
    PythonConfig, TargetString, program, resolved_definition, resolver_environment,
};
use super::test_support::{canonicalize, fixture_dir, system_path, test_db, write_file};
use crate::database::HydraDatabase;
use crate::yaml_parser::{YamlParser, YamlValue};

fn database(workspace: &Path, interpreter: Option<SystemPathBuf>) -> (HydraDatabase, PythonConfig) {
    let mut db = test_db();
    let config = PythonConfig::frozen(
        &db,
        system_path(workspace),
        Some(interpreter.unwrap_or_else(|| "/nonexistent/python".into())),
    );
    db.set_python_config(config);
    (db, config)
}

fn environment(db: &HydraDatabase, config: PythonConfig) -> ProgramEnvironment<'_> {
    ProgramEnvironment::from_program(program(db, config))
}

/// The callable a dotted target names: the longest prefix that resolves as a module, then the
/// attribute path after it.
fn target_type<'db>(
    db: &'db HydraDatabase,
    config: PythonConfig,
    env: &ProgramEnvironment<'db>,
    target: &str,
) -> Option<Type<'db>> {
    let parts: Vec<&str> = target.split('.').collect();
    let environment = resolver_environment(db, config);
    (1..parts.len()).rev().find_map(|split| {
        let module = parts[..split].join(".");
        super::cache::resolve_module_by_name(db, &module, environment)?;
        module_member(db, env, &ModuleName::new(&module)?, &parts[split..])
    })
}

fn value_type<'db>(
    db: &'db HydraDatabase,
    config: PythonConfig,
    env: &ProgramEnvironment<'db>,
    value: &YamlValue,
) -> Type<'db> {
    match value {
        YamlValue::Null => Type::none(db, env),
        YamlValue::Bool(value) => Type::bool_literal(*value),
        YamlValue::Integer(value) => int_literal(*value),
        YamlValue::Float(_) => float_instance(db, env),
        YamlValue::String(value) => str_literal(db, value),
        YamlValue::Sequence(_) => list_of_unknown(db, env),
        YamlValue::Mapping(map) => match map.get("_target_") {
            Some(YamlValue::String(target)) => {
                let args: Vec<_> = map
                    .iter()
                    .filter(|(key, _)| !key.starts_with('_'))
                    .map(|(key, value)| (key.as_str(), value_type(db, config, env, value)))
                    .collect();
                let partial = matches!(map.get("_partial_"), Some(YamlValue::Bool(true)));
                nested_call(db, config, env, target, &args, partial)
                    .map_or_else(Type::unknown, |check| check.return_type)
            }
            _ => dict_of_str_unknown(db, env),
        },
    }
}

fn nested_call<'db>(
    db: &'db HydraDatabase,
    config: PythonConfig,
    env: &ProgramEnvironment<'db>,
    target: &str,
    keywords: &[(&str, Type<'db>)],
    partial: bool,
) -> Option<CallCheck<'db>> {
    let callable = target_type(db, config, env, target)?;
    let args: Vec<_> = keywords
        .iter()
        .map(|(key, ty)| (Arg::Keyword(key), *ty))
        .collect();
    let check = if partial {
        check_partial_call
    } else {
        check_call
    };
    Some(check(db, env, callable, &args))
}

/// `key: expected <- found` for each argument type error in each `_target_` of `yaml`.
fn check_yaml(db: &HydraDatabase, config: PythonConfig, yaml: &str) -> Vec<String> {
    let env = environment(db, config);
    let parsed = YamlParser::parse(yaml).expect("valid YAML");
    let mut reports = Vec::new();
    for object in &parsed.hydra_objects {
        let target = TargetString::new(db, object.target.value.clone());
        if resolved_definition(db, config, target).get().is_err() {
            reports.push(format!("{}: unresolved", object.target.value));
            continue;
        }
        let callable = target_type(db, config, &env, &object.target.value)
            .unwrap_or_else(|| panic!("ty finds {}", object.target.value));
        let args: Vec<_> = object
            .parameters
            .iter()
            .map(|parameter| {
                let arg = parameter.key().map_or(Arg::Positional, Arg::Keyword);
                (arg, value_type(db, config, &env, &parameter.value))
            })
            .collect();
        let partial = object.partial.as_ref().is_some_and(|partial| partial.value);
        let check = if partial {
            check_partial_call(db, &env, callable, &args)
        } else {
            check_call(db, &env, callable, &args)
        };
        for error in &check.errors {
            let parameter = &object.parameters[error.argument_index];
            reports.push(format!(
                "{}:{} {}: {} <- {}",
                object.target.value,
                parameter.line + 1,
                parameter.key().unwrap_or("_args_"),
                error.expected.display(db, &env),
                error.found.display(db, &env),
            ));
        }
        if check.no_matching_overload {
            reports.push(format!("{}: no matching overload", object.target.value));
        }
    }
    reports
}

fn fixture_workspace() -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    for fixture in ["reexport", "simple"] {
        let options = fs_extra::dir::CopyOptions::new()
            .content_only(true)
            .overwrite(true);
        fs_extra::dir::copy(fixture_dir(fixture), tmp.path(), &options).expect("copy fixture");
    }
    write_file(
        &tmp.path().join("shapes.py"),
        "\
from dataclasses import dataclass
from typing import Callable

class WithNew:
    def __new__(cls, size: int) -> 'WithNew': ...

@dataclass
class Point:
    x: float
    y: float = 0.0

class Model:
    def __init__(self, width: int) -> None: ...

def make_model(width: int) -> Model: ...
def make_point(x: float) -> Point: ...
def build(factory: Callable[[], Model], scale: float = 1.0) -> Model: ...
def train(model: Model, lr: float) -> None: ...
",
    );
    tmp
}

#[test]
fn spike_module_member_finds_targets() {
    let tmp = fixture_workspace();
    let workspace = canonicalize(tmp.path());
    let (db, config) = database(&workspace, None);
    let env = environment(&db, config);
    let display = |target: &str| {
        target_type(&db, config, &env, target)
            .map(|ty| ty.display(&db, &env).to_string())
            .unwrap_or_else(|| "<none>".to_string())
    };

    assert_eq!(display("mylib.Linear"), "<class 'Linear'>");
    assert_eq!(display("mylib.AliasedClass"), "<class 'OriginalClass'>");
    assert_eq!(
        display("mylib.StarExportedClass"),
        "<class 'StarExportedClass'>"
    );
    assert_eq!(
        display("my_module.create_model"),
        "def create_model(input_dim: int, output_dim: int, hidden_dim: int = 128) -> Unknown"
    );
    assert_eq!(
        display("class_methods.ModelFactory.from_config"),
        "bound method <class 'ModelFactory'>.from_config(config: dict[Unknown, Unknown]) -> ModelFactory"
    );
    assert_eq!(
        display("os.path.join"),
        "Overload[(a: LiteralString, /, *paths: LiteralString) -> LiteralString, (a: str | PathLike[str], /, *paths: str | PathLike[str]) -> str, (a: bytes | PathLike[bytes], /, *paths: bytes | PathLike[bytes]) -> bytes]"
    );
    assert_eq!(display("mylib.Missing"), "<none>");
}

#[test]
fn spike_check_call_end_to_end() {
    let tmp = fixture_workspace();
    let workspace = canonicalize(tmp.path());
    let (db, config) = database(&workspace, None);

    let yaml = "\
model:
  _target_: mylib.Linear
  in_features: 128
  out_features: \"64\"
  bias: yes
ok:
  _target_: my_module.DataLoader
  batch_size: 32
  shuffle: true
new:
  _target_: shapes.WithNew
  size: 1.5
point:
  _target_: shapes.Point
  _args_: [1, \"a\"]
reader:
  _target_: overloaded.Reader
  path: 3
method:
  _target_: class_methods.ModelFactory.from_config
  config: [1]
nested:
  _target_: shapes.build
  factory:
    _target_: shapes.Model
    _partial_: true
    width: 3
  scale: 2
bad_partial:
  _target_: shapes.build
  factory:
    _target_: shapes.Model
    _partial_: true
    width: wide
nested_fn:
  _target_: shapes.build
  factory:
    _target_: shapes.make_model
    _partial_: true
    width: 3
wrong_partial:
  _target_: shapes.build
  factory:
    _target_: shapes.make_point
    _partial_: true
    x: 1
wrong_class_partial:
  _target_: shapes.build
  factory:
    _target_: shapes.Point
    _partial_: true
    x: 1
wrong_return:
  _target_: shapes.train
  model:
    _target_: shapes.Point
    x: 1
  lr: 0.1
";
    assert_eq!(
        check_yaml(&db, config, yaml),
        [
            "mylib.Linear:4 out_features: int <- Literal[\"64\"]",
            "mylib.Linear:5 bias: bool <- Literal[\"yes\"]",
            "shapes.WithNew:12 size: int <- float*",
            "shapes.Point:15 _args_: float <- Literal[\"a\"]",
            "overloaded.Reader: no matching overload",
            "class_methods.ModelFactory.from_config:21 config: dict[Unknown, Unknown] <- list[Unknown]",
            "shapes.Model:34 width: int <- Literal[\"wide\"]",
            "shapes.build:43 factory: () -> Model <- partial[(*, x: float = 1) -> Point]",
            "shapes.build:49 factory: () -> Model <- partial[Point]",
            "shapes.train:55 model: Model <- Point",
        ]
    );

    // `_partial_: true` is typed as `functools.partial`: a function's signature is reduced, and
    // a class gives `partial[<instance>]`.
    let env = environment(&db, config);
    let parsed = YamlParser::parse(yaml).expect("valid YAML");
    let factories: Vec<_> = parsed
        .hydra_objects
        .iter()
        .filter(|object| object.target.value == "shapes.build")
        .map(|build| {
            let factory = value_type(&db, config, &env, &build.parameters[0].value);
            factory.display(&db, &env).to_string()
        })
        .collect();
    assert_eq!(
        factories,
        [
            "partial[Model]",
            "partial[Model]",
            "partial[(*, width: int = 3) -> Model]",
            "partial[(*, x: float = 1) -> Point]",
            "partial[Point]",
        ]
    );
}

/// Cold and warm timings against a real environment with a heavy library. Run with
/// `HYDRUST_SPIKE_PYTHON=<venv>/bin/python cargo test --release spike_timings -- --ignored --nocapture`.
#[test]
#[ignore = "needs an interpreter with torch installed"]
fn spike_timings() {
    let interpreter = std::env::var("HYDRUST_SPIKE_PYTHON").expect("HYDRUST_SPIKE_PYTHON is set");
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let workspace = canonicalize(tmp.path());
    let yaml = "\
model:
  _target_: torch.nn.Linear
  in_features: 10
  out_features: \"5\"
optimizer:
  _target_: torch.optim.Adam
  params: []
  lr: 0.001
  betas: [0.9, 0.999]
loss:
  _target_: torch.nn.CrossEntropyLoss
  reduction: mean
  label_smoothing: 0.1
";
    let (db, config) = database(&workspace, Some(interpreter.into()));

    let resolve = Instant::now();
    for target in [
        "torch.nn.Linear",
        "torch.optim.Adam",
        "torch.nn.CrossEntropyLoss",
    ] {
        let target = TargetString::new(&db, target.to_string());
        assert!(resolved_definition(&db, config, target).get().is_ok());
    }
    let resolve = resolve.elapsed();

    let cold = Instant::now();
    let reports = check_yaml(&db, config, yaml);
    let cold = cold.elapsed();
    let warm = Instant::now();
    let warm_reports = check_yaml(&db, config, yaml);
    let warm = warm.elapsed();
    let edited = Instant::now();
    let _ = check_yaml(
        &db,
        config,
        &yaml.replace("in_features: 10", "in_features: 11"),
    );
    let edited = edited.elapsed();

    assert_eq!(reports, warm_reports);
    println!("reports: {reports:#?}");
    println!("hydrust resolution (cold): {resolve:?}");
    println!("ty check_call, cold (after hydrust resolution): {cold:?}");
    println!("ty check_call, warm: {warm:?}");
    println!("ty check_call, one value edited: {edited:?}");
}
