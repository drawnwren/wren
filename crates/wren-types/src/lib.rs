#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;
use std::ops::Range;

use serde::{Deserialize, Serialize};
use thiserror::Error;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        pub struct $name(u64);

        impl $name {
            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

id_type!(DocumentId);
id_type!(BufferId);
id_type!(WindowId);
id_type!(TabId);
id_type!(FloatingSurfaceId);
id_type!(DecorationNamespaceId);
id_type!(ExtmarkId);
id_type!(DocumentRevision);
id_type!(ClientSequence);
id_type!(LeaseEpoch);
id_type!(ClientId);
id_type!(MutationId);
id_type!(SessionId);
id_type!(SessionEpoch);
id_type!(SessionSequence);
id_type!(SemanticGroupId);
id_type!(WorkspaceGeneration);
id_type!(ViewId);
id_type!(ConfigGeneration);
id_type!(CommandTaskId);
id_type!(PersistBatchId);
id_type!(ProviderGeneration);

impl DocumentRevision {
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

/// A byte-offset edit against the transaction's explicit base revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    pub range: Range<usize>,
    pub insert: Box<str>,
}

impl Edit {
    #[must_use]
    pub fn new(range: Range<usize>, insert: impl Into<Box<str>>) -> Self {
        Self {
            range,
            insert: insert.into(),
        }
    }
}

/// A sorted set of non-overlapping edits against one document revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    pub base_revision: DocumentRevision,
    pub edits: Vec<Edit>,
    /// Preserves anchor provenance when a composition cannot be represented by
    /// the final byte edits alone (for example, an edit at a collapsed edge).
    #[doc(hidden)]
    #[serde(skip)]
    composition: Option<Box<Composition>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Composition {
    first: Transaction,
    second: Transaction,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TransactionError {
    #[error("edit range {start}..{end} is reversed")]
    ReversedRange { start: usize, end: usize },
    #[error("edit at {start} overlaps the previous edit ending at {previous_end}")]
    OverlappingEdits { start: usize, previous_end: usize },
    #[error("byte offset {offset} exceeds text length {len}")]
    OutOfBounds { offset: usize, len: usize },
    #[error("byte offset {offset} is not a UTF-8 character boundary")]
    NotCharBoundary { offset: usize },
    #[error("the following transaction must target revision {expected}, not {actual}")]
    RevisionMismatch { expected: u64, actual: u64 },
    #[error("document revision overflow")]
    RevisionOverflow,
    #[error("offset arithmetic overflow")]
    OffsetOverflow,
    #[error("inverse requires {expected} deleted strings, but received {actual}")]
    DeletedTextCount { expected: usize, actual: usize },
    #[error("deleted text {index} has {actual} bytes; expected {expected}")]
    DeletedTextLength {
        index: usize,
        expected: usize,
        actual: usize,
    },
    #[error("transactions cannot be represented as a monotonic edit composition")]
    InvalidComposition,
}

impl Transaction {
    pub fn new(
        base_revision: DocumentRevision,
        edits: Vec<Edit>,
    ) -> Result<Self, TransactionError> {
        let transaction = Self {
            base_revision,
            edits,
            composition: None,
        };
        transaction.validate()?;
        Ok(transaction)
    }

    #[must_use]
    pub const fn empty(base_revision: DocumentRevision) -> Self {
        Self {
            base_revision,
            edits: Vec::new(),
            composition: None,
        }
    }

    pub fn validate(&self) -> Result<(), TransactionError> {
        let mut previous_end = 0;
        let mut has_previous = false;
        for edit in &self.edits {
            if edit.range.start > edit.range.end {
                return Err(TransactionError::ReversedRange {
                    start: edit.range.start,
                    end: edit.range.end,
                });
            }
            if has_previous && edit.range.start < previous_end {
                return Err(TransactionError::OverlappingEdits {
                    start: edit.range.start,
                    previous_end,
                });
            }
            previous_end = edit.range.end;
            has_previous = true;
        }
        Ok(())
    }

    pub fn validate_for_text(&self, text: &str) -> Result<(), TransactionError> {
        self.validate()?;
        for edit in &self.edits {
            for offset in [edit.range.start, edit.range.end] {
                if offset > text.len() {
                    return Err(TransactionError::OutOfBounds {
                        offset,
                        len: text.len(),
                    });
                }
                if !text.is_char_boundary(offset) {
                    return Err(TransactionError::NotCharBoundary { offset });
                }
            }
        }
        Ok(())
    }

    /// Maps offsets through this transaction using the same bias for each.
    pub fn apply_to_offsets(
        &self,
        offsets: &mut [usize],
        bias: Bias,
    ) -> Result<(), TransactionError> {
        self.validate()?;
        for offset in offsets {
            *offset = self.map_offset(*offset, bias)?;
        }
        Ok(())
    }

    pub fn map_offset(&self, byte: usize, bias: Bias) -> Result<usize, TransactionError> {
        self.validate()?;
        if let Some(composition) = &self.composition {
            let intermediate = composition.first.map_offset(byte, bias)?;
            return composition.second.map_offset(intermediate, bias);
        }
        let mut delta = 0_i128;
        let mut index = 0;

        while index < self.edits.len() {
            let edit = &self.edits[index];
            let start =
                i128::try_from(edit.range.start).map_err(|_| TransactionError::OffsetOverflow)?;
            let end =
                i128::try_from(edit.range.end).map_err(|_| TransactionError::OffsetOverflow)?;
            let direct_insert_len =
                i128::try_from(edit.insert.len()).map_err(|_| TransactionError::OffsetOverflow)?;
            let mut insert_len = direct_insert_len;
            let byte_value = i128::try_from(byte).map_err(|_| TransactionError::OffsetOverflow)?;

            // A deletion followed by insertion(s) at its right edge is a
            // provenance-preserving replacement produced by composition.
            let mut trailing_insert = false;
            let mut consumed = 1;
            if start < end {
                while index + consumed < self.edits.len() {
                    let candidate = &self.edits[index + consumed];
                    if candidate.range.start != edit.range.end
                        || candidate.range.end != edit.range.end
                    {
                        break;
                    }
                    insert_len += i128::try_from(candidate.insert.len())
                        .map_err(|_| TransactionError::OffsetOverflow)?;
                    trailing_insert = true;
                    consumed += 1;
                }
            }

            if byte_value < start {
                return usize_from_i128(byte_value + delta);
            }

            if start == end && byte_value == start {
                return match bias {
                    Bias::Left => usize_from_i128(start + delta),
                    Bias::Right => usize_from_i128(start + delta + insert_len),
                };
            }

            if byte_value == start || (byte_value > start && byte_value < end) {
                return match bias {
                    Bias::Left => usize_from_i128(start + delta),
                    Bias::Right => usize_from_i128(start + delta + insert_len),
                };
            }

            if byte_value == end && trailing_insert {
                return match bias {
                    Bias::Left => usize_from_i128(start + delta + direct_insert_len),
                    Bias::Right => usize_from_i128(start + delta + insert_len),
                };
            }

            delta += insert_len - (end - start);
            index += consumed;
        }

        let byte_value = i128::try_from(byte).map_err(|_| TransactionError::OffsetOverflow)?;
        usize_from_i128(byte_value + delta)
    }

    /// Applies the transaction to UTF-8 text, rejecting invalid byte boundaries.
    pub fn apply_to_string(&self, text: &str) -> Result<String, TransactionError> {
        self.validate_for_text(text)?;
        let mut output = text.to_owned();
        for edit in self.edits.iter().rev() {
            output.replace_range(edit.range.clone(), &edit.insert);
        }
        Ok(output)
    }

    /// Composes `self` followed by `next` into edits against `self.base_revision`.
    ///
    /// The implementation uses symbolic base and inserted pieces, so it does not
    /// need the original document contents.
    pub fn then(&self, next: &Self) -> Result<Self, TransactionError> {
        self.validate()?;
        next.validate()?;
        let expected = self
            .base_revision
            .next()
            .ok_or(TransactionError::RevisionOverflow)?;
        if next.base_revision != expected {
            return Err(TransactionError::RevisionMismatch {
                expected: expected.get(),
                actual: next.base_revision.get(),
            });
        }

        let self_extent = self
            .edits
            .iter()
            .map(|edit| edit.range.end)
            .max()
            .unwrap_or(0);
        let next_extent = next
            .edits
            .iter()
            .map(|edit| edit.range.end)
            .max()
            .unwrap_or(0);
        let deleted = self
            .edits
            .iter()
            .try_fold(0_usize, |sum, edit| sum.checked_add(edit.range.len()))
            .ok_or(TransactionError::OffsetOverflow)?;
        let inserted = self
            .edits
            .iter()
            .try_fold(0_usize, |sum, edit| sum.checked_add(edit.insert.len()))
            .ok_or(TransactionError::OffsetOverflow)?;
        let mut base_extent = self_extent;
        let post_extent = base_extent
            .checked_sub(deleted)
            .and_then(|value| value.checked_add(inserted))
            .ok_or(TransactionError::OffsetOverflow)?;
        if post_extent < next_extent {
            base_extent = base_extent
                .checked_add(next_extent - post_extent)
                .ok_or(TransactionError::OffsetOverflow)?;
        }

        let first_pieces = self.symbolic_pieces(base_extent)?;
        let final_pieces = apply_symbolic_edits(&first_pieces, &next.edits, base_extent)?;
        let edits = pieces_to_edits(&final_pieces, base_extent)?;
        let mut composed = Self::new(self.base_revision, edits)?;
        composed.composition = Some(Box::new(Composition {
            first: self.clone(),
            second: next.clone(),
        }));
        Ok(composed)
    }

    /// Builds an inverse transaction from the exact strings removed by each edit.
    pub fn invert(&self, deleted_text: &[Box<str>]) -> Result<Self, TransactionError> {
        self.validate()?;
        if deleted_text.len() != self.edits.len() {
            return Err(TransactionError::DeletedTextCount {
                expected: self.edits.len(),
                actual: deleted_text.len(),
            });
        }

        let base_revision = self
            .base_revision
            .next()
            .ok_or(TransactionError::RevisionOverflow)?;
        let mut delta = 0_i128;
        let mut inverse = Vec::with_capacity(self.edits.len());
        for (index, (edit, deleted)) in self.edits.iter().zip(deleted_text).enumerate() {
            let expected = edit.range.len();
            if deleted.len() != expected {
                return Err(TransactionError::DeletedTextLength {
                    index,
                    expected,
                    actual: deleted.len(),
                });
            }
            let start = i128::try_from(edit.range.start)
                .map_err(|_| TransactionError::OffsetOverflow)?
                + delta;
            let insert_len =
                i128::try_from(edit.insert.len()).map_err(|_| TransactionError::OffsetOverflow)?;
            let inverse_start = usize_from_i128(start)?;
            let inverse_end = usize_from_i128(start + insert_len)?;
            inverse.push(Edit::new(inverse_start..inverse_end, deleted.clone()));
            delta += insert_len
                - i128::try_from(edit.range.len()).map_err(|_| TransactionError::OffsetOverflow)?;
        }
        Self::new(base_revision, inverse)
    }

    /// Extracts deleted strings from `base_text` and returns the inverse.
    pub fn inverted_against(&self, base_text: &str) -> Result<Self, TransactionError> {
        self.validate_for_text(base_text)?;
        let mut deleted = Vec::with_capacity(self.edits.len());
        for edit in &self.edits {
            let text = base_text
                .get(edit.range.clone())
                .ok_or(TransactionError::OutOfBounds {
                    offset: edit.range.end,
                    len: base_text.len(),
                })?;
            deleted.push(Box::<str>::from(text));
        }
        self.invert(&deleted)
    }

    fn symbolic_pieces(&self, base_extent: usize) -> Result<Vec<Piece>, TransactionError> {
        let mut pieces = Vec::new();
        let mut cursor = 0;
        for edit in &self.edits {
            if edit.range.end > base_extent {
                return Err(TransactionError::OutOfBounds {
                    offset: edit.range.end,
                    len: base_extent,
                });
            }
            if cursor < edit.range.start {
                pieces.push(Piece::Base(cursor..edit.range.start));
            }
            if !edit.insert.is_empty() {
                pieces.push(Piece::Inserted {
                    text: edit.insert.clone(),
                    anchor: edit.range.start,
                });
            }
            cursor = edit.range.end;
        }
        if cursor < base_extent {
            pieces.push(Piece::Base(cursor..base_extent));
        }
        Ok(coalesce_pieces(pieces))
    }
}

fn usize_from_i128(value: i128) -> Result<usize, TransactionError> {
    usize::try_from(value).map_err(|_| TransactionError::OffsetOverflow)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Bias {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub byte: usize,
    pub bias: Bias,
}

impl Anchor {
    pub fn map_through(self, transaction: &Transaction) -> Result<Self, TransactionError> {
        Ok(Self {
            byte: transaction.map_offset(self.byte, self.bias)?,
            bias: self.bias,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionSet {
    pub primary: usize,
    pub ranges: Vec<SelRange>,
}

impl SelectionSet {
    pub fn validate(&self) -> Result<(), SelectionError> {
        if self.ranges.is_empty() {
            return Err(SelectionError::Empty);
        }
        if self.primary >= self.ranges.len() {
            return Err(SelectionError::PrimaryOutOfBounds {
                primary: self.primary,
                len: self.ranges.len(),
            });
        }
        Ok(())
    }

    pub fn map_through(&self, transaction: &Transaction) -> Result<Self, TransactionError> {
        let mut ranges = Vec::with_capacity(self.ranges.len());
        for range in &self.ranges {
            let (anchor_bias, head_bias) = if range.anchor < range.head {
                (Bias::Left, Bias::Right)
            } else if range.anchor > range.head {
                (Bias::Right, Bias::Left)
            } else {
                (Bias::Right, Bias::Right)
            };
            ranges.push(SelRange {
                anchor: transaction.map_offset(range.anchor, anchor_bias)?,
                head: transaction.map_offset(range.head, head_bias)?,
            });
        }
        Ok(Self {
            primary: self.primary,
            ranges,
        })
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SelectionError {
    #[error("a selection set must contain at least one range")]
    Empty,
    #[error("primary selection {primary} is outside {len} ranges")]
    PrimaryOutOfBounds { primary: usize, len: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelRange {
    pub anchor: usize,
    pub head: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DocumentClass {
    Normal,
    Large,
    Pathological,
}

/// Inputs to the deterministic document-class policy. Parse throughput is a
/// short bounded sample, never a whole-file probe on the open path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentProfile {
    pub byte_length: u64,
    pub longest_line_estimate: u64,
    pub parse_bytes_per_millisecond: Option<u64>,
    pub generated_file: bool,
}

/// Runtime limits selected from a [`DocumentClass`]. Values are explicit so
/// providers and benchmark reports can publish the policy they applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentPolicy {
    pub syntax_cpu_budget_micros: u64,
    pub whole_document_syntax: bool,
    pub lsp_freshness_slo_millis: Option<u64>,
    pub native_completion: bool,
    pub incremental_git_diff_budget_bytes: u64,
    pub display_work_budget_cells: u64,
    pub approximate_display_columns: bool,
}

impl DocumentClass {
    pub const LARGE_BYTES: u64 = 10 * 1_048_576;
    pub const LARGE_LINE_BYTES: u64 = 128 * 1_024;
    pub const PATHOLOGICAL_BYTES: u64 = 1_073_741_824;
    pub const PATHOLOGICAL_LINE_BYTES: u64 = 1_048_576;
    pub const SLOW_PARSE_BYTES_PER_MILLISECOND: u64 = 64 * 1_024;
    pub const PATHOLOGICAL_PARSE_BYTES_PER_MILLISECOND: u64 = 8 * 1_024;

    #[must_use]
    pub const fn classify(profile: DocumentProfile) -> Self {
        let parse_rate = match profile.parse_bytes_per_millisecond {
            Some(value) => value,
            None => u64::MAX,
        };
        if profile.byte_length >= Self::PATHOLOGICAL_BYTES
            || profile.longest_line_estimate >= Self::PATHOLOGICAL_LINE_BYTES
            || parse_rate < Self::PATHOLOGICAL_PARSE_BYTES_PER_MILLISECOND
        {
            Self::Pathological
        } else if profile.byte_length >= Self::LARGE_BYTES
            || profile.longest_line_estimate >= Self::LARGE_LINE_BYTES
            || parse_rate < Self::SLOW_PARSE_BYTES_PER_MILLISECOND
            || profile.generated_file
        {
            Self::Large
        } else {
            Self::Normal
        }
    }

    #[must_use]
    pub const fn policy(self) -> DocumentPolicy {
        match self {
            Self::Normal => DocumentPolicy {
                syntax_cpu_budget_micros: 2_000,
                whole_document_syntax: true,
                lsp_freshness_slo_millis: Some(250),
                native_completion: true,
                incremental_git_diff_budget_bytes: 4 * 1_048_576,
                display_work_budget_cells: 1_000_000,
                approximate_display_columns: false,
            },
            Self::Large => DocumentPolicy {
                syntax_cpu_budget_micros: 1_000,
                whole_document_syntax: false,
                lsp_freshness_slo_millis: None,
                native_completion: true,
                incremental_git_diff_budget_bytes: 512 * 1_024,
                display_work_budget_cells: 250_000,
                approximate_display_columns: false,
            },
            Self::Pathological => DocumentPolicy {
                syntax_cpu_budget_micros: 250,
                whole_document_syntax: false,
                lsp_freshness_slo_millis: None,
                native_completion: true,
                incremental_git_diff_budget_bytes: 64 * 1_024,
                display_work_budget_cells: 32_768,
                approximate_display_columns: true,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Priority {
    Background,
    NearViewport,
    Visible,
    Interactive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDemand {
    pub revision: DocumentRevision,
    pub visible: Vec<Range<usize>>,
    pub near_viewport: Vec<Range<usize>>,
    pub priority: Priority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum WorkspaceGenerationKind {
    Git,
    Index,
    Config,
    Manifest,
    ProjectIndex,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FreshnessKey {
    Document {
        document_id: DocumentId,
        document_revision: DocumentRevision,
        provider_generation: ProviderGeneration,
    },
    Workspace {
        kind: WorkspaceGenerationKind,
        workspace_generation: WorkspaceGeneration,
        provider_generation: ProviderGeneration,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Freshness {
    Fresh,
    LocallyMapped { from_revision: DocumentRevision },
    Stale { revisions_behind: u64 },
    Disconnected { age_millis: u64 },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageBundle {
    pub language_id: Box<str>,
    pub grammar_hash: [u8; 32],
    pub grammar_abi: u32,
    pub grammar_semver: Box<str>,
    pub highlight_query_hash: [u8; 32],
    pub object_query_hash: [u8; 32],
    pub outline_query_hash: [u8; 32],
    pub injection_query_hash: [u8; 32],
    pub config_schema_version: u32,
}

impl LanguageBundle {
    #[must_use]
    pub fn content_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.language_id.as_bytes());
        hasher.update(&self.grammar_hash);
        hasher.update(&self.grammar_abi.to_le_bytes());
        hasher.update(self.grammar_semver.as_bytes());
        hasher.update(&self.highlight_query_hash);
        hasher.update(&self.object_query_hash);
        hasher.update(&self.outline_query_hash);
        hasher.update(&self.injection_query_hash);
        hasher.update(&self.config_schema_version.to_le_bytes());
        *hasher.finalize().as_bytes()
    }

    #[must_use]
    pub fn provider_generation(&self) -> ProviderGeneration {
        let hash = self.content_hash();
        ProviderGeneration::new(u64::from_le_bytes(hash[..8].try_into().unwrap_or([0; 8])))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteFrameAuthority {
    Correct,
    Speculative,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteOpenState {
    CachedHeadValidated {
        revision: DocumentRevision,
    },
    CachedAwaitingHead {
        cached_revision: DocumentRevision,
    },
    Progressive {
        authoritative_revision: DocumentRevision,
        received_bytes: u64,
        total_bytes: u64,
    },
    Materialized {
        authoritative_revision: DocumentRevision,
        content_hash: [u8; 32],
    },
}

impl RemoteOpenState {
    #[must_use]
    pub const fn frame_authority(&self) -> RemoteFrameAuthority {
        match self {
            Self::CachedHeadValidated { .. } | Self::Materialized { .. } => {
                RemoteFrameAuthority::Correct
            }
            Self::CachedAwaitingHead { .. } | Self::Progressive { .. } => {
                RemoteFrameAuthority::Speculative
            }
        }
    }

    /// Sparse editing is deliberately absent in v1. Editing and any
    /// whole-document operation unlock only after a complete authoritative
    /// revision exists locally.
    #[must_use]
    pub const fn editing_enabled(&self) -> bool {
        matches!(
            self,
            Self::CachedHeadValidated { .. } | Self::Materialized { .. }
        )
    }

    #[must_use]
    pub const fn whole_document_operations_enabled(&self) -> bool {
        self.editing_enabled()
    }
}

/// One semantic atomic mutation. Serialization is used for the durable journal;
/// the public transport remains the explicitly versioned `wren-proto` schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientMutation {
    pub mutation_id: MutationId,
    pub client_id: ClientId,
    pub client_sequence: ClientSequence,
    pub state_deltas: Vec<StateDelta>,
    pub documents: Vec<DocumentMutation>,
}

impl ClientMutation {
    pub fn validate(&self) -> Result<(), MutationValidationError> {
        if self.state_deltas.is_empty() && self.documents.is_empty() {
            return Err(MutationValidationError::EmptyMutation);
        }
        let mut document_ids = std::collections::BTreeSet::new();
        for document in &self.documents {
            if !document_ids.insert(document.document_id) {
                return Err(MutationValidationError::DuplicateDocument {
                    document_id: document.document_id,
                });
            }
            document.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StateDelta {
    Register {
        name: char,
        text: Box<str>,
        linewise: bool,
    },
    SearchPattern(Box<str>),
    CommandHistory(Box<str>),
    GlobalMark {
        name: char,
        document_id: DocumentId,
        anchor: Anchor,
    },
    UndoBranchHead {
        document_id: DocumentId,
        semantic_group_id: Option<SemanticGroupId>,
    },
    RepeatData(Vec<u8>),
    MacroRecording {
        name: char,
        raw_keys: Vec<u8>,
        lowered_ir: Vec<u8>,
    },
    JumpList {
        entries: Vec<DurableJumpEntry>,
        current: Option<usize>,
    },
}

/// A durable global jump uses the document identity as authority and carries a
/// presentation-path hint so a detached client can materialize that document
/// before it has rebuilt its workspace document table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableJumpEntry {
    pub document_id: DocumentId,
    pub anchor: Anchor,
    pub path_hint: Option<Box<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentMutation {
    pub document_id: DocumentId,
    pub lease_epoch: LeaseEpoch,
    pub base_revision: DocumentRevision,
    pub semantic_group_id: SemanticGroupId,
    pub semantic_group_kind: SemanticGroupKind,
    pub undo_parent: Option<SemanticGroupId>,
    pub transactions: Vec<Transaction>,
}

impl DocumentMutation {
    pub fn validate(&self) -> Result<(), MutationValidationError> {
        if self.transactions.is_empty() {
            return Err(MutationValidationError::EmptyDocumentMutation {
                document_id: self.document_id,
            });
        }
        let mut expected = self.base_revision;
        for transaction in &self.transactions {
            transaction.validate().map_err(|source| {
                MutationValidationError::InvalidTransaction {
                    document_id: self.document_id,
                    source,
                }
            })?;
            if transaction.base_revision != expected {
                return Err(MutationValidationError::RevisionChain {
                    document_id: self.document_id,
                    expected: expected.get(),
                    actual: transaction.base_revision.get(),
                });
            }
            expected = expected
                .next()
                .ok_or(MutationValidationError::RevisionOverflow)?;
        }
        Ok(())
    }

    pub fn accepted_revision(&self) -> Result<DocumentRevision, MutationValidationError> {
        self.validate()?;
        let count = u64::try_from(self.transactions.len())
            .map_err(|_| MutationValidationError::RevisionOverflow)?;
        self.base_revision
            .get()
            .checked_add(count)
            .map(DocumentRevision::new)
            .ok_or(MutationValidationError::RevisionOverflow)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SemanticGroupKind {
    InsertRun,
    Operator,
    MacroInvocation,
    Formatter,
    WorkspaceRefactor,
    UndoOf(SemanticGroupId),
    RedoOf(SemanticGroupId),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MutationValidationError {
    #[error("a client mutation must contain state or document changes")]
    EmptyMutation,
    #[error("document {document_id:?} occurs more than once in one atomic mutation")]
    DuplicateDocument { document_id: DocumentId },
    #[error("document {document_id:?} mutation has no transactions")]
    EmptyDocumentMutation { document_id: DocumentId },
    #[error(
        "document {document_id:?} transaction chain expected revision {expected}, got {actual}"
    )]
    RevisionChain {
        document_id: DocumentId,
        expected: u64,
        actual: u64,
    },
    #[error("document {document_id:?} contains an invalid transaction: {source}")]
    InvalidTransaction {
        document_id: DocumentId,
        #[source]
        source: TransactionError,
    },
    #[error("document mutation revision overflow")]
    RevisionOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MutationResult {
    Received {
        mutation_id: MutationId,
    },
    Durable {
        mutation_id: MutationId,
        client_sequence: ClientSequence,
        session_sequence: SessionSequence,
        documents: Vec<AcceptedDocument>,
    },
    RebaseRequired {
        mutation_id: MutationId,
        document_id: DocumentId,
        authoritative_revision: DocumentRevision,
        delta_since_base: Vec<Transaction>,
    },
    LeaseLost {
        document_id: DocumentId,
        current_lease_epoch: LeaseEpoch,
    },
    Conflict {
        document_id: DocumentId,
        reason: Box<str>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedDocument {
    pub document_id: DocumentId,
    pub accepted_revision: DocumentRevision,
    pub canonical_transaction_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEvent {
    pub session_sequence: SessionSequence,
    pub origin: EventOrigin,
    pub payload: SessionEventPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventOrigin {
    Client(ClientId),
    Workspace,
    Recovery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEventPayload {
    DocumentDelta {
        document_id: DocumentId,
        accepted_revision: DocumentRevision,
        transactions: Vec<Transaction>,
    },
    StateDelta(StateDelta),
    LeaseChange(LeaseGrant),
    ExternalChange {
        document_id: DocumentId,
        content_hash: [u8; 32],
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveRequest {
    pub document_id: DocumentId,
    pub required_frontier: DocumentRevision,
    pub expected_file_identity: FileIdentity,
    pub expected_content_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub document_id: DocumentId,
    pub persisted_frontier: DocumentRevision,
    pub new_file_identity: FileIdentity,
    pub new_content_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    pub device: u64,
    pub file: u64,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateCheckpoint {
    pub client_id: ClientId,
    pub through_client_sequence: ClientSequence,
    pub state: Vec<StateDelta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseGrant {
    pub document_id: DocumentId,
    pub lease_epoch: LeaseEpoch,
    pub holder_id: ClientId,
    pub offline_policy: OfflinePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OfflinePolicy {
    DenyEdits,
    LocalBranch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    pub session_id: SessionId,
    pub session_epoch: SessionEpoch,
    pub last_session_sequence: SessionSequence,
    pub document_frontiers: Vec<DocumentFrontier>,
    pub outstanding_mutation_ids: Vec<MutationId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentFrontier {
    pub document_id: DocumentId,
    pub revision: DocumentRevision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResumeResult {
    Replay {
        events: Vec<SessionEvent>,
    },
    SnapshotRequired {
        new_session_epoch: SessionEpoch,
        workspace_generation: WorkspaceGeneration,
        document_heads: Vec<DocumentFrontier>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DurabilityFrontier {
    Applied,
    LocalDurable,
    RemoteDurable,
    Persisted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceTransaction {
    pub document_edits: Vec<DocumentMutation>,
    pub resource_ops: Vec<ResourceOp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceOp {
    Create {
        path: Box<str>,
        expected_absent: bool,
    },
    Rename {
        from: Box<str>,
        to: Box<str>,
        expected_source_identity: FileIdentity,
        expected_target: ExpectedTarget,
    },
    Delete {
        path: Box<str>,
        expected_identity: FileIdentity,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpectedTarget {
    Absent,
    Identity(FileIdentity),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeViewState {
    pub client_id: ClientId,
    pub view_id: ViewId,
    pub document_id: DocumentId,
    pub document_revision: DocumentRevision,
    pub selections: SelectionSet,
    pub top_line: usize,
    pub rows: usize,
    pub columns: usize,
    pub config_generation: ConfigGeneration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentHead {
    pub session_epoch: SessionEpoch,
    pub document_id: DocumentId,
    pub authoritative_revision: DocumentRevision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedViewportKey {
    pub client_id: ClientId,
    pub view_id: ViewId,
    pub document_revision: DocumentRevision,
    pub rows: usize,
    pub columns: usize,
    pub theme_hash: [u8; 32],
    pub config_generation: ConfigGeneration,
    pub renderer_version: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadValidation {
    Correct,
    Stale { authoritative: DocumentHead },
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandClass {
    Realtime,
    Bounded { budget_micros: u32 },
    Task,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandArgumentType {
    Boolean,
    Integer,
    Number,
    String,
    StringList,
    Enumeration(Vec<Box<str>>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandArgumentSchema {
    pub name: Box<str>,
    pub argument_type: CommandArgumentType,
    pub required: bool,
    pub description: Box<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSchema {
    pub name: Box<str>,
    pub description: Box<str>,
    pub class: CommandClass,
    pub arguments: Vec<CommandArgumentSchema>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CommandValue {
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(Box<str>),
    StringList(Vec<Box<str>>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandInvocation {
    pub command: Box<str>,
    pub arguments: BTreeMap<Box<str>, CommandValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditProposal {
    pub document_id: DocumentId,
    pub base_revision: DocumentRevision,
    pub transactions: Vec<Transaction>,
    pub label: Box<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiEffect {
    OpenDocument {
        document_id: DocumentId,
    },
    RevealRange {
        document_id: DocumentId,
        range: Range<usize>,
    },
    ShowPicker {
        provider: Box<str>,
    },
    Notify {
        message: Box<str>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Effects {
    pub edit_proposals: Vec<EditProposal>,
    pub workspace_transactions: Vec<WorkspaceTransaction>,
    pub ui_effects: Vec<UiEffect>,
    pub messages: Vec<Box<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandTask {
    pub task_id: CommandTaskId,
    pub affected_documents: Vec<DocumentId>,
    pub label: Box<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandOutcome {
    Immediate(Effects),
    Pending(CommandTask),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Base(Range<usize>),
    Inserted { text: Box<str>, anchor: usize },
}

impl Piece {
    fn len(&self) -> usize {
        match self {
            Self::Base(range) => range.len(),
            Self::Inserted { text, .. } => text.len(),
        }
    }
}

fn apply_symbolic_edits(
    pieces: &[Piece],
    edits: &[Edit],
    base_extent: usize,
) -> Result<Vec<Piece>, TransactionError> {
    let total_len = pieces
        .iter()
        .try_fold(0_usize, |sum, piece| sum.checked_add(piece.len()))
        .ok_or(TransactionError::OffsetOverflow)?;
    let mut output = Vec::new();
    let mut cursor = 0;
    for edit in edits {
        if edit.range.end > total_len {
            return Err(TransactionError::OutOfBounds {
                offset: edit.range.end,
                len: total_len,
            });
        }
        output.extend(slice_pieces(pieces, cursor..edit.range.start)?);
        if !edit.insert.is_empty() {
            let anchor_position = if edit.range.is_empty() {
                edit.range.start
            } else {
                edit.range.end
            };
            output.push(Piece::Inserted {
                text: edit.insert.clone(),
                anchor: anchor_at_position(pieces, anchor_position, base_extent)?,
            });
        }
        cursor = edit.range.end;
    }
    output.extend(slice_pieces(pieces, cursor..total_len)?);
    Ok(coalesce_pieces(output))
}

fn slice_pieces(pieces: &[Piece], wanted: Range<usize>) -> Result<Vec<Piece>, TransactionError> {
    if wanted.start > wanted.end {
        return Err(TransactionError::ReversedRange {
            start: wanted.start,
            end: wanted.end,
        });
    }
    let mut output = Vec::new();
    let mut offset = 0_usize;
    for piece in pieces {
        let piece_end = offset
            .checked_add(piece.len())
            .ok_or(TransactionError::OffsetOverflow)?;
        let start = wanted.start.max(offset);
        let end = wanted.end.min(piece_end);
        if start < end {
            let local_start = start - offset;
            let local_end = end - offset;
            match piece {
                Piece::Base(range) => output.push(Piece::Base(
                    (range.start + local_start)..(range.start + local_end),
                )),
                Piece::Inserted { text, anchor } => {
                    let slice = text.get(local_start..local_end).ok_or(
                        TransactionError::NotCharBoundary {
                            offset: local_start,
                        },
                    )?;
                    output.push(Piece::Inserted {
                        text: Box::from(slice),
                        anchor: *anchor,
                    });
                }
            }
        }
        offset = piece_end;
        if offset >= wanted.end {
            break;
        }
    }
    Ok(output)
}

fn coalesce_pieces(pieces: Vec<Piece>) -> Vec<Piece> {
    let mut output: Vec<Piece> = Vec::with_capacity(pieces.len());
    for piece in pieces {
        match (output.last_mut(), piece) {
            (Some(Piece::Base(previous)), Piece::Base(next)) if previous.end == next.start => {
                previous.end = next.end;
            }
            (
                Some(Piece::Inserted {
                    text: previous,
                    anchor: previous_anchor,
                }),
                Piece::Inserted {
                    text: next,
                    anchor: next_anchor,
                },
            ) if *previous_anchor == next_anchor => {
                let mut joined = String::with_capacity(previous.len() + next.len());
                joined.push_str(previous);
                joined.push_str(&next);
                *previous = joined.into_boxed_str();
            }
            (_, piece) => output.push(piece),
        }
    }
    output
}

fn pieces_to_edits(pieces: &[Piece], base_extent: usize) -> Result<Vec<Edit>, TransactionError> {
    let mut edits = Vec::new();
    let mut base_cursor = 0;
    let mut inserted = String::new();

    for piece in pieces {
        match piece {
            Piece::Inserted { text, anchor } => {
                if *anchor < base_cursor {
                    return Err(TransactionError::InvalidComposition);
                }
                if *anchor > base_cursor {
                    edits.push(Edit::new(
                        base_cursor..*anchor,
                        std::mem::take(&mut inserted),
                    ));
                    base_cursor = *anchor;
                }
                inserted.push_str(text);
            }
            Piece::Base(range) => {
                if range.start < base_cursor {
                    return Err(TransactionError::InvalidComposition);
                }
                if range.start > base_cursor || !inserted.is_empty() {
                    edits.push(Edit::new(
                        base_cursor..range.start,
                        std::mem::take(&mut inserted),
                    ));
                }
                base_cursor = range.end;
            }
        }
    }
    if base_cursor < base_extent || !inserted.is_empty() {
        edits.push(Edit::new(base_cursor..base_extent, inserted));
    }

    Ok(edits)
}

fn anchor_at_position(
    pieces: &[Piece],
    position: usize,
    base_extent: usize,
) -> Result<usize, TransactionError> {
    let mut offset = 0_usize;
    for piece in pieces {
        let end = offset
            .checked_add(piece.len())
            .ok_or(TransactionError::OffsetOverflow)?;
        if position < end {
            return match piece {
                Piece::Base(range) => Ok(range.start + (position - offset)),
                Piece::Inserted { anchor, .. } => Ok(*anchor),
            };
        }
        if position == offset {
            return match piece {
                Piece::Base(range) => Ok(range.start),
                Piece::Inserted { anchor, .. } => Ok(*anchor),
            };
        }
        offset = end;
    }
    if position == offset {
        Ok(base_extent)
    } else {
        Err(TransactionError::OutOfBounds {
            offset: position,
            len: offset,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn anchor_bias_controls_insert_boundary() {
        let transaction = Transaction::new(DocumentRevision::new(4), vec![Edit::new(2..2, "xyz")])
            .expect("valid transaction");
        assert_eq!(transaction.map_offset(2, Bias::Left), Ok(2));
        assert_eq!(transaction.map_offset(2, Bias::Right), Ok(5));
    }

    #[test]
    fn selection_mapping_preserves_range_edges() {
        let transaction = Transaction::new(DocumentRevision::new(0), vec![Edit::new(2..4, "long")])
            .expect("valid transaction");
        let selection = SelectionSet {
            primary: 0,
            ranges: vec![SelRange { anchor: 2, head: 4 }],
        };
        let mapped = selection
            .map_through(&transaction)
            .expect("mapping succeeds");
        assert_eq!(mapped.ranges, vec![SelRange { anchor: 2, head: 6 }]);
    }

    #[test]
    fn semantic_mutation_requires_a_contiguous_revision_chain() {
        let document_id = DocumentId::new(3);
        let mutation = ClientMutation {
            mutation_id: MutationId::new(10),
            client_id: ClientId::new(4),
            client_sequence: ClientSequence::new(7),
            state_deltas: Vec::new(),
            documents: vec![DocumentMutation {
                document_id,
                lease_epoch: LeaseEpoch::new(2),
                base_revision: DocumentRevision::new(8),
                semantic_group_id: SemanticGroupId::new(9),
                semantic_group_kind: SemanticGroupKind::InsertRun,
                undo_parent: None,
                transactions: vec![
                    Transaction::new(DocumentRevision::new(8), vec![Edit::new(0..0, "a")])
                        .expect("first transaction"),
                    Transaction::new(DocumentRevision::new(10), vec![Edit::new(1..1, "b")])
                        .expect("second transaction"),
                ],
            }],
        };
        assert_eq!(
            mutation.validate(),
            Err(MutationValidationError::RevisionChain {
                document_id,
                expected: 9,
                actual: 10,
            })
        );
    }

    #[test]
    fn durability_frontiers_are_strictly_ordered() {
        assert!(DurabilityFrontier::Applied < DurabilityFrontier::LocalDurable);
        assert!(DurabilityFrontier::LocalDurable < DurabilityFrontier::RemoteDurable);
        assert!(DurabilityFrontier::RemoteDurable < DurabilityFrontier::Persisted);
    }

    #[test]
    fn document_classification_accounts_for_size_lines_sampling_and_generation() {
        let base = DocumentProfile {
            byte_length: 1_024,
            longest_line_estimate: 80,
            parse_bytes_per_millisecond: Some(1_000_000),
            generated_file: false,
        };
        assert_eq!(DocumentClass::classify(base), DocumentClass::Normal);
        assert_eq!(
            DocumentClass::classify(DocumentProfile {
                generated_file: true,
                ..base
            }),
            DocumentClass::Large
        );
        assert_eq!(
            DocumentClass::classify(DocumentProfile {
                longest_line_estimate: DocumentClass::PATHOLOGICAL_LINE_BYTES,
                ..base
            }),
            DocumentClass::Pathological
        );
    }

    #[test]
    fn degraded_document_policies_keep_native_completion_but_bound_provider_work() {
        let normal = DocumentClass::Normal.policy();
        let large = DocumentClass::Large.policy();
        let pathological = DocumentClass::Pathological.policy();
        assert!(normal.whole_document_syntax);
        assert!(!large.whole_document_syntax);
        assert!(!pathological.whole_document_syntax);
        assert!(large.native_completion && pathological.native_completion);
        assert!(normal.syntax_cpu_budget_micros > large.syntax_cpu_budget_micros);
        assert!(large.syntax_cpu_budget_micros > pathological.syntax_cpu_budget_micros);
        assert_eq!(normal.syntax_cpu_budget_micros, 2_000);
        assert_eq!(large.syntax_cpu_budget_micros, 1_000);
        assert_eq!(pathological.syntax_cpu_budget_micros, 250);
        assert!(pathological.approximate_display_columns);
    }

    #[test]
    fn remote_open_never_enables_sparse_editing_or_claims_unvalidated_correctness() {
        let progressive = RemoteOpenState::Progressive {
            authoritative_revision: DocumentRevision::new(7),
            received_bytes: 1_024,
            total_bytes: 8_192,
        };
        assert_eq!(
            progressive.frame_authority(),
            RemoteFrameAuthority::Speculative
        );
        assert!(!progressive.editing_enabled());
        assert!(!progressive.whole_document_operations_enabled());
        let cached = RemoteOpenState::CachedHeadValidated {
            revision: DocumentRevision::new(7),
        };
        assert_eq!(cached.frame_authority(), RemoteFrameAuthority::Correct);
        assert!(cached.editing_enabled());
    }

    #[test]
    fn language_bundle_identity_changes_provider_generation() {
        let bundle = LanguageBundle {
            language_id: "rust".into(),
            grammar_hash: [1; 32],
            grammar_abi: 15,
            grammar_semver: "0.24.0".into(),
            highlight_query_hash: [2; 32],
            object_query_hash: [3; 32],
            outline_query_hash: [4; 32],
            injection_query_hash: [5; 32],
            config_schema_version: 1,
        };
        let mut changed = bundle.clone();
        changed.highlight_query_hash = [9; 32];
        assert_ne!(bundle.content_hash(), changed.content_hash());
        assert_ne!(bundle.provider_generation(), changed.provider_generation());
    }

    proptest! {
        #[test]
        fn compose_matches_sequential_application(
            base in "[a-z]{0,48}",
            a in 0_usize..96,
            b in 0_usize..96,
            first_insert in "[A-Z]{0,10}",
            c in 0_usize..96,
            d in 0_usize..96,
            second_insert in "[0-9]{0,10}",
        ) {
            let first_start = a.min(b).min(base.len());
            let first_end = a.max(b).min(base.len());
            let first = Transaction::new(
                DocumentRevision::new(7),
                vec![Edit::new(first_start..first_end, first_insert)],
            ).expect("generated first transaction is valid");
            let middle = first.apply_to_string(&base).expect("first applies");
            let second_start = c.min(d).min(middle.len());
            let second_end = c.max(d).min(middle.len());
            let second = Transaction::new(
                DocumentRevision::new(8),
                vec![Edit::new(second_start..second_end, second_insert)],
            ).expect("generated second transaction is valid");
            let sequential = second.apply_to_string(&middle).expect("second applies");
            let composed = first.then(&second).expect("transactions compose");
            let direct = composed.apply_to_string(&base).expect("composition applies");
            prop_assert_eq!(direct, sequential);
        }

        #[test]
        fn map_then_map_equals_map_composed(
            base in "[a-z]{0,48}",
            a in 0_usize..96,
            b in 0_usize..96,
            first_insert in "[A-Z]{0,10}",
            c in 0_usize..96,
            d in 0_usize..96,
            second_insert in "[0-9]{0,10}",
            raw_offset in 0_usize..96,
            use_right_bias in any::<bool>(),
        ) {
            let first_start = a.min(b).min(base.len());
            let first_end = a.max(b).min(base.len());
            let first = Transaction::new(
                DocumentRevision::new(12),
                vec![Edit::new(first_start..first_end, first_insert)],
            ).expect("valid first");
            let middle = first.apply_to_string(&base).expect("first applies");
            let second_start = c.min(d).min(middle.len());
            let second_end = c.max(d).min(middle.len());
            let second = Transaction::new(
                DocumentRevision::new(13),
                vec![Edit::new(second_start..second_end, second_insert)],
            ).expect("valid second");
            let composed = first.then(&second).expect("composition succeeds");
            let offset = raw_offset.min(base.len());
            let bias = if use_right_bias { Bias::Right } else { Bias::Left };
            let sequential = second.map_offset(
                first.map_offset(offset, bias).expect("first map"),
                bias,
            ).expect("second map");
            let direct = composed.map_offset(offset, bias).expect("composed map");
            prop_assert_eq!(direct, sequential);
        }

        #[test]
        fn inverse_restores_original(
            base in "[a-z]{0,64}",
            a in 0_usize..128,
            b in 0_usize..128,
            insert in "[A-Z0-9]{0,12}",
        ) {
            let start = a.min(b).min(base.len());
            let end = a.max(b).min(base.len());
            let transaction = Transaction::new(
                DocumentRevision::new(2),
                vec![Edit::new(start..end, insert)],
            ).expect("valid transaction");
            let inverse = transaction.inverted_against(&base).expect("inverse exists");
            let changed = transaction.apply_to_string(&base).expect("forward applies");
            let restored = inverse.apply_to_string(&changed).expect("inverse applies");
            prop_assert_eq!(restored, base);
        }
    }
}
