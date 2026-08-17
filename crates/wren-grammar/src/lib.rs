#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod ex;
mod expression;

pub use ex::{
    BufferAction, ExAddress, ExCommand, ExError, ExRange, SubstituteFlags, TabAction, parse_ex,
};
pub use expression::{ExpressionContext, ExpressionError, Value, evaluate_expression};

use std::num::NonZeroU32;

use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use thiserror::Error;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    pub struct Modifiers: u8 {
        const SHIFT = 1 << 0;
        const CONTROL = 1 << 1;
        const ALT = 1 << 2;
        const SUPER = 1 << 3;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeyCode {
    Char(char),
    Escape,
    Enter,
    Tab,
    Backspace,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: Modifiers,
}

impl KeyEvent {
    #[must_use]
    pub const fn plain(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: Modifiers::empty(),
        }
    }

    #[must_use]
    pub const fn character(character: char) -> Self {
        Self::plain(KeyCode::Char(character))
    }
}

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
    FindForward(char),
    FindBackward(char),
    TillForward(char),
    TillBackward(char),
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    ApplyOperator {
        operator: Operator,
        motion: Motion,
        count: NonZeroU32,
        register: Option<Register>,
        range_kind: RangeKind,
    },
    Move {
        motion: Motion,
        count: NonZeroU32,
    },
    EnterInsert,
    EnterAppend,
    EnterInsertAtLineStart,
    EnterInsertAtLineEnd,
    EnterReplace,
    OpenLine {
        above: bool,
    },
    DeleteChar {
        backward: bool,
        count: NonZeroU32,
        register: Option<Register>,
    },
    JoinLines {
        count: NonZeroU32,
    },
    ReplaceChar {
        character: char,
        count: NonZeroU32,
    },
    ToggleCase {
        count: NonZeroU32,
    },
    Paste {
        before: bool,
        count: NonZeroU32,
        register: Option<Register>,
    },
    Undo {
        count: NonZeroU32,
    },
    Redo {
        count: NonZeroU32,
    },
    SearchNext {
        reverse: bool,
        count: NonZeroU32,
    },
    Repeat {
        count: NonZeroU32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseState {
    Count {
        value: NonZeroU32,
    },
    Register {
        count: NonZeroU32,
    },
    OperatorPending {
        operator: Operator,
        count: NonZeroU32,
        register: Option<Register>,
    },
    TextObjectPending {
        operator: Operator,
        count: NonZeroU32,
        register: Option<Register>,
        around: bool,
    },
    FindCharacterPending {
        operator: Option<Operator>,
        count: NonZeroU32,
        register: Option<Register>,
        forward: bool,
        till: bool,
    },
    PrefixG {
        operator: Option<Operator>,
        count: NonZeroU32,
        register: Option<Register>,
    },
    ReplaceCharacterPending {
        count: NonZeroU32,
    },
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
        return Ok(ParseResult::Command(Command::Redo {
            count: NonZeroU32::MIN,
        }));
    }

    let mut cursor = 0;
    let prefix_count = parse_count(keys, &mut cursor)?;
    let count = nonzero(prefix_count.unwrap_or(1));
    if cursor == keys.len() {
        return Ok(ParseResult::Pending(ParseState::Count { value: count }));
    }

    let mut register = None;
    if char_at(keys, cursor)? == Some('"') {
        cursor += 1;
        if cursor == keys.len() {
            return Ok(ParseResult::Pending(ParseState::Register { count }));
        }
        let character = require_char(keys, cursor)?;
        register = Some(parse_register(character, cursor)?);
        cursor += 1;
    }
    if cursor == keys.len() {
        return Ok(ParseResult::Pending(ParseState::Register { count }));
    }

    let key = checked_key(keys, cursor)?;
    if let Some(operator) = operator_for(key.code) {
        cursor += 1;
        let post_count = parse_count(keys, &mut cursor)?.unwrap_or(1);
        let combined = count
            .get()
            .checked_mul(post_count)
            .ok_or(GrammarError::CountOverflow { index: cursor })?;
        let operator_count = nonzero(combined);
        if cursor == keys.len() {
            return Ok(ParseResult::Pending(ParseState::OperatorPending {
                operator,
                count: operator_count,
                register,
            }));
        }
        let motion_key = checked_key(keys, cursor)?;
        if operator_for(motion_key.code) == Some(operator) {
            return finish(
                keys,
                cursor + 1,
                Command::ApplyOperator {
                    operator,
                    motion: Motion::WholeLine,
                    count: operator_count,
                    register,
                    range_kind: RangeKind::LineWise,
                },
            );
        }
        if matches!(motion_key.code, KeyCode::Char('i' | 'a')) {
            let around = motion_key.code == KeyCode::Char('a');
            cursor += 1;
            if cursor == keys.len() {
                return Ok(ParseResult::Pending(ParseState::TextObjectPending {
                    operator,
                    count: operator_count,
                    register,
                    around,
                }));
            }
            let object = text_object(require_char(keys, cursor)?, cursor)?;
            return finish(
                keys,
                cursor + 1,
                Command::ApplyOperator {
                    operator,
                    motion: if around {
                        Motion::Around(object)
                    } else {
                        Motion::Inside(object)
                    },
                    count: operator_count,
                    register,
                    range_kind: RangeKind::CharacterWise,
                },
            );
        }
        return parse_operator_motion(keys, cursor, operator, operator_count, register);
    }

    parse_normal_command(keys, cursor, count, register)
}

