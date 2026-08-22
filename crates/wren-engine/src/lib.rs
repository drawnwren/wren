#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod editor;
mod search;

use std::borrow::Cow;
use std::ops::ControlFlow;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use wren_text::{DefaultText, TextStore};
use wren_types::{Transaction, TransactionError};

#[cfg(any(test, feature = "conformance"))]
pub use editor::VisualSelection;
pub use editor::{DurableUndoState, Editor, EditorState, EngineError, InsertStyle, Mode, RegisterValue, SearchDirection, TransactionBatch, UndoGroup};
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
    snapshot: Arc<FrameSnapshot>,
    previous_snapshot: Option<Weak<FrameSnapshot>>,
    single_line_change: Option<FrameTextChange>,
}

/// A positionable, allocation-free reader over a [`FrameText`] snapshot.
///
/// The cursor deliberately exposes byte positions because transactions and
/// selections are byte-addressed.  Its character operations cross piece-tree
/// and rope chunks without requesting a contiguous copy of the document.
#[derive(Debug, Clone, Copy)]
pub struct FrameTextCursor<'a> {
    text: &'a FrameText,
    byte: usize,
}

#[derive(Debug)]
struct FrameSnapshot {
    text: DefaultText,
    materialized: OnceLock<Arc<str>>,
    materializations: AtomicUsize,
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
    pub(crate) fn from_store(text: DefaultText) -> Self {
        Self {
            snapshot: Arc::new(FrameSnapshot { text, materialized: OnceLock::new(), materializations: AtomicUsize::new(0) }),
            previous_snapshot: None,
            single_line_change: None,
        }
    }

    fn following(&self, transaction: &Transaction) -> Self {
        let mut text = self.snapshot.text.clone();
        text.apply(transaction);
        Self {
            snapshot: Arc::new(FrameSnapshot { text, materialized: OnceLock::new(), materializations: AtomicUsize::new(0) }),
            previous_snapshot: Some(Arc::downgrade(&self.snapshot)),
            single_line_change: self.single_line_change(transaction),
        }
    }

    pub fn edited(&self, transaction: &Transaction) -> Result<Self, TransactionError> {
        transaction.validate_boundaries(self.len(), |offset| self.is_char_boundary(offset))?;
        Ok(self.following(transaction))
    }

    pub(crate) fn store(&self) -> &DefaultText {
        &self.snapshot.text
    }

    fn single_line_change(&self, transaction: &Transaction) -> Option<FrameTextChange> {
        let [edit] = transaction.edits() else {
            return None;
        };
        if edit.insert.contains('\n') || self.contains_newline(edit.range.clone()) {
            return None;
        }
        let line = self.line_of_byte(edit.range.start);
        let old_start = self.byte_of_line(line);
        let next_line = self.byte_of_line(line.saturating_add(1));
        let old_end = if next_line > old_start && self.character_at(next_line - 1) == Some('\n') { next_line - 1 } else { next_line };
        let new_end = old_end.checked_sub(edit.range.len())?.checked_add(edit.insert.len())?;
        Some(FrameTextChange { line, old_start, old_end, new_start: old_start, new_end })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.snapshot.text.len_bytes()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.snapshot.text.len_bytes() == 0
    }

    #[must_use]
    pub fn is_char_boundary(&self, byte: usize) -> bool {
        self.snapshot.text.is_char_boundary(byte)
    }

