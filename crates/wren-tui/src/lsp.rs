use super::*;
use ls_types::{Location, LocationLink, Range as LspRange, SemanticTokens, Uri};

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum LspLocations {
    Location(Location),
    Locations(Vec<Location>),
    Link(LocationLink),
    Links(Vec<LocationLink>),
}

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
    let spec = WorkflowTaskSpec::persisted(
        server.program.clone(),
        server.arguments.iter().cloned().map(String::into_boxed_str).collect(),
        environment,
        16 * 1024 * 1024,
    );
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
        client.notify("workspace/didChangeConfiguration", serde_json::json!({"settings": server.settings}))?;
    }
    let uri = file_uri(path);
    client.did_open(&uri, &server.language_id, i64::try_from(revision.get()).unwrap_or(i64::MAX), text)?;
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
        token_types: legend.get("tokenTypes")?.as_array()?.iter().filter_map(serde_json::Value::as_str).map(str::to_owned).collect(),
        token_modifiers: legend.get("tokenModifiers")?.as_array()?.iter().filter_map(serde_json::Value::as_str).map(str::to_owned).collect(),
    })
}

pub(super) fn parse_semantic_tokens(text: &str, response: &serde_json::Value, legend: &SemanticTokenLegend) -> Vec<HighlightSpan> {
    let Ok(Some(tokens)) = serde_json::from_value::<Option<SemanticTokens>>(response.clone()) else {
        return Vec::new();
    };
    let line_starts = std::iter::once(0).chain(text.match_indices('\n').map(|(byte, _)| byte.saturating_add(1))).collect::<Vec<_>>();
    let mut spans = Vec::with_capacity(tokens.data.len());
    let mut line = 0_u32;
    let mut character = 0_u32;
    for token in tokens.data {
        line = line.saturating_add(token.delta_line);
        character = if token.delta_line == 0 { character.saturating_add(token.delta_start) } else { token.delta_start };
        let Some(start) = lsp_position_byte_indexed(text, &line_starts, line, character) else {
            continue;
        };
        let Some(end) = lsp_position_byte_indexed(text, &line_starts, line, character.saturating_add(token.length)) else {
            continue;
        };
        if start >= end {
            continue;
        }
        let Some(token_type) = usize::try_from(token.token_type).ok().and_then(|index| legend.token_types.get(index)).map(String::as_str) else {
            continue;
        };
        let has_modifier = |name: &str| {
            legend
                .token_modifiers
                .iter()
                .position(|modifier| modifier == name)
                .and_then(|index| u32::try_from(index).ok())
                .is_some_and(|index| token.token_modifiers_bitset & 1_u32.checked_shl(index).unwrap_or(0) != 0)
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
        spans.push(HighlightSpan::new(start..end, kind, u32::MAX));
    }
    spans
}

fn lsp_position_byte_indexed(text: &str, line_starts: &[usize], line: u32, character: u32) -> Option<usize> {
    let line = usize::try_from(line).ok()?;
    let column = usize::try_from(character).ok()?;
    utf16_position_to_byte(text, line_starts, line, column).ok()
}

pub(super) fn language_server_invocation(path: Option<&Path>) -> Option<LanguageServerInvocation> {
    let path = path?;
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let (program, arguments, language_id, initialization_options, settings) = match bundled_language_id(path)? {
        "rust" => ("rust-analyzer", &[][..], "rust", serde_json::Value::Null, serde_json::json!({"rust-analyzer": {"check": {"command": "clippy"}}})),
        "javascript" | "typescript" | "tsx" => (
            "pnpm",
            &["exec", "typescript-language-server", "--stdio"][..],
            match extension.as_str() {
                "ts" | "mts" | "cts" => "typescript",
                "tsx" => "typescriptreact",
                "jsx" => "javascriptreact",
                _ => "javascript",
            },
            serde_json::json!({"jsx": {"enabled": true}}),
            serde_json::json!({
                "typescript": {
                    "suggestionActions": {"enabled": true},
                    "updateImportsOnFileMove": {"enabled": "always"}
                },
                "javascript": {"updateImportsOnFileMove": {"enabled": "always"}}
            }),
        ),
        "python" => (
            "basedpyright-langserver",
            &["--stdio"][..],
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
        ),
        "go" => ("gopls", &[][..], "go", serde_json::Value::Null, serde_json::json!({})),
        "hcl" => ("terraform-ls", &["serve"][..], "terraform", serde_json::Value::Null, serde_json::json!({})),
        "nix" => (
            "nixd",
            &[][..],
            "nix",
            serde_json::Value::Null,
            serde_json::json!({"nixd": {
                "nixpkgs": {"expr": nixd_nixpkgs_expression(
                    nixd_expression_path().as_deref(),
                    &env::var("NIXD_NIXPKGS_INPUT").unwrap_or_else(|_| "nixpkgs".to_owned())
                )},
                "formatting": {"command": ["nixfmt"]}
            }}),
        ),
        "haskell" => (
            "haskell-language-server-wrapper",
            &["--lsp"][..],
            "haskell",
            serde_json::Value::Null,
            serde_json::json!({"haskell": {
                "plugin": {
                    "hlint": {"diagnosticsOn": true, "codeActionsOn": true},
                    "fourmolu": {"config": {"external": true}}
                },
                "formattingProvider": "fourmolu"
            }}),
        ),
        "lua" => (
            "lua-language-server",
            &[][..],
            "lua",
            serde_json::Value::Null,
            serde_json::json!({"Lua": {
                "runtime": {"version": "LuaJIT"},
                "workspace": {"checkThirdParty": false}
            }}),
        ),
        "bash" => ("bash-language-server", &["start"][..], "shellscript", serde_json::Value::Null, serde_json::json!({})),
        "c" | "cpp" => ("clangd", &[][..], bundled_language_id(path)?, serde_json::Value::Null, serde_json::json!({})),
        _ => return None,
    };
    Some(language_server(program, arguments, language_id, initialization_options, settings))
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
        arguments: arguments.iter().map(|argument| (*argument).to_owned()).collect(),
        language_id: language_id.to_owned(),
        initialization_options,
        settings,
    }
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
    Uri::from_file_path(path).map_or_else(String::new, |uri| uri.as_str().to_owned())
}

