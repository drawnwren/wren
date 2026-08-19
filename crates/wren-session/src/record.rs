use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

const CHECKSUM_LEN: usize = 32;
const LENGTH_LEN: usize = size_of::<u64>();

#[derive(Debug, Error)]
pub enum DurableRecordError {
    #[error("{store} operation for {path} failed: {source}")]
    Io {
        store: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{store} record in {path} has an invalid checksum at byte {offset}")]
    Checksum { store: &'static str, path: PathBuf, offset: usize },
    #[error("{store} record in {path} is malformed at byte {offset}: {reason}")]
    Malformed { store: &'static str, path: PathBuf, offset: usize, reason: Box<str> },
    #[error("{store} record serialization failed: {source}")]
    Serialization {
        store: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct RecordStore<'a> {
    store: &'static str,
    path: &'a Path,
    magic: &'static [u8; 8],
}

impl<'a> RecordStore<'a> {
    pub(crate) const fn new(store: &'static str, path: &'a Path, magic: &'static [u8; 8]) -> Self {
        Self { store, path, magic }
    }

    pub(crate) fn error(self, source: io::Error) -> DurableRecordError {
        DurableRecordError::Io { store: self.store, path: self.path.to_path_buf(), source }
    }

    pub(crate) fn malformed(self, reason: impl Into<Box<str>>) -> DurableRecordError {
        self.malformed_at(0, reason)
    }

    pub(crate) fn append<T: Serialize>(self, value: &T) -> Result<(), DurableRecordError> {
        self.append_many(std::slice::from_ref(value))
    }

    pub(crate) fn append_many<T: Serialize>(self, values: &[T]) -> Result<(), DurableRecordError> {
        if values.is_empty() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| self.error(error))?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(self.path).map_err(|error| self.error(error))?;
        self.write_many(&mut file, values)?;
        file.sync_data().map_err(|error| self.error(error))
    }

    pub(crate) fn write<T: Serialize>(self, writer: &mut impl Write, value: &T) -> Result<(), DurableRecordError> {
        self.write_many(writer, std::slice::from_ref(value))
    }

    /// Serializes independently recoverable records into one write. The caller
    /// chooses the durability frontier after this returns.
    pub(crate) fn write_many<T: Serialize>(self, writer: &mut impl Write, values: &[T]) -> Result<(), DurableRecordError> {
        let mut records = Vec::new();
        for value in values {
            let payload = serde_json::to_vec(value).map_err(|source| DurableRecordError::Serialization { store: self.store, source })?;
            let length = u64::try_from(payload.len()).map_err(|_| self.malformed("record length exceeds u64"))?;
            records.reserve(self.magic.len() + LENGTH_LEN + CHECKSUM_LEN + payload.len());
            records.extend_from_slice(self.magic);
            records.extend_from_slice(&length.to_le_bytes());
            records.extend_from_slice(blake3::hash(&payload).as_bytes());
            records.extend_from_slice(&payload);
        }
        writer.write_all(&records).map_err(|error| self.error(error))
    }

    pub(crate) fn recover<T: DeserializeOwned>(self) -> Result<Vec<T>, DurableRecordError> {
        let mut bytes = Vec::new();
        match File::open(self.path) {
            Ok(mut file) => file.read_to_end(&mut bytes).map_err(|error| self.error(error))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(self.error(error)),
        };
        self.decode(&bytes)
    }

    fn decode<T: DeserializeOwned>(self, bytes: &[u8]) -> Result<Vec<T>, DurableRecordError> {
        let header_len = self.magic.len() + LENGTH_LEN + CHECKSUM_LEN;
        let mut cursor = 0;
        let mut records = Vec::new();
        while bytes.len().saturating_sub(cursor) >= header_len {
            if bytes.get(cursor..cursor + self.magic.len()) != Some(self.magic) {
                return Err(self.malformed_at(cursor, "bad record magic"));
            }
            let length_start = cursor + self.magic.len();
            let length_end = length_start + LENGTH_LEN;
            let length_bytes: [u8; LENGTH_LEN] = bytes[length_start..length_end].try_into().map_err(|_| self.malformed_at(cursor, "invalid length field"))?;
            let length =
                usize::try_from(u64::from_le_bytes(length_bytes)).map_err(|_| self.malformed_at(cursor, "record length does not fit this address space"))?;
            let checksum_start = length_end;
            let payload_start = checksum_start + CHECKSUM_LEN;
            let payload_end = payload_start.checked_add(length).ok_or_else(|| self.malformed_at(cursor, "record length overflow"))?;
            if payload_end > bytes.len() {
                break;
            }
            let payload = &bytes[payload_start..payload_end];
            if blake3::hash(payload).as_bytes() != &bytes[checksum_start..payload_start] {
                return Err(DurableRecordError::Checksum { store: self.store, path: self.path.to_path_buf(), offset: cursor });
            }
            records.push(serde_json::from_slice(payload).map_err(|source| DurableRecordError::Serialization { store: self.store, source })?);
            cursor = payload_end;
        }
        Ok(records)
    }

    fn malformed_at(self, offset: usize, reason: impl Into<Box<str>>) -> DurableRecordError {
        DurableRecordError::Malformed { store: self.store, path: self.path.to_path_buf(), offset, reason: reason.into() }
    }
}
