//! Type-checking the call each `_target_` node makes, with ty.

use std::sync::Arc;

use tracing::debug;
use ty_module_resolver::ModuleName;
use ty_python_semantic::ProgramEnvironment;
use ty_python_semantic::embed::{Arg, check_call, check_partial_call, module_member};
use ty_python_semantic::types::Type;

use super::cache::{PythonConfig, TargetString, program, resolved_definition};
use super::value_type::{ContainerTypes, parameter_type};
use crate::yaml_cache::{DocumentInput, parsed_yaml};
use crate::yaml_parser::{HydraDefaults, HydraObject};

pub use super::value_type::ConfigContainer;

/// An argument whose value does not have a type its parameter accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgumentTypeMismatch {
    /// The index of the argument in `HydraObject::parameters`.
    pub argument: usize,
    /// The name of the parameter it binds to, if it has one.
    pub parameter: Option<String>,
    pub expected: String,
    pub found: String,
    /// The OmegaConf config the value is passed as, when passing it as a plain container
    /// instead would fit the parameter.
    pub config_container: Option<ConfigContainer>,
}

/// The result of type-checking one node's call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetCallCheck {
    /// The callable as the target names it, such as `Model` or `Model.create`.
    pub callable: String,
    pub mismatches: Vec<ArgumentTypeMismatch>,
    /// Whether ty found no overload of the callable that accepts the argument types.
    pub no_matching_overload: bool,
}

/// The call checks of a document's nodes, by their index in `ParsedContent::hydra_objects`.
///
/// Uses `Arc` + pointer equality, like `DefinitionResult`.
#[derive(Clone)]
pub struct DocumentCallChecks {
    inner: Arc<Vec<Option<TargetCallCheck>>>,
}

impl DocumentCallChecks {
    /// The check of the node at `index`, or `None` if its target did not resolve.
    pub fn get(&self, index: usize) -> Option<&TargetCallCheck> {
        self.inner.get(index)?.as_ref()
    }
}

impl PartialEq for DocumentCallChecks {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for DocumentCallChecks {}

/// Type-check the call of every node in `input` whose `_target_` resolves.
///
/// Nested nodes are checked before the nodes they are arguments of, so a parent sees the
/// type each child builds. `defaults` gives the `_convert_` and `_recursive_` of nodes the
/// file does not set them for.
#[salsa::tracked(returns(clone), no_eq, lru = 512)]
pub fn call_checks(
    db: &dyn ty_python_semantic::Db,
    input: DocumentInput,
    config: PythonConfig,
    defaults: HydraDefaults,
) -> DocumentCallChecks {
    debug!("call_checks: executing (cache miss)");
    let parsed = parsed_yaml(db, input);
    let objects = match parsed.result() {
        Ok(content) => content.hydra_objects.as_slice(),
        Err(_) => &[],
    };
    let env = ProgramEnvironment::from_program(program(db, config));
    let containers = ContainerTypes::new(db, &env);

    // A node's own index is always lower than its children's.
    let mut built = vec![Type::unknown(); objects.len()];
    let mut checks = vec![None; objects.len()];
    for (index, object) in objects.iter().enumerate().rev() {
        let Some((callable, name)) = target_callable(db, &env, config, object) else {
            continue;
        };
        let convert = object.convert_mode(defaults);
        let recursive = object.is_recursive(defaults);

        // Hydra passes `_args_` before the keywords.
        let (positional, keywords): (Vec<usize>, Vec<usize>) =
            (0..object.parameters.len()).partition(|&i| object.parameters[i].is_positional());
        let order: Vec<usize> = positional.into_iter().chain(keywords).collect();
        let args: Vec<_> = order
            .iter()
            .map(|&i| {
                let parameter = &object.parameters[i];
                let arg = parameter.key().map_or(Arg::Positional, Arg::Keyword);
                let ty = parameter_type(
                    db,
                    &env,
                    &containers,
                    parameter,
                    convert,
                    recursive,
                    |child| built[child],
                );
                (arg, ty)
            })
            .collect();

        let check = if object.is_partial() {
            check_partial_call(db, &env, callable, &args)
        } else {
            check_call(db, &env, callable, &args)
        };
        built[index] = check.return_type;
        let mismatches = check
            .errors
            .iter()
            .map(|error| ArgumentTypeMismatch {
                argument: order[error.argument_index],
                parameter: error.parameter.clone(),
                expected: display(db, &env, error.expected),
                found: display(db, &env, error.found),
                config_container: containers
                    .config_container(error.found)
                    .filter(|(_, plain)| plain.is_assignable_to(db, &env, error.expected))
                    .map(|(container, _)| container),
            })
            .collect();
        checks[index] = Some(TargetCallCheck {
            callable: name,
            mismatches,
            no_matching_overload: check.no_matching_overload,
        });
    }

    DocumentCallChecks {
        inner: Arc::new(checks),
    }
}

/// The type of the callable `object`'s `_target_` names, and its name as resolution found
/// it. `None` unless hydrust resolves the target and ty finds it too.
fn target_callable<'db>(
    db: &'db dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'db>,
    config: PythonConfig,
    object: &HydraObject,
) -> Option<(Type<'db>, String)> {
    let target = TargetString::new(db, object.target.value.clone());
    let resolved = resolved_definition(db, config, target);
    let definition = resolved.get().ok()?;
    let path = &definition.target_path;
    let attributes: Vec<&str> = path.attributes.iter().map(String::as_str).collect();
    let callable = module_member(db, env, &ModuleName::new(&path.module)?, &attributes)?;
    Some((callable, definition.symbol_name.clone()))
}

