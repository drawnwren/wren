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
    let capabilities = serde_json::from_str(include_str!("lsp-capabilities.json"))?;
    let initialize = client.initialize_with_options(&file_uri(root), capabilities, server.initialization_options.clone())?;
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
    let language = bundled_language_id(path)?;
    let profile = language_tool_profile(language)?.server.as_ref()?;
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let language_id = match extension.as_str() {
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "jsx" => "javascriptreact",
        _ => profile.language_id.as_deref().unwrap_or(language),
    };
    let mut settings = profile.settings.clone();
    match language {
        "python" => settings["python"]["pythonPath"] = python_interpreter(path).map_or(serde_json::Value::Null, serde_json::Value::String),
        "nix" => {
            settings["nixd"]["nixpkgs"]["expr"] = serde_json::Value::String(nixd_nixpkgs_expression(
                nixd_expression_path().as_deref(),
                &env::var("NIXD_NIXPKGS_INPUT").unwrap_or_else(|_| "nixpkgs".to_owned()),
            ));
        }
        _ => {}
    }
    LanguageServerInvocation {
        program: profile.program.to_string(),
        arguments: profile.arguments.iter().map(ToString::to_string).collect(),
        language_id: language_id.to_owned(),
        initialization_options: profile.initialization_options.clone(),
        settings,
    }
    .into()
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
    env::var_os("VIRTUAL_ENV")
        .map(PathBuf::from)
        .map(|directory| directory.join("bin/python"))
        .filter(|candidate| candidate.is_file())
        .or_else(|| {
            path.ancestors()
                .skip(1)
                .flat_map(|directory| [".venv", "venv"].map(|name| directory.join(name).join("bin/python")))
                .find(|candidate| candidate.is_file())
        })
        .or_else(|| {
            env::var_os("PATH").and_then(|path| env::split_paths(&path).map(|directory| directory.join("python3")).find(|candidate| candidate.is_file()))
        })
        .map(|candidate| candidate.to_string_lossy().into_owned())
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
    uri.to_file_path().map(std::borrow::Cow::into_owned).ok_or_else(|| anyhow!("file LSP URI omitted a path: {}", uri.as_str()))
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
        serde_json::Value::Object(values) => ["value", "label", "contents", "signatures", "documentation"]
            .into_iter()
            .filter_map(|key| values.get(key))
            .map(render_lsp_text)
            .find(|rendered| !rendered.is_empty())
            .unwrap_or_default(),
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
            match (placeholder.split_once(':'), placeholder.split_once('|')) {
                (Some((_, default)), _) => output.push_str(default),
                (None, Some((_, choices))) => output.push_str(choices.trim_end_matches('|').split(',').next().unwrap_or_default()),
                (None, None) => {}
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
