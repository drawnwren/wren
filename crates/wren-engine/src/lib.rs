#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod editor;
mod search;

use std::borrow::Cow;
use std::ops::Range;
use std::sync::{Arc, OnceLock, Weak};

use wren_types::{Edit, Transaction, TransactionError};

pub use editor::{DurableUndoState, Editor, EngineError, InsertStyle, Mode, RegisterValue, SearchDirection, UndoGroup, VisualSelection};
pub use search::{CaseOverride, VimPattern, VimReplacement, resolve_previous_replacement};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFrame {
    pub text: FrameText,
    pub cursor_byte: usize,
}

impl EngineFrame {
    #[must_use]
    pub fn new(text: impl Into<FrameText>, cursor_byte: usize) -> Self {
        Self { text: text.into(), cursor_byte }
    }
}

#[derive(Debug, Clone)]
pub struct FrameText {
    storage: Arc<FrameStorage>,
    line_starts: Arc<[usize]>,
    line_start_shift: Option<LineStartShift>,
    previous_storage: Option<Weak<FrameStorage>>,
    single_line_change: Option<FrameTextChange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineStartShift {
    from_line: usize,
    delta: i128,
}

struct EditedLineIndex {
    text_len: usize,
    starts: Arc<[usize]>,
    shift: Option<LineStartShift>,
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
    Edited { previous: FrameText, edits: Arc<[Edit]> },
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
        line_starts.extend(text.bytes().enumerate().filter_map(|(byte, value)| (value == b'\n').then_some(byte + 1)));
        Self {
            storage: Arc::new(FrameStorage { len: text.len(), source: FrameSource::Original, materialized: OnceLock::from(text) }),
            line_starts: line_starts.into(),
            line_start_shift: None,
            previous_storage: None,
            single_line_change: None,
        }
    }

    pub(crate) fn from_indexed(text: Arc<str>, line_starts: Vec<usize>) -> Self {
        debug_assert_eq!(line_starts.first(), Some(&0));
        debug_assert!(line_starts.windows(2).all(|pair| pair[0] < pair[1]));
        debug_assert!(line_starts.last().is_some_and(|start| *start <= text.len()));
        Self {
            storage: Arc::new(FrameStorage { len: text.len(), source: FrameSource::Original, materialized: OnceLock::from(text) }),
            line_starts: line_starts.into(),
            line_start_shift: None,
            previous_storage: None,
            single_line_change: None,
        }
    }

    /// Build an immutable preview of this snapshot after `transaction`.
    /// The returned value shares unchanged storage and never mutates the text
    /// store that originally produced this frame.
    pub fn edited(&self, transaction: &Transaction) -> Result<Self, TransactionError> {
        self.validate_edit_boundaries(transaction)?;
        let single_line_change = self.single_line_change(transaction);
        if let Some(previous) = self.previous_if_inverse(transaction) {
            return Ok(Self {
                storage: Arc::clone(&previous.storage),
                line_starts: Arc::clone(&previous.line_starts),
                line_start_shift: previous.line_start_shift,
                previous_storage: Some(Arc::downgrade(&self.storage)),
                single_line_change,
            });
        }
        let index = self.edited_line_starts(transaction, single_line_change)?;
        Ok(Self {
            storage: Arc::new(FrameStorage {
                source: FrameSource::Edited { previous: self.clone(), edits: transaction.edits().to_vec().into() },
                materialized: OnceLock::new(),
                len: index.text_len,
            }),
            line_starts: index.starts,
            line_start_shift: index.shift,
            previous_storage: Some(Arc::downgrade(&self.storage)),
            single_line_change,
        })
    }

    fn validate_edit_boundaries(&self, transaction: &Transaction) -> Result<(), TransactionError> {
        for offset in transaction.edits().iter().flat_map(|edit| [edit.range.start, edit.range.end]) {
            if offset > self.len() {
                return Err(TransactionError::OutOfBounds { offset, len: self.len() });
            }
            if !self.is_char_boundary(offset) {
                return Err(TransactionError::NotCharBoundary { offset });
            }
        }
        Ok(())
    }

    fn single_line_change(&self, transaction: &Transaction) -> Option<FrameTextChange> {
        let [edit] = transaction.edits() else {
            return None;
        };
        if edit.insert.contains('\n') || self.slice(edit.range.clone()).contains('\n') {
            return None;
        }
        let line = self.line_of_byte(edit.range.start);
        let old_start = self.byte_of_line(line);
        let next_line = self.byte_of_line(line.saturating_add(1));
        let old_end = if next_line > old_start && self.byte_at(next_line - 1) == Some(b'\n') { next_line - 1 } else { next_line };
        let new_end = old_end.checked_sub(edit.range.len())?.checked_add(edit.insert.len())?;
        Some(FrameTextChange { line, old_start, old_end, new_start: old_start, new_end })
    }

