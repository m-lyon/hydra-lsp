//! The ty types of the values Hydra passes for a node's arguments.

use ty_module_resolver::ModuleName;
use ty_python_semantic::ProgramEnvironment;
use ty_python_semantic::embed::{
    dict_of_str_unknown, float_instance, instance_of, int_literal, list_of_unknown, module_member,
    str_instance, str_literal,
};
use ty_python_semantic::types::Type;

use crate::yaml_parser::{ConfigScalar, ConvertMode, Parameter, TARGET_KEY, YamlValue};

/// OmegaConf's marker for a value that must be supplied by an override.
const MISSING: &str = "???";

/// The container types a list or mapping is passed as.
pub(super) struct ContainerTypes<'db> {
    /// `omegaconf.ListConfig` and `omegaconf.DictConfig`, or `Unknown` when omegaconf does not
    /// resolve.
    list_config: Type<'db>,
    dict_config: Type<'db>,
    list: Type<'db>,
    dict: Type<'db>,
}

/// A container Hydra passes as an OmegaConf config rather than a plain `list` or `dict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigContainer {
    ListConfig,
    DictConfig,
}

impl<'db> ContainerTypes<'db> {
    pub(super) fn new(db: &'db dyn ty_python_semantic::Db, env: &ProgramEnvironment<'db>) -> Self {
        let config = |class| {
            let omegaconf = ModuleName::new_static("omegaconf")?;
            instance_of(db, env, module_member(db, env, &omegaconf, &[class])?)
        };
        ContainerTypes {
            list_config: config("ListConfig").unwrap_or_else(Type::unknown),
            dict_config: config("DictConfig").unwrap_or_else(Type::unknown),
            list: list_of_unknown(db, env),
            dict: dict_of_str_unknown(db, env),
        }
    }

    /// The type of a list (`mapping: false`) or mapping (`true`) under `convert`.
    fn container(&self, mapping: bool, convert: ConvertMode) -> Type<'db> {
        match (mapping, convert) {
            (false, ConvertMode::None) => self.list_config,
            (true, ConvertMode::None) => self.dict_config,
            (false, _) => self.list,
            (true, _) => self.dict,
        }
    }

    /// Which OmegaConf config `ty` is, if it is one of the two, along with the plain container
    /// `_convert_` would pass in its place.
    pub(super) fn config_container(&self, ty: Type<'db>) -> Option<(ConfigContainer, Type<'db>)> {
        if ty.is_unknown() {
            None
        } else if ty == self.list_config {
            Some((ConfigContainer::ListConfig, self.list))
        } else if ty == self.dict_config {
            Some((ConfigContainer::DictConfig, self.dict))
        } else {
            None
        }
    }
}

/// The type of the value Hydra passes for `parameter`, an argument of a node whose effective
/// `_convert_` and `_recursive_` are `convert` and `recursive`.
///
/// `child` gives the type a nested `_target_` node instantiates to, by its index in
/// `ParsedContent::hydra_objects`.
pub(super) fn parameter_type<'db>(
    db: &'db dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'db>,
    containers: &ContainerTypes<'db>,
    parameter: &Parameter,
    convert: ConvertMode,
    recursive: bool,
    child: impl FnOnce(usize) -> Type<'db>,
) -> Type<'db> {
    match &parameter.value {
        YamlValue::Mapping(map) if map.contains_key(TARGET_KEY) && recursive => {
            // A `_target_` that is not a string, or one in `_args_`, which hydrust does not
            // parse as a node.
            parameter.child.map_or_else(Type::unknown, child)
        }
        YamlValue::Mapping(_) => containers.container(true, convert),
        YamlValue::Sequence(_) => containers.container(false, convert),
        _ => match &parameter.scalar {
            Some(scalar) => scalar_type(db, env, scalar.config_value()),
            // A tagged scalar.
            None => Type::unknown(),
        },
    }
}

