use regex::{CaptureMatches, Captures, Regex, RegexBuilder};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CaseOverride {
    #[default]
    Default,
    Ignore,
    Sensitive,
}

#[derive(Debug, Clone)]
pub struct VimPattern {
    source: Box<str>,
    regex: Regex,
}

impl VimPattern {
    pub fn compile(pattern: &str, ignore_case: bool, smart_case: bool, case_override: CaseOverride) -> Result<Self, Box<str>> {
        let translated = translate_vim_pattern(pattern)?;
        let insensitive = match translated.case_override.unwrap_or(case_override) {
            CaseOverride::Default => ignore_case && (!smart_case || !pattern.chars().any(char::is_uppercase)),
            CaseOverride::Ignore => true,
            CaseOverride::Sensitive => false,
        };
        let regex =
            RegexBuilder::new(&translated.regex).multi_line(true).case_insensitive(insensitive).build().map_err(|error| error.to_string().into_boxed_str())?;
        Ok(Self { source: pattern.into(), regex })
    }

    #[must_use]
    pub fn is_match(&self, text: &str) -> bool {
        self.regex.is_match(text)
    }

    pub fn find_at<'text>(&self, text: &'text str, start: usize) -> Option<regex::Match<'text>> {
        self.regex.find_at(text, start)
    }

    pub fn find_iter<'text>(&'text self, text: &'text str) -> regex::Matches<'text, 'text> {
        self.regex.find_iter(text)
    }

    pub fn captures_iter<'text>(&'text self, text: &'text str) -> CaptureMatches<'text, 'text> {
        self.regex.captures_iter(text)
    }

    /// Exact byte width for the common plain-ASCII search form. Regex-shaped
    /// patterns deliberately return `None`; only fixed-width literals can be
    /// repaired locally after an edit without changing search semantics.
    pub(crate) fn literal_width(&self) -> Option<usize> {
        (!self.source.is_empty() && self.source.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b' '))).then_some(self.source.len())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VimReplacement {
    source: Box<str>,
}

impl VimReplacement {
    #[must_use]
    pub fn new(source: impl Into<Box<str>>) -> Self {
        Self { source: source.into() }
    }

    #[must_use]
    pub fn expand(&self, captures: &Captures<'_>) -> String {
        let mut output = String::new();
        let mut characters = self.source.chars().peekable();
        let mut case = CaseTransform::None;
        let mut next_case = CaseTransform::None;
        while let Some(character) = characters.next() {
            match character {
                '&' => append_capture(&mut output, captures.get(0), case, &mut next_case),
                '\\' => match characters.next() {
                    Some('0') => append_capture(&mut output, captures.get(0), case, &mut next_case),
                    Some(first @ '1'..='9') => {
                        let mut group = first.to_digit(10).unwrap_or_default() as usize;
                        while let Some(next) = characters.peek().and_then(|value| value.to_digit(10)) {
                            let Some(candidate) = group.checked_mul(10).and_then(|value| value.checked_add(next as usize)) else {
                                break;
                            };
                            group = candidate;
                            characters.next();
                        }
                        append_capture(&mut output, captures.get(group), case, &mut next_case);
                    }
                    Some('r' | 'n') => append_text(&mut output, "\n", case, &mut next_case),
                    Some('t') => append_text(&mut output, "\t", case, &mut next_case),
                    Some('u') => next_case = CaseTransform::Upper,
                    Some('l') => next_case = CaseTransform::Lower,
                    Some('U') => case = CaseTransform::Upper,
                    Some('L') => case = CaseTransform::Lower,
                    Some('E' | 'e') => {
                        case = CaseTransform::None;
                        next_case = CaseTransform::None;
                    }
                    Some(next) => {
                        let mut encoded = [0_u8; 4];
                        append_text(&mut output, next.encode_utf8(&mut encoded), case, &mut next_case);
                    }
                    None => append_text(&mut output, "\\", case, &mut next_case),
                },
                _ => {
                    let mut encoded = [0_u8; 4];
                    append_text(&mut output, character.encode_utf8(&mut encoded), case, &mut next_case);
                }
            }
        }
        output
    }
}

#[must_use]
pub fn resolve_previous_replacement(input: &str, previous: Option<&str>) -> String {
    let mut output = String::new();
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\' && characters.peek() == Some(&'~') {
            output.push('\\');
            output.push('~');
            characters.next();
        } else if character == '~' {
            output.push_str(previous.unwrap_or(""));
        } else {
            output.push(character);
        }
    }
    output
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaseTransform {
    None,
    Upper,
    Lower,
}

fn append_capture(output: &mut String, capture: Option<regex::Match<'_>>, case: CaseTransform, next_case: &mut CaseTransform) {
    if let Some(capture) = capture {
        append_text(output, capture.as_str(), case, next_case);
    }
}