    /// Returns a cursor positioned on the preceding character boundary.
    #[must_use]
    pub fn cursor(&self, byte: usize) -> FrameTextCursor<'_> {
        FrameTextCursor { text: self, byte: self.floor_char_boundary(byte) }
    }

    /// Clamps an offset to the preceding UTF-8 character boundary.
    #[must_use]
    pub fn floor_char_boundary(&self, byte: usize) -> usize {
        let mut byte = byte.min(self.len());
        while byte > 0 && !self.is_char_boundary(byte) {
            byte -= 1;
        }
        byte
    }

    /// Returns the boundary immediately following the character at `byte`.
    #[must_use]
    pub fn next_char_boundary(&self, byte: usize) -> usize {
        let byte = self.floor_char_boundary(byte);
        self.character_at(byte).map_or(byte, |character| byte + character.len_utf8())
    }

    /// Returns the boundary immediately preceding `byte`.
    #[must_use]
    pub fn previous_char_boundary(&self, byte: usize) -> usize {
        let byte = self.floor_char_boundary(byte);
        if byte == 0 {
            return 0;
        }
        let mut previous = byte - 1;
        while previous > 0 && !self.is_char_boundary(previous) {
            previous -= 1;
        }
        previous
    }

    /// Reads one character without requiring the character to live in a
    /// contiguous storage segment.
    #[must_use]
    pub fn character_at(&self, byte: usize) -> Option<char> {
        let byte = self.floor_char_boundary(byte);
        if byte == self.len() {
            return None;
        }
        let mut character = None;
        let _ = self.visit_chunks(byte..byte.saturating_add(char::MAX_LEN_UTF8), |chunk| {
            character = chunk.chars().next();
            ControlFlow::Break(())
        });
        character
    }

    #[must_use]
    pub fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        self.snapshot.text.slice(self.bounded_char_range(range))
    }

    /// Returns a borrowed range only when the active text backend can provide
    /// it without copying. Realtime callers should use [`Self::visit_chunks`]
    /// when a range can cross storage segments.
    #[must_use]
    pub fn contiguous(&self, range: Range<usize>) -> Option<&str> {
        self.snapshot.text.contiguous(self.bounded_char_range(range))
    }

    /// Visits the storage chunks for a bounded range without materializing the
    /// document. This is the realtime text-access boundary.
    pub fn visit_chunks<R>(&self, range: Range<usize>, visit: impl FnMut(&str) -> ControlFlow<R>) -> Option<R> {
        self.snapshot.text.visit_chunks(self.bounded_char_range(range), visit)
    }

    fn contains_newline(&self, range: Range<usize>) -> bool {
        let mut found = false;
        let _ = self.visit_chunks(range, |chunk| {
            if chunk.contains('\n') {
                found = true;
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        found
    }

    fn bounded_char_range(&self, range: Range<usize>) -> Range<usize> {
        let start = self.floor_char_boundary(range.start);
        let mut end = range.end.min(self.len()).max(start);
        while end > start && !self.is_char_boundary(end) {
            end -= 1;
        }
        start..end
    }

    fn materialized(&self) -> &Arc<str> {
        self.snapshot.materialized.get_or_init(|| {
            self.snapshot.materializations.fetch_add(1, Ordering::Relaxed);
            Arc::from(self.snapshot.text.slice(0..self.len()).into_owned())
        })
    }

    #[must_use]
    pub fn line_of_byte(&self, byte: usize) -> usize {
        self.snapshot.text.line_of_byte(byte.min(self.len()))
    }

    #[must_use]
    pub fn byte_of_line(&self, line: usize) -> usize {
        self.snapshot.text.byte_of_line(line)
    }

    #[must_use]
    pub fn materialize_for_task(&self) -> Arc<str> {
        Arc::clone(self.materialized())
    }

    /// Number of complete-document materializations performed for this
    /// immutable frame. Realtime tests use this to enforce the no-copy path.
    #[must_use]
    pub fn materialization_count(&self) -> usize {
        self.snapshot.materializations.load(Ordering::Relaxed)
    }

    /// This is intentionally crate-private: editor commands that have not yet
    /// been ported to chunk cursors must name their cold-path materialization
    /// explicitly instead of receiving it through `Deref` or `AsRef`.
    pub(crate) fn materialize_for_cold_path(&self) -> &str {
        self.materialized()
    }

    /// Returns whether both values refer to the same immutable editor
    /// snapshot. Retained rendering uses this instead of scanning the whole
    /// document merely to prove that revision-local indexes are reusable.
    #[must_use]
    pub fn same_snapshot(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.snapshot, &other.snapshot)
    }

    pub fn single_line_change_from(&self, previous: &Self) -> Option<FrameTextChange> {
        self.previous_snapshot
            .as_ref()
            .is_some_and(|identity| identity.ptr_eq(&Arc::downgrade(&previous.snapshot)))
            .then_some(self.single_line_change)
            .flatten()
    }

    #[cfg(test)]
    fn is_materialized(&self) -> bool {
        self.snapshot.materialized.get().is_some()
    }
}

impl FrameTextCursor<'_> {
    /// Current byte position in the snapshot.
    #[must_use]
    pub const fn byte(self) -> usize {
        self.byte
    }

    /// Moves to the preceding character boundary and returns the character at
    /// the new position, if any.
    pub fn previous(&mut self) -> Option<char> {
        let previous = self.text.previous_char_boundary(self.byte);
        if previous == self.byte {
            return None;
        }
        self.byte = previous;
        self.text.character_at(previous)
    }

    /// Returns the character at the current position and advances past it.
    pub fn next(&mut self) -> Option<char> {
        let character = self.text.character_at(self.byte)?;
        self.byte += character.len_utf8();
        Some(character)
    }

    /// Repositions the cursor on the preceding character boundary.
    pub fn seek(&mut self, byte: usize) {
        self.byte = self.text.floor_char_boundary(byte);
    }
}

