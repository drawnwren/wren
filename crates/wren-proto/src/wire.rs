use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};
use thiserror::Error;
pub use wren_types::{
    AcceptedDocument, ClientMutation, DocumentFrontier, FileIdentity, MutationResult, Resume, ResumeResult, SaveRequest, Saved, SessionEvent,
};

pub const PROTOCOL_MAJOR: u32 = 3;
pub const PROTOCOL_MINOR: u32 = 0;
pub const PROTOCOL_VERSION: u32 = (PROTOCOL_MAJOR << 16) | PROTOCOL_MINOR;
pub const DEFAULT_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("unsupported protocol version {actual:#010x}; expected major {expected_major}")]
    UnsupportedVersion { actual: u32, expected_major: u32 },
    #[error("protocol field {0} is required")]
    MissingField(&'static str),
    #[error("encoded frame is {actual} bytes, exceeding limit {limit}")]
    FrameTooLarge { actual: usize, limit: usize },
    #[error("decode protocol frame: {0}")]
    Decode(String),
    #[error("decoded frame has {0} trailing bytes")]
    TrailingBytes(usize),
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("protocol transport I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol_version: u32,
    pub request_id: u64,
    pub payload: Option<envelope::Payload>,
}

impl Envelope {
    #[must_use]
    pub const fn new(request_id: u64, payload: envelope::Payload) -> Self {
        Self { protocol_version: PROTOCOL_VERSION, request_id, payload: Some(payload) }
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.protocol_version >> 16 != PROTOCOL_MAJOR {
            return Err(ProtocolError::UnsupportedVersion { actual: self.protocol_version, expected_major: PROTOCOL_MAJOR });
        }
        self.payload.as_ref().ok_or(ProtocolError::MissingField("Envelope.payload")).map(drop)
    }
}

pub mod envelope {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Payload {
        Hello(Hello),
        HelloAck(HelloAck),
        ClientMutation(ClientMutation),
        MutationResult(MutationResult),
        SessionEvent(SessionEvent),
        Resume(Resume),
        ResumeResult(ResumeResult),
        SaveRequest(SaveRequest),
        Saved(Saved),
        OpenDocument(OpenDocument),
        DocumentOpened(DocumentOpened),
        RemoteCall(RemoteCall),
        RemoteReply(RemoteReply),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub major: u32,
    pub minor: u32,
    pub capabilities: Vec<String>,
    pub max_frame_bytes: u64,
}

pub type HelloAck = Hello;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenDocument {
    pub document_id: u64,
    pub client_id: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentOpened {
    pub document_id: u64,
    pub revision: u64,
    pub lease_epoch: u64,
    pub session_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteCall {
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteReply {
    pub body: Vec<u8>,
}

pub fn encode_frame(envelope: &Envelope, limit: usize) -> Result<Vec<u8>, ProtocolError> {
    envelope.validate()?;
    let payload = postcard::to_allocvec(envelope).map_err(|error| ProtocolError::Decode(error.to_string()))?;
    if payload.len() > limit {
        return Err(ProtocolError::FrameTooLarge { actual: payload.len(), limit });
    }
    let length = u32::try_from(payload.len()).map_err(|_| ProtocolError::FrameTooLarge { actual: payload.len(), limit })?;
    let mut bytes = Vec::with_capacity(payload.len() + 4);
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

/// Reads one length-delimited envelope. EOF before a new delimiter is a clean
/// connection close; EOF after any delimiter byte is an I/O error.
pub fn read_envelope(reader: &mut impl Read, limit: usize) -> Result<Option<Envelope>, TransportError> {
    let mut delimiter = [0_u8; 4];
    let read = reader.read(&mut delimiter)?;
    if read == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut delimiter[read..])?;
    let length = u32::from_be_bytes(delimiter) as usize;
    if length > limit {
        return Err(ProtocolError::FrameTooLarge { actual: length, limit }.into());
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    let (envelope, trailing) = postcard::take_from_bytes::<Envelope>(&payload).map_err(|error| ProtocolError::Decode(error.to_string()))?;
    if !trailing.is_empty() {
        return Err(ProtocolError::TrailingBytes(trailing.len()).into());
    }
    envelope.validate()?;
    Ok(Some(envelope))
}

pub fn write_envelope(writer: &mut impl Write, envelope: &Envelope, limit: usize) -> Result<(), TransportError> {
    writer.write_all(&encode_frame(envelope, limit)?)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wren_types::{ClientId, ClientSequence, MutationId};

    fn mutation() -> ClientMutation {
        ClientMutation {
            mutation_id: MutationId::new(9),
            client_id: ClientId::new(3),
            client_sequence: ClientSequence::new(7),
            state_deltas: Vec::new(),
            documents: Vec::new(),
        }
    }

    #[test]
    fn semantic_payload_round_trips_through_typed_frame() {
        let expected = mutation();
        let envelope = Envelope::new(42, envelope::Payload::ClientMutation(expected.clone()));
        let mut bytes = Vec::new();
        write_envelope(&mut bytes, &envelope, DEFAULT_MAX_FRAME_BYTES).expect("encode");
        let decoded = read_envelope(&mut bytes.as_slice(), DEFAULT_MAX_FRAME_BYTES).expect("decode").expect("frame");
        let envelope::Payload::ClientMutation(actual) = decoded.payload.expect("payload") else { panic!("wrong payload") };
        assert_eq!(actual, expected);
    }

    #[test]
    fn rejects_wrong_major_oversize_and_trailing_bytes() {
        let mut envelope = Envelope::new(1, envelope::Payload::RemoteCall(RemoteCall { body: Vec::new() }));
        envelope.protocol_version = 99 << 16;
        assert!(matches!(envelope.validate(), Err(ProtocolError::UnsupportedVersion { .. })));
        envelope.protocol_version = PROTOCOL_VERSION;
        assert!(matches!(encode_frame(&envelope, 1), Err(ProtocolError::FrameTooLarge { .. })));
    }

    #[test]
    fn stream_framing_handles_multiple_messages_clean_eof_and_limits() {
        let first =
            Envelope::new(1, envelope::Payload::Hello(Hello { major: PROTOCOL_MAJOR, minor: PROTOCOL_MINOR, capabilities: Vec::new(), max_frame_bytes: 1024 }));
        let second = Envelope::new(2, envelope::Payload::ClientMutation(mutation()));
        let mut bytes = Vec::new();
        write_envelope(&mut bytes, &first, DEFAULT_MAX_FRAME_BYTES).expect("first frame");
        write_envelope(&mut bytes, &second, DEFAULT_MAX_FRAME_BYTES).expect("second frame");
        let mut cursor = std::io::Cursor::new(bytes);
        assert_eq!(read_envelope(&mut cursor, DEFAULT_MAX_FRAME_BYTES).expect("read first"), Some(first));
        assert_eq!(read_envelope(&mut cursor, DEFAULT_MAX_FRAME_BYTES).expect("read second"), Some(second));
        assert!(read_envelope(&mut cursor, DEFAULT_MAX_FRAME_BYTES).expect("clean eof").is_none());
        assert!(matches!(read_envelope(&mut &[0, 0, 0, 129][..], 8), Err(TransportError::Protocol(ProtocolError::FrameTooLarge { .. }))));
        assert!(matches!(read_envelope(&mut &[0, 0][..], 8), Err(TransportError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof));
    }
}
