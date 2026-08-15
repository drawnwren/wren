use proptest::prelude::*;
use wren_grammar::KeyEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelMode {
    Normal,
    Insert,
    Visual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratorState {
    pub mode: ModelMode,
    pub pending_operator: Option<char>,
}

impl Default for GeneratorState {
    fn default() -> Self {
        Self {
            mode: ModelMode::Normal,
            pending_operator: None,
        }
    }
}

/// Grammar-aware key strategy conditioned on current mode and parser state.
pub fn validish_sequence(state: &GeneratorState) -> BoxedStrategy<Vec<KeyEvent>> {
    let choices: Vec<&'static str> = if state.pending_operator.is_some() {
        vec!["w", "e", "b", "$", "0", "j", "k", "iw", "aw", "gg"]
    } else {
        match state.mode {
            ModelMode::Normal => vec![
                "h", "j", "k", "l", "w", "b", "0", "$", "dw", "dd", "cw", "ciw", "yy", "2w", "3dd",
                "\"ayy", "\"ap", "u", ".",
            ],
            ModelMode::Insert => vec!["a", "Z", " ", "<Esc>"],
            ModelMode::Visual => vec!["h", "j", "k", "l", "w", "d", "y", "<Esc>"],
        }
    };
    proptest::sample::select(choices)
        .prop_map(|sequence| sequence.chars().map(KeyEvent::character).collect())
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    proptest! {
        #[test]
        fn generated_normal_sequences_are_nonempty(sequence in validish_sequence(&GeneratorState::default())) {
            prop_assert!(!sequence.is_empty());
        }
    }
}
