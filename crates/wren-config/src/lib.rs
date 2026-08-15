#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use thiserror::Error;
use wren_grammar::{ExpressionContext, Value, evaluate_expression};
use wren_types::{
    CommandArgumentType, CommandInvocation, CommandSchema, CommandValue, LanguageBundle,
};

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub keys: BTreeMap<String, BTreeMap<String, KeyBinding>>,
    #[serde(default, rename = "task")]
    pub tasks: BTreeMap<String, TaskConfig>,
    #[serde(default, rename = "extension")]
    pub extensions: BTreeMap<String, ExtensionConfig>,
    #[serde(default)]
    pub environment: EnvironmentConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct KeyBinding {
    pub command: String,
    #[serde(default)]
    pub args: BTreeMap<String, toml::Value>,
    pub when: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TaskConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "persisted_view")]
    pub document_view: String,
    #[serde(default = "prompt_save")]
    pub save: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ExtensionConfig {
    pub component: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct EnvironmentConfig {
    pub direnv: Option<bool>,
    pub nix: Option<bool>,
}

fn persisted_view() -> String {
    "persisted".to_owned()
}

fn prompt_save() -> String {
    "prompt".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigBundle {
    pub config_toml: Box<str>,
    pub query_sources: BTreeMap<Box<str>, Box<str>>,
    pub theme_sources: BTreeMap<Box<str>, Box<str>>,
    pub language_bundles: Vec<LanguageBundle>,
    pub content_hash: [u8; 32],
}

impl ConfigBundle {
    #[must_use]
    pub fn new(
        config_toml: impl Into<Box<str>>,
        query_sources: BTreeMap<Box<str>, Box<str>>,
        theme_sources: BTreeMap<Box<str>, Box<str>>,
        mut language_bundles: Vec<LanguageBundle>,
    ) -> Self {
        language_bundles.sort_by(|left, right| left.language_id.cmp(&right.language_id));
        let config_toml = config_toml.into();
        let mut hasher = blake3::Hasher::new();
        hasher.update(config_toml.as_bytes());
        for (name, source) in &query_sources {
            hasher.update(name.as_bytes());
            hasher.update(source.as_bytes());
        }
        for (name, source) in &theme_sources {
            hasher.update(name.as_bytes());
            hasher.update(source.as_bytes());
        }
        for bundle in &language_bundles {
            hasher.update(&bundle.content_hash());
        }
        Self {
            config_toml,
            query_sources,
            theme_sources,
            language_bundles,
            content_hash: *hasher.finalize().as_bytes(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceTrust {
    Untrusted,
    Trusted { executable_hash: [u8; 32] },
}

impl WorkspaceTrust {
    #[must_use]
    pub fn allows(&self, executable_hash: [u8; 32]) -> bool {
        matches!(self, Self::Trusted { executable_hash: trusted } if *trusted == executable_hash)
    }
}

#[derive(Debug, Clone, Default)]
pub struct CommandRegistry {
    schemas: BTreeMap<Box<str>, CommandSchema>,
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum ConfigError {
    #[error("parse TOML configuration: {0}")]
    Toml(String),
    #[error("unknown command {0}")]
    UnknownCommand(Box<str>),
    #[error("command {command} is missing required argument {argument}")]
    MissingArgument {
        command: Box<str>,
        argument: Box<str>,
    },
    #[error("command {command} has unknown argument {argument}")]
    UnknownArgument {
        command: Box<str>,
        argument: Box<str>,
    },
    #[error("command {command} argument {argument} has the wrong type")]
    ArgumentType {
        command: Box<str>,
        argument: Box<str>,
    },
    #[error("invalid `when` expression for {key}: {reason}")]
    When { key: Box<str>, reason: Box<str> },
    #[error("workspace trust is required for: {0}")]
    TrustRequired(Box<str>),
    #[error("task {task} has invalid document_view {value}")]
    DocumentView { task: Box<str>, value: Box<str> },
    #[error("task {task} has invalid save policy {value}")]
    SavePolicy { task: Box<str>, value: Box<str> },
}

impl CommandRegistry {
    #[must_use]
    pub fn new(schemas: impl IntoIterator<Item = CommandSchema>) -> Self {
        Self {
            schemas: schemas
                .into_iter()
                .map(|schema| (schema.name.clone(), schema))
                .collect(),
        }
    }

    pub fn validate(
        &self,
        command: &str,
        arguments: &BTreeMap<String, toml::Value>,
    ) -> Result<CommandInvocation, ConfigError> {
        let schema = self
            .schemas
            .get(command)
            .ok_or_else(|| ConfigError::UnknownCommand(command.into()))?;
        for argument in arguments.keys() {
            if !schema
                .arguments
                .iter()
                .any(|candidate| candidate.name.as_ref() == argument)
            {
                return Err(ConfigError::UnknownArgument {
                    command: command.into(),
                    argument: argument.as_str().into(),
                });
            }
        }
        let mut values = BTreeMap::new();
        for argument in &schema.arguments {
            let Some(value) = arguments.get(argument.name.as_ref()) else {
                if argument.required {
                    return Err(ConfigError::MissingArgument {
                        command: command.into(),
                        argument: argument.name.clone(),
                    });
                }
                continue;
            };
            let value = command_value(value, &argument.argument_type).ok_or_else(|| {
                ConfigError::ArgumentType {
                    command: command.into(),
                    argument: argument.name.clone(),
                }
            })?;
            values.insert(argument.name.clone(), value);
        }
        Ok(CommandInvocation {
            command: command.into(),
            arguments: values,
        })
    }
}

pub fn parse_and_validate(
    source: &str,
    registry: &CommandRegistry,
    trust: WorkspaceTrust,
) -> Result<Config, ConfigError> {
    let config: Config =
        toml::from_str(source).map_err(|error| ConfigError::Toml(error.to_string()))?;
    let context = validation_context();
    for (mode, bindings) in &config.keys {
        for (keys, binding) in bindings {
            registry.validate(&binding.command, &binding.args)?;
            if let Some(when) = &binding.when {
                evaluate_expression(when, &context).map_err(|error| ConfigError::When {
                    key: format!("{mode}.{keys}").into(),
                    reason: error.to_string().into(),
                })?;
            }
        }
    }
    for (name, task) in &config.tasks {
        if !matches!(task.document_view.as_str(), "persisted" | "remote-acked") {
            return Err(ConfigError::DocumentView {
                task: name.as_str().into(),
                value: task.document_view.as_str().into(),
            });
        }
        if !matches!(task.save.as_str(), "never" | "prompt" | "all") {
            return Err(ConfigError::SavePolicy {
                task: name.as_str().into(),
                value: task.save.as_str().into(),
            });
        }
    }
    let executable_hash = executable_hash(&config);
    if has_executable_contributions(&config) && !trust.allows(executable_hash) {
        return Err(ConfigError::TrustRequired(
            "tasks, extensions, or environment activation".into(),
        ));
    }
    Ok(config)
}

#[must_use]
pub fn executable_hash(config: &Config) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for (name, task) in &config.tasks {
        hasher.update(name.as_bytes());
        hasher.update(task.command.as_bytes());
        for argument in &task.args {
            hasher.update(argument.as_bytes());
        }
        hasher.update(task.document_view.as_bytes());
        hasher.update(task.save.as_bytes());
    }
    for (name, extension) in &config.extensions {
        hasher.update(name.as_bytes());
        hasher.update(extension.component.as_bytes());
        for capability in &extension.capabilities {
            hasher.update(capability.as_bytes());
        }
    }
    hasher.update(&[u8::from(config.environment.direnv.unwrap_or(false))]);
    hasher.update(&[u8::from(config.environment.nix.unwrap_or(false))]);
    *hasher.finalize().as_bytes()
}

#[must_use]
pub fn sanitized_environment(
    inherited: impl IntoIterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    let exact: BTreeSet<&str> = ["HOME", "PATH", "LANG", "TERM", "TMPDIR", "USER", "SHELL"]
        .into_iter()
        .collect();
    inherited
        .into_iter()
        .filter(|(name, _)| exact.contains(name.as_str()) || name.starts_with("LC_"))
        .collect()
}

fn has_executable_contributions(config: &Config) -> bool {
    !config.tasks.is_empty()
        || !config.extensions.is_empty()
        || config.environment.direnv.unwrap_or(false)
        || config.environment.nix.unwrap_or(false)
}

fn validation_context() -> ExpressionContext {
    ExpressionContext::new()
        .with("language", Value::String("rust".to_owned()))
        .with("remote", Value::Bool(false))
        .with("os", Value::String("linux".to_owned()))
        .with("selection.nonempty", Value::Bool(false))
        .with("lsp.available", Value::Bool(false))
        .with("document.class", Value::String("normal".to_owned()))
        .with("workspace.trusted", Value::Bool(false))
}

fn command_value(value: &toml::Value, expected: &CommandArgumentType) -> Option<CommandValue> {
    match expected {
        CommandArgumentType::Boolean => value.as_bool().map(CommandValue::Boolean),
        CommandArgumentType::Integer => value.as_integer().map(CommandValue::Integer),
        CommandArgumentType::Number => value
            .as_float()
            .or_else(|| value.as_integer().map(|value| value as f64))
            .map(CommandValue::Number),
        CommandArgumentType::String => value
            .as_str()
            .map(|value| CommandValue::String(value.into())),
        CommandArgumentType::StringList => value.as_array().and_then(|values| {
            values
                .iter()
                .map(|value| value.as_str().map(Box::<str>::from))
                .collect::<Option<Vec<_>>>()
                .map(CommandValue::StringList)
        }),
        CommandArgumentType::Enumeration(allowed) => value.as_str().and_then(|value| {
            allowed
                .iter()
                .any(|candidate| candidate.as_ref() == value)
                .then(|| CommandValue::String(value.into()))
        }),
    }
}

#[cfg(test)]
mod tests {
    use wren_types::{CommandArgumentSchema, CommandClass};

    use super::*;

    fn registry() -> CommandRegistry {
        CommandRegistry::new([CommandSchema {
            name: "picker.files".into(),
            description: "files".into(),
            class: CommandClass::Task,
            arguments: vec![
                CommandArgumentSchema {
                    name: "root".into(),
                    argument_type: CommandArgumentType::Enumeration(vec![
                        "workspace".into(),
                        "cwd".into(),
                    ]),
                    required: true,
                    description: "root".into(),
                },
                CommandArgumentSchema {
                    name: "hidden".into(),
                    argument_type: CommandArgumentType::Boolean,
                    required: false,
                    description: "hidden".into(),
                },
            ],
        }])
    }

    #[test]
    fn validates_typed_commands_and_closed_when_expressions() {
        let source = r#"
[keys.normal."space f"]
command = "picker.files"
when = "language == 'rust' && !remote"
[keys.normal."space f".args]
root = "workspace"
hidden = false
"#;
        let config = parse_and_validate(source, &registry(), WorkspaceTrust::Untrusted)
            .expect("valid config");
        assert_eq!(config.keys["normal"]["space f"].command, "picker.files");
        assert!(
            parse_and_validate(
                &source.replace("workspace", "invalid"),
                &registry(),
                WorkspaceTrust::Untrusted
            )
            .is_err()
        );
        assert!(
            parse_and_validate(
                &source.replace("language == 'rust'", "read_file('secret')"),
                &registry(),
                WorkspaceTrust::Untrusted
            )
            .is_err()
        );
    }

    #[test]
    fn executable_inputs_are_hash_fenced_by_trust() {
        let source = r#"
[task.build]
command = "cargo"
args = ["test"]
document_view = "persisted"
save = "prompt"
[environment]
direnv = true
"#;
        let untrusted = parse_and_validate(source, &registry(), WorkspaceTrust::Untrusted);
        assert!(matches!(untrusted, Err(ConfigError::TrustRequired(_))));
        let parsed: Config = toml::from_str(source).expect("parse");
        let hash = executable_hash(&parsed);
        parse_and_validate(
            source,
            &registry(),
            WorkspaceTrust::Trusted {
                executable_hash: hash,
            },
        )
        .expect("trusted");
        assert!(
            !WorkspaceTrust::Trusted {
                executable_hash: hash
            }
            .allows(*blake3::hash(b"changed").as_bytes())
        );
    }

    #[test]
    fn sanitizes_inherited_environment_before_project_activation() {
        let environment = sanitized_environment([
            ("PATH".to_owned(), "/bin".to_owned()),
            ("LD_PRELOAD".to_owned(), "project.so".to_owned()),
            ("SECRET_TOKEN".to_owned(), "secret".to_owned()),
            ("LC_ALL".to_owned(), "C".to_owned()),
        ]);
        assert_eq!(environment.get("PATH").map(String::as_str), Some("/bin"));
        assert_eq!(environment.get("LC_ALL").map(String::as_str), Some("C"));
        assert!(!environment.contains_key("LD_PRELOAD"));
        assert!(!environment.contains_key("SECRET_TOKEN"));
    }
}