    fn edited_line_starts(&self, transaction: &Transaction, single_line_change: Option<FrameTextChange>) -> Result<EditedLineIndex, TransactionError> {
        let inserted_bytes = transaction.edits().iter().map(|edit| edit.insert.len()).sum::<usize>();
        let deleted_bytes = transaction.edits().iter().map(|edit| edit.range.len()).sum::<usize>();
        let capacity = self.len().checked_add(inserted_bytes).and_then(|len| len.checked_sub(deleted_bytes)).ok_or(TransactionError::OffsetOverflow)?;
        if let Some(change) = single_line_change {
            let inserted = i128::try_from(inserted_bytes).map_err(|_| TransactionError::OffsetOverflow)?;
            let deleted = i128::try_from(deleted_bytes).map_err(|_| TransactionError::OffsetOverflow)?;
            let delta = inserted - deleted;
            let from_line = change.line.saturating_add(1);
            if delta == 0 || from_line >= self.line_starts.len() {
                return Ok(EditedLineIndex { text_len: capacity, starts: Arc::clone(&self.line_starts), shift: self.line_start_shift });
            }
            if self.line_start_shift.is_none_or(|shift| shift.from_line == from_line) {
                let delta = self.line_start_shift.map_or(delta, |shift| shift.delta.saturating_add(delta));
                return Ok(EditedLineIndex {
                    text_len: capacity,
                    starts: Arc::clone(&self.line_starts),
                    shift: (delta != 0).then_some(LineStartShift { from_line, delta }),
                });
            }
        }
        let mut line_starts = Vec::with_capacity(self.line_starts.len());
        line_starts.push(0);
        let mut old_line = 1;
        let mut source_cursor = 0;
        let mut target_len = 0;

        for edit in transaction.edits() {
            let target_start = target_len + (edit.range.start - source_cursor);
            while self.line_start(old_line).is_some_and(|start| start <= edit.range.start) {
                let start = self.line_start(old_line).unwrap_or(edit.range.start);
                line_starts.push(target_start - (edit.range.start - start));
                old_line += 1;
            }
            while self.line_start(old_line).is_some_and(|start| start <= edit.range.end) {
                old_line += 1;
            }
            line_starts.extend(edit.insert.bytes().enumerate().filter_map(|(byte, value)| (value == b'\n').then_some(target_start + byte + 1)));
            target_len = target_start.saturating_add(edit.insert.len());
            source_cursor = edit.range.end;
        }

        let target_start = target_len;
        while let Some(start) = self.line_start(old_line) {
            line_starts.push(target_start + (start - source_cursor));
            old_line += 1;
        }
        debug_assert_eq!(target_start + (self.len() - source_cursor), capacity);
        Ok(EditedLineIndex { text_len: capacity, starts: line_starts.into(), shift: None })
    }

    fn line_start(&self, line: usize) -> Option<usize> {
        let start = self.line_starts.get(line).copied()?;
        let Some(shift) = self.line_start_shift.filter(|shift| line >= shift.from_line) else {
            return Some(start);
        };
        if shift.delta >= 0 {
            start.checked_add(usize::try_from(shift.delta).ok()?)
        } else {
            start.checked_sub(usize::try_from(shift.delta.unsigned_abs()).ok()?)
        }
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
        byte == 0 || byte == self.len() || self.byte_at(byte).is_some_and(|value| value & 0b1100_0000 != 0b1000_0000)
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
            append_intersection(range.clone(), new_cursor..new_cursor.saturating_add(unchanged_len), |relative| {
                previous.append_range(old_cursor.saturating_add(relative.start)..old_cursor.saturating_add(relative.end), target);
            });
            new_cursor = new_cursor.saturating_add(unchanged_len);
            append_intersection(range.clone(), new_cursor..new_cursor.saturating_add(edit.insert.len()), |relative| {
                if let Some(inserted) = edit.insert.get(relative) {
                    target.push_str(inserted);
                }
            });
            new_cursor = new_cursor.saturating_add(edit.insert.len());
            old_cursor = edit.range.end;
        }
        let remaining = previous.len().saturating_sub(old_cursor);
        append_intersection(range, new_cursor..new_cursor.saturating_add(remaining), |relative| {
            previous.append_range(old_cursor.saturating_add(relative.start)..old_cursor.saturating_add(relative.end), target);
        });
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

    fn previous_if_inverse(&self, transaction: &Transaction) -> Option<&FrameText> {
        let FrameSource::Edited { previous, edits } = &self.storage.source else {
            return None;
        };
        if edits.len() != transaction.edit_count() {
            return None;
        }
        let mut delta = 0_i128;
        for (forward, inverse) in edits.iter().zip(transaction.edits()) {
            let start = i128::try_from(forward.range.start).ok()?.checked_add(delta)?;
            let start = usize::try_from(start).ok()?;
            let end = start.checked_add(forward.insert.len())?;
            if inverse.range != (start..end) || inverse.insert.as_ref() != previous.slice(forward.range.clone()).as_ref() {
                return None;
            }
            delta = delta.checked_add(i128::try_from(forward.insert.len()).ok()?)?.checked_sub(i128::try_from(forward.range.len()).ok()?)?;
        }
        Some(previous)
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
        let byte = byte.min(self.len());
        if self.line_start_shift.is_none() {
            return self.line_starts.partition_point(|start| *start <= byte).saturating_sub(1);
        }
        let mut first = 0;
        let mut last = self.line_starts.len();
        while first < last {
            let middle = first + (last - first) / 2;
            if self.line_start(middle).is_some_and(|start| start <= byte) {
                first = middle + 1;
            } else {
                last = middle;
            }
        }
        first.saturating_sub(1)
    }

