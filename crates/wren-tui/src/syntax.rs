use super::*;

pub(super) fn provider_decoration(span: HighlightSpan, theme: CatppuccinPalette) -> DecorationSpan {
    let style = match span.kind.as_ref() {
        kind if kind.starts_with("keyword")
            || matches!(
                kind,
                "conditional"
                    | "repeat"
                    | "exception"
                    | "type.qualifier"
                    | "type.definition"
                    | "storage"
                    | "storageclass"
            ) =>
        {
            CellStyle {
                bold: true,
                foreground: Some(CellColor::Rgb(theme.mauve)),
                ..CellStyle::default()
            }
        }
        kind if kind.starts_with("comment") => CellStyle {
            italic: true,
            foreground: Some(CellColor::Rgb(theme.overlay2)),
            ..CellStyle::default()
        },
        kind if kind.starts_with("preproc")
            || kind.starts_with("attribute")
            || kind.starts_with("decorator")
            || matches!(kind, "include" | "constant.macro" | "function.macro") =>
        {
            CellStyle {
                foreground: Some(CellColor::Rgb(theme.pink)),
                ..CellStyle::default()
            }
        }
        "operator" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.sky)),
            ..CellStyle::default()
        },
        kind if kind.starts_with("punctuation") => CellStyle {
            foreground: Some(CellColor::Rgb(theme.overlay2)),
            ..CellStyle::default()
        },
        kind if kind.starts_with("string.escape")
            || kind.starts_with("string.special")
            || kind == "escape" =>
        {
            CellStyle {
                foreground: Some(CellColor::Rgb(theme.pink)),
                ..CellStyle::default()
            }
        }
        kind if kind.starts_with("string") => CellStyle {
            foreground: Some(CellColor::Rgb(theme.green)),
            ..CellStyle::default()
        },
        kind if kind.starts_with("character.special") => CellStyle {
            foreground: Some(CellColor::Rgb(theme.pink)),
            ..CellStyle::default()
        },
        kind if kind.starts_with("character") => CellStyle {
            foreground: Some(CellColor::Rgb(theme.teal)),
            ..CellStyle::default()
        },
        kind if kind == "boolean"
            || kind == "float"
            || kind.starts_with("number")
            || kind.starts_with("constant") =>
        {
            CellStyle {
                foreground: Some(CellColor::Rgb(theme.peach)),
                ..CellStyle::default()
            }
        }
        "type.builtin" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.mauve)),
            ..CellStyle::default()
        },
        kind if kind.starts_with("type") || kind == "constructor" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.yellow)),
            ..CellStyle::default()
        },
        "function.builtin" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.peach)),
            ..CellStyle::default()
        },
        kind if kind.starts_with("function") || kind.starts_with("method") || kind == "tag" => {
            CellStyle {
                foreground: Some(CellColor::Rgb(theme.blue)),
                ..CellStyle::default()
            }
        }
        "variable.parameter.builtin" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.pink)),
            ..CellStyle::default()
        },
        kind if kind == "parameter" || kind.starts_with("variable.parameter") => CellStyle {
            foreground: Some(CellColor::Rgb(theme.maroon)),
            ..CellStyle::default()
        },
        kind if matches!(kind, "property" | "field") || kind.starts_with("variable.member") => {
            CellStyle {
                foreground: Some(CellColor::Rgb(theme.lavender)),
                ..CellStyle::default()
            }
        }
        "semantic.enum-member" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.teal)),
            ..CellStyle::default()
        },
        "semantic.event" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.yellow)),
            ..CellStyle::default()
        },
        "semantic.regexp" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.pink)),
            ..CellStyle::default()
        },
        "semantic.decorator" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.blue)),
            ..CellStyle::default()
        },
        "semantic.namespace" => CellStyle {
            italic: true,
            foreground: Some(CellColor::Rgb(theme.yellow)),
            ..CellStyle::default()
        },
        "tag.attribute" => CellStyle {
            italic: true,
            foreground: Some(CellColor::Rgb(theme.yellow)),
            ..CellStyle::default()
        },
        "variable.builtin" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.red)),
            ..CellStyle::default()
        },
        "namespace" | "module" | "module.builtin" => CellStyle {
            italic: true,
            foreground: Some(CellColor::Rgb(theme.yellow)),
            ..CellStyle::default()
        },
        kind if kind.starts_with("markup.heading") => CellStyle {
            bold: true,
            foreground: Some(CellColor::Rgb(theme.mauve)),
            ..CellStyle::default()
        },
        kind if kind.starts_with("markup.raw") => CellStyle {
            foreground: Some(CellColor::Rgb(theme.green)),
            background: Some(CellColor::Rgb(theme.surface0)),
            ..CellStyle::default()
        },
        kind if kind.starts_with("markup.link") || kind.ends_with(".url") => CellStyle {
            underline: true,
            foreground: Some(CellColor::Rgb(theme.blue)),
            ..CellStyle::default()
        },
        "markup.strong" => CellStyle {
            bold: true,
            foreground: Some(CellColor::Rgb(theme.text)),
            ..CellStyle::default()
        },
        "markup.italic" => CellStyle {
            italic: true,
            foreground: Some(CellColor::Rgb(theme.text)),
            ..CellStyle::default()
        },
        "label" | "symbol" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.sapphire)),
            ..CellStyle::default()
        },
        kind if kind.starts_with("variable") => CellStyle {
            foreground: Some(CellColor::Rgb(theme.text)),
            ..CellStyle::default()
        },
        _ => CellStyle {
            foreground: Some(CellColor::Rgb(theme.text)),
            ..CellStyle::default()
        },
    };
    DecorationSpan {
        range: span.range,
        style,
        priority: span.priority,
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
