use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::Range;

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use thiserror::Error;
use wren_grammar::{Command, Grammar, KeyCode, KeyEvent, Modifiers, Motion, Operator, ParseResult, ParseState, RangeKind, Register, TargetAction, TextObject};
use wren_position::LazyLinePositionIndex;
use wren_text::{DefaultText, TextStore};
use wren_types::{
    Anchor, Bias, DocumentRevision, Edit, RealtimeEditBatch, SelRange, SelectionSet, Transaction, TransactionError, floor_char_boundary, merge_ranges,
    ranges_overlap,
};

use crate::{CaseOverride, EngineFrame, FrameText, VimPattern};

pub type TransactionBatch = SmallVec<[Transaction; 1]>;

fn transactions(transaction: Option<Transaction>) -> TransactionBatch {
    transaction.into_iter().collect()
}

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
#[cfg(any(test, feature = "conformance"))]
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
        Self { forward: Vec::with_capacity(8), inverse: Vec::with_capacity(8), before: before.clone(), after: before }
    }

    fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }
}

#[derive(Debug, Clone)]
struct SearchState {
    pattern: Box<str>,
    direction: SearchDirection,
    compiled: VimPattern,
    cache: RefCell<SearchMatchCache>,
}

/// The restorable editor-owned state. Text, revision, presentation options,
/// and transient input parsing stay on [`Editor`]; durable editing history and
/// user state cross persistence boundaries as this single value.
#[derive(Debug, Clone, Default)]
pub struct EditorState {
    undo: Vec<UndoGroup>,
    redo: Vec<UndoGroup>,
    undo_branches: Vec<Vec<UndoGroup>>,
    registers: BTreeMap<char, RegisterValue>,
    marks: BTreeMap<char, Anchor>,
    macros: BTreeMap<char, Vec<KeyEvent>>,
    last_change: Option<RepeatAction>,
    search: Option<SearchState>,
}

impl EditorState {
    pub fn set_undo(&mut self, state: DurableUndoState) {
        self.undo = state.undo;
        self.redo = state.redo;
        self.undo_branches = state.branches;
    }

    pub fn set_register(&mut self, name: char, text: impl Into<Box<str>>, linewise: bool) {
        self.registers.insert(name, RegisterValue { text: text.into(), linewise });
    }

    pub fn set_macro(&mut self, name: char, keys: Vec<KeyEvent>) {
        self.macros.insert(name.to_ascii_lowercase(), keys);
    }

    pub fn set_repeat_data(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        self.last_change = Some(serde_json::from_slice(bytes).map_err(|error| EngineError::InvalidRepeatData(error.to_string().into()))?);
        Ok(())
    }

    pub fn set_search(&mut self, pattern: impl Into<Box<str>>, direction: SearchDirection, ignore_case: bool, smart_case: bool) -> Result<(), EngineError> {
        let pattern = pattern.into();
        let compiled = VimPattern::compile(&pattern, ignore_case, smart_case, CaseOverride::Default).map_err(EngineError::InvalidSearchPattern)?;
        self.search = Some(SearchState { pattern, direction, compiled, cache: RefCell::default() });
        Ok(())
    }

    pub fn set_mark(&mut self, name: char, byte: usize, document_len: usize) {
        self.marks.insert(name, Anchor { byte: byte.min(document_len), bias: Bias::Right });
    }

    fn validate(&self) -> Result<(), EngineError> {
        self.undo.iter().chain(self.redo.iter()).chain(self.undo_branches.iter().flatten()).try_for_each(|group| {
            group.before.validate().map_err(|error| EngineError::InvalidUndoState(error.to_string().into()))?;
            group.after.validate().map_err(|error| EngineError::InvalidUndoState(error.to_string().into()))
        })
    }
}

#[derive(Debug, Clone, Default)]
struct SearchMatchCache {
    revision: Option<DocumentRevision>,
    windows: BTreeMap<(usize, usize), CachedMatchWindow>,
    #[cfg(test)]
    window_scans: usize,
}

#[derive(Debug, Clone)]
struct CachedMatchWindow {
    ranges: Vec<Range<usize>>,
    /// Whether `ranges` contains every non-empty match in the window.
    complete: bool,
}

impl SearchState {
    /// Finds a nearby match without ever making a contiguous copy of the
    /// complete document. Synchronous input only examines bounded windows.
    fn find_from_frame(&self, text: &FrameText, revision: DocumentRevision, direction: SearchDirection, cursor: usize) -> Option<usize> {
        self.cache.borrow_mut().prepare_revision(revision);
        find_search_in_frame_windows(&self.compiled, text, direction, cursor)
    }

    fn match_ranges(&self, text: &str, revision: DocumentRevision, byte_range: Range<usize>, text_origin: usize, limit: usize) -> Vec<Range<usize>> {
        let key = (byte_range.start, byte_range.end);
        let mut cache = self.cache.borrow_mut();
        cache.prepare_revision(revision);
        if let Some(window) = cache.windows.get(&key)
            && (window.complete || window.ranges.len() >= limit)
        {
            return window.ranges.iter().take(limit).cloned().collect();
        }
        drop(cache);
        let window = scan_match_window(&self.compiled, text, byte_range.clone(), text_origin, limit);

        let mut cache = self.cache.borrow_mut();
        cache.prepare_revision(revision);
        const MAX_CACHED_WINDOWS: usize = 32;
        if cache.windows.len() >= MAX_CACHED_WINDOWS
            && let Some(first) = cache.windows.keys().next().copied()
        {
            cache.windows.remove(&first);
        }
        #[cfg(test)]
        {
            cache.window_scans += 1;
        }
        let ranges = window.ranges.iter().take(limit).cloned().collect();
        cache.windows.insert(key, window);
        ranges
    }

    fn map_literal_cache_through(&self, transaction: &Transaction, text: &FrameText, revision: DocumentRevision) {
        let Some(context) = self.compiled.literal_width().map(|width| width.saturating_sub(1)) else {
            return;
        };
        let mut cache = self.cache.borrow_mut();
        if cache.revision != Some(transaction.base_revision()) {
            return;
        }
        let mut affected = transaction
            .edits()
            .iter()
            .filter_map(|edit| {
                let range = transaction.map_range(edit.range.clone(), Bias::Left, Bias::Right).ok()?;
                Some(expand_literal_context(text, range, context))
            })
            .collect::<Vec<_>>();
        merge_ranges(&mut affected);
        let mut windows = BTreeMap::new();
        for ((start, end), cached) in std::mem::take(&mut cache.windows) {
            let Ok(window) = transaction.map_range(start..end, Bias::Left, Bias::Right) else {
                continue;
            };
            let impacts = affected.iter().filter_map(|range| intersect_ranges(range, &window)).collect::<Vec<_>>();
            let mut mapped = cached
                .ranges
                .into_iter()
                .filter(|range| {
                    !transaction.edits().iter().any(|edit| {
                        if edit.range.is_empty() { range.start < edit.range.start && edit.range.start < range.end } else { ranges_overlap(range, &edit.range) }
                    })
                })
                .filter_map(|range| {
                    let range = transaction.map_range(range, Bias::Left, Bias::Right).ok()?;
                    (!range.is_empty()).then_some(range)
                })
                .collect::<Vec<_>>();
            for impact in impacts {
                let source = text.slice(impact.clone());
                mapped
                    .extend(self.compiled.find_iter(&source).map(|found| impact.start.saturating_add(found.start())..impact.start.saturating_add(found.end())));
            }
            mapped.sort_by_key(|range| (range.start, range.end));
            mapped.dedup();
            windows.insert((window.start, window.end), CachedMatchWindow { ranges: mapped, complete: cached.complete });
        }
        cache.revision = Some(revision);
        cache.windows = windows;
    }
}

fn scan_match_window(compiled: &VimPattern, text: &str, byte_range: Range<usize>, text_origin: usize, limit: usize) -> CachedMatchWindow {
    let Some(text_end) = text_origin.checked_add(text.len()) else {
        unreachable!("search context must fit in the document coordinate space");
    };
    assert!(
        text_origin <= byte_range.start && byte_range.start <= byte_range.end && byte_range.end <= text_end,
        "search window must be contained in its text context"
    );
    let relative_start = byte_range.start - text_origin;
    let relative_end = byte_range.end - text_origin;
    assert!(text.is_char_boundary(relative_start) && text.is_char_boundary(relative_end), "search window must begin and end at character boundaries");

    let mut ranges = Vec::new();
    let mut cursor = relative_start;
    while cursor <= relative_end && ranges.len() < limit.saturating_add(1) {
        let Some(found) = compiled.find_at(text, cursor) else {
            break;
        };
        let found_range = text_origin + found.start()..text_origin + found.end();
        if found_range.start >= byte_range.end || found_range.end > byte_range.end {
            break;
        }
        let empty = found.is_empty();
        if is_non_empty_range_within(&found_range, &byte_range) {
            ranges.push(found_range);
        }
        cursor = if empty { next_char_boundary(text, found.start()) } else { found.end() };
        if cursor == text.len() && empty {
            break;
        }
    }
    let complete = ranges.len() <= limit;
    ranges.truncate(limit);
    CachedMatchWindow { ranges, complete }
}

fn is_non_empty_range_within(range: &Range<usize>, window: &Range<usize>) -> bool {
    !range.is_empty() && window.start <= range.start && range.end <= window.end
}

