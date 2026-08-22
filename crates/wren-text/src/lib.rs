#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::borrow::Cow;
use std::io::{self, Read};
use std::ops::ControlFlow;
use std::ops::Range;
use std::sync::Arc;
use std::{fmt, path::Path};

use mmap_io::MemoryMappedFile;
use wren_types::Transaction;

#[cfg(any(test, feature = "benchmarking"))]
mod snapshot;

#[cfg(any(test, feature = "benchmarking"))]
pub use snapshot::{HeldSnapshot, SnapshotError, SnapshotHandle, SnapshotManager, SnapshotMetrics, SnapshotQuota};

/// Cloneable storage for UTF-8 editor text.
pub trait TextStore: Clone {
    fn from_reader(reader: impl Read) -> io::Result<Self>;
    fn len_bytes(&self) -> usize;
    fn content_eq(&self, other: &Self) -> bool;
    fn is_char_boundary(&self, byte: usize) -> bool;
    /// Returns a borrowed range when a backend can provide one without
    /// materializing a rope or piece-tree slice. Callers must be prepared to
    /// visit segmented text instead.
    fn contiguous(&self, _range: Range<usize>) -> Option<&str> {
        None
    }
    /// Visits the storage segments covering `range` without constructing a
    /// whole-document string. `Some` is returned only when the visitor stops
    /// early with `ControlFlow::Break`.
    fn visit_chunks<R>(&self, range: Range<usize>, visit: impl FnMut(&str) -> ControlFlow<R>) -> Option<R>;
    fn slice(&self, range: Range<usize>) -> Cow<'_, str>;
    fn line_of_byte(&self, byte: usize) -> usize;
    fn byte_of_line(&self, line: usize) -> usize;
    fn line_starts(&self) -> Vec<usize> {
        let last_line = self.line_of_byte(self.len_bytes());
        (0..=last_line).map(|line| self.byte_of_line(line)).collect()
    }
    fn apply(&mut self, transaction: &Transaction);
    fn snapshot(&self) -> Self {
        self.clone()
    }
}

/// Ropey-backed candidate.
#[cfg(any(test, feature = "benchmarking"))]
#[derive(Debug, Clone)]
pub struct RopeyText {
    rope: ropey::Rope,
}

#[cfg(any(test, feature = "benchmarking"))]
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

    fn visit_chunks<R>(&self, range: Range<usize>, mut visit: impl FnMut(&str) -> ControlFlow<R>) -> Option<R> {
        let start = range.start.min(self.rope.len_bytes());
        let end = range.end.min(self.rope.len_bytes()).max(start);
        for chunk in self.rope.byte_slice(start..end).chunks() {
            if let ControlFlow::Break(result) = visit(chunk) {
                return Some(result);
            }
        }
        None
    }

    fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        Cow::Owned(self.rope.byte_slice(range).to_string())
    }

    fn line_of_byte(&self, byte: usize) -> usize {
        self.rope.byte_to_line(byte.min(self.rope.len_bytes()))
    }

    fn byte_of_line(&self, line: usize) -> usize {
        if line >= self.rope.len_lines() { self.rope.len_bytes() } else { self.rope.line_to_byte(line) }
    }

    fn apply(&mut self, transaction: &Transaction) {
        for edit in transaction.edits().iter().rev() {
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
}

/// Crop-backed candidate.
#[derive(Debug, Clone)]
pub struct CropText {
    rope: crop::Rope,
}

impl CropText {
    #[must_use]
    pub fn from_string(text: String) -> Self {
        Self { rope: crop::Rope::from(text) }
    }
}

impl TextStore for CropText {
    fn from_reader(mut reader: impl Read) -> io::Result<Self> {
        let mut text = String::new();
        reader.read_to_string(&mut text)?;
        Ok(Self::from_string(text))
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

    fn visit_chunks<R>(&self, range: Range<usize>, mut visit: impl FnMut(&str) -> ControlFlow<R>) -> Option<R> {
        let start = range.start.min(self.rope.byte_len());
        let end = range.end.min(self.rope.byte_len()).max(start);
        for chunk in self.rope.byte_slice(start..end).chunks() {
            if let ControlFlow::Break(result) = visit(chunk) {
                return Some(result);
            }
        }
        None
    }

    fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        Cow::Owned(self.rope.byte_slice(range).to_string())
    }

    fn line_of_byte(&self, byte: usize) -> usize {
        let mut byte = byte.min(self.rope.byte_len());
        while !self.rope.is_char_boundary(byte) {
            byte -= 1;
        }
        self.rope.line_of_byte(byte)
    }

    fn byte_of_line(&self, line: usize) -> usize {
        self.rope.byte_of_line(line.min(self.rope.line_len()))
    }

    fn apply(&mut self, transaction: &Transaction) {
        for edit in transaction.edits().iter().rev() {
            self.rope.replace(edit.range.clone(), &edit.insert);
        }
    }
}

/// Copy-on-write piece storage with an immutable, read-only memory-mapped
/// base. The mapped bytes are never rewritten: edits are represented as small
/// replacement pieces, so a localized change does not duplicate the original
/// file. Callers must keep the normal external-change identity checks in
/// place; a mapping represents the file generation that was opened.
#[derive(Clone)]
pub struct MmapPieceText {
    base: PieceBase,
    pieces: Vec<TextPiece>,
    len: usize,
}

#[derive(Clone)]
enum PieceBase {
    Owned(Arc<str>),
    Mapped(Arc<MemoryMappedFile>),
}

#[derive(Clone)]
enum TextPiece {
    Base(Range<usize>),
    Added { text: Arc<str>, range: Range<usize> },
}

impl fmt::Debug for MmapPieceText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("MmapPieceText").field("bytes", &self.len).field("pieces", &self.pieces.len()).finish()
    }
}

