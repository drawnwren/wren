use std::io::{self, Read, Write};

use prost::{Enumeration, Message, Oneof};
use thiserror::Error;
use wren_types as semantic;

pub const PROTOCOL_MAJOR: u32 = 1;
pub const PROTOCOL_MINOR: u32 = 1;
pub const PROTOCOL_VERSION: u32 = (PROTOCOL_MAJOR << 16) | PROTOCOL_MINOR;
pub const DEFAULT_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const SCHEMA: &str = include_str!("../proto/wren.proto");

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("unsupported protocol version {actual:#010x}; expected major {expected_major}")]
    UnsupportedVersion { actual: u32, expected_major: u32 },
    #[error("protocol field {0} is required")]
    MissingField(&'static str),
    #[error("protocol field {field} has invalid value {value}")]
    InvalidEnum { field: &'static str, value: i32 },
    #[error("protocol field {field} does not fit this address space")]
    IntegerOverflow { field: &'static str },
    #[error("protocol field {field} must contain exactly 32 bytes, got {actual}")]
    HashLength { field: &'static str, actual: usize },
    #[error("protocol field {field} must contain exactly one Unicode scalar")]
    InvalidCharacter { field: &'static str },
    #[error("encoded frame is {actual} bytes, exceeding limit {limit}")]
    FrameTooLarge { actual: usize, limit: usize },
    #[error("decode protobuf frame: {0}")]
    Decode(String),
    #[error("decoded frame has {0} trailing bytes")]
    TrailingBytes(usize),
    #[error("invalid semantic transaction: {0}")]
    InvalidTransaction(String),
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("protocol transport I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("protobuf length delimiter exceeds ten bytes")]
    InvalidLengthDelimiter,
}

#[derive(Clone, PartialEq, Message)]
pub struct Envelope {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(uint64, tag = "2")]
    pub request_id: u64,
    #[prost(
        oneof = "envelope::Payload",
        tags = "10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22"
    )]
    pub payload: Option<envelope::Payload>,
}

pub mod envelope {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Payload {
        #[prost(message, tag = "10")]
        Hello(Hello),
        #[prost(message, tag = "11")]
        HelloAck(HelloAck),
        #[prost(message, tag = "12")]
        ClientMutation(ClientMutation),
        #[prost(message, tag = "13")]
        MutationResult(MutationResult),
        #[prost(message, tag = "14")]
        SessionEvent(SessionEvent),
        #[prost(message, tag = "15")]
        Resume(Resume),
        #[prost(message, tag = "16")]
        ResumeResult(ResumeResult),
        #[prost(message, tag = "17")]
        SaveRequest(SaveRequest),
        #[prost(message, tag = "18")]
        Saved(Saved),
        #[prost(message, tag = "19")]
        OpenDocument(OpenDocument),
        #[prost(message, tag = "20")]
        DocumentOpened(DocumentOpened),
        #[prost(message, tag = "21")]
        RemoteCall(RemoteCall),
        #[prost(message, tag = "22")]
        RemoteReply(RemoteReply),
    }
}

/// Versioned remote-workspace RPC. The body is a serde representation owned by
/// `wren-remote`; keeping it behind a protobuf envelope lets protocol-major,
/// request-id, frame-limit, and capability rules remain shared with sessions.
#[derive(Clone, PartialEq, Message)]
pub struct RemoteCall {
    #[prost(bytes = "vec", tag = "1")]
    pub body: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RemoteReply {
    #[prost(bytes = "vec", tag = "1")]
    pub body: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct OpenDocument {
    #[prost(uint64, tag = "1")]
    pub document_id: u64,
    #[prost(uint64, tag = "2")]
    pub client_id: u64,
    #[prost(string, tag = "3")]
    pub text: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct DocumentOpened {
    #[prost(uint64, tag = "1")]
    pub document_id: u64,
    #[prost(uint64, tag = "2")]
    pub revision: u64,
    #[prost(uint64, tag = "3")]
    pub lease_epoch: u64,
    #[prost(uint64, tag = "4")]
    pub session_epoch: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Hello {
    #[prost(uint32, tag = "1")]
    pub major: u32,
    #[prost(uint32, tag = "2")]
    pub minor: u32,
    #[prost(string, repeated, tag = "3")]
    pub capabilities: Vec<String>,
    #[prost(uint64, tag = "4")]
    pub max_frame_bytes: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct HelloAck {
    #[prost(uint32, tag = "1")]
    pub major: u32,
    #[prost(uint32, tag = "2")]
    pub minor: u32,
    #[prost(string, repeated, tag = "3")]
    pub capabilities: Vec<String>,
    #[prost(uint64, tag = "4")]
    pub max_frame_bytes: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Edit {
    #[prost(uint64, tag = "1")]
    pub start: u64,
    #[prost(uint64, tag = "2")]
    pub end: u64,
    #[prost(string, tag = "3")]
    pub insert: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Transaction {
    #[prost(uint64, tag = "1")]
    pub base_revision: u64,
    #[prost(message, repeated, tag = "2")]
    pub edits: Vec<Edit>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RegisterDelta {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub text: String,
    #[prost(bool, tag = "3")]
    pub linewise: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct GlobalMarkDelta {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(uint64, tag = "2")]
    pub document_id: u64,
    #[prost(uint64, tag = "3")]
    pub byte: u64,
    #[prost(bool, tag = "4")]
    pub right_bias: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct UndoBranchDelta {
    #[prost(uint64, tag = "1")]
    pub document_id: u64,
    #[prost(uint64, optional, tag = "2")]
    pub semantic_group_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct MacroRecordingDelta {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(bytes = "vec", tag = "2")]
    pub raw_keys: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub lowered_ir: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DurableJumpEntry {
    #[prost(uint64, tag = "1")]
    pub document_id: u64,
    #[prost(uint64, tag = "2")]
    pub byte: u64,
    #[prost(bool, tag = "3")]
    pub right_bias: bool,
    #[prost(string, optional, tag = "4")]
    pub path_hint: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct JumpListDelta {
    #[prost(message, repeated, tag = "1")]
    pub entries: Vec<DurableJumpEntry>,
    #[prost(uint64, optional, tag = "2")]
    pub current: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct StateDelta {
    #[prost(oneof = "state_delta::Value", tags = "1, 2, 3, 4, 5, 6, 7, 8, 9")]
    pub value: Option<state_delta::Value>,
}

pub mod state_delta {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Value {
        #[prost(message, tag = "1")]
        Register(RegisterDelta),
        #[prost(string, tag = "2")]
        SearchPattern(String),
        #[prost(string, tag = "3")]
        CommandHistory(String),
        #[prost(message, tag = "4")]
        GlobalMark(GlobalMarkDelta),
        #[prost(message, tag = "5")]
        UndoBranch(UndoBranchDelta),
        #[prost(bytes = "vec", tag = "6")]
        RepeatData(Vec<u8>),
        #[prost(message, tag = "7")]
        MacroRecording(MacroRecordingDelta),
        #[prost(message, tag = "8")]
        JumpList(JumpListDelta),
        #[prost(bool, tag = "9")]
        SearchBackward(bool),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum SemanticGroupKind {
    InsertRun = 0,
    Operator = 1,
    MacroInvocation = 2,
    Formatter = 3,
    WorkspaceRefactor = 4,
    UndoOf = 5,
    RedoOf = 6,
}

#[derive(Clone, PartialEq, Message)]
pub struct DocumentMutation {
    #[prost(uint64, tag = "1")]
    pub document_id: u64,
    #[prost(uint64, tag = "2")]
    pub lease_epoch: u64,
    #[prost(uint64, tag = "3")]
    pub base_revision: u64,
    #[prost(uint64, tag = "4")]
    pub semantic_group_id: u64,
    #[prost(enumeration = "SemanticGroupKind", tag = "5")]
    pub semantic_group_kind: i32,
    #[prost(uint64, optional, tag = "6")]
    pub related_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "7")]
    pub undo_parent: Option<u64>,
    #[prost(message, repeated, tag = "8")]
    pub transactions: Vec<Transaction>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientMutation {
    #[prost(uint64, tag = "1")]
    pub mutation_id: u64,
    #[prost(uint64, tag = "2")]
    pub client_id: u64,
    #[prost(uint64, tag = "3")]
    pub client_sequence: u64,
    #[prost(message, repeated, tag = "4")]
    pub state_deltas: Vec<StateDelta>,
    #[prost(message, repeated, tag = "5")]
    pub documents: Vec<DocumentMutation>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AcceptedDocument {
    #[prost(uint64, tag = "1")]
    pub document_id: u64,
    #[prost(uint64, tag = "2")]
    pub accepted_revision: u64,
    #[prost(bytes = "vec", tag = "3")]
    pub canonical_transaction_hash: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Received {
    #[prost(uint64, tag = "1")]
    pub mutation_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Durable {
    #[prost(uint64, tag = "1")]
    pub mutation_id: u64,
    #[prost(uint64, tag = "2")]
    pub client_sequence: u64,
    #[prost(uint64, tag = "3")]
    pub session_sequence: u64,
    #[prost(message, repeated, tag = "4")]
    pub documents: Vec<AcceptedDocument>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RebaseRequired {
    #[prost(uint64, tag = "1")]
    pub mutation_id: u64,
    #[prost(uint64, tag = "2")]
    pub document_id: u64,
    #[prost(uint64, tag = "3")]
    pub authoritative_revision: u64,
    #[prost(message, repeated, tag = "4")]
    pub delta_since_base: Vec<Transaction>,
}

#[derive(Clone, PartialEq, Message)]
pub struct LeaseLost {
    #[prost(uint64, tag = "1")]
    pub document_id: u64,
    #[prost(uint64, tag = "2")]
    pub current_lease_epoch: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Conflict {
    #[prost(uint64, tag = "1")]
    pub document_id: u64,
    #[prost(string, tag = "2")]
    pub reason: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct MutationResult {
    #[prost(oneof = "mutation_result::Value", tags = "1, 2, 3, 4, 5")]
    pub value: Option<mutation_result::Value>,
}

pub mod mutation_result {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Value {
        #[prost(message, tag = "1")]
        Received(Received),
        #[prost(message, tag = "2")]
        Durable(Durable),
        #[prost(message, tag = "3")]
        RebaseRequired(RebaseRequired),
        #[prost(message, tag = "4")]
        LeaseLost(LeaseLost),
        #[prost(message, tag = "5")]
        Conflict(Conflict),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum OfflinePolicy {
    DenyEdits = 0,
    LocalBranch = 1,
}

#[derive(Clone, PartialEq, Message)]
pub struct LeaseGrant {
    #[prost(uint64, tag = "1")]
    pub document_id: u64,
    #[prost(uint64, tag = "2")]
    pub lease_epoch: u64,
    #[prost(uint64, tag = "3")]
    pub holder_id: u64,
    #[prost(enumeration = "OfflinePolicy", tag = "4")]
    pub offline_policy: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum EventOriginKind {
    Client = 0,
    Workspace = 1,
    Recovery = 2,
}

#[derive(Clone, PartialEq, Message)]
pub struct EventOrigin {
    #[prost(enumeration = "EventOriginKind", tag = "1")]
    pub kind: i32,
    #[prost(uint64, optional, tag = "2")]
    pub client_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DocumentDelta {
    #[prost(uint64, tag = "1")]
    pub document_id: u64,
    #[prost(uint64, tag = "2")]
    pub accepted_revision: u64,
    #[prost(message, repeated, tag = "3")]
    pub transactions: Vec<Transaction>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExternalChange {
    #[prost(uint64, tag = "1")]
    pub document_id: u64,
    #[prost(bytes = "vec", tag = "2")]
    pub content_hash: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionEventPayload {
    #[prost(oneof = "session_event_payload::Value", tags = "1, 2, 3, 4")]
    pub value: Option<session_event_payload::Value>,
}

pub mod session_event_payload {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Value {
        #[prost(message, tag = "1")]
        DocumentDelta(DocumentDelta),
        #[prost(message, tag = "2")]
        StateDelta(StateDelta),
        #[prost(message, tag = "3")]
        LeaseChange(LeaseGrant),
        #[prost(message, tag = "4")]
        ExternalChange(ExternalChange),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionEvent {
    #[prost(uint64, tag = "1")]
    pub session_sequence: u64,
    #[prost(message, optional, tag = "2")]
    pub origin: Option<EventOrigin>,
    #[prost(message, optional, tag = "3")]
    pub payload: Option<SessionEventPayload>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DocumentFrontier {
    #[prost(uint64, tag = "1")]
    pub document_id: u64,
    #[prost(uint64, tag = "2")]
    pub revision: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Resume {
    #[prost(uint64, tag = "1")]
    pub session_id: u64,
    #[prost(uint64, tag = "2")]
    pub session_epoch: u64,
    #[prost(uint64, tag = "3")]
    pub last_session_sequence: u64,
    #[prost(message, repeated, tag = "4")]
    pub document_frontiers: Vec<DocumentFrontier>,
    #[prost(uint64, repeated, tag = "5")]
    pub outstanding_mutation_ids: Vec<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Replay {
    #[prost(message, repeated, tag = "1")]
    pub events: Vec<SessionEvent>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SnapshotRequired {
    #[prost(uint64, tag = "1")]
    pub new_session_epoch: u64,
    #[prost(uint64, tag = "2")]
    pub workspace_generation: u64,
    #[prost(message, repeated, tag = "3")]
    pub document_heads: Vec<DocumentFrontier>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ResumeResult {
    #[prost(oneof = "resume_result::Value", tags = "1, 2")]
    pub value: Option<resume_result::Value>,
}

pub mod resume_result {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Value {
        #[prost(message, tag = "1")]
        Replay(Replay),
        #[prost(message, tag = "2")]
        SnapshotRequired(SnapshotRequired),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct FileIdentity {
    #[prost(uint64, tag = "1")]
    pub device: u64,
    #[prost(uint64, tag = "2")]
    pub file: u64,
    #[prost(uint64, tag = "3")]
    pub generation: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct SaveRequest {
    #[prost(uint64, tag = "1")]
    pub document_id: u64,
    #[prost(uint64, tag = "2")]
    pub required_frontier: u64,
    #[prost(message, optional, tag = "3")]
    pub expected_file_identity: Option<FileIdentity>,
    #[prost(bytes = "vec", tag = "4")]
    pub expected_content_hash: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Saved {
    #[prost(uint64, tag = "1")]
    pub document_id: u64,
    #[prost(uint64, tag = "2")]
    pub persisted_frontier: u64,
    #[prost(message, optional, tag = "3")]
    pub new_file_identity: Option<FileIdentity>,
    #[prost(bytes = "vec", tag = "4")]
    pub new_content_hash: Vec<u8>,
}

impl Envelope {
    #[must_use]
    pub fn new(request_id: u64, payload: envelope::Payload) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            request_id,
            payload: Some(payload),
        }
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.protocol_version >> 16 != PROTOCOL_MAJOR {
            return Err(ProtocolError::UnsupportedVersion {
                actual: self.protocol_version,
                expected_major: PROTOCOL_MAJOR,
            });
        }
        if self.payload.is_none() {
            return Err(ProtocolError::MissingField("Envelope.payload"));
        }
        Ok(())
    }
}

pub fn encode_frame(envelope: &Envelope, limit: usize) -> Result<Vec<u8>, ProtocolError> {
    envelope.validate()?;
    let encoded_len = envelope.encoded_len();
    if encoded_len > limit {
        return Err(ProtocolError::FrameTooLarge {
            actual: encoded_len,
            limit,
        });
    }
    let mut bytes = Vec::with_capacity(encoded_len + 10);
    envelope
        .encode_length_delimited(&mut bytes)
        .map_err(|error| ProtocolError::Decode(error.to_string()))?;
    Ok(bytes)
}

pub fn decode_frame(mut bytes: &[u8], limit: usize) -> Result<Envelope, ProtocolError> {
    if bytes.len() > limit.saturating_add(10) {
        return Err(ProtocolError::FrameTooLarge {
            actual: bytes.len(),
            limit,
        });
    }
    let envelope = Envelope::decode_length_delimited(&mut bytes)
        .map_err(|error| ProtocolError::Decode(error.to_string()))?;
    if !bytes.is_empty() {
        return Err(ProtocolError::TrailingBytes(bytes.len()));
    }
    envelope.validate()?;
    Ok(envelope)
}

/// Reads one length-delimited envelope. EOF before a new delimiter is a clean
/// connection close; EOF after any delimiter byte is an I/O error.
pub fn read_envelope(
    reader: &mut impl Read,
    limit: usize,
) -> Result<Option<Envelope>, TransportError> {
    let mut first = [0_u8; 1];
    match reader.read(&mut first) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
            return read_envelope(reader, limit);
        }
        Err(error) => return Err(error.into()),
    }
    let mut length = u64::from(first[0] & 0x7f);
    let mut shift = 7_u32;
    let mut byte = first[0];
    let mut delimiter_bytes = 1_usize;
    while byte & 0x80 != 0 {
        if delimiter_bytes >= 10 || shift >= 64 {
            return Err(TransportError::InvalidLengthDelimiter);
        }
        let mut next = [0_u8; 1];
        reader.read_exact(&mut next)?;
        byte = next[0];
        let component = u64::from(byte & 0x7f)
            .checked_shl(shift)
            .ok_or(TransportError::InvalidLengthDelimiter)?;
        length = length
            .checked_add(component)
            .ok_or(TransportError::InvalidLengthDelimiter)?;
        shift += 7;
        delimiter_bytes += 1;
    }
    let length = usize::try_from(length).map_err(|_| ProtocolError::IntegerOverflow {
        field: "frame length delimiter",
    })?;
    if length > limit {
        return Err(ProtocolError::FrameTooLarge {
            actual: length,
            limit,
        }
        .into());
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    let envelope = Envelope::decode(payload.as_slice())
        .map_err(|error| ProtocolError::Decode(error.to_string()))?;
    envelope.validate()?;
    Ok(Some(envelope))
}

pub fn write_envelope(
    writer: &mut impl Write,
    envelope: &Envelope,
    limit: usize,
) -> Result<(), TransportError> {
    envelope.validate()?;
    let length = envelope.encoded_len();
    if length > limit {
        return Err(ProtocolError::FrameTooLarge {
            actual: length,
            limit,
        }
        .into());
    }
    write_varint(writer, length as u64)?;
    let mut payload = Vec::with_capacity(length);
    envelope
        .encode(&mut payload)
        .map_err(|error| ProtocolError::Decode(error.to_string()))?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

fn write_varint(writer: &mut impl Write, mut value: u64) -> io::Result<()> {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        writer.write_all(&[byte])?;
        if value == 0 {
            return Ok(());
        }
    }
}

fn required<T>(value: Option<T>, field: &'static str) -> Result<T, ProtocolError> {
    value.ok_or(ProtocolError::MissingField(field))
}

fn checked_usize(value: u64, field: &'static str) -> Result<usize, ProtocolError> {
    usize::try_from(value).map_err(|_| ProtocolError::IntegerOverflow { field })
}

fn one_char(value: String, field: &'static str) -> Result<char, ProtocolError> {
    let mut chars = value.chars();
    let character = chars
        .next()
        .ok_or(ProtocolError::InvalidCharacter { field })?;
    if chars.next().is_some() {
        return Err(ProtocolError::InvalidCharacter { field });
    }
    Ok(character)
}

fn hash32(value: Vec<u8>, field: &'static str) -> Result<[u8; 32], ProtocolError> {
    let actual = value.len();
    value
        .try_into()
        .map_err(|_| ProtocolError::HashLength { field, actual })
}

impl From<&semantic::Transaction> for Transaction {
    fn from(value: &semantic::Transaction) -> Self {
        Self {
            base_revision: value.base_revision.get(),
            edits: value
                .edits
                .iter()
                .map(|edit| Edit {
                    start: edit.range.start as u64,
                    end: edit.range.end as u64,
                    insert: edit.insert.to_string(),
                })
                .collect(),
        }
    }
}

impl TryFrom<Transaction> for semantic::Transaction {
    type Error = ProtocolError;

    fn try_from(value: Transaction) -> Result<Self, Self::Error> {
        let edits = value
            .edits
            .into_iter()
            .map(|edit| {
                Ok(semantic::Edit::new(
                    checked_usize(edit.start, "Edit.start")?..checked_usize(edit.end, "Edit.end")?,
                    edit.insert,
                ))
            })
            .collect::<Result<Vec<_>, ProtocolError>>()?;
        semantic::Transaction::new(semantic::DocumentRevision::new(value.base_revision), edits)
            .map_err(|error| ProtocolError::InvalidTransaction(error.to_string()))
    }
}

impl From<&semantic::StateDelta> for StateDelta {
    fn from(value: &semantic::StateDelta) -> Self {
        let value = match value {
            semantic::StateDelta::Register {
                name,
                text,
                linewise,
            } => state_delta::Value::Register(RegisterDelta {
                name: name.to_string(),
                text: text.to_string(),
                linewise: *linewise,
            }),
            semantic::StateDelta::SearchPattern(pattern) => {
                state_delta::Value::SearchPattern(pattern.to_string())
            }
            semantic::StateDelta::SearchDirection { backward } => {
                state_delta::Value::SearchBackward(*backward)
            }
            semantic::StateDelta::CommandHistory(command) => {
                state_delta::Value::CommandHistory(command.to_string())
            }
            semantic::StateDelta::GlobalMark {
                name,
                document_id,
                anchor,
            } => state_delta::Value::GlobalMark(GlobalMarkDelta {
                name: name.to_string(),
                document_id: document_id.get(),
                byte: anchor.byte as u64,
                right_bias: anchor.bias == semantic::Bias::Right,
            }),
            semantic::StateDelta::UndoBranchHead {
                document_id,
                semantic_group_id,
            } => state_delta::Value::UndoBranch(UndoBranchDelta {
                document_id: document_id.get(),
                semantic_group_id: semantic_group_id.map(semantic::SemanticGroupId::get),
            }),
            semantic::StateDelta::RepeatData(data) => state_delta::Value::RepeatData(data.clone()),
            semantic::StateDelta::MacroRecording {
                name,
                raw_keys,
                lowered_ir,
            } => state_delta::Value::MacroRecording(MacroRecordingDelta {
                name: name.to_string(),
                raw_keys: raw_keys.clone(),
                lowered_ir: lowered_ir.clone(),
            }),
            semantic::StateDelta::JumpList { entries, current } => {
                state_delta::Value::JumpList(JumpListDelta {
                    entries: entries
                        .iter()
                        .map(|entry| DurableJumpEntry {
                            document_id: entry.document_id.get(),
                            byte: entry.anchor.byte as u64,
                            right_bias: entry.anchor.bias == semantic::Bias::Right,
                            path_hint: entry.path_hint.as_deref().map(str::to_owned),
                        })
                        .collect(),
                    current: current.and_then(|index| u64::try_from(index).ok()),
                })
            }
        };
        Self { value: Some(value) }
    }
}

impl TryFrom<StateDelta> for semantic::StateDelta {
    type Error = ProtocolError;

    fn try_from(value: StateDelta) -> Result<Self, Self::Error> {
        match required(value.value, "StateDelta.value")? {
            state_delta::Value::Register(register) => Ok(Self::Register {
                name: one_char(register.name, "RegisterDelta.name")?,
                text: register.text.into_boxed_str(),
                linewise: register.linewise,
            }),
            state_delta::Value::SearchPattern(pattern) => {
                Ok(Self::SearchPattern(pattern.into_boxed_str()))
            }
            state_delta::Value::SearchBackward(backward) => Ok(Self::SearchDirection { backward }),
            state_delta::Value::CommandHistory(command) => {
                Ok(Self::CommandHistory(command.into_boxed_str()))
            }
            state_delta::Value::GlobalMark(mark) => Ok(Self::GlobalMark {
                name: one_char(mark.name, "GlobalMarkDelta.name")?,
                document_id: semantic::DocumentId::new(mark.document_id),
                anchor: semantic::Anchor {
                    byte: checked_usize(mark.byte, "GlobalMarkDelta.byte")?,
                    bias: if mark.right_bias {
                        semantic::Bias::Right
                    } else {
                        semantic::Bias::Left
                    },
                },
            }),
            state_delta::Value::UndoBranch(branch) => Ok(Self::UndoBranchHead {
                document_id: semantic::DocumentId::new(branch.document_id),
                semantic_group_id: branch.semantic_group_id.map(semantic::SemanticGroupId::new),
            }),
            state_delta::Value::RepeatData(data) => Ok(Self::RepeatData(data)),
            state_delta::Value::MacroRecording(recording) => Ok(Self::MacroRecording {
                name: one_char(recording.name, "MacroRecordingDelta.name")?,
                raw_keys: recording.raw_keys,
                lowered_ir: recording.lowered_ir,
            }),
            state_delta::Value::JumpList(jumps) => Ok(Self::JumpList {
                entries: jumps
                    .entries
                    .into_iter()
                    .map(|entry| {
                        Ok(semantic::DurableJumpEntry {
                            document_id: semantic::DocumentId::new(entry.document_id),
                            anchor: semantic::Anchor {
                                byte: checked_usize(entry.byte, "DurableJumpEntry.byte")?,
                                bias: if entry.right_bias {
                                    semantic::Bias::Right
                                } else {
                                    semantic::Bias::Left
                                },
                            },
                            path_hint: entry.path_hint.map(String::into_boxed_str),
                        })
                    })
                    .collect::<Result<Vec<_>, ProtocolError>>()?,
                current: jumps
                    .current
                    .map(|index| checked_usize(index, "JumpListDelta.current"))
                    .transpose()?,
            }),
        }
    }
}

impl From<&semantic::DocumentMutation> for DocumentMutation {
    fn from(value: &semantic::DocumentMutation) -> Self {
        let (kind, related_group_id) = match value.semantic_group_kind {
            semantic::SemanticGroupKind::InsertRun => (SemanticGroupKind::InsertRun, None),
            semantic::SemanticGroupKind::Operator => (SemanticGroupKind::Operator, None),
            semantic::SemanticGroupKind::MacroInvocation => {
                (SemanticGroupKind::MacroInvocation, None)
            }
            semantic::SemanticGroupKind::Formatter => (SemanticGroupKind::Formatter, None),
            semantic::SemanticGroupKind::WorkspaceRefactor => {
                (SemanticGroupKind::WorkspaceRefactor, None)
            }
            semantic::SemanticGroupKind::UndoOf(group) => {
                (SemanticGroupKind::UndoOf, Some(group.get()))
            }
            semantic::SemanticGroupKind::RedoOf(group) => {
                (SemanticGroupKind::RedoOf, Some(group.get()))
            }
        };
        Self {
            document_id: value.document_id.get(),
            lease_epoch: value.lease_epoch.get(),
            base_revision: value.base_revision.get(),
            semantic_group_id: value.semantic_group_id.get(),
            semantic_group_kind: kind as i32,
            related_group_id,
            undo_parent: value.undo_parent.map(semantic::SemanticGroupId::get),
            transactions: value.transactions.iter().map(Transaction::from).collect(),
        }
    }
}

impl TryFrom<DocumentMutation> for semantic::DocumentMutation {
    type Error = ProtocolError;

    fn try_from(value: DocumentMutation) -> Result<Self, Self::Error> {
        let tag = SemanticGroupKind::try_from(value.semantic_group_kind).map_err(|_| {
            ProtocolError::InvalidEnum {
                field: "DocumentMutation.semantic_group_kind",
                value: value.semantic_group_kind,
            }
        })?;
        let related_group = value.related_group_id.map(semantic::SemanticGroupId::new);
        let semantic_group_kind = match tag {
            SemanticGroupKind::InsertRun => semantic::SemanticGroupKind::InsertRun,
            SemanticGroupKind::Operator => semantic::SemanticGroupKind::Operator,
            SemanticGroupKind::MacroInvocation => semantic::SemanticGroupKind::MacroInvocation,
            SemanticGroupKind::Formatter => semantic::SemanticGroupKind::Formatter,
            SemanticGroupKind::WorkspaceRefactor => semantic::SemanticGroupKind::WorkspaceRefactor,
            SemanticGroupKind::UndoOf => semantic::SemanticGroupKind::UndoOf(required(
                related_group,
                "DocumentMutation.related_group_id for UndoOf",
            )?),
            SemanticGroupKind::RedoOf => semantic::SemanticGroupKind::RedoOf(required(
                related_group,
                "DocumentMutation.related_group_id for RedoOf",
            )?),
        };
        let mutation = Self {
            document_id: semantic::DocumentId::new(value.document_id),
            lease_epoch: semantic::LeaseEpoch::new(value.lease_epoch),
            base_revision: semantic::DocumentRevision::new(value.base_revision),
            semantic_group_id: semantic::SemanticGroupId::new(value.semantic_group_id),
            semantic_group_kind,
            undo_parent: value.undo_parent.map(semantic::SemanticGroupId::new),
            transactions: value
                .transactions
                .into_iter()
                .map(semantic::Transaction::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        };
        mutation
            .validate()
            .map_err(|error| ProtocolError::InvalidTransaction(error.to_string()))?;
        Ok(mutation)
    }
}

impl From<&semantic::ClientMutation> for ClientMutation {
    fn from(value: &semantic::ClientMutation) -> Self {
        Self {
            mutation_id: value.mutation_id.get(),
            client_id: value.client_id.get(),
            client_sequence: value.client_sequence.get(),
            state_deltas: value.state_deltas.iter().map(StateDelta::from).collect(),
            documents: value.documents.iter().map(DocumentMutation::from).collect(),
        }
    }
}

impl TryFrom<ClientMutation> for semantic::ClientMutation {
    type Error = ProtocolError;

    fn try_from(value: ClientMutation) -> Result<Self, Self::Error> {
        let mutation = Self {
            mutation_id: semantic::MutationId::new(value.mutation_id),
            client_id: semantic::ClientId::new(value.client_id),
            client_sequence: semantic::ClientSequence::new(value.client_sequence),
            state_deltas: value
                .state_deltas
                .into_iter()
                .map(semantic::StateDelta::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            documents: value
                .documents
                .into_iter()
                .map(semantic::DocumentMutation::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        };
        mutation
            .validate()
            .map_err(|error| ProtocolError::InvalidTransaction(error.to_string()))?;
        Ok(mutation)
    }
}

impl From<&semantic::MutationResult> for MutationResult {
    fn from(value: &semantic::MutationResult) -> Self {
        let value = match value {
            semantic::MutationResult::Received { mutation_id } => {
                mutation_result::Value::Received(Received {
                    mutation_id: mutation_id.get(),
                })
            }
            semantic::MutationResult::Durable {
                mutation_id,
                client_sequence,
                session_sequence,
                documents,
            } => mutation_result::Value::Durable(Durable {
                mutation_id: mutation_id.get(),
                client_sequence: client_sequence.get(),
                session_sequence: session_sequence.get(),
                documents: documents
                    .iter()
                    .map(|document| AcceptedDocument {
                        document_id: document.document_id.get(),
                        accepted_revision: document.accepted_revision.get(),
                        canonical_transaction_hash: document.canonical_transaction_hash.to_vec(),
                    })
                    .collect(),
            }),
            semantic::MutationResult::RebaseRequired {
                mutation_id,
                document_id,
                authoritative_revision,
                delta_since_base,
            } => mutation_result::Value::RebaseRequired(RebaseRequired {
                mutation_id: mutation_id.get(),
                document_id: document_id.get(),
                authoritative_revision: authoritative_revision.get(),
                delta_since_base: delta_since_base.iter().map(Transaction::from).collect(),
            }),
            semantic::MutationResult::LeaseLost {
                document_id,
                current_lease_epoch,
            } => mutation_result::Value::LeaseLost(LeaseLost {
                document_id: document_id.get(),
                current_lease_epoch: current_lease_epoch.get(),
            }),
            semantic::MutationResult::Conflict {
                document_id,
                reason,
            } => mutation_result::Value::Conflict(Conflict {
                document_id: document_id.get(),
                reason: reason.to_string(),
            }),
        };
        Self { value: Some(value) }
    }
}

impl TryFrom<MutationResult> for semantic::MutationResult {
    type Error = ProtocolError;

    fn try_from(value: MutationResult) -> Result<Self, Self::Error> {
        match required(value.value, "MutationResult.value")? {
            mutation_result::Value::Received(received) => Ok(Self::Received {
                mutation_id: semantic::MutationId::new(received.mutation_id),
            }),
            mutation_result::Value::Durable(durable) => Ok(Self::Durable {
                mutation_id: semantic::MutationId::new(durable.mutation_id),
                client_sequence: semantic::ClientSequence::new(durable.client_sequence),
                session_sequence: semantic::SessionSequence::new(durable.session_sequence),
                documents: durable
                    .documents
                    .into_iter()
                    .map(|document| {
                        Ok(semantic::AcceptedDocument {
                            document_id: semantic::DocumentId::new(document.document_id),
                            accepted_revision: semantic::DocumentRevision::new(
                                document.accepted_revision,
                            ),
                            canonical_transaction_hash: hash32(
                                document.canonical_transaction_hash,
                                "AcceptedDocument.canonical_transaction_hash",
                            )?,
                        })
                    })
                    .collect::<Result<Vec<_>, ProtocolError>>()?,
            }),
            mutation_result::Value::RebaseRequired(rebase) => Ok(Self::RebaseRequired {
                mutation_id: semantic::MutationId::new(rebase.mutation_id),
                document_id: semantic::DocumentId::new(rebase.document_id),
                authoritative_revision: semantic::DocumentRevision::new(
                    rebase.authoritative_revision,
                ),
                delta_since_base: rebase
                    .delta_since_base
                    .into_iter()
                    .map(semantic::Transaction::try_from)
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            mutation_result::Value::LeaseLost(lease) => Ok(Self::LeaseLost {
                document_id: semantic::DocumentId::new(lease.document_id),
                current_lease_epoch: semantic::LeaseEpoch::new(lease.current_lease_epoch),
            }),
            mutation_result::Value::Conflict(conflict) => Ok(Self::Conflict {
                document_id: semantic::DocumentId::new(conflict.document_id),
                reason: conflict.reason.into_boxed_str(),
            }),
        }
    }
}

impl From<&semantic::LeaseGrant> for LeaseGrant {
    fn from(value: &semantic::LeaseGrant) -> Self {
        Self {
            document_id: value.document_id.get(),
            lease_epoch: value.lease_epoch.get(),
            holder_id: value.holder_id.get(),
            offline_policy: match value.offline_policy {
                semantic::OfflinePolicy::DenyEdits => OfflinePolicy::DenyEdits as i32,
                semantic::OfflinePolicy::LocalBranch => OfflinePolicy::LocalBranch as i32,
            },
        }
    }
}

impl TryFrom<LeaseGrant> for semantic::LeaseGrant {
    type Error = ProtocolError;

    fn try_from(value: LeaseGrant) -> Result<Self, Self::Error> {
        let policy = OfflinePolicy::try_from(value.offline_policy).map_err(|_| {
            ProtocolError::InvalidEnum {
                field: "LeaseGrant.offline_policy",
                value: value.offline_policy,
            }
        })?;
        Ok(Self {
            document_id: semantic::DocumentId::new(value.document_id),
            lease_epoch: semantic::LeaseEpoch::new(value.lease_epoch),
            holder_id: semantic::ClientId::new(value.holder_id),
            offline_policy: match policy {
                OfflinePolicy::DenyEdits => semantic::OfflinePolicy::DenyEdits,
                OfflinePolicy::LocalBranch => semantic::OfflinePolicy::LocalBranch,
            },
        })
    }
}

impl From<&semantic::SessionEvent> for SessionEvent {
    fn from(value: &semantic::SessionEvent) -> Self {
        let origin = match value.origin {
            semantic::EventOrigin::Client(client_id) => EventOrigin {
                kind: EventOriginKind::Client as i32,
                client_id: Some(client_id.get()),
            },
            semantic::EventOrigin::Workspace => EventOrigin {
                kind: EventOriginKind::Workspace as i32,
                client_id: None,
            },
            semantic::EventOrigin::Recovery => EventOrigin {
                kind: EventOriginKind::Recovery as i32,
                client_id: None,
            },
        };
        let payload = match &value.payload {
            semantic::SessionEventPayload::DocumentDelta {
                document_id,
                accepted_revision,
                transactions,
            } => session_event_payload::Value::DocumentDelta(DocumentDelta {
                document_id: document_id.get(),
                accepted_revision: accepted_revision.get(),
                transactions: transactions.iter().map(Transaction::from).collect(),
            }),
            semantic::SessionEventPayload::StateDelta(delta) => {
                session_event_payload::Value::StateDelta(StateDelta::from(delta))
            }
            semantic::SessionEventPayload::LeaseChange(grant) => {
                session_event_payload::Value::LeaseChange(LeaseGrant::from(grant))
            }
            semantic::SessionEventPayload::ExternalChange {
                document_id,
                content_hash,
            } => session_event_payload::Value::ExternalChange(ExternalChange {
                document_id: document_id.get(),
                content_hash: content_hash.to_vec(),
            }),
        };
        Self {
            session_sequence: value.session_sequence.get(),
            origin: Some(origin),
            payload: Some(SessionEventPayload {
                value: Some(payload),
            }),
        }
    }
}

impl TryFrom<SessionEvent> for semantic::SessionEvent {
    type Error = ProtocolError;

    fn try_from(value: SessionEvent) -> Result<Self, Self::Error> {
        let origin = required(value.origin, "SessionEvent.origin")?;
        let origin_kind =
            EventOriginKind::try_from(origin.kind).map_err(|_| ProtocolError::InvalidEnum {
                field: "EventOrigin.kind",
                value: origin.kind,
            })?;
        let origin = match origin_kind {
            EventOriginKind::Client => semantic::EventOrigin::Client(semantic::ClientId::new(
                required(origin.client_id, "EventOrigin.client_id")?,
            )),
            EventOriginKind::Workspace => semantic::EventOrigin::Workspace,
            EventOriginKind::Recovery => semantic::EventOrigin::Recovery,
        };
        let payload = required(value.payload, "SessionEvent.payload")?;
        let payload = match required(payload.value, "SessionEventPayload.value")? {
            session_event_payload::Value::DocumentDelta(delta) => {
                semantic::SessionEventPayload::DocumentDelta {
                    document_id: semantic::DocumentId::new(delta.document_id),
                    accepted_revision: semantic::DocumentRevision::new(delta.accepted_revision),
                    transactions: delta
                        .transactions
                        .into_iter()
                        .map(semantic::Transaction::try_from)
                        .collect::<Result<Vec<_>, _>>()?,
                }
            }
            session_event_payload::Value::StateDelta(delta) => {
                semantic::SessionEventPayload::StateDelta(delta.try_into()?)
            }
            session_event_payload::Value::LeaseChange(grant) => {
                semantic::SessionEventPayload::LeaseChange(grant.try_into()?)
            }
            session_event_payload::Value::ExternalChange(change) => {
                semantic::SessionEventPayload::ExternalChange {
                    document_id: semantic::DocumentId::new(change.document_id),
                    content_hash: hash32(change.content_hash, "ExternalChange.content_hash")?,
                }
            }
        };
        Ok(Self {
            session_sequence: semantic::SessionSequence::new(value.session_sequence),
            origin,
            payload,
        })
    }
}

impl From<&semantic::DocumentFrontier> for DocumentFrontier {
    fn from(value: &semantic::DocumentFrontier) -> Self {
        Self {
            document_id: value.document_id.get(),
            revision: value.revision.get(),
        }
    }
}

impl From<DocumentFrontier> for semantic::DocumentFrontier {
    fn from(value: DocumentFrontier) -> Self {
        Self {
            document_id: semantic::DocumentId::new(value.document_id),
            revision: semantic::DocumentRevision::new(value.revision),
        }
    }
}

impl From<&semantic::Resume> for Resume {
    fn from(value: &semantic::Resume) -> Self {
        Self {
            session_id: value.session_id.get(),
            session_epoch: value.session_epoch.get(),
            last_session_sequence: value.last_session_sequence.get(),
            document_frontiers: value
                .document_frontiers
                .iter()
                .map(DocumentFrontier::from)
                .collect(),
            outstanding_mutation_ids: value
                .outstanding_mutation_ids
                .iter()
                .map(|id| id.get())
                .collect(),
        }
    }
}

impl From<Resume> for semantic::Resume {
    fn from(value: Resume) -> Self {
        Self {
            session_id: semantic::SessionId::new(value.session_id),
            session_epoch: semantic::SessionEpoch::new(value.session_epoch),
            last_session_sequence: semantic::SessionSequence::new(value.last_session_sequence),
            document_frontiers: value
                .document_frontiers
                .into_iter()
                .map(semantic::DocumentFrontier::from)
                .collect(),
            outstanding_mutation_ids: value
                .outstanding_mutation_ids
                .into_iter()
                .map(semantic::MutationId::new)
                .collect(),
        }
    }
}

impl From<&semantic::ResumeResult> for ResumeResult {
    fn from(value: &semantic::ResumeResult) -> Self {
        let value = match value {
            semantic::ResumeResult::Replay { events } => resume_result::Value::Replay(Replay {
                events: events.iter().map(SessionEvent::from).collect(),
            }),
            semantic::ResumeResult::SnapshotRequired {
                new_session_epoch,
                workspace_generation,
                document_heads,
            } => resume_result::Value::SnapshotRequired(SnapshotRequired {
                new_session_epoch: new_session_epoch.get(),
                workspace_generation: workspace_generation.get(),
                document_heads: document_heads.iter().map(DocumentFrontier::from).collect(),
            }),
        };
        Self { value: Some(value) }
    }
}

impl TryFrom<ResumeResult> for semantic::ResumeResult {
    type Error = ProtocolError;

    fn try_from(value: ResumeResult) -> Result<Self, Self::Error> {
        match required(value.value, "ResumeResult.value")? {
            resume_result::Value::Replay(replay) => Ok(Self::Replay {
                events: replay
                    .events
                    .into_iter()
                    .map(semantic::SessionEvent::try_from)
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            resume_result::Value::SnapshotRequired(snapshot) => Ok(Self::SnapshotRequired {
                new_session_epoch: semantic::SessionEpoch::new(snapshot.new_session_epoch),
                workspace_generation: semantic::WorkspaceGeneration::new(
                    snapshot.workspace_generation,
                ),
                document_heads: snapshot
                    .document_heads
                    .into_iter()
                    .map(semantic::DocumentFrontier::from)
                    .collect(),
            }),
        }
    }
}

impl From<&semantic::FileIdentity> for FileIdentity {
    fn from(value: &semantic::FileIdentity) -> Self {
        Self {
            device: value.device,
            file: value.file,
            generation: value.generation,
        }
    }
}

impl From<FileIdentity> for semantic::FileIdentity {
    fn from(value: FileIdentity) -> Self {
        Self {
            device: value.device,
            file: value.file,
            generation: value.generation,
        }
    }
}

impl From<&semantic::SaveRequest> for SaveRequest {
    fn from(value: &semantic::SaveRequest) -> Self {
        Self {
            document_id: value.document_id.get(),
            required_frontier: value.required_frontier.get(),
            expected_file_identity: Some(FileIdentity::from(&value.expected_file_identity)),
            expected_content_hash: value.expected_content_hash.to_vec(),
        }
    }
}

impl TryFrom<SaveRequest> for semantic::SaveRequest {
    type Error = ProtocolError;

    fn try_from(value: SaveRequest) -> Result<Self, Self::Error> {
        Ok(Self {
            document_id: semantic::DocumentId::new(value.document_id),
            required_frontier: semantic::DocumentRevision::new(value.required_frontier),
            expected_file_identity: required(
                value.expected_file_identity,
                "SaveRequest.expected_file_identity",
            )?
            .into(),
            expected_content_hash: hash32(
                value.expected_content_hash,
                "SaveRequest.expected_content_hash",
            )?,
        })
    }
}

impl From<&semantic::Saved> for Saved {
    fn from(value: &semantic::Saved) -> Self {
        Self {
            document_id: value.document_id.get(),
            persisted_frontier: value.persisted_frontier.get(),
            new_file_identity: Some(FileIdentity::from(&value.new_file_identity)),
            new_content_hash: value.new_content_hash.to_vec(),
        }
    }
}

impl TryFrom<Saved> for semantic::Saved {
    type Error = ProtocolError;

    fn try_from(value: Saved) -> Result<Self, Self::Error> {
        Ok(Self {
            document_id: semantic::DocumentId::new(value.document_id),
            persisted_frontier: semantic::DocumentRevision::new(value.persisted_frontier),
            new_file_identity: required(value.new_file_identity, "Saved.new_file_identity")?.into(),
            new_content_hash: hash32(value.new_content_hash, "Saved.new_content_hash")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn semantic_mutation() -> semantic::ClientMutation {
        semantic::ClientMutation {
            mutation_id: semantic::MutationId::new(9),
            client_id: semantic::ClientId::new(4),
            client_sequence: semantic::ClientSequence::new(3),
            state_deltas: vec![
                semantic::StateDelta::Register {
                    name: 'a',
                    text: "copied".into(),
                    linewise: false,
                },
                semantic::StateDelta::SearchDirection { backward: true },
                semantic::StateDelta::MacroRecording {
                    name: 'q',
                    raw_keys: vec![1, 2],
                    lowered_ir: vec![3, 4],
                },
                semantic::StateDelta::JumpList {
                    entries: vec![semantic::DurableJumpEntry {
                        document_id: semantic::DocumentId::new(2),
                        anchor: semantic::Anchor {
                            byte: 7,
                            bias: semantic::Bias::Right,
                        },
                        path_hint: Some("/workspace/main.rs".into()),
                    }],
                    current: Some(0),
                },
            ],
            documents: vec![semantic::DocumentMutation {
                document_id: semantic::DocumentId::new(2),
                lease_epoch: semantic::LeaseEpoch::new(7),
                base_revision: semantic::DocumentRevision::new(5),
                semantic_group_id: semantic::SemanticGroupId::new(8),
                semantic_group_kind: semantic::SemanticGroupKind::UndoOf(
                    semantic::SemanticGroupId::new(6),
                ),
                undo_parent: Some(semantic::SemanticGroupId::new(5)),
                transactions: vec![
                    semantic::Transaction::new(
                        semantic::DocumentRevision::new(5),
                        vec![semantic::Edit::new(1..3, "x")],
                    )
                    .expect("transaction"),
                ],
            }],
        }
    }

    #[test]
    fn mutation_round_trips_through_typed_protobuf_frame() {
        let expected = semantic_mutation();
        let envelope = Envelope::new(
            42,
            envelope::Payload::ClientMutation(ClientMutation::from(&expected)),
        );
        let bytes = encode_frame(&envelope, DEFAULT_MAX_FRAME_BYTES).expect("encode");
        let decoded = decode_frame(&bytes, DEFAULT_MAX_FRAME_BYTES).expect("decode");
        let envelope::Payload::ClientMutation(mutation) = decoded.payload.expect("payload") else {
            panic!("unexpected payload");
        };
        let actual = semantic::ClientMutation::try_from(mutation).expect("semantic conversion");
        assert_eq!(actual, expected);
    }

    #[test]
    fn result_event_resume_and_save_contracts_round_trip() {
        let result = semantic::MutationResult::Durable {
            mutation_id: semantic::MutationId::new(1),
            client_sequence: semantic::ClientSequence::new(2),
            session_sequence: semantic::SessionSequence::new(3),
            documents: vec![semantic::AcceptedDocument {
                document_id: semantic::DocumentId::new(4),
                accepted_revision: semantic::DocumentRevision::new(5),
                canonical_transaction_hash: [6; 32],
            }],
        };
        assert_eq!(
            semantic::MutationResult::try_from(MutationResult::from(&result)).expect("result"),
            result
        );

        let event = semantic::SessionEvent {
            session_sequence: semantic::SessionSequence::new(8),
            origin: semantic::EventOrigin::Workspace,
            payload: semantic::SessionEventPayload::ExternalChange {
                document_id: semantic::DocumentId::new(4),
                content_hash: [9; 32],
            },
        };
        assert_eq!(
            semantic::SessionEvent::try_from(SessionEvent::from(&event)).expect("event"),
            event
        );

        let resume = semantic::ResumeResult::SnapshotRequired {
            new_session_epoch: semantic::SessionEpoch::new(2),
            workspace_generation: semantic::WorkspaceGeneration::new(3),
            document_heads: vec![semantic::DocumentFrontier {
                document_id: semantic::DocumentId::new(4),
                revision: semantic::DocumentRevision::new(5),
            }],
        };
        assert_eq!(
            semantic::ResumeResult::try_from(ResumeResult::from(&resume)).expect("resume"),
            resume
        );

        let saved = semantic::Saved {
            document_id: semantic::DocumentId::new(4),
            persisted_frontier: semantic::DocumentRevision::new(5),
            new_file_identity: semantic::FileIdentity {
                device: 1,
                file: 2,
                generation: 3,
            },
            new_content_hash: [7; 32],
        };
        assert_eq!(
            semantic::Saved::try_from(Saved::from(&saved)).expect("saved"),
            saved
        );
    }

    #[test]
    fn rejects_wrong_major_missing_payload_oversize_and_bad_hashes() {
        let mut envelope = Envelope::new(
            1,
            envelope::Payload::Hello(Hello {
                major: 1,
                minor: 0,
                capabilities: Vec::new(),
                max_frame_bytes: 1024,
            }),
        );
        envelope.protocol_version = 2 << 16;
        assert!(matches!(
            envelope.validate(),
            Err(ProtocolError::UnsupportedVersion { .. })
        ));
        envelope.protocol_version = PROTOCOL_VERSION;
        envelope.payload = None;
        assert!(matches!(
            envelope.validate(),
            Err(ProtocolError::MissingField(_))
        ));
        let envelope = Envelope::new(
            1,
            envelope::Payload::Hello(Hello {
                major: 1,
                minor: 0,
                capabilities: vec!["x".repeat(100)],
                max_frame_bytes: 1024,
            }),
        );
        assert!(matches!(
            encode_frame(&envelope, 8),
            Err(ProtocolError::FrameTooLarge { .. })
        ));
        let invalid = MutationResult {
            value: Some(mutation_result::Value::Durable(Durable {
                mutation_id: 1,
                client_sequence: 1,
                session_sequence: 1,
                documents: vec![AcceptedDocument {
                    document_id: 1,
                    accepted_revision: 1,
                    canonical_transaction_hash: vec![0; 31],
                }],
            })),
        };
        assert!(matches!(
            semantic::MutationResult::try_from(invalid),
            Err(ProtocolError::HashLength { .. })
        ));
    }

    #[test]
    fn stream_framing_handles_multiple_messages_clean_eof_and_limits() {
        let first = Envelope::new(
            1,
            envelope::Payload::Hello(Hello {
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
                capabilities: vec!["mutation.v1".to_owned()],
                max_frame_bytes: 1024,
            }),
        );
        let second = Envelope::new(
            2,
            envelope::Payload::ClientMutation(ClientMutation::from(&semantic_mutation())),
        );
        let mut bytes = Vec::new();
        write_envelope(&mut bytes, &first, DEFAULT_MAX_FRAME_BYTES).expect("first frame");
        write_envelope(&mut bytes, &second, DEFAULT_MAX_FRAME_BYTES).expect("second frame");
        let mut cursor = std::io::Cursor::new(bytes);
        assert_eq!(
            read_envelope(&mut cursor, DEFAULT_MAX_FRAME_BYTES)
                .expect("read first")
                .expect("first"),
            first
        );
        assert_eq!(
            read_envelope(&mut cursor, DEFAULT_MAX_FRAME_BYTES)
                .expect("read second")
                .expect("second"),
            second
        );
        assert!(
            read_envelope(&mut cursor, DEFAULT_MAX_FRAME_BYTES)
                .expect("clean eof")
                .is_none()
        );

        let mut oversized = std::io::Cursor::new(vec![0x81, 0x01]);
        assert!(matches!(
            read_envelope(&mut oversized, 8),
            Err(TransportError::Protocol(
                ProtocolError::FrameTooLarge { .. }
            ))
        ));
    }
}