fn parse_operator_motion(
    keys: &[KeyEvent],
    cursor: usize,
    operator: Operator,
    count: NonZeroU32,
    register: Option<Register>,
) -> Result<ParseResult, GrammarError> {
    let key = checked_key(keys, cursor)?;
    if matches!(key.code, KeyCode::Char('f' | 'F' | 't' | 'T')) {
        let forward = matches!(key.code, KeyCode::Char('f' | 't'));
        let till = matches!(key.code, KeyCode::Char('t' | 'T'));
        if cursor + 1 == keys.len() {
            return Ok(ParseResult::Pending(ParseState::FindCharacterPending {
                operator: Some(operator),
                count,
                register,
                forward,
                till,
            }));
        }
        let character = require_char(keys, cursor + 1)?;
        return finish_operator(
            keys,
            cursor + 2,
            operator,
            match (forward, till) {
                (true, false) => Motion::FindForward(character),
                (false, false) => Motion::FindBackward(character),
                (true, true) => Motion::TillForward(character),
                (false, true) => Motion::TillBackward(character),
            },
            count,
            register,
        );
    }
    if key.code == KeyCode::Char('g') {
        if cursor + 1 == keys.len() {
            return Ok(ParseResult::Pending(ParseState::PrefixG {
                operator: Some(operator),
                count,
                register,
            }));
        }
        let motion = match require_char(keys, cursor + 1)? {
            'g' => Motion::GoToLine,
            'e' => Motion::WordEndBackward,
            'E' => Motion::BigWordBackward,
            '0' => Motion::LineStart,
            '$' => Motion::LineEnd,
            '_' => Motion::LastNonBlank,
            _ => {
                return Err(GrammarError::UnexpectedKey {
                    index: cursor + 1,
                    key: checked_key(keys, cursor + 1)?.code,
                });
            }
        };
        return finish_operator(keys, cursor + 2, operator, motion, count, register);
    }
    let motion = motion_for(key.code).ok_or(GrammarError::UnexpectedKey {
        index: cursor,
        key: key.code,
    })?;
    finish_operator(keys, cursor + 1, operator, motion, count, register)
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
    finish(
        keys,
        cursor,
        Command::ApplyOperator {
            operator,
            motion,
            count,
            register,
            range_kind,
        },
    )
}