impl MmapPieceText {
    /// Opens a valid UTF-8 file as a mapped immutable base. Empty files use a
    /// small owned base because operating systems do not map zero-length
    /// files. Invalid UTF-8 is rejected so all `TextStore` offsets remain
    /// character-safe.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let mapped = MemoryMappedFile::open_ro(path.as_ref()).map_err(|error| io::Error::other(error.to_string()))?;
        let len = usize::try_from(mapped.len()).map_err(|_| io::Error::other("mapped text exceeds addressable editor size"))?;
        if len == 0 {
            return Ok(Self::from_string(String::new()));
        }
        {
            let bytes = mapped.as_slice(0, mapped.len()).map_err(|error| io::Error::other(error.to_string()))?;
            std::str::from_utf8(&bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        }
        Ok(Self { base: PieceBase::Mapped(Arc::new(mapped)), pieces: vec![TextPiece::Base(0..len)], len })
    }

    #[must_use]
    pub fn from_string(text: String) -> Self {
        let len = text.len();
        let base = Arc::from(text);
        Self { base: PieceBase::Owned(base), pieces: (len > 0).then_some(TextPiece::Base(0..len)).into_iter().collect(), len }
    }

    fn piece_len(piece: &TextPiece) -> usize {
        match piece {
            TextPiece::Base(range) | TextPiece::Added { range, .. } => range.end.saturating_sub(range.start),
        }
    }

    fn visit_base<R>(&self, range: Range<usize>, visit: &mut impl FnMut(&str) -> ControlFlow<R>) -> Option<R> {
        match &self.base {
            PieceBase::Owned(text) => text.get(range).and_then(|chunk| match visit(chunk) {
                ControlFlow::Continue(()) => None,
                ControlFlow::Break(result) => Some(result),
            }),
            PieceBase::Mapped(mapped) => {
                let start = u64::try_from(range.start).ok()?;
                let len = u64::try_from(range.end.saturating_sub(range.start)).ok()?;
                let bytes = mapped.as_slice(start, len).ok()?;
                let chunk = std::str::from_utf8(&bytes).ok()?;
                match visit(chunk) {
                    ControlFlow::Continue(()) => None,
                    ControlFlow::Break(result) => Some(result),
                }
            }
        }
    }

    fn visit_piece<R>(&self, piece: &TextPiece, range: Range<usize>, visit: &mut impl FnMut(&str) -> ControlFlow<R>) -> Option<R> {
        match piece {
            TextPiece::Base(base) => self.visit_base(base.start.saturating_add(range.start)..base.start.saturating_add(range.end), visit),
            TextPiece::Added { text, range: source } => {
                text.get(source.start.saturating_add(range.start)..source.start.saturating_add(range.end)).and_then(|chunk| match visit(chunk) {
                    ControlFlow::Continue(()) => None,
                    ControlFlow::Break(result) => Some(result),
                })
            }
        }
    }

    fn byte_at(&self, byte: usize) -> Option<u8> {
        if byte >= self.len {
            return None;
        }
        let mut found = None;
        let _ = self.visit_chunks(byte..byte.saturating_add(1), |chunk| {
            found = chunk.as_bytes().first().copied();
            ControlFlow::Break(())
        });
        found
    }

