#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod editor;
mod search;

use std::borrow::Cow;
use std::ops::Range;
use std::sync::{Arc, OnceLock, Weak};

use wren_types::{Edit, Transaction, TransactionError};

pub use editor::{
    DurableUndoState, Editor, EngineError, InsertStyle, Mode, RegisterValue, SearchDirection,
    UndoGroup, VisualSelection,
};
pub use search::{CaseOverride, VimPattern, VimReplacement, resolve_previous_replacement};

/// Minimal deterministic hot-path engine used to validate latency plumbing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EchoEngine {
    text: String,
    cursor: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFrame {
    pub text: FrameText,
    pub cursor_byte: usize,
}

#[derive(Debug, Clone)]
pub struct FrameText {
    storage: Arc<FrameStorage>,
    line_starts: Arc<[usize]>,
    previous_storage: Option<Weak<FrameStorage>>,
    single_line_change: Option<FrameTextChange>,
}

#[derive(Debug)]
struct FrameStorage {
    source: FrameSource,
    materialized: OnceLock<Arc<str>>,
    len: usize,
}

#[derive(Debug)]
enum FrameSource {
    Original,
    Edited {
        previous: FrameText,
        edits: Arc<[Edit]>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTextChange {
    pub line: usize,
    pub old_start: usize,
    pub old_end: usize,
    pub new_start: usize,
    pub new_end: usize,
}

impl FrameText {
    fn new(text: Arc<str>) -> Self {
        let mut line_starts = Vec::with_capacity(text.len().saturating_div(48).saturating_add(1));
        line_starts.push(0);
        line_starts.extend(
            text.bytes()
                .enumerate()
                .filter_map(|(byte, value)| (value == b'\n').then_some(byte + 1)),
        );
        Self {
            storage: Arc::new(FrameStorage {
                len: text.len(),
                source: FrameSource::Original,
                materialized: OnceLock::from(text),
            }),
            line_starts: line_starts.into(),
            previous_storage: None,
            single_line_change: None,
        }
    }

    pub(crate) fn from_indexed(text: Arc<str>, line_starts: Vec<usize>) -> Self {
        debug_assert_eq!(line_starts.first(), Some(&0));
        debug_assert!(line_starts.windows(2).all(|pair| pair[0] < pair[1]));
        debug_assert!(line_starts.last().is_some_and(|start| *start <= text.len()));
        Self {
            storage: Arc::new(FrameStorage {
                len: text.len(),
                source: FrameSource::Original,
                materialized: OnceLock::from(text),
            }),
            line_starts: line_starts.into(),
            previous_storage: None,
            single_line_change: None,
        }
    }

    fn edited(&self, transaction: &Transaction) -> Result<Self, TransactionError> {
        transaction.validate()?;
        for edit in &transaction.edits {
            for offset in [edit.range.start, edit.range.end] {
                if offset > self.len() {
                    return Err(TransactionError::OutOfBounds {
                        offset,
                        len: self.len(),
                    });
                }
                if !self.is_char_boundary(offset) {
                    return Err(TransactionError::NotCharBoundary { offset });
                }
            }
        }
        let single_line_change = transaction.edits.as_slice().first().and_then(|edit| {
            (transaction.edits.len() == 1
                && !edit.insert.contains('\n')
                && !self.slice(edit.range.clone()).contains('\n'))
            .then(|| {
                let line = self.line_of_byte(edit.range.start);
                let old_start = self.byte_of_line(line);
                let mut old_end = self.byte_of_line(line.saturating_add(1));
                if old_end > old_start && self.byte_at(old_end - 1) == Some(b'\n') {
                    old_end -= 1;
                }
                let new_end = old_end
                    .checked_sub(edit.range.len())?
                    .checked_add(edit.insert.len())?;
                Some(FrameTextChange {
                    line,
                    old_start,
                    old_end,
                    new_start: old_start,
                    new_end,
                })
            })
            .flatten()
        });
        let inserted_bytes = transaction
            .edits
            .iter()
            .map(|edit| edit.insert.len())
            .sum::<usize>();
        let deleted_bytes = transaction
            .edits
            .iter()
            .map(|edit| edit.range.len())
            .sum::<usize>();
        let capacity = self
            .len()
            .checked_add(inserted_bytes)
            .and_then(|len| len.checked_sub(deleted_bytes))
            .ok_or(TransactionError::OffsetOverflow)?;
        let mut line_starts = Vec::with_capacity(self.line_starts.len());
        line_starts.push(0);
        let mut old_line = 1;
        let mut source_cursor = 0;
        let mut target_len = 0;

        for edit in &transaction.edits {
            let target_start = target_len + (edit.range.start - source_cursor);
            while self
                .line_starts
                .get(old_line)
                .is_some_and(|start| *start <= edit.range.start)
            {
                let start = self.line_starts[old_line];
                line_starts.push(target_start - (edit.range.start - start));
                old_line += 1;
            }
            while self
                .line_starts
                .get(old_line)
                .is_some_and(|start| *start <= edit.range.end)
            {
                old_line += 1;
            }
            line_starts.extend(
                edit.insert.bytes().enumerate().filter_map(|(byte, value)| {
                    (value == b'\n').then_some(target_start + byte + 1)
                }),
            );
            target_len = target_start.saturating_add(edit.insert.len());
            source_cursor = edit.range.end;
        }

        let target_start = target_len;
        while let Some(start) = self.line_starts.get(old_line).copied() {
            line_starts.push(target_start + (start - source_cursor));
            old_line += 1;
        }
        debug_assert_eq!(target_start + (self.len() - source_cursor), capacity);
        Ok(Self {
            storage: Arc::new(FrameStorage {
                source: FrameSource::Edited {
                    previous: self.clone(),
                    edits: transaction.edits.clone().into(),
                },
                materialized: OnceLock::new(),
                len: capacity,
            }),
            line_starts: line_starts.into(),
            previous_storage: Some(Arc::downgrade(&self.storage)),
            single_line_change,
        })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.storage.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn is_char_boundary(&self, byte: usize) -> bool {
        byte == 0
            || byte == self.len()
            || self
                .byte_at(byte)
                .is_some_and(|value| value & 0b1100_0000 != 0b1000_0000)
    }

    #[must_use]
    pub fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        let start = range.start.min(self.len());
        let end = range.end.min(self.len()).max(start);
        if let Some(text) = self.storage.materialized.get() {
            return Cow::Borrowed(text.get(start..end).unwrap_or_default());
        }
        let mut text = String::with_capacity(end.saturating_sub(start));
        self.append_range(start..end, &mut text);
        Cow::Owned(text)
    }

    fn append_range(&self, range: Range<usize>, target: &mut String) {
        if range.is_empty() {
            return;
        }
        if let Some(text) = self.storage.materialized.get() {
            if let Some(slice) = text.get(range) {
                target.push_str(slice);
            }
            return;
        }
        let FrameSource::Edited { previous, edits } = &self.storage.source else {
            return;
        };
        let mut old_cursor = 0_usize;
        let mut new_cursor = 0_usize;
        for edit in edits.iter() {
            let unchanged_len = edit.range.start.saturating_sub(old_cursor);
            append_intersection(
                range.clone(),
                new_cursor..new_cursor.saturating_add(unchanged_len),
                |relative| {
                    previous.append_range(
                        old_cursor.saturating_add(relative.start)
                            ..old_cursor.saturating_add(relative.end),
                        target,
                    );
                },
            );
            new_cursor = new_cursor.saturating_add(unchanged_len);
            append_intersection(
                range.clone(),
                new_cursor..new_cursor.saturating_add(edit.insert.len()),
                |relative| {
                    if let Some(inserted) = edit.insert.get(relative) {
                        target.push_str(inserted);
                    }
                },
            );
            new_cursor = new_cursor.saturating_add(edit.insert.len());
            old_cursor = edit.range.end;
        }
        let remaining = previous.len().saturating_sub(old_cursor);
        append_intersection(
            range,
            new_cursor..new_cursor.saturating_add(remaining),
            |relative| {
                previous.append_range(
                    old_cursor.saturating_add(relative.start)
                        ..old_cursor.saturating_add(relative.end),
                    target,
                );
            },
        );
    }

    fn byte_at(&self, byte: usize) -> Option<u8> {
        if byte >= self.len() {
            return None;
        }
        if let Some(text) = self.storage.materialized.get() {
            return text.as_bytes().get(byte).copied();
        }
        let FrameSource::Edited { previous, edits } = &self.storage.source else {
            return None;
        };
        let mut old_cursor = 0_usize;
        let mut new_cursor = 0_usize;
        for edit in edits.iter() {
            let unchanged_len = edit.range.start.saturating_sub(old_cursor);
            if byte < new_cursor.saturating_add(unchanged_len) {
                return previous.byte_at(old_cursor.saturating_add(byte - new_cursor));
            }
            new_cursor = new_cursor.saturating_add(unchanged_len);
            if byte < new_cursor.saturating_add(edit.insert.len()) {
                return edit.insert.as_bytes().get(byte - new_cursor).copied();
            }
            new_cursor = new_cursor.saturating_add(edit.insert.len());
            old_cursor = edit.range.end;
        }
        previous.byte_at(old_cursor.saturating_add(byte - new_cursor))
    }

    fn materialized(&self) -> &Arc<str> {
        self.storage.materialized.get_or_init(|| {
            let mut text = String::with_capacity(self.len());
            self.append_range(0..self.len(), &mut text);
            debug_assert_eq!(text.len(), self.len());
            Arc::from(text)
        })
    }

    #[must_use]
    pub fn line_of_byte(&self, byte: usize) -> usize {
        self.line_starts
            .partition_point(|start| *start <= byte.min(self.len()))
            .saturating_sub(1)
    }

    #[must_use]
    pub fn byte_of_line(&self, line: usize) -> usize {
        self.line_starts.get(line).copied().unwrap_or(self.len())
    }

    #[must_use]
    pub fn shared(&self) -> Arc<str> {
        Arc::clone(self.materialized())
    }

    /// Returns whether both values refer to the same immutable editor
    /// snapshot. Retained rendering uses this instead of scanning the whole
    /// document merely to prove that revision-local indexes are reusable.
    #[must_use]
    pub fn same_snapshot(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.storage, &other.storage)
            && Arc::ptr_eq(&self.line_starts, &other.line_starts)
    }

    #[must_use]
    pub fn single_line_change_from(&self, previous: &Self) -> Option<FrameTextChange> {
        self.previous_storage
            .as_ref()
            .is_some_and(|identity| identity.ptr_eq(&Arc::downgrade(&previous.storage)))
            .then_some(self.single_line_change)
            .flatten()
    }

    #[cfg(test)]
    fn is_materialized(&self) -> bool {
        self.storage.materialized.get().is_some()
    }
}

impl PartialEq for FrameText {
    fn eq(&self, other: &Self) -> bool {
        self.as_ref() == other.as_ref() && self.line_starts == other.line_starts
    }
}

impl Eq for FrameText {}

impl std::ops::Deref for FrameText {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.materialized()
    }
}

