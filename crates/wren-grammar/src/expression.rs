use std::collections::BTreeMap;
use std::fmt;

use logos::{Lexer, Logos};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Number(f64),
    String(String),
    Bool(bool),
    List(Vec<Value>),
    Null,
}

impl Value {
    #[must_use]
    pub fn as_bool(&self) -> bool {
        match self {
            Self::Bool(value) => *value,
            Self::Number(value) => *value != 0.0,
            Self::String(value) => !value.is_empty(),
            Self::List(value) => !value.is_empty(),
            Self::Null => false,
        }
    }

    #[must_use]
    pub fn to_editor_text(&self) -> String {
        match self {
            Self::Number(value) if value.fract() == 0.0 => format!("{value:.0}"),
            Self::Number(value) => value.to_string(),
            Self::String(value) => value.clone(),
            Self::Bool(value) => value.to_string(),
            Self::List(values) => values.iter().map(Self::to_editor_text).collect::<Vec<_>>().join("\n"),
            Self::Null => String::new(),
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            Self::Number(_) => "number",
            Self::String(_) => "string",
            Self::Bool(_) => "boolean",
            Self::List(_) => "list",
            Self::Null => "null",
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_editor_text())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExpressionContext {
    values: BTreeMap<String, Value>,
}

impl ExpressionContext {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: Value) -> Self {
        self.values.insert(key.into(), value);
        self
    }

    pub fn insert(&mut self, key: impl Into<String>, value: Value) -> Option<Value> {
        self.values.insert(key.into(), value)
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Default)]
pub enum ExpressionError {
    #[error("unexpected character {character:?} at byte {byte}")]
    UnexpectedCharacter { character: char, byte: usize },
    #[error("unterminated string starting at byte {byte}")]
    UnterminatedString { byte: usize },
    #[error("invalid number {text:?} at byte {byte}")]
    InvalidNumber { text: String, byte: usize },
    #[error("unexpected token {token} at token {index}")]
    UnexpectedToken { token: String, index: usize },
    #[default]
    #[error("expression ended unexpectedly")]
    UnexpectedEnd,
    #[error("unknown context key {0}")]
    UnknownContextKey(String),
    #[error("unknown function {0}")]
    UnknownFunction(String),
    #[error("{operator} does not accept {left} and {right}")]
    InvalidBinaryTypes { operator: &'static str, left: &'static str, right: &'static str },
    #[error("{operator} does not accept {operand}")]
    InvalidUnaryType { operator: &'static str, operand: &'static str },
    #[error("function {function} expected {expected}, received {actual} arguments")]
    ArgumentCount { function: String, expected: usize, actual: usize },
    #[error("function {function} argument {index} must be {expected}, not {actual}")]
    ArgumentType { function: String, index: usize, expected: &'static str, actual: &'static str },
    #[error("division by zero")]
    DivisionByZero,
}

#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(error = ExpressionError)]
#[logos(skip r"[ \t\r\n\f]+")]
enum Token {
    #[regex(r"(?:[0-9][0-9.]*|\.[0-9][0-9.]*)", lex_number)]
    Number(f64),
    #[token("\"", lex_string)]
    #[token("'", lex_string)]
    String(String),
    #[regex(r"[\p{Alphabetic}_][\p{Alphabetic}\p{Number}_.]*", |lexer| lexer.slice().to_owned())]
    Identifier(String),
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("!")]
    Bang,
    #[token("==")]
    EqualEqual,
    #[token("!=")]
    BangEqual,
    #[token("<")]
    Less,
    #[token("<=")]
    LessEqual,
    #[token(">")]
    Greater,
    #[token(">=")]
    GreaterEqual,
    #[token("&&")]
    AndAnd,
    #[token("||")]
    OrOr,
    #[token("(")]
    LeftParen,
    #[token(")")]
    RightParen,
    #[token("[")]
    LeftBracket,
    #[token("]")]
    RightBracket,
    #[token(",")]
    Comma,
    #[regex(r".", lex_unexpected, priority = 1)]
    Invalid,
}

impl Token {
    fn label(&self) -> String {
        match self {
            Self::Number(value) => value.to_string(),
            Self::String(value) => format!("{value:?}"),
            Self::Identifier(value) => value.clone(),
            Self::Plus => "+".to_owned(),
            Self::Minus => "-".to_owned(),
            Self::Star => "*".to_owned(),
            Self::Slash => "/".to_owned(),
            Self::Percent => "%".to_owned(),
            Self::Bang => "!".to_owned(),
            Self::EqualEqual => "==".to_owned(),
            Self::BangEqual => "!=".to_owned(),
            Self::Less => "<".to_owned(),
            Self::LessEqual => "<=".to_owned(),
            Self::Greater => ">".to_owned(),
            Self::GreaterEqual => ">=".to_owned(),
            Self::AndAnd => "&&".to_owned(),
            Self::OrOr => "||".to_owned(),
            Self::LeftParen => "(".to_owned(),
            Self::RightParen => ")".to_owned(),
            Self::LeftBracket => "[".to_owned(),
            Self::RightBracket => "]".to_owned(),
            Self::Comma => ",".to_owned(),
            Self::Invalid => "invalid token".to_owned(),
        }
    }
}

pub fn evaluate_expression(source: &str, context: &ExpressionContext) -> Result<Value, ExpressionError> {
    let tokens = lex(source)?;
    let mut parser = Parser { tokens: &tokens, cursor: 0, context };
    let value = parser.expression(0)?;
    if let Some(token) = parser.tokens.get(parser.cursor) {
        return Err(ExpressionError::UnexpectedToken { token: token.label(), index: parser.cursor });
    }
    Ok(value)
}

fn lex(source: &str) -> Result<Vec<Token>, ExpressionError> {
    Token::lexer(source).collect()
}

fn lex_number(lexer: &mut Lexer<'_, Token>) -> Result<f64, ExpressionError> {
    lexer.slice().parse().map_err(|_| ExpressionError::InvalidNumber { text: lexer.slice().to_owned(), byte: lexer.span().start })
}

fn lex_string(lexer: &mut Lexer<'_, Token>) -> Result<String, ExpressionError> {
    let start = lexer.span().start;
    let quote = lexer.slice().chars().next().unwrap_or('"');
    let mut value = String::new();
    let mut escaped = false;
    for (offset, next) in lexer.remainder().char_indices() {
        if escaped {
            value.push(unescape(next));
            escaped = false;
        } else if next == '\\' {
            escaped = true;
        } else if next == quote {
            lexer.bump(offset + next.len_utf8());
            return Ok(value);
        } else {
            value.push(next);
        }
    }
    Err(ExpressionError::UnterminatedString { byte: start })
}

fn unescape(character: char) -> char {
    match character {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        other => other,
    }
}

fn lex_unexpected(lexer: &mut Lexer<'_, Token>) -> Result<(), ExpressionError> {
    Err(ExpressionError::UnexpectedCharacter { character: lexer.slice().chars().next().unwrap_or('\0'), byte: lexer.span().start })
}

struct Parser<'a> {
    tokens: &'a [Token],
    cursor: usize,
    context: &'a ExpressionContext,
}