impl PartialEq for FrameText {
    fn eq(&self, other: &Self) -> bool {
        self.snapshot.text.content_eq(&other.snapshot.text)
    }
}

impl Eq for FrameText {}

impl<T: Into<Arc<str>>> From<T> for FrameText {
    fn from(text: T) -> Self {
        Self::from_store(DefaultText::from_string(text.into().to_string()))
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
        let transaction = Transaction::new(DocumentRevision::new(0), vec![Edit::new(6..10, "βeta")]).expect("transaction");
        let edited = original.edited(&transaction).expect("edited snapshot");

        assert!(!edited.is_materialized());
        assert_eq!(edited.materialization_count(), 0);
        assert_eq!(edited.slice(6..11), "βeta");
        assert_eq!(edited.byte_of_line(2), 12);
        assert!(!edited.is_materialized());
        assert_eq!(edited.materialize_for_task().as_ref(), "alpha\nβeta\ngamma\n");
        assert!(edited.is_materialized());
        assert_eq!(edited.materialization_count(), 1);
    }

    #[test]
    fn same_line_edits_update_the_persistent_line_index() {
        let original = FrameText::from("alpha\nbeta\ngamma\n");
        let first = original.edited(&Transaction::new(DocumentRevision::new(0), vec![Edit::new(1..1, "x")]).expect("first transaction")).expect("first edit");
        assert_eq!(first.byte_of_line(1), 7);
        assert_eq!(first.line_of_byte(7), 1);

        let second = first.edited(&Transaction::new(DocumentRevision::new(1), vec![Edit::new(2..2, "y")]).expect("second transaction")).expect("second edit");
        assert_eq!(second.byte_of_line(2), 13);

        let different_line = second
            .edited(&Transaction::new(DocumentRevision::new(2), vec![Edit::new(9..9, "z")]).expect("different-line transaction"))
            .expect("different-line edit");
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
        assert_eq!(edited.materialize_for_task().as_ref(), "λ one\nβ three\n");
    }

    #[test]
    fn exact_inverse_restores_the_previous_snapshot_contents() {
        let original = FrameText::from("large immutable document");
        let forward = Transaction::new(DocumentRevision::new(0), vec![Edit::new(5..5, "!")]).expect("forward transaction");
        let edited = original.edited(&forward).expect("edited snapshot");
        let inverse = forward.invert(&[Box::from("")]).expect("inverse transaction");
        let restored = edited.edited(&inverse).expect("restored snapshot");

        assert!(!restored.same_snapshot(&original));
        assert_eq!(restored.materialize_for_task(), original.materialize_for_task());
    }
}
