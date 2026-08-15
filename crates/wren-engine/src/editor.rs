use std::collections::BTreeMap;
use std::ops::Range;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use wren_grammar::{
    Command, Grammar, KeyCode, KeyEvent, Modifiers, Motion, Operator, ParseResult, ParseState,
    RangeKind, Register, TextObject,
};
use wren_text::TextStore;
use wren_types::{
    Anchor, Bias, DocumentRevision, Edit, SelRange, SelectionSet, Transaction, TransactionError,
};

use crate::EngineFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Replace,
    Visual,
    VisualLine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InsertStyle {
    Insert,
    Append,
    LineStart,
    LineEnd,
    OpenAbove,
    OpenBelow,
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterValue {
    pub text: Box<str>,
    pub linewise: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualSelection {
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub linewise: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoGroup {
    pub forward: Vec<Transaction>,
    pub inverse: Vec<Transaction>,
    before: SelectionSet,
    after: SelectionSet,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DurableUndoState {
    pub undo: Vec<UndoGroup>,
    pub redo: Vec<UndoGroup>,
    pub branches: Vec<Vec<UndoGroup>>,
}

impl UndoGroup {
    fn new(before: SelectionSet) -> Self {
        Self {
            forward: Vec::new(),
            inverse: Vec::new(),
            before: before.clone(),
            after: before,
        }
    }

    fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchState {
    pattern: Box<str>,
    direction: SearchDirection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum RepeatAction {
    Command(Command),
    Insert { style: InsertStyle, text: Box<str> },
    ChangeInsert { command: Command, text: Box<str> },
    NumberDelta(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingControl {
    RecordMacro,
    ReplayMacro,
    SetMark,
    JumpMark { linewise: bool },
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Transaction(#[from] TransactionError),
    #[error("transaction targets revision {actual}, current revision is {expected}")]
    RevisionMismatch { expected: u64, actual: u64 },
    #[error("document revision overflow")]
    RevisionOverflow,
    #[error("grammar rejected the pending key sequence")]
    InvalidGrammar,
    #[error("macro recursion limit exceeded")]
    MacroRecursion,
    #[error("document is read-only")]
    ReadOnly,
    #[error("durable repeat data is invalid: {0}")]
    InvalidRepeatData(Box<str>),
    #[error("durable undo state is invalid: {0}")]
    InvalidUndoState(Box<str>),
}

#[derive(Debug, Clone)]
pub struct Editor<T: TextStore> {
    text: T,
    revision: DocumentRevision,
    selections: SelectionSet,
    mode: Mode,
    pending_keys: Vec<KeyEvent>,
    parse_state: Option<ParseState>,
    undo: Vec<UndoGroup>,
    redo: Vec<UndoGroup>,
    undo_branches: Vec<Vec<UndoGroup>>,
    insert_group: Option<UndoGroup>,
    insert_style: InsertStyle,
    insert_capture: String,
    registers: BTreeMap<char, RegisterValue>,
    marks: BTreeMap<char, Anchor>,
    macros: BTreeMap<char, Vec<KeyEvent>>,
    recording_macro: Option<char>,
    recording_keys: Vec<KeyEvent>,
    last_macro: Option<char>,
    pending_control: Option<PendingControl>,
    macro_depth: u8,
    last_change: Option<RepeatAction>,
    pending_change: Option<Command>,
    replaying_change: bool,
    search: Option<SearchState>,
    last_visual: Option<VisualSelection>,
    jumplist: Vec<usize>,
    jump_index: usize,
    changelist: Vec<usize>,
    change_index: usize,
    last_find: Option<Motion>,
    messages: Vec<Box<str>>,
    dirty: bool,
    read_only: bool,
    ignore_case: bool,
    smart_case: bool,
    clipboard_unnamed: bool,
    expand_tab: bool,
    tab_stop: usize,
    shift_width: usize,
    smart_indent: bool,
    expand_region_keys: bool,
    visual_region_history: Vec<SelectionSet>,
}

impl<T: TextStore> Editor<T> {
    #[must_use]
    pub fn new(text: T) -> Self {
        Self {
            text,
            revision: DocumentRevision::new(0),
            selections: SelectionSet {
                primary: 0,
                ranges: vec![SelRange { anchor: 0, head: 0 }],
            },
            mode: Mode::Normal,
            pending_keys: Vec::new(),
            parse_state: None,
            undo: Vec::new(),
            redo: Vec::new(),
            undo_branches: Vec::new(),
            insert_group: None,
            insert_style: InsertStyle::Insert,
            insert_capture: String::new(),
            registers: BTreeMap::new(),
            marks: BTreeMap::new(),
            macros: BTreeMap::new(),
            recording_macro: None,
            recording_keys: Vec::new(),
            last_macro: None,
            pending_control: None,
            macro_depth: 0,
            last_change: None,
            pending_change: None,
            replaying_change: false,
            search: None,
            last_visual: None,
            jumplist: Vec::new(),
            jump_index: 0,
            changelist: vec![0],
            change_index: 1,
            last_find: None,
            messages: Vec::new(),
            dirty: false,
            read_only: false,
            ignore_case: false,
            smart_case: false,
            clipboard_unnamed: false,
            expand_tab: false,
            tab_stop: 4,
            shift_width: 4,
            smart_indent: false,
            expand_region_keys: false,
            visual_region_history: Vec::new(),
        }
    }

    pub const fn set_search_options(&mut self, ignore_case: bool, smart_case: bool) {
        self.ignore_case = ignore_case;
        self.smart_case = smart_case;
    }

    pub const fn set_clipboard_unnamed(&mut self, enabled: bool) {
        self.clipboard_unnamed = enabled;
    }

    pub const fn set_indent_options(
        &mut self,
        expand_tab: bool,
        tab_stop: usize,
        shift_width: usize,
        smart_indent: bool,
    ) {
        self.expand_tab = expand_tab;
        self.tab_stop = if tab_stop == 0 { 1 } else { tab_stop };
        self.shift_width = if shift_width == 0 { 1 } else { shift_width };
        self.smart_indent = smart_indent;
    }

    pub const fn set_expand_region_keys(&mut self, enabled: bool) {
        self.expand_region_keys = enabled;
    }

    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }

    #[must_use]
    pub const fn revision(&self) -> DocumentRevision {
        self.revision
    }

    #[must_use]
    pub fn selections(&self) -> &SelectionSet {
        &self.selections
    }

    #[must_use]
    pub fn pending_parse_state(&self) -> Option<&ParseState> {
        self.parse_state.as_ref()
    }

    #[must_use]
    pub fn text(&self) -> &T {
        &self.text
    }

    #[must_use]
    pub fn contents(&self) -> String {
        self.text.slice(0..self.text.len_bytes()).into_owned()
    }

    #[must_use]
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
            + usize::from(
                self.insert_group
                    .as_ref()
                    .is_some_and(|group| !group.is_empty()),
            )
    }

    #[must_use]
    pub fn redo_depth(&self) -> usize {
        self.redo.len()
    }

    #[must_use]
    pub fn undo_tree_len(&self) -> usize {
        self.undo.len() + self.redo.len() + self.undo_branches.iter().map(Vec::len).sum::<usize>()
    }

    pub fn durable_undo_state(&mut self) -> DurableUndoState {
        self.finish_insert_group();
        DurableUndoState {
            undo: self.undo.clone(),
            redo: self.redo.clone(),
            branches: self.undo_branches.clone(),
        }
    }

    pub fn restore_undo_state(&mut self, state: DurableUndoState) -> Result<(), EngineError> {
        for group in state
            .undo
            .iter()
            .chain(state.redo.iter())
            .chain(state.branches.iter().flatten())
        {
            group
                .before
                .validate()
                .map_err(|error| EngineError::InvalidUndoState(error.to_string().into()))?;
            group
                .after
                .validate()
                .map_err(|error| EngineError::InvalidUndoState(error.to_string().into()))?;
        }
        self.undo = state.undo;
        self.redo = state.redo;
        self.undo_branches = state.branches;
        self.insert_group = None;
        Ok(())
    }

    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
        if read_only && matches!(self.mode, Mode::Insert | Mode::Replace) {
            self.leave_insert();
        }
    }

    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn cancel_pending(&mut self) {
        self.pending_keys.clear();
        self.parse_state = None;
        self.pending_control = None;
    }

    #[must_use]
    pub fn primary_cursor(&self) -> usize {
        self.selections
            .ranges
            .get(self.selections.primary)
            .map_or(0, |range| range.head)
    }

    #[must_use]
    pub fn cursor_line_column(&self) -> (usize, usize) {
        let text = self.contents();
        let cursor = self.primary_cursor().min(text.len());
        let start = line_start(&text, cursor);
        (
            self.text.line_of_byte(cursor),
            text[start..cursor].chars().count(),
        )
    }

    #[must_use]
    pub fn frame(&self) -> EngineFrame {
        EngineFrame {
            text: self.contents().into_boxed_str(),
            cursor_byte: self.primary_cursor(),
        }
    }

    #[must_use]
    pub fn register(&self, name: char) -> Option<&RegisterValue> {
        self.registers.get(&name.to_ascii_lowercase())
    }

    pub fn registers(&self) -> impl Iterator<Item = (char, &RegisterValue)> {
        self.registers.iter().map(|(name, value)| (*name, value))
    }

    pub fn set_register(&mut self, name: char, text: impl Into<Box<str>>, linewise: bool) {
        let value = RegisterValue {
            text: text.into(),
            linewise,
        };
        self.registers.insert(name, value.clone());
        self.registers.insert('"', value);
    }

    /// Restores one durable register without applying interactive unnamed-
    /// register side effects. Client-state replay uses this for exact startup.
    pub fn restore_register(&mut self, name: char, text: impl Into<Box<str>>, linewise: bool) {
        self.registers.insert(
            name,
            RegisterValue {
                text: text.into(),
                linewise,
            },
        );
    }

    pub fn macros(&self) -> impl Iterator<Item = (char, &[KeyEvent])> + '_ {
        self.macros
            .iter()
            .map(|(name, keys)| (*name, keys.as_slice()))
    }

    /// Restores raw physical-key macro input. The synchronous grammar remains
    /// closed: restored keys are replayed through the same parser as live keys.
    pub fn restore_macro(&mut self, name: char, keys: Vec<KeyEvent>) {
        self.macros.insert(name.to_ascii_lowercase(), keys);
    }

    #[must_use]
    pub fn durable_repeat_data(&self) -> Option<Vec<u8>> {
        self.last_change
            .as_ref()
            .and_then(|action| serde_json::to_vec(action).ok())
    }

    pub fn restore_repeat_data(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        self.last_change = Some(
            serde_json::from_slice(bytes)
                .map_err(|error| EngineError::InvalidRepeatData(error.to_string().into()))?,
        );
        Ok(())
    }

    pub fn restore_search_pattern(&mut self, pattern: impl Into<Box<str>>) {
        self.search = Some(SearchState {
            pattern: pattern.into(),
            direction: SearchDirection::Forward,
        });
    }

    pub fn restore_mark(&mut self, name: char, byte: usize) {
        let byte = byte.min(self.text.len_bytes());
        self.marks.insert(
            name,
            Anchor {
                byte,
                bias: Bias::Right,
            },
        );
    }

    #[must_use]
    pub const fn last_visual_selection(&self) -> Option<VisualSelection> {
        self.last_visual
    }

    pub fn jumplist(&self) -> impl Iterator<Item = usize> + '_ {
        self.jumplist.iter().copied()
    }

    #[must_use]
    pub const fn jump_index(&self) -> usize {
        self.jump_index
    }

    pub fn changelist(&self) -> impl Iterator<Item = usize> + '_ {
        self.changelist.iter().copied()
    }

    #[must_use]
    pub const fn change_index(&self) -> usize {
        self.change_index
    }

    pub fn navigate_jump(&mut self, backward: bool) -> bool {
        if backward && self.jump_index == self.jumplist.len() {
            self.push_jump(self.primary_cursor());
            self.jump_index = self.jumplist.len().saturating_sub(1);
        }
        let next = if backward {
            self.jump_index.checked_sub(1)
        } else {
            self.jump_index
                .checked_add(1)
                .filter(|index| *index < self.jumplist.len())
        };
        let Some(next) = next else {
            return false;
        };
        self.jump_index = next;
        if let Some(byte) = self.jumplist.get(next).copied() {
            self.set_cursor(byte);
            true
        } else {
            false
        }
    }

    pub fn navigate_change(&mut self, backward: bool) -> bool {
        let next = if backward {
            self.change_index.checked_sub(1)
        } else {
            self.change_index
                .checked_add(1)
                .filter(|index| *index < self.changelist.len())
        };
        let Some(next) = next else {
            return false;
        };
        self.change_index = next;
        if let Some(byte) = self.changelist.get(next).copied() {
            self.set_cursor(byte);
            true
        } else {
            false
        }
    }

    pub fn repeat_find(&mut self, reverse: bool, count: u32) -> bool {
        let Some(motion) = self.last_find else {
            return false;
        };
        let motion = if reverse {
            match motion {
                Motion::FindForward(character) => Motion::FindBackward(character),
                Motion::FindBackward(character) => Motion::FindForward(character),
                Motion::TillForward(character) => Motion::TillBackward(character),
                Motion::TillBackward(character) => Motion::TillForward(character),
                _ => return false,
            }
        } else {
            motion
        };
        let before = self.primary_cursor();
        self.move_cursor(motion, count.max(1));
        self.primary_cursor() != before
    }

    pub fn adjust_number(&mut self, delta: i64) -> Result<Option<Transaction>, EngineError> {
        self.ensure_writable()?;
        let text = self.contents();
        let cursor = self.primary_cursor().min(text.len());
        let line_start = line_start(&text, cursor);
        let line_end = line_end(&text, cursor);
        let mut start =
            if cursor < line_end && text.as_bytes().get(cursor).is_some_and(u8::is_ascii_digit) {
                cursor
            } else {
                let Some(relative) =
                    text[cursor..line_end].find(|character: char| character.is_ascii_digit())
                else {
                    return Ok(None);
                };
                cursor + relative
            };
        while start > line_start
            && text
                .as_bytes()
                .get(start - 1)
                .is_some_and(u8::is_ascii_digit)
        {
            start -= 1;
        }
        if start > line_start && text.as_bytes().get(start - 1) == Some(&b'-') {
            start -= 1;
        }
        let digit_start = start + usize::from(text.as_bytes().get(start) == Some(&b'-'));
        let mut end = digit_start;
        while end < line_end && text.as_bytes().get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        let original = &text[start..end];
        let Ok(value) = original.parse::<i128>() else {
            return Ok(None);
        };
        let changed = value.saturating_add(i128::from(delta));
        let digit_width = end.saturating_sub(digit_start);
        let replacement = if original
            .strip_prefix('-')
            .unwrap_or(original)
            .starts_with('0')
            && changed >= 0
        {
            format!("{changed:0digit_width$}")
        } else {
            changed.to_string()
        };
        let transaction =
            Transaction::new(self.revision, vec![Edit::new(start..end, replacement)])?;
        self.apply_recorded(transaction.clone(), false)?;
        self.collapse_selection(start);
        if let Some(group) = self.undo.last_mut() {
            group.after = self.selections.clone();
        }
        if !self.replaying_change {
            self.last_change = Some(RepeatAction::NumberDelta(delta));
        }
        Ok(Some(transaction))
    }

    #[must_use]
    pub fn last_search(&self) -> Option<(&str, SearchDirection)> {
        self.search
            .as_ref()
            .map(|search| (search.pattern.as_ref(), search.direction))
    }

    pub fn messages(&self) -> impl Iterator<Item = &str> {
        self.messages.iter().map(AsRef::as_ref)
    }

    #[must_use]
    pub fn mark(&self, name: char) -> Option<usize> {
        self.marks.get(&name).map(|anchor| anchor.byte)
    }

    pub fn marks(&self) -> impl Iterator<Item = (char, usize)> + '_ {
        self.marks.iter().map(|(name, anchor)| (*name, anchor.byte))
    }

    pub fn set_cursor(&mut self, byte: usize) {
        let text = self.contents();
        self.collapse_selection(floor_char_boundary(&text, byte.min(text.len())));
    }

    pub fn set_selection_range(&mut self, range: Range<usize>) {
        let text = self.contents();
        let start = floor_char_boundary(&text, range.start.min(text.len()));
        let end = floor_char_boundary(&text, range.end.min(text.len()).max(start));
        if let Some(primary) = self.selections.ranges.get_mut(self.selections.primary) {
            primary.anchor = start;
            primary.head = end;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<Option<Transaction>, EngineError> {
        if matches!(self.mode, Mode::Insert | Mode::Replace) {
            if self.recording_macro.is_some() {
                self.recording_keys.push(key);
            }
            return self.handle_insert_key(key);
        }
        if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            return self.handle_visual_key(key);
        }

        if key.code == KeyCode::Escape && key.modifiers.is_empty() {
            self.cancel_pending();
            return Ok(None);
        }

        if self.pending_keys.is_empty() && key.modifiers.is_empty() {
            match key.code {
                KeyCode::Char('v') => {
                    self.enter_visual(Mode::Visual);
                    return Ok(None);
                }
                KeyCode::Char('V') => {
                    self.enter_visual(Mode::VisualLine);
                    return Ok(None);
                }
                _ => {}
            }
        }

        if let Some(control) = self.pending_control.take() {
            return self.finish_pending_control(control, key);
        }

        if self.recording_macro.is_some()
            && key == KeyEvent::character('q')
            && self.pending_keys.is_empty()
        {
            self.finish_macro_recording();
            return Ok(None);
        }
        if self.recording_macro.is_some() {
            self.recording_keys.push(key);
        }

        if self.pending_keys.is_empty() && key.modifiers.is_empty() {
            match key.code {
                KeyCode::Char('q') => {
                    self.pending_control = Some(PendingControl::RecordMacro);
                    return Ok(None);
                }
                KeyCode::Char('@') => {
                    self.pending_control = Some(PendingControl::ReplayMacro);
                    return Ok(None);
                }
                KeyCode::Char('m') => {
                    self.pending_control = Some(PendingControl::SetMark);
                    return Ok(None);
                }
                KeyCode::Char('\'') => {
                    self.pending_control = Some(PendingControl::JumpMark { linewise: true });
                    return Ok(None);
                }
                KeyCode::Char('`') => {
                    self.pending_control = Some(PendingControl::JumpMark { linewise: false });
                    return Ok(None);
                }
                _ => {}
            }
        }

        self.pending_keys.push(key);
        match Grammar.parse(&self.pending_keys) {
            ParseResult::Pending(state) => {
                self.parse_state = Some(state);
                Ok(None)
            }
            ParseResult::Invalid(_) => {
                self.pending_keys.clear();
                self.parse_state = None;
                Err(EngineError::InvalidGrammar)
            }
            ParseResult::Command(command) => {
                self.pending_keys.clear();
                self.parse_state = None;
                self.execute(command)
            }
        }
    }

    fn enter_visual(&mut self, mode: Mode) {
        self.cancel_pending();
        self.visual_region_history.clear();
        self.mode = mode;
        let cursor = self.primary_cursor();
        if let Some(primary) = self.selections.ranges.get_mut(self.selections.primary) {
            primary.anchor = cursor;
            primary.head = cursor;
        }
    }

    fn leave_visual(&mut self, cursor: usize) {
        let text = self.contents();
        if let Some(selection) = self.selections.ranges.get(self.selections.primary) {
            let start = selection.anchor.min(selection.head);
            let end = selection.anchor.max(selection.head);
            let (start_line, start_column) = line_column_at(&text, start);
            let (end_line, end_column) = line_column_at(&text, end);
            self.last_visual = Some(VisualSelection {
                start_line,
                start_column,
                end_line,
                end_column: if self.mode == Mode::VisualLine {
                    usize::MAX
                } else {
                    end_column
                },
                linewise: self.mode == Mode::VisualLine,
            });
        }
        self.mode = Mode::Normal;
        self.visual_region_history.clear();
        self.cancel_pending();
        self.collapse_selection(cursor);
    }

    fn handle_visual_key(&mut self, key: KeyEvent) -> Result<Option<Transaction>, EngineError> {
        if self.expand_region_keys
            && key.code == KeyCode::Char('v')
            && key.modifiers == Modifiers::CONTROL
        {
            if let Some(selection) = self.visual_region_history.pop() {
                self.selections = selection;
            }
            return Ok(None);
        }
        if key.modifiers.is_empty() && self.pending_keys.is_empty() {
            match key.code {
                KeyCode::Escape => {
                    self.leave_visual(self.primary_cursor());
                    return Ok(None);
                }
                KeyCode::Char('v') => {
                    if self.expand_region_keys {
                        self.expand_visual_region();
                    } else if self.mode == Mode::Visual {
                        self.leave_visual(self.primary_cursor());
                    } else {
                        self.mode = Mode::Visual;
                    }
                    return Ok(None);
                }
                KeyCode::Char('V') => {
                    if self.mode == Mode::VisualLine {
                        self.leave_visual(self.primary_cursor());
                    } else {
                        self.mode = Mode::VisualLine;
                    }
                    return Ok(None);
                }
                KeyCode::Char('o') => {
                    if let Some(primary) = self.selections.ranges.get_mut(self.selections.primary) {
                        std::mem::swap(&mut primary.anchor, &mut primary.head);
                    }
                    return Ok(None);
                }
                KeyCode::Char('d' | 'x') | KeyCode::Delete => {
                    return self.apply_visual_operator(Operator::Delete, None);
                }
                KeyCode::Char('c') => {
                    return self.apply_visual_operator(Operator::Change, None);
                }
                KeyCode::Char('y' | 'Y') => {
                    return self.apply_visual_operator(Operator::Yank, None);
                }
                KeyCode::Char('>' | '<') => {
                    return self.apply_visual_operator(
                        if key.code == KeyCode::Char('>') {
                            Operator::Indent
                        } else {
                            Operator::Outdent
                        },
                        None,
                    );
                }
                KeyCode::Char('p' | 'P') => {
                    return self.visual_paste(key.code == KeyCode::Char('P'), None);
                }
                _ => {}
            }
        }

        self.pending_keys.push(key);
        match Grammar.parse(&self.pending_keys) {
            ParseResult::Pending(state) => {
                self.parse_state = Some(state);
                Ok(None)
            }
            ParseResult::Invalid(_) => {
                self.cancel_pending();
                Err(EngineError::InvalidGrammar)
            }
            ParseResult::Command(Command::Move { motion, count }) => {
                self.cancel_pending();
                self.move_visual_head(motion, count.get());
                Ok(None)
            }
            ParseResult::Command(_) => {
                self.cancel_pending();
                Err(EngineError::InvalidGrammar)
            }
        }
    }

    fn expand_visual_region(&mut self) {
        let text = self.contents();
        if text.is_empty() {
            return;
        }
        let current = self.selection_byte_range();
        let cursor = current.start.min(text.len().saturating_sub(1));
        let selected_end = current.end.max(current.start);
        let first_line = line_start(&text, current.start);
        let last_byte = previous_char_boundary(&text, selected_end.min(text.len()));
        let last_line_end = line_end_with_newline(&text, last_byte);
        let mut candidates = vec![
            word_object_range(&text, cursor, false),
            first_line..last_line_end,
            0..text.len(),
        ];
        candidates.extend(
            ['(', '[', '{', '<']
                .into_iter()
                .map(|open| bracket_object_range(&text, cursor, open, true)),
        );
        let next = candidates
            .into_iter()
            .filter(|candidate| {
                candidate.start <= current.start
                    && candidate.end >= selected_end
                    && (candidate.start < current.start || candidate.end > selected_end)
            })
            .min_by_key(|candidate| candidate.end.saturating_sub(candidate.start));
        let Some(next) = next else {
            return;
        };
        self.visual_region_history.push(self.selections.clone());
        self.mode = Mode::Visual;
        if let Some(primary) = self.selections.ranges.get_mut(self.selections.primary) {
            primary.anchor = next.start;
            primary.head = previous_char_boundary(&text, next.end);
        }
    }

    fn move_visual_head(&mut self, motion: Motion, count: u32) {
        let text = self.contents();
        let destination = normal_cursor_destination(
            &text,
            self.motion_destination(self.primary_cursor(), motion, count),
        );
        if let Some(primary) = self.selections.ranges.get_mut(self.selections.primary) {
            primary.head = destination;
        }
    }

    #[must_use]
    pub fn selection_byte_range(&self) -> Range<usize> {
        let text = self.contents();
        let Some(selection) = self.selections.ranges.get(self.selections.primary) else {
            return 0..0;
        };
        if self.mode == Mode::VisualLine {
            line_start(&text, selection.anchor.min(selection.head))
                ..line_end_with_newline(&text, selection.anchor.max(selection.head))
        } else if self.mode == Mode::Visual {
            let start = selection.anchor.min(selection.head);
            let end = next_char_boundary(&text, selection.anchor.max(selection.head));
            start..end
        } else {
            selection.head..selection.head
        }
    }

    fn apply_visual_operator(
        &mut self,
        operator: Operator,
        register: Option<Register>,
    ) -> Result<Option<Transaction>, EngineError> {
        let text = self.contents();
        let range = self.selection_byte_range();
        if range.is_empty() {
            self.leave_visual(range.start);
            return Ok(None);
        }
        let linewise = self.mode == Mode::VisualLine;
        if matches!(operator, Operator::Delete | Operator::Change) {
            self.ensure_writable()?;
            self.write_delete_register(register, &text[range.clone()], linewise);
        } else if operator == Operator::Yank {
            self.write_yank_register(register, &text[range.clone()], linewise);
            self.leave_visual(range.start);
            return Ok(None);
        }
        let edit_range = if operator == Operator::Change
            && linewise
            && text.as_bytes().get(range.end.saturating_sub(1)) == Some(&b'\n')
        {
            range.start..range.end.saturating_sub(1)
        } else {
            range.clone()
        };
        let edits = match operator {
            Operator::Indent => line_edits(&text, edit_range, true, self.shift_width),
            Operator::Outdent => line_edits(&text, edit_range, false, self.shift_width),
            Operator::Delete | Operator::Change => vec![Edit::new(edit_range, "")],
            Operator::Yank => Vec::new(),
        };
        self.leave_visual(range.start);
        if edits.is_empty() {
            return Ok(None);
        }
        if operator == Operator::Change {
            self.enter_insert(InsertStyle::Insert);
        }
        let transaction = Transaction::new(self.revision, edits)?;
        self.apply_recorded(transaction.clone(), operator == Operator::Change)?;
        Ok(Some(transaction))
    }

    fn visual_paste(
        &mut self,
        _before: bool,
        register: Option<Register>,
    ) -> Result<Option<Transaction>, EngineError> {
        self.ensure_writable()?;
        let Some(value) = self.read_register(register).cloned() else {
            return Ok(None);
        };
        let text = self.contents();
        let range = self.selection_byte_range();
        self.write_delete_register(None, &text[range.clone()], self.mode == Mode::VisualLine);
        self.leave_visual(range.start);
        let transaction =
            Transaction::new(self.revision, vec![Edit::new(range.clone(), value.text)])?;
        self.apply_recorded(transaction.clone(), false)?;
        self.collapse_selection(range.start);
        Ok(Some(transaction))
    }

    pub fn insert_text(&mut self, text: &str) -> Result<Option<Transaction>, EngineError> {
        if !matches!(self.mode, Mode::Insert | Mode::Replace) || text.is_empty() {
            return Ok(None);
        }
        if self.mode == Mode::Replace {
            return self.replace_insert_text(text);
        }
        let cursor = self.primary_cursor();
        let transaction = Transaction::new(
            self.revision,
            vec![Edit::new(cursor..cursor, text.to_owned())],
        )?;
        self.insert_capture.push_str(text);
        self.apply_recorded(transaction.clone(), true)?;
        Ok(Some(transaction))
    }

    pub fn apply_transaction(&mut self, transaction: Transaction) -> Result<(), EngineError> {
        self.finish_insert_group();
        self.apply_recorded(transaction, false)
    }

    pub fn undo(&mut self) -> Result<Option<Transaction>, EngineError> {
        self.finish_insert_group();
        let Some(group) = self.undo.pop() else {
            self.messages.push("already at oldest change".into());
            return Ok(None);
        };
        let mut last = None;
        for stored in group.inverse.iter().rev() {
            let mut inverse = stored.clone();
            inverse.base_revision = self.revision;
            self.apply_without_history(&inverse)?;
            last = Some(inverse);
        }
        self.selections = group.before.clone();
        self.redo.push(group);
        self.messages.push("undo change".into());
        self.dirty = true;
        Ok(last)
    }

    pub fn redo(&mut self) -> Result<Option<Transaction>, EngineError> {
        self.finish_insert_group();
        let Some(group) = self.redo.pop() else {
            self.messages.push("already at newest change".into());
            return Ok(None);
        };
        let mut last = None;
        for stored in &group.forward {
            let mut forward = stored.clone();
            forward.base_revision = self.revision;
            self.apply_without_history(&forward)?;
            last = Some(forward);
        }
        self.undo.push(group);
        self.messages.push("redo change".into());
        self.dirty = true;
        Ok(last)
    }

    pub fn search(&mut self, pattern: &str, direction: SearchDirection) -> bool {
        if pattern.is_empty() {
            return false;
        }
        self.search = Some(SearchState {
            pattern: Box::from(pattern),
            direction,
        });
        self.search_next(false)
    }

    pub fn search_next(&mut self, reverse: bool) -> bool {
        let Some(search) = self.search.clone() else {
            return false;
        };
        let direction = if reverse {
            match search.direction {
                SearchDirection::Forward => SearchDirection::Backward,
                SearchDirection::Backward => SearchDirection::Forward,
            }
        } else {
            search.direction
        };
        let text = self.contents();
        let cursor = self.primary_cursor().min(text.len());
        let case_insensitive = self.ignore_case
            && (!self.smart_case || !search.pattern.chars().any(char::is_uppercase));
        let found = match direction {
            SearchDirection::Forward => {
                let after = next_char_boundary(&text, cursor);
                find_literal(&text[after..], search.pattern.as_ref(), case_insensitive)
                    .map(|offset| after + offset)
                    .or_else(|| {
                        find_literal(&text[..after], search.pattern.as_ref(), case_insensitive)
                    })
            }
            SearchDirection::Backward => {
                rfind_literal(&text[..cursor], search.pattern.as_ref(), case_insensitive).or_else(
                    || {
                        rfind_literal(&text[cursor..], search.pattern.as_ref(), case_insensitive)
                            .map(|offset| cursor + offset)
                    },
                )
            }
        };
        if let Some(byte) = found {
            let previous = self.primary_cursor();
            if byte != previous {
                self.push_jump(previous);
            }
            self.collapse_selection(byte);
            true
        } else {
            false
        }
    }

    pub fn replace_literal(
        &mut self,
        needle: &str,
        replacement: &str,
        whole_document: bool,
        global: bool,
    ) -> Result<usize, EngineError> {
        if needle.is_empty() {
            return Ok(0);
        }
        let text = self.contents();
        let cursor = self.primary_cursor();
        let range = if whole_document {
            0..text.len()
        } else {
            line_start(&text, cursor)..line_end(&text, cursor)
        };
        let haystack = &text[range.clone()];
        let mut matches: Vec<_> = haystack
            .match_indices(needle)
            .map(|(offset, _)| offset)
            .collect();
        if !global {
            matches.truncate(1);
        }
        if matches.is_empty() {
            return Ok(0);
        }
        let edits = matches
            .iter()
            .map(|offset| {
                let start = range.start + offset;
                Edit::new(start..start + needle.len(), replacement.to_owned())
            })
            .collect();
        let count = matches.len();
        self.apply_recorded(Transaction::new(self.revision, edits)?, false)?;
        Ok(count)
    }

    fn handle_insert_key(&mut self, key: KeyEvent) -> Result<Option<Transaction>, EngineError> {
        match key.code {
            KeyCode::Escape => {
                self.leave_insert();
                Ok(None)
            }
            KeyCode::Char(character)
                if key.modifiers.is_empty()
                    && self.smart_indent
                    && matches!(character, '}' | ']' | ')') =>
            {
                self.insert_smart_closing_delimiter(character)
            }
            KeyCode::Char(character) if key.modifiers.is_empty() => {
                self.insert_text(&character.to_string())
            }
            KeyCode::Enter => {
                let text = self.contents();
                let cursor = self.primary_cursor();
                let start = line_start(&text, cursor);
                let mut indent: String = text[start..line_end(&text, start)]
                    .chars()
                    .take_while(|character| matches!(character, ' ' | '\t'))
                    .collect();
                if self.smart_indent && text[start..cursor].trim_end().ends_with(['{', '[', '(']) {
                    indent.push_str(&" ".repeat(self.shift_width));
                }
                self.insert_text(&format!("\n{indent}"))
            }
            KeyCode::Tab if self.expand_tab => {
                let column = self.cursor_line_column().1;
                let spaces = self.tab_stop - (column % self.tab_stop);
                self.insert_text(&" ".repeat(spaces))
            }
            KeyCode::Tab => self.insert_text("\t"),
            KeyCode::Backspace => self.delete_insert_backward(),
            KeyCode::Delete => self.delete_insert_forward(),
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End => {
                let motion = match key.code {
                    KeyCode::Left => Motion::Left,
                    KeyCode::Right => Motion::Right,
                    KeyCode::Up => Motion::Up,
                    KeyCode::Down => Motion::Down,
                    KeyCode::Home => Motion::LineStart,
                    _ => Motion::LineEnd,
                };
                self.move_cursor(motion, 1);
                Ok(None)
            }
            KeyCode::Char('w') if key.modifiers == Modifiers::CONTROL => {
                let cursor = self.primary_cursor();
                let start = self.motion_destination(cursor, Motion::WordBackward, 1);
                self.delete_insert_range(start, cursor)
            }
            KeyCode::Char('u') if key.modifiers == Modifiers::CONTROL => {
                let cursor = self.primary_cursor();
                let text = self.contents();
                self.delete_insert_range(line_start(&text, cursor), cursor)
            }
            _ => Ok(None),
        }
    }

    fn insert_smart_closing_delimiter(
        &mut self,
        character: char,
    ) -> Result<Option<Transaction>, EngineError> {
        let text = self.contents();
        let cursor = self.primary_cursor();
        let start = line_start(&text, cursor);
        let prefix = &text[start..cursor];
        if !prefix
            .chars()
            .all(|character| matches!(character, ' ' | '\t'))
        {
            return self.insert_text(&character.to_string());
        }
        let remove_start = if prefix.ends_with('\t') {
            previous_char_boundary(&text, cursor)
        } else {
            let spaces = prefix
                .as_bytes()
                .iter()
                .rev()
                .take(self.shift_width)
                .take_while(|byte| **byte == b' ')
                .count();
            cursor.saturating_sub(spaces)
        };
        let transaction = Transaction::new(
            self.revision,
            vec![Edit::new(remove_start..cursor, character.to_string())],
        )?;
        self.apply_recorded(transaction.clone(), true)?;
        self.insert_capture.push(character);
        Ok(Some(transaction))
    }

    fn execute(&mut self, command: Command) -> Result<Option<Transaction>, EngineError> {
        match command.clone() {
            Command::EnterInsert => {
                self.ensure_writable()?;
                self.enter_insert(InsertStyle::Insert);
                Ok(None)
            }
            Command::EnterAppend => {
                self.ensure_writable()?;
                let text = self.contents();
                let cursor = self.primary_cursor();
                if cursor < line_end(&text, cursor) {
                    self.collapse_selection(next_char_boundary(&text, cursor));
                }
                self.enter_insert(InsertStyle::Append);
                Ok(None)
            }
            Command::EnterInsertAtLineStart => {
                self.ensure_writable()?;
                let text = self.contents();
                self.collapse_selection(first_non_blank(&text, self.primary_cursor()));
                self.enter_insert(InsertStyle::LineStart);
                Ok(None)
            }
            Command::EnterInsertAtLineEnd => {
                self.ensure_writable()?;
                let text = self.contents();
                self.collapse_selection(line_end(&text, self.primary_cursor()));
                self.enter_insert(InsertStyle::LineEnd);
                Ok(None)
            }
            Command::EnterReplace => {
                self.ensure_writable()?;
                self.enter_insert(InsertStyle::Replace);
                self.mode = Mode::Replace;
                Ok(None)
            }
            Command::OpenLine { above } => {
                self.ensure_writable()?;
                self.open_line(above)
            }
            Command::Move { motion, count } => {
                let previous = self.primary_cursor();
                if matches!(
                    motion,
                    Motion::FindForward(_)
                        | Motion::FindBackward(_)
                        | Motion::TillForward(_)
                        | Motion::TillBackward(_)
                ) {
                    self.last_find = Some(motion);
                }
                self.move_cursor(motion, count.get());
                if matches!(
                    motion,
                    Motion::GoToLine
                        | Motion::DocumentEnd
                        | Motion::ParagraphForward
                        | Motion::ParagraphBackward
                ) && self.primary_cursor() != previous
                {
                    self.push_jump(previous);
                }
                Ok(None)
            }
            Command::ApplyOperator {
                operator,
                motion,
                count,
                register,
                range_kind,
            } => {
                let result =
                    self.apply_operator(operator, motion, count.get(), register, range_kind)?;
                if result.is_some() && operator != Operator::Yank && !self.replaying_change {
                    if operator == Operator::Change {
                        self.pending_change = Some(command);
                    } else {
                        self.last_change = Some(RepeatAction::Command(command));
                    }
                }
                Ok(result)
            }
            Command::DeleteChar {
                backward,
                count,
                register,
            } => {
                let result = self.delete_chars(backward, count.get(), register)?;
                self.remember_command_change(&command, result.is_some());
                Ok(result)
            }
            Command::JoinLines { count } => {
                let result = self.join_lines(count.get())?;
                self.remember_command_change(&command, result.is_some());
                Ok(result)
            }
            Command::ReplaceChar { character, count } => {
                let result = self.replace_chars(character, count.get())?;
                self.remember_command_change(&command, result.is_some());
                Ok(result)
            }
            Command::ToggleCase { count } => {
                let result = self.toggle_case(count.get())?;
                self.remember_command_change(&command, result.is_some());
                Ok(result)
            }
            Command::Paste {
                before,
                count,
                register,
            } => {
                let result = self.paste(before, count.get(), register)?;
                self.remember_command_change(&command, result.is_some());
                Ok(result)
            }
            Command::Undo { count } => self.undo_count(count.get()),
            Command::Redo { count } => self.redo_count(count.get()),
            Command::SearchNext { reverse, count } => {
                for _ in 0..count.get() {
                    if !self.search_next(reverse) {
                        break;
                    }
                }
                Ok(None)
            }
            Command::Repeat { count } => self.repeat_last(count.get()),
        }
    }

    fn remember_command_change(&mut self, command: &Command, changed: bool) {
        if changed && !self.replaying_change {
            self.last_change = Some(RepeatAction::Command(command.clone()));
        }
    }

    fn undo_count(&mut self, count: u32) -> Result<Option<Transaction>, EngineError> {
        let mut last = None;
        for _ in 0..count {
            if let Some(transaction) = self.undo()? {
                last = Some(transaction);
            } else {
                break;
            }
        }
        Ok(last)
    }

    fn redo_count(&mut self, count: u32) -> Result<Option<Transaction>, EngineError> {
        let mut last = None;
        for _ in 0..count {
            if let Some(transaction) = self.redo()? {
                last = Some(transaction);
            } else {
                break;
            }
        }
        Ok(last)
    }

    fn enter_insert(&mut self, style: InsertStyle) {
        self.mode = Mode::Insert;
        self.insert_style = style;
        self.insert_capture.clear();
        self.insert_group = Some(UndoGroup::new(self.selections.clone()));
    }

    fn leave_insert(&mut self) {
        self.mode = Mode::Normal;
        if !self.insert_capture.is_empty() {
            let text = self.contents();
            let cursor = self.primary_cursor();
            let destination = previous_char_boundary(&text, cursor).max(line_start(&text, cursor));
            self.collapse_selection(destination);
            if let Some(group) = &mut self.insert_group {
                group.after = self.selections.clone();
            }
        }
        self.finish_insert_group();
        if !self.replaying_change {
            if let Some(command) = self.pending_change.take() {
                self.last_change = Some(RepeatAction::ChangeInsert {
                    command,
                    text: self.insert_capture.clone().into_boxed_str(),
                });
            } else if !self.insert_capture.is_empty() {
                self.last_change = Some(RepeatAction::Insert {
                    style: self.insert_style,
                    text: self.insert_capture.clone().into_boxed_str(),
                });
            }
        }
        self.insert_capture.clear();
    }

    fn finish_insert_group(&mut self) {
        if let Some(group) = self.insert_group.take()
            && !group.is_empty()
        {
            self.undo.push(group);
        }
    }

    fn apply_recorded(
        &mut self,
        transaction: Transaction,
        insert_group: bool,
    ) -> Result<(), EngineError> {
        self.ensure_writable()?;
        if transaction.base_revision != self.revision {
            return Err(EngineError::RevisionMismatch {
                expected: self.revision.get(),
                actual: transaction.base_revision.get(),
            });
        }
        let inverse = transaction.inverted_against(&self.contents())?;
        let starts_change = if insert_group {
            self.insert_group.as_ref().is_none_or(UndoGroup::is_empty)
        } else {
            true
        };
        if starts_change && !transaction.edits.is_empty() {
            self.push_change(transaction.edits[0].range.start);
        }
        let before = self.selections.clone();
        self.apply_without_history(&transaction)?;
        let after = self.selections.clone();

        if insert_group {
            let group = self
                .insert_group
                .get_or_insert_with(|| UndoGroup::new(before.clone()));
            group.forward.push(transaction);
            group.inverse.push(inverse);
            group.after = after;
        } else {
            let mut group = UndoGroup::new(before);
            group.forward.push(transaction);
            group.inverse.push(inverse);
            group.after = after;
            self.undo.push(group);
        }
        if !self.redo.is_empty() {
            self.undo_branches.push(std::mem::take(&mut self.redo));
        }
        self.dirty = true;
        Ok(())
    }

    fn apply_without_history(&mut self, transaction: &Transaction) -> Result<(), EngineError> {
        if transaction.base_revision != self.revision {
            return Err(EngineError::RevisionMismatch {
                expected: self.revision.get(),
                actual: transaction.base_revision.get(),
            });
        }
        self.text.apply(transaction);
        self.selections = self.selections.map_through(transaction)?;
        for anchor in self.marks.values_mut() {
            *anchor = (*anchor).map_through(transaction)?;
        }
        for jump in &mut self.jumplist {
            *jump = transaction.map_offset(*jump, Bias::Left)?;
        }
        for change in &mut self.changelist {
            *change = transaction.map_offset(*change, Bias::Left)?;
        }
        self.revision = self.revision.next().ok_or(EngineError::RevisionOverflow)?;
        Ok(())
    }

    fn push_change(&mut self, byte: usize) {
        if self.changelist.last().copied() != Some(byte) {
            self.changelist.push(byte);
            if self.changelist.len() > 100 {
                self.changelist.remove(0);
            }
        }
        self.change_index = self.changelist.len();
    }

    fn delete_insert_backward(&mut self) -> Result<Option<Transaction>, EngineError> {
        if self.mode == Mode::Replace {
            return self.restore_replaced_character();
        }
        let cursor = self.primary_cursor();
        if cursor == 0 {
            return Ok(None);
        }
        let text = self.contents();
        let start = previous_char_boundary(&text, cursor);
        self.insert_capture.pop();
        self.delete_insert_range(start, cursor)
    }

    fn replace_insert_text(&mut self, inserted: &str) -> Result<Option<Transaction>, EngineError> {
        if inserted.is_empty() {
            return Ok(None);
        }
        let text = self.contents();
        let cursor = self.primary_cursor();
        let mut end = cursor;
        if !inserted.contains('\n') {
            for _ in inserted.chars() {
                if end >= text.len() {
                    break;
                }
                let next = next_char_boundary(&text, end);
                if text.as_bytes().get(end) == Some(&b'\n') {
                    break;
                }
                end = next;
            }
        }
        let transaction = Transaction::new(
            self.revision,
            vec![Edit::new(cursor..end, inserted.to_owned())],
        )?;
        self.insert_capture.push_str(inserted);
        self.apply_recorded(transaction.clone(), true)?;
        Ok(Some(transaction))
    }

    fn restore_replaced_character(&mut self) -> Result<Option<Transaction>, EngineError> {
        let cursor = self.primary_cursor();
        let Some((forward, stored_inverse)) = self.insert_group.as_ref().and_then(|group| {
            group
                .forward
                .last()
                .zip(group.inverse.last())
                .map(|(forward, inverse)| (forward.clone(), inverse.clone()))
        }) else {
            return Ok(None);
        };
        let Some(edit) = forward.edits.first() else {
            return Ok(None);
        };
        if edit.range.start.saturating_add(edit.insert.len()) != cursor {
            return Ok(None);
        }
        let destination = edit.range.start;
        let mut inverse = stored_inverse;
        inverse.base_revision = self.revision;
        self.apply_without_history(&inverse)?;
        if let Some(group) = &mut self.insert_group {
            group.forward.pop();
            group.inverse.pop();
            group.after = self.selections.clone();
        }
        self.insert_capture.pop();
        self.collapse_selection(destination);
        self.dirty = true;
        Ok(Some(inverse))
    }

    fn delete_insert_forward(&mut self) -> Result<Option<Transaction>, EngineError> {
        let cursor = self.primary_cursor();
        let text = self.contents();
        self.delete_insert_range(cursor, next_char_boundary(&text, cursor))
    }

    fn delete_insert_range(
        &mut self,
        start: usize,
        end: usize,
    ) -> Result<Option<Transaction>, EngineError> {
        if start == end {
            return Ok(None);
        }
        let transaction = Transaction::new(self.revision, vec![Edit::new(start..end, "")])?;
        self.apply_recorded(transaction.clone(), true)?;
        Ok(Some(transaction))
    }

    fn open_line(&mut self, above: bool) -> Result<Option<Transaction>, EngineError> {
        let text = self.contents();
        let cursor = self.primary_cursor();
        let start = line_start(&text, cursor);
        let end = line_end(&text, cursor);
        let mut indent: String = text[start..end]
            .chars()
            .take_while(|character| character.is_whitespace())
            .collect();
        if !above && self.smart_indent && text[start..end].trim_end().ends_with(['{', '[', '(']) {
            indent.push_str(&" ".repeat(self.shift_width));
        }
        let (position, inserted, destination, style) = if above {
            (start, format!("{indent}\n"), start, InsertStyle::OpenAbove)
        } else if end < text.len() {
            (
                end + 1,
                format!("{indent}\n"),
                end + 1,
                InsertStyle::OpenBelow,
            )
        } else {
            (end, format!("\n{indent}"), end + 1, InsertStyle::OpenBelow)
        };
        self.enter_insert(style);
        let transaction =
            Transaction::new(self.revision, vec![Edit::new(position..position, inserted)])?;
        self.apply_recorded(transaction.clone(), true)?;
        self.collapse_selection(destination);
        Ok(Some(transaction))
    }

    fn apply_operator(
        &mut self,
        operator: Operator,
        motion: Motion,
        count: u32,
        register: Option<Register>,
        range_kind: RangeKind,
    ) -> Result<Option<Transaction>, EngineError> {
        let text = self.contents();
        let mut range = self.operator_range(&text, motion, count, range_kind);
        if operator == Operator::Change
            && matches!(motion, Motion::WordForward | Motion::BigWordForward)
        {
            while range.end > range.start
                && text[..range.end]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace)
                && text.as_bytes().get(range.end - 1) != Some(&b'\n')
            {
                range.end = previous_char_boundary(&text, range.end);
            }
        }
        if range.is_empty() {
            return Ok(None);
        }
        let linewise = range_kind == RangeKind::LineWise || motion == Motion::WholeLine;
        if matches!(operator, Operator::Delete | Operator::Change) {
            self.write_delete_register(register, &text[range.clone()], linewise);
        } else if operator == Operator::Yank {
            self.write_yank_register(register, &text[range.clone()], linewise);
        }
        if operator == Operator::Yank {
            return Ok(None);
        }
        let edit_range = if operator == Operator::Change
            && linewise
            && range.end > range.start
            && text.as_bytes().get(range.end - 1) == Some(&b'\n')
        {
            range.start..range.end - 1
        } else {
            range
        };
        let edits = match operator {
            Operator::Indent => line_edits(&text, edit_range, true, self.shift_width),
            Operator::Outdent => line_edits(&text, edit_range, false, self.shift_width),
            Operator::Delete | Operator::Change => vec![Edit::new(edit_range, "")],
            Operator::Yank => Vec::new(),
        };
        if edits.is_empty() {
            return Ok(None);
        }
        if operator == Operator::Change {
            self.enter_insert(InsertStyle::Insert);
        }
        let transaction = Transaction::new(self.revision, edits)?;
        self.apply_recorded(transaction.clone(), operator == Operator::Change)?;
        Ok(Some(transaction))
    }

    fn operator_range(
        &self,
        text: &str,
        motion: Motion,
        count: u32,
        range_kind: RangeKind,
    ) -> Range<usize> {
        let cursor = self.primary_cursor().min(text.len());
        if motion == Motion::WholeLine {
            return whole_line_range(text, cursor, count);
        }
        if let Motion::Inside(object) | Motion::Around(object) = motion {
            return text_object_range(text, cursor, object, matches!(motion, Motion::Around(_)));
        }
        let destination = self.motion_destination(cursor, motion, count);
        if range_kind == RangeKind::LineWise {
            let first = line_start(text, cursor.min(destination));
            let last = line_end_with_newline(text, cursor.max(destination));
            return first..last;
        }
        let start = cursor.min(destination);
        let mut end = cursor.max(destination);
        if matches!(
            motion,
            Motion::WordEnd | Motion::BigWordEnd | Motion::FindForward(_) | Motion::TillForward(_)
        ) && destination >= cursor
        {
            end = next_char_boundary(text, end);
        }
        start..end
    }

    fn delete_chars(
        &mut self,
        backward: bool,
        count: u32,
        register: Option<Register>,
    ) -> Result<Option<Transaction>, EngineError> {
        let text = self.contents();
        let cursor = self.primary_cursor();
        let mut edge = cursor;
        for _ in 0..count {
            edge = if backward {
                previous_char_boundary(&text, edge)
            } else {
                let next = next_char_boundary(&text, edge);
                if text[edge..next].contains('\n') {
                    edge
                } else {
                    next
                }
            };
        }
        let range = edge.min(cursor)..edge.max(cursor);
        if range.is_empty() {
            return Ok(None);
        }
        self.write_delete_register(register, &text[range.clone()], false);
        let transaction = Transaction::new(self.revision, vec![Edit::new(range, "")])?;
        self.apply_recorded(transaction.clone(), false)?;
        Ok(Some(transaction))
    }

    fn join_lines(&mut self, count: u32) -> Result<Option<Transaction>, EngineError> {
        let text = self.contents();
        let mut position = line_end(&text, self.primary_cursor());
        let mut edits = Vec::new();
        for _ in 0..count.max(1) {
            if position >= text.len() || text.as_bytes().get(position) != Some(&b'\n') {
                break;
            }
            let mut end = position + 1;
            while end < text.len() && matches!(text.as_bytes()[end], b' ' | b'\t') {
                end += 1;
            }
            edits.push(Edit::new(position..end, " "));
            position = line_end(&text, end);
        }
        if edits.is_empty() {
            return Ok(None);
        }
        let cursor = edits.first().map_or(0, |edit| edit.range.start);
        let transaction = Transaction::new(self.revision, edits)?;
        self.apply_recorded(transaction.clone(), false)?;
        self.collapse_selection(cursor);
        if let Some(group) = self.undo.last_mut() {
            group.after = self.selections.clone();
        }
        Ok(Some(transaction))
    }

    fn replace_chars(
        &mut self,
        character: char,
        count: u32,
    ) -> Result<Option<Transaction>, EngineError> {
        let text = self.contents();
        let start = self.primary_cursor();
        let mut end = start;
        let mut replaced = 0;
        while replaced < count && end < text.len() {
            let next = next_char_boundary(&text, end);
            if text[end..next].contains('\n') {
                break;
            }
            end = next;
            replaced += 1;
        }
        if start == end {
            return Ok(None);
        }
        let insert: String =
            std::iter::repeat_n(character, usize::try_from(replaced).unwrap_or_default()).collect();
        let transaction = Transaction::new(self.revision, vec![Edit::new(start..end, insert)])?;
        self.apply_recorded(transaction.clone(), false)?;
        self.collapse_selection(start);
        if let Some(group) = self.undo.last_mut() {
            group.after = self.selections.clone();
        }
        Ok(Some(transaction))
    }

    fn toggle_case(&mut self, count: u32) -> Result<Option<Transaction>, EngineError> {
        self.ensure_writable()?;
        let text = self.contents();
        let mut cursor = self.primary_cursor();
        let mut edits = Vec::new();
        for _ in 0..count {
            if cursor >= text.len() {
                break;
            }
            let next = next_char_boundary(&text, cursor);
            let Some(character) = text[cursor..next].chars().next() else {
                break;
            };
            if character == '\n' {
                break;
            }
            let replacement = if character.is_uppercase() {
                character.to_lowercase().collect::<String>()
            } else {
                character.to_uppercase().collect::<String>()
            };
            edits.push(Edit::new(cursor..next, replacement));
            cursor = next;
        }
        if edits.is_empty() {
            return Ok(None);
        }
        let transaction = Transaction::new(self.revision, edits)?;
        self.apply_recorded(transaction.clone(), false)?;
        let destination = normal_cursor_destination(&self.contents(), cursor);
        self.collapse_selection(destination);
        if let Some(group) = self.undo.last_mut() {
            group.after = self.selections.clone();
        }
        Ok(Some(transaction))
    }

    fn paste(
        &mut self,
        before: bool,
        count: u32,
        register: Option<Register>,
    ) -> Result<Option<Transaction>, EngineError> {
        let Some(value) = self.read_register(register).cloned() else {
            return Ok(None);
        };
        let text = self.contents();
        let cursor = self.primary_cursor();
        let position = if value.linewise {
            if before {
                line_start(&text, cursor)
            } else {
                line_end_with_newline(&text, cursor)
            }
        } else if before {
            cursor
        } else {
            next_char_boundary(&text, cursor).min(line_end(&text, cursor))
        };
        let inserted = value.text.repeat(usize::try_from(count).unwrap_or(1));
        let inserted_len = inserted.len();
        let transaction =
            Transaction::new(self.revision, vec![Edit::new(position..position, inserted)])?;
        self.apply_recorded(transaction.clone(), false)?;
        let new_text = self.contents();
        let destination = if value.linewise {
            first_non_blank(&new_text, position)
        } else if inserted_len == 0 {
            position
        } else {
            previous_char_boundary(&new_text, position.saturating_add(inserted_len))
        };
        self.collapse_selection(destination);
        if let Some(group) = self.undo.last_mut() {
            group.after = self.selections.clone();
        }
        Ok(Some(transaction))
    }

    fn repeat_last(&mut self, count: u32) -> Result<Option<Transaction>, EngineError> {
        let Some(action) = self.last_change.clone() else {
            return Ok(None);
        };
        self.replaying_change = true;
        let result = (|| {
            let mut last = None;
            for _ in 0..count {
                match &action {
                    RepeatAction::Command(command) => {
                        last = self.execute(command.clone())?;
                    }
                    RepeatAction::Insert { style, text } => {
                        let transaction = self.repeated_insert(*style, text)?;
                        self.apply_recorded(transaction.clone(), false)?;
                        if let Some(edit) = transaction.edits.first() {
                            let current = self.contents();
                            let destination = match style {
                                InsertStyle::OpenAbove | InsertStyle::OpenBelow => {
                                    let content_start = edit.range.start
                                        + usize::from(edit.insert.starts_with('\n'));
                                    first_non_blank(&current, content_start)
                                }
                                _ => previous_char_boundary(
                                    &current,
                                    edit.range.start.saturating_add(edit.insert.len()),
                                ),
                            };
                            self.collapse_selection(destination);
                            if let Some(group) = self.undo.last_mut() {
                                group.after = self.selections.clone();
                            }
                        }
                        last = Some(transaction);
                    }
                    RepeatAction::ChangeInsert { command, text } => {
                        last = self.execute(command.clone())?;
                        if self.mode == Mode::Insert {
                            if let Some(transaction) = self.insert_text(text)? {
                                last = Some(transaction);
                            }
                            self.leave_insert();
                        }
                    }
                    RepeatAction::NumberDelta(delta) => {
                        last = self.adjust_number(*delta)?;
                    }
                }
            }
            Ok(last)
        })();
        self.replaying_change = false;
        result
    }

    fn position_for_insert(&mut self, style: InsertStyle) {
        let text = self.contents();
        let cursor = self.primary_cursor();
        let destination = match style {
            InsertStyle::Insert => cursor,
            InsertStyle::Append => next_char_boundary(&text, cursor).min(line_end(&text, cursor)),
            InsertStyle::LineStart => first_non_blank(&text, cursor),
            InsertStyle::LineEnd => line_end(&text, cursor),
            InsertStyle::OpenAbove => line_start(&text, cursor),
            InsertStyle::OpenBelow => line_end_with_newline(&text, cursor),
            InsertStyle::Replace => cursor,
        };
        self.collapse_selection(destination);
    }

    fn repeated_insert(
        &mut self,
        style: InsertStyle,
        inserted: &str,
    ) -> Result<Transaction, EngineError> {
        let text = self.contents();
        let cursor = self.primary_cursor();
        let (position, content) = match style {
            InsertStyle::OpenAbove => (line_start(&text, cursor), format!("{inserted}\n")),
            InsertStyle::OpenBelow => {
                let end = line_end(&text, cursor);
                if end < text.len() {
                    (end + 1, format!("{inserted}\n"))
                } else {
                    (end, format!("\n{inserted}"))
                }
            }
            InsertStyle::Replace => {
                let mut end = cursor;
                for _ in inserted.chars() {
                    if end >= text.len() || text.as_bytes().get(end) == Some(&b'\n') {
                        break;
                    }
                    end = next_char_boundary(&text, end);
                }
                return Ok(Transaction::new(
                    self.revision,
                    vec![Edit::new(cursor..end, inserted.to_owned())],
                )?);
            }
            _ => {
                self.position_for_insert(style);
                (self.primary_cursor(), inserted.to_owned())
            }
        };
        Ok(Transaction::new(
            self.revision,
            vec![Edit::new(position..position, content)],
        )?)
    }

    fn move_cursor(&mut self, motion: Motion, count: u32) {
        let text = self.contents();
        let destination = normal_cursor_destination(
            &text,
            self.motion_destination(self.primary_cursor(), motion, count),
        );
        self.collapse_selection(destination);
    }

    fn motion_destination(&self, cursor: usize, motion: Motion, count: u32) -> usize {
        let text = self.contents();
        let mut destination = cursor.min(text.len());
        if motion == Motion::LineFirstNonBlank {
            for _ in 1..count.max(1) {
                destination = vertical_motion(&text, destination, 1);
            }
            return first_non_blank(&text, destination);
        }
        if motion == Motion::Column {
            return byte_at_line_column(&text, destination, count.saturating_sub(1) as usize);
        }
        for _ in 0..count {
            destination = match motion {
                Motion::Left => {
                    previous_char_boundary(&text, destination).max(line_start(&text, destination))
                }
                Motion::Right => {
                    next_char_boundary(&text, destination).min(line_end(&text, destination))
                }
                Motion::WordBackward => word_backward(&text, destination),
                Motion::WordForward => word_forward(&text, destination),
                Motion::WordEnd => word_end(&text, destination),
                Motion::BigWordBackward => big_word_backward(&text, destination),
                Motion::BigWordForward => big_word_forward(&text, destination),
                Motion::BigWordEnd => big_word_end(&text, destination),
                Motion::WordEndBackward => word_end_backward(&text, destination),
                Motion::LineStart => line_start(&text, destination),
                Motion::FirstNonBlank => first_non_blank(&text, destination),
                Motion::NextLineFirstNonBlank => {
                    first_non_blank(&text, vertical_motion(&text, destination, 1))
                }
                Motion::PreviousLineFirstNonBlank => {
                    first_non_blank(&text, vertical_motion(&text, destination, -1))
                }
                Motion::LineFirstNonBlank => first_non_blank(&text, destination),
                Motion::LastNonBlank => last_non_blank(&text, destination),
                Motion::Column => {
                    byte_at_line_column(&text, destination, count.saturating_sub(1) as usize)
                }
                Motion::LineEnd => line_end(&text, destination),
                Motion::GoToLine => byte_of_line(&text, count.saturating_sub(1) as usize),
                Motion::DocumentEnd => first_non_blank(&text, line_start(&text, text.len())),
                Motion::WholeLine => line_end_with_newline(&text, destination),
                Motion::Up => vertical_motion(&text, destination, -1),
                Motion::Down => vertical_motion(&text, destination, 1),
                Motion::FindForward(character) => find_on_line(&text, destination, character, true),
                Motion::FindBackward(character) => {
                    find_on_line(&text, destination, character, false)
                }
                Motion::TillForward(character) => {
                    let found = find_on_line(&text, destination, character, true);
                    if found == destination {
                        destination
                    } else {
                        previous_char_boundary(&text, found)
                    }
                }
                Motion::TillBackward(character) => {
                    let found = find_on_line(&text, destination, character, false);
                    if found == destination {
                        destination
                    } else {
                        next_char_boundary(&text, found)
                    }
                }
                Motion::ParagraphForward => paragraph_forward(&text, destination),
                Motion::ParagraphBackward => paragraph_backward(&text, destination),
                Motion::MatchPair => matching_pair(&text, destination),
                Motion::Inside(object) | Motion::Around(object) => {
                    text_object_range(
                        &text,
                        destination,
                        object,
                        matches!(motion, Motion::Around(_)),
                    )
                    .start
                }
            };
        }
        floor_char_boundary(&text, destination.min(text.len()))
    }

    fn write_register(&mut self, register: Option<Register>, text: &str, linewise: bool) {
        if register == Some(Register::BlackHole) {
            return;
        }
        let value = RegisterValue {
            text: Box::from(text),
            linewise,
        };
        self.registers.insert('"', value.clone());
        if self.clipboard_unnamed && (register.is_none() || register == Some(Register::Unnamed)) {
            self.registers.insert('+', value.clone());
        }
        if let Some(name) = register_key(register) {
            if name.is_ascii_uppercase() {
                let key = name.to_ascii_lowercase();
                self.registers
                    .entry(key)
                    .and_modify(|existing| {
                        let mut joined = existing.text.to_string();
                        joined.push_str(text);
                        existing.text = joined.into_boxed_str();
                    })
                    .or_insert(value);
            } else {
                self.registers.insert(name, value);
            }
        }
    }

    fn write_yank_register(&mut self, register: Option<Register>, text: &str, linewise: bool) {
        if register == Some(Register::BlackHole) {
            return;
        }
        self.write_register(register, text, linewise);
        if register.is_none() || register == Some(Register::Unnamed) {
            self.registers.insert(
                '0',
                RegisterValue {
                    text: text.into(),
                    linewise,
                },
            );
        }
    }

    fn write_delete_register(&mut self, register: Option<Register>, text: &str, linewise: bool) {
        if register == Some(Register::BlackHole) {
            return;
        }
        self.write_register(register, text, linewise);
        let value = RegisterValue {
            text: text.into(),
            linewise,
        };
        if linewise || text.contains('\n') {
            for number in (2_u8..=9).rev() {
                if let Some(previous) = self.registers.get(&char::from(b'0' + number - 1)).cloned()
                {
                    self.registers.insert(char::from(b'0' + number), previous);
                }
            }
            self.registers.insert('1', value);
        } else {
            self.registers.insert('-', value);
        }
    }

    fn read_register(&self, register: Option<Register>) -> Option<&RegisterValue> {
        let key = register_key(register).unwrap_or('"').to_ascii_lowercase();
        self.registers.get(&key)
    }

    fn finish_pending_control(
        &mut self,
        control: PendingControl,
        key: KeyEvent,
    ) -> Result<Option<Transaction>, EngineError> {
        let KeyCode::Char(character) = key.code else {
            return Err(EngineError::InvalidGrammar);
        };
        match control {
            PendingControl::RecordMacro if character.is_ascii_alphanumeric() => {
                self.recording_macro = Some(character.to_ascii_lowercase());
                self.recording_keys.clear();
                Ok(None)
            }
            PendingControl::ReplayMacro => self.replay_macro(character),
            PendingControl::SetMark if character.is_ascii_alphabetic() => {
                self.marks.insert(
                    character,
                    Anchor {
                        byte: self.primary_cursor(),
                        bias: Bias::Left,
                    },
                );
                Ok(None)
            }
            PendingControl::JumpMark { linewise } => {
                if let Some(anchor) = self.marks.get(&character).copied() {
                    let text = self.contents();
                    let destination = if linewise {
                        first_non_blank(&text, anchor.byte)
                    } else {
                        anchor.byte
                    };
                    if destination != self.primary_cursor() {
                        self.push_jump(self.primary_cursor());
                    }
                    self.collapse_selection(destination);
                }
                Ok(None)
            }
            _ => Err(EngineError::InvalidGrammar),
        }
    }

    fn finish_macro_recording(&mut self) {
        if let Some(register) = self.recording_macro.take() {
            let keys = std::mem::take(&mut self.recording_keys);
            let text: String = keys.iter().filter_map(macro_key_character).collect();
            self.macros.insert(register, keys);
            self.registers.insert(
                register,
                RegisterValue {
                    text: text.into_boxed_str(),
                    linewise: false,
                },
            );
        }
    }

    fn replay_macro(&mut self, register: char) -> Result<Option<Transaction>, EngineError> {
        let register = if register == '@' {
            self.last_macro.unwrap_or('@')
        } else {
            register.to_ascii_lowercase()
        };
        let Some(keys) = self.macros.get(&register).cloned() else {
            return Ok(None);
        };
        if self.macro_depth >= 32 {
            return Err(EngineError::MacroRecursion);
        }
        self.last_macro = Some(register);
        self.macro_depth += 1;
        let result = (|| {
            let mut last = None;
            for key in keys {
                if let Some(transaction) = self.handle_key(key)? {
                    last = Some(transaction);
                }
            }
            Ok(last)
        })();
        self.macro_depth -= 1;
        result
    }

    fn collapse_selection(&mut self, byte: usize) {
        if let Some(primary) = self.selections.ranges.get_mut(self.selections.primary) {
            primary.anchor = byte;
            primary.head = byte;
        }
    }

    fn push_jump(&mut self, byte: usize) {
        if self.jumplist.last().copied() != Some(byte) {
            self.jumplist.push(byte);
        }
        self.jump_index = self.jumplist.len();
    }

    fn ensure_writable(&self) -> Result<(), EngineError> {
        if self.read_only {
            Err(EngineError::ReadOnly)
        } else {
            Ok(())
        }
    }
}

fn register_key(register: Option<Register>) -> Option<char> {
    match register {
        None | Some(Register::Unnamed) => Some('"'),
        Some(Register::Named(name)) => Some(name),
        Some(Register::Numbered(number)) => char::from_digit(u32::from(number), 10),
        Some(Register::SmallDelete) => Some('-'),
        Some(Register::BlackHole) => Some('_'),
        Some(Register::Clipboard) => Some('+'),
        Some(Register::PrimarySelection) => Some('*'),
        Some(Register::Expression) => Some('='),
    }
}

fn floor_char_boundary(text: &str, mut byte: usize) -> usize {
    byte = byte.min(text.len());
    while byte > 0 && !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

fn previous_char_boundary(text: &str, byte: usize) -> usize {
    text[..floor_char_boundary(text, byte)]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_char_boundary(text: &str, byte: usize) -> usize {
    let byte = floor_char_boundary(text, byte);
    text[byte..]
        .chars()
        .next()
        .map_or(byte, |character| byte + character.len_utf8())
}

fn macro_key_character(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(character) if key.modifiers.is_empty() => Some(character),
        KeyCode::Escape => Some('\u{1b}'),
        KeyCode::Enter => Some('\r'),
        KeyCode::Tab => Some('\t'),
        _ => None,
    }
}

fn line_start(text: &str, byte: usize) -> usize {
    let byte = floor_char_boundary(text, byte);
    text[..byte].rfind('\n').map_or(0, |index| index + 1)
}

fn line_column_at(text: &str, byte: usize) -> (usize, usize) {
    let byte = floor_char_boundary(text, byte.min(text.len()));
    let start = line_start(text, byte);
    (
        text[..byte].bytes().filter(|value| *value == b'\n').count(),
        text[start..byte].chars().count(),
    )
}

fn line_end(text: &str, byte: usize) -> usize {
    let byte = floor_char_boundary(text, byte);
    text[byte..]
        .find('\n')
        .map_or(text.len(), |offset| byte + offset)
}

fn line_end_with_newline(text: &str, byte: usize) -> usize {
    let end = line_end(text, byte);
    if end < text.len() { end + 1 } else { end }
}

fn normal_cursor_destination(text: &str, byte: usize) -> usize {
    let byte = floor_char_boundary(text, byte.min(text.len()));
    let start = line_start(text, byte);
    let end = line_end(text, byte);
    if end > start && byte >= end {
        previous_char_boundary(text, end)
    } else {
        byte
    }
}

fn first_non_blank(text: &str, byte: usize) -> usize {
    let start = line_start(text, byte);
    let end = line_end(text, byte);
    text[start..end]
        .char_indices()
        .find(|(_, character)| !matches!(character, ' ' | '\t'))
        .map_or(end, |(offset, _)| start + offset)
}

fn last_non_blank(text: &str, byte: usize) -> usize {
    let start = line_start(text, byte);
    let end = line_end(text, byte);
    text[start..end]
        .char_indices()
        .rev()
        .find(|(_, character)| !matches!(character, ' ' | '\t'))
        .map_or(start, |(offset, _)| start + offset)
}

fn byte_at_line_column(text: &str, byte: usize, column: usize) -> usize {
    let start = line_start(text, byte);
    let end = line_end(text, byte);
    text[start..end]
        .char_indices()
        .nth(column)
        .map_or_else(|| last_non_blank(text, byte), |(offset, _)| start + offset)
}

fn byte_of_line(text: &str, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    text.match_indices('\n')
        .nth(line - 1)
        .map_or(text.len(), |(offset, _)| offset + 1)
}

fn vertical_motion(text: &str, byte: usize, direction: i8) -> usize {
    let start = line_start(text, byte);
    let column = text[start..byte].chars().count();
    let target_start = if direction < 0 {
        if start == 0 {
            return byte;
        }
        line_start(text, start - 1)
    } else {
        let next = line_end_with_newline(text, byte);
        if next >= text.len() {
            return byte;
        }
        next
    };
    let target_end = line_end(text, target_start);
    text[target_start..target_end]
        .char_indices()
        .nth(column)
        .map_or(target_end, |(offset, _)| target_start + offset)
}

fn char_class(character: char) -> u8 {
    if character.is_whitespace() {
        0
    } else if character.is_alphanumeric() || character == '_' {
        1
    } else {
        2
    }
}

fn word_forward(text: &str, byte: usize) -> usize {
    let mut iterator = text[byte..].char_indices().peekable();
    let Some((_, first)) = iterator.next() else {
        return byte;
    };
    let class = char_class(first);
    while let Some((_, character)) = iterator.peek() {
        if char_class(*character) != class {
            break;
        }
        iterator.next();
    }
    while let Some((_, character)) = iterator.peek() {
        if !character.is_whitespace() {
            break;
        }
        iterator.next();
    }
    iterator
        .peek()
        .map_or(text.len(), |(offset, _)| byte + offset)
}

fn word_backward(text: &str, byte: usize) -> usize {
    let mut cursor = previous_char_boundary(text, byte);
    while cursor > 0 {
        let character = text[cursor..].chars().next().unwrap_or(' ');
        if !character.is_whitespace() {
            break;
        }
        cursor = previous_char_boundary(text, cursor);
    }
    let class = text[cursor..].chars().next().map_or(0, char_class);
    while cursor > 0 {
        let previous = previous_char_boundary(text, cursor);
        let previous_class = text[previous..].chars().next().map_or(0, char_class);
        if previous_class != class {
            break;
        }
        cursor = previous;
    }
    cursor
}

fn word_end(text: &str, byte: usize) -> usize {
    let mut cursor = byte;
    while cursor < text.len() {
        let character = text[cursor..].chars().next().unwrap_or(' ');
        if !character.is_whitespace() {
            break;
        }
        cursor = next_char_boundary(text, cursor);
    }
    let class = text[cursor..].chars().next().map_or(0, char_class);
    while cursor < text.len() {
        let next = next_char_boundary(text, cursor);
        if next >= text.len() || text[next..].chars().next().map_or(0, char_class) != class {
            return cursor;
        }
        cursor = next;
    }
    cursor
}

fn big_word_forward(text: &str, byte: usize) -> usize {
    let mut cursor = byte.min(text.len());
    while cursor < text.len()
        && !text[cursor..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
    {
        cursor = next_char_boundary(text, cursor);
    }
    while cursor < text.len()
        && text[cursor..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
    {
        cursor = next_char_boundary(text, cursor);
    }
    cursor
}

fn big_word_backward(text: &str, byte: usize) -> usize {
    let mut cursor = previous_char_boundary(text, byte.min(text.len()));
    while cursor > 0
        && text[cursor..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
    {
        cursor = previous_char_boundary(text, cursor);
    }
    while cursor > 0 {
        let previous = previous_char_boundary(text, cursor);
        if text[previous..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
        {
            break;
        }
        cursor = previous;
    }
    cursor
}

fn big_word_end(text: &str, byte: usize) -> usize {
    let mut cursor = byte.min(text.len());
    while cursor < text.len()
        && text[cursor..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
    {
        cursor = next_char_boundary(text, cursor);
    }
    while cursor < text.len() {
        let next = next_char_boundary(text, cursor);
        if next >= text.len() || text[next..].chars().next().is_some_and(char::is_whitespace) {
            return cursor;
        }
        cursor = next;
    }
    cursor
}

fn word_end_backward(text: &str, byte: usize) -> usize {
    let mut cursor = previous_char_boundary(text, byte.min(text.len()));
    while cursor > 0
        && text[cursor..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
    {
        cursor = previous_char_boundary(text, cursor);
    }
    cursor
}

fn paragraph_forward(text: &str, byte: usize) -> usize {
    let search_start = next_char_boundary(text, byte.min(text.len()));
    let Some(boundary) = text[search_start..].find("\n\n") else {
        return text.len();
    };
    search_start + boundary + 1
}

fn paragraph_backward(text: &str, byte: usize) -> usize {
    let before = line_start(text, byte.min(text.len()));
    let Some(boundary) = text[..before.saturating_sub(1)].rfind("\n\n") else {
        return 0;
    };
    let mut cursor = boundary + 2;
    while cursor < before && text.as_bytes().get(cursor) == Some(&b'\n') {
        cursor += 1;
    }
    cursor
}

fn matching_pair(text: &str, byte: usize) -> usize {
    let line_end = line_end(text, byte);
    let candidate = text[byte.min(text.len())..line_end]
        .char_indices()
        .find(|(_, character)| matches!(character, '(' | ')' | '[' | ']' | '{' | '}'))
        .map(|(offset, character)| (byte.min(text.len()) + offset, character));
    let Some((origin, character)) = candidate else {
        return byte;
    };
    let (open, close, forward) = match character {
        '(' => ('(', ')', true),
        '[' => ('[', ']', true),
        '{' => ('{', '}', true),
        ')' => ('(', ')', false),
        ']' => ('[', ']', false),
        '}' => ('{', '}', false),
        _ => return byte,
    };
    let mut depth = 0_u32;
    if forward {
        for (offset, current) in text[origin..].char_indices() {
            if current == open {
                depth = depth.saturating_add(1);
            } else if current == close {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return origin + offset;
                }
            }
        }
    } else {
        for (offset, current) in text[..=origin].char_indices().rev() {
            if current == close {
                depth = depth.saturating_add(1);
            } else if current == open {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return offset;
                }
            }
        }
    }
    byte
}

fn find_on_line(text: &str, byte: usize, needle: char, forward: bool) -> usize {
    let start = line_start(text, byte);
    let end = line_end(text, byte);
    if forward {
        let after = next_char_boundary(text, byte).min(end);
        text[after..end]
            .find(needle)
            .map_or(byte, |offset| after + offset)
    } else {
        text[start..byte]
            .rfind(needle)
            .map_or(byte, |offset| start + offset)
    }
}

fn find_literal(haystack: &str, needle: &str, ignore_ascii_case: bool) -> Option<usize> {
    if !ignore_ascii_case || !needle.is_ascii() {
        return haystack.find(needle);
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn rfind_literal(haystack: &str, needle: &str, ignore_ascii_case: bool) -> Option<usize> {
    if !ignore_ascii_case || !needle.is_ascii() {
        return haystack.rfind(needle);
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .rposition(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn whole_line_range(text: &str, byte: usize, count: u32) -> Range<usize> {
    let start = line_start(text, byte);
    let mut end = start;
    for _ in 0..count {
        end = line_end_with_newline(text, end);
    }
    start..end
}

fn text_object_range(text: &str, byte: usize, object: TextObject, around: bool) -> Range<usize> {
    match object {
        TextObject::Word | TextObject::BigWord => word_object_range(text, byte, around),
        TextObject::Paragraph => whole_line_range(text, byte, 1),
        TextObject::Quotes(quote) => quote_object_range(text, byte, quote, around),
        TextObject::Brackets(open) => bracket_object_range(text, byte, open, around),
    }
}

fn word_object_range(text: &str, byte: usize, around: bool) -> Range<usize> {
    let mut start = byte.min(text.len());
    let mut end = start;
    while start > 0 {
        let previous = previous_char_boundary(text, start);
        let character = text[previous..start].chars().next().unwrap_or(' ');
        if character.is_whitespace() {
            break;
        }
        start = previous;
    }
    while end < text.len() {
        let next = next_char_boundary(text, end);
        let character = text[end..next].chars().next().unwrap_or(' ');
        if character.is_whitespace() {
            break;
        }
        end = next;
    }
    if around {
        while end < text.len() {
            let next = next_char_boundary(text, end);
            if !text[end..next]
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
            {
                break;
            }
            end = next;
        }
    }
    start..end
}

fn quote_object_range(text: &str, byte: usize, quote: char, around: bool) -> Range<usize> {
    let start_line = line_start(text, byte);
    let end_line = line_end(text, byte);
    let left = text[start_line..byte]
        .rfind(quote)
        .map(|offset| start_line + offset);
    let after = next_char_boundary(text, byte).min(end_line);
    let right = text[after..end_line]
        .find(quote)
        .map(|offset| after + offset);
    match (left, right) {
        (Some(left), Some(right)) if around => left..next_char_boundary(text, right),
        (Some(left), Some(right)) => next_char_boundary(text, left)..right,
        _ => byte..byte,
    }
}

fn bracket_object_range(text: &str, byte: usize, open: char, around: bool) -> Range<usize> {
    let close = match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        _ => open,
    };
    let left = text[..byte].rfind(open);
    let right = text[byte..].find(close).map(|offset| byte + offset);
    match (left, right) {
        (Some(left), Some(right)) if around => left..next_char_boundary(text, right),
        (Some(left), Some(right)) => next_char_boundary(text, left)..right,
        _ => byte..byte,
    }
}

fn line_edits(text: &str, range: Range<usize>, indent: bool, shift_width: usize) -> Vec<Edit> {
    let mut edits = Vec::new();
    let mut start = line_start(text, range.start);
    while start < range.end.min(text.len()) || (start == 0 && text.is_empty()) {
        if indent {
            edits.push(Edit::new(start..start, " ".repeat(shift_width)));
        } else {
            let end = (start + shift_width).min(line_end(text, start));
            let removable = text[start..end]
                .bytes()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
            if removable > 0 {
                edits.push(Edit::new(start..start + removable, ""));
            }
        }
        let next = line_end_with_newline(text, start);
        if next <= start || next >= range.end {
            break;
        }
        start = next;
    }
    edits
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use wren_text::RopeyText;

    use super::*;

    fn editor(source: &str) -> Editor<RopeyText> {
        Editor::new(RopeyText::from_reader(Cursor::new(source)).expect("load text"))
    }

    fn feed(editor: &mut Editor<RopeyText>, keys: &str) {
        for character in keys.chars() {
            editor
                .handle_key(if character == '\u{1b}' {
                    KeyEvent::plain(KeyCode::Escape)
                } else {
                    KeyEvent::character(character)
                })
                .expect("key is accepted");
        }
    }

    #[test]
    fn insert_group_undo_and_redo_are_transactional() {
        let mut editor = editor("ab");
        feed(&mut editor, "i界x\u{1b}");
        assert_eq!(editor.contents(), "界xab");
        assert_eq!(editor.undo_depth(), 1);
        feed(&mut editor, "u");
        assert_eq!(editor.contents(), "ab");
        editor
            .handle_key(KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: Modifiers::CONTROL,
            })
            .expect("redo");
        assert_eq!(editor.contents(), "界xab");
    }

    #[test]
    fn motions_operators_registers_and_paste_work_by_line() {
        let mut editor = editor("alpha beta\nsecond\n");
        feed(&mut editor, "dw");
        assert_eq!(editor.contents(), "beta\nsecond\n");
        assert_eq!(
            editor.register('"').map(|value| value.text.as_ref()),
            Some("alpha ")
        );
        feed(&mut editor, "p");
        assert_eq!(editor.contents(), "balpha eta\nsecond\n");
        feed(&mut editor, "ggdd");
        assert_eq!(editor.contents(), "second\n");
        assert!(editor.register('"').is_some_and(|value| value.linewise));
    }

    #[test]
    fn vertical_unicode_motion_preserves_scalar_column() {
        let mut editor = editor("a界c\nxyz\n");
        feed(&mut editor, "llj");
        assert_eq!(editor.cursor_line_column(), (1, 2));
        feed(&mut editor, "k");
        assert_eq!(editor.primary_cursor(), "a界".len());
    }

    #[test]
    fn big_word_till_pair_and_paragraph_motions_are_native() {
        let mut editor = editor("aa.bb cc-dd\n\npara (x[y]) end\n\nlast");
        feed(&mut editor, "E");
        assert_eq!(editor.primary_cursor(), 4);
        feed(&mut editor, "W");
        assert_eq!(editor.primary_cursor(), 6);
        feed(&mut editor, "0dt-");
        assert_eq!(editor.contents(), "-dd\n\npara (x[y]) end\n\nlast");
        editor.set_cursor(editor.contents().find('(').expect("open pair"));
        feed(&mut editor, "%");
        assert_eq!(
            editor.contents()[editor.primary_cursor()..].chars().next(),
            Some(')')
        );
        feed(&mut editor, "}");
        assert_eq!(editor.cursor_line_column(), (3, 0));
        feed(&mut editor, "{");
        assert_eq!(editor.cursor_line_column(), (2, 0));
    }

    #[test]
    fn search_wraps_and_marks_map_through_edits() {
        let mut editor = editor("one two one\n");
        assert!(editor.search("one", SearchDirection::Forward));
        assert_eq!(editor.primary_cursor(), 8);
        feed(&mut editor, "ma0iX\u{1b}`a");
        assert_eq!(editor.primary_cursor(), 9);
        feed(&mut editor, "n");
        assert_eq!(editor.primary_cursor(), 1);
    }

    #[test]
    fn dotfile_search_is_ignorecase_with_smartcase_and_unnamed_uses_clipboard() {
        let mut editor = editor("Alpha alpha BETA\n");
        editor.set_search_options(true, true);
        assert!(editor.search("alpha", SearchDirection::Forward));
        assert_eq!(editor.primary_cursor(), 6);
        editor.set_cursor(0);
        assert!(editor.search("BETA", SearchDirection::Forward));
        assert_eq!(editor.primary_cursor(), 12);
        assert!(!editor.search("Beta", SearchDirection::Forward));

        editor.set_clipboard_unnamed(true);
        editor.set_cursor(0);
        feed(&mut editor, "yw");
        assert_eq!(
            editor.register('+').map(|value| value.text.as_ref()),
            Some("Alpha ")
        );
    }

    #[test]
    fn dotfile_indentation_expands_tabs_smartindents_and_shifts_by_two() {
        let mut braces = editor("");
        let mut editor = editor("fn main() {\nbody\n}\n");
        editor.set_indent_options(true, 2, 2, true);
        feed(&mut editor, "A");
        editor
            .handle_key(KeyEvent::plain(KeyCode::Enter))
            .expect("smart-indented newline");
        editor
            .handle_key(KeyEvent::plain(KeyCode::Tab))
            .expect("expanded tab");
        feed(&mut editor, "x\u{1b}j>>");
        assert_eq!(editor.contents(), "fn main() {\n    x\n  body\n}\n");

        braces.set_indent_options(true, 2, 2, true);
        feed(&mut braces, "ifn() {");
        braces
            .handle_key(KeyEvent::plain(KeyCode::Enter))
            .expect("newline inside braces");
        feed(&mut braces, "x");
        braces
            .handle_key(KeyEvent::plain(KeyCode::Enter))
            .expect("newline before closing brace");
        feed(&mut braces, "}");
        assert_eq!(braces.contents(), "fn() {\n  x\n}");
    }

    #[test]
    fn dotfile_visual_v_expands_and_control_v_shrinks_the_region() {
        let mut editor = editor("one two\n");
        editor.set_expand_region_keys(true);
        feed(&mut editor, "vv");
        assert_eq!(editor.selection_byte_range(), 0..3);
        feed(&mut editor, "v");
        assert_eq!(editor.selection_byte_range(), 0..8);
        editor
            .handle_key(KeyEvent {
                code: KeyCode::Char('v'),
                modifiers: Modifiers::CONTROL,
            })
            .expect("shrink region");
        assert_eq!(editor.selection_byte_range(), 0..3);
    }

    #[test]
    fn records_and_replays_raw_key_macros() {
        let mut editor = editor("abc\n");
        feed(&mut editor, "qaix\u{1b}q@a");
        assert_eq!(editor.contents(), "xxabc\n");
    }

    #[test]
    fn dot_repeats_insert_and_operator_changes() {
        let mut editor = editor("ab cd");
        feed(&mut editor, "ix\u{1b}w.");
        assert_eq!(editor.contents(), "xab xcd");
        feed(&mut editor, "0xw.");
        assert_eq!(editor.contents(), "ab cd");
    }

    #[test]
    fn dot_repeat_action_survives_durable_round_trip() {
        let mut original = editor("one two");
        feed(&mut original, "A!");
        let durable = original.durable_repeat_data().expect("repeat state");

        let mut restored = editor("alpha\nbeta");
        restored
            .restore_repeat_data(&durable)
            .expect("restore repeat");
        feed(&mut restored, "j.");
        assert_eq!(restored.contents(), "alpha\nbeta!");
        assert!(restored.restore_repeat_data(b"not-json").is_err());
    }

    #[test]
    fn divergent_undo_history_retains_the_abandoned_redo_branch() {
        let mut editor = editor("abc");
        feed(&mut editor, "x");
        feed(&mut editor, "u");
        feed(&mut editor, "rZ");
        assert_eq!(editor.contents(), "Zbc");
        assert_eq!(editor.undo_depth(), 1);
        assert_eq!(editor.redo_depth(), 0);
        assert_eq!(editor.undo_tree_len(), 2);
    }

    #[test]
    fn durable_undo_state_restores_undo_redo_and_abandoned_branches() {
        let mut original = editor("abc");
        feed(&mut original, "xurZ");
        assert_eq!(original.contents(), "Zbc");
        let state = original.durable_undo_state();

        let mut restored = editor("Zbc");
        restored
            .restore_undo_state(state)
            .expect("restore undo tree");
        feed(&mut restored, "u");
        assert_eq!(restored.contents(), "abc");
        assert_eq!(restored.undo_tree_len(), 2);
        restored
            .handle_key(KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: Modifiers::CONTROL,
            })
            .expect("redo restored change");
        assert_eq!(restored.contents(), "Zbc");
    }

    #[test]
    fn replace_and_open_line_are_undoable() {
        let mut editor = editor("abc\n");
        feed(&mut editor, "2rXohello\u{1b}");
        assert_eq!(editor.contents(), "XXc\nhello\n");
        feed(&mut editor, "u");
        assert_eq!(editor.contents(), "XXc\n");
    }

    #[test]
    fn substitute_line_substitute_and_toggle_case_repeat_transactionally() {
        let mut editor = editor("alpha beta\nsecond\n");
        feed(&mut editor, "sXw2~");
        assert_eq!(editor.contents(), "Xlpha BEta\nsecond\n");
        feed(&mut editor, "jSline");
        assert_eq!(editor.contents(), "Xlpha BEta\nline\n");
        feed(&mut editor, "u");
        assert_eq!(editor.contents(), "Xlpha BEta\nsecond\n");
    }

    #[test]
    fn exposes_pending_operator_state() {
        let mut editor = editor("abc");
        editor
            .handle_key(KeyEvent::character('d'))
            .expect("pending delete");
        assert!(matches!(
            editor.pending_parse_state(),
            Some(ParseState::OperatorPending { .. })
        ));
    }

    #[test]
    fn read_only_documents_allow_navigation_but_reject_changes() {
        let mut editor = editor("abc");
        editor.set_read_only(true);
        feed(&mut editor, "l");
        assert_eq!(editor.primary_cursor(), 1);
        assert!(matches!(
            editor.handle_key(KeyEvent::character('i')),
            Err(EngineError::ReadOnly)
        ));
        assert!(matches!(
            editor.handle_key(KeyEvent::character('x')),
            Err(EngineError::ReadOnly)
        ));
        assert_eq!(editor.contents(), "abc");
    }

    #[test]
    fn line_change_is_one_undo_group_and_dot_repeats_the_whole_change() {
        let mut editor = editor("old\nnext\n");
        feed(&mut editor, "ccnew\u{1b}");
        assert_eq!(editor.contents(), "new\nnext\n");
        assert_eq!(editor.undo_depth(), 1);
        feed(&mut editor, "j.");
        assert_eq!(editor.contents(), "new\nnew\n");
        feed(&mut editor, "u");
        assert_eq!(editor.contents(), "new\nnext\n");
    }

    #[test]
    fn dot_repeat_of_open_line_creates_another_line() {
        let mut editor = editor("one\ntwo\n");
        feed(&mut editor, "oX\u{1b}j.");
        assert_eq!(editor.contents(), "one\nX\ntwo\nX\n");
    }

    #[test]
    fn visual_character_and_line_operators_are_native_transactions() {
        let mut editor = editor("alpha beta\nsecond\nthird\n");
        feed(&mut editor, "vwd");
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.contents(), "eta\nsecond\nthird\n");
        assert_eq!(
            editor.register('"').map(|value| value.text.as_ref()),
            Some("alpha b")
        );
        feed(&mut editor, "Vjy");
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(
            editor.register('"').map(|value| value.text.as_ref()),
            Some("eta\nsecond\n")
        );
    }

    #[test]
    fn visual_change_enters_insert_and_is_undoable() {
        let mut editor = editor("word tail");
        feed(&mut editor, "vechello\u{1b}");
        assert_eq!(editor.contents(), "hello tail");
        assert_eq!(editor.mode(), Mode::Normal);
        feed(&mut editor, "u");
        assert_eq!(editor.contents(), "word tail");
    }

    #[test]
    fn basic_line_column_and_last_nonblank_motions_match_vim_shapes() {
        let mut editor = editor("one\n  two\nthree  \n");
        editor
            .handle_key(KeyEvent::plain(KeyCode::Escape))
            .expect("normal Escape is a native no-op");
        feed(&mut editor, "+");
        assert_eq!(editor.cursor_line_column(), (1, 2));
        feed(&mut editor, "-");
        assert_eq!(editor.cursor_line_column(), (0, 0));
        feed(&mut editor, "2_");
        assert_eq!(editor.cursor_line_column(), (1, 2));
        feed(&mut editor, "2|");
        assert_eq!(editor.cursor_line_column(), (1, 1));
        feed(&mut editor, "jg_");
        assert_eq!(editor.cursor_line_column(), (2, 4));
    }

    #[test]
    fn replace_mode_restores_with_backspace_groups_undo_and_dot_repeats() {
        let mut editor = editor("abc def");
        feed(&mut editor, "RXY");
        assert_eq!(editor.mode(), Mode::Replace);
        assert_eq!(editor.contents(), "XYc def");
        editor
            .handle_key(KeyEvent::plain(KeyCode::Backspace))
            .expect("replace backspace");
        assert_eq!(editor.contents(), "Xbc def");
        feed(&mut editor, "Z");
        assert_eq!(editor.contents(), "XZc def");
        assert_eq!(editor.undo_depth(), 1);
        feed(&mut editor, "u");
        assert_eq!(editor.contents(), "abc def");
        feed(&mut editor, "RXYw.");
        assert_eq!(editor.contents(), "XYc XYf");
    }

    #[test]
    fn control_number_adjustment_preserves_zero_padding_and_repeats() {
        let mut editor = editor("item 007 and 9");
        editor.adjust_number(2).expect("increment");
        assert_eq!(editor.contents(), "item 009 and 9");
        feed(&mut editor, "w.");
        assert_eq!(editor.contents(), "item 009 and 11");
        editor.adjust_number(-3).expect("decrement");
        assert_eq!(editor.contents(), "item 009 and 8");
    }

    #[test]
    fn visual_o_swaps_the_active_end_of_the_selection() {
        let mut editor = editor("abcd");
        feed(&mut editor, "vlo");
        let selection = &editor.selections().ranges[0];
        assert_eq!((selection.anchor, selection.head), (1, 0));
    }
}