impl Parser<'_> {
    fn expression(&mut self, minimum_binding: u8) -> Result<Value, ExpressionError> {
        let mut left = self.prefix()?;
        while let Some(operator) = self.tokens.get(self.cursor) {
            let Some((left_binding, right_binding)) = binding_power(operator) else {
                break;
            };
            if left_binding < minimum_binding {
                break;
            }
            let operator = operator.clone();
            self.cursor += 1;
            let right = self.expression(right_binding)?;
            left = apply_binary(operator, left, right)?;
        }
        Ok(left)
    }

    fn prefix(&mut self) -> Result<Value, ExpressionError> {
        let token = self.tokens.get(self.cursor).cloned().ok_or(ExpressionError::UnexpectedEnd)?;
        self.cursor += 1;
        match token {
            Token::Number(value) => Ok(Value::Number(value)),
            Token::String(value) => Ok(Value::String(value)),
            Token::Identifier(identifier) => self.identifier(identifier),
            Token::Minus => match self.expression(11)? {
                Value::Number(value) => Ok(Value::Number(-value)),
                value => Err(ExpressionError::InvalidUnaryType { operator: "-", operand: value.type_name() }),
            },
            Token::Bang => Ok(Value::Bool(!self.expression(11)?.as_bool())),
            Token::LeftParen => {
                let value = self.expression(0)?;
                self.expect(Token::RightParen)?;
                Ok(value)
            }
            Token::LeftBracket => self.list(),
            unexpected => Err(ExpressionError::UnexpectedToken { token: unexpected.label(), index: self.cursor - 1 }),
        }
    }

    fn identifier(&mut self, identifier: String) -> Result<Value, ExpressionError> {
        match identifier.as_str() {
            "true" => return Ok(Value::Bool(true)),
            "false" => return Ok(Value::Bool(false)),
            "null" => return Ok(Value::Null),
            _ => {}
        }
        if self.tokens.get(self.cursor) == Some(&Token::LeftParen) {
            self.cursor += 1;
            let mut arguments = Vec::new();
            if self.tokens.get(self.cursor) != Some(&Token::RightParen) {
                loop {
                    arguments.push(self.expression(0)?);
                    if self.tokens.get(self.cursor) != Some(&Token::Comma) {
                        break;
                    }
                    self.cursor += 1;
                }
            }
            self.expect(Token::RightParen)?;
            call_function(&identifier, arguments)
        } else {
            self.context.get(&identifier).cloned().ok_or(ExpressionError::UnknownContextKey(identifier))
        }
    }

    fn list(&mut self) -> Result<Value, ExpressionError> {
        let mut values = Vec::new();
        if self.tokens.get(self.cursor) != Some(&Token::RightBracket) {
            loop {
                values.push(self.expression(0)?);
                if self.tokens.get(self.cursor) != Some(&Token::Comma) {
                    break;
                }
                self.cursor += 1;
            }
        }
        self.expect(Token::RightBracket)?;
        Ok(Value::List(values))
    }

    fn expect(&mut self, expected: Token) -> Result<(), ExpressionError> {
        match self.tokens.get(self.cursor) {
            Some(actual) if *actual == expected => {
                self.cursor += 1;
                Ok(())
            }
            Some(actual) => Err(ExpressionError::UnexpectedToken { token: actual.label(), index: self.cursor }),
            None => Err(ExpressionError::UnexpectedEnd),
        }
    }
}

