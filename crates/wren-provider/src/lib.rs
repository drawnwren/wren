#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

#[cfg(feature = "gpu")]
mod gpu;

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::Instant;

use nucleo_matcher::{Config, Matcher, Utf32Str, pattern::Pattern};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use wren_types::{
    Bias, DocumentId, DocumentRevision, Edit, Freshness, FreshnessKey, LanguageBundle,
    ProviderDemand, ProviderGeneration, Transaction,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "kebab-case")]
pub enum ProviderRequest {
    Hello {
        protocol: u32,
    },
    UpdateDocument {
        document_id: DocumentId,
        revision: DocumentRevision,
        text: Box<str>,
        bundle: LanguageBundle,
    },
    Demand {
        document_id: DocumentId,
        demand: ProviderDemand,
    },
    Complete {
        document_id: DocumentId,
        revision: DocumentRevision,
        byte: usize,
    },
    CrashForTest,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "kebab-case")]
pub enum ProviderResponse {
    Hello { protocol: u32 },
    Updated { key: FreshnessKey },
    Highlight(HighlightResult),
    Completion(CompletionResult),
    Error { message: Box<str> },
    Bye,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightSpan {
    pub range: Range<usize>,
    pub kind: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightResult {
    pub key: FreshnessKey,
    pub freshness: Freshness,
    pub spans: Vec<HighlightSpan>,
    pub requested_ranges: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionCandidate {
    pub label: Box<str>,
    pub insert: Box<str>,
    pub source: Box<str>,
    #[serde(default)]
    pub detail: Box<str>,
    #[serde(default)]
    pub documentation: Box<str>,
    #[serde(default)]
    pub replace: Option<Range<usize>>,
    #[serde(default)]
    pub snippet: Option<Box<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionResult {
    pub key: FreshnessKey,
    pub freshness: Freshness,
    pub replace: Range<usize>,
    pub candidates: Vec<CompletionCandidate>,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("provider protocol JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("provider process closed its response stream")]
    Closed,
    #[error("document {0:?} has not been updated")]
    UnknownDocument(DocumentId),
    #[error("provider process executable is missing: {0}")]
    MissingProgram(PathBuf),
    #[error("completion result targets revision {actual:?}, expected {expected:?}")]
    StaleCompletion {
        expected: DocumentRevision,
        actual: DocumentRevision,
    },
}

#[derive(Debug, Clone)]
struct ProviderDocument {
    revision: DocumentRevision,
    text: Box<str>,
    generation: ProviderGeneration,
    syntax_spans: Vec<HighlightSpan>,
    syntax_prefix_max_end: Vec<usize>,
    grammar_backend: GrammarBackend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrammarBackend {
    BundledNative,
    DynamicWasmFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelerationBackend {
    Pending,
    Gpu,
    Cpu,
}

// Below this point dispatch, synchronization, and readback cost more than the
// serial lexer on the benchmarked hardware. Keep interactive viewport work on
// CPU and reserve the GPU for provider-scale batches.
const MIN_GPU_CLASSIFICATION_BYTES: usize = 4 * 1024 * 1024;

struct LexicalAccelerator {
    backend: AccelerationBackend,
    #[cfg(feature = "gpu")]
    gpu: Option<gpu::GpuLexical>,
}

impl std::fmt::Debug for LexicalAccelerator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LexicalAccelerator")
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

impl LexicalAccelerator {
    fn prefer_gpu() -> Self {
        Self {
            backend: if cfg!(feature = "gpu") {
                AccelerationBackend::Pending
            } else {
                AccelerationBackend::Cpu
            },
            #[cfg(feature = "gpu")]
            gpu: None,
        }
    }

    fn cpu_only() -> Self {
        Self {
            backend: AccelerationBackend::Cpu,
            #[cfg(feature = "gpu")]
            gpu: None,
        }
    }

    fn classify(&mut self, text: &str) -> Option<Vec<Range<usize>>> {
        if text.len() < MIN_GPU_CLASSIFICATION_BYTES
            || !text.is_ascii()
            || self.backend == AccelerationBackend::Cpu
        {
            return None;
        }
        #[cfg(not(feature = "gpu"))]
        {
            self.backend = AccelerationBackend::Cpu;
            let _ = text;
            None
        }
        #[cfg(feature = "gpu")]
        {
            if self.backend == AccelerationBackend::Pending {
                let initialized =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(gpu::GpuLexical::new));
                match initialized {
                    Ok(Ok(gpu)) => {
                        self.gpu = Some(gpu);
                        self.backend = AccelerationBackend::Gpu;
                    }
                    Ok(Err(_)) | Err(_) => {
                        self.backend = AccelerationBackend::Cpu;
                        return None;
                    }
                }
            }
            let gpu = self.gpu.as_mut()?;
            if !gpu.supports(text.len()) {
                return None;
            }
            let classified =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| gpu.classify(text)));
            match classified {
                Ok(Ok(ranges)) => Some(ranges),
                Ok(Err(_)) | Err(_) => {
                    self.gpu = None;
                    self.backend = AccelerationBackend::Cpu;
                    None
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct ProviderActor {
    documents: BTreeMap<DocumentId, ProviderDocument>,
    lexical_accelerator: LexicalAccelerator,
}

impl Default for ProviderActor {
    fn default() -> Self {
        Self {
            documents: BTreeMap::new(),
            lexical_accelerator: LexicalAccelerator::prefer_gpu(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueuedDemand {
    pub document_id: DocumentId,
    pub demand: ProviderDemand,
    enqueued_at: Instant,
    sequence: u64,
}

impl QueuedDemand {
    #[must_use]
    pub fn age_micros(&self) -> u64 {
        u64::try_from(self.enqueued_at.elapsed().as_micros()).unwrap_or(u64::MAX)
    }
}

/// Bounded latest-wins scheduler. A document can occupy only one queue slot;
/// newer revisions replace obsolete work before it reaches the provider.
#[derive(Debug)]
pub struct LatestDemandQueue {
    capacity: usize,
    sequence: u64,
    dropped: u64,
    pending: BTreeMap<DocumentId, QueuedDemand>,
}

impl LatestDemandQueue {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            sequence: 0,
            dropped: 0,
            pending: BTreeMap::new(),
        }
    }

    pub fn push(&mut self, document_id: DocumentId, demand: ProviderDemand) {
        self.sequence = self.sequence.saturating_add(1);
        if self.pending.contains_key(&document_id) {
            self.dropped = self.dropped.saturating_add(1);
        } else if self.pending.len() >= self.capacity {
            let eviction = self
                .pending
                .iter()
                .min_by_key(|(_, queued)| (queued.demand.priority, queued.sequence))
                .map(|(id, _)| *id);
            if let Some(eviction) = eviction {
                self.pending.remove(&eviction);
                self.dropped = self.dropped.saturating_add(1);
            }
        }
        self.pending.insert(
            document_id,
            QueuedDemand {
                document_id,
                demand,
                enqueued_at: Instant::now(),
                sequence: self.sequence,
            },
        );
    }

    pub fn pop(&mut self) -> Option<QueuedDemand> {
        let next = self
            .pending
            .iter()
            .max_by_key(|(_, queued)| (queued.demand.priority, std::cmp::Reverse(queued.sequence)))
            .map(|(id, _)| *id)?;
        self.pending.remove(&next)
    }

    #[must_use]
    pub fn depth(&self) -> usize {
        self.pending.len()
    }

    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl ProviderActor {
    #[must_use]
    pub fn cpu_only() -> Self {
        Self {
            documents: BTreeMap::new(),
            lexical_accelerator: LexicalAccelerator::cpu_only(),
        }
    }

    #[must_use]
    pub const fn acceleration_backend(&self) -> AccelerationBackend {
        self.lexical_accelerator.backend
    }

    pub fn handle(&mut self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        match request {
            ProviderRequest::Hello { protocol } => Ok(ProviderResponse::Hello { protocol }),
            ProviderRequest::UpdateDocument {
                document_id,
                revision,
                text,
                bundle,
            } => {
                let generation = bundle.provider_generation();
                let (grammar_backend, mut syntax_spans) =
                    native_tree_sitter_spans(&text, &bundle.language_id).map_or_else(
                        || (GrammarBackend::DynamicWasmFallback, Vec::new()),
                        |spans| (GrammarBackend::BundledNative, spans),
                    );
                syntax_spans.sort_by_key(|span| (span.range.start, span.range.end));
                let mut maximum_end = 0;
                let syntax_prefix_max_end = syntax_spans
                    .iter()
                    .map(|span| {
                        maximum_end = maximum_end.max(span.range.end);
                        maximum_end
                    })
                    .collect();
                self.documents.insert(
                    document_id,
                    ProviderDocument {
                        revision,
                        text,
                        generation,
                        syntax_spans,
                        syntax_prefix_max_end,
                        grammar_backend,
                    },
                );
                Ok(ProviderResponse::Updated {
                    key: document_key(document_id, revision, generation),
                })
            }
            ProviderRequest::Demand {
                document_id,
                demand,
            } => self
                .highlight(document_id, demand)
                .map(ProviderResponse::Highlight),
            ProviderRequest::Complete {
                document_id,
                revision,
                byte,
            } => self
                .complete(document_id, revision, byte)
                .map(ProviderResponse::Completion),
            ProviderRequest::CrashForTest => panic!("injected provider process crash"),
            ProviderRequest::Shutdown => Ok(ProviderResponse::Bye),
        }
    }

    fn highlight(
        &mut self,
        document_id: DocumentId,
        demand: ProviderDemand,
    ) -> Result<HighlightResult, ProviderError> {
        let (documents, lexical_accelerator) = (&self.documents, &mut self.lexical_accelerator);
        let document = documents
            .get(&document_id)
            .ok_or(ProviderError::UnknownDocument(document_id))?;
        let freshness = revision_freshness(document.revision, demand.revision);
        let mut requested_ranges = demand.visible;
        requested_ranges.extend(demand.near_viewport);
        requested_ranges = coalesce_ranges(requested_ranges, document.text.len());
        let mut spans = Vec::new();
        for range in &requested_ranges {
            let first = document
                .syntax_prefix_max_end
                .partition_point(|maximum_end| *maximum_end <= range.start);
            let last = document
                .syntax_spans
                .partition_point(|span| span.range.start < range.end);
            spans.extend(
                document.syntax_spans[first..last]
                    .iter()
                    .filter(|span| span.range.start < range.end && range.start < span.range.end)
                    .cloned(),
            );
        }
        if document.grammar_backend == GrammarBackend::DynamicWasmFallback
            || (document.grammar_backend == GrammarBackend::BundledNative && spans.is_empty())
        {
            for range in &requested_ranges {
                let source = &document.text[range.clone()];
                if let Some(ranges) = lexical_accelerator.classify(source) {
                    spans.extend(ranges.into_iter().map(|span| HighlightSpan {
                        range: range.start + span.start..range.start + span.end,
                        kind: "keyword".into(),
                    }));
                } else {
                    lexical_spans(source, range.start, &mut spans);
                }
            }
        }
        spans.sort_by_key(|span| span.range.start);
        spans.dedup_by(|left, right| left.range == right.range && left.kind == right.kind);
        Ok(HighlightResult {
            key: document_key(document_id, document.revision, document.generation),
            freshness,
            spans,
            requested_ranges,
        })
    }

    fn complete(
        &self,
        document_id: DocumentId,
        requested_revision: DocumentRevision,
        byte: usize,
    ) -> Result<CompletionResult, ProviderError> {
        let document = self
            .documents
            .get(&document_id)
            .ok_or(ProviderError::UnknownDocument(document_id))?;
        let byte = floor_boundary(&document.text, byte.min(document.text.len()));
        let start = document.text[..byte]
            .char_indices()
            .rev()
            .take_while(|(_, character)| character.is_alphanumeric() || *character == '_')
            .last()
            .map_or(byte, |(offset, _)| offset);
        let prefix = &document.text[start..byte];
        let mut words: Vec<_> = document
            .text
            .split(|character: char| !character.is_alphanumeric() && character != '_')
            .filter(|word| !word.is_empty() && *word != prefix)
            .collect();
        words.sort_unstable();
        words.dedup();
        let candidates = fuzzy_rank(prefix, words.into_iter())
            .into_iter()
            .take(64)
            .map(|word| CompletionCandidate {
                label: word.into(),
                insert: word.into(),
                source: "word".into(),
                detail: "Buffer".into(),
                documentation: "".into(),
                replace: None,
                snippet: None,
            })
            .collect();
        Ok(CompletionResult {
            key: document_key(document_id, document.revision, document.generation),
            freshness: revision_freshness(document.revision, requested_revision),
            replace: start..byte,
            candidates,
        })
    }
}

fn native_tree_sitter_spans(text: &str, language_id: &str) -> Option<Vec<HighlightSpan>> {
    use ast_grep_language::{LanguageExt, SupportLang};

    let language = language_id.parse::<SupportLang>().ok()?;
    let document = language.ast_grep(text);
    Some(
        document
            .root()
            .dfs()
            .filter_map(|node| {
                let kind = node.kind();
                let kind = kind.as_ref();
                let source = node.text();
                let token = source.as_ref();
                let parent = node.parent();
                let parent_kind = parent
                    .as_ref()
                    .map_or_else(String::new, |parent| parent.kind().into_owned());
                let grandparent_kind = parent
                    .as_ref()
                    .and_then(|parent| parent.parent())
                    .map_or_else(String::new, |parent| parent.kind().into_owned());
                let is_parent_field = |field: &str| {
                    parent
                        .as_ref()
                        .and_then(|parent| parent.field(field))
                        .is_some_and(|field_node| field_node.range() == node.range())
                };
                let in_attribute = node.ancestors().take(5).any(|ancestor| {
                    let ancestor = ancestor.kind();
                    ancestor.contains("attribute") || ancestor.contains("decorator")
                });
                let in_method_declaration = node.ancestors().take(6).any(|ancestor| {
                    matches!(
                        ancestor.kind().as_ref(),
                        "impl_item"
                            | "class_body"
                            | "class_definition"
                            | "method_definition"
                            | "method_declaration"
                    )
                });

                let highlight = if kind.contains("comment") {
                    "comment"
                } else if kind.contains("heading") {
                    "markup.heading"
                } else if kind.contains("code_span") || kind.contains("fenced_code") {
                    "markup.raw"
                } else if kind.contains("preproc") || matches!(kind, "shebang" | "directive") {
                    "preproc"
                } else if in_attribute {
                    "attribute"
                } else if kind.contains("escape") {
                    "escape"
                } else if kind.contains("string")
                    || matches!(kind, "template_literal" | "quoted_attribute_value")
                {
                    "string"
                } else if kind.contains("char") && kind != "character_count" {
                    "character"
                } else if matches!(token, "true" | "false") {
                    "boolean"
                } else if kind.contains("number")
                    || kind.contains("integer")
                    || kind.contains("float")
                {
                    "number"
                } else if matches!(token, "null" | "nil" | "none" | "None" | "undefined") {
                    "constant.builtin"
                } else if matches!(token, "self" | "this" | "super" | "Self") {
                    "variable.builtin"
                } else if matches!(
                    token,
                    "if" | "else" | "elif" | "case" | "match" | "switch" | "then" | "when"
                ) {
                    "conditional"
                } else if matches!(token, "for" | "while" | "loop" | "repeat") {
                    "repeat"
                } else if matches!(
                    token,
                    "import"
                        | "from"
                        | "include"
                        | "require"
                        | "use"
                        | "module"
                        | "package"
                        | "export"
                ) {
                    "include"
                } else if matches!(
                    token,
                    "try" | "catch" | "except" | "finally" | "throw" | "raise"
                ) {
                    "exception"
                } else if matches!(token, "let" | "var" | "const" | "static" | "mut" | "val") {
                    "storage"
                } else if matches!(
                    token,
                    "struct" | "class" | "enum" | "trait" | "interface" | "type" | "typedef"
                ) {
                    "type.definition"
                } else if matches!(token, "impl" | "extends" | "implements" | "where") {
                    "type.qualifier"
                } else if matches!(
                    token,
                    "as" | "in"
                        | "is"
                        | "and"
                        | "or"
                        | "not"
                        | "instanceof"
                        | "+"
                        | "-"
                        | "*"
                        | "/"
                        | "%"
                        | "="
                        | "=="
                        | "!="
                        | "<"
                        | ">"
                        | "<="
                        | ">="
                        | "&&"
                        | "||"
                        | "!"
                        | "&"
                        | "|"
                        | "^"
                        | "~"
                        | "<<"
                        | ">>"
                        | "+="
                        | "-="
                        | "*="
                        | "/="
                        | "%="
                        | "=>"
                        | "->"
                        | "?"
                ) {
                    "operator"
                } else if matches!(token, "(" | ")" | "[" | "]" | "{" | "}") {
                    "punctuation.bracket"
                } else if matches!(token, "." | "," | ";" | ":" | "::") {
                    "punctuation.delimiter"
                } else if kind.contains("type_identifier")
                    || kind == "primitive_type"
                    || kind.contains("type_name")
                    || kind.contains("builtin_type")
                {
                    if kind == "primitive_type" || kind.contains("builtin_type") {
                        "type.builtin"
                    } else {
                        "type"
                    }
                } else if kind.contains("tag_name") {
                    "tag"
                } else if matches!(kind, "attribute_name" | "attribute_selector") {
                    "tag.attribute"
                } else if kind.contains("field_identifier")
                    || kind.contains("property_identifier")
                    || kind == "shorthand_property_identifier"
                {
                    if parent_kind.contains("field_expression")
                        && grandparent_kind.contains("call_expression")
                    {
                        "method"
                    } else {
                        "property"
                    }
                } else if kind.contains("namespace") || kind.contains("module_name") {
                    "namespace"
                } else if matches!(kind, "function_name" | "function_identifier") {
                    "function"
                } else if matches!(kind, "method_name" | "method_identifier") {
                    "method"
                } else if matches!(kind, "self" | "this" | "super") {
                    "variable.builtin"
                } else if kind.contains("identifier") || kind == "name" {
                    if parent_kind.contains("macro") {
                        "function.macro"
                    } else if parent_kind.contains("parameter")
                        || parent_kind == "formal"
                        || grandparent_kind.contains("parameter")
                    {
                        "parameter"
                    } else if parent_kind == "attrpath" && grandparent_kind == "binding" {
                        "property"
                    } else if parent_kind == "attrpath"
                        && grandparent_kind == "select_expression"
                        && node
                            .ancestors()
                            .take(5)
                            .any(|ancestor| ancestor.kind().as_ref() == "apply_expression")
                    {
                        "function"
                    } else if parent_kind == "attrpath" && grandparent_kind == "select_expression" {
                        "property"
                    } else if (parent_kind.contains("function") || parent_kind.contains("method"))
                        && (is_parent_field("name") || is_parent_field("declarator"))
                    {
                        if in_method_declaration {
                            "method"
                        } else {
                            "function"
                        }
                    } else if parent_kind.contains("call_expression")
                        && (is_parent_field("function") || is_parent_field("callee"))
                    {
                        "function"
                    } else if parent_kind.contains("scoped_identifier")
                        && grandparent_kind.contains("call_expression")
                        && is_parent_field("name")
                    {
                        if token == "new" || token.starts_with("new_") {
                            "constructor"
                        } else {
                            "function"
                        }
                    } else if parent_kind.contains("scoped_identifier")
                        || parent_kind.contains("qualified_name")
                        || parent_kind.contains("use_")
                    {
                        if token.chars().next().is_some_and(char::is_uppercase) {
                            "type"
                        } else {
                            "namespace"
                        }
                    } else if token.chars().all(|character| {
                        character.is_ascii_uppercase()
                            || character.is_ascii_digit()
                            || character == '_'
                    }) {
                        "constant"
                    } else if token.chars().next().is_some_and(char::is_uppercase) {
                        "type"
                    } else {
                        "variable"
                    }
                } else if matches!(
                    token,
                    "async"
                        | "await"
                        | "break"
                        | "continue"
                        | "data"
                        | "def"
                        | "do"
                        | "fn"
                        | "func"
                        | "inherit"
                        | "lambda"
                        | "new"
                        | "private"
                        | "pub"
                        | "public"
                        | "rec"
                        | "return"
                        | "assert"
                        | "yield"
                        | "with"
                ) {
                    "keyword"
                } else if kind.contains("constant") {
                    "constant"
                } else {
                    return None;
                };
                Some(HighlightSpan {
                    range: node.range(),
                    kind: highlight.into(),
                })
            })
            .collect(),
    )
}

/// Synchronously highlights bounded UI text such as a Telescope preview using
/// the same native Tree-sitter backend as editor buffers.
#[must_use]
pub fn highlight_text(text: &str, language_id: &str) -> Vec<HighlightSpan> {
    native_tree_sitter_spans(text, language_id).unwrap_or_else(|| {
        let mut spans = Vec::new();
        lexical_spans(text, 0, &mut spans);
        spans
    })
}

/// Parser-free first-frame fallback. It is safe to run in the client while
/// the failure-isolated provider process computes the full syntax result.
#[must_use]
pub fn lexical_highlight_text(text: &str) -> Vec<HighlightSpan> {
    let mut spans = Vec::new();
    lexical_spans(text, 0, &mut spans);
    spans
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecorationSet {
    pub key: FreshnessKey,
    pub freshness: Freshness,
    pub spans: Vec<HighlightSpan>,
}

impl DecorationSet {
    pub fn map_through(&mut self, transaction: &Transaction) {
        let mut mapped = Vec::with_capacity(self.spans.len());
        for span in &self.spans {
            let start = transaction.map_offset(span.range.start, Bias::Left);
            let end = transaction.map_offset(span.range.end, Bias::Right);
            if let (Ok(start), Ok(end)) = (start, end)
                && start <= end
            {
                mapped.push(HighlightSpan {
                    range: start..end,
                    kind: span.kind.clone(),
                });
            }
        }
        self.spans = mapped;
        if let FreshnessKey::Document {
            document_revision, ..
        } = &mut self.key
        {
            let from_revision = *document_revision;
            if let Some(next) = document_revision.next() {
                *document_revision = next;
                self.freshness = Freshness::LocallyMapped { from_revision };
            } else {
                self.spans.clear();
                self.freshness = Freshness::Unknown;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompletionSession {
    pub revision: DocumentRevision,
    pub replace: Range<usize>,
    pub candidates: Vec<CompletionCandidate>,
}

impl CompletionSession {
    #[must_use]
    pub fn merge(
        revision: DocumentRevision,
        replace: Range<usize>,
        local: Vec<CompletionCandidate>,
        remote: Vec<CompletionCandidate>,
    ) -> Self {
        let mut candidates = local;
        candidates.extend(remote);
        candidates.sort_by(|left, right| left.label.cmp(&right.label));
        candidates.dedup_by(|left, right| left.insert == right.insert);
        Self {
            revision,
            replace,
            candidates,
        }
    }

    pub fn accept(
        &self,
        current_revision: DocumentRevision,
        index: usize,
    ) -> Result<Option<Transaction>, ProviderError> {
        if current_revision != self.revision {
            return Err(ProviderError::StaleCompletion {
                expected: self.revision,
                actual: current_revision,
            });
        }
        self.candidates
            .get(index)
            .map(|candidate| {
                Transaction::new(
                    current_revision,
                    vec![Edit::new(
                        candidate
                            .replace
                            .clone()
                            .unwrap_or_else(|| self.replace.clone()),
                        candidate.insert.clone(),
                    )],
                )
                .map_err(|error| {
                    ProviderError::Json(serde_json::Error::io(io::Error::other(error)))
                })
            })
            .transpose()
    }
}

pub struct ProviderProcess {
    child: Child,
    input: BufWriter<ChildStdin>,
    output: BufReader<ChildStdout>,
}

impl ProviderProcess {
    pub fn spawn(program: impl AsRef<Path>) -> Result<Self, ProviderError> {
        Self::spawn_with_args(program, std::iter::empty::<&str>())
    }

    pub fn spawn_with_args(
        program: impl AsRef<Path>,
        args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    ) -> Result<Self, ProviderError> {
        let program = program.as_ref();
        if !program.exists() {
            return Err(ProviderError::MissingProgram(program.to_path_buf()));
        }
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let input = child.stdin.take().ok_or(ProviderError::Closed)?;
        let output = child.stdout.take().ok_or(ProviderError::Closed)?;
        Ok(Self {
            child,
            input: BufWriter::new(input),
            output: BufReader::new(output),
        })
    }

    pub fn request(
        &mut self,
        request: &ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        serde_json::to_writer(&mut self.input, request)?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        let mut line = String::new();
        if self.output.read_line(&mut line)? == 0 {
            return Err(ProviderError::Closed);
        }
        Ok(serde_json::from_str(&line)?)
    }
}

impl Drop for ProviderProcess {
    fn drop(&mut self) {
        let _ = self.request(&ProviderRequest::Shutdown);
        let _ = self.child.wait();
    }
}

pub struct ProviderSupervisor {
    program: PathBuf,
    args: Vec<std::ffi::OsString>,
    process: ProviderProcess,
    open_documents: BTreeMap<DocumentId, ProviderRequest>,
    restart_count: u64,
}

impl ProviderSupervisor {
    pub fn spawn(program: impl AsRef<Path>) -> Result<Self, ProviderError> {
        Self::spawn_with_args(program, std::iter::empty::<&str>())
    }

    pub fn spawn_with_args(
        program: impl AsRef<Path>,
        args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    ) -> Result<Self, ProviderError> {
        let program = program.as_ref().to_path_buf();
        let args = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect::<Vec<_>>();
        let process = ProviderProcess::spawn_with_args(&program, &args)?;
        Ok(Self {
            program,
            args,
            process,
            open_documents: BTreeMap::new(),
            restart_count: 0,
        })
    }

    #[must_use]
    pub const fn restart_count(&self) -> u64 {
        self.restart_count
    }

    pub fn request(
        &mut self,
        request: &ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        match self.process.request(request) {
            Ok(response) => {
                self.remember(request);
                Ok(response)
            }
            Err(error) => {
                self.restart()?;
                if matches!(request, ProviderRequest::CrashForTest) {
                    return Err(error);
                }
                let response = self.process.request(request)?;
                self.remember(request);
                Ok(response)
            }
        }
    }

    fn remember(&mut self, request: &ProviderRequest) {
        if let ProviderRequest::UpdateDocument { document_id, .. } = request {
            self.open_documents.insert(*document_id, request.clone());
        }
    }

    fn restart(&mut self) -> Result<(), ProviderError> {
        self.process = ProviderProcess::spawn_with_args(&self.program, &self.args)?;
        self.restart_count = self.restart_count.saturating_add(1);
        for update in self.open_documents.values() {
            match self.process.request(update)? {
                ProviderResponse::Updated { .. } => {}
                response => {
                    return Err(ProviderError::Json(serde_json::Error::io(
                        io::Error::other(format!("unexpected replay response {response:?}")),
                    )));
                }
            }
        }
        Ok(())
    }
}

pub fn serve(reader: impl BufRead, mut writer: impl Write) -> Result<(), ProviderError> {
    let mut actor = ProviderActor::default();
    for line in reader.lines() {
        let request: ProviderRequest = serde_json::from_str(&line?)?;
        let shutdown = matches!(request, ProviderRequest::Shutdown);
        let response = actor
            .handle(request)
            .unwrap_or_else(|error| ProviderResponse::Error {
                message: error.to_string().into(),
            });
        serde_json::to_writer(&mut writer, &response)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        if shutdown {
            break;
        }
    }
    Ok(())
}

fn document_key(
    document_id: DocumentId,
    document_revision: DocumentRevision,
    provider_generation: ProviderGeneration,
) -> FreshnessKey {
    FreshnessKey::Document {
        document_id,
        document_revision,
        provider_generation,
    }
}

fn revision_freshness(actual: DocumentRevision, requested: DocumentRevision) -> Freshness {
    if actual == requested {
        Freshness::Fresh
    } else if actual < requested {
        Freshness::Stale {
            revisions_behind: requested.get().saturating_sub(actual.get()),
        }
    } else {
        Freshness::LocallyMapped {
            from_revision: requested,
        }
    }
}

fn coalesce_ranges(mut ranges: Vec<Range<usize>>, text_len: usize) -> Vec<Range<usize>> {
    for range in &mut ranges {
        range.start = range.start.min(text_len);
        range.end = range.end.min(text_len).max(range.start);
    }
    ranges.sort_unstable_by_key(|range| range.start);
    let mut written = 0;
    for current in 0..ranges.len() {
        if written > 0 && ranges[current].start <= ranges[written - 1].end {
            ranges[written - 1].end = ranges[written - 1].end.max(ranges[current].end);
        } else {
            if written != current {
                ranges.swap(written, current);
            }
            written += 1;
        }
    }
    ranges.truncate(written);
    ranges
}

fn lexical_spans(text: &str, base: usize, output: &mut Vec<HighlightSpan>) {
    let keywords = [
        "fn", "let", "mut", "struct", "enum", "impl", "trait", "pub", "use", "match", "if", "else",
        "for", "while", "return",
    ];
    let mut offset = 0;
    for token in
        text.split_inclusive(|character: char| !character.is_alphanumeric() && character != '_')
    {
        let word = token
            .trim_end_matches(|character: char| !character.is_alphanumeric() && character != '_');
        if keywords.contains(&word) {
            output.push(HighlightSpan {
                range: base + offset..base + offset + word.len(),
                kind: "keyword".into(),
            });
        }
        offset += token.len();
    }
}

pub fn fuzzy_rank<'a>(needle: &str, candidates: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(
        needle,
        nucleo_matcher::pattern::CaseMatching::Smart,
        nucleo_matcher::pattern::Normalization::Smart,
    );
    let mut scored: Vec<_> = candidates
        .filter_map(|candidate| {
            let mut buffer = Vec::new();
            let haystack = Utf32Str::new(candidate, &mut buffer);
            pattern
                .score(haystack, &mut matcher)
                .map(|score| (score, candidate))
        })
        .collect();
    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(right.1)));
    scored.into_iter().map(|(_, candidate)| candidate).collect()
}

fn floor_boundary(text: &str, mut byte: usize) -> usize {
    while byte > 0 && !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

#[cfg(test)]
mod tests {
    use wren_types::{Priority, ProviderDemand};

    use super::*;

    fn bundle() -> LanguageBundle {
        LanguageBundle {
            language_id: "rust".into(),
            grammar_hash: [1; 32],
            grammar_abi: 15,
            grammar_semver: "0.24".into(),
            highlight_query_hash: [2; 32],
            object_query_hash: [3; 32],
            outline_query_hash: [4; 32],
            injection_query_hash: [5; 32],
            config_schema_version: 1,
        }
    }

    #[test]
    fn actor_keys_results_and_limits_work_to_viewport_demand() {
        let mut actor = ProviderActor::default();
        actor
            .handle(ProviderRequest::UpdateDocument {
                document_id: DocumentId::new(1),
                revision: DocumentRevision::new(4),
                text: "fn one() {}\nlet two = 2;\n".into(),
                bundle: bundle(),
            })
            .expect("update");
        let ProviderResponse::Highlight(result) = actor
            .handle(ProviderRequest::Demand {
                document_id: DocumentId::new(1),
                demand: ProviderDemand {
                    revision: DocumentRevision::new(4),
                    visible: std::iter::once(0..11).collect(),
                    near_viewport: Vec::new(),
                    priority: Priority::Visible,
                },
            })
            .expect("highlight")
        else {
            panic!("highlight response");
        };
        assert_eq!(result.freshness, Freshness::Fresh);
        assert_eq!(result.requested_ranges, vec![0..11]);
        assert_eq!(result.spans[0].range, 0..2);
        assert!(result.spans.iter().all(|span| span.range.end <= 11));
    }

    #[test]
    fn completion_is_revision_validated_and_accepts_atomically() {
        let local = vec![CompletionCandidate {
            label: "alphabet".into(),
            insert: "alphabet".into(),
            source: "word".into(),
            detail: "Buffer".into(),
            documentation: "".into(),
            replace: None,
            snippet: None,
        }];
        let session = CompletionSession::merge(DocumentRevision::new(2), 0..3, local, Vec::new());
        assert!(matches!(
            session.accept(DocumentRevision::new(3), 0),
            Err(ProviderError::StaleCompletion { .. })
        ));
        let transaction = session
            .accept(DocumentRevision::new(2), 0)
            .expect("fresh")
            .expect("candidate");
        assert_eq!(transaction.edits[0].range, 0..3);
    }

    #[test]
    fn native_tree_sitter_highlights_complete_utf8_byte_ranges() {
        let source = "use std::path::Path;\nfn main() { let answer: i32 = 42; println!(\"wren\"); } // note\n";
        let spans = highlight_text(source, "rust");
        assert!(
            spans
                .iter()
                .any(|span| span.range == (0..3) && span.kind.as_ref() == "include"),
            "{spans:?}"
        );
        for (needle, kind) in [
            ("42", "number"),
            ("\"wren\"", "string"),
            ("// note", "comment"),
        ] {
            let start = source.find(needle).expect("needle");
            assert!(
                spans.iter().any(|span| {
                    span.range == (start..start + needle.len()) && span.kind.as_ref() == kind
                }),
                "missing {kind} for {needle:?}: {spans:?}"
            );
        }
    }

    #[test]
    fn native_tree_sitter_emits_semantic_captures_for_rust_identifiers() {
        let source = "struct Widget { value: usize }\nimpl Widget { fn compute(&self, extra: usize) -> usize { let total = self.value + extra; total } }\nfn main() { let item = Widget { value: 2 }; item.compute(3); }\n";
        let spans = highlight_text(source, "rust");
        for (needle, occurrence, kind) in [
            ("Widget", 0, "type"),
            ("compute", 0, "method"),
            ("extra", 0, "parameter"),
            ("total", 0, "variable"),
            ("value", 0, "property"),
            ("+", 0, "operator"),
            ("compute", 1, "method"),
        ] {
            let start = source
                .match_indices(needle)
                .nth(occurrence)
                .map(|(start, _)| start)
                .expect("semantic token");
            assert!(
                spans.iter().any(|span| {
                    span.range == (start..start + needle.len()) && span.kind.as_ref() == kind
                }),
                "missing {kind} for {needle:?} occurrence {occurrence}: {spans:?}"
            );
        }
    }

    #[test]
    fn every_bundled_tree_sitter_language_parses_and_highlights_real_source() {
        let samples = [
            ("bash", "if true; then echo \"hi\"; fi # note\n"),
            ("c", "int main(void) { return 0; }\n"),
            ("cpp", "class Widget { public: int value; };\n"),
            ("csharp", "class Widget { static int Main() => 0; }\n"),
            ("css", ".item { color: red; }\n"),
            ("dart", "void main() { print(\"hi\"); }\n"),
            ("elixir", "defmodule Demo do\n  def run, do: :ok\nend\n"),
            ("go", "package main\nfunc main() {}\n"),
            (
                "haskell",
                "module Main where\nimport Data.Text\nanswer :: Int\nanswer = 42\n",
            ),
            ("hcl", "resource \"x\" \"y\" { enabled = true }\n"),
            ("html", "<main class=\"x\">hi</main>\n"),
            (
                "java",
                "class Main { static int answer() { return 42; } }\n",
            ),
            ("javascript", "export const answer = () => 42;\n"),
            ("json", "{\"answer\": 42}\n"),
            ("kotlin", "fun answer(): Int = 42\n"),
            ("lua", "local function answer() return 42 end\n"),
            ("markdown", "# Heading\n\n`code`\n"),
            (
                "nix",
                "{ lib, ... }: let greeting = \"hello\"; in { programs.wren.enable = lib.mkDefault true; } # note\n",
            ),
            ("php", "<?php function answer(): int { return 42; }\n"),
            ("python", "def answer() -> int:\n    return 42\n"),
            ("ruby", "def answer\n  42\nend\n"),
            ("rust", "fn answer() -> i32 { 42 }\n"),
            ("scala", "def answer: Int = 42\n"),
            ("solidity", "pragma solidity ^0.8.0; contract Demo {}\n"),
            ("swift", "func answer() -> Int { 42 }\n"),
            (
                "tsx",
                "const App = (): JSX.Element => <main>Hello</main>;\n",
            ),
            ("typescript", "interface User { name: string }\n"),
            ("yaml", "answer: 42\nenabled: true\n"),
        ];
        for (language, source) in samples {
            let spans = native_tree_sitter_spans(source, language)
                .unwrap_or_else(|| panic!("bundled {language} grammar was not resolved"));
            assert!(
                !spans.is_empty(),
                "bundled {language} grammar emitted no highlights"
            );
            assert!(
                spans
                    .iter()
                    .all(|span| span.range.start < span.range.end && span.range.end <= source.len()),
                "bundled {language} emitted an invalid range: {spans:?}"
            );
        }
    }

    #[test]
    fn nix_haskell_typescript_and_c_have_tree_sitter_semantic_baselines() {
        let cases = [
            (
                "nix",
                "{ lib, ... }: let greeting = \"hello\"; in { enabled = lib.mkDefault true; } # note\n",
                [
                    ("lib", "parameter"),
                    ("enabled", "property"),
                    ("mkDefault", "function"),
                ],
            ),
            (
                "haskell",
                "module Main where\nanswer :: Int\nanswer = 42\n",
                [("module", "include"), ("Int", "type"), ("42", "number")],
            ),
            (
                "typescript",
                "interface User { name: string }\nconst answer = 42;\n",
                [
                    ("interface", "type.definition"),
                    ("User", "type"),
                    ("42", "number"),
                ],
            ),
            (
                "c",
                "struct User { int value; }; int answer(void) { return 42; }\n",
                [
                    ("struct", "type.definition"),
                    ("answer", "function"),
                    ("42", "number"),
                ],
            ),
        ];
        for (language, source, expected) in cases {
            let spans = native_tree_sitter_spans(source, language).expect("bundled grammar");
            for (needle, kind) in expected {
                let start = source.find(needle).expect("sample token");
                assert!(
                    spans.iter().any(|span| {
                        span.range == (start..start + needle.len()) && span.kind.as_ref() == kind
                    }),
                    "{language} did not classify {needle:?} as {kind}: {spans:?}"
                );
            }
        }
    }

    #[test]
    fn cpu_fallback_highlights_unsupported_and_unicode_documents() {
        let source = "fn café() { let return_value = return; }\n";
        let mut fallback_bundle = bundle();
        fallback_bundle.language_id = "unsupported-language".into();
        let mut actor = ProviderActor::cpu_only();
        actor
            .handle(ProviderRequest::UpdateDocument {
                document_id: DocumentId::new(9),
                revision: DocumentRevision::new(1),
                text: source.into(),
                bundle: fallback_bundle,
            })
            .expect("update unsupported document");
        let ProviderResponse::Highlight(result) = actor
            .handle(ProviderRequest::Demand {
                document_id: DocumentId::new(9),
                demand: ProviderDemand {
                    revision: DocumentRevision::new(1),
                    visible: std::iter::once(0..source.len()).collect(),
                    near_viewport: Vec::new(),
                    priority: Priority::Visible,
                },
            })
            .expect("highlight unsupported document")
        else {
            panic!("highlight response");
        };
        assert_eq!(actor.acceleration_backend(), AccelerationBackend::Cpu);
        assert_eq!(result.spans, lexical_highlight_text(source));
    }

    #[test]
    fn small_workloads_preserve_cpu_lexical_results() {
        let source = "pub fn generated() { let value = match value { _ => return }; }\n";
        let mut fallback_bundle = bundle();
        fallback_bundle.language_id = "unsupported-language".into();
        let mut actor = ProviderActor::default();
        actor
            .handle(ProviderRequest::UpdateDocument {
                document_id: DocumentId::new(10),
                revision: DocumentRevision::new(1),
                text: source.into(),
                bundle: fallback_bundle,
            })
            .expect("update accelerated document");
        let ProviderResponse::Highlight(result) = actor
            .handle(ProviderRequest::Demand {
                document_id: DocumentId::new(10),
                demand: ProviderDemand {
                    revision: DocumentRevision::new(1),
                    visible: std::iter::once(0..source.len()).collect(),
                    near_viewport: Vec::new(),
                    priority: Priority::Visible,
                },
            })
            .expect("highlight accelerated document")
        else {
            panic!("highlight response");
        };
        assert!(matches!(
            actor.acceleration_backend(),
            AccelerationBackend::Pending | AccelerationBackend::Cpu
        ));
        assert_eq!(result.spans, lexical_highlight_text(source));
    }

    #[cfg(feature = "gpu")]
    #[test]
    fn gpu_lexical_classifier_matches_cpu_when_an_adapter_is_available() {
        let Ok(mut gpu) = gpu::GpuLexical::new() else {
            return;
        };
        for source in [
            "fn",
            "x fn let struct return while trait enum impl match pub use mut if else for",
            "diffn fn_ fn\nlet\tmut; struct-item return_value return!",
            "pub fn generated() { let value = match value { _ => return }; }\n",
        ] {
            let expected = lexical_highlight_text(source)
                .into_iter()
                .map(|span| span.range)
                .collect::<Vec<_>>();
            let actual = gpu.classify(source).expect("GPU lexical classification");
            assert_eq!(actual, expected, "GPU mismatch for {source:?}");
        }
    }

    #[test]
    fn decorations_map_forward_or_hide_instead_of_misplacing() {
        let mut decorations = DecorationSet {
            key: document_key(
                DocumentId::new(1),
                DocumentRevision::new(1),
                ProviderGeneration::new(1),
            ),
            freshness: Freshness::Fresh,
            spans: vec![HighlightSpan {
                range: 2..4,
                kind: "keyword".into(),
            }],
        };
        decorations.map_through(
            &Transaction::new(DocumentRevision::new(1), vec![Edit::new(0..0, "xx")])
                .expect("transaction"),
        );
        assert_eq!(decorations.spans[0].range, 4..6);
        assert!(matches!(
            decorations.freshness,
            Freshness::LocallyMapped { .. }
        ));
    }

    #[test]
    fn line_protocol_round_trips_without_exposing_cells_or_terminal_state() {
        let request = ProviderRequest::Hello { protocol: 1 };
        let mut input = serde_json::to_vec(&request).expect("encode");
        input.push(b'\n');
        let mut output = Vec::new();
        serve(std::io::Cursor::new(input), &mut output).expect("serve");
        let response: ProviderResponse = serde_json::from_slice(&output).expect("response");
        assert_eq!(response, ProviderResponse::Hello { protocol: 1 });
    }

    #[test]
    fn demand_queue_is_bounded_latest_wins_and_priority_ordered() {
        let mut queue = LatestDemandQueue::new(2);
        for revision in 1..=10_000 {
            queue.push(
                DocumentId::new(1),
                ProviderDemand {
                    revision: DocumentRevision::new(revision),
                    visible: std::iter::once(0..10).collect(),
                    near_viewport: Vec::new(),
                    priority: Priority::Visible,
                },
            );
        }
        queue.push(
            DocumentId::new(2),
            ProviderDemand {
                revision: DocumentRevision::new(1),
                visible: std::iter::once(0..1).collect(),
                near_viewport: Vec::new(),
                priority: Priority::Interactive,
            },
        );
        assert_eq!(queue.depth(), 2);
        assert_eq!(queue.dropped(), 9_999);
        assert_eq!(
            queue.pop().expect("interactive").document_id,
            DocumentId::new(2)
        );
        assert_eq!(
            queue.pop().expect("latest").demand.revision,
            DocumentRevision::new(10_000)
        );
    }
}
