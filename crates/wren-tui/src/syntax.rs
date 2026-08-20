use super::*;
use std::collections::HashMap;

struct HighlightRule {
    color: CatppuccinColor,
    attributes: u8,
    background: Option<CatppuccinColor>,
    exact: &'static [&'static str],
    prefixes: &'static [&'static str],
    suffixes: &'static [&'static str],
}

impl HighlightRule {
    const fn new(color: CatppuccinColor, exact: &'static [&'static str], prefixes: &'static [&'static str], suffixes: &'static [&'static str]) -> Self {
        Self { color, attributes: 0, background: None, exact, prefixes, suffixes }
    }

    const fn attributes(mut self, attributes: u8) -> Self {
        self.attributes = attributes;
        self
    }

    const fn background(mut self, background: CatppuccinColor) -> Self {
        self.background = Some(background);
        self
    }

    fn matches(&self, kind: &str) -> bool {
        self.exact.contains(&kind) || self.prefixes.iter().any(|prefix| kind.starts_with(prefix)) || self.suffixes.iter().any(|suffix| kind.ends_with(suffix))
    }
}

const HIGHLIGHT_RULES: &[HighlightRule] = &[
    HighlightRule::new(
        CatppuccinColor::Mauve,
        &["conditional", "repeat", "exception", "type.qualifier", "type.definition", "storage", "storageclass"],
        &["keyword"],
        &[],
    )
    .attributes(1),
    HighlightRule::new(CatppuccinColor::Overlay2, &[], &["comment"], &[]).attributes(2),
    HighlightRule::new(CatppuccinColor::Pink, &["include", "constant.macro", "function.macro"], &["preproc", "attribute", "decorator"], &[]),
    HighlightRule::new(CatppuccinColor::Sky, &["operator"], &[], &[]),
    HighlightRule::new(CatppuccinColor::Overlay2, &[], &["punctuation"], &[]),
    HighlightRule::new(CatppuccinColor::Pink, &["escape"], &["string.escape", "string.special"], &[]),
    HighlightRule::new(CatppuccinColor::Green, &[], &["string"], &[]),
    HighlightRule::new(CatppuccinColor::Pink, &[], &["character.special"], &[]),
    HighlightRule::new(CatppuccinColor::Teal, &[], &["character"], &[]),
    HighlightRule::new(CatppuccinColor::Peach, &["boolean", "float"], &["number", "constant"], &[]),
    HighlightRule::new(CatppuccinColor::Mauve, &["type.builtin"], &[], &[]),
    HighlightRule::new(CatppuccinColor::Yellow, &["constructor"], &["type"], &[]),
    HighlightRule::new(CatppuccinColor::Peach, &["function.builtin"], &[], &[]),
    HighlightRule::new(CatppuccinColor::Blue, &["tag"], &["function", "method"], &[]),
    HighlightRule::new(CatppuccinColor::Pink, &["variable.parameter.builtin"], &[], &[]),
    HighlightRule::new(CatppuccinColor::Maroon, &["parameter"], &["variable.parameter"], &[]),
    HighlightRule::new(CatppuccinColor::Lavender, &["property", "field"], &["variable.member"], &[]),
    HighlightRule::new(CatppuccinColor::Teal, &["semantic.enum-member"], &[], &[]),
    HighlightRule::new(CatppuccinColor::Yellow, &["semantic.event"], &[], &[]),
    HighlightRule::new(CatppuccinColor::Pink, &["semantic.regexp"], &[], &[]),
    HighlightRule::new(CatppuccinColor::Blue, &["semantic.decorator"], &[], &[]),
    HighlightRule::new(CatppuccinColor::Yellow, &["semantic.namespace", "tag.attribute", "namespace", "module", "module.builtin"], &[], &[]).attributes(2),
    HighlightRule::new(CatppuccinColor::Red, &["variable.builtin"], &[], &[]),
    HighlightRule::new(CatppuccinColor::Mauve, &[], &["markup.heading"], &[]).attributes(1),
    HighlightRule::new(CatppuccinColor::Green, &[], &["markup.raw"], &[]).background(CatppuccinColor::Surface0),
    HighlightRule::new(CatppuccinColor::Blue, &[], &["markup.link"], &[".url"]).attributes(4),
    HighlightRule::new(CatppuccinColor::Text, &["markup.strong"], &[], &[]).attributes(1),
    HighlightRule::new(CatppuccinColor::Text, &["markup.italic"], &[], &[]).attributes(2),
    HighlightRule::new(CatppuccinColor::Sapphire, &["label", "symbol"], &[], &[]),
    HighlightRule::new(CatppuccinColor::Text, &[], &["variable"], &[]),
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
    let rule = HIGHLIGHT_RULES.iter().find(|rule| rule.matches(kind));
    let color = rule.map_or(CatppuccinColor::Text, |rule| rule.color);
    let attributes = rule.map_or(0, |rule| rule.attributes);
    CellStyle {
        attributes,
        foreground: Some(CellColor::Rgb(theme.color(color))),
        background: rule.and_then(|rule| rule.background).map(|color| CellColor::Rgb(theme.color(color))),
    }
}

pub(super) fn lsp_popup_markdown(markdown: &str, theme: CatppuccinPalette) -> (String, Vec<DecorationSpan>) {
    let mut text = String::new();
    let mut code_block = None::<(usize, String)>;
    let mut code_spans = Vec::new();
    for line in markdown.split_inclusive('\n') {
        let trimmed = line.trim();
        if let Some(fence) = trimmed.strip_prefix("```") {
            if let Some((start, language)) = code_block.take() {
                append_fenced_highlights(&text, start, &language, theme, &mut code_spans);
            } else {
                code_block = Some((text.len(), fence.trim().to_owned()));
            }
            continue;
        }
        text.push_str(line);
    }
    if let Some((start, language)) = code_block {
        append_fenced_highlights(&text, start, &language, theme, &mut code_spans);
    }
    while text.ends_with('\n') {
        text.pop();
    }
    let mut decorations = provider_decorations(highlight_text(&text, "markdown"), theme);
    decorations.extend(code_spans);
    (text, decorations)
}

fn append_fenced_highlights(text: &str, start: usize, language: &str, theme: CatppuccinPalette, output: &mut Vec<DecorationSpan>) {
    output.extend(provider_decorations(highlight_text(&text[start..], normalized_fence_language(language)), theme).into_iter().map(|mut span| {
        span.range = start + span.range.start..start + span.range.end;
        span
    }));
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