impl AsRef<str> for FrameText {
    fn as_ref(&self) -> &str {
        self.materialized()
    }
}

fn append_intersection(
    wanted: Range<usize>,
    segment: Range<usize>,
    mut append: impl FnMut(Range<usize>),
) {
    let start = wanted.start.max(segment.start);
    let end = wanted.end.min(segment.end);
    if start < end {
        append(start - segment.start..end - segment.start);
    }
}

impl From<String> for FrameText {
    fn from(text: String) -> Self {
        Self::new(text.into())
    }
}

impl From<Box<str>> for FrameText {
    fn from(text: Box<str>) -> Self {
        Self::new(text.into())
    }
}

impl From<Arc<str>> for FrameText {
    fn from(text: Arc<str>) -> Self {
        Self::new(text)
    }
}

impl From<&str> for FrameText {
    fn from(text: &str) -> Self {
        Self::new(Arc::from(text))
    }
}

impl EchoEngine {
    pub fn apply_character(&mut self, character: char) {
        self.text.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    pub fn backspace(&mut self) {
        if let Some((start, _)) = self
            .text
            .get(..self.cursor)
            .and_then(|text| text.char_indices().next_back())
        {
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
    }

    #[must_use]
    pub fn frame(&self) -> EngineFrame {
        EngineFrame {
            text: self.text.as_str().into(),
            cursor_byte: self.cursor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FrameText;
    use wren_types::{DocumentRevision, Edit, Transaction};

    #[test]
    fn snapshot_identity_does_not_confuse_equal_independent_text() {
        let snapshot = FrameText::from("same text");
        let cloned = snapshot.clone();
        let independent = FrameText::from("same text");

        assert!(snapshot.same_snapshot(&cloned));
        assert!(!snapshot.same_snapshot(&independent));
        assert_eq!(snapshot, independent);
    }

    #[test]
    fn edited_snapshot_serves_bounded_slices_without_materializing_the_file() {
        let original = FrameText::from("alpha\nbeta\ngamma\n");
        let transaction =
            Transaction::new(DocumentRevision::new(0), vec![Edit::new(6..10, "βeta")])
                .expect("transaction");
        let edited = original.edited(&transaction).expect("edited snapshot");

        assert!(!edited.is_materialized());
        assert_eq!(edited.slice(6..11), "βeta");
        assert_eq!(edited.byte_of_line(2), 12);
        assert!(!edited.is_materialized());
        assert_eq!(edited.shared().as_ref(), "alpha\nβeta\ngamma\n");
        assert!(edited.is_materialized());
    }

    #[test]
    fn lazy_snapshot_preserves_multiple_edits_and_unicode_boundaries() {
        let original = FrameText::from("α one\nβ two\n");
        let transaction = Transaction::new(
            DocumentRevision::new(0),
            vec![Edit::new(0..2, "λ"), Edit::new(10..13, "three")],
        )
        .expect("transaction");
        let edited = original.edited(&transaction).expect("edited snapshot");

        assert_eq!(edited.slice(0..edited.len()), "λ one\nβ three\n");
        assert!(edited.is_char_boundary(2));
        assert!(!edited.is_char_boundary(1));
        assert_eq!(edited.line_of_byte(8), 1);
        assert_eq!(edited.shared().as_ref(), "λ one\nβ three\n");
    }
}
