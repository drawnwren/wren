use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

const CHECKSUM_LEN: usize = 32;
const LENGTH_LEN: usize = size_of::<u64>();

#[derive(Debug)]
pub(crate) enum RecordError {
    Io(io::Error),
    Checksum { offset: usize },
    Malformed { offset: usize, reason: Box<str> },
    Serialization(serde_json::Error),
}

pub(crate) fn append<T: Serialize>(
    path: &Path,
    magic: &[u8; 8],
    value: &T,
) -> Result<(), RecordError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(RecordError::Io)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(RecordError::Io)?;
    write(&mut file, magic, value)?;
    file.sync_data().map_err(RecordError::Io)
}

pub(crate) fn write<T: Serialize>(
    writer: &mut impl Write,
    magic: &[u8; 8],
    value: &T,
) -> Result<(), RecordError> {
    let payload = serde_json::to_vec(value).map_err(RecordError::Serialization)?;
    let length = u64::try_from(payload.len()).map_err(|_| RecordError::Malformed {
        offset: 0,
        reason: "record length exceeds u64".into(),
    })?;
    writer
        .write_all(magic)
        .and_then(|()| writer.write_all(&length.to_le_bytes()))
        .and_then(|()| writer.write_all(blake3::hash(&payload).as_bytes()))
        .and_then(|()| writer.write_all(&payload))
        .map_err(RecordError::Io)
}

pub(crate) fn recover<T: DeserializeOwned>(
    path: &Path,
    magic: &[u8; 8],
) -> Result<Vec<T>, RecordError> {
    let mut bytes = Vec::new();
    match File::open(path) {
        Ok(mut file) => file.read_to_end(&mut bytes).map_err(RecordError::Io)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(RecordError::Io(error)),
    };
    decode(&bytes, magic)
}

fn decode<T: DeserializeOwned>(bytes: &[u8], magic: &[u8; 8]) -> Result<Vec<T>, RecordError> {
    let header_len = magic.len() + LENGTH_LEN + CHECKSUM_LEN;
    let mut cursor = 0;
    let mut records = Vec::new();
    while bytes.len().saturating_sub(cursor) >= header_len {
        if bytes.get(cursor..cursor + magic.len()) != Some(magic) {
            return Err(RecordError::Malformed {
                offset: cursor,
                reason: "bad record magic".into(),
            });
        }
        let length_start = cursor + magic.len();
        let length_end = length_start + LENGTH_LEN;
        let length_bytes: [u8; LENGTH_LEN] =
            bytes[length_start..length_end]
                .try_into()
                .map_err(|_| RecordError::Malformed {
                    offset: cursor,
                    reason: "invalid length field".into(),
                })?;
        let length = usize::try_from(u64::from_le_bytes(length_bytes)).map_err(|_| {
            RecordError::Malformed {
                offset: cursor,
                reason: "record length does not fit this address space".into(),
            }
        })?;
        let checksum_start = length_end;
        let payload_start = checksum_start + CHECKSUM_LEN;
        let payload_end =
            payload_start
                .checked_add(length)
                .ok_or_else(|| RecordError::Malformed {
                    offset: cursor,
                    reason: "record length overflow".into(),
                })?;
        if payload_end > bytes.len() {
            break;
        }
        let payload = &bytes[payload_start..payload_end];
        if blake3::hash(payload).as_bytes() != &bytes[checksum_start..payload_start] {
            return Err(RecordError::Checksum { offset: cursor });
        }
        records.push(serde_json::from_slice(payload).map_err(RecordError::Serialization)?);
        cursor = payload_end;
    }
    Ok(records)
}