fn binding_power(token: &Token) -> Option<(u8, u8)> {
    Some(match token {
        Token::OrOr => (1, 2),
        Token::AndAnd => (3, 4),
        Token::EqualEqual | Token::BangEqual => (5, 6),
        Token::Less | Token::LessEqual | Token::Greater | Token::GreaterEqual => (7, 8),
        Token::Plus | Token::Minus => (9, 10),
        Token::Star | Token::Slash | Token::Percent => (11, 12),
        _ => return None,
    })
}

fn apply_binary(operator: Token, left: Value, right: Value) -> Result<Value, ExpressionError> {
    match operator {
        Token::Plus => match (left, right) {
            (Value::Number(left), Value::Number(right)) => Ok(Value::Number(left + right)),
            (Value::String(mut left), Value::String(right)) => {
                left.push_str(&right);
                Ok(Value::String(left))
            }
            (Value::List(mut left), Value::List(right)) => {
                left.extend(right);
                Ok(Value::List(left))
            }
            (left, right) => invalid_binary("+", &left, &right),
        },
        Token::Minus => numbers("-", left, right, |left, right| left - right),
        Token::Star => numbers("*", left, right, |left, right| left * right),
        Token::Slash => {
            if right == Value::Number(0.0) {
                Err(ExpressionError::DivisionByZero)
            } else {
                numbers("/", left, right, |left, right| left / right)
            }
        }
        Token::Percent => {
            if right == Value::Number(0.0) {
                Err(ExpressionError::DivisionByZero)
            } else {
                numbers("%", left, right, |left, right| left % right)
            }
        }
        Token::EqualEqual => Ok(Value::Bool(left == right)),
        Token::BangEqual => Ok(Value::Bool(left != right)),
        Token::Less => comparison("<", left, right, |order| order.is_lt()),
        Token::LessEqual => comparison("<=", left, right, |order| order.is_le()),
        Token::Greater => comparison(">", left, right, |order| order.is_gt()),
        Token::GreaterEqual => comparison(">=", left, right, |order| order.is_ge()),
        Token::AndAnd => Ok(Value::Bool(left.as_bool() && right.as_bool())),
        Token::OrOr => Ok(Value::Bool(left.as_bool() || right.as_bool())),
        unexpected => Err(ExpressionError::UnexpectedToken { token: unexpected.label(), index: 0 }),
    }
}

