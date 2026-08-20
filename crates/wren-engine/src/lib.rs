#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod editor;
mod search;

use std::borrow::Cow;
use std::ops::Range;
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

#[derive(Debug)]
struct FrameSnapshot {
    text: DefaultText,
    materialized: OnceLock<Arc<str>>,
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
        Self { snapshot: Arc::new(FrameSnapshot { text, materialized: OnceLock::new() }), previous_snapshot: None, single_line_change: None }
    }

    fn following(&self, transaction: &Transaction) -> Self {
        let mut text = self.snapshot.text.clone();
        text.apply(transaction);
        Self {
            snapshot: Arc::new(FrameSnapshot { text, materialized: OnceLock::new() }),
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
        if edit.insert.contains('\n') || self.slice(edit.range.clone()).contains('\n') {
            return None;
        }
        let line = self.line_of_byte(edit.range.start);
        let old_start = self.byte_of_line(line);
        let next_line = self.byte_of_line(line.saturating_add(1));
        let old_end = if next_line > old_start && self.slice(next_line - 1..next_line) == "\n" { next_line - 1 } else { next_line };
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

    #[must_use]
    pub fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        self.snapshot.text.slice(range.start.min(self.len())..range.end.min(self.len()).max(range.start.min(self.len())))
    }

    fn materialized(&self) -> &Arc<str> {
        self.snapshot.materialized.get_or_init(|| Arc::from(self.snapshot.text.slice(0..self.len()).into_owned()))
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
    pub fn shared(&self) -> Arc<str> {
        Arc::clone(self.materialized())
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

impl PartialEq for FrameText {
    fn eq(&self, other: &Self) -> bool {
        self.snapshot.text.content_eq(&other.snapshot.text)
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
        assert_eq!(edited.slice(6..11), "βeta");
        assert_eq!(edited.byte_of_line(2), 12);
        assert!(!edited.is_materialized());
        assert_eq!(edited.shared().as_ref(), "alpha\nβeta\ngamma\n");
        assert!(edited.is_materialized());
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
        assert_eq!(edited.shared().as_ref(), "λ one\nβ three\n");
    }

    #[test]
    fn exact_inverse_restores_the_previous_snapshot_contents() {
        let original = FrameText::from("large immutable document");
        let forward = Transaction::new(DocumentRevision::new(0), vec![Edit::new(5..5, "!")]).expect("forward transaction");
        let edited = original.edited(&forward).expect("edited snapshot");
        let inverse = forward.invert(&[Box::from("")]).expect("inverse transaction");
        let restored = edited.edited(&inverse).expect("restored snapshot");

        assert!(!restored.same_snapshot(&original));
        assert_eq!(restored.as_ref(), original.as_ref());
    }
}