fn parse_normal_command(
    keys: &[KeyEvent],
    cursor: usize,
    count: NonZeroU32,
    register: Option<Register>,
) -> Result<ParseResult, GrammarError> {
    let key = checked_key(keys, cursor)?;
    match key.code {
        KeyCode::Char('g') => parse_g_command(keys, cursor, count, register),
        KeyCode::Char('f' | 'F' | 't' | 'T') => {
            parse_find_command(keys, cursor, key.code, count, register)
        }
        KeyCode::Char('r') => parse_replace_command(keys, cursor, count),
        code => finish(
            keys,
            cursor + 1,
            simple_normal_command(code, cursor, count, register)?,
        ),
    }
}

fn parse_g_command(
    keys: &[KeyEvent],
    cursor: usize,
    count: NonZeroU32,
    register: Option<Register>,
) -> Result<ParseResult, GrammarError> {
    if cursor + 1 == keys.len() {
        return Ok(ParseResult::Pending(ParseState::PrefixG {
            operator: None,
            count,
            register,
        }));
    }
    let motion = match require_char(keys, cursor + 1)? {
        'g' => Motion::GoToLine,
        'e' => Motion::WordEndBackward,
        'E' => Motion::BigWordBackward,
        '0' => Motion::LineStart,
        '$' => Motion::LineEnd,
        '_' => Motion::LastNonBlank,
        _ => {
            return Err(GrammarError::UnexpectedKey {
                index: cursor + 1,
                key: checked_key(keys, cursor + 1)?.code,
            });
        }
    };
    finish(keys, cursor + 2, Command::Move { motion, count })
}

fn parse_find_command(
    keys: &[KeyEvent],
    cursor: usize,
    code: KeyCode,
    count: NonZeroU32,
    register: Option<Register>,
) -> Result<ParseResult, GrammarError> {
    let forward = matches!(code, KeyCode::Char('f' | 't'));
    let till = matches!(code, KeyCode::Char('t' | 'T'));
    if cursor + 1 == keys.len() {
        return Ok(ParseResult::Pending(ParseState::FindCharacterPending {
            operator: None,
            count,
            register,
            forward,
            till,
        }));
    }
    let character = require_char(keys, cursor + 1)?;
    let motion = match (forward, till) {
        (true, false) => Motion::FindForward(character),
        (false, false) => Motion::FindBackward(character),
        (true, true) => Motion::TillForward(character),
        (false, true) => Motion::TillBackward(character),
    };
    finish(keys, cursor + 2, Command::Move { motion, count })
}

fn parse_replace_command(
    keys: &[KeyEvent],
    cursor: usize,
    count: NonZeroU32,
) -> Result<ParseResult, GrammarError> {
    if cursor + 1 == keys.len() {
        return Ok(ParseResult::Pending(ParseState::ReplaceCharacterPending {
            count,
        }));
    }
    finish(
        keys,
        cursor + 2,
        Command::ReplaceChar {
            character: require_char(keys, cursor + 1)?,
            count,
        },
    )
}

fn simple_normal_command(
    code: KeyCode,
    index: usize,
    count: NonZeroU32,
    register: Option<Register>,
) -> Result<Command, GrammarError> {
    complete_normal_command(code, count, register)
        .ok_or(GrammarError::UnexpectedKey { index, key: code })
}