fn numbers(operator: &'static str, left: Value, right: Value, operation: impl FnOnce(f64, f64) -> f64) -> Result<Value, ExpressionError> {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => Ok(Value::Number(operation(left, right))),
        (left, right) => invalid_binary(operator, &left, &right),
    }
}

fn comparison(operator: &'static str, left: Value, right: Value, predicate: impl FnOnce(std::cmp::Ordering) -> bool) -> Result<Value, ExpressionError> {
    let ordering = match (&left, &right) {
        (Value::Number(left), Value::Number(right)) => left.partial_cmp(right),
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        _ => return invalid_binary(operator, &left, &right),
    };
    Ok(Value::Bool(ordering.is_some_and(predicate)))
}

fn invalid_binary(operator: &'static str, left: &Value, right: &Value) -> Result<Value, ExpressionError> {
    Err(ExpressionError::InvalidBinaryTypes { operator, left: left.type_name(), right: right.type_name() })
}

fn call_function(name: &str, arguments: Vec<Value>) -> Result<Value, ExpressionError> {
    match name {
        "len" => {
            require_count(name, &arguments, 1)?;
            let length = match &arguments[0] {
                Value::String(value) => value.chars().count(),
                Value::List(value) => value.len(),
                value => return argument_type(name, 0, "string or list", value),
            };
            Ok(Value::Number(length as f64))
        }
        "upper" | "lower" => {
            require_count(name, &arguments, 1)?;
            let Value::String(value) = &arguments[0] else {
                return argument_type(name, 0, "string", &arguments[0]);
            };
            Ok(Value::String(if name == "upper" { value.to_uppercase() } else { value.to_lowercase() }))
        }
        "contains" => {
            require_count(name, &arguments, 2)?;
            let Value::String(haystack) = &arguments[0] else {
                return argument_type(name, 0, "string", &arguments[0]);
            };
            let Value::String(needle) = &arguments[1] else {
                return argument_type(name, 1, "string", &arguments[1]);
            };
            Ok(Value::Bool(haystack.contains(needle)))
        }
        "join" => {
            require_count(name, &arguments, 2)?;
            let Value::List(values) = &arguments[0] else {
                return argument_type(name, 0, "list", &arguments[0]);
            };
            let Value::String(separator) = &arguments[1] else {
                return argument_type(name, 1, "string", &arguments[1]);
            };
            Ok(Value::String(values.iter().map(Value::to_editor_text).collect::<Vec<_>>().join(separator)))
        }
        _ => Err(ExpressionError::UnknownFunction(name.to_owned())),
    }
}

fn require_count(name: &str, arguments: &[Value], expected: usize) -> Result<(), ExpressionError> {
    if arguments.len() == expected { Ok(()) } else { Err(ExpressionError::ArgumentCount { function: name.to_owned(), expected, actual: arguments.len() }) }
}

fn argument_type(name: &str, index: usize, expected: &'static str, actual: &Value) -> Result<Value, ExpressionError> {
    Err(ExpressionError::ArgumentType { function: name.to_owned(), index, expected, actual: actual.type_name() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_precedence_strings_lists_and_context() {
        let context =
            ExpressionContext::new().with("cursor.line", Value::Number(4.0)).with("café", Value::Bool(true)).with("workspace.trusted", Value::Bool(true));
        assert_eq!(evaluate_expression("1 + 2 * 3", &context), Ok(Value::Number(7.0)));
        assert_eq!(evaluate_expression("upper('wr' + 'en')", &context), Ok(Value::String("WREN".to_owned())));
        assert_eq!(evaluate_expression("join(['a', cursor.line], ':')", &context), Ok(Value::String("a:4".to_owned())));
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