fn scalar_type<'db>(
    db: &'db dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'db>,
    scalar: ConfigScalar<'_>,
) -> Type<'db> {
    match scalar {
        ConfigScalar::Null => Type::none(db, env),
        ConfigScalar::Bool(value) => Type::bool_literal(value),
        ConfigScalar::Int(Some(value)) => int_literal(value),
        ConfigScalar::Int(None) => ModuleName::new_static("builtins")
            .and_then(|builtins| module_member(db, env, &builtins, &["int"]))
            .and_then(|int| instance_of(db, env, int))
            .unwrap_or_else(Type::unknown),
        ConfigScalar::Float => float_instance(db, env),
        ConfigScalar::Str(text) if text == MISSING || is_interpolation(text) => Type::unknown(),
        ConfigScalar::Str(text) if text.contains("${") => str_instance(db, env),
        ConfigScalar::Str(text) => str_literal(db, text),
    }
}

/// Whether `text` is a single interpolation, such as `${model.width}` or
/// `${oc.env:HOME,${x}}`, whose value can have any type.
fn is_interpolation(text: &str) -> bool {
    let Some(body) = text.strip_prefix("${") else {
        return false;
    };
    let mut depth = 1;
    for (index, ch) in body.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return index == body.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use saphyr::ScalarStyle;

    use super::*;
    use crate::database::HydraDatabase;
    use crate::python::cache::{PythonConfig, program};
    use crate::python::test_support::{canonicalize, system_path, test_db, write_file};
    use crate::yaml_parser::{ArgKind, Scalar};

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

    fn parameter(value: YamlValue, scalar: Option<(&str, ScalarStyle)>) -> Parameter {
        Parameter {
            kind: ArgKind::Positional,
            value,
            scalar: scalar.map(|(text, style)| Scalar {
                text: text.to_string(),
                style,
            }),
            child: None,
            line: 0,
            value_start: 0,
            value_end: 0,
            suppressed_rules: HashSet::new(),
        }
    }

    fn plain(text: &str) -> Parameter {
        // The YAML 1.2 value is not read for a scalar.
        parameter(YamlValue::Null, Some((text, ScalarStyle::Plain)))
    }

    fn mapping(target: bool, child: Option<usize>) -> Parameter {
        let mut map = hashlink::LinkedHashMap::new();
        if target {
            map.insert(
                TARGET_KEY.to_string(),
                YamlValue::String("pkg.f".to_string()),
            );
        }
        Parameter {
            child,
            ..parameter(YamlValue::Mapping(map), None)
        }
    }

    /// The displayed type of `parameter` under `convert` and `recursive`, with a nested node
    /// typed as `Child`.
    fn display(
        (db, config): &(HydraDatabase, PythonConfig),
        parameter: &Parameter,
        convert: ConvertMode,
        recursive: bool,
    ) -> String {
        let env = ProgramEnvironment::from_program(program(db, *config));
        let containers = ContainerTypes::new(db, &env);
        let child = |_| str_literal(db, "Child");
        parameter_type(db, &env, &containers, parameter, convert, recursive, child)
            .display(db, &env)
            .to_string()
    }

    fn omegaconf_workspace() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        write_file(
            &tmp.path().join("omegaconf/__init__.py"),
            "class ListConfig: ...\nclass DictConfig: ...\n",
        );
        tmp
    }

    #[test]
    fn scalars() {
        let tmp = omegaconf_workspace();
        let db = database(&canonicalize(tmp.path()));
        let none = ConvertMode::None;
        for (parameter, expected) in [
            (plain("~"), "None"),
            (plain(""), "None"),
            (plain("yes"), "Literal[True]"),
            (plain("off"), "Literal[False]"),
            (plain("017"), "Literal[15]"),
            (plain("99999999999999999999"), "int"),
            (plain("1e5"), "float*"),
            (plain("0o17"), "Literal[\"0o17\"]"),
            (plain("adam"), "Literal[\"adam\"]"),
            (
                parameter(YamlValue::Null, Some(("yes", ScalarStyle::DoubleQuoted))),
                "Literal[\"yes\"]",
            ),
            (
                parameter(YamlValue::Null, Some(("017", ScalarStyle::SingleQuoted))),
                "Literal[\"017\"]",
            ),
            (plain("${model.width}"), "Unknown"),
            (plain("${oc.env:HOME,${x}}"), "Unknown"),
            (
                parameter(YamlValue::Null, Some(("${x}", ScalarStyle::DoubleQuoted))),
                "Unknown",
            ),
            (plain("run_${x}"), "str"),
            (plain("${x}_${y}"), "str"),
            (plain("???"), "Unknown"),
            // A tagged scalar.
            (
                parameter(YamlValue::String("12".to_string()), None),
                "Unknown",
            ),
        ] {
            assert_eq!(
                display(&db, &parameter, none, true),
                expected,
                "{:?}",
                parameter.scalar
            );
        }
    }

    #[test]
    fn containers_follow_convert() {
        let tmp = omegaconf_workspace();
        let db = database(&canonicalize(tmp.path()));
        let list = parameter(YamlValue::Sequence(Vec::new()), None);
        let map = mapping(false, None);
        for (convert, expected_list, expected_map) in [
            (ConvertMode::None, "ListConfig", "DictConfig"),
            (ConvertMode::Partial, "list[Unknown]", "dict[str, Unknown]"),
            (ConvertMode::Object, "list[Unknown]", "dict[str, Unknown]"),
            (ConvertMode::All, "list[Unknown]", "dict[str, Unknown]"),
        ] {
            assert_eq!(display(&db, &list, convert, true), expected_list);
            assert_eq!(display(&db, &map, convert, true), expected_map);
        }
    }

    #[test]
    fn containers_without_omegaconf_are_unknown() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = database(&canonicalize(tmp.path()));
        let list = parameter(YamlValue::Sequence(Vec::new()), None);
        assert_eq!(display(&db, &list, ConvertMode::None, true), "Unknown");
        assert_eq!(
            display(&db, &mapping(false, None), ConvertMode::None, true),
            "Unknown"
        );
        assert_eq!(display(&db, &list, ConvertMode::All, true), "list[Unknown]");
    }

    #[test]
    fn nested_targets() {
        let tmp = omegaconf_workspace();
        let db = database(&canonicalize(tmp.path()));
        let nested = mapping(true, Some(1));
        assert_eq!(
            display(&db, &nested, ConvertMode::None, true),
            "Literal[\"Child\"]"
        );
        // Not instantiated under `_recursive_: false`.
        assert_eq!(
            display(&db, &nested, ConvertMode::None, false),
            "DictConfig"
        );
        assert_eq!(
            display(&db, &nested, ConvertMode::All, false),
            "dict[str, Unknown]"
        );
        // A `_target_` mapping hydrust did not parse as a node.
        assert_eq!(
            display(&db, &mapping(true, None), ConvertMode::None, true),
            "Unknown"
        );
    }

    #[test]
    fn config_containers() {
        let tmp = omegaconf_workspace();
        let (db, config) = database(&canonicalize(tmp.path()));
        let env = ProgramEnvironment::from_program(program(&db, config));
        let containers = ContainerTypes::new(&db, &env);
        let found = |ty| {
            containers
                .config_container(ty)
                .map(|(container, plain)| (container, plain.display(&db, &env).to_string()))
        };
        assert_eq!(
            found(containers.list_config),
            Some((ConfigContainer::ListConfig, "list[Unknown]".to_string()))
        );
        assert_eq!(
            found(containers.dict_config),
            Some((
                ConfigContainer::DictConfig,
                "dict[str, Unknown]".to_string()
            ))
        );
        assert_eq!(found(containers.list), None);

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (db, config) = database(&canonicalize(tmp.path()));
        let env = ProgramEnvironment::from_program(program(&db, config));
        let containers = ContainerTypes::new(&db, &env);
        assert_eq!(containers.config_container(Type::unknown()), None);
    }

    #[test]
    fn interpolations() {
        assert!(is_interpolation("${a}"));
        assert!(is_interpolation("${oc.env:A,${b}}"));
        assert!(!is_interpolation("${a}${b}"));
        assert!(!is_interpolation("x${a}"));
        assert!(!is_interpolation("${a"));
        assert!(!is_interpolation("$a"));
    }
}