    fn split_at(&mut self, byte: usize) {
        if byte == 0 || byte >= self.len {
            return;
        }
        let mut cursor: usize = 0;
        for index in 0..self.pieces.len() {
            let piece_len = Self::piece_len(&self.pieces[index]);
            let end = cursor.saturating_add(piece_len);
            if byte > cursor && byte < end {
                let split = byte - cursor;
                let piece = self.pieces[index].clone();
                let (left, right) = match piece {
                    TextPiece::Base(range) => (TextPiece::Base(range.start..range.start + split), TextPiece::Base(range.start + split..range.end)),
                    TextPiece::Added { text, range } => (
                        TextPiece::Added { text: Arc::clone(&text), range: range.start..range.start + split },
                        TextPiece::Added { text, range: range.start + split..range.end },
                    ),
                };
                self.pieces[index] = left;
                self.pieces.insert(index + 1, right);
                return;
            }
            cursor = end;
        }
    }

    fn piece_index_at(&self, byte: usize) -> usize {
        let mut cursor: usize = 0;
        for (index, piece) in self.pieces.iter().enumerate() {
            if cursor == byte {
                return index;
            }
            cursor = cursor.saturating_add(Self::piece_len(piece));
        }
        self.pieces.len()
    }

    fn replace(&mut self, range: Range<usize>, insert: &str) {
        let start = range.start.min(self.len);
        let end = range.end.min(self.len).max(start);
        self.split_at(start);
        self.split_at(end);
        let start_index = self.piece_index_at(start);
        let end_index = self.piece_index_at(end);
        let replacement = (!insert.is_empty()).then(|| TextPiece::Added { text: Arc::from(insert), range: 0..insert.len() });
        self.pieces.splice(start_index..end_index, replacement);
        self.len = self.len.saturating_sub(end.saturating_sub(start)).saturating_add(insert.len());
    }
}

impl TextStore for MmapPieceText {
    fn from_reader(mut reader: impl Read) -> io::Result<Self> {
        let mut text = String::new();
        reader.read_to_string(&mut text)?;
        Ok(Self::from_string(text))
    }

    fn len_bytes(&self) -> usize {
        self.len
    }

    fn content_eq(&self, other: &Self) -> bool {
        self.len == other.len && self.slice(0..self.len) == other.slice(0..other.len)
    }

    fn is_char_boundary(&self, byte: usize) -> bool {
        byte <= self.len && self.byte_at(byte).is_none_or(|value| value & 0b1100_0000 != 0b1000_0000)
    }

    fn contiguous(&self, range: Range<usize>) -> Option<&str> {
        let start = range.start.min(self.len);
        let end = range.end.min(self.len).max(start);
        let mut cursor: usize = 0;
        for piece in &self.pieces {
            let piece_len = Self::piece_len(piece);
            let piece_end = cursor.saturating_add(piece_len);
            if cursor <= start && end <= piece_end {
                return match (piece, &self.base) {
                    (TextPiece::Base(source), PieceBase::Owned(text)) => text.get(source.start + start - cursor..source.start + end - cursor),
                    (TextPiece::Added { text, range: source }, _) => text.get(source.start + start - cursor..source.start + end - cursor),
                    (TextPiece::Base(_), PieceBase::Mapped(_)) => None,
                };
            }
            cursor = piece_end;
        }
        None
    }

    fn visit_chunks<R>(&self, range: Range<usize>, mut visit: impl FnMut(&str) -> ControlFlow<R>) -> Option<R> {
        let start = range.start.min(self.len);
        let end = range.end.min(self.len).max(start);
        let mut cursor: usize = 0;
        for piece in &self.pieces {
            let piece_len = Self::piece_len(piece);
            let piece_end = cursor.saturating_add(piece_len);
            let overlap_start = start.max(cursor);
            let overlap_end = end.min(piece_end);
            if overlap_start < overlap_end
                && let Some(result) = self.visit_piece(piece, overlap_start - cursor..overlap_end - cursor, &mut visit)
            {
                return Some(result);
            }
            if cursor >= end {
                break;
            }
            cursor = piece_end;
        }
        None
    }

    fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        let start = range.start.min(self.len);
        let end = range.end.min(self.len).max(start);
        if let Some(text) = self.contiguous(start..end) {
            return Cow::Borrowed(text);
        }
        let mut text = String::with_capacity(end.saturating_sub(start));
        let _ = self.visit_chunks(start..end, |chunk| {
            text.push_str(chunk);
            ControlFlow::<()>::Continue(())
        });
        Cow::Owned(text)
    }

    fn line_of_byte(&self, byte: usize) -> usize {
        let mut lines = 0;
        let _ = self.visit_chunks(0..byte.min(self.len), |chunk| {
            lines += chunk.bytes().filter(|byte| *byte == b'\n').count();
            ControlFlow::<()>::Continue(())
        });
        lines
    }

    fn byte_of_line(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        let mut lines = 0;
        let mut offset = 0;
        let mut found = None;
        let _ = self.visit_chunks(0..self.len, |chunk| {
            for (index, byte) in chunk.bytes().enumerate() {
                if byte == b'\n' {
                    lines += 1;
                    if lines == line {
                        found = Some(offset + index + 1);
                        return ControlFlow::Break(());
                    }
                }
            }
            offset += chunk.len();
            ControlFlow::Continue(())
        });
        found.unwrap_or(self.len)
    }

    fn apply(&mut self, transaction: &Transaction) {
        for edit in transaction.edits().iter().rev() {
            self.replace(edit.range.clone(), edit.insert.as_ref());
        }
    }
}

/// The production storage choice. Ordinary and generated buffers stay on the
/// compact Crop implementation; eligible path-backed large files can retain
/// an immutable mapped base with `from_mapped_path`.
#[derive(Debug, Clone)]
pub enum DefaultText {
    Crop(CropText),
    MmapPiece(MmapPieceText),
}

impl DefaultText {
    #[must_use]
    pub fn from_string(text: String) -> Self {
        Self::Crop(CropText::from_string(text))
    }

    pub fn from_mapped_path(path: impl AsRef<Path>) -> io::Result<Self> {
        MmapPieceText::open(path).map(Self::MmapPiece)
    }

    #[must_use]
    pub const fn is_mapped_piece_text(&self) -> bool {
        matches!(self, Self::MmapPiece(_))
    }
}

impl TextStore for DefaultText {
    fn from_reader(reader: impl Read) -> io::Result<Self> {
        CropText::from_reader(reader).map(Self::Crop)
    }

    fn len_bytes(&self) -> usize {
        match self {
            Self::Crop(text) => text.len_bytes(),
            Self::MmapPiece(text) => text.len_bytes(),
        }
    }

    fn content_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Crop(left), Self::Crop(right)) => left.content_eq(right),
            (Self::MmapPiece(left), Self::MmapPiece(right)) => left.content_eq(right),
            _ => self.slice(0..self.len_bytes()) == other.slice(0..other.len_bytes()),
        }
    }

    fn is_char_boundary(&self, byte: usize) -> bool {
        match self {
            Self::Crop(text) => text.is_char_boundary(byte),
            Self::MmapPiece(text) => text.is_char_boundary(byte),
        }
    }

    fn contiguous(&self, range: Range<usize>) -> Option<&str> {
        match self {
            Self::Crop(text) => text.contiguous(range),
            Self::MmapPiece(text) => text.contiguous(range),
        }
    }

    fn visit_chunks<R>(&self, range: Range<usize>, visit: impl FnMut(&str) -> ControlFlow<R>) -> Option<R> {
        match self {
            Self::Crop(text) => text.visit_chunks(range, visit),
            Self::MmapPiece(text) => text.visit_chunks(range, visit),
        }
    }

    fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        match self {
            Self::Crop(text) => text.slice(range),
            Self::MmapPiece(text) => text.slice(range),
        }
    }

    fn line_of_byte(&self, byte: usize) -> usize {
        match self {
            Self::Crop(text) => text.line_of_byte(byte),
            Self::MmapPiece(text) => text.line_of_byte(byte),
        }
    }

    fn byte_of_line(&self, line: usize) -> usize {
        match self {
            Self::Crop(text) => text.byte_of_line(line),
            Self::MmapPiece(text) => text.byte_of_line(line),
        }
    }

    fn apply(&mut self, transaction: &Transaction) {
        match self {
            Self::Crop(text) => text.apply(transaction),
            Self::MmapPiece(text) => text.apply(transaction),
        }
    }
}

/// Experimental copy-on-write placeholder for the mmap-base piece-tree track.
///
/// It is deliberately functionally correct but does not claim mmap or append
/// buffer performance. The bake-off keeps it visible as the baseline that must
/// be replaced if large-file measurements justify novel implementation work.
#[cfg(any(test, feature = "benchmarking"))]
#[derive(Debug, Clone)]
pub struct PieceTreeStub {
    text: Arc<str>,
}

#[cfg(any(test, feature = "benchmarking"))]
impl TextStore for PieceTreeStub {
    fn from_reader(mut reader: impl Read) -> io::Result<Self> {
        let mut text = String::new();
        reader.read_to_string(&mut text)?;
        Ok(Self { text: Arc::from(text) })
    }

    fn len_bytes(&self) -> usize {
        self.text.len()
    }