    #[must_use]
    pub fn byte_of_line(&self, line: usize) -> usize {
        if self.line_start_shift.is_none() {
            return self.line_starts.get(line).copied().unwrap_or(self.len());
        }
        self.line_start(line).unwrap_or(self.len())
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
        Arc::ptr_eq(&self.storage, &other.storage) && Arc::ptr_eq(&self.line_starts, &other.line_starts) && self.line_start_shift == other.line_start_shift
    }

    #[must_use]
    pub fn single_line_change_from(&self, previous: &Self) -> Option<FrameTextChange> {
        self.previous_storage.as_ref().is_some_and(|identity| identity.ptr_eq(&Arc::downgrade(&previous.storage))).then_some(self.single_line_change).flatten()
    }

    #[cfg(test)]
    fn is_materialized(&self) -> bool {
        self.storage.materialized.get().is_some()
    }
}

impl PartialEq for FrameText {
    fn eq(&self, other: &Self) -> bool {
        self.as_ref() == other.as_ref() && self.line_starts == other.line_starts && self.line_start_shift == other.line_start_shift
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

fn append_intersection(wanted: Range<usize>, segment: Range<usize>, mut append: impl FnMut(Range<usize>)) {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

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
        let transaction = Transaction::new(DocumentRevision::new(0), vec![Edit::new(6..10, "βeta")]).expect("transaction");
        let edited = original.edited(&transaction).expect("edited snapshot");

        assert!(!edited.is_materialized());
        assert_eq!(edited.slice(6..11), "βeta");
        assert_eq!(edited.byte_of_line(2), 12);
        assert!(!edited.is_materialized());
        assert_eq!(edited.shared().as_ref(), "alpha\nβeta\ngamma\n");
        assert!(edited.is_materialized());
    }

    #[test]
    fn same_line_edits_share_the_large_line_index() {
        let original = FrameText::from("alpha\nbeta\ngamma\n");
        let first = original.edited(&Transaction::new(DocumentRevision::new(0), vec![Edit::new(1..1, "x")]).expect("first transaction")).expect("first edit");
        assert!(Arc::ptr_eq(&original.line_starts, &first.line_starts));
        assert_eq!(first.byte_of_line(1), 7);
        assert_eq!(first.line_of_byte(7), 1);

        let second = first.edited(&Transaction::new(DocumentRevision::new(1), vec![Edit::new(2..2, "y")]).expect("second transaction")).expect("second edit");
        assert!(Arc::ptr_eq(&original.line_starts, &second.line_starts));
        assert_eq!(second.byte_of_line(2), 13);

        let different_line = second
            .edited(&Transaction::new(DocumentRevision::new(2), vec![Edit::new(9..9, "z")]).expect("different-line transaction"))
            .expect("different-line edit");
        assert!(!Arc::ptr_eq(&original.line_starts, &different_line.line_starts));
        assert_eq!(different_line.slice(0..different_line.len()), "axylpha\nbzeta\ngamma\n");
        assert_eq!(different_line.byte_of_line(2), 14);
    }

    #[test]
    fn lazy_snapshot_preserves_multiple_edits_and_unicode_boundaries() {
        let original = FrameText::from("α one\nβ two\n");
        let transaction = Transaction::new(DocumentRevision::new(0), vec![Edit::new(0..2, "λ"), Edit::new(10..13, "three")]).expect("transaction");
        let edited = original.edited(&transaction).expect("edited snapshot");

        assert_eq!(edited.slice(0..edited.len()), "λ one\nβ three\n");
        assert!(edited.is_char_boundary(2));
        assert!(!edited.is_char_boundary(1));
        assert_eq!(edited.line_of_byte(8), 1);
        assert_eq!(edited.shared().as_ref(), "λ one\nβ three\n");
    }

    #[test]
    fn exact_inverse_reuses_the_previous_snapshot_identity() {
        let original = FrameText::from("large immutable document");
        let forward = Transaction::new(DocumentRevision::new(0), vec![Edit::new(5..5, "!")]).expect("forward transaction");
        let edited = original.edited(&forward).expect("edited snapshot");
        let inverse = forward.inverted_against(original.as_ref()).expect("inverse transaction");
        let restored = edited.edited(&inverse).expect("restored snapshot");

        assert!(restored.same_snapshot(&original));
        assert_eq!(restored.as_ref(), original.as_ref());
    }
}