pub(super) fn path_from_file_uri(uri: &str) -> Result<PathBuf> {
    let uri = uri.parse::<Uri>()?;
    if uri.scheme().as_str() != "file" {
        bail!("unsupported non-file LSP URI {}", uri.as_str());
    }
    uri.to_file_path().map(|path| path.into_owned()).ok_or_else(|| anyhow!("file LSP URI omitted a path: {}", uri.as_str()))
}

pub(super) fn parse_lsp_locations(value: &serde_json::Value) -> Result<Vec<QuickfixEntry>> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    if value.as_object().is_some_and(|location| !location.contains_key("uri") && !location.contains_key("targetUri")) {
        bail!("LSP location omitted URI");
    }
    let response = serde_json::from_value::<LspLocations>(value.clone())?;
    match response {
        LspLocations::Location(location) => Ok(vec![quickfix_location(&location.uri, location.range)?]),
        LspLocations::Locations(locations) => locations.into_iter().map(|location| quickfix_location(&location.uri, location.range)).collect(),
        LspLocations::Link(location) => Ok(vec![quickfix_location(&location.target_uri, location.target_selection_range)?]),
        LspLocations::Links(locations) => {
            locations.into_iter().map(|location| quickfix_location(&location.target_uri, location.target_selection_range)).collect()
        }
    }
}

fn quickfix_location(uri: &Uri, range: LspRange) -> Result<QuickfixEntry> {
    Ok(QuickfixEntry::new(path_from_file_uri(uri.as_str())?, range.start.line as usize + 1, range.start.character as usize + 1, "language-server location")
        .utf16()
        .with_end(range.end.line as usize + 1, range.end.character as usize + 1))
}

pub(super) fn render_lsp_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Array(values) => values.iter().map(render_lsp_text).filter(|value| !value.is_empty()).collect::<Vec<_>>().join(" · "),
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
        if bytes[index] == b'\\' && bytes.get(index + 1).is_some_and(|next| matches!(next, b'$' | b'}' | b'\\')) {
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
                output.push_str(choices.trim_end_matches('|').split(',').next().unwrap_or_default());
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