fn complete_normal_command(
    code: KeyCode,
    count: NonZeroU32,
    register: Option<Register>,
) -> Option<Command> {
    let command = match code {
        KeyCode::Char('i') => Command::EnterInsert,
        KeyCode::Char('a') => Command::EnterAppend,
        KeyCode::Char('I') => Command::EnterInsertAtLineStart,
        KeyCode::Char('A') => Command::EnterInsertAtLineEnd,
        KeyCode::Char('R') => Command::EnterReplace,
        KeyCode::Char('o' | 'O') => Command::OpenLine {
            above: code == KeyCode::Char('O'),
        },
        KeyCode::Char('x' | 'X') | KeyCode::Delete | KeyCode::Backspace => Command::DeleteChar {
            backward: matches!(code, KeyCode::Char('X') | KeyCode::Backspace),
            count,
            register,
        },
        KeyCode::Char('J') => Command::JoinLines { count },
        KeyCode::Char(character @ ('D' | 'C' | 'Y')) => {
            line_operator_command(character, count, register)
        }
        KeyCode::Char(character @ ('s' | 'S')) => substitute_command(character, count, register),
        KeyCode::Char('~') => Command::ToggleCase { count },
        KeyCode::Char('p' | 'P') => Command::Paste {
            before: code == KeyCode::Char('P'),
            count,
            register,
        },
        KeyCode::Char('u') => Command::Undo { count },
        KeyCode::Char('n' | 'N') => Command::SearchNext {
            reverse: code == KeyCode::Char('N'),
            count,
        },
        KeyCode::Char('.') => Command::Repeat { count },
        code => Command::Move {
            motion: motion_for(code)?,
            count,
        },
    };
    Some(command)
}

fn line_operator_command(
    character: char,
    count: NonZeroU32,
    register: Option<Register>,
) -> Command {
    let linewise = character == 'Y';
    Command::ApplyOperator {
        operator: match character {
            'C' => Operator::Change,
            'Y' => Operator::Yank,
            _ => Operator::Delete,
        },
        motion: if linewise {
            Motion::WholeLine
        } else {
            Motion::LineEnd
        },
        count,
        register,
        range_kind: if linewise {
            RangeKind::LineWise
        } else {
            RangeKind::CharacterWise
        },
    }
}

fn substitute_command(character: char, count: NonZeroU32, register: Option<Register>) -> Command {
    let linewise = character == 'S';
    Command::ApplyOperator {
        operator: Operator::Change,
        motion: if linewise {
            Motion::WholeLine
        } else {
            Motion::Right
        },
        count,
        register,
        range_kind: if linewise {
            RangeKind::LineWise
        } else {
            RangeKind::CharacterWise
        },
    }
}

fn finish(keys: &[KeyEvent], cursor: usize, command: Command) -> Result<ParseResult, GrammarError> {
    if cursor == keys.len() {
        Ok(ParseResult::Command(command))
    } else {
        Err(GrammarError::TrailingInput { index: cursor })
    }
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
        let digit = character.to_digit(10).ok_or(GrammarError::UnexpectedKey {
            index: *cursor,
            key: KeyCode::Char(character),
        })?;
        value = Some(
            value
                .unwrap_or(0)
                .checked_mul(10)
                .and_then(|current| current.checked_add(digit))
                .ok_or(GrammarError::CountOverflow { index: *cursor })?,
        );
        *cursor += 1;
    }
    Ok(value)
}

fn checked_key(keys: &[KeyEvent], index: usize) -> Result<KeyEvent, GrammarError> {
    let key = keys
        .get(index)
        .copied()
        .ok_or(GrammarError::TrailingInput { index })?;
    if key.modifiers.is_empty() {
        Ok(key)
    } else {
        Err(GrammarError::UnsupportedModifier { index })
    }
}

fn char_at(keys: &[KeyEvent], index: usize) -> Result<Option<char>, GrammarError> {
    Ok(match checked_key(keys, index)?.code {
        KeyCode::Char(character) => Some(character),
        _ => None,
    })
}

