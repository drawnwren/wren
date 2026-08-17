use super::*;

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
        self.exact.contains(&kind)
            || self.prefixes.iter().any(|prefix| kind.starts_with(prefix))
            || self.suffixes.iter().any(|suffix| kind.ends_with(suffix))
    }
}

const HIGHLIGHT_RULES: &[HighlightRule] = &[
    HighlightRule {
        style: HighlightStyle::Keyword,
        exact: &[
            "conditional",
            "repeat",
            "exception",
            "type.qualifier",
            "type.definition",
            "storage",
            "storageclass",
        ],
        prefixes: &["keyword"],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Comment,
        exact: &[],
        prefixes: &["comment"],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Pink,
        exact: &["include", "constant.macro", "function.macro"],
        prefixes: &["preproc", "attribute", "decorator"],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Sky,
        exact: &["operator"],
        prefixes: &[],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Overlay,
        exact: &[],
        prefixes: &["punctuation"],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Pink,
        exact: &["escape"],
        prefixes: &["string.escape", "string.special"],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Green,
        exact: &[],
        prefixes: &["string"],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Pink,
        exact: &[],
        prefixes: &["character.special"],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Teal,
        exact: &[],
        prefixes: &["character"],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Peach,
        exact: &["boolean", "float"],
        prefixes: &["number", "constant"],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Mauve,
        exact: &["type.builtin"],
        prefixes: &[],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Yellow,
        exact: &["constructor"],
        prefixes: &["type"],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Peach,
        exact: &["function.builtin"],
        prefixes: &[],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Blue,
        exact: &["tag"],
        prefixes: &["function", "method"],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Pink,
        exact: &["variable.parameter.builtin"],
        prefixes: &[],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Maroon,
        exact: &["parameter"],
        prefixes: &["variable.parameter"],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Lavender,
        exact: &["property", "field"],
        prefixes: &["variable.member"],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Teal,
        exact: &["semantic.enum-member"],
        prefixes: &[],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Yellow,
        exact: &["semantic.event"],
        prefixes: &[],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Pink,
        exact: &["semantic.regexp"],
        prefixes: &[],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Blue,
        exact: &["semantic.decorator"],
        prefixes: &[],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::ItalicYellow,
        exact: &[
            "semantic.namespace",
            "tag.attribute",
            "namespace",
            "module",
            "module.builtin",
        ],
        prefixes: &[],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Red,
        exact: &["variable.builtin"],
        prefixes: &[],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Keyword,
        exact: &[],
        prefixes: &["markup.heading"],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Raw,
        exact: &[],
        prefixes: &["markup.raw"],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Link,
        exact: &[],
        prefixes: &["markup.link"],
        suffixes: &[".url"],
    },
    HighlightRule {
        style: HighlightStyle::Strong,
        exact: &["markup.strong"],
        prefixes: &[],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Italic,
        exact: &["markup.italic"],
        prefixes: &[],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Sapphire,
        exact: &["label", "symbol"],
        prefixes: &[],
        suffixes: &[],
    },
    HighlightRule {
        style: HighlightStyle::Text,
        exact: &[],
        prefixes: &["variable"],
        suffixes: &[],
    },
];

pub(super) fn provider_decoration(span: HighlightSpan, theme: CatppuccinPalette) -> DecorationSpan {
    let style = HIGHLIGHT_RULES
        .iter()
        .find(|rule| rule.matches(&span.kind))
        .map_or(HighlightStyle::Text, |rule| rule.style);
    DecorationSpan {
        range: span.range,
        style: highlight_cell_style(style, theme),
        priority: span.priority,
    }
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
    CellStyle {
        bold,
        italic,
        underline,
        foreground: Some(CellColor::Rgb(foreground)),
        background: background.map(CellColor::Rgb),
        ..CellStyle::default()
    }
}

pub(super) fn markdown_decorations(text: &str, theme: CatppuccinPalette) -> Vec<DecorationSpan> {
    let mut spans = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let indentation = line.len().saturating_sub(trimmed.len());
        if trimmed.starts_with('#') {
            spans.push(DecorationSpan {
                range: offset + indentation..offset + line.trim_end().len(),
                priority: 1_100_000,
                style: CellStyle {
                    bold: true,
                    foreground: Some(CellColor::Rgb(theme.mauve)),
                    ..CellStyle::default()
                },
            });
        } else if trimmed.starts_with("> ") {
            spans.push(DecorationSpan {
                range: offset + indentation..offset + line.trim_end().len(),
                priority: 1_100_000,
                style: CellStyle {
                    italic: true,
                    foreground: Some(CellColor::Rgb(theme.overlay2)),
                    ..CellStyle::default()
                },
            });
        }
        for (delimiter, mut style) in [
            (
                "**",
                CellStyle {
                    bold: true,
                    ..CellStyle::default()
                },
            ),
            (
                "~~",
                CellStyle {
                    strikethrough: true,
                    ..CellStyle::default()
                },
            ),
            (
                "`",
                CellStyle {
                    foreground: Some(CellColor::Rgb(theme.green)),
                    background: Some(CellColor::Rgb(theme.surface0)),
                    ..CellStyle::default()
                },
            ),
        ] {
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
                spans.push(DecorationSpan {
                    range: offset + start..offset + end,
                    priority: 1_100_000,
                    style,
                });
                search = end;
            }
        }
        offset += line.len();
    }
    spans
}

pub(super) fn lsp_popup_markdown(
    markdown: &str,
    theme: CatppuccinPalette,
) -> (String, Vec<DecorationSpan>) {
    let mut text = String::new();
    let mut code_block = None::<(usize, String)>;
    let mut code_spans = Vec::new();
    for line in markdown.split_inclusive('\n') {
        let trimmed = line.trim();
        if let Some(fence) = trimmed.strip_prefix("```") {
            if let Some((start, language)) = code_block.take() {
                let _language = normalized_fence_language(&language);
                code_spans.extend(lexical_highlight_text(&text[start..]).into_iter().map(
                    |mut span| {
                        span.range = start + span.range.start..start + span.range.end;
                        provider_decoration(span, theme)
                    },
                ));
            } else {
                code_block = Some((text.len(), fence.trim().to_owned()));
            }
            continue;
        }
        text.push_str(line);
    }
    if let Some((start, language)) = code_block {
        let _language = normalized_fence_language(&language);
        code_spans.extend(
            lexical_highlight_text(&text[start..])
                .into_iter()
                .map(|mut span| {
                    span.range = start + span.range.start..start + span.range.end;
                    provider_decoration(span, theme)
                }),
        );
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
