use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub use cel_interpreter::Value;
use cel_interpreter::objects::{Key as CelKey, Map as CelMap};
use cel_interpreter::{Context as CelContext, ExecutionError as CelError, Program};
use thiserror::Error;

#[must_use]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExpressionContext {
    values: BTreeMap<String, Value>,
}

impl ExpressionContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.values.insert(key.into(), value.into());
        self
    }

    fn interpreter(&self, aliases: &BTreeMap<String, String>) -> CelContext<'static> {
        let mut roots = HashMap::new();
        let mut objects = HashMap::<&str, HashMap<CelKey, Value>>::new();
        for (name, value) in &self.values {
            match (aliases.get(name), name.split_once('.')) {
                (Some(alias), _) => {
                    roots.insert(alias.as_str(), value.clone());
                }
                (None, Some((object, field))) => {
                    objects.entry(object).or_default().insert(field.into(), value.clone());
                }
                (None, None) => {
                    roots.insert(name.as_str(), value.clone());
                }
            }
        }
        roots.extend(objects.into_iter().map(|(name, fields)| (name, Value::Map(CelMap { map: Arc::new(fields) }))));
        let mut context = CelContext::empty();
        for (name, value) in roots {
            context.add_variable_from_value(name, value);
        }
        context.add_function("len", cel_interpreter::functions::size);
        context.add_function("contains", cel_interpreter::functions::contains);
        context.add_function("upper", |value: Arc<String>| value.to_uppercase());
        context.add_function("lower", |value: Arc<String>| value.to_lowercase());
        context.add_function("join", |values: Arc<Vec<Value>>, separator: Arc<String>| {
            values.iter().map(expression_editor_text).collect::<Vec<_>>().join(&separator)
        });
        context
    }
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum ExpressionError {
    #[error("unknown context key {0}")]
    UnknownContextKey(String),
    #[error("unknown function {0}")]
    UnknownFunction(String),
    #[error("{operator} does not accept {left} and {right}")]
    InvalidBinaryTypes { operator: &'static str, left: String, right: String },
    #[error("division by zero")]
    DivisionByZero,
    #[error("{0}")]
    Evaluation(String),
}

pub fn evaluate_expression(source: &str, context: &ExpressionContext) -> Result<Value, ExpressionError> {
    let (cel_source, aliases) = cel_source(source, context);
    let program = Program::compile(&cel_source).map_err(|error| ExpressionError::Evaluation(error.to_string()))?;
    program.execute(&context.interpreter(&aliases)).map_err(|error| map_error(source, &program, error))
}

fn cel_source(source: &str, context: &ExpressionContext) -> (String, BTreeMap<String, String>) {
    let mut output = String::with_capacity(source.len());
    let mut aliases = BTreeMap::new();
    let mut index = 0;
    let mut quote = None;
    while index < source.len() {
        let character = source[index..].chars().next().unwrap_or_default();
        match (quote, character) {
            (Some(_), '\\') => {
                let escaped = source[index + 1..].chars().next();
                output.push(character);
                output.extend(escaped);
                index += character.len_utf8() + escaped.map_or(0, char::len_utf8);
                continue;
            }
            (Some(active), character) if active == character => quote = None,
            (None, character @ ('\'' | '"')) => quote = Some(character),
            (None, character) if character.is_alphabetic() || character == '_' => {
                let end = source[index..]
                    .char_indices()
                    .take_while(|(_, character)| character.is_alphanumeric() || matches!(character, '_' | '.'))
                    .last()
                    .map_or(index + character.len_utf8(), |(offset, character)| index + offset + character.len_utf8());
                let name = &source[index..end];
                if !name.is_ascii() && context.values.contains_key(name) {
                    let next = aliases.len();
                    let alias = aliases.entry(name.to_owned()).or_insert_with(|| format!("__wren_unicode_{next}"));
                    output.push_str(alias);
                } else {
                    output.push_str(name);
                }
                index = end;
                continue;
            }
            _ => {}
        }
        output.push(character);
        index += character.len_utf8();
    }
    (output, aliases)
}

fn map_error(source: &str, program: &Program, error: CelError) -> ExpressionError {
    match error {
        CelError::UndeclaredReference(name) if program.references().has_function(name.as_str()) => ExpressionError::UnknownFunction(name.as_ref().clone()),
        CelError::UndeclaredReference(_) => ExpressionError::UnknownContextKey(source.trim().to_owned()),
        CelError::UnsupportedBinaryOperator(operator, left, right) => {
            ExpressionError::InvalidBinaryTypes { operator, left: left.type_of().to_string(), right: right.type_of().to_string() }
        }
        CelError::DivisionByZero(_) | CelError::RemainderByZero(_) => ExpressionError::DivisionByZero,
        error => ExpressionError::Evaluation(error.to_string()),
    }
}

#[must_use]
pub fn expression_editor_text(value: &Value) -> String {
    match value {
        Value::Int(value) => value.to_string(),
        Value::UInt(value) => value.to_string(),
        Value::Float(value) if value.fract() == 0.0 => format!("{value:.0}"),
        Value::Float(value) => value.to_string(),
        Value::String(value) => value.as_ref().clone(),
        Value::Bool(value) => value.to_string(),
        Value::List(values) => values.iter().map(expression_editor_text).collect::<Vec<_>>().join("\n"),
        Value::Null => String::new(),
        value => format!("{value:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_precedence_strings_lists_and_context() {
        let context = ExpressionContext::new().with("cursor.line", 4.0).with("café", true).with("workspace.trusted", true);
        assert_eq!(evaluate_expression("1 + 2 * 3", &context), Ok(Value::Int(7)));
        assert_eq!(evaluate_expression("upper('wr' + 'en')", &context), Ok(Value::from("WREN")));
        assert_eq!(evaluate_expression("join(['a', cursor.line], ':')", &context), Ok(Value::from("a:4")));
        assert_eq!(evaluate_expression("workspace.trusted && cursor.line >= 4", &context), Ok(Value::Bool(true)));
        assert_eq!(evaluate_expression("café", &context), Ok(Value::Bool(true)));
    }

    #[test]
    fn rejects_io_and_unknown_context_instead_of_becoming_a_script_language() {
        let context = ExpressionContext::new();
        assert!(matches!(evaluate_expression("read_file('/etc/passwd')", &context), Err(ExpressionError::UnknownFunction(_))));
        assert_eq!(evaluate_expression("lsp.available", &context), Err(ExpressionError::UnknownContextKey("lsp.available".to_owned())));
    }

    #[test]
    fn reports_type_and_arithmetic_errors() {
        let context = ExpressionContext::new();
        assert!(matches!(evaluate_expression("'x' - 1", &context), Err(ExpressionError::InvalidBinaryTypes { .. })));
        assert_eq!(evaluate_expression("1 / 0", &context), Err(ExpressionError::DivisionByZero));
    }
}
