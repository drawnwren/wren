#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::borrow::Cow;
use std::io::{self, Read};
use std::ops::Range;
use std::sync::Arc;

use wren_types::Transaction;

mod snapshot;

pub use snapshot::{
    HeldSnapshot, SnapshotError, SnapshotHandle, SnapshotManager, SnapshotMetrics, SnapshotQuota,
};

/// Cloneable storage for UTF-8 editor text.
pub trait TextStore: Clone {
    fn from_reader(reader: impl Read) -> io::Result<Self>;
    fn len_bytes(&self) -> usize;
    fn content_eq(&self, other: &Self) -> bool;
    fn is_char_boundary(&self, byte: usize) -> bool;
    fn slice(&self, range: Range<usize>) -> Cow<'_, str>;
    fn line_of_byte(&self, byte: usize) -> usize;
    fn byte_of_line(&self, line: usize) -> usize;
    fn line_starts(&self) -> Vec<usize> {
        let last_line = self.line_of_byte(self.len_bytes());
        (0..=last_line)
            .map(|line| self.byte_of_line(line))
            .collect()
    }
    fn apply(&mut self, transaction: &Transaction);
    fn snapshot(&self) -> Self;
}

/// Ropey-backed candidate.
#[derive(Debug, Clone)]
pub struct RopeyText {
    rope: ropey::Rope,
}

impl TextStore for RopeyText {
    fn from_reader(reader: impl Read) -> io::Result<Self> {
        ropey::Rope::from_reader(reader).map(|rope| Self { rope })
    }

    fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    fn content_eq(&self, other: &Self) -> bool {
        self.rope == other.rope
    }

    fn is_char_boundary(&self, byte: usize) -> bool {
        byte <= self.rope.len_bytes() && self.rope.try_byte_to_char(byte).is_ok()
    }

    fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        Cow::Owned(self.rope.byte_slice(range).to_string())
    }

    fn line_of_byte(&self, byte: usize) -> usize {
        self.rope.byte_to_line(byte.min(self.rope.len_bytes()))
    }

    fn byte_of_line(&self, line: usize) -> usize {
        if line >= self.rope.len_lines() {
            self.rope.len_bytes()
        } else {
            self.rope.line_to_byte(line)
        }
    }

    fn apply(&mut self, transaction: &Transaction) {
        for edit in transaction.edits.iter().rev() {
            let start = self.rope.byte_to_char(edit.range.start);
            let end = self.rope.byte_to_char(edit.range.end);
            if start != end {
                self.rope.remove(start..end);
            }
            if !edit.insert.is_empty() {
                self.rope.insert(start, &edit.insert);
            }
        }
    }

    fn snapshot(&self) -> Self {
        self.clone()
    }
}

/// Crop-backed candidate.
#[derive(Debug, Clone)]
pub struct CropText {
    rope: crop::Rope,
}

/// Chosen Phase-0 default; see `docs/decisions/0002-textstore.md`.
pub type DefaultText = CropText;

impl TextStore for CropText {
    fn from_reader(mut reader: impl Read) -> io::Result<Self> {
        let mut text = String::new();
        reader.read_to_string(&mut text)?;
        Ok(Self {
            rope: crop::Rope::from(text),
        })
    }

    fn len_bytes(&self) -> usize {
        self.rope.byte_len()
    }

    fn content_eq(&self, other: &Self) -> bool {
        self.rope == other.rope
    }

    fn is_char_boundary(&self, byte: usize) -> bool {
        byte <= self.rope.byte_len() && self.rope.is_char_boundary(byte)
    }

    fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        Cow::Owned(self.rope.byte_slice(range).to_string())
    }

    fn line_of_byte(&self, byte: usize) -> usize {
        self.rope.line_of_byte(byte.min(self.rope.byte_len()))
    }

    fn byte_of_line(&self, line: usize) -> usize {
        self.rope.byte_of_line(line.min(self.rope.line_len()))
    }

    fn apply(&mut self, transaction: &Transaction) {
        for edit in transaction.edits.iter().rev() {
            self.rope.replace(edit.range.clone(), &edit.insert);
        }
    }

    fn snapshot(&self) -> Self {
        self.clone()
    }
}

/// Experimental copy-on-write placeholder for the mmap-base piece-tree track.
///
/// It is deliberately functionally correct but does not claim mmap or append
/// buffer performance. The bake-off keeps it visible as the baseline that must
/// be replaced if large-file measurements justify novel implementation work.
#[derive(Debug, Clone)]
pub struct PieceTreeStub {
    text: Arc<str>,
}

impl TextStore for PieceTreeStub {
    fn from_reader(mut reader: impl Read) -> io::Result<Self> {
        let mut text = String::new();
        reader.read_to_string(&mut text)?;
        Ok(Self {
            text: Arc::from(text),
        })
    }

