use super::*;
use std::collections::HashMap;

#[derive(Clone, Copy)]
enum HighlightStyle {
    Keyword,
    Comment,
    Pink,
    Sky,
    Overlay,
    Green,
    Teal,
    Peach,
    Mauve,
    Yellow,
    ItalicYellow,
    Blue,
    Maroon,
    Lavender,
    Red,
    Sapphire,
    Text,
    Raw,
    Link,
    Strong,
    Italic,
}

struct HighlightRule {
    style: HighlightStyle,
    exact: &'static [&'static str],
    prefixes: &'static [&'static str],
    suffixes: &'static [&'static str],
}

impl HighlightRule {
    fn matches(&self, kind: &str) -> bool {
        self.exact.contains(&kind) || self.prefixes.iter().any(|prefix| kind.starts_with(prefix)) || self.suffixes.iter().any(|suffix| kind.ends_with(suffix))
    }
}

macro_rules! highlight_rule {
    ($style:ident; $exact:expr; $prefixes:expr; $suffixes:expr) => {
        HighlightRule { style: HighlightStyle::$style, exact: $exact, prefixes: $prefixes, suffixes: $suffixes }
    };
}

const HIGHLIGHT_RULES: &[HighlightRule] = &[
    highlight_rule!(
        Keyword;
        &[
            "conditional",
            "repeat",
            "exception",
            "type.qualifier",
            "type.definition",
            "storage",
            "storageclass",
        ];
        &["keyword"];
        &[]
    ),
    highlight_rule!(Comment; &[]; &["comment"]; &[]),
    highlight_rule!(
        Pink;
        &["include", "constant.macro", "function.macro"];
        &["preproc", "attribute", "decorator"];
        &[]
    ),
    highlight_rule!(Sky; &["operator"]; &[]; &[]),
    highlight_rule!(Overlay; &[]; &["punctuation"]; &[]),
    highlight_rule!(Pink; &["escape"]; &["string.escape", "string.special"]; &[]),
    highlight_rule!(Green; &[]; &["string"]; &[]),
    highlight_rule!(Pink; &[]; &["character.special"]; &[]),
    highlight_rule!(Teal; &[]; &["character"]; &[]),
    highlight_rule!(Peach; &["boolean", "float"]; &["number", "constant"]; &[]),
    highlight_rule!(Mauve; &["type.builtin"]; &[]; &[]),
    highlight_rule!(Yellow; &["constructor"]; &["type"]; &[]),
    highlight_rule!(Peach; &["function.builtin"]; &[]; &[]),
    highlight_rule!(Blue; &["tag"]; &["function", "method"]; &[]),
    highlight_rule!(Pink; &["variable.parameter.builtin"]; &[]; &[]),
    highlight_rule!(Maroon; &["parameter"]; &["variable.parameter"]; &[]),
    highlight_rule!(Lavender; &["property", "field"]; &["variable.member"]; &[]),
    highlight_rule!(Teal; &["semantic.enum-member"]; &[]; &[]),
    highlight_rule!(Yellow; &["semantic.event"]; &[]; &[]),
    highlight_rule!(Pink; &["semantic.regexp"]; &[]; &[]),
    highlight_rule!(Blue; &["semantic.decorator"]; &[]; &[]),
    highlight_rule!(
        ItalicYellow;
        &[
            "semantic.namespace",
            "tag.attribute",
            "namespace",
            "module",
            "module.builtin",
        ];
        &[];
        &[]
    ),
    highlight_rule!(Red; &["variable.builtin"]; &[]; &[]),
    highlight_rule!(Keyword; &[]; &["markup.heading"]; &[]),
    highlight_rule!(Raw; &[]; &["markup.raw"]; &[]),
    highlight_rule!(Link; &[]; &["markup.link"]; &[".url"]),
    highlight_rule!(Strong; &["markup.strong"]; &[]; &[]),
    highlight_rule!(Italic; &["markup.italic"]; &[]; &[]),
    highlight_rule!(Sapphire; &["label", "symbol"]; &[]; &[]),
    highlight_rule!(Text; &[]; &["variable"]; &[]),
];

pub(super) fn provider_decoration(span: HighlightSpan, theme: CatppuccinPalette) -> DecorationSpan {
    DecorationSpan::new(span.range, provider_cell_style(&span.kind, theme), span.priority)
}

pub(super) fn provider_decorations(spans: Vec<HighlightSpan>, theme: CatppuccinPalette) -> Vec<DecorationSpan> {
    let mut styles = HashMap::<Arc<str>, CellStyle>::new();
    spans
        .into_iter()
        .map(|span| {
            let style = styles.get(span.kind.as_ref()).copied().unwrap_or_else(|| {
                let style = provider_cell_style(&span.kind, theme);
                styles.insert(Arc::clone(&span.kind), style);
                style
            });
            DecorationSpan::new(span.range, style, span.priority)
        })
        .collect()
}

fn provider_cell_style(kind: &str, theme: CatppuccinPalette) -> CellStyle {
    let style = HIGHLIGHT_RULES.iter().find(|rule| rule.matches(kind)).map_or(HighlightStyle::Text, |rule| rule.style);
    highlight_cell_style(style, theme)
}

