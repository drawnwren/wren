use super::*;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct LanguageServerInvocation {
    pub(super) program: String,
    pub(super) arguments: Vec<String>,
    pub(super) language_id: String,
    pub(super) initialization_options: serde_json::Value,
    pub(super) settings: serde_json::Value,
}

pub(super) fn spawn_lsp_client(
    server: &LanguageServerInvocation,
    path: &Path,
    root: &Path,
    revision: DocumentRevision,
    text: &str,
    environment: BTreeMap<Box<str>, Box<str>>,
) -> Result<(LspClient, String, LspCapabilities)> {
    let spec = WorkflowTaskSpec {
        program: server.program.clone().into(),
        arguments: server
            .arguments
            .iter()
            .cloned()
            .map(String::into_boxed_str)
            .collect(),
        environment,
        visibility: DocumentVisibility::Persisted,
        save: SavePolicy::Never,
        max_output_bytes: 16 * 1024 * 1024,
    };
    let mut client = LspClient::spawn(&spec, true, 16 * 1024 * 1024)?;
    let initialize = client.initialize_with_options(
        &file_uri(root),
        serde_json::json!({
            "workspace": {"workspaceFolders": true},
            "textDocument": {
                "hover": {"contentFormat": ["markdown", "plaintext"]},
                "signatureHelp": {"signatureInformation": {"documentationFormat": ["markdown", "plaintext"]}},
                "completion": {"completionItem": {"snippetSupport": true, "documentationFormat": ["markdown", "plaintext"]}},
                "publishDiagnostics": {"relatedInformation": true},
                "codeAction": {"dynamicRegistration": true},
                "rename": {"prepareSupport": true},
                "semanticTokens": {
                    "dynamicRegistration": true,
                    "requests": {"range": false, "full": true},
                    "tokenTypes": [
                        "namespace", "type", "class", "enum", "interface", "struct",
                        "typeParameter", "parameter", "variable", "property", "enumMember",
                        "event", "function", "method", "macro", "keyword", "modifier",
                        "comment", "string", "number", "regexp", "operator", "decorator"
                    ],
                    "tokenModifiers": [
                        "declaration", "definition", "readonly", "static", "deprecated",
                        "abstract", "async", "modification", "documentation", "defaultLibrary"
                    ],
                    "formats": ["relative"],
                    "overlappingTokenSupport": false,
                    "multilineTokenSupport": false
                }
            }
        }),
        server.initialization_options.clone(),
    )?;
    if !server.settings.is_null() {
        client.notify(
            "workspace/didChangeConfiguration",
            serde_json::json!({"settings": server.settings}),
        )?;
    }
    let uri = file_uri(path);
    client.did_open(
        &uri,
        &server.language_id,
        i64::try_from(revision.get()).unwrap_or(i64::MAX),
        text,
    )?;
    Ok((client, uri, lsp_capabilities(&initialize)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SemanticTokenLegend {
    pub(super) token_types: Vec<String>,
    pub(super) token_modifiers: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct LspNavigationCapabilities {
    pub(super) declaration: bool,
    pub(super) definition: bool,
    pub(super) implementation: bool,
    pub(super) references: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct LspCapabilities {
    pub(super) semantic_legend: Option<SemanticTokenLegend>,
    pub(super) navigation: LspNavigationCapabilities,
}

pub(super) fn lsp_capabilities(initialize: &serde_json::Value) -> LspCapabilities {
    let provider_enabled = |name: &str| {
        initialize
            .get("capabilities")
            .and_then(|capabilities| capabilities.get(name))
            .is_some_and(|provider| provider.as_bool().unwrap_or_else(|| provider.is_object()))
    };
    LspCapabilities {
        semantic_legend: semantic_token_legend(initialize),
        navigation: LspNavigationCapabilities {
            declaration: provider_enabled("declarationProvider"),
            definition: provider_enabled("definitionProvider"),
            implementation: provider_enabled("implementationProvider"),
            references: provider_enabled("referencesProvider"),
        },
    }
}

pub(super) fn semantic_token_legend(initialize: &serde_json::Value) -> Option<SemanticTokenLegend> {
    let legend = initialize.pointer("/capabilities/semanticTokensProvider/legend")?;
    Some(SemanticTokenLegend {
        token_types: legend
            .get("tokenTypes")?
            .as_array()?
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect(),
        token_modifiers: legend
            .get("tokenModifiers")?
            .as_array()?
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect(),
    })
}

pub(super) fn parse_semantic_tokens(
    text: &str,
    response: &serde_json::Value,
    legend: &SemanticTokenLegend,
) -> Vec<HighlightSpan> {
    let Some(data) = response.get("data").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let line_starts = std::iter::once(0)
        .chain(
            text.match_indices('\n')
                .map(|(byte, _)| byte.saturating_add(1)),
        )
        .collect::<Vec<_>>();
    let mut spans = Vec::with_capacity(data.len() / 5);
    let mut line = 0_u32;
    let mut character = 0_u32;
    for token in data.chunks_exact(5) {
        let Some(delta_line) = token[0]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
        else {
            continue;
        };
        let Some(delta_start) = token[1]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
        else {
            continue;
        };
        let Some(length) = token[2]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
        else {
            continue;
        };
        let Some(token_type) = token[3]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
        else {
            continue;
        };
        let modifiers = token[4].as_u64().unwrap_or(0);
        line = line.saturating_add(delta_line);
        character = if delta_line == 0 {
            character.saturating_add(delta_start)
        } else {
            delta_start
        };
        let Some(start) = lsp_position_byte_indexed(text, &line_starts, line, character) else {
            continue;
        };
        let Some(end) =
            lsp_position_byte_indexed(text, &line_starts, line, character.saturating_add(length))
        else {
            continue;
        };
        if start >= end {
            continue;
        }
        let Some(token_type) = legend.token_types.get(token_type).map(String::as_str) else {
            continue;
        };
        let has_modifier = |name: &str| {
            legend
                .token_modifiers
                .iter()
                .position(|modifier| modifier == name)
                .and_then(|index| u32::try_from(index).ok())
                .is_some_and(|index| modifiers & 1_u64.checked_shl(index).unwrap_or(0) != 0)
        };
        let kind = match token_type {
            "namespace" => "semantic.namespace",
            "type" | "class" | "enum" | "interface" | "typeParameter" => "type",
            "parameter" => "parameter",
            "variable" if has_modifier("defaultLibrary") => "variable.builtin",
            "variable" if has_modifier("readonly") => "constant",
            "variable" => "variable",
            "property" => "property",
            "event" => "semantic.event",
            "enumMember" => "semantic.enum-member",
            "function" if has_modifier("defaultLibrary") => "function.builtin",
            "function" => "function",
            "method" => "method",
            "keyword" => "keyword",
            "modifier" => "type.qualifier",
            "comment" => "comment",
            "string" => "string",
            "regexp" => "semantic.regexp",
            "number" => "number",
            "operator" => "operator",
            "decorator" => "semantic.decorator",
            // Catppuccin deliberately leaves these and rust-analyzer's
            // extended token types transparent. Omitting the semantic span
            // preserves the more specific Tree-sitter capture underneath.
            _ => continue,
        };
        spans.push(HighlightSpan {
            range: start..end,
            kind: kind.into(),
            priority: u32::MAX,
        });
    }
    spans
}

fn lsp_position_byte_indexed(
    text: &str,
    line_starts: &[usize],
    line: u32,
    character: u32,
) -> Option<usize> {
    let line = usize::try_from(line).ok()?;
    let start = *line_starts.get(line)?;
    let end = line_starts
        .get(line.saturating_add(1))
        .map_or(text.len(), |next| next.saturating_sub(1));
    let wanted = usize::try_from(character).ok()?;
    let mut utf16 = 0;
    for (offset, current) in text[start..end].char_indices() {
        if utf16 == wanted {
            return Some(start + offset);
        }
        utf16 = utf16.saturating_add(current.len_utf16());
        if utf16 > wanted {
            return None;
        }
    }
    (utf16 == wanted).then_some(end)
}

type LanguageServerFactory = fn(&Path, &str) -> LanguageServerInvocation;

struct LanguageServerRule {
    extensions: &'static [&'static str],
    build: LanguageServerFactory,
}

const LANGUAGE_SERVER_RULES: &[LanguageServerRule] = &[
    LanguageServerRule {
        extensions: &["rs"],
        build: rust_language_server,
    },
    LanguageServerRule {
        extensions: &["js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx"],
        build: typescript_language_server,
    },
    LanguageServerRule {
        extensions: &["py", "pyi"],
        build: python_language_server,
    },
    LanguageServerRule {
        extensions: &["go"],
        build: go_language_server,
    },
    LanguageServerRule {
        extensions: &["tf", "tfvars"],
        build: terraform_language_server,
    },
    LanguageServerRule {
        extensions: &["nix"],
        build: nix_language_server,
    },
    LanguageServerRule {
        extensions: &["hs", "lhs"],
        build: haskell_language_server,
    },
    LanguageServerRule {
        extensions: &["lua"],
        build: lua_language_server,
    },
    LanguageServerRule {
        extensions: &["sh", "bash", "zsh"],
        build: bash_language_server,
    },
    LanguageServerRule {
        extensions: &["c", "h"],
        build: c_language_server,
    },
    LanguageServerRule {
        extensions: &["cc", "cpp", "cxx", "hh", "hpp", "hxx", "msg"],
        build: cpp_language_server,
    },
];

pub(super) fn language_server_invocation(path: Option<&Path>) -> Option<LanguageServerInvocation> {
    let path = path?;
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    LANGUAGE_SERVER_RULES
        .iter()
        .find(|rule| rule.extensions.contains(&extension.as_str()))
        .map(|rule| (rule.build)(path, &extension))
}

fn language_server(
    program: &str,
    arguments: &[&str],
    language_id: &str,
    initialization_options: serde_json::Value,
    settings: serde_json::Value,
) -> LanguageServerInvocation {
    LanguageServerInvocation {
        program: program.to_owned(),
        arguments: arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
        language_id: language_id.to_owned(),
        initialization_options,
        settings,
    }
}

fn rust_language_server(_: &Path, _: &str) -> LanguageServerInvocation {
    language_server(
        "rust-analyzer",
        &[],
        "rust",
        serde_json::Value::Null,
        serde_json::json!({"rust-analyzer": {"check": {"command": "clippy"}}}),
    )
}

fn typescript_language_server(_: &Path, extension: &str) -> LanguageServerInvocation {
    let language_id = match extension {
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "jsx" => "javascriptreact",
        _ => "javascript",
    };
    language_server(
        "pnpm",
        &["exec", "typescript-language-server", "--stdio"],
        language_id,
        serde_json::json!({"jsx": {"enabled": true}}),
        serde_json::json!({
            "typescript": {
                "suggestionActions": {"enabled": true},
                "updateImportsOnFileMove": {"enabled": "always"}
            },
            "javascript": {"updateImportsOnFileMove": {"enabled": "always"}}
        }),
    )
}

fn python_language_server(path: &Path, _: &str) -> LanguageServerInvocation {
    language_server(
        "basedpyright-langserver",
        &["--stdio"],
        "python",
        serde_json::Value::Null,
        serde_json::json!({
            "python": {"pythonPath": python_interpreter(path)},
            "basedpyright": {"analysis": {
                "autoSearchPaths": true,
                "useLibraryCodeForTypes": true,
                "diagnosticMode": "workspace",
                "inlayHints": {
                    "variableTypes": true,
                    "callArgumentNames": true,
                    "functionReturnTypes": true,
                    "parameterNames": true
                }
            }}
        }),
    )
}

fn go_language_server(_: &Path, _: &str) -> LanguageServerInvocation {
    language_server(
        "gopls",
        &[],
        "go",
        serde_json::Value::Null,
        serde_json::json!({}),
    )
}

fn terraform_language_server(_: &Path, _: &str) -> LanguageServerInvocation {
    language_server(
        "terraform-ls",
        &["serve"],
        "terraform",
        serde_json::Value::Null,
        serde_json::json!({}),
    )
}

fn nix_language_server(_: &Path, _: &str) -> LanguageServerInvocation {
    let input = env::var("NIXD_NIXPKGS_INPUT").unwrap_or_else(|_| "nixpkgs".to_owned());
    let expression = nixd_nixpkgs_expression(nixd_expression_path().as_deref(), &input);
    language_server(
        "nixd",
        &[],
        "nix",
        serde_json::Value::Null,
        serde_json::json!({"nixd": {
            "nixpkgs": {"expr": expression},
            "formatting": {"command": ["nixfmt"]}
        }}),
    )
}

fn haskell_language_server(_: &Path, _: &str) -> LanguageServerInvocation {
    language_server(
        "haskell-language-server-wrapper",
        &["--lsp"],
        "haskell",
        serde_json::Value::Null,
        serde_json::json!({"haskell": {
            "plugin": {
                "hlint": {"diagnosticsOn": true, "codeActionsOn": true},
                "fourmolu": {"config": {"external": true}}
            },
            "formattingProvider": "fourmolu"
        }}),
    )
}

fn lua_language_server(_: &Path, _: &str) -> LanguageServerInvocation {
    language_server(
        "lua-language-server",
        &[],
        "lua",
        serde_json::Value::Null,
        serde_json::json!({"Lua": {
            "runtime": {"version": "LuaJIT"},
            "workspace": {"checkThirdParty": false}
        }}),
    )
}

fn bash_language_server(_: &Path, _: &str) -> LanguageServerInvocation {
    language_server(
        "bash-language-server",
        &["start"],
        "shellscript",
        serde_json::Value::Null,
        serde_json::json!({}),
    )
}

fn c_language_server(_: &Path, _: &str) -> LanguageServerInvocation {
    language_server(
        "clangd",
        &[],
        "c",
        serde_json::Value::Null,
        serde_json::json!({}),
    )
}

fn cpp_language_server(_: &Path, _: &str) -> LanguageServerInvocation {
    language_server(
        "clangd",
        &[],
        "cpp",
        serde_json::Value::Null,
        serde_json::json!({}),
    )
}

pub(super) fn nixd_nixpkgs_expression(config: Option<&Path>, input: &str) -> String {
    config.map_or_else(
        || "import <nixpkgs> { }".to_owned(),
        |config| {
            format!(
                "let ctx = import {} {{ self = \"dummy\"; }}; in if ctx.local != null then ctx.local.inputs.{input} else import <nixpkgs> {{ }}",
                config.display()
            )
        },
    )
}

pub(super) fn python_interpreter(path: &Path) -> Option<String> {
    if let Some(virtual_environment) = env::var_os("VIRTUAL_ENV") {
        let candidate = PathBuf::from(virtual_environment).join("bin/python");
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    let mut directory = path.parent();
    while let Some(current) = directory {
        for name in [".venv", "venv"] {
            let candidate = current.join(name).join("bin/python");
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
        directory = current.parent();
    }
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join("python3"))
            .find(|candidate| candidate.is_file())
            .map(|candidate| candidate.to_string_lossy().into_owned())
    })
}

pub(super) fn nixd_expression_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(config) = env::var_os("XDG_CONFIG_HOME") {
        candidates.push(PathBuf::from(config).join("nvim/nixd/_nixd-expr.nix"));
    }
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        candidates.push(home.join(".config/nvim/nixd/_nixd-expr.nix"));
        candidates.push(home.join("nixfiles/config/nvim/nixd/_nixd-expr.nix"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

pub(super) fn file_uri(path: &Path) -> String {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut encoded = String::new();
    for byte in path.to_string_lossy().bytes() {
        if byte == b'/' || byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
        {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("file://{encoded}")
}

pub(super) fn path_from_file_uri(uri: &str) -> Result<PathBuf> {
    let encoded = uri
        .strip_prefix("file://")
        .ok_or_else(|| anyhow!("unsupported non-file LSP URI {uri}"))?;
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let value = std::str::from_utf8(&bytes[index + 1..index + 3])?;
            decoded.push(u8::from_str_radix(value, 16)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Ok(PathBuf::from(String::from_utf8(decoded)?))
}

pub(super) fn parse_lsp_locations(value: &serde_json::Value) -> Result<Vec<QuickfixEntry>> {
    let values = value
        .as_array()
        .map_or_else(|| vec![value], |values| values.iter().collect());
    values
        .into_iter()
        .filter(|value| !value.is_null())
        .map(|location| {
            let uri = location
                .get("targetUri")
                .or_else(|| location.get("uri"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow!("LSP location omitted URI"))?;
            let range = location
                .get("targetSelectionRange")
                .or_else(|| location.get("range"))
                .or_else(|| location.get("targetRange"))
                .ok_or_else(|| anyhow!("LSP location omitted range"))?;
            let line = range
                .pointer("/start/line")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(0)
                + 1;
            let column = range
                .pointer("/start/character")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(0)
                + 1;
            let selection_end = range
                .pointer("/end/line")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .zip(
                    range
                        .pointer("/end/character")
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|value| usize::try_from(value).ok()),
                )
                .map(|(line, column)| QuickfixPosition {
                    line: line + 1,
                    column: column + 1,
                });
            Ok(QuickfixEntry {
                path: path_from_file_uri(uri)?,
                line,
                column,
                selection_end,
                column_utf16: true,
                text: "language-server location".to_owned(),
            })
        })
        .collect()
}

pub(super) fn render_lsp_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Array(values) => values
            .iter()
            .map(render_lsp_text)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(" · "),
        serde_json::Value::Object(values) => {
            for key in ["value", "label", "contents", "signatures", "documentation"] {
                if let Some(value) = values.get(key) {
                    let rendered = render_lsp_text(value);
                    if !rendered.is_empty() {
                        return rendered;
                    }
                }
            }
            String::new()
        }
        _ => value.to_string(),
    }
}

pub(super) fn expand_lsp_snippet(snippet: &str) -> String {
    expand_lsp_snippet_with_stops(snippet).0
}

pub(super) fn expand_lsp_snippet_with_stops(snippet: &str) -> (String, Vec<Range<usize>>) {
    let bytes = snippet.as_bytes();
    let mut output = String::new();
    let mut stops = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && bytes
                .get(index + 1)
                .is_some_and(|next| matches!(next, b'$' | b'}' | b'\\'))
        {
            output.push(char::from(bytes[index + 1]));
            index += 2;
            continue;
        }
        if bytes[index] != b'$' {
            let character = snippet[index..].chars().next().unwrap_or_default();
            output.push(character);
            index += character.len_utf8();
            continue;
        }
        if bytes.get(index + 1) == Some(&b'{') {
            let Some(relative_end) = snippet[index + 2..].find('}') else {
                output.push('$');
                index += 1;
                continue;
            };
            let end = index + 2 + relative_end;
            let placeholder = &snippet[index + 2..end];
            let digits = placeholder.bytes().take_while(u8::is_ascii_digit).count();
            let stop = placeholder[..digits].parse::<usize>().ok();
            let start = output.len();
            if let Some((_, default)) = placeholder.split_once(':') {
                output.push_str(default);
            } else if let Some((_, choices)) = placeholder.split_once('|') {
                output.push_str(
                    choices
                        .trim_end_matches('|')
                        .split(',')
                        .next()
                        .unwrap_or_default(),
                );
            }
            if let Some(stop) = stop {
                stops.push((stop, start..output.len()));
            }
            index = end + 1;
            continue;
        }
        index += 1;
        let digit_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if digit_start < index
            && let Ok(stop) = snippet[digit_start..index].parse::<usize>()
        {
            stops.push((stop, output.len()..output.len()));
        }
    }
    stops.sort_by_key(|(stop, _)| (*stop == 0, *stop));
    stops.dedup_by_key(|(stop, _)| *stop);
    (output, stops.into_iter().map(|(_, range)| range).collect())
}