    fn line_starts(&self) -> Vec<usize> {
        let mut starts = Vec::with_capacity(self.text.bytes().filter(|value| *value == b'\n').count().saturating_add(1));
        starts.push(0);
        starts.extend(self.text.bytes().enumerate().filter_map(|(byte, value)| (value == b'\n').then_some(byte + 1)));
        starts
    }

    fn content_eq(&self, other: &Self) -> bool {
        self.text == other.text
    }

    fn is_char_boundary(&self, byte: usize) -> bool {
        self.text.is_char_boundary(byte)
    }

    fn contiguous(&self, range: Range<usize>) -> Option<&str> {
        self.text.get(range)
    }

    fn visit_chunks<R>(&self, range: Range<usize>, mut visit: impl FnMut(&str) -> ControlFlow<R>) -> Option<R> {
        let start = range.start.min(self.text.len());
        let end = range.end.min(self.text.len()).max(start);
        match visit(&self.text[start..end]) {
            ControlFlow::Continue(()) => None,
            ControlFlow::Break(result) => Some(result),
        }
    }

    fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        Cow::Borrowed(self.text.get(range).unwrap_or(""))
    }

    fn line_of_byte(&self, byte: usize) -> usize {
        let end = byte.min(self.text.len());
        self.text.get(..end).map_or(0, |prefix| prefix.bytes().filter(|value| *value == b'\n').count())
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
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use proptest::prelude::*;
    use wren_types::{DocumentRevision, Edit};

    use super::*;

    fn exercise_backend<T: TextStore>() {
        let source = "alpha\nβeta\n👩🏽‍💻 end\n";
        let mut store = T::from_reader(Cursor::new(source)).expect("source is valid UTF-8");
        let snapshot = store.snapshot();
        assert!(store.content_eq(&snapshot));
        let transaction = Transaction::new(DocumentRevision::new(0), vec![Edit::new(6..8, "B")]).expect("valid edit");
        store.apply(&transaction);
        assert!(!store.content_eq(&snapshot));
        assert_eq!(store.slice(0..store.len_bytes()), "alpha\nBeta\n👩🏽‍💻 end\n");
        assert_eq!(snapshot.slice(0..snapshot.len_bytes()), source);
        assert_eq!(store.line_of_byte(6), 1);
        assert_eq!(store.byte_of_line(2), 11);
        assert_eq!(store.byte_of_line(usize::MAX), store.len_bytes());
        assert_line_index(&store);

        let empty = T::from_reader(Cursor::new("")).expect("empty UTF-8 document loads");
        assert_eq!(empty.byte_of_line(24), 0);
        assert_eq!(empty.line_starts(), vec![0]);
    }

    fn assert_line_index<T: TextStore>(store: &T) {
        let line_starts = store.line_starts();
        assert_eq!(line_starts.first(), Some(&0));
        assert_eq!(line_starts.len(), 4);
        assert!(line_starts.iter().enumerate().all(|(line, byte)| store.byte_of_line(line) == *byte));
    }

    #[test]
    fn all_backends_obey_basic_contract() {
        exercise_backend::<RopeyText>();
        exercise_backend::<CropText>();
        exercise_backend::<MmapPieceText>();
        exercise_backend::<PieceTreeStub>();
    }

    #[test]
    fn mapped_piece_base_preserves_untouched_bytes_across_local_edits() {
        let mut source = tempfile::NamedTempFile::new().expect("temporary source");
        source.write_all(b"alpha\nbeta\ngamma\n").expect("write source");
        source.flush().expect("flush source");
        let mut text = MmapPieceText::open(source.path()).expect("map UTF-8 source");
        let transaction = Transaction::new(DocumentRevision::new(0), vec![Edit::new(6..10, "BETA")]).expect("valid mapped edit");
        text.apply(&transaction);
        assert_eq!(text.slice(0..text.len_bytes()), "alpha\nBETA\ngamma\n");
        assert_eq!(text.byte_of_line(2), "alpha\nBETA\n".len());
        assert!(text.is_char_boundary(text.len_bytes()));
    }

    #[test]
    fn chunk_visitors_return_bounded_segments_and_can_stop_early() {
        let text = CropText::from_reader(Cursor::new("alpha\nβeta\ngamma")).expect("text");
        let mut visited = String::new();
        let result = text.visit_chunks(0..text.len_bytes(), |chunk| {
            visited.push_str(chunk);
            ControlFlow::<usize>::Continue(())
        });
        assert_eq!(result, None);
        assert_eq!(visited, "alpha\nβeta\ngamma");

        let stopped = text.visit_chunks(0..text.len_bytes(), |_| ControlFlow::Break(7_usize));
        assert_eq!(stopped, Some(7));
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