    fn len_bytes(&self) -> usize {
        self.text.len()
    }

    fn line_starts(&self) -> Vec<usize> {
        let mut starts = Vec::with_capacity(
            self.text
                .bytes()
                .filter(|value| *value == b'\n')
                .count()
                .saturating_add(1),
        );
        starts.push(0);
        starts.extend(
            self.text
                .bytes()
                .enumerate()
                .filter_map(|(byte, value)| (value == b'\n').then_some(byte + 1)),
        );
        starts
    }

    fn content_eq(&self, other: &Self) -> bool {
        self.text == other.text
    }

    fn is_char_boundary(&self, byte: usize) -> bool {
        self.text.is_char_boundary(byte)
    }

    fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        match self.text.get(range) {
            Some(text) => Cow::Borrowed(text),
            None => Cow::Borrowed(""),
        }
    }

    fn line_of_byte(&self, byte: usize) -> usize {
        let end = byte.min(self.text.len());
        self.text.get(..end).map_or(0, |prefix| {
            prefix.bytes().filter(|value| *value == b'\n').count()
        })
    }

    fn byte_of_line(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        let mut seen = 0;
        for (index, value) in self.text.bytes().enumerate() {
            if value == b'\n' {
                seen += 1;
                if seen == line {
                    return index + 1;
                }
            }
        }
        self.text.len()
    }

    fn apply(&mut self, transaction: &Transaction) {
        if let Ok(changed) = transaction.apply_to_string(&self.text) {
            self.text = Arc::from(changed);
        }
    }

    fn snapshot(&self) -> Self {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use proptest::prelude::*;
    use wren_types::{DocumentRevision, Edit};

    use super::*;

    fn exercise_backend<T: TextStore>() {
        let source = "alpha\nβeta\n👩🏽‍💻 end\n";
        let mut store = T::from_reader(Cursor::new(source)).expect("source is valid UTF-8");
        let snapshot = store.snapshot();
        assert!(store.content_eq(&snapshot));
        let transaction = Transaction::new(DocumentRevision::new(0), vec![Edit::new(6..8, "B")])
            .expect("valid edit");
        store.apply(&transaction);
        assert!(!store.content_eq(&snapshot));
        assert_eq!(store.slice(0..store.len_bytes()), "alpha\nBeta\n👩🏽‍💻 end\n");
        assert_eq!(snapshot.slice(0..snapshot.len_bytes()), source);
        assert_eq!(store.line_of_byte(6), 1);
        assert_eq!(store.byte_of_line(2), 11);
        assert_eq!(store.byte_of_line(usize::MAX), store.len_bytes());
        let line_starts = store.line_starts();
        assert_eq!(line_starts.first(), Some(&0));
        assert_eq!(line_starts.len(), 4);
        assert!(
            line_starts
                .iter()
                .enumerate()
                .all(|(line, byte)| store.byte_of_line(line) == *byte)
        );

        let empty = T::from_reader(Cursor::new("")).expect("empty UTF-8 document loads");
        assert_eq!(empty.byte_of_line(24), 0);
        assert_eq!(empty.line_starts(), vec![0]);
    }

    #[test]
    fn all_backends_obey_basic_contract() {
        exercise_backend::<RopeyText>();
        exercise_backend::<CropText>();
        exercise_backend::<PieceTreeStub>();
    }

    proptest! {
        #[test]
        fn backends_match_string_model(
            source in "[a-z\\n]{0,128}",
            a in 0_usize..256,
            b in 0_usize..256,
            insert in "[A-Z]{0,16}",
        ) {
            let start = a.min(b).min(source.len());
            let end = a.max(b).min(source.len());
            let transaction = Transaction::new(
                DocumentRevision::new(0),
                vec![Edit::new(start..end, insert)],
            ).expect("ASCII edit is valid");
            let expected = transaction.apply_to_string(&source).expect("model applies");

            let mut ropey = RopeyText::from_reader(Cursor::new(&source)).expect("ropey loads");
            let mut crop = CropText::from_reader(Cursor::new(&source)).expect("crop loads");
            let mut piece = PieceTreeStub::from_reader(Cursor::new(&source)).expect("piece loads");
            ropey.apply(&transaction);
            crop.apply(&transaction);
            piece.apply(&transaction);

            prop_assert_eq!(ropey.slice(0..ropey.len_bytes()), expected.as_str());
            prop_assert_eq!(crop.slice(0..crop.len_bytes()), expected.as_str());
            prop_assert_eq!(piece.slice(0..piece.len_bytes()), expected.as_str());
        }
    }
}