fn previous_frame_char_boundary(text: &FrameText, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    if byte == 0 {
        return 0;
    }
    byte -= 1;
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

fn floor_frame_char_boundary(text: &FrameText, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    while byte > 0 && !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

fn next_frame_char_boundary(text: &FrameText, byte: usize) -> usize {
    let mut byte = byte.saturating_add(1).min(text.len());
    while byte < text.len() && !text.is_char_boundary(byte) {
        byte += 1;
    }
    byte
}

impl SearchMatchCache {
    fn prepare_revision(&mut self, revision: DocumentRevision) {
        if self.revision != Some(revision) {
            self.revision = Some(revision);
            self.windows.clear();
        }
    }
}

fn expand_literal_context(text: &FrameText, range: Range<usize>, context: usize) -> Range<usize> {
    let mut start = range.start.saturating_sub(context).min(text.len());
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = range.end.saturating_add(context).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    start..end
}

fn intersect_ranges(left: &Range<usize>, right: &Range<usize>) -> Option<Range<usize>> {
    let intersection = left.start.max(right.start)..left.end.min(right.end);
    (!intersection.is_empty()).then_some(intersection)
}

const SYNCHRONOUS_SEARCH_WINDOW_BYTES: usize = 64 * 1024;

fn frame_search_window(text: &FrameText, range: Range<usize>) -> (usize, String) {
    let start = text.floor_char_boundary(range.start);
    let end = text.floor_char_boundary(range.end.max(start));
    let mut window = String::with_capacity(end.saturating_sub(start));
    let _ = text.visit_chunks(start..end, |chunk| {
        window.push_str(chunk);
        std::ops::ControlFlow::<()>::Continue(())
    });
    (start, window)
}

fn first_search_match_in_frame_window(pattern: &VimPattern, text: &FrameText, range: Range<usize>, start: usize) -> Option<usize> {
    let (origin, window) = frame_search_window(text, range);
    let offset = start.saturating_sub(origin).min(window.len());
    pattern.find_at(&window, offset).map(|found| origin + found.start())
}

fn last_search_match_in_frame_window(pattern: &VimPattern, text: &FrameText, range: Range<usize>, before: usize) -> Option<usize> {
    let (origin, window) = frame_search_window(text, range);
    pattern.find_iter(&window).map(|found| origin + found.start()).filter(|start| *start < before).last()
}

fn find_search_in_frame_windows(pattern: &VimPattern, text: &FrameText, direction: SearchDirection, cursor: usize) -> Option<usize> {
    let cursor = text.floor_char_boundary(cursor);
    match direction {
        SearchDirection::Forward => {
            let after = text.next_char_boundary(cursor);
            let nearby_end = text.floor_char_boundary(after.saturating_add(SYNCHRONOUS_SEARCH_WINDOW_BYTES));
            first_search_match_in_frame_window(pattern, text, after..nearby_end, after).or_else(|| {
                let wrap_end = text.floor_char_boundary(after.min(SYNCHRONOUS_SEARCH_WINDOW_BYTES));
                first_search_match_in_frame_window(pattern, text, 0..wrap_end, 0)
            })
        }
        SearchDirection::Backward => {
            let nearby_start = text.floor_char_boundary(cursor.saturating_sub(SYNCHRONOUS_SEARCH_WINDOW_BYTES));
            last_search_match_in_frame_window(pattern, text, nearby_start..cursor, cursor).or_else(|| {
                let wrap_start = text.floor_char_boundary(text.len().saturating_sub(SYNCHRONOUS_SEARCH_WINDOW_BYTES));
                last_search_match_in_frame_window(pattern, text, wrap_start..text.len(), text.len())
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum RepeatAction {
    Command(Command),
    Insert { style: InsertStyle, text: Box<str> },
    ChangeInsert { command: Command, text: Box<str> },
    NumberDelta(i64),
}

#[derive(Debug, Clone)]
struct LocationHistory {
    entries: Vec<usize>,
    index: usize,
    limit: usize,
}

impl LocationHistory {
    fn new(capacity: usize, limit: usize, initial: Option<usize>) -> Self {
        let mut entries = Vec::with_capacity(capacity);
        entries.extend(initial);
        Self { index: entries.len(), entries, limit }
    }

    fn push(&mut self, byte: usize) {
        if self.entries.last().copied() != Some(byte) {
            self.entries.push(byte);
            if self.entries.len() > self.limit {
                self.entries.remove(0);
            }
        }
        self.index = self.entries.len();
    }

    fn navigate(&mut self, backward: bool, current: Option<usize>) -> Option<usize> {
        if backward
            && self.index == self.entries.len()
            && let Some(current) = current
        {
            self.push(current);
            self.index = self.entries.len().saturating_sub(1);
        }
        self.index = if backward { self.index.checked_sub(1)? } else { self.index.checked_add(1).filter(|index| *index < self.entries.len())? };
        self.entries.get(self.index).copied()
    }
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Transaction(#[from] TransactionError),
    #[error("transaction targets revision {actual}, current revision is {expected}")]
    RevisionMismatch { expected: u64, actual: u64 },
    #[error("document revision overflow")]
    RevisionOverflow,
    #[error("grammar rejected the pending key sequence: {reason}")]
    InvalidGrammar { sequence: Box<[KeyEvent]>, reason: Box<str> },
    #[error("macro recursion limit exceeded")]
    MacroRecursion,
    #[error("document is read-only")]
    ReadOnly,
    #[error("durable repeat data is invalid: {0}")]
    InvalidRepeatData(Box<str>),
    #[error("durable undo state is invalid: {0}")]
    InvalidUndoState(Box<str>),
    #[error("invalid search pattern: {0}")]
    InvalidSearchPattern(Box<str>),
}

#[derive(Debug, Clone)]
pub struct Editor {
    clean_frame_text: Option<FrameText>,
    frame_text: FrameText,
    revision: DocumentRevision,
    position_index: RefCell<LazyLinePositionIndex>,
    selections: SelectionSet,
    mode: Mode,
    pending_keys: Vec<KeyEvent>,
    state: EditorState,
    insert_group: Option<UndoGroup>,
    insert_style: InsertStyle,
    insert_capture: String,
    pending_clipboard_writes: Vec<(char, Box<str>)>,
    recording_macro: Option<char>,
    recording_keys: Vec<KeyEvent>,
    last_macro: Option<char>,
    macro_depth: u8,
    pending_change: Option<Command>,
    replaying_change: bool,
    #[cfg(any(test, feature = "conformance"))]
    last_visual: Option<VisualSelection>,
    jumps: LocationHistory,
    changes: LocationHistory,
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

impl Editor {
    #[must_use]
    pub fn new(text: DefaultText) -> Self {
        let frame_text = FrameText::from_store(text.snapshot());
        Self {
            clean_frame_text: Some(frame_text.clone()),
            frame_text,
            revision: DocumentRevision::new(0),
            position_index: RefCell::default(),
            selections: SelectionSet { primary: 0, ranges: vec![SelRange { anchor: 0, head: 0 }] },
            mode: Mode::Normal,
            pending_keys: Vec::with_capacity(8),
            state: EditorState { undo: Vec::with_capacity(64), redo: Vec::with_capacity(64), undo_branches: Vec::with_capacity(8), ..EditorState::default() },
            insert_group: None,
            insert_style: InsertStyle::Insert,
            insert_capture: String::with_capacity(64),
            pending_clipboard_writes: Vec::with_capacity(4),
            recording_macro: None,
            recording_keys: Vec::with_capacity(64),
            last_macro: None,
            macro_depth: 0,
            pending_change: None,
            replaying_change: false,
            #[cfg(any(test, feature = "conformance"))]
            last_visual: None,
            jumps: LocationHistory::new(64, usize::MAX, None),
            changes: LocationHistory::new(64, 100, Some(0)),
            last_find: None,
            messages: Vec::with_capacity(8),
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
            visual_region_history: Vec::with_capacity(16),
        }
    }

    pub fn set_search_options(&mut self, ignore_case: bool, smart_case: bool) {
        self.ignore_case = ignore_case;
        self.smart_case = smart_case;
        if let Some(search) = &mut self.state.search
            && let Ok(compiled) = VimPattern::compile(&search.pattern, ignore_case, smart_case, CaseOverride::Default)
        {
            search.compiled = compiled;
            search.cache = RefCell::default();
        }
    }

    pub const fn set_clipboard_unnamed(&mut self, enabled: bool) {
        self.clipboard_unnamed = enabled;
    }

    pub const fn set_indent_options(&mut self, expand_tab: bool, tab_stop: usize, shift_width: usize, smart_indent: bool) {
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
    #[cfg(any(test, feature = "conformance"))]
    pub fn selections(&self) -> &SelectionSet {
        &self.selections
    }

    pub fn pending_parse_state(&self) -> Option<ParseState> {
        if self.pending_keys.is_empty() {
            return None;
        }
        match Grammar.parse(&self.pending_keys) {
            ParseResult::Pending(state) => Some(state),
            ParseResult::Command(_) | ParseResult::Invalid(_) => None,
        }
    }

    pub fn pending_register_name(&self) -> Option<char> {
        self.pending_keys.windows(2).find_map(|keys| {
            (keys[0].modifiers.is_empty() && keys[0].code == KeyCode::Char('"')).then_some(keys[1]).filter(|key| key.modifiers.is_empty()).and_then(|key| {
                match key.code {
                    KeyCode::Char(name) => Some(name),
                    _ => None,
                }
            })
        })
    }

    #[must_use]
    pub fn text(&self) -> &DefaultText {
        self.frame_text.store()
    }

    #[must_use]
    pub fn contents(&self) -> String {
        self.frame_text.materialize_for_task().to_string()
    }

    #[must_use]
    #[cfg(test)]
    pub fn undo_depth(&self) -> usize {
        self.state.undo.len() + usize::from(self.insert_group.as_ref().is_some_and(|group| !group.is_empty()))
    }

    #[must_use]
    #[cfg(any(test, feature = "conformance"))]
    pub fn redo_depth(&self) -> usize {
        self.state.redo.len()
    }

    #[must_use]
    #[cfg(any(test, feature = "conformance"))]
    pub fn undo_tree_len(&self) -> usize {
        self.state.undo.len() + self.state.redo.len() + self.state.undo_branches.iter().map(Vec::len).sum::<usize>()
    }

    pub fn durable_undo_state(&mut self) -> DurableUndoState {
        self.finish_insert_group();
        DurableUndoState { undo: self.state.undo.clone(), redo: self.state.redo.clone(), branches: self.state.undo_branches.clone() }
    }

    pub fn restore(&mut self, mut state: EditorState) -> Result<(), EngineError> {
        state.validate()?;
        for anchor in state.marks.values_mut() {
            anchor.byte = anchor.byte.min(self.frame_text.len());
        }
        self.state = state;
        self.insert_group = None;
        Ok(())
    }

    pub fn state_mut(&mut self) -> &mut EditorState {
        &mut self.state
    }

    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_clean(&mut self) {
        self.clean_frame_text = Some(self.frame_text.clone());
        self.dirty = false;
    }

    pub fn mark_dirty(&mut self) {
        self.clean_frame_text = None;
        self.dirty = true;
    }

    fn refresh_dirty(&mut self) {
        self.dirty = self.clean_frame_text.as_ref().is_none_or(|clean| !self.frame_text.same_snapshot(clean) && self.frame_text != *clean);
        if !self.dirty {
            self.clean_frame_text = Some(self.frame_text.clone());
        }
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
    }

    #[must_use]
    pub fn primary_cursor(&self) -> usize {
        self.selections.ranges.get(self.selections.primary).map_or(0, |range| range.head)
    }

    #[must_use]
    pub fn cursor_line_column(&self) -> (usize, usize) {
        let cursor = self.primary_cursor().min(self.frame_text.len());
        let line = self.frame_text.line_of_byte(cursor);
        let start = self.frame_text.byte_of_line(line);
        let column = self
            .position_index
            .borrow_mut()
            .line(self.frame_text.store(), line)
            .ok()
            .and_then(|index| index.absolute_byte_to_position(cursor).ok())
            .map_or_else(|| frame_character_count(&self.frame_text, start, cursor), |position| position.scalar);
        (line, column)
    }

    #[must_use]
    pub fn frame(&self) -> EngineFrame {
        EngineFrame::new(self.frame_text.clone(), self.primary_cursor())
    }

    #[cfg(any(test, feature = "conformance"))]
    pub fn register(&self, name: char) -> Option<&RegisterValue> {
        self.state.registers.get(&name.to_ascii_lowercase())
    }

    pub fn registers(&self) -> impl Iterator<Item = (char, &RegisterValue)> {
        self.state.registers.iter().map(|(name, value)| (*name, value))
    }

    pub fn set_register(&mut self, name: char, text: impl Into<Box<str>>, linewise: bool) {
        let value = RegisterValue { text: text.into(), linewise };
        self.state.registers.insert(name, value.clone());
        self.state.registers.insert('"', value.clone());
        if self.clipboard_unnamed {
            self.state.registers.insert('+', value);
        }
    }

    /// Returns clipboard register writes produced by editor commands since the
    /// previous drain. Restoring a register from the terminal clipboard does
    /// not create an echoing write.
    pub fn take_clipboard_writes(&mut self) -> Vec<(char, Box<str>)> {
        std::mem::take(&mut self.pending_clipboard_writes)
    }

    pub fn macros(&self) -> impl Iterator<Item = (char, &[KeyEvent])> + '_ {
        self.state.macros.iter().map(|(name, keys)| (*name, keys.as_slice()))
    }

    pub fn durable_repeat_data(&self) -> Option<Vec<u8>> {
        self.state.last_change.as_ref().and_then(|action| serde_json::to_vec(action).ok())
    }

    pub fn set_search(&mut self, pattern: impl Into<Box<str>>, direction: SearchDirection) -> Result<(), EngineError> {
        self.state.set_search(pattern, direction, self.ignore_case, self.smart_case)
    }

    pub fn clear_search(&mut self) {
        self.state.search = None;
    }

    #[must_use]
    #[cfg(any(test, feature = "conformance"))]
    pub const fn last_visual_selection(&self) -> Option<VisualSelection> {
        self.last_visual
    }

    pub fn jumplist(&self) -> impl Iterator<Item = usize> + '_ {
        self.jumps.entries.iter().copied()
    }

    #[must_use]
    #[cfg(any(test, feature = "conformance"))]
    pub const fn jump_index(&self) -> usize {
        self.jumps.index
    }

    #[cfg(any(test, feature = "conformance"))]
    pub fn changelist(&self) -> impl Iterator<Item = usize> + '_ {
        self.changes.entries.iter().copied()
    }

    #[must_use]
    #[cfg(any(test, feature = "conformance"))]
    pub const fn change_index(&self) -> usize {
        self.changes.index
    }

    pub fn navigate_jump(&mut self, backward: bool) -> bool {
        let current = self.primary_cursor();
        let Some(byte) = self.jumps.navigate(backward, Some(current)) else {
            return false;
        };
        self.set_cursor(byte);
        true
    }

    pub fn navigate_change(&mut self, backward: bool) -> bool {
        let Some(byte) = self.changes.navigate(backward, None) else {
            return false;
        };
        self.set_cursor(byte);
        true
    }

    pub fn repeat_find(&mut self, reverse: bool, count: u32) -> bool {
        let Some(motion) = self.last_find else {
            return false;
        };
        let Motion::Find { character, mut forward, till } = motion else {
            return false;
        };
        forward ^= reverse;
        let motion = Motion::Find { character, forward, till };
        let before = self.primary_cursor();
        self.move_cursor(motion, count.max(1));
        self.primary_cursor() != before
    }

    pub fn adjust_number(&mut self, delta: i64) -> Result<Option<Transaction>, EngineError> {
        self.ensure_writable()?;
        let cursor = self.primary_cursor().min(self.frame_text.len());
        let line_start = frame_line_start(&self.frame_text, cursor);
        let line_end = frame_line_end(&self.frame_text, cursor);
        let mut start = if cursor < line_end && self.frame_text.character_at(cursor).is_some_and(|character| character.is_ascii_digit()) {
            cursor
        } else {
            let mut scan = cursor;
            loop {
                if scan >= line_end {
                    return Ok(None);
                }
                if self.frame_text.character_at(scan).is_some_and(|character| character.is_ascii_digit()) {
                    break scan;
                }
                let next = self.frame_text.next_char_boundary(scan);
                if next == scan {
                    return Ok(None);
                }
                scan = next;
            }
        };
        while start > line_start {
            let previous = self.frame_text.previous_char_boundary(start);
            if !self.frame_text.character_at(previous).is_some_and(|character| character.is_ascii_digit()) {
                break;
            }
            start = previous;
        }
        if start > line_start {
            let previous = self.frame_text.previous_char_boundary(start);
            if self.frame_text.character_at(previous) == Some('-') {
                start = previous;
            }
        }
        let digit_start = if self.frame_text.character_at(start) == Some('-') { self.frame_text.next_char_boundary(start) } else { start };
        let mut end = digit_start;
        while end < line_end && self.frame_text.character_at(end).is_some_and(|character| character.is_ascii_digit()) {
            end = self.frame_text.next_char_boundary(end);
        }
        let mut original = String::with_capacity(end.saturating_sub(start));
        let _ = self.frame_text.visit_chunks(start..end, |chunk| {
            original.push_str(chunk);
            std::ops::ControlFlow::<()>::Continue(())
        });
        let Ok(value) = original.parse::<i128>() else {
            return Ok(None);
        };
        let changed = value.saturating_add(i128::from(delta));
        let digit_width = end.saturating_sub(digit_start);
        let replacement = if original.strip_prefix('-').unwrap_or(&original).starts_with('0') && changed >= 0 {
            format!("{changed:0digit_width$}")
        } else {
            changed.to_string()
        };
        let transaction = Transaction::new(self.revision, vec![Edit::new(start..end, replacement)])?;
        self.apply_recorded(transaction.clone(), false)?;
        self.collapse_selection(start);
        if let Some(group) = self.state.undo.last_mut() {
            group.after = self.selections.clone();
        }
        if !self.replaying_change {
            self.state.last_change = Some(RepeatAction::NumberDelta(delta));
        }
        Ok(Some(transaction))
    }

    pub fn last_search(&self) -> Option<(&str, SearchDirection)> {
        self.state.search.as_ref().map(|search| (search.pattern.as_ref(), search.direction))
    }

    #[cfg(any(test, feature = "conformance"))]
    pub fn messages(&self) -> impl Iterator<Item = &str> {
        self.messages.iter().map(AsRef::as_ref)
    }

    pub fn mark(&self, name: char) -> Option<usize> {
        self.state.marks.get(&name).map(|anchor| anchor.byte)
    }

    pub fn marks(&self) -> impl Iterator<Item = (char, usize)> + '_ {
        self.state.marks.iter().map(|(name, anchor)| (*name, anchor.byte))
    }

    pub fn set_cursor(&mut self, byte: usize) {
        let destination = floor_frame_char_boundary(&self.frame_text, byte.min(self.frame_text.len()));
        self.collapse_selection(destination);
    }

    /// Restores a logical source position after a whole-document transform.
    /// The line and character column are clamped to the transformed text.
    pub fn set_cursor_line_column(&mut self, line: usize, column: usize) {
        let line = line.min(self.frame_text.line_of_byte(self.frame_text.len()));
        let start = self.frame_text.byte_of_line(line);
        let destination = self
            .position_index
            .get_mut()
            .line(self.frame_text.store(), line)
            .ok()
            .and_then(|index| index.scalar_to_byte(column).ok())
            .map_or_else(|| frame_nth_character_or_end(&self.frame_text, start, frame_line_end(&self.frame_text, start), column), |byte| start + byte);
        self.set_cursor(destination);
    }

    /// Pre-fault the synchronous single-key navigation path before the first
    /// frame is presented. The temporary motion is fully rolled back; only
    /// allocation capacity and executable pages remain warm for real input.
    pub fn prepare_realtime_navigation(&mut self) {
        if self.mode != Mode::Normal || !self.pending_keys.is_empty() {
            return;
        }
        let selections = self.selections.clone();
        let jumps = self.jumps.clone();
        let _ = self.handle_key(KeyEvent::character('G'));
        self.selections = selections;
        self.jumps = jumps;
    }

    #[must_use]
    pub fn document_end_byte(&self) -> usize {
        self.document_end_destination()
    }

    pub fn set_selection_range(&mut self, range: Range<usize>) {
        let start = floor_frame_char_boundary(&self.frame_text, range.start.min(self.frame_text.len()));
        let end = floor_frame_char_boundary(&self.frame_text, range.end.min(self.frame_text.len()).max(start));
        self.set_primary_selection(start, end);
    }

    /// Enters characterwise Visual mode with an explicit anchor and head.
    /// Pointer selection uses the same selection state as keyboard Visual mode,
    /// so every operator and register observes one canonical range.
    pub fn set_visual_selection(&mut self, anchor: usize, head: usize) {
        self.cancel_pending();
        self.visual_region_history.clear();
        self.mode = Mode::Visual;
        let anchor = floor_frame_char_boundary(&self.frame_text, anchor.min(self.frame_text.len()));
        let head = floor_frame_char_boundary(&self.frame_text, head.min(self.frame_text.len()));
        self.set_primary_selection(anchor, head);
    }

    /// Applies one key and returns every text transaction it produced in
    /// revision order. Most keys produce zero or one transaction; undo,
    /// redo, repeats, and macros can produce a revision chain.
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<TransactionBatch, EngineError> {
        if matches!(self.mode, Mode::Insert | Mode::Replace) {
            if self.recording_macro.is_some() {
                self.recording_keys.push(key);
            }
            return self.handle_insert_key(key).map(transactions);
        }
        if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            return self.handle_visual_key(key).map(transactions);
        }

        if key.code == KeyCode::Escape && key.modifiers.is_empty() {
            self.cancel_pending();
            return Ok(TransactionBatch::new());
        }

        if self.pending_keys.is_empty() && key.modifiers.is_empty() {
            match key.code {
                KeyCode::Char('v') => {
                    self.enter_visual(Mode::Visual);
                    return Ok(TransactionBatch::new());
                }
                KeyCode::Char('V') => {
                    self.enter_visual(Mode::VisualLine);
                    return Ok(TransactionBatch::new());
                }
                _ => {}
            }
        }

        if self.recording_macro.is_some() && key == KeyEvent::character('q') && self.pending_keys.is_empty() {
            self.finish_macro_recording();
            return Ok(TransactionBatch::new());
        }
        if self.recording_macro.is_some() {
            self.recording_keys.push(key);
        }

        let Some(command) = self.parse_key(key)? else {
            return Ok(TransactionBatch::new());
        };
        self.pending_keys.clear();
        self.execute(command)
    }

    fn parse_key(&mut self, key: KeyEvent) -> Result<Option<Command>, EngineError> {
        self.pending_keys.push(key);
        match Grammar.parse(&self.pending_keys) {
            ParseResult::Pending(_) => Ok(None),
            ParseResult::Invalid(error) => {
                let sequence = std::mem::take(&mut self.pending_keys).into_boxed_slice();
                Err(EngineError::InvalidGrammar { sequence, reason: error.to_string().into() })
            }
            ParseResult::Command(command) => Ok(Some(command)),
        }
    }

    fn enter_visual(&mut self, mode: Mode) {
        self.cancel_pending();
        self.visual_region_history.clear();
        self.mode = mode;
        let cursor = self.primary_cursor();
        self.set_primary_selection(cursor, cursor);
    }

    fn leave_visual(&mut self, cursor: usize) {
        #[cfg(any(test, feature = "conformance"))]
        {
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
                    end_column: if self.mode == Mode::VisualLine { usize::MAX } else { end_column },
                    linewise: self.mode == Mode::VisualLine,
                });
            }
        }
        self.mode = Mode::Normal;
        self.visual_region_history.clear();
        self.cancel_pending();
        self.collapse_selection(cursor);
    }

    fn handle_visual_key(&mut self, key: KeyEvent) -> Result<Option<Transaction>, EngineError> {
        if self.expand_region_keys && key.code == KeyCode::Char('v') && key.modifiers == Modifiers::CONTROL {
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
                KeyCode::Char('d' | 'x') | KeyCode::Delete => return self.apply_visual_operator(Operator::Delete, None),
                KeyCode::Char('c') => return self.apply_visual_operator(Operator::Change, None),
                KeyCode::Char('y' | 'Y') => return self.apply_visual_operator(Operator::Yank, None),
                KeyCode::Char('>') => return self.apply_visual_operator(Operator::Indent, None),
                KeyCode::Char('<') => return self.apply_visual_operator(Operator::Outdent, None),
                KeyCode::Char('p' | 'P') => {
                    return self.visual_paste(key.code == KeyCode::Char('P'), None);
                }
                _ => {}
            }
        }

        match self.parse_key(key)? {
            None => Ok(None),
            Some(Command::Move { motion, count }) => {
                self.cancel_pending();
                self.move_visual_head(motion, count.get());
                Ok(None)
            }
            Some(command) => {
                let sequence = std::mem::take(&mut self.pending_keys).into_boxed_slice();
                Err(EngineError::InvalidGrammar { sequence, reason: format!("command {command:?} is not valid in visual mode").into() })
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
        let mut candidates = vec![word_object_range(&text, cursor, false), first_line..last_line_end, 0..text.len()];
        candidates.extend(['(', '[', '{', '<'].into_iter().map(|open| bracket_object_range(&text, cursor, open, true)));
        let next = candidates
            .into_iter()
            .filter(|candidate| {
                candidate.start <= current.start && candidate.end >= selected_end && (candidate.start < current.start || candidate.end > selected_end)
            })
            .min_by_key(|candidate| candidate.end.saturating_sub(candidate.start));
        let Some(next) = next else {
            return;
        };
        self.visual_region_history.push(self.selections.clone());
        self.mode = Mode::Visual;
        self.set_primary_selection(next.start, previous_char_boundary(&text, next.end));
    }

    fn move_visual_head(&mut self, motion: Motion, count: u32) {
        let destination = self.cursor_destination(motion, count);
        if let Some(primary) = self.selections.ranges.get_mut(self.selections.primary) {
            primary.head = destination;
        }
    }

    #[must_use]
    pub fn selection_byte_range(&self) -> Range<usize> {
        let Some(selection) = self.selections.ranges.get(self.selections.primary) else {
            return 0..0;
        };
        if self.mode == Mode::VisualLine {
            frame_line_start(&self.frame_text, selection.anchor.min(selection.head))
                ..frame_line_end_with_newline(&self.frame_text, selection.anchor.max(selection.head))
        } else if self.mode == Mode::Visual {
            let start = selection.anchor.min(selection.head);
            let end = self.frame_text.next_char_boundary(selection.anchor.max(selection.head));
            start..end
        } else {
            selection.head..selection.head
        }
    }

    fn apply_visual_operator(&mut self, operator: Operator, register: Option<Register>) -> Result<Option<Transaction>, EngineError> {
        let text = self.contents();
        let range = self.selection_byte_range();
        let linewise = self.mode == Mode::VisualLine;
        let cursor = range.start;
        self.apply_operator_range(operator, register, &text, range, linewise, Some(cursor))
    }

    fn visual_paste(&mut self, _before: bool, register: Option<Register>) -> Result<Option<Transaction>, EngineError> {
        self.ensure_writable()?;
        let Some(value) = self.read_register(register).cloned() else {
            return Ok(None);
        };
        let text = self.contents();
        let range = self.selection_byte_range();
        self.write_delete_register(None, &text[range.clone()], self.mode == Mode::VisualLine);
        self.leave_visual(range.start);
        let transaction = Transaction::new(self.revision, vec![Edit::new(range.clone(), value.text)])?;
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
        let transaction = RealtimeEditBatch::single(cursor..cursor, text).into_transaction(self.revision)?;
        self.insert_capture.push_str(text);
        self.apply_recorded(transaction.clone(), true)?;
        Ok(Some(transaction))
    }

    fn insert_character(&mut self, character: char) -> Result<Option<Transaction>, EngineError> {
        let mut encoded = [0_u8; char::MAX_LEN_UTF8];
        self.insert_text(character.encode_utf8(&mut encoded))
    }

    pub fn apply_transaction(&mut self, transaction: Transaction) -> Result<(), EngineError> {
        self.finish_insert_group();
        self.apply_recorded(transaction, false)
    }

    pub fn undo(&mut self) -> Result<TransactionBatch, EngineError> {
        self.finish_insert_group();
        let Some(group) = self.state.undo.pop() else {
            self.messages.push("already at oldest change".into());
            return Ok(TransactionBatch::new());
        };
        let transactions = self.apply_history(group.inverse.iter().rev())?;
        self.selections = group.before.clone();
        self.state.redo.push(group);
        self.messages.push("undo change".into());
        Ok(transactions)
    }

    pub fn redo(&mut self) -> Result<TransactionBatch, EngineError> {
        self.finish_insert_group();
        let Some(group) = self.state.redo.pop() else {
            self.messages.push("already at newest change".into());
            return Ok(TransactionBatch::new());
        };
        let transactions = self.apply_history(&group.forward)?;
        self.state.undo.push(group);
        self.messages.push("redo change".into());
        Ok(transactions)
    }

    fn apply_history<'a>(&mut self, transactions: impl IntoIterator<Item = &'a Transaction>) -> Result<TransactionBatch, EngineError> {
        let mut applied = TransactionBatch::new();
        for transaction in transactions {
            let mut transaction = transaction.clone();
            transaction.rebase(self.revision);
            self.apply_without_history(&transaction)?;
            applied.push(transaction);
        }
        Ok(applied)
    }

    pub fn search(&mut self, pattern: &str, direction: SearchDirection) -> Result<bool, EngineError> {
        if pattern.is_empty() {
            return Ok(false);
        }
        self.set_search(pattern, direction)?;
        Ok(self.move_to_search_match(direction))
    }

    pub fn preview_search(&self, pattern: &str, direction: SearchDirection, cursor: usize) -> Result<Option<usize>, EngineError> {
        if pattern.is_empty() {
            return Ok(None);
        }
        let compiled = self.compile_search_pattern(pattern, CaseOverride::Default)?;
        Ok(find_search_in_frame_windows(&compiled, &self.frame_text, direction, cursor))
    }

    #[must_use]
    pub fn search_match_ranges(&self, byte_range: Range<usize>, limit: usize) -> Vec<Range<usize>> {
        let Some(search) = &self.state.search else {
            return Vec::new();
        };
        if search.pattern.is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut start = byte_range.start.min(self.frame_text.len());
        while start > 0 && !self.frame_text.is_char_boundary(start) {
            start -= 1;
        }
        let mut end = byte_range.end.min(self.frame_text.len());
        while end > start && !self.frame_text.is_char_boundary(end) {
            end -= 1;
        }
        let context_start = previous_frame_char_boundary(&self.frame_text, start);
        let context_end = next_frame_char_boundary(&self.frame_text, end);
        let text = self.frame_text.slice(context_start..context_end);
        search.match_ranges(&text, self.revision, start..end, context_start, limit)
    }

    pub fn search_next(&mut self, reverse: bool) -> bool {
        let Some(search) = self.state.search.clone() else {
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
        self.move_to_search_match(direction)
    }

    fn move_to_search_match(&mut self, direction: SearchDirection) -> bool {
        let byte = self.state.search.as_ref().and_then(|search| search.find_from_frame(&self.frame_text, self.revision, direction, self.primary_cursor()));
        let Some(byte) = byte else {
            return false;
        };
        let previous = self.primary_cursor();
        if byte != previous {
            self.jumps.push(previous);
        }
        self.collapse_selection(byte);
        true
    }

    pub fn compile_search_pattern(&self, pattern: &str, case_override: CaseOverride) -> Result<VimPattern, EngineError> {
        VimPattern::compile(pattern, self.ignore_case, self.smart_case, case_override).map_err(EngineError::InvalidSearchPattern)
    }

    #[cfg(test)]
    fn search_scan_count(&self) -> usize {
        self.state.search.as_ref().map_or(0, |search| {
            let cache = search.cache.borrow();
            cache.window_scans
        })
    }

    fn handle_insert_key(&mut self, key: KeyEvent) -> Result<Option<Transaction>, EngineError> {
        match key.code {
            KeyCode::Escape => {
                self.leave_insert();
                Ok(None)
            }
            KeyCode::Char(character) if key.modifiers.is_empty() && self.smart_indent && matches!(character, '}' | ']' | ')') => {
                self.insert_smart_closing_delimiter(character)
            }
            KeyCode::Char(character) if key.modifiers.is_empty() => self.insert_character(character),
            KeyCode::Enter => {
                let cursor = self.primary_cursor();
                let start = frame_line_start(&self.frame_text, cursor);
                let mut indent = frame_line_indentation(&self.frame_text, start);
                if self.smart_indent && frame_prefix_ends_with_open_delimiter(&self.frame_text, start, cursor) {
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
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End => {
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
                let start = frame_motion_destination(&self.frame_text, cursor, Motion::WordBackward, 1);
                self.delete_insert_range(start, cursor)
            }
            KeyCode::Char('u') if key.modifiers == Modifiers::CONTROL => {
                let cursor = self.primary_cursor();
                self.delete_insert_range(frame_line_start(&self.frame_text, cursor), cursor)
            }
            _ => Ok(None),
        }
    }

    fn insert_smart_closing_delimiter(&mut self, character: char) -> Result<Option<Transaction>, EngineError> {
        let cursor = self.primary_cursor();
        let start = frame_line_start(&self.frame_text, cursor);
        if !frame_prefix_is_indentation(&self.frame_text, start, cursor) {
            return self.insert_character(character);
        }
        let remove_start = if self.frame_text.character_at(self.frame_text.previous_char_boundary(cursor)) == Some('\t') {
            self.frame_text.previous_char_boundary(cursor)
        } else {
            let mut remove_start = cursor;
            for _ in 0..self.shift_width {
                let previous = self.frame_text.previous_char_boundary(remove_start);
                if previous == remove_start || self.frame_text.character_at(previous) != Some(' ') {
                    break;
                }
                remove_start = previous;
            }
            remove_start
        };
        let mut encoded = [0_u8; char::MAX_LEN_UTF8];
        let inserted: &str = character.encode_utf8(&mut encoded);
        let transaction = RealtimeEditBatch::single(remove_start..cursor, inserted).into_transaction(self.revision)?;
        self.apply_recorded(transaction.clone(), true)?;
        self.insert_capture.push(character);
        Ok(Some(transaction))
    }

    fn execute(&mut self, command: Command) -> Result<TransactionBatch, EngineError> {
        match &command {
            Command::EnterInsert => self.enter_insert_command(InsertStyle::Insert).map(transactions),
            Command::EnterAppend => self.enter_insert_command(InsertStyle::Append).map(transactions),
            Command::EnterInsertAtLineStart => self.enter_insert_command(InsertStyle::LineStart).map(transactions),
            Command::EnterInsertAtLineEnd => self.enter_insert_command(InsertStyle::LineEnd).map(transactions),
            Command::EnterReplace => self.enter_insert_command(InsertStyle::Replace).map(transactions),
            Command::OpenLine { above } => {
                self.ensure_writable()?;
                self.open_line(*above).map(transactions)
            }
            Command::Move { motion, count } => Ok(transactions(self.execute_move(*motion, count.get()))),
            Command::ApplyOperator { operator, motion, count, register, range_kind } => {
                self.execute_operator(&command, *operator, *motion, count.get(), *register, *range_kind).map(transactions)
            }
            Command::DeleteChar { backward, count, register } => {
                self.finish_command_change(&command, |editor| editor.delete_chars(*backward, count.get(), *register)).map(transactions)
            }
            Command::JoinLines { count } => self.finish_command_change(&command, |editor| editor.join_lines(count.get())).map(transactions),
            Command::ReplaceChar { character, count } => {
                self.finish_command_change(&command, |editor| editor.replace_chars(*character, count.get())).map(transactions)
            }
            Command::ToggleCase { count } => self.finish_command_change(&command, |editor| editor.toggle_case(count.get())).map(transactions),
            Command::Paste { before, count, register } => {
                self.finish_command_change(&command, |editor| editor.paste(*before, count.get(), *register)).map(transactions)
            }
            Command::Undo { count } => self.history_count(count.get(), Self::undo),
            Command::Redo { count } => self.history_count(count.get(), Self::redo),
            Command::SearchNext { reverse, count } => {
                for _ in 0..count.get() {
                    if !self.search_next(*reverse) {
                        break;
                    }
                }
                Ok(TransactionBatch::new())
            }
            Command::Repeat { count } => self.repeat_last(count.get()),
            Command::Target { action, target, count } => self.execute_target(*action, *target, count.get()),
        }
    }

    fn execute_target(&mut self, action: TargetAction, target: char, count: u32) -> Result<TransactionBatch, EngineError> {
        match action {
            TargetAction::RecordMacro => {
                self.recording_macro = Some(target.to_ascii_lowercase());
                self.recording_keys.clear();
                Ok(TransactionBatch::new())
            }
            TargetAction::ReplayMacro => {
                let mut transactions = TransactionBatch::new();
                for _ in 0..count {
                    transactions.extend(self.replay_macro(target)?);
                }
                Ok(transactions)
            }
            TargetAction::SetMark => {
                self.state.marks.insert(target, Anchor { byte: self.primary_cursor(), bias: Bias::Left });
                Ok(TransactionBatch::new())
            }
            TargetAction::JumpMark { linewise } => {
                let Some(anchor) = self.state.marks.get(&target).copied() else {
                    return Ok(TransactionBatch::new());
                };
                let destination = if linewise { frame_first_non_blank(&self.frame_text, anchor.byte) } else { anchor.byte };
                if destination != self.primary_cursor() {
                    self.jumps.push(self.primary_cursor());
                }
                self.collapse_selection(destination);
                Ok(TransactionBatch::new())
            }
        }
    }

    fn enter_insert_command(&mut self, style: InsertStyle) -> Result<Option<Transaction>, EngineError> {
        self.ensure_writable()?;
        let destination = match style {
            InsertStyle::Append => {
                let cursor = self.primary_cursor();
                (cursor < frame_line_end(&self.frame_text, cursor)).then(|| self.frame_text.next_char_boundary(cursor))
            }
            InsertStyle::LineStart => Some(frame_first_non_blank(&self.frame_text, self.primary_cursor())),
            InsertStyle::LineEnd => Some(frame_line_end(&self.frame_text, self.primary_cursor())),
            _ => None,
        };
        if let Some(destination) = destination {
            self.collapse_selection(destination);
        }
        self.enter_insert(style);
        if style == InsertStyle::Replace {
            self.mode = Mode::Replace;
        }
        Ok(None)
    }

    fn execute_move(&mut self, motion: Motion, count: u32) -> Option<Transaction> {
        let previous = self.primary_cursor();
        if matches!(motion, Motion::Find { .. }) {
            self.last_find = Some(motion);
        }
        self.move_cursor(motion, count);
        if matches!(motion, Motion::GoToLine | Motion::DocumentEnd | Motion::ParagraphForward | Motion::ParagraphBackward) && self.primary_cursor() != previous
        {
            self.jumps.push(previous);
        }
        None
    }

    fn execute_operator(
        &mut self,
        command: &Command,
        operator: Operator,
        motion: Motion,
        count: u32,
        register: Option<Register>,
        range_kind: RangeKind,
    ) -> Result<Option<Transaction>, EngineError> {
        let result = self.apply_operator(operator, motion, count, register, range_kind)?;
        if result.is_none() || operator == Operator::Yank || self.replaying_change {
            return Ok(result);
        }
        if operator == Operator::Change {
            self.pending_change = Some(command.clone());
        } else {
            self.state.last_change = Some(RepeatAction::Command(command.clone()));
        }
        Ok(result)
    }

    fn finish_command_change(
        &mut self,
        command: &Command,
        operation: impl FnOnce(&mut Self) -> Result<Option<Transaction>, EngineError>,
    ) -> Result<Option<Transaction>, EngineError> {
        let result = operation(self)?;
        if result.is_some() && !self.replaying_change {
            self.state.last_change = Some(RepeatAction::Command(command.clone()));
        }
        Ok(result)
    }

    fn history_count(&mut self, count: u32, operation: fn(&mut Self) -> Result<TransactionBatch, EngineError>) -> Result<TransactionBatch, EngineError> {
        let mut transactions = TransactionBatch::new();
        for _ in 0..count {
            let applied = operation(self)?;
            if applied.is_empty() {
                break;
            }
            transactions.extend(applied);
        }
        Ok(transactions)
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
            let cursor = self.primary_cursor();
            let destination = self.frame_text.previous_char_boundary(cursor).max(frame_line_start(&self.frame_text, cursor));
            self.collapse_selection(destination);
            if let Some(group) = &mut self.insert_group {
                group.after = self.selections.clone();
            }
        }
        self.finish_insert_group();
        if !self.replaying_change {
            if let Some(command) = self.pending_change.take() {
                self.state.last_change = Some(RepeatAction::ChangeInsert { command, text: self.insert_capture.clone().into_boxed_str() });
            } else if !self.insert_capture.is_empty() {
                self.state.last_change = Some(RepeatAction::Insert { style: self.insert_style, text: self.insert_capture.clone().into_boxed_str() });
            }
        }
        self.insert_capture.clear();
    }

    fn finish_insert_group(&mut self) {
        if let Some(group) = self.insert_group.take()
            && !group.is_empty()
        {
            self.state.undo.push(group);
        }
    }

    fn apply_recorded(&mut self, transaction: Transaction, insert_group: bool) -> Result<(), EngineError> {
        self.ensure_writable()?;
        self.ensure_revision(&transaction)?;
        let inverse = self.invert_transaction(&transaction)?;
        let starts_change = if insert_group { self.insert_group.as_ref().is_none_or(UndoGroup::is_empty) } else { true };
        if starts_change && !transaction.is_empty() {
            self.changes.push(transaction.edits()[0].range.start);
        }
        let before = self.selections.clone();
        self.apply_without_history(&transaction)?;
        let after = self.selections.clone();

        if insert_group {
            let group = self.insert_group.get_or_insert_with(|| UndoGroup::new(before.clone()));
            group.forward.push(transaction);
            group.inverse.push(inverse);
            group.after = after;
        } else {
            let mut group = UndoGroup::new(before);
            group.forward.push(transaction);
            group.inverse.push(inverse);
            group.after = after;
            self.state.undo.push(group);
        }
        if !self.state.redo.is_empty() {
            self.state.undo_branches.push(std::mem::take(&mut self.state.redo));
        }
        Ok(())
    }

    fn invert_transaction(&self, transaction: &Transaction) -> Result<Transaction, EngineError> {
        let text = self.frame_text.store();
        let text_len = text.len_bytes();
        let mut deleted = SmallVec::<[Box<str>; 1]>::new();
        for edit in transaction.edits() {
            if edit.range.end > text_len {
                return Err(TransactionError::OutOfBounds { offset: edit.range.end, len: text_len }.into());
            }
            [edit.range.start, edit.range.end]
                .into_iter()
                .try_for_each(|offset| text.is_char_boundary(offset).then_some(()).ok_or(TransactionError::NotCharBoundary { offset }))?;
            deleted.push(text.slice(edit.range.clone()).into_owned().into_boxed_str());
        }
        transaction.invert(&deleted).map_err(Into::into)
    }

    fn apply_without_history(&mut self, transaction: &Transaction) -> Result<(), EngineError> {
        self.ensure_revision(transaction)?;
        let revision = self.revision.next().ok_or(EngineError::RevisionOverflow)?;
        let frame_text = self.frame_text.edited(transaction)?;
        if let Some(search) = &self.state.search {
            search.map_literal_cache_through(transaction, &frame_text, revision);
        }
        self.frame_text = frame_text;
        self.position_index.get_mut().invalidate_transaction(self.frame_text.store(), transaction);
        self.refresh_dirty();
        if self.selections.ranges.len() == 1 {
            let range = &mut self.selections.ranges[0];
            let (anchor_bias, head_bias) = match range.anchor.cmp(&range.head) {
                std::cmp::Ordering::Less => (Bias::Left, Bias::Right),
                std::cmp::Ordering::Greater => (Bias::Right, Bias::Left),
                std::cmp::Ordering::Equal => (Bias::Right, Bias::Right),
            };
            range.anchor = transaction.map_offset(range.anchor, anchor_bias)?;
            range.head = transaction.map_offset(range.head, head_bias)?;
        } else {
            self.selections = self.selections.map_through(transaction)?;
        }
        for anchor in self.state.marks.values_mut() {
            *anchor = (*anchor).map_through(transaction)?;
        }
        for history in [&mut self.jumps, &mut self.changes] {
            for byte in &mut history.entries {
                *byte = transaction.map_offset(*byte, Bias::Left)?;
            }
        }
        self.revision = revision;
        Ok(())
    }

    fn ensure_revision(&self, transaction: &Transaction) -> Result<(), EngineError> {
        (transaction.base_revision() == self.revision)
            .then_some(())
            .ok_or(EngineError::RevisionMismatch { expected: self.revision.get(), actual: transaction.base_revision().get() })
    }

    fn delete_insert_backward(&mut self) -> Result<Option<Transaction>, EngineError> {
        if self.mode == Mode::Replace {
            return self.undo_last_replacement();
        }
        let cursor = self.primary_cursor();
        if cursor == 0 {
            return Ok(None);
        }
        let mut start = cursor.saturating_sub(1);
        while start > 0 && !self.frame_text.is_char_boundary(start) {
            start -= 1;
        }
        self.insert_capture.pop();
        self.delete_insert_range(start, cursor)
    }

    fn replace_insert_text(&mut self, inserted: &str) -> Result<Option<Transaction>, EngineError> {
        if inserted.is_empty() {
            return Ok(None);
        }
        let cursor = self.primary_cursor();
        let mut end = cursor;
        if !inserted.contains('\n') {
            for _ in inserted.chars() {
                if end >= self.frame_text.len() {
                    break;
                }
                let next = self.frame_text.next_char_boundary(end);
                if self.frame_text.character_at(end) == Some('\n') {
                    break;
                }
                end = next;
            }
        }
        let transaction = Transaction::new(self.revision, vec![Edit::new(cursor..end, inserted.to_owned())])?;
        self.insert_capture.push_str(inserted);
        self.apply_recorded(transaction.clone(), true)?;
        Ok(Some(transaction))
    }

    fn undo_last_replacement(&mut self) -> Result<Option<Transaction>, EngineError> {
        let cursor = self.primary_cursor();
        let Some((forward, stored_inverse)) = self
            .insert_group
            .as_ref()
            .and_then(|group| group.forward.last().zip(group.inverse.last()).map(|(forward, inverse)| (forward.clone(), inverse.clone())))
        else {
            return Ok(None);
        };
        let Some(edit) = forward.edits().first() else {
            return Ok(None);
        };
        if edit.range.start.saturating_add(edit.insert.len()) != cursor {
            return Ok(None);
        }
        let destination = edit.range.start;
        let mut inverse = stored_inverse;
        inverse.rebase(self.revision);
        self.apply_without_history(&inverse)?;
        if let Some(group) = &mut self.insert_group {
            group.forward.pop();
            group.inverse.pop();
            group.after = self.selections.clone();
        }
        self.insert_capture.pop();
        self.collapse_selection(destination);
        Ok(Some(inverse))
    }

    fn delete_insert_forward(&mut self) -> Result<Option<Transaction>, EngineError> {
        let cursor = self.primary_cursor();
        self.delete_insert_range(cursor, self.frame_text.next_char_boundary(cursor))
    }

    fn delete_insert_range(&mut self, start: usize, end: usize) -> Result<Option<Transaction>, EngineError> {
        if start == end {
            return Ok(None);
        }
        let transaction = Transaction::new(self.revision, vec![Edit::new(start..end, "")])?;
        self.apply_recorded(transaction.clone(), true)?;
        Ok(Some(transaction))
    }

    fn open_line(&mut self, above: bool) -> Result<Option<Transaction>, EngineError> {
        let cursor = self.primary_cursor();
        let start = frame_line_start(&self.frame_text, cursor);
        let end = frame_line_end(&self.frame_text, cursor);
        let mut indent = frame_line_indentation(&self.frame_text, start);
        if !above && self.smart_indent && frame_prefix_ends_with_open_delimiter(&self.frame_text, start, end) {
            indent.push_str(&" ".repeat(self.shift_width));
        }
        let (position, inserted, destination, style) = if above {
            (start, format!("{indent}\n"), start, InsertStyle::OpenAbove)
        } else if end < self.frame_text.len() {
            let next = self.frame_text.next_char_boundary(end);
            (next, format!("{indent}\n"), next, InsertStyle::OpenBelow)
        } else {
            (end, format!("\n{indent}"), end + 1, InsertStyle::OpenBelow)
        };
        self.enter_insert(style);
        let transaction = Transaction::new(self.revision, vec![Edit::new(position..position, inserted)])?;
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
        if operator == Operator::Change && matches!(motion, Motion::WordForward | Motion::BigWordForward) {
            while range.end > range.start
                && text[..range.end].chars().next_back().is_some_and(char::is_whitespace)
                && text.as_bytes().get(range.end - 1) != Some(&b'\n')
            {
                range.end = previous_char_boundary(&text, range.end);
            }
        }
        let linewise = range_kind == RangeKind::LineWise || motion == Motion::WholeLine;
        self.apply_operator_range(operator, register, &text, range, linewise, None)
    }

    fn apply_operator_range(
        &mut self,
        operator: Operator,
        register: Option<Register>,
        text: &str,
        range: Range<usize>,
        linewise: bool,
        visual_cursor: Option<usize>,
    ) -> Result<Option<Transaction>, EngineError> {
        if range.is_empty() {
            if let Some(cursor) = visual_cursor {
                self.leave_visual(cursor);
            }
            return Ok(None);
        }
        if operator == Operator::Yank {
            self.write_yank_register(register, &text[range.clone()], linewise);
            if let Some(cursor) = visual_cursor {
                self.leave_visual(cursor);
            }
            return Ok(None);
        }
        self.ensure_writable()?;
        if matches!(operator, Operator::Delete | Operator::Change) {
            self.write_delete_register(register, &text[range.clone()], linewise);
        }
        let edit_range = if operator == Operator::Change && linewise && range.end > range.start && text.as_bytes().get(range.end - 1) == Some(&b'\n') {
            range.start..range.end - 1
        } else {
            range
        };
        let edits = match operator {
            Operator::Indent => line_edits(text, edit_range, true, self.shift_width),
            Operator::Outdent => line_edits(text, edit_range, false, self.shift_width),
            Operator::Delete | Operator::Change => vec![Edit::new(edit_range, "")],
            Operator::Yank => Vec::new(),
        };
        if let Some(cursor) = visual_cursor {
            self.leave_visual(cursor);
        }
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

    fn operator_range(&self, text: &str, motion: Motion, count: u32, range_kind: RangeKind) -> Range<usize> {
        let cursor = self.primary_cursor().min(text.len());
        if motion == Motion::WholeLine {
            return whole_line_range(text, cursor, count);
        }
        if let Motion::Inside(object) | Motion::Around(object) = motion {
            return text_object_range(text, cursor, object, matches!(motion, Motion::Around(_)));
        }
        let destination = Self::motion_destination(text, cursor, motion, count);
        if range_kind == RangeKind::LineWise {
            let first = line_start(text, cursor.min(destination));
            let last = line_end_with_newline(text, cursor.max(destination));
            return first..last;
        }
        let start = cursor.min(destination);
        let mut end = cursor.max(destination);
        if matches!(motion, Motion::WordEnd | Motion::BigWordEnd | Motion::Find { forward: true, .. }) && destination >= cursor {
            end = next_char_boundary(text, end);
        }
        start..end
    }

    fn delete_chars(&mut self, backward: bool, count: u32, register: Option<Register>) -> Result<Option<Transaction>, EngineError> {
        let cursor = self.primary_cursor();
        let mut edge = cursor;
        for _ in 0..count {
            edge = if backward {
                self.frame_text.previous_char_boundary(edge)
            } else {
                let next = self.frame_text.next_char_boundary(edge);
                if self.frame_text.character_at(edge) == Some('\n') { edge } else { next }
            };
        }
        let range = edge.min(cursor)..edge.max(cursor);
        if range.is_empty() {
            return Ok(None);
        }
        let deleted = self.frame_text.slice(range.clone()).into_owned();
        self.write_delete_register(register, &deleted, false);
        let transaction = Transaction::new(self.revision, vec![Edit::new(range, "")])?;
        self.apply_recorded(transaction.clone(), false)?;
        Ok(Some(transaction))
    }

    fn join_lines(&mut self, count: u32) -> Result<Option<Transaction>, EngineError> {
        let mut position = frame_line_end(&self.frame_text, self.primary_cursor());
        let mut edits = Vec::new();
        for _ in 0..count.max(1) {
            if position >= self.frame_text.len() || self.frame_text.character_at(position) != Some('\n') {
                break;
            }
            let mut end = self.frame_text.next_char_boundary(position);
            while end < self.frame_text.len() && matches!(self.frame_text.character_at(end), Some(' ' | '\t')) {
                end = self.frame_text.next_char_boundary(end);
            }
            edits.push(Edit::new(position..end, " "));
            position = frame_line_end(&self.frame_text, end);
        }
        if edits.is_empty() {
            return Ok(None);
        }
        let cursor = edits.first().map_or(0, |edit| edit.range.start);
        let transaction = Transaction::new(self.revision, edits)?;
        self.apply_recorded(transaction.clone(), false)?;
        self.finish_recorded_edit(cursor);
        Ok(Some(transaction))
    }

    fn replace_chars(&mut self, character: char, count: u32) -> Result<Option<Transaction>, EngineError> {
        let start = self.primary_cursor();
        let mut end = start;
        let mut replaced = 0;
        while replaced < count && end < self.frame_text.len() {
            let next = self.frame_text.next_char_boundary(end);
            if self.frame_text.character_at(end) == Some('\n') {
                break;
            }
            end = next;
            replaced += 1;
        }
        if start == end {
            return Ok(None);
        }
        let insert: String = std::iter::repeat_n(character, usize::try_from(replaced).unwrap_or_default()).collect();
        let transaction = Transaction::new(self.revision, vec![Edit::new(start..end, insert)])?;
        self.apply_recorded(transaction.clone(), false)?;
        self.finish_recorded_edit(start);
        Ok(Some(transaction))
    }

    fn toggle_case(&mut self, count: u32) -> Result<Option<Transaction>, EngineError> {
        self.ensure_writable()?;
        let mut cursor = self.primary_cursor();
        let mut edits = Vec::new();
        for _ in 0..count {
            if cursor >= self.frame_text.len() {
                break;
            }
            let next = self.frame_text.next_char_boundary(cursor);
            let Some(character) = self.frame_text.character_at(cursor) else {
                break;
            };
            if character == '\n' {
                break;
            }
            let replacement =
                if character.is_uppercase() { character.to_lowercase().collect::<String>() } else { character.to_uppercase().collect::<String>() };
            edits.push(Edit::new(cursor..next, replacement));
            cursor = next;
        }
        if edits.is_empty() {
            return Ok(None);
        }
        let transaction = Transaction::new(self.revision, edits)?;
        self.apply_recorded(transaction.clone(), false)?;
        let destination = frame_normal_cursor_destination(&self.frame_text, cursor);
        self.finish_recorded_edit(destination);
        Ok(Some(transaction))
    }

    fn paste(&mut self, before: bool, count: u32, register: Option<Register>) -> Result<Option<Transaction>, EngineError> {
        let Some(value) = self.read_register(register).cloned() else {
            return Ok(None);
        };
        let cursor = self.primary_cursor();
        let position = if value.linewise {
            if before { frame_line_start(&self.frame_text, cursor) } else { frame_line_end_with_newline(&self.frame_text, cursor) }
        } else if before {
            cursor
        } else {
            self.frame_text.next_char_boundary(cursor).min(frame_line_end(&self.frame_text, cursor))
        };
        let inserted = value.text.repeat(usize::try_from(count).unwrap_or(1));
        let inserted_len = inserted.len();
        let transaction = Transaction::new(self.revision, vec![Edit::new(position..position, inserted)])?;
        self.apply_recorded(transaction.clone(), false)?;
        let destination = if value.linewise {
            frame_first_non_blank(&self.frame_text, position)
        } else if inserted_len == 0 {
            position
        } else {
            self.frame_text.previous_char_boundary(position.saturating_add(inserted_len))
        };
        self.finish_recorded_edit(destination);
        Ok(Some(transaction))
    }

    fn finish_recorded_edit(&mut self, destination: usize) {
        self.collapse_selection(destination);
        if let Some(group) = self.state.undo.last_mut() {
            group.after = self.selections.clone();
        }
    }

    fn repeat_last(&mut self, count: u32) -> Result<TransactionBatch, EngineError> {
        let Some(action) = self.state.last_change.clone() else {
            return Ok(TransactionBatch::new());
        };
        self.replaying_change = true;
        let result = (|| {
            let mut transactions = TransactionBatch::new();
            for _ in 0..count {
                match &action {
                    RepeatAction::Command(command) => {
                        transactions.extend(self.execute(command.clone())?);
                    }
                    RepeatAction::Insert { style, text } => {
                        let transaction = self.repeated_insert(*style, text)?;
                        self.apply_recorded(transaction.clone(), false)?;
                        if let Some(edit) = transaction.edits().first() {
                            let destination = match style {
                                InsertStyle::OpenAbove | InsertStyle::OpenBelow => {
                                    let content_start = edit.range.start + usize::from(edit.insert.starts_with('\n'));
                                    frame_first_non_blank(&self.frame_text, content_start)
                                }
                                _ => self.frame_text.previous_char_boundary(edit.range.start.saturating_add(edit.insert.len())),
                            };
                            self.collapse_selection(destination);
                            if let Some(group) = self.state.undo.last_mut() {
                                group.after = self.selections.clone();
                            }
                        }
                        transactions.push(transaction);
                    }
                    RepeatAction::ChangeInsert { command, text } => {
                        transactions.extend(self.execute(command.clone())?);
                        if self.mode == Mode::Insert {
                            if let Some(transaction) = self.insert_text(text)? {
                                transactions.push(transaction);
                            }
                            self.leave_insert();
                        }
                    }
                    RepeatAction::NumberDelta(delta) => {
                        transactions.extend(self.adjust_number(*delta)?);
                    }
                }
            }
            Ok(transactions)
        })();
        self.replaying_change = false;
        result
    }

    fn position_for_insert(&mut self, style: InsertStyle) {
        let cursor = self.primary_cursor();
        let destination = match style {
            InsertStyle::Insert | InsertStyle::Replace => cursor,
            InsertStyle::Append => self.frame_text.next_char_boundary(cursor).min(frame_line_end(&self.frame_text, cursor)),
            InsertStyle::LineStart => frame_first_non_blank(&self.frame_text, cursor),
            InsertStyle::LineEnd => frame_line_end(&self.frame_text, cursor),
            InsertStyle::OpenAbove => frame_line_start(&self.frame_text, cursor),
            InsertStyle::OpenBelow => frame_line_end_with_newline(&self.frame_text, cursor),
        };
        self.collapse_selection(destination);
    }

    fn repeated_insert(&mut self, style: InsertStyle, inserted: &str) -> Result<Transaction, EngineError> {
        let cursor = self.primary_cursor();
        let (position, content) = match style {
            InsertStyle::OpenAbove => (frame_line_start(&self.frame_text, cursor), format!("{inserted}\n")),
            InsertStyle::OpenBelow => {
                let end = frame_line_end(&self.frame_text, cursor);
                if end < self.frame_text.len() { (self.frame_text.next_char_boundary(end), format!("{inserted}\n")) } else { (end, format!("\n{inserted}")) }
            }
            InsertStyle::Replace => {
                let mut end = cursor;
                for _ in inserted.chars() {
                    if end >= self.frame_text.len() || self.frame_text.character_at(end) == Some('\n') {
                        break;
                    }
                    end = self.frame_text.next_char_boundary(end);
                }
                return Ok(Transaction::new(self.revision, vec![Edit::new(cursor..end, inserted.to_owned())])?);
            }
            _ => {
                self.position_for_insert(style);
                (self.primary_cursor(), inserted.to_owned())
            }
        };
        Ok(Transaction::new(self.revision, vec![Edit::new(position..position, content)])?)
    }

    fn move_cursor(&mut self, motion: Motion, count: u32) {
        let destination = self.cursor_destination(motion, count);
        self.collapse_selection(destination);
    }

    fn cursor_destination(&self, motion: Motion, count: u32) -> usize {
        if motion == Motion::DocumentEnd {
            self.document_end_destination()
        } else {
            frame_normal_cursor_destination(&self.frame_text, frame_motion_destination(&self.frame_text, self.primary_cursor(), motion, count))
        }
    }

    fn document_end_destination(&self) -> usize {
        let start = self.frame_text.byte_of_line(self.frame_text.line_of_byte(self.frame_text.len()));
        let end = self.frame_text.len();
        let mut cursor = self.frame_text.cursor(start);
        while cursor.byte() < end {
            let position = cursor.byte();
            if !matches!(cursor.next(), Some(' ' | '\t')) {
                return position;
            }
        }
        if end > start { self.frame_text.previous_char_boundary(end) } else { start }
    }

    fn motion_destination(text: &str, cursor: usize, motion: Motion, count: u32) -> usize {
        let mut destination = cursor.min(text.len());
        if motion == Motion::LineFirstNonBlank {
            for _ in 1..count.max(1) {
                destination = vertical_motion(text, destination, 1);
            }
            return first_non_blank(text, destination);
        }
        if motion == Motion::Column {
            return byte_at_line_column(text, destination, count.saturating_sub(1) as usize);
        }
        for _ in 0..count {
            destination = match motion {
                Motion::Left => previous_char_boundary(text, destination).max(line_start(text, destination)),
                Motion::Right => next_char_boundary(text, destination).min(line_end(text, destination)),
                Motion::WordBackward | Motion::BigWordBackward => backward_word(text, destination, matches!(motion, Motion::BigWordBackward)),
                Motion::WordForward | Motion::BigWordForward => forward_word(text, destination, matches!(motion, Motion::BigWordForward)),
                Motion::WordEnd | Motion::BigWordEnd => end_word(text, destination, matches!(motion, Motion::BigWordEnd)),
                Motion::WordEndBackward => word_end_backward(text, destination),
                Motion::LineStart => line_start(text, destination),
                Motion::FirstNonBlank | Motion::LineFirstNonBlank => first_non_blank(text, destination),
                Motion::NextLineFirstNonBlank => first_non_blank(text, vertical_motion(text, destination, 1)),
                Motion::PreviousLineFirstNonBlank => first_non_blank(text, vertical_motion(text, destination, -1)),
                Motion::LastNonBlank => last_non_blank(text, destination),
                Motion::Column => byte_at_line_column(text, destination, count.saturating_sub(1) as usize),
                Motion::LineEnd => line_end(text, destination),
                Motion::GoToLine => byte_of_line(text, count.saturating_sub(1) as usize),
                Motion::DocumentEnd => first_non_blank(text, line_start(text, text.len())),
                Motion::WholeLine => line_end_with_newline(text, destination),
                Motion::Up => vertical_motion(text, destination, -1),
                Motion::Down => vertical_motion(text, destination, 1),
                Motion::Find { character, forward, till } => {
                    let found = find_on_line(text, destination, character, forward);
                    match (till, found.cmp(&destination)) {
                        (true, std::cmp::Ordering::Greater) => previous_char_boundary(text, found),
                        (true, std::cmp::Ordering::Less) => next_char_boundary(text, found),
                        _ => found,
                    }
                }
                Motion::ParagraphForward => paragraph_forward(text, destination),
                Motion::ParagraphBackward => paragraph_backward(text, destination),
                Motion::MatchPair => matching_pair(text, destination),
                Motion::Inside(object) | Motion::Around(object) => text_object_range(text, destination, object, matches!(motion, Motion::Around(_))).start,
            };
        }
        floor_char_boundary(text, destination.min(text.len()))
    }

    fn write_register(&mut self, register: Option<Register>, text: &str, linewise: bool) {
        if register == Some(Register::BlackHole) {
            return;
        }
        let value = RegisterValue { text: Box::from(text), linewise };
        self.state.registers.insert('"', value.clone());
        if self.clipboard_unnamed {
            self.state.registers.insert('+', value.clone());
            self.pending_clipboard_writes.push(('+', value.text.clone()));
        }
        if let Some(name) = register_key(register) {
            if name.is_ascii_uppercase() {
                let key = name.to_ascii_lowercase();
                self.state
                    .registers
                    .entry(key)
                    .and_modify(|existing| {
                        let mut joined = existing.text.to_string();
                        joined.push_str(text);
                        existing.text = joined.into_boxed_str();
                    })
                    .or_insert(value);
            } else {
                self.state.registers.insert(name, value.clone());
                if matches!(name, '+' | '*') && !self.pending_clipboard_writes.iter().any(|(pending, _)| *pending == name) {
                    self.pending_clipboard_writes.push((name, value.text));
                }
            }
        }
    }

    fn write_yank_register(&mut self, register: Option<Register>, text: &str, linewise: bool) {
        if register == Some(Register::BlackHole) {
            return;
        }
        self.write_register(register, text, linewise);
        if register.is_none() || register == Some(Register::Unnamed) {
            self.state.registers.insert('0', RegisterValue { text: text.into(), linewise });
        }
    }

    fn write_delete_register(&mut self, register: Option<Register>, text: &str, linewise: bool) {
        if register == Some(Register::BlackHole) {
            return;
        }
        self.write_register(register, text, linewise);
        let value = RegisterValue { text: text.into(), linewise };
        if linewise || text.contains('\n') {
            for number in (2_u8..=9).rev() {
                if let Some(previous) = self.state.registers.get(&char::from(b'0' + number - 1)).cloned() {
                    self.state.registers.insert(char::from(b'0' + number), previous);
                }
            }
            self.state.registers.insert('1', value);
        } else {
            self.state.registers.insert('-', value);
        }
    }

    fn read_register(&self, register: Option<Register>) -> Option<&RegisterValue> {
        let key = if self.clipboard_unnamed && (register.is_none() || register == Some(Register::Unnamed)) {
            '+'
        } else {
            register_key(register).unwrap_or('"').to_ascii_lowercase()
        };
        self.state.registers.get(&key)
    }

    fn finish_macro_recording(&mut self) {
        if let Some(register) = self.recording_macro.take() {
            let keys = std::mem::take(&mut self.recording_keys);
            let text: String = keys.iter().filter_map(macro_key_character).collect();
            self.state.macros.insert(register, keys);
            self.state.registers.insert(register, RegisterValue { text: text.into_boxed_str(), linewise: false });
        }
    }

    fn replay_macro(&mut self, register: char) -> Result<TransactionBatch, EngineError> {
        let register = if register == '@' { self.last_macro.unwrap_or('@') } else { register.to_ascii_lowercase() };
        let Some(keys) = self.state.macros.get(&register).cloned() else {
            return Ok(TransactionBatch::new());
        };
        if self.macro_depth >= 32 {
            return Err(EngineError::MacroRecursion);
        }
        self.last_macro = Some(register);
        self.macro_depth += 1;
        let result = (|| {
            let mut transactions = TransactionBatch::new();
            for key in keys {
                transactions.extend(self.handle_key(key)?);
            }
            Ok(transactions)
        })();
        self.macro_depth -= 1;
        result
    }

    fn collapse_selection(&mut self, byte: usize) {
        self.set_primary_selection(byte, byte);
    }

    fn set_primary_selection(&mut self, anchor: usize, head: usize) {
        if let Some(primary) = self.selections.ranges.get_mut(self.selections.primary) {
            primary.anchor = anchor;
            primary.head = head;
        }
    }

    fn ensure_writable(&self) -> Result<(), EngineError> {
        if self.read_only { Err(EngineError::ReadOnly) } else { Ok(()) }
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

fn frame_character_count(text: &FrameText, start: usize, end: usize) -> usize {
    let mut cursor = text.cursor(start);
    let mut count = 0;
    while cursor.byte() < end && cursor.next().is_some() {
        count += 1;
    }
    count
}

fn frame_nth_character_or_end(text: &FrameText, start: usize, end: usize, column: usize) -> usize {
    let mut cursor = text.cursor(start);
    for _ in 0..column {
        if cursor.byte() >= end || cursor.next().is_none() {
            return end;
        }
    }
    cursor.byte().min(end)
}

fn frame_line_start(text: &FrameText, byte: usize) -> usize {
    text.byte_of_line(text.line_of_byte(byte.min(text.len())))
}

fn frame_line_end(text: &FrameText, byte: usize) -> usize {
    let start = frame_line_start(text, byte);
    let next = text.byte_of_line(text.line_of_byte(start).saturating_add(1));
    if next > start && text.character_at(text.previous_char_boundary(next)) == Some('\n') { text.previous_char_boundary(next) } else { next }
}

fn frame_line_end_with_newline(text: &FrameText, byte: usize) -> usize {
    let end = frame_line_end(text, byte);
    if text.character_at(end) == Some('\n') { text.next_char_boundary(end) } else { end }
}

fn frame_normal_cursor_destination(text: &FrameText, byte: usize) -> usize {
    let byte = text.floor_char_boundary(byte.min(text.len()));
    let start = frame_line_start(text, byte);
    let end = frame_line_end(text, byte);
    if end > start && byte >= end { text.previous_char_boundary(end) } else { byte }
}

fn frame_first_non_blank(text: &FrameText, byte: usize) -> usize {
    let start = frame_line_start(text, byte);
    let end = frame_line_end(text, byte);
    let mut cursor = text.cursor(start);
    while cursor.byte() < end {
        let position = cursor.byte();
        if !matches!(cursor.next(), Some(' ' | '\t')) {
            return position;
        }
    }
    end
}

fn frame_last_non_blank(text: &FrameText, byte: usize) -> usize {
    let start = frame_line_start(text, byte);
    let end = frame_line_end(text, byte);
    let mut cursor = text.cursor(start);
    let mut last = start;
    while cursor.byte() < end {
        let position = cursor.byte();
        if !matches!(cursor.next(), Some(' ' | '\t')) {
            last = position;
        }
    }
    last
}

fn frame_byte_at_line_column(text: &FrameText, byte: usize, column: usize) -> usize {
    let start = frame_line_start(text, byte);
    let end = frame_line_end(text, byte);
    let destination = frame_nth_character_or_end(text, start, end, column);
    if destination == end { frame_last_non_blank(text, byte) } else { destination }
}

fn frame_vertical_motion(text: &FrameText, byte: usize, direction: i8) -> usize {
    let start = frame_line_start(text, byte);
    let column = frame_character_count(text, start, byte);
    let target_start = if direction < 0 {
        if start == 0 {
            return byte;
        }
        frame_line_start(text, text.previous_char_boundary(start))
    } else {
        let next = frame_line_end_with_newline(text, byte);
        if next >= text.len() {
            return byte;
        }
        next
    };
    frame_nth_character_or_end(text, target_start, frame_line_end(text, target_start), column)
}

fn frame_forward_word(text: &FrameText, byte: usize, big: bool) -> usize {
    let mut cursor = text.cursor(byte);
    let Some(first) = cursor.next() else {
        return cursor.byte();
    };
    let class = word_class(first, big);
    while text.character_at(cursor.byte()).is_some_and(|character| word_class(character, big) == class) {
        let _ = cursor.next();
    }
    while text.character_at(cursor.byte()).is_some_and(char::is_whitespace) {
        let _ = cursor.next();
    }
    cursor.byte()
}

fn frame_backward_word(text: &FrameText, byte: usize, big: bool) -> usize {
    let mut cursor = text.previous_char_boundary(byte.min(text.len()));
    while cursor > 0 && text.character_at(cursor).is_some_and(char::is_whitespace) {
        cursor = text.previous_char_boundary(cursor);
    }
    let class = text.character_at(cursor).map_or(0, |character| word_class(character, big));
    while cursor > 0 {
        let previous = text.previous_char_boundary(cursor);
        if text.character_at(previous).map_or(0, |character| word_class(character, big)) != class {
            break;
        }
        cursor = previous;
    }
    cursor
}

fn frame_end_word(text: &FrameText, byte: usize, big: bool) -> usize {
    let mut cursor = text.floor_char_boundary(byte.min(text.len()));
    while text.character_at(cursor).is_some_and(char::is_whitespace) {
        cursor = text.next_char_boundary(cursor);
    }
    let class = text.character_at(cursor).map_or(0, |character| word_class(character, big));
    while cursor < text.len() {
        let next = text.next_char_boundary(cursor);
        if next >= text.len() || text.character_at(next).map_or(0, |character| word_class(character, big)) != class {
            return cursor;
        }
        cursor = next;
    }
    cursor
}

fn frame_word_end_backward(text: &FrameText, byte: usize) -> usize {
    let mut cursor = text.previous_char_boundary(byte.min(text.len()));
    while cursor > 0 && text.character_at(cursor).is_some_and(char::is_whitespace) {
        cursor = text.previous_char_boundary(cursor);
    }
    cursor
}

fn frame_paragraph_forward(text: &FrameText, byte: usize) -> usize {
    let mut cursor = text.next_char_boundary(byte.min(text.len()));
    let mut previous_newline = false;
    while let Some(character) = text.character_at(cursor) {
        if character == '\n' && previous_newline {
            return cursor;
        }
        previous_newline = character == '\n';
        cursor = text.next_char_boundary(cursor);
    }
    text.len()
}

fn frame_paragraph_backward(text: &FrameText, byte: usize) -> usize {
    let before = frame_line_start(text, byte.min(text.len()));
    let mut cursor = 0;
    let mut previous_newline = false;
    let mut boundary = None;
    while cursor < before.saturating_sub(1) {
        let Some(character) = text.character_at(cursor) else {
            break;
        };
        if character == '\n' && previous_newline {
            boundary = Some(text.next_char_boundary(cursor));
        }
        previous_newline = character == '\n';
        cursor = text.next_char_boundary(cursor);
    }
    let mut cursor = boundary.unwrap_or(0);
    while cursor < before && text.character_at(cursor) == Some('\n') {
        cursor = text.next_char_boundary(cursor);
    }
    cursor
}

fn frame_matching_pair(text: &FrameText, byte: usize) -> usize {
    let line_end = frame_line_end(text, byte);
    let mut cursor = text.cursor(byte.min(text.len()));
    let mut candidate = None;
    while cursor.byte() < line_end {
        let position = cursor.byte();
        let Some(character) = cursor.next() else {
            break;
        };
        if matches!(character, '(' | ')' | '[' | ']' | '{' | '}') {
            candidate = Some((position, character));
            break;
        }
    }
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
        let mut cursor = text.cursor(origin);
        while let Some(current) = cursor.next() {
            if current == open {
                depth = depth.saturating_add(1);
            } else if current == close {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return text.previous_char_boundary(cursor.byte());
                }
            }
        }
    } else {
        let mut cursor = origin;
        loop {
            let Some(current) = text.character_at(cursor) else {
                break;
            };
            if current == close {
                depth = depth.saturating_add(1);
            } else if current == open {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return cursor;
                }
            }
            if cursor == 0 {
                break;
            }
            cursor = text.previous_char_boundary(cursor);
        }
    }
    byte
}

fn frame_find_on_line(text: &FrameText, byte: usize, needle: char, forward: bool) -> usize {
    let start = frame_line_start(text, byte);
    let end = frame_line_end(text, byte);
    if forward {
        let mut cursor = text.cursor(text.next_char_boundary(byte).min(end));
        while cursor.byte() < end {
            let position = cursor.byte();
            if cursor.next() == Some(needle) {
                return position;
            }
        }
    } else {
        let mut cursor = text.floor_char_boundary(byte);
        while cursor > start {
            cursor = text.previous_char_boundary(cursor);
            if text.character_at(cursor) == Some(needle) {
                return cursor;
            }
        }
    }
    byte
}

fn frame_line_indentation(text: &FrameText, start: usize) -> String {
    let end = frame_line_end(text, start);
    let mut cursor = text.cursor(start);
    let mut indent = String::new();
    while cursor.byte() < end {
        match cursor.next() {
            Some(character @ (' ' | '\t')) => indent.push(character),
            _ => break,
        }
    }
    indent
}

fn frame_prefix_is_indentation(text: &FrameText, start: usize, end: usize) -> bool {
    let mut cursor = text.cursor(start);
    while cursor.byte() < end {
        if !matches!(cursor.next(), Some(' ' | '\t')) {
            return false;
        }
    }
    true
}

fn frame_prefix_ends_with_open_delimiter(text: &FrameText, start: usize, end: usize) -> bool {
    let mut cursor = text.floor_char_boundary(end);
    while cursor > start {
        cursor = text.previous_char_boundary(cursor);
        match text.character_at(cursor) {
            Some(' ' | '\t') => {}
            Some(character) => return matches!(character, '{' | '[' | '('),
            None => return false,
        }
    }
    false
}

fn frame_motion_destination(text: &FrameText, cursor: usize, motion: Motion, count: u32) -> usize {
    let mut destination = cursor.min(text.len());
    if motion == Motion::LineFirstNonBlank {
        for _ in 1..count.max(1) {
            destination = frame_vertical_motion(text, destination, 1);
        }
        return frame_first_non_blank(text, destination);
    }
    if motion == Motion::Column {
        return frame_byte_at_line_column(text, destination, count.saturating_sub(1) as usize);
    }
    for _ in 0..count {
        destination = match motion {
            Motion::Left => text.previous_char_boundary(destination).max(frame_line_start(text, destination)),
            Motion::Right => text.next_char_boundary(destination).min(frame_line_end(text, destination)),
            Motion::WordBackward | Motion::BigWordBackward => frame_backward_word(text, destination, matches!(motion, Motion::BigWordBackward)),
            Motion::WordForward | Motion::BigWordForward => frame_forward_word(text, destination, matches!(motion, Motion::BigWordForward)),
            Motion::WordEnd | Motion::BigWordEnd => frame_end_word(text, destination, matches!(motion, Motion::BigWordEnd)),
            Motion::WordEndBackward => frame_word_end_backward(text, destination),
            Motion::LineStart => frame_line_start(text, destination),
            Motion::FirstNonBlank | Motion::LineFirstNonBlank => frame_first_non_blank(text, destination),
            Motion::NextLineFirstNonBlank => frame_first_non_blank(text, frame_vertical_motion(text, destination, 1)),
            Motion::PreviousLineFirstNonBlank => frame_first_non_blank(text, frame_vertical_motion(text, destination, -1)),
            Motion::LastNonBlank => frame_last_non_blank(text, destination),
            Motion::Column => frame_byte_at_line_column(text, destination, count.saturating_sub(1) as usize),
            Motion::LineEnd => frame_line_end(text, destination),
            Motion::GoToLine => text.byte_of_line(count.saturating_sub(1) as usize),
            Motion::DocumentEnd => frame_first_non_blank(text, frame_line_start(text, text.len())),
            Motion::WholeLine => frame_line_end_with_newline(text, destination),
            Motion::Up => frame_vertical_motion(text, destination, -1),
            Motion::Down => frame_vertical_motion(text, destination, 1),
            Motion::Find { character, forward, till } => {
                let found = frame_find_on_line(text, destination, character, forward);
                match (till, found.cmp(&destination)) {
                    (true, std::cmp::Ordering::Greater) => text.previous_char_boundary(found),
                    (true, std::cmp::Ordering::Less) => text.next_char_boundary(found),
                    _ => found,
                }
            }
            Motion::ParagraphForward => frame_paragraph_forward(text, destination),
            Motion::ParagraphBackward => frame_paragraph_backward(text, destination),
            Motion::MatchPair => frame_matching_pair(text, destination),
            // Text objects produce an edit range and remain on the explicitly
            // named cold path until their range builders accept chunk cursors.
            Motion::Inside(object) | Motion::Around(object) => {
                let source = text.materialize_for_cold_path();
                text_object_range(source, destination, object, matches!(motion, Motion::Around(_))).start
            }
        };
    }
    text.floor_char_boundary(destination.min(text.len()))
}

fn previous_char_boundary(text: &str, byte: usize) -> usize {
    text[..floor_char_boundary(text, byte)].char_indices().next_back().map_or(0, |(index, _)| index)
}

fn next_char_boundary(text: &str, byte: usize) -> usize {
    let byte = floor_char_boundary(text, byte);
    text[byte..].chars().next().map_or(byte, |character| byte + character.len_utf8())
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

#[cfg(any(test, feature = "conformance"))]
fn line_column_at(text: &str, byte: usize) -> (usize, usize) {
    let byte = floor_char_boundary(text, byte.min(text.len()));
    let start = line_start(text, byte);
    (text[..byte].bytes().filter(|value| *value == b'\n').count(), text[start..byte].chars().count())
}

fn line_end(text: &str, byte: usize) -> usize {
    let byte = floor_char_boundary(text, byte);
    text[byte..].find('\n').map_or(text.len(), |offset| byte + offset)
}

fn line_end_with_newline(text: &str, byte: usize) -> usize {
    let end = line_end(text, byte);
    if end < text.len() { end + 1 } else { end }
}

fn first_non_blank(text: &str, byte: usize) -> usize {
    let start = line_start(text, byte);
    let end = line_end(text, byte);
    text[start..end].char_indices().find(|(_, character)| !matches!(character, ' ' | '\t')).map_or(end, |(offset, _)| start + offset)
}

fn last_non_blank(text: &str, byte: usize) -> usize {
    let start = line_start(text, byte);
    let end = line_end(text, byte);
    text[start..end].char_indices().rev().find(|(_, character)| !matches!(character, ' ' | '\t')).map_or(start, |(offset, _)| start + offset)
}

fn byte_at_line_column(text: &str, byte: usize, column: usize) -> usize {
    let start = line_start(text, byte);
    let end = line_end(text, byte);
    text[start..end].char_indices().nth(column).map_or_else(|| last_non_blank(text, byte), |(offset, _)| start + offset)
}

fn byte_of_line(text: &str, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    text.match_indices('\n').nth(line - 1).map_or(text.len(), |(offset, _)| offset + 1)
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
    text[target_start..target_end].char_indices().nth(column).map_or(target_end, |(offset, _)| target_start + offset)
}

fn word_class(character: char, big: bool) -> u8 {
    match character {
        character if character.is_whitespace() => 0,
        character if big || character.is_alphanumeric() || character == '_' => 1,
        _ => 2,
    }
}

fn character_at(text: &str, byte: usize) -> Option<char> {
    text.get(byte..)?.chars().next()
}

fn advance_while(text: &str, mut cursor: usize, predicate: impl Fn(char) -> bool) -> usize {
    while character_at(text, cursor).is_some_and(&predicate) {
        cursor = next_char_boundary(text, cursor);
    }
    cursor
}

fn retreat_while(text: &str, mut cursor: usize, predicate: impl Fn(char) -> bool) -> usize {
    while cursor > 0 && character_at(text, cursor).is_some_and(&predicate) {
        cursor = previous_char_boundary(text, cursor);
    }
    cursor
}

fn forward_word(text: &str, byte: usize, big: bool) -> usize {
    let mut characters = text[byte..].char_indices().peekable();
    let Some((_, first)) = characters.next() else {
        return byte;
    };
    let class = word_class(first, big);
    while characters.peek().is_some_and(|(_, character)| word_class(*character, big) == class) {
        characters.next();
    }
    while characters.peek().is_some_and(|(_, character)| character.is_whitespace()) {
        characters.next();
    }
    characters.peek().map_or(text.len(), |(offset, _)| byte + offset)
}

fn backward_word(text: &str, byte: usize, big: bool) -> usize {
    let cursor = retreat_while(text, previous_char_boundary(text, byte.min(text.len())), char::is_whitespace);
    let class = character_at(text, cursor).map_or(0, |character| word_class(character, big));
    let mut cursor = cursor;
    while cursor > 0 {
        let previous = previous_char_boundary(text, cursor);
        if character_at(text, previous).map_or(0, |character| word_class(character, big)) != class {
            break;
        }
        cursor = previous;
    }
    cursor
}

fn end_word(text: &str, byte: usize, big: bool) -> usize {
    let mut cursor = advance_while(text, byte.min(text.len()), char::is_whitespace);
    let class = character_at(text, cursor).map_or(0, |character| word_class(character, big));
    while cursor < text.len() {
        let next = next_char_boundary(text, cursor);
        if next >= text.len() || character_at(text, next).map_or(0, |character| word_class(character, big)) != class {
            return cursor;
        }
        cursor = next;
    }
    cursor
}

fn word_end_backward(text: &str, byte: usize) -> usize {
    retreat_while(text, previous_char_boundary(text, byte.min(text.len())), char::is_whitespace)
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
        text[after..end].find(needle).map_or(byte, |offset| after + offset)
    } else {
        text[start..byte].rfind(needle).map_or(byte, |offset| start + offset)
    }
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
            if !text[end..next].chars().next().is_some_and(char::is_whitespace) {
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
    let left = text[start_line..byte].rfind(quote).map(|offset| start_line + offset);
    let after = next_char_boundary(text, byte).min(end_line);
    let right = text[after..end_line].find(quote).map(|offset| after + offset);
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
            let removable = text[start..end].bytes().take_while(|byte| matches!(byte, b' ' | b'\t')).count();
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

    use super::*;

    fn editor(source: &str) -> Editor {
        Editor::new(DefaultText::from_reader(Cursor::new(source)).expect("load text"))
    }

    fn feed(editor: &mut Editor, keys: &str) {
        for character in keys.chars() {
            editor.handle_key(if character == '\u{1b}' { KeyEvent::plain(KeyCode::Escape) } else { KeyEvent::character(character) }).expect("key is accepted");
        }
    }

    fn assert_two_transaction_revision_chain(transactions: &[Transaction], label: &str) {
        assert_eq!(transactions.len(), 2, "a grouped {label} must expose its complete revision chain");
        assert_eq!(transactions[0].base_revision().next(), Some(transactions[1].base_revision()));
    }

    macro_rules! edit_scenarios {
        ($($name:ident: $source:expr => [$($keys:expr => $expected:expr),+ $(,)?];)+) => {$(
            #[test]
            fn $name() {
                let mut editor = editor($source);
                $(feed(&mut editor, $keys); assert_eq!(editor.contents(), $expected);)+
            }
        )+};
    }

    edit_scenarios! {
        records_and_replays_raw_key_macros: "abc\n" => ["qaix\u{1b}q@a" => "xxabc\n"];
        dot_repeats_insert_and_operator_changes: "ab cd" => ["ix\u{1b}w." => "xab xcd", "0xw." => "ab cd"];
        replace_and_open_line_are_undoable: "abc\n" => ["2rXohello\u{1b}" => "XXc\nhello\n", "u" => "XXc\n"];
        substitute_line_substitute_and_toggle_case_repeat_transactionally: "alpha beta\nsecond\n" => ["sX\u{1b}w2~" => "Xlpha BEta\nsecond\n", "jSline\u{1b}" => "Xlpha BEta\nline\n", "u" => "Xlpha BEta\nsecond\n"];
        dot_repeat_of_open_line_creates_another_line: "one\ntwo\n" => ["oX\u{1b}j." => "one\nX\ntwo\nX\n"];
    }

    #[test]
    fn insert_group_undo_and_redo_are_transactional() {
        let mut editor = editor("ab");
        assert!(!editor.is_dirty());
        feed(&mut editor, "i界x\u{1b}");
        assert_eq!(editor.contents(), "界xab");
        assert!(editor.is_dirty());
        assert_eq!(editor.undo_depth(), 1);
        let undo = editor.handle_key(KeyEvent::character('u')).expect("undo");
        assert_two_transaction_revision_chain(&undo, "undo");
        assert_eq!(editor.contents(), "ab");
        assert!(!editor.is_dirty());
        let redo = editor.handle_key(KeyEvent { code: KeyCode::Char('r'), modifiers: Modifiers::CONTROL }).expect("redo");
        assert_two_transaction_revision_chain(&redo, "redo");
        assert_eq!(editor.contents(), "界xab");
        assert!(editor.is_dirty());
    }

    #[test]
    fn modified_state_tracks_saved_contents_across_history_branches() {
        let mut editor = editor("abc");
        feed(&mut editor, "x");
        assert!(editor.is_dirty());

        feed(&mut editor, "ia\u{1b}");
        assert_eq!(editor.contents(), "abc");
        assert!(!editor.is_dirty(), "identical text is not modified");

        feed(&mut editor, "Ax\u{1b}");
        editor.mark_clean();
        assert!(!editor.is_dirty());
        feed(&mut editor, "Ay\u{1b}u");
        assert_eq!(editor.contents(), "abcx");
        assert!(!editor.is_dirty(), "undo returned to the save point");
        feed(&mut editor, "u");
        assert_eq!(editor.contents(), "abc");
        assert!(editor.is_dirty(), "undo before the save point is modified");
    }

    #[test]
    fn motions_operators_registers_and_paste_work_by_line() {
        let mut editor = editor("alpha beta\nsecond\n");
        feed(&mut editor, "dw");
        assert_eq!(editor.contents(), "beta\nsecond\n");
        assert_eq!(editor.register('"').map(|value| value.text.as_ref()), Some("alpha "));
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
        assert_eq!(editor.contents()[editor.primary_cursor()..].chars().next(), Some(')'));
        feed(&mut editor, "}");
        assert_eq!(editor.cursor_line_column(), (3, 0));
        feed(&mut editor, "{");
        assert_eq!(editor.cursor_line_column(), (2, 0));
    }

    #[test]
    fn search_wraps_and_marks_map_through_edits() {
        let mut editor = editor("one two one\n");
        assert!(editor.search("one", SearchDirection::Forward).expect("search"));
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
        assert!(editor.search("alpha", SearchDirection::Forward).expect("search"));
        assert_eq!(editor.primary_cursor(), 6);
        editor.set_cursor(0);
        assert!(editor.search("BETA", SearchDirection::Forward).expect("search"));
        assert_eq!(editor.primary_cursor(), 12);
        assert!(!editor.search("Beta", SearchDirection::Forward).expect("search"));

        editor.set_clipboard_unnamed(true);
        editor.set_cursor(0);
        feed(&mut editor, "yw");
        assert_eq!(editor.register('+').map(|value| value.text.as_ref()), Some("Alpha "));

        editor.state_mut().set_register('+', "system", false);
        editor.set_cursor(0);
        feed(&mut editor, "p");
        assert_eq!(editor.contents(), "Asystemlpha alpha BETA\n");

        editor.set_cursor(0);
        feed(&mut editor, "\"ayw");
        assert_eq!(editor.register('+').map(|value| value.text.as_ref()), Some("Asystemlpha "), "a named yank still updates the unnamedplus register");
    }

    #[test]
    fn pointer_selection_uses_characterwise_visual_semantics() {
        let mut editor = editor("zero 界 tail\n");
        editor.set_visual_selection(5, 5 + '界'.len_utf8());
        assert_eq!(editor.mode(), Mode::Visual);
        assert_eq!(&editor.contents()[editor.selection_byte_range()], "界 ", "Visual mode includes the character under the selection head");
        feed(&mut editor, "y");
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.register('"').map(|value| value.text.as_ref()), Some("界 "));
    }

    #[test]
    fn unnamedplus_and_star_emit_distinct_terminal_clipboard_writes() {
        let mut editor = editor("alpha beta\n");
        editor.set_clipboard_unnamed(true);
        feed(&mut editor, "\"*yw");
        assert_eq!(editor.take_clipboard_writes(), vec![('+', Box::<str>::from("alpha ")), ('*', Box::<str>::from("alpha "))]);
        assert!(editor.take_clipboard_writes().is_empty());

        editor.state_mut().set_register('+', "clipboard", false);
        editor.set_cursor(0);
        feed(&mut editor, "p");
        assert_eq!(editor.contents(), "aclipboardlpha beta\n");
        assert!(editor.take_clipboard_writes().is_empty(), "pasting a terminal register must not echo it back through OSC 52");
    }

    #[test]
    fn search_patterns_are_regexes_and_invalid_patterns_are_reported() {
        let mut editor = editor("zero beta BETA\n");
        editor.set_search_options(true, true);
        assert!(editor.search(r"b.ta", SearchDirection::Forward).expect("regex search"));
        assert_eq!(editor.primary_cursor(), 5);
        assert!(matches!(editor.search("[", SearchDirection::Forward), Err(EngineError::InvalidSearchPattern(_))));
    }

    #[test]
    fn literal_search_cache_maps_through_edits_and_repairs_changed_context() {
        let mut cached = editor("one two one\n");
        cached.search("one", SearchDirection::Forward).expect("search");
        let end = cached.text().len_bytes();
        assert_eq!(cached.search_match_ranges(0..end, 16).len(), 2);
        assert_eq!(cached.search_match_ranges(0..end, 16).len(), 2);
        assert_eq!(cached.search_scan_count(), 1);

        cached.apply_transaction(Transaction::new(cached.revision(), vec![Edit::new(end..end, "one")]).expect("transaction")).expect("edit");
        let new_end = cached.text().len_bytes();
        assert_eq!(cached.search_match_ranges(0..new_end, 16).len(), 3);
        assert_eq!(cached.search_scan_count(), 1);

        let mut crossing = editor("val_ tail");
        crossing.set_search("value_", SearchDirection::Forward).expect("literal search");
        let end = crossing.text().len_bytes();
        assert!(crossing.search_match_ranges(0..end, 16).is_empty());
        crossing.apply_transaction(Transaction::new(crossing.revision(), vec![Edit::new(3..3, "ue")]).expect("crossing transaction")).expect("crossing edit");
        let end = crossing.text().len_bytes();
        assert_eq!(crossing.search_match_ranges(0..end, 16), vec![0..6]);
        assert_eq!(crossing.search_scan_count(), 1);
    }

    #[test]
    fn regex_search_windows_are_invalidated_after_edits() {
        let mut editor = editor("one two one\n");
        editor.set_search("o.e", SearchDirection::Forward).expect("regex search");
        let end = editor.text().len_bytes();
        assert_eq!(editor.search_match_ranges(0..end, 16).len(), 2);
        editor.apply_transaction(Transaction::new(editor.revision(), vec![Edit::new(end..end, "one")]).expect("transaction")).expect("edit");
        let end = editor.text().len_bytes();
        assert_eq!(editor.search_match_ranges(0..end, 16).len(), 3);
        assert_eq!(editor.search_scan_count(), 2);
    }

    #[test]
    fn search_window_cache_rescans_for_a_larger_limit() {
        let mut editor = editor("one one one");
        editor.set_search("one", SearchDirection::Forward).expect("search");
        let end = editor.text().len_bytes();

        assert_eq!(editor.search_match_ranges(0..end, 1), vec![0..3]);
        assert_eq!(editor.search_scan_count(), 1);
        assert_eq!(editor.search_match_ranges(0..end, 8), vec![0..3, 4..7, 8..11]);
        assert_eq!(editor.search_scan_count(), 2);
        assert_eq!(editor.search_match_ranges(0..end, 2), vec![0..3, 4..7]);
        assert_eq!(editor.search_scan_count(), 2);
    }

    #[test]
    fn search_window_matches_use_text_outside_the_window_for_context() {
        let mut editor = editor("sword\nwordx\nword\n");
        for pattern in ["^word$", r"\<word\>"] {
            editor.set_search(pattern, SearchDirection::Forward).expect("search");
            assert!(editor.search_match_ranges(1..5, 16).is_empty(), "{pattern} must see the leading s");
            assert!(editor.search_match_ranges(6..10, 16).is_empty(), "{pattern} must see the trailing x");
            assert_eq!(editor.search_match_ranges(12..16, 16), vec![12..16], "{pattern} must match the complete word");
        }
    }

    #[test]
    fn backward_search_uses_bounded_chunk_windows() {
        let mut editor = editor("one two one three one\n");
        editor.set_cursor(editor.text().len_bytes());
        editor.search("one", SearchDirection::Backward).expect("backward search");
        assert!(editor.search_next(false));
        assert!(editor.search_next(false));

        let end = editor.text().len_bytes();
        editor.apply_transaction(Transaction::new(editor.revision(), vec![Edit::new(end..end, "one")]).expect("transaction")).expect("edit");
        assert!(editor.search_next(false));
    }

    #[test]
    fn dotfile_indentation_expands_tabs_smartindents_and_shifts_by_two() {
        let mut braces = editor("");
        let mut editor = editor("fn main() {\nbody\n}\n");
        editor.set_indent_options(true, 2, 2, true);
        feed(&mut editor, "A");
        editor.handle_key(KeyEvent::plain(KeyCode::Enter)).expect("smart-indented newline");
        editor.handle_key(KeyEvent::plain(KeyCode::Tab)).expect("expanded tab");
        feed(&mut editor, "x\u{1b}j>>");
        assert_eq!(editor.contents(), "fn main() {\n    x\n  body\n}\n");

        braces.set_indent_options(true, 2, 2, true);
        feed(&mut braces, "ifn() {");
        braces.handle_key(KeyEvent::plain(KeyCode::Enter)).expect("newline inside braces");
        feed(&mut braces, "x");
        braces.handle_key(KeyEvent::plain(KeyCode::Enter)).expect("newline before closing brace");
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
        editor.handle_key(KeyEvent { code: KeyCode::Char('v'), modifiers: Modifiers::CONTROL }).expect("shrink region");
        assert_eq!(editor.selection_byte_range(), 0..3);
    }

    #[test]
    fn dot_repeat_action_survives_durable_round_trip() {
        let mut original = editor("one two");
        feed(&mut original, "A!");
        let durable = original.durable_repeat_data().expect("repeat state");

        let mut restored = editor("alpha\nbeta");
        let mut state = EditorState::default();
        state.set_repeat_data(&durable).expect("restore repeat data");
        restored.restore(state).expect("restore repeat");
        feed(&mut restored, "j.");
        assert_eq!(restored.contents(), "alpha\nbeta!");
        assert!(EditorState::default().set_repeat_data(b"not-json").is_err());
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
        let mut editor_state = EditorState::default();
        editor_state.set_undo(state);
        restored.restore(editor_state).expect("restore undo tree");
        feed(&mut restored, "u");
        assert_eq!(restored.contents(), "abc");
        assert_eq!(restored.undo_tree_len(), 2);
        restored.handle_key(KeyEvent { code: KeyCode::Char('r'), modifiers: Modifiers::CONTROL }).expect("redo restored change");
        assert_eq!(restored.contents(), "Zbc");
    }

    #[test]
    fn exposes_pending_operator_state() {
        let mut editor = editor("abc");
        editor.handle_key(KeyEvent::character('d')).expect("pending delete");
        assert_eq!(editor.pending_parse_state(), Some(ParseState::Operator));
    }

    #[test]
    fn read_only_documents_allow_navigation_but_reject_changes() {
        let mut editor = editor("abc");
        editor.set_read_only(true);
        feed(&mut editor, "l");
        assert_eq!(editor.primary_cursor(), 1);
        assert!(matches!(editor.handle_key(KeyEvent::character('i')), Err(EngineError::ReadOnly)));
        assert!(matches!(editor.handle_key(KeyEvent::character('x')), Err(EngineError::ReadOnly)));
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
    fn visual_character_and_line_operators_are_native_transactions() {
        let mut editor = editor("alpha beta\nsecond\nthird\n");
        feed(&mut editor, "vwd");
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.contents(), "eta\nsecond\nthird\n");
        assert_eq!(editor.register('"').map(|value| value.text.as_ref()), Some("alpha b"));
        feed(&mut editor, "Vjy");
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.register('"').map(|value| value.text.as_ref()), Some("eta\nsecond\n"));
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
        editor.handle_key(KeyEvent::plain(KeyCode::Escape)).expect("normal Escape is a native no-op");
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
        editor.handle_key(KeyEvent::plain(KeyCode::Backspace)).expect("replace backspace");
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

    #[test]
    fn document_end_uses_the_line_index_without_materializing_an_edit() {
        let mut editor = editor("alpha\n  beta\n");
        feed(&mut editor, "ix\u{1b}");
        assert!(!editor.frame_text.is_materialized());

        feed(&mut editor, "G");

        assert_eq!(editor.primary_cursor(), editor.frame_text.len());
        assert!(!editor.frame_text.is_materialized());
    }

    #[test]
    fn realtime_motions_edits_and_search_do_not_materialize_the_document() {
        let source = "alpha 0042 beta (x)\n".repeat(8_192);
        let mut editor = editor(&source);

        editor.set_cursor(source.find("0042").expect("number"));
        editor.adjust_number(1).expect("adjust number");
        feed(&mut editor, "lwje0$fb%k");
        assert!(editor.search("beta", SearchDirection::Forward).expect("search"));
        assert!(editor.search_next(false));
        feed(&mut editor, "iX\u{1b}");

        assert!(!editor.frame_text.is_materialized());
        assert_eq!(editor.frame_text.materialization_count(), 0);
    }

    #[test]
    fn realtime_navigation_preparation_has_no_editor_state_effect() {
        let mut editor = editor("alpha\nbeta\n");
        editor.set_cursor(2);
        let selections = editor.selections().clone();
        let jumps = editor.jumplist().collect::<Vec<_>>();
        let jump_index = editor.jump_index();

        editor.prepare_realtime_navigation();

        assert_eq!(editor.selections(), &selections);
        assert_eq!(editor.jumplist().collect::<Vec<_>>(), jumps);
        assert_eq!(editor.jump_index(), jump_index);
        assert_eq!(editor.mode(), Mode::Normal);
    }
}