fn append_text(output: &mut String, text: &str, case: CaseTransform, next_case: &mut CaseTransform) {
    for character in text.chars() {
        let transform = if *next_case == CaseTransform::None {
            case
        } else {
            let transform = *next_case;
            *next_case = CaseTransform::None;
            transform
        };
        match transform {
            CaseTransform::None => output.push(character),
            CaseTransform::Upper => output.extend(character.to_uppercase()),
            CaseTransform::Lower => output.extend(character.to_lowercase()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PatternMode {
    Very,
    #[default]
    Default,
    Nomagic,
    Literal,
}

struct TranslatedPattern {
    regex: String,
    case_override: Option<CaseOverride>,
}

fn translate_vim_pattern(pattern: &str) -> Result<TranslatedPattern, Box<str>> {
    let mut translator = PatternTranslator::default();
    let mut characters = pattern.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\' {
            translator.push_escape(&mut characters)?;
        } else {
            translator.push_source(character);
        }
    }
    Ok(translator.finish())
}

#[derive(Default)]
struct PatternTranslator {
    output: String,
    magic: PatternMode,
    case_override: Option<CaseOverride>,
}

impl PatternTranslator {
    fn push_source(&mut self, character: char) {
        if self.magic.is_magic(character) {
            self.output.push(character);
        } else {
            push_escaped_literal(&mut self.output, character);
        }
    }

    fn push_escape(&mut self, characters: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<(), Box<str>> {
        let escaped = characters.next().ok_or_else(|| Box::<str>::from("pattern ends with an incomplete escape"))?;
        match escaped {
            'v' => self.magic = PatternMode::Very,
            'm' => self.magic = PatternMode::Default,
            'M' => self.magic = PatternMode::Nomagic,
            'V' => self.magic = PatternMode::Literal,
            'c' => self.case_override = Some(CaseOverride::Ignore),
            'C' => self.case_override = Some(CaseOverride::Sensitive),
            '<' | '>' => self.output.push_str(r"\b"),
            'z' if matches!(characters.peek(), Some('s' | 'e')) => {
                let suffix = characters.next().unwrap_or_default();
                return Err(format!("Vim atom \\z{suffix} is not supported by the bounded search engine").into_boxed_str());
            }
            '1'..='9' => {
                return Err("Vim pattern backreferences are not supported by the bounded search engine".into());
            }
            '(' | ')' | '|' | '+' | '?' | '{' | '}' | '=' if matches!(self.magic, PatternMode::Default | PatternMode::Nomagic) => {
                self.output.push(if escaped == '=' { '?' } else { escaped });
            }
            '.' | '*' | '[' | ']' | '^' | '$' if self.magic == PatternMode::Default => {
                push_escaped_literal(&mut self.output, escaped);
            }
            '.' | '*' | '[' | ']' if self.magic == PatternMode::Nomagic => {
                self.output.push(escaped);
            }
            '^' | '$' if self.magic == PatternMode::Nomagic => {
                push_escaped_literal(&mut self.output, escaped);
            }
            'd' | 'D' | 's' | 'S' | 'w' | 'W' | 'n' | 'r' | 't' | 'b' | 'B' => {
                self.output.push('\\');
                self.output.push(escaped);
            }
            _ if self.magic == PatternMode::Literal || (!escaped.is_alphanumeric() && escaped != '_') => {
                push_escaped_literal(&mut self.output, escaped);
            }
            _ => return Err(format!("unsupported Vim atom \\{escaped}").into_boxed_str()),
        }
        Ok(())
    }

    fn finish(self) -> TranslatedPattern {
        TranslatedPattern { regex: self.output, case_override: self.case_override }
    }
}

impl PatternMode {
    fn is_magic(self, character: char) -> bool {
        match self {
            Self::Very => !character.is_alphanumeric() && character != '_',
            Self::Default => matches!(character, '.' | '*' | '[' | ']' | '^' | '$'),
            Self::Nomagic => matches!(character, '^' | '$'),
            Self::Literal => false,
        }
    }
}

fn push_escaped_literal(output: &mut String, character: char) {
    if matches!(character, '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '\\') {
        output.push('\\');
    }
    output.push(character);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(source: &str) -> VimPattern {
        VimPattern::compile(source, false, false, CaseOverride::Default).expect("pattern")
    }

    #[test]
    fn translates_default_and_very_magic_vim_patterns() {
        assert!(pattern(r"\<foo\>").is_match("a foo b"));
        assert!(!pattern(r"\<foo\>").is_match("food"));
        assert!(pattern(r"\(ab\)\+").is_match("abab"));
        assert!(pattern(r"\v(ab)+").is_match("abab"));
        assert!(pattern(r"\Ma\.b").is_match("acb"));
        assert!(!pattern(r"\Ma.b").is_match("acb"));
        assert!(pattern(r"\Va+b").is_match("a+b"));
        assert!(pattern(r"foo\/bar").is_match("foo/bar"));
        assert!(pattern("a+b").is_match("a+b"));
        assert!(!pattern("a+b").is_match("aaab"));
    }

    #[test]
    fn honors_smart_case_and_inline_case_atoms() {
        assert!(VimPattern::compile("alpha", true, true, CaseOverride::Default).expect("smart case").is_match("ALPHA"));
        assert!(!VimPattern::compile("Alpha", true, true, CaseOverride::Default).expect("smart case").is_match("ALPHA"));
        assert!(VimPattern::compile(r"Alpha\c", false, false, CaseOverride::Default).expect("inline ignore case").is_match("ALPHA"));
        assert!(!VimPattern::compile(r"alpha\C", true, false, CaseOverride::Default).expect("inline sensitive").is_match("ALPHA"));
    }

    #[test]
    fn expands_vim_captures_newlines_and_case_controls() {
        let pattern = pattern(r"\(foo\)-\(bar\)");
        let captures = pattern.captures_iter("foo-bar").next().expect("captures");
        let replacement = VimReplacement::new(r"\U\1\E:\u\2:&\rnext");
        assert_eq!(replacement.expand(&captures), "FOO:Bar:foo-bar\nnext");
    }

    #[test]
    fn resolves_unescaped_previous_replacement_atoms() {
        assert_eq!(resolve_previous_replacement(r"pre~:\~", Some("OLD")), r"preOLD:\~");
    }
}