fn require_char(keys: &[KeyEvent], index: usize) -> Result<char, GrammarError> {
    char_at(keys, index)?.ok_or_else(|| GrammarError::UnexpectedKey {
        index,
        key: keys.get(index).map_or(KeyCode::Escape, |key| key.code),
    })
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
        _ => Err(GrammarError::InvalidRegister {
            index,
            register: character,
        }),
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
        _ => Err(GrammarError::UnexpectedKey {
            index,
            key: KeyCode::Char(character),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Vec<KeyEvent> {
        text.chars().map(KeyEvent::character).collect()
    }

    #[test]
    fn exposes_operator_pending_state() {
        assert_eq!(
            Grammar.parse(&keys("2\"ad")),
            ParseResult::Pending(ParseState::OperatorPending {
                operator: Operator::Delete,
                count: nonzero(2),
                register: Some(Register::Named('a')),
            })
        );
    }

    #[test]
    fn parses_count_register_operator_and_motion() {
        assert_eq!(
            Grammar.parse(&keys("2\"ad3w")),
            ParseResult::Command(Command::ApplyOperator {
                operator: Operator::Delete,
                motion: Motion::WordForward,
                count: nonzero(6),
                register: Some(Register::Named('a')),
                range_kind: RangeKind::CharacterWise,
            })
        );
    }

    #[test]
    fn parses_linewise_and_text_object_operators() {
        assert!(matches!(
            Grammar.parse(&keys("3dd")),
            ParseResult::Command(Command::ApplyOperator {
                motion: Motion::WholeLine,
                range_kind: RangeKind::LineWise,
                ..
            })
        ));
        assert!(matches!(
            Grammar.parse(&keys("ciw")),
            ParseResult::Command(Command::ApplyOperator {
                motion: Motion::Inside(TextObject::Word),
                ..
            })
        ));
    }

    #[test]
    fn parses_native_editing_commands() {
        assert!(matches!(
            Grammar.parse(&keys("gg")),
            ParseResult::Command(Command::Move {
                motion: Motion::GoToLine,
                ..
            })
        ));
        assert!(matches!(
            Grammar.parse(&keys("3x")),
            ParseResult::Command(Command::DeleteChar { count, .. }) if count.get() == 3
        ));
        assert_eq!(
            Grammar.parse(&keys("r界")),
            ParseResult::Command(Command::ReplaceChar {
                character: '界',
                count: NonZeroU32::MIN,
            })
        );
        assert!(matches!(
            Grammar.parse(&keys("A")),
            ParseResult::Command(Command::EnterInsertAtLineEnd)
        ));
    }

    #[test]
    fn parses_big_word_till_pair_paragraph_and_backward_end_motions() {
        assert!(matches!(
            Grammar.parse(&keys("W")),
            ParseResult::Command(Command::Move {
                motion: Motion::BigWordForward,
                ..
            })
        ));
        assert!(matches!(
            Grammar.parse(&keys("ge")),
            ParseResult::Command(Command::Move {
                motion: Motion::WordEndBackward,
                ..
            })
        ));
        assert!(matches!(
            Grammar.parse(&keys("dt,")),
            ParseResult::Command(Command::ApplyOperator {
                motion: Motion::TillForward(','),
                ..
            })
        ));
        assert!(matches!(
            Grammar.parse(&keys("%")),
            ParseResult::Command(Command::Move {
                motion: Motion::MatchPair,
                ..
            })
        ));
        assert!(matches!(
            Grammar.parse(&keys("}")),
            ParseResult::Command(Command::Move {
                motion: Motion::ParagraphForward,
                ..
            })
        ));
    }

    #[test]
    fn control_r_is_redo() {
        assert!(matches!(
            Grammar.parse(&[KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: Modifiers::CONTROL,
            }]),
            ParseResult::Command(Command::Redo { .. })
        ));
    }

    #[test]
    fn expression_register_is_typed_but_expression_input_stays_outside_key_grammar() {
        assert!(matches!(
            Grammar.parse(&keys("\"=")),
            ParseResult::Pending(ParseState::Register { .. })
        ));
        assert!(matches!(
            Grammar.parse(&keys("\"=p")),
            ParseResult::Command(Command::Paste {
                register: Some(Register::Expression),
                ..
            })
        ));
    }
}
