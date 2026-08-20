#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod ex;
mod expression;

pub use ex::{BufferAction, ExAddress, ExCommand, ExError, ExRange, SubstituteFlags, TabAction, ex_command_completions, parse_ex};
pub use expression::{ExpressionContext, ExpressionError, Value, evaluate_expression, expression_editor_text};

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};
use thiserror::Error;
pub use wren_types::{KeyCode, KeyEvent, Modifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operator {
    Delete,
    Change,
    Yank,
    Indent,
    Outdent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextObject {
    Word,
    BigWord,
    Paragraph,
    Quotes(char),
    Brackets(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordForward,
    WordEnd,
    WordBackward,
    BigWordForward,
    BigWordEnd,
    BigWordBackward,
    WordEndBackward,
    LineStart,
    FirstNonBlank,
    NextLineFirstNonBlank,
    PreviousLineFirstNonBlank,
    LineFirstNonBlank,
    LastNonBlank,
    Column,
    LineEnd,
    GoToLine,
    DocumentEnd,
    WholeLine,
    Find { character: char, forward: bool, till: bool },
    ParagraphForward,
    ParagraphBackward,
    MatchPair,
    Inside(TextObject),
    Around(TextObject),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RangeKind {
    CharacterWise,
    LineWise,
    BlockWise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Register {
    Unnamed,
    Named(char),
    Numbered(u8),
    SmallDelete,
    BlackHole,
    Clipboard,
    PrimarySelection,
    Expression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetAction {
    RecordMacro,
    ReplayMacro,
    SetMark,
    JumpMark { linewise: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    ApplyOperator { operator: Operator, motion: Motion, count: NonZeroU32, register: Option<Register>, range_kind: RangeKind },
    Move { motion: Motion, count: NonZeroU32 },
    EnterInsert,
    EnterAppend,
    EnterInsertAtLineStart,
    EnterInsertAtLineEnd,
    EnterReplace,
    OpenLine { above: bool },
    DeleteChar { backward: bool, count: NonZeroU32, register: Option<Register> },
    JoinLines { count: NonZeroU32 },
    ReplaceChar { character: char, count: NonZeroU32 },
    ToggleCase { count: NonZeroU32 },
    Paste { before: bool, count: NonZeroU32, register: Option<Register> },
    Undo { count: NonZeroU32 },
    Redo { count: NonZeroU32 },
    SearchNext { reverse: bool, count: NonZeroU32 },
    Repeat { count: NonZeroU32 },
    Target { action: TargetAction, target: char, count: NonZeroU32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseState {
    Count(NonZeroU32),
    Register,
    Operator,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseResult {
    Pending(ParseState),
    Command(Command),
    Invalid(GrammarError),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum GrammarError {
    #[error("unsupported modifier at key {index}")]
    UnsupportedModifier { index: usize },
    #[error("invalid register {register:?} at key {index}")]
    InvalidRegister { index: usize, register: char },
    #[error("unexpected key at index {index}: {key:?}")]
    UnexpectedKey { index: usize, key: KeyCode },
    #[error("count overflow at key {index}")]
    CountOverflow { index: usize },
    #[error("extra key at index {index}")]
    TrailingInput { index: usize },
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Grammar;

impl Grammar {
    #[must_use]
    pub fn parse(self, keys: &[KeyEvent]) -> ParseResult {
        match parse_impl(keys) {
            Ok(result) => result,
            Err(error) => ParseResult::Invalid(error),
        }
    }
}

fn parse_impl(keys: &[KeyEvent]) -> Result<ParseResult, GrammarError> {
    if let [key] = keys
        && key.modifiers.is_empty()
        && let Some(command) = complete_normal_command(key.code, NonZeroU32::MIN, None)
    {
        return Ok(ParseResult::Command(command));
    }
    if let [key] = keys
        && key.code == KeyCode::Char('r')
        && key.modifiers == Modifiers::CONTROL
    {
        return Ok(ParseResult::Command(Command::Redo { count: NonZeroU32::MIN }));
    }

    let mut cursor = 0;
    let prefix_count = parse_count(keys, &mut cursor)?;
    let count = nonzero(prefix_count.unwrap_or(1));
    if cursor == keys.len() {
        return Ok(ParseResult::Pending(ParseState::Count(count)));
    }

    let mut register = None;
    if char_at(keys, cursor)? == Some('"') {
        cursor += 1;
        if cursor == keys.len() {
            return Ok(ParseResult::Pending(ParseState::Register));
        }
        let character = require_char(keys, cursor)?;
        register = Some(parse_register(character, cursor)?);
        cursor += 1;
    }
    if cursor == keys.len() {
        return Ok(ParseResult::Pending(ParseState::Register));
    }

    let key = checked_key(keys, cursor)?;
    if let Some(operator) = operator_for(key.code) {
        cursor += 1;
        let post_count = parse_count(keys, &mut cursor)?.unwrap_or(1);
        let combined = count.get().checked_mul(post_count).ok_or(GrammarError::CountOverflow { index: cursor })?;
        let operator_count = nonzero(combined);
        if cursor == keys.len() {
            return Ok(ParseResult::Pending(ParseState::Operator));
        }
        let motion_key = checked_key(keys, cursor)?;
        if operator_for(motion_key.code) == Some(operator) {
            return finish(
                keys,
                cursor + 1,
                Command::ApplyOperator { operator, motion: Motion::WholeLine, count: operator_count, register, range_kind: RangeKind::LineWise },
            );
        }
        if matches!(motion_key.code, KeyCode::Char('i' | 'a')) {
            let around = motion_key.code == KeyCode::Char('a');
            cursor += 1;
            if cursor == keys.len() {
                return Ok(ParseResult::Pending(ParseState::Other));
            }
            let object = text_object(require_char(keys, cursor)?, cursor)?;
            return finish(
                keys,
                cursor + 1,
                Command::ApplyOperator {
                    operator,
                    motion: if around { Motion::Around(object) } else { Motion::Inside(object) },
                    count: operator_count,
                    register,
                    range_kind: RangeKind::CharacterWise,
                },
            );
        }
        return parse_motion_command(keys, cursor, Some(operator), operator_count, register);
    }

    parse_normal_command(keys, cursor, count, register)
}

fn parse_motion_command(
    keys: &[KeyEvent],
    cursor: usize,
    operator: Option<Operator>,
    count: NonZeroU32,
    register: Option<Register>,
) -> Result<ParseResult, GrammarError> {
    let key = checked_key(keys, cursor)?;
    let (motion, end) = match key.code {
        KeyCode::Char(character @ ('f' | 'F' | 't' | 'T')) => {
            let forward = matches!(character, 'f' | 't');
            let till = matches!(character, 't' | 'T');
            if cursor + 1 == keys.len() {
                return Ok(ParseResult::Pending(ParseState::Other));
            }
            (Motion::Find { character: require_char(keys, cursor + 1)?, forward, till }, cursor + 2)
        }
        KeyCode::Char('g') => {
            if cursor + 1 == keys.len() {
                return Ok(ParseResult::Pending(ParseState::Other));
            }
            (g_motion(keys, cursor + 1)?, cursor + 2)
        }
        code => (motion_for(code).ok_or(GrammarError::UnexpectedKey { index: cursor, key: code })?, cursor + 1),
    };
    operator.map_or_else(|| finish(keys, end, Command::Move { motion, count }), |operator| finish_operator(keys, end, operator, motion, count, register))
}

fn finish_operator(
    keys: &[KeyEvent],
    cursor: usize,
    operator: Operator,
    motion: Motion,
    count: NonZeroU32,
    register: Option<Register>,
) -> Result<ParseResult, GrammarError> {
    let range_kind = if matches!(
        motion,
        Motion::Up
            | Motion::Down
            | Motion::GoToLine
            | Motion::WholeLine
            | Motion::NextLineFirstNonBlank
            | Motion::PreviousLineFirstNonBlank
            | Motion::LineFirstNonBlank
    ) {
        RangeKind::LineWise
    } else {
        RangeKind::CharacterWise
    };
    finish(keys, cursor, Command::ApplyOperator { operator, motion, count, register, range_kind })
}

fn parse_normal_command(keys: &[KeyEvent], cursor: usize, count: NonZeroU32, register: Option<Register>) -> Result<ParseResult, GrammarError> {
    let key = checked_key(keys, cursor)?;
    match key.code {
        KeyCode::Char('g' | 'f' | 'F' | 't' | 'T') => parse_motion_command(keys, cursor, None, count, register),
        KeyCode::Char('r') => parse_replace_command(keys, cursor, count),
        KeyCode::Char(prefix @ ('q' | '@' | 'm' | '\'' | '`')) if register.is_none() => parse_target_command(keys, cursor, count, target_action(prefix)),
        code => finish(keys, cursor + 1, simple_normal_command(code, cursor, count, register)?),
    }
}

fn parse_target_command(keys: &[KeyEvent], cursor: usize, count: NonZeroU32, action: TargetAction) -> Result<ParseResult, GrammarError> {
    if cursor + 1 == keys.len() {
        return Ok(ParseResult::Pending(ParseState::Other));
    }
    let target = require_char(keys, cursor + 1)?;
    let valid = match action {
        TargetAction::RecordMacro => target.is_ascii_alphanumeric(),
        TargetAction::SetMark => target.is_ascii_alphabetic(),
        TargetAction::ReplayMacro | TargetAction::JumpMark { .. } => true,
    };
    if !valid {
        return Err(GrammarError::UnexpectedKey { index: cursor + 1, key: KeyCode::Char(target) });
    }
    finish(keys, cursor + 2, Command::Target { action, target, count })
}

const fn target_action(prefix: char) -> TargetAction {
    match prefix {
        'q' => TargetAction::RecordMacro,
        '@' => TargetAction::ReplayMacro,
        'm' => TargetAction::SetMark,
        '\'' => TargetAction::JumpMark { linewise: true },
        _ => TargetAction::JumpMark { linewise: false },
    }
}

fn g_motion(keys: &[KeyEvent], cursor: usize) -> Result<Motion, GrammarError> {
    let motion = match require_char(keys, cursor)? {
        'g' => Motion::GoToLine,
        'e' => Motion::WordEndBackward,
        'E' => Motion::BigWordBackward,
        '0' => Motion::LineStart,
        '$' => Motion::LineEnd,
        '_' => Motion::LastNonBlank,
        _ => {
            return Err(GrammarError::UnexpectedKey { index: cursor, key: checked_key(keys, cursor)?.code });
        }
    };
    Ok(motion)
}

fn parse_replace_command(keys: &[KeyEvent], cursor: usize, count: NonZeroU32) -> Result<ParseResult, GrammarError> {
    if cursor + 1 == keys.len() {
        return Ok(ParseResult::Pending(ParseState::Other));
    }
    finish(keys, cursor + 2, Command::ReplaceChar { character: require_char(keys, cursor + 1)?, count })
}

fn simple_normal_command(code: KeyCode, index: usize, count: NonZeroU32, register: Option<Register>) -> Result<Command, GrammarError> {
    complete_normal_command(code, count, register).ok_or(GrammarError::UnexpectedKey { index, key: code })
}

fn complete_normal_command(code: KeyCode, count: NonZeroU32, register: Option<Register>) -> Option<Command> {
    let command = match code {
        KeyCode::Char('i') => Command::EnterInsert,
        KeyCode::Char('a') => Command::EnterAppend,
        KeyCode::Char('I') => Command::EnterInsertAtLineStart,
        KeyCode::Char('A') => Command::EnterInsertAtLineEnd,
        KeyCode::Char('R') => Command::EnterReplace,
        KeyCode::Char('o' | 'O') => Command::OpenLine { above: code == KeyCode::Char('O') },
        KeyCode::Char('x' | 'X') | KeyCode::Delete | KeyCode::Backspace => {
            Command::DeleteChar { backward: matches!(code, KeyCode::Char('X') | KeyCode::Backspace), count, register }
        }
        KeyCode::Char('J') => Command::JoinLines { count },
        KeyCode::Char(character @ ('D' | 'C' | 'Y')) => line_operator_command(character, count, register),
        KeyCode::Char(character @ ('s' | 'S')) => substitute_command(character, count, register),
        KeyCode::Char('~') => Command::ToggleCase { count },
        KeyCode::Char('p' | 'P') => Command::Paste { before: code == KeyCode::Char('P'), count, register },
        KeyCode::Char('u') => Command::Undo { count },
        KeyCode::Char('n' | 'N') => Command::SearchNext { reverse: code == KeyCode::Char('N'), count },
        KeyCode::Char('.') => Command::Repeat { count },
        code => Command::Move { motion: motion_for(code)?, count },
    };
    Some(command)
}

fn line_operator_command(character: char, count: NonZeroU32, register: Option<Register>) -> Command {
    let linewise = character == 'Y';
    Command::ApplyOperator {
        operator: match character {
            'C' => Operator::Change,
            'Y' => Operator::Yank,
            _ => Operator::Delete,
        },
        motion: if linewise { Motion::WholeLine } else { Motion::LineEnd },
        count,
        register,
        range_kind: if linewise { RangeKind::LineWise } else { RangeKind::CharacterWise },
    }
}

fn substitute_command(character: char, count: NonZeroU32, register: Option<Register>) -> Command {
    let linewise = character == 'S';
    Command::ApplyOperator {
        operator: Operator::Change,
        motion: if linewise { Motion::WholeLine } else { Motion::Right },
        count,
        register,
        range_kind: if linewise { RangeKind::LineWise } else { RangeKind::CharacterWise },
    }
}

fn finish(keys: &[KeyEvent], cursor: usize, command: Command) -> Result<ParseResult, GrammarError> {
    if cursor == keys.len() { Ok(ParseResult::Command(command)) } else { Err(GrammarError::TrailingInput { index: cursor }) }
}

fn parse_count(keys: &[KeyEvent], cursor: &mut usize) -> Result<Option<u32>, GrammarError> {
    let mut value: Option<u32> = None;
    while *cursor < keys.len() {
        let Some(character) = char_at(keys, *cursor)? else {
            break;
        };
        if !character.is_ascii_digit() || (value.is_none() && character == '0') {
            break;
        }
        let digit = character.to_digit(10).ok_or(GrammarError::UnexpectedKey { index: *cursor, key: KeyCode::Char(character) })?;
        value = Some(value.unwrap_or(0).checked_mul(10).and_then(|current| current.checked_add(digit)).ok_or(GrammarError::CountOverflow { index: *cursor })?);
        *cursor += 1;
    }
    Ok(value)
}

fn checked_key(keys: &[KeyEvent], index: usize) -> Result<KeyEvent, GrammarError> {
    let key = keys.get(index).copied().ok_or(GrammarError::TrailingInput { index })?;
    if key.modifiers.is_empty() { Ok(key) } else { Err(GrammarError::UnsupportedModifier { index }) }
}

fn char_at(keys: &[KeyEvent], index: usize) -> Result<Option<char>, GrammarError> {
    Ok(match checked_key(keys, index)?.code {
        KeyCode::Char(character) => Some(character),
        _ => None,
    })
}

fn require_char(keys: &[KeyEvent], index: usize) -> Result<char, GrammarError> {
    char_at(keys, index)?.ok_or_else(|| GrammarError::UnexpectedKey { index, key: keys.get(index).map_or(KeyCode::Escape, |key| key.code) })
}

fn nonzero(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap_or(NonZeroU32::MIN)
}

fn operator_for(code: KeyCode) -> Option<Operator> {
    match code {
        KeyCode::Char('d') => Some(Operator::Delete),
        KeyCode::Char('c') => Some(Operator::Change),
        KeyCode::Char('y') => Some(Operator::Yank),
        KeyCode::Char('>') => Some(Operator::Indent),
        KeyCode::Char('<') => Some(Operator::Outdent),
        _ => None,
    }
}

fn motion_for(code: KeyCode) -> Option<Motion> {
    match code {
        KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => Some(Motion::Left),
        KeyCode::Char('l') | KeyCode::Right | KeyCode::Delete => Some(Motion::Right),
        KeyCode::Char('k') | KeyCode::Up => Some(Motion::Up),
        KeyCode::Char('j') | KeyCode::Down => Some(Motion::Down),
        KeyCode::Char('w') => Some(Motion::WordForward),
        KeyCode::Char('e') => Some(Motion::WordEnd),
        KeyCode::Char('b') => Some(Motion::WordBackward),
        KeyCode::Char('W') => Some(Motion::BigWordForward),
        KeyCode::Char('E') => Some(Motion::BigWordEnd),
        KeyCode::Char('B') => Some(Motion::BigWordBackward),
        KeyCode::Char('0') | KeyCode::Home => Some(Motion::LineStart),
        KeyCode::Char('^') => Some(Motion::FirstNonBlank),
        KeyCode::Char('+') | KeyCode::Enter => Some(Motion::NextLineFirstNonBlank),
        KeyCode::Char('-') => Some(Motion::PreviousLineFirstNonBlank),
        KeyCode::Char('_') => Some(Motion::LineFirstNonBlank),
        KeyCode::Char('|') => Some(Motion::Column),
        KeyCode::Char('$') | KeyCode::End => Some(Motion::LineEnd),
        KeyCode::Char('G') => Some(Motion::DocumentEnd),
        KeyCode::Char('}') => Some(Motion::ParagraphForward),
        KeyCode::Char('{') => Some(Motion::ParagraphBackward),
        KeyCode::Char('%') => Some(Motion::MatchPair),
        _ => None,
    }
}

fn parse_register(character: char, index: usize) -> Result<Register, GrammarError> {
    match character {
        'a'..='z' | 'A'..='Z' => Ok(Register::Named(character)),
        '0'..='9' => Ok(Register::Numbered(character as u8 - b'0')),
        '"' => Ok(Register::Unnamed),
        '-' => Ok(Register::SmallDelete),
        '_' => Ok(Register::BlackHole),
        '+' => Ok(Register::Clipboard),
        '*' => Ok(Register::PrimarySelection),
        '=' => Ok(Register::Expression),
        _ => Err(GrammarError::InvalidRegister { index, register: character }),
    }
}

fn text_object(character: char, index: usize) -> Result<TextObject, GrammarError> {
    match character {
        'w' => Ok(TextObject::Word),
        'W' => Ok(TextObject::BigWord),
        'p' => Ok(TextObject::Paragraph),
        '\'' | '"' | '`' => Ok(TextObject::Quotes(character)),
        '(' | ')' | 'b' => Ok(TextObject::Brackets('(')),
        '[' | ']' => Ok(TextObject::Brackets('[')),
        '{' | '}' | 'B' => Ok(TextObject::Brackets('{')),
        '<' | '>' => Ok(TextObject::Brackets('<')),
        _ => Err(GrammarError::UnexpectedKey { index, key: KeyCode::Char(character) }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Vec<KeyEvent> {
        text.chars().map(KeyEvent::character).collect()
    }

    fn movement(motion: Motion) -> ParseResult {
        ParseResult::Command(Command::Move { motion, count: NonZeroU32::MIN })
    }

    fn operation(motion: Motion, count: u32, range_kind: RangeKind) -> ParseResult {
        ParseResult::Command(Command::ApplyOperator { operator: Operator::Delete, motion, count: nonzero(count), register: None, range_kind })
    }

    #[test]
    fn parses_normal_command_table() {
        let cases = [
            ("3dd", operation(Motion::WholeLine, 3, RangeKind::LineWise)),
            ("gg", movement(Motion::GoToLine)),
            ("W", movement(Motion::BigWordForward)),
            ("ge", movement(Motion::WordEndBackward)),
            ("dt,", operation(Motion::Find { character: ',', forward: true, till: true }, 1, RangeKind::CharacterWise)),
            ("%", movement(Motion::MatchPair)),
            ("}", movement(Motion::ParagraphForward)),
            ("A", ParseResult::Command(Command::EnterInsertAtLineEnd)),
            ("r界", ParseResult::Command(Command::ReplaceChar { character: '界', count: NonZeroU32::MIN })),
            ("3x", ParseResult::Command(Command::DeleteChar { backward: false, count: nonzero(3), register: None })),
            (
                "ciw",
                ParseResult::Command(Command::ApplyOperator {
                    operator: Operator::Change,
                    motion: Motion::Inside(TextObject::Word),
                    count: NonZeroU32::MIN,
                    register: None,
                    range_kind: RangeKind::CharacterWise,
                }),
            ),
            ("2\"ad", ParseResult::Pending(ParseState::Operator)),
            (
                "2\"ad3w",
                ParseResult::Command(Command::ApplyOperator {
                    operator: Operator::Delete,
                    motion: Motion::WordForward,
                    count: nonzero(6),
                    register: Some(Register::Named('a')),
                    range_kind: RangeKind::CharacterWise,
                }),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(Grammar.parse(&keys(input)), expected, "{input}");
        }
    }

    #[test]
    fn control_r_is_redo() {
        assert!(matches!(Grammar.parse(&[KeyEvent { code: KeyCode::Char('r'), modifiers: Modifiers::CONTROL }]), ParseResult::Command(Command::Redo { .. })));
    }

    #[test]
    fn expression_register_is_typed_but_expression_input_stays_outside_key_grammar() {
        assert_eq!(Grammar.parse(&keys("\"=")), ParseResult::Pending(ParseState::Register));
        assert!(matches!(Grammar.parse(&keys("\"=p")), ParseResult::Command(Command::Paste { register: Some(Register::Expression), .. })));
    }
}
