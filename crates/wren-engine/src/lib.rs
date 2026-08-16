#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod editor;
mod search;

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
    pub text: Box<str>,
    pub cursor_byte: usize,
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
            text: self.text.clone().into_boxed_str(),
            cursor_byte: self.cursor,
        }
    }
}