fn highlight_cell_style(style: HighlightStyle, theme: CatppuccinPalette) -> CellStyle {
    let (foreground, bold, italic, underline, background) = match style {
        HighlightStyle::Keyword => (theme.mauve, true, false, false, None),
        HighlightStyle::Comment => (theme.overlay2, false, true, false, None),
        HighlightStyle::Pink => (theme.pink, false, false, false, None),
        HighlightStyle::Sky => (theme.sky, false, false, false, None),
        HighlightStyle::Overlay => (theme.overlay2, false, false, false, None),
        HighlightStyle::Green => (theme.green, false, false, false, None),
        HighlightStyle::Teal => (theme.teal, false, false, false, None),
        HighlightStyle::Peach => (theme.peach, false, false, false, None),
        HighlightStyle::Mauve => (theme.mauve, false, false, false, None),
        HighlightStyle::Yellow => (theme.yellow, false, false, false, None),
        HighlightStyle::ItalicYellow => (theme.yellow, false, true, false, None),
        HighlightStyle::Blue => (theme.blue, false, false, false, None),
        HighlightStyle::Maroon => (theme.maroon, false, false, false, None),
        HighlightStyle::Lavender => (theme.lavender, false, false, false, None),
        HighlightStyle::Red => (theme.red, false, false, false, None),
        HighlightStyle::Sapphire => (theme.sapphire, false, false, false, None),
        HighlightStyle::Text => (theme.text, false, false, false, None),
        HighlightStyle::Raw => (theme.green, false, false, false, Some(theme.surface0)),
        HighlightStyle::Link => (theme.blue, false, false, true, None),
        HighlightStyle::Strong => (theme.text, true, false, false, None),
        HighlightStyle::Italic => (theme.text, false, true, false, None),
    };
    CellStyle { bold, italic, underline, foreground: Some(CellColor::Rgb(foreground)), background: background.map(CellColor::Rgb), ..CellStyle::default() }
}

pub(super) fn markdown_decorations(text: &str, theme: CatppuccinPalette) -> Vec<DecorationSpan> {
    let mut spans = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let indentation = line.len().saturating_sub(trimmed.len());
        if trimmed.starts_with('#') {
            spans.push(DecorationSpan::new(
                offset + indentation..offset + line.trim_end().len(),
                CellStyle::default().with_foreground(CellColor::Rgb(theme.mauve)).with_bold(),
                1_100_000,
            ));
        } else if trimmed.starts_with("> ") {
            spans.push(DecorationSpan::new(
                offset + indentation..offset + line.trim_end().len(),
                CellStyle::default().with_foreground(CellColor::Rgb(theme.overlay2)).with_italic(),
                1_100_000,
            ));
        }
        for (delimiter, mut style) in
            [("**", CellStyle::default().with_bold()), ("~~", CellStyle::default().with_strikethrough()), ("`", CellStyle::rgb(theme.green, theme.surface0))]
        {
            let mut search = 0;
            while let Some(start) = line[search..].find(delimiter) {
                let start = search + start;
                let content_start = start + delimiter.len();
                let Some(end) = line[content_start..].find(delimiter) else {
                    break;
                };
                let end = content_start + end + delimiter.len();
                if delimiter == "`" {
                    style.italic = false;
                }
                spans.push(DecorationSpan::new(offset + start..offset + end, style, 1_100_000));
                search = end;
            }
        }
        offset += line.len();
    }
    spans
}

pub(super) fn lsp_popup_markdown(markdown: &str, theme: CatppuccinPalette) -> (String, Vec<DecorationSpan>) {
    let mut text = String::new();
    let mut code_block = None::<(usize, String)>;
    let mut code_spans = Vec::new();
    for line in markdown.split_inclusive('\n') {
        let trimmed = line.trim();
        if let Some(fence) = trimmed.strip_prefix("```") {
            if let Some((start, language)) = code_block.take() {
                let _language = normalized_fence_language(&language);
                code_spans.extend(lexical_highlight_text(&text[start..]).into_iter().map(|mut span| {
                    span.range = start + span.range.start..start + span.range.end;
                    provider_decoration(span, theme)
                }));
            } else {
                code_block = Some((text.len(), fence.trim().to_owned()));
            }
            continue;
        }
        text.push_str(line);
    }
    if let Some((start, language)) = code_block {
        let _language = normalized_fence_language(&language);
        code_spans.extend(lexical_highlight_text(&text[start..]).into_iter().map(|mut span| {
            span.range = start + span.range.start..start + span.range.end;
            provider_decoration(span, theme)
        }));
    }
    while text.ends_with('\n') {
        text.pop();
    }
    let mut decorations = markdown_decorations(&text, theme);
    decorations.extend(code_spans);
    (text, decorations)
}

pub(super) fn normalized_fence_language(language: &str) -> &str {
    match language.trim().to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "js" | "jsx" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "py" => "python",
        "sh" | "shell" | "zsh" => "bash",
        "hs" => "haskell",
        "tf" | "terraform" => "hcl",
        _ => language.trim(),
    }
}

pub(super) fn language_bundle(path: Option<&Path>) -> LanguageBundle {
    let language_id = path.and_then(bundled_language_id).unwrap_or("text");
    let mut identity = [0_u8; 32];
    for (index, byte) in language_id.bytes().enumerate() {
        identity[index % identity.len()] ^= byte;
    }
    LanguageBundle {
        language_id: language_id.into(),
        grammar_hash: identity,
        grammar_abi: 15,
        grammar_semver: "bundled".into(),
        highlight_query_hash: identity,
        object_query_hash: identity,
        outline_query_hash: identity,
        injection_query_hash: identity,
        config_schema_version: 1,
    }
}