/// `ty` as a YAML user reads it. ty marks an instance of `float` itself, which a YAML float
/// is, as `float*`; to them that is just a `float`.
fn display<'db>(
    db: &'db dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> String {
    ty.display(db, env).to_string().replace("float*", "float")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::HydraDatabase;
    use crate::python::test_support::{
        canonicalize, fixture_dir, system_path, test_db, write_file,
    };
    use crate::yaml_parser::{ConvertMode, YamlParser};

    /// The `reexport` and `simple` fixtures, plus `shapes.py` and a minimal `omegaconf`.
    fn workspace() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        for fixture in ["reexport", "simple"] {
            let options = fs_extra::dir::CopyOptions::new()
                .content_only(true)
                .overwrite(true);
            fs_extra::dir::copy(fixture_dir(fixture), tmp.path(), &options).expect("copy fixture");
        }
        write_file(
            &tmp.path().join("omegaconf/__init__.py"),
            "class ListConfig: ...\nclass DictConfig: ...\n",
        );
        write_file(
            &tmp.path().join("shapes.py"),
            "\
from dataclasses import dataclass
from typing import Callable

from omegaconf import DictConfig

class WithNew:
    def __new__(cls, size: int) -> 'WithNew': ...

class WithBoth:
    def __new__(cls, size: int) -> 'WithBoth': ...
    def __init__(self, size: int) -> None: ...

@dataclass
class Point:
    x: float
    y: float = 0.0

class Model:
    def __init__(self, width: int, layers: list[int] | None = None) -> None: ...

def make_model(width: int) -> Model: ...
def make_point(x: float) -> Point: ...
def build(factory: Callable[[], Model], scale: float = 1.0) -> Model: ...
def train(model: Model, lr: float) -> None: ...
def configure(model: DictConfig, options: dict[str, int] | None = None) -> None: ...
",
        );
        tmp
    }

    fn database(workspace: &std::path::Path) -> (HydraDatabase, PythonConfig) {
        let mut db = test_db();
        let config = PythonConfig::frozen(
            &db,
            system_path(workspace),
            Some("/nonexistent/python".into()),
        );
        db.set_python_config(config);
        (db, config)
    }

    /// `target:line argument: expected <- found` for each mismatch in `yaml`, with
    /// ` [ListConfig]`/` [DictConfig]` where a plain container would fit, and
    /// `target: no matching overload`.
    fn check(
        (db, config): &(HydraDatabase, PythonConfig),
        yaml: &str,
        defaults: HydraDefaults,
    ) -> Vec<String> {
        let input = DocumentInput::new(db, yaml.to_string(), 0);
        let checks = call_checks(db, input, *config, defaults);
        let parsed = YamlParser::parse(yaml).expect("valid YAML");
        let mut reports = Vec::new();
        for (index, object) in parsed.hydra_objects.iter().enumerate() {
            let Some(check) = checks.get(index) else {
                reports.push(format!("{}: unchecked", object.target.value));
                continue;
            };
            for mismatch in &check.mismatches {
                let parameter = &object.parameters[mismatch.argument];
                let container = mismatch
                    .config_container
                    .map(|container| format!(" [{container:?}]"))
                    .unwrap_or_default();
                reports.push(format!(
                    "{}:{} {}: {} <- {}{container}",
                    object.target.value,
                    parameter.line + 1,
                    mismatch.parameter.as_deref().unwrap_or("?"),
                    mismatch.expected,
                    mismatch.found,
                ));
            }
            if check.no_matching_overload {
                reports.push(format!("{}: no matching overload", object.target.value));
            }
        }
        reports
    }

    #[test]
    fn checks_argument_types_end_to_end() {
        let tmp = workspace();
        let db = database(&canonicalize(tmp.path()));
        let yaml = "\
model:
  _target_: mylib.Linear
  in_features: 128
  out_features: \"64\"
  bias: yes
aliased:
  _target_: mylib.AliasedClass
  param: 3
new:
  _target_: shapes.WithNew
  size: 1.5
both:
  _target_: shapes.WithBoth
  size: 1.5
point:
  _target_: shapes.Point
  y: 2
  _args_: [\"a\"]
reader:
  _target_: overloaded.Reader
  path: 3
method:
  _target_: class_methods.ModelFactory.from_config
  config: [1]
missing:
  _target_: nowhere.Thing
  value: 1
interpolated:
  _target_: shapes.make_point
  x: ${other.x}
star:
  _target_: mylib.StarExportedClass
  size: big
stdlib:
  _target_: os.path.join
  _args_: [1]
";
        assert_eq!(
            check(&db, yaml, HydraDefaults::default()),
            [
                "mylib.Linear:4 out_features: int <- Literal[\"64\"]",
                "mylib.AliasedClass:8 param: str <- Literal[3]",
                "shapes.WithNew:11 size: int <- float",
                // Reported once, although both `__new__` and `__init__` take `size`.
                "shapes.WithBoth:14 size: int <- float",
                "shapes.Point:18 x: float <- Literal[\"a\"]",
                "overloaded.Reader: no matching overload",
                "class_methods.ModelFactory.from_config:24 config: dict[Unknown, Unknown] <- ListConfig",
                "nowhere.Thing: unchecked",
                "mylib.StarExportedClass:33 size: int <- Literal[\"big\"]",
                "os.path.join: no matching overload",
            ]
        );
    }

    #[test]
    fn nested_targets_are_checked_before_their_parents() {
        let tmp = workspace();
        let db = database(&canonicalize(tmp.path()));
        let yaml = "\
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
wrong_return:
  _target_: shapes.train
  model:
    _target_: shapes.Point
    x: 1
  lr: 0.1
unresolved_child:
  _target_: shapes.train
  model:
    _target_: nowhere.Model
  lr: 0.1
";
        assert_eq!(
            check(&db, yaml, HydraDefaults::default()),
            [
                "shapes.Model:13 width: int <- Literal[\"wide\"]",
                "shapes.build:22 factory: () -> Model <- partial[(*, x: float = 1) -> Point]",
                "shapes.train:28 model: Model <- Point",
                "nowhere.Model: unchecked",
            ]
        );
    }

    #[test]
    fn convert_and_recursive_decide_container_types() {
        let tmp = workspace();
        let db = database(&canonicalize(tmp.path()));
        let yaml = "\
plain:
  _target_: shapes.Model
  width: 1
  layers: [1, 2]
converted:
  _target_: shapes.Model
  _convert_: all
  width: 1
  layers: [1, 2]
not_recursive:
  _target_: shapes.configure
  _recursive_: false
  model:
    _target_: shapes.Model
    width: 1
  options:
    a: 1
";
        assert_eq!(
            check(&db, yaml, HydraDefaults::default()),
            [
                "shapes.Model:4 layers: list[int] | None <- ListConfig [ListConfig]",
                "shapes.configure:16 options: dict[str, int] | None <- DictConfig [DictConfig]",
            ]
        );

        let all = HydraDefaults {
            convert: ConvertMode::All,
            recursive: false,
        };
        assert_eq!(
            check(&db, yaml, all),
            ["shapes.configure:13 model: DictConfig <- dict[str, Unknown]"]
        );

        let directive = format!("# hydrust: convert[partial]\n{yaml}");
        assert_eq!(
            check(&db, &directive, HydraDefaults::default()),
            ["shapes.configure:14 model: DictConfig <- dict[str, Unknown]"]
        );
    }
}
