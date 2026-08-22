#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use wren_text::TextStore;
use wren_types::{Anchor, ConfigGeneration, DocumentClass, Transaction};

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinePosition {
    pub byte: usize,
    pub scalar: usize,
    pub utf16: usize,
    pub grapheme: usize,
    pub cell: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionIndex {
    base_byte: usize,
    line_bytes: usize,
    checkpoints: Vec<LinePosition>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PositionError {
    #[error("coordinate {value} exceeds the indexed line")]
    OutOfRange { value: usize },
    #[error("byte {byte} is not a UTF-8 scalar boundary")]
    NotScalarBoundary { byte: usize },
    #[error("cell {cell} lies inside a wide grapheme")]
    InsideWideGrapheme { cell: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspPositionEncoding {
    Utf8,
    Utf16,
}

#[must_use]
pub fn negotiate_lsp_position_encoding(peer: &[LspPositionEncoding]) -> LspPositionEncoding {
    if peer.contains(&LspPositionEncoding::Utf8) { LspPositionEncoding::Utf8 } else { LspPositionEncoding::Utf16 }
}

/// Converts one LSP UTF-16 column within a line to its UTF-8 byte offset.
/// This is the shared boundary primitive for semantic tokens, workspace edits,
/// quickfix locations, and cursor navigation.
pub fn utf16_column_to_byte(line: &str, wanted: usize) -> Result<usize, PositionError> {
    let mut utf16 = 0;
    for (byte, character) in line.char_indices() {
        if utf16 == wanted {
            return Ok(byte);
        }
        utf16 += character.len_utf16();
        if utf16 > wanted {
            return Err(PositionError::OutOfRange { value: wanted });
        }
    }
    (utf16 == wanted).then_some(line.len()).ok_or(PositionError::OutOfRange { value: wanted })
}

/// Converts a UTF-8 byte column within a line to an LSP UTF-16 column.
pub fn byte_column_to_utf16(line: &str, byte: usize) -> Result<usize, PositionError> {
    if byte > line.len() {
        return Err(PositionError::OutOfRange { value: byte });
    }
    if !line.is_char_boundary(byte) {
        return Err(PositionError::NotScalarBoundary { byte });
    }
    Ok(line[..byte].encode_utf16().count())
}

pub fn utf16_position_to_byte(text: &str, line_starts: &[usize], line: usize, column: usize) -> Result<usize, PositionError> {
    let start = *line_starts.get(line).ok_or(PositionError::OutOfRange { value: line })?;
    let end = line_starts.get(line.saturating_add(1)).map_or(text.len(), |next| next.saturating_sub(1));
    let relative = utf16_column_to_byte(text.get(start..end).ok_or(PositionError::OutOfRange { value: line })?, column)?;
    Ok(start + relative)
}

#[derive(Debug, Clone, Default)]
pub struct LazyLinePositionIndex {
    lines: BTreeMap<usize, PositionIndex>,
}

impl LazyLinePositionIndex {
    pub fn line<T: TextStore>(&mut self, store: &T, line: usize) -> Result<&PositionIndex, PositionError> {
        if let std::collections::btree_map::Entry::Vacant(entry) = self.lines.entry(line) {
            entry.insert(PositionIndex::from_store_line(store, line)?);
        }
        self.lines.get(&line).ok_or(PositionError::OutOfRange { value: line })
    }

    pub fn invalidate_transaction<T: TextStore>(&mut self, store: &T, transaction: &Transaction) {
        let first_changed_line = transaction.edits().iter().map(|edit| store.line_of_byte(edit.range.start.min(store.len_bytes()))).min();
        if let Some(first_changed_line) = first_changed_line {
            self.lines.retain(|line, _| *line < first_changed_line);
        }
    }

    #[must_use]
    pub fn indexed_lines(&self) -> usize {
        self.lines.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayCheckpoint {
    pub byte_offset: usize,
    pub grapheme_boundary: usize,
    pub display_column: usize,
    pub config_generation: ConfigGeneration,
    pub exact: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayLookup {
    pub checkpoint: DisplayCheckpoint,
    pub scanned_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct DisplayIndex {
    checkpoints: BTreeMap<usize, DisplayCheckpoint>,
    config_generation: ConfigGeneration,
    tab_width: usize,
    exact_scan_budget_bytes: usize,
}

impl DisplayIndex {
    #[must_use]
    pub fn new(config_generation: ConfigGeneration, tab_width: usize) -> Self {
        let origin = DisplayCheckpoint { byte_offset: 0, grapheme_boundary: 0, display_column: 0, config_generation, exact: true };
        Self { checkpoints: BTreeMap::from([(0, origin)]), config_generation, tab_width: tab_width.max(1), exact_scan_budget_bytes: 64 * 1024 }
    }

    pub fn lookup(&mut self, line: &str, byte: usize, config_generation: ConfigGeneration, class: DocumentClass) -> Result<DisplayLookup, PositionError> {
        if config_generation != self.config_generation {
            self.config_generation = config_generation;
            self.checkpoints.clear();
            self.checkpoints.insert(0, DisplayCheckpoint { byte_offset: 0, grapheme_boundary: 0, display_column: 0, config_generation, exact: true });
        }
        if byte > line.len() {
            return Err(PositionError::OutOfRange { value: byte });
        }
        if !line.is_char_boundary(byte) {
            return Err(PositionError::NotScalarBoundary { byte });
        }
        if let Some(checkpoint) = self.checkpoints.get(&byte).copied() {
            return Ok(DisplayLookup { checkpoint, scanned_bytes: 0 });
        }
        let start = self.checkpoints.range(..byte).next_back().map_or(0, |(offset, _)| *offset);
        let base = self.checkpoints.get(&start).copied().ok_or(PositionError::OutOfRange { value: start })?;
        let distance = byte.saturating_sub(start);
        if class == DocumentClass::Pathological && distance > self.exact_scan_budget_bytes {
            let checkpoint = DisplayCheckpoint { byte_offset: byte, grapheme_boundary: byte, display_column: byte, config_generation, exact: false };
            self.checkpoints.insert(byte, checkpoint);
            return Ok(DisplayLookup { checkpoint, scanned_bytes: 0 });
        }
        let mut column = base.display_column;
        let mut grapheme = base.grapheme_boundary;
        for value in line[start..byte].graphemes(true) {
            if value == "\t" {
                column += self.tab_width - (column % self.tab_width);
            } else {
                column += UnicodeWidthStr::width(value);
            }
            grapheme += 1;
        }
        let checkpoint = DisplayCheckpoint { byte_offset: byte, grapheme_boundary: grapheme, display_column: column, config_generation, exact: base.exact };
        self.checkpoints.insert(byte, checkpoint);
        Ok(DisplayLookup { checkpoint, scanned_bytes: distance })
    }

    pub fn invalidate_transaction(&mut self, transaction: &Transaction) {
        if let Some(first) = transaction.edits().iter().map(|edit| edit.range.start).min() {
            self.checkpoints.retain(|offset, _| *offset < first);
            if !self.checkpoints.contains_key(&0) {
                self.checkpoints.insert(
                    0,
                    DisplayCheckpoint { byte_offset: 0, grapheme_boundary: 0, display_column: 0, config_generation: self.config_generation, exact: true },
                );
            }
        }
    }

    #[must_use]
    pub fn checkpoint_count(&self) -> usize {
        self.checkpoints.len()
    }
}

impl PositionIndex {
    #[must_use]
    pub fn new(line: &str, base_byte: usize) -> Self {
        let mut checkpoints = vec![LinePosition { byte: 0, scalar: 0, utf16: 0, grapheme: 0, cell: 0 }];
        let mut scalar = 0;
        let mut utf16 = 0;
        let mut cells = 0;

        for (grapheme_index, (grapheme_byte, grapheme)) in line.grapheme_indices(true).enumerate() {
            for (char_byte, character) in grapheme.char_indices() {
                if char_byte > 0 {
                    checkpoints.push(LinePosition { byte: grapheme_byte + char_byte, scalar, utf16, grapheme: grapheme_index, cell: cells });
                }
                scalar += 1;
                utf16 += character.len_utf16();
            }
            cells += UnicodeWidthStr::width(grapheme);
            checkpoints.push(LinePosition { byte: grapheme_byte + grapheme.len(), scalar, utf16, grapheme: grapheme_index + 1, cell: cells });
        }
        checkpoints.dedup_by_key(|position| position.byte);
        Self { base_byte, line_bytes: line.len(), checkpoints }
    }

    pub fn from_store_line<T: TextStore>(store: &T, line: usize) -> Result<Self, PositionError> {
        let start = store.byte_of_line(line);
        if start > store.len_bytes() || (start == store.len_bytes() && line > store.line_of_byte(start)) {
            return Err(PositionError::OutOfRange { value: line });
        }
        let end = store.byte_of_line(line.saturating_add(1));
        let text = store.slice(start..end);
        Ok(Self::new(&text, start))
    }

    #[must_use]
    pub const fn base_byte(&self) -> usize {
        self.base_byte
    }

    #[must_use]
    pub const fn len_bytes(&self) -> usize {
        self.line_bytes
    }

    pub fn byte_to_position(&self, byte: usize) -> Result<LinePosition, PositionError> {
        match self.checkpoints.binary_search_by_key(&byte, |position| position.byte) {
            Ok(index) => self.checkpoints.get(index).copied().ok_or(PositionError::OutOfRange { value: byte }),
            Err(_) if byte <= self.line_bytes => Err(PositionError::NotScalarBoundary { byte }),
            Err(_) => Err(PositionError::OutOfRange { value: byte }),
        }
    }

    pub fn absolute_byte_to_position(&self, byte: usize) -> Result<LinePosition, PositionError> {
        let relative = byte.checked_sub(self.base_byte).ok_or(PositionError::OutOfRange { value: byte })?;
        self.byte_to_position(relative)
    }

    pub fn anchor_to_position(&self, anchor: Anchor) -> Result<LinePosition, PositionError> {
        self.absolute_byte_to_position(anchor.byte)
    }

    pub fn scalar_to_byte(&self, scalar: usize) -> Result<usize, PositionError> {
        self.lookup_exact(scalar, |position| position.scalar)
    }

    pub fn utf16_to_byte(&self, utf16: usize) -> Result<usize, PositionError> {
        self.lookup_exact(utf16, |position| position.utf16)
    }

    pub fn grapheme_to_byte(&self, grapheme: usize) -> Result<usize, PositionError> {
        self.lookup_exact(grapheme, |position| position.grapheme)
    }

    pub fn cell_to_byte(&self, cell: usize) -> Result<usize, PositionError> {
        let mut previous_cell = 0;
        for position in &self.checkpoints {
            if position.cell == cell {
                return Ok(position.byte);
            }
            if previous_cell < cell && cell < position.cell {
                return Err(PositionError::InsideWideGrapheme { cell });
            }
            previous_cell = position.cell;
        }
        Err(PositionError::OutOfRange { value: cell })
    }

    fn lookup_exact(&self, value: usize, coordinate: impl Fn(&LinePosition) -> usize) -> Result<usize, PositionError> {
        self.checkpoints.iter().find(|position| coordinate(position) == value).map(|position| position.byte).ok_or(PositionError::OutOfRange { value })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use wren_text::DefaultText;
    use wren_types::{DocumentRevision, Edit};

    use super::*;

    #[test]
    fn indexes_scalar_utf16_grapheme_and_cell_coordinates() {
        let index = PositionIndex::new("a👩🏽‍💻界", 100);
        let end = index.byte_to_position("a👩🏽‍💻界".len()).expect("end exists");
        assert_eq!(end.scalar, 6);
        assert_eq!(end.utf16, 9);
        assert_eq!(end.grapheme, 3);
        assert_eq!(end.cell, 5);
        assert_eq!(index.grapheme_to_byte(1), Ok(1));
        assert_eq!(index.absolute_byte_to_position(101).map(|p| p.grapheme), Ok(1));
        assert_eq!(index.cell_to_byte(3), Ok("a👩🏽‍💻".len()));
        assert_eq!(index.cell_to_byte(2), Err(PositionError::InsideWideGrapheme { cell: 2 }));
    }

    #[test]
    fn shared_lsp_conversion_rejects_surrogate_interiors_and_maps_lines() {
        let text = "a👩z\nβeta";
        let starts = [0, "a👩z\n".len()];
        assert_eq!(utf16_column_to_byte("a👩z", 3), Ok("a👩".len()));
        assert!(utf16_column_to_byte("a👩z", 2).is_err());
        assert_eq!(byte_column_to_utf16("a👩z", "a👩".len()), Ok(3));
        assert!(byte_column_to_utf16("a👩z", 2).is_err());
        assert_eq!(utf16_position_to_byte(text, &starts, 1, 1), Ok("a👩z\nβ".len()));
    }

    #[test]
    fn rejects_a_byte_inside_a_scalar() {
        let index = PositionIndex::new("界", 0);
        assert_eq!(index.byte_to_position(1), Err(PositionError::NotScalarBoundary { byte: 1 }));
    }

    #[test]
    fn line_position_indexes_are_built_and_invalidated_lazily() {
        let store = DefaultText::from_reader(Cursor::new("one\n界 two\nthree")).expect("store");
        let mut indexes = LazyLinePositionIndex::default();
        assert_eq!(indexes.indexed_lines(), 0);
        assert_eq!(indexes.line(&store, 1).expect("line").base_byte(), 4);
        assert_eq!(indexes.indexed_lines(), 1);
        indexes.invalidate_transaction(&store, &Transaction::new(DocumentRevision::new(0), vec![Edit::new(0..0, "x")]).expect("transaction"));
        assert_eq!(indexes.indexed_lines(), 0);
    }

    #[test]
    fn pathological_horizontal_jump_never_rescans_a_multimegabyte_prefix() {
        let line = "x".repeat(8 * 1024 * 1024);
        let mut index = DisplayIndex::new(ConfigGeneration::new(1), 4);
        let lookup = index.lookup(&line, 7 * 1024 * 1024, ConfigGeneration::new(1), DocumentClass::Pathological).expect("jump");
        assert_eq!(lookup.scanned_bytes, 0);
        assert!(!lookup.checkpoint.exact);
        assert_eq!(lookup.checkpoint.byte_offset, 7 * 1024 * 1024);
    }

    #[test]
    fn display_checkpoints_track_tabs_and_reset_on_config_change() {
        let mut index = DisplayIndex::new(ConfigGeneration::new(1), 4);
        let lookup = index.lookup("a\t界", "a\t界".len(), ConfigGeneration::new(1), DocumentClass::Normal).expect("display");
        assert_eq!(lookup.checkpoint.display_column, 6);
        assert!(lookup.checkpoint.exact);
        index.lookup("a\t界", 1, ConfigGeneration::new(2), DocumentClass::Normal).expect("new config");
        assert_eq!(index.checkpoint_count(), 2);
        assert_eq!(negotiate_lsp_position_encoding(&[LspPositionEncoding::Utf16]), LspPositionEncoding::Utf16);
        assert_eq!(negotiate_lsp_position_encoding(&[LspPositionEncoding::Utf16, LspPositionEncoding::Utf8]), LspPositionEncoding::Utf8);
    }
}
