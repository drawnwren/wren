use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAGIC: &[u8; 8] = b"WRENWAL1";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredState {
    pub base_hash: [u8; 32],
    pub revision: u64,
    pub text: String,
    pub cursor: usize,
}

impl RecoveredState {
    #[must_use]
    pub fn matches_base(&self, bytes: &[u8]) -> bool {
        self.base_hash == *blake3::hash(bytes).as_bytes()
    }
}

#[derive(Debug, Error)]
pub enum WalError {
    #[error("WAL operation for {path} failed: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("WAL record in {path} has an invalid checksum")]
    Checksum { path: PathBuf },
    #[error("WAL record in {path} is malformed: {reason}")]
    Malformed { path: PathBuf, reason: String },
    #[error("WAL record serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct LocalWal {
    path: PathBuf,
}

impl LocalWal {
    #[must_use]
    pub fn in_directory(directory: impl AsRef<Path>, document_key: &[u8]) -> Self {
        let name = format!("{}.wal", blake3::hash(document_key).to_hex());
        Self {
            path: directory.as_ref().join(name),
        }
    }

    pub fn for_document(path: &Path) -> Result<Self, WalError> {
        let state = state_directory().ok_or_else(|| WalError::Malformed {
            path: path.to_path_buf(),
            reason: "neither XDG_STATE_HOME nor HOME is set".to_owned(),
        })?;
        Ok(Self::in_directory(
            state.join("wren/outbox/local"),
            path.as_os_str().as_encoded_bytes(),
        ))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, state: &RecoveredState) -> Result<(), WalError> {
        crate::record::append(&self.path, MAGIC, state).map_err(|error| self.record(error))
    }

    pub fn recover_latest(&self) -> Result<Option<RecoveredState>, WalError> {
        crate::record::recover(&self.path, MAGIC)
            .map(|records: Vec<RecoveredState>| records.into_iter().next_back())
            .map_err(|error| self.record(error))
    }

    pub fn clear(&self) -> Result<(), WalError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(self.io(error)),
        }
    }

    fn io(&self, source: io::Error) -> WalError {
        WalError::Io {
            path: self.path.clone(),
            source,
        }
    }

    fn record(&self, error: crate::record::RecordError) -> WalError {
        match error {
            crate::record::RecordError::Io(source) => self.io(source),
            crate::record::RecordError::Checksum { .. } => WalError::Checksum {
                path: self.path.clone(),
            },
            crate::record::RecordError::Malformed { offset, reason } => WalError::Malformed {
                path: self.path.clone(),
                reason: format!("{reason} at byte {offset}"),
            },
            crate::record::RecordError::Serialization(error) => WalError::Serialization(error),
        }
    }
}

fn state_directory() -> Option<PathBuf> {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write as _;

    use tempfile::tempdir;

    use super::*;

    fn state(revision: u64, text: &str) -> RecoveredState {
        RecoveredState {
            base_hash: *blake3::hash(b"base").as_bytes(),
            revision,
            text: text.to_owned(),
            cursor: text.len(),
        }
    }

    #[test]
    fn recovers_last_complete_synced_record() {
        let directory = tempdir().expect("temporary directory");
        let wal = LocalWal::in_directory(directory.path(), b"document");
        wal.append(&state(1, "one")).expect("first append");
        wal.append(&state(2, "two")).expect("second append");
        assert_eq!(
            wal.recover_latest().expect("recover"),
            Some(state(2, "two"))
        );
    }

    #[test]
    fn ignores_a_torn_trailing_record_and_clear_is_idempotent() {
        let directory = tempdir().expect("temporary directory");
        let wal = LocalWal::in_directory(directory.path(), b"document");
        wal.append(&state(1, "safe")).expect("append");
        OpenOptions::new()
            .append(true)
            .open(wal.path())
            .expect("open")
            .write_all(b"WRENWAL1\x40")
            .expect("torn write");
        assert_eq!(
            wal.recover_latest().expect("recover"),
            Some(state(1, "safe"))
        );
        wal.clear().expect("clear");
        wal.clear().expect("clear again");
        assert_eq!(wal.recover_latest().expect("empty"), None);
    }

    #[test]
    fn detects_checksum_corruption() {
        let directory = tempdir().expect("temporary directory");
        let wal = LocalWal::in_directory(directory.path(), b"document");
        wal.append(&state(1, "safe")).expect("append");
        let mut bytes = fs::read(wal.path()).expect("read");
        let last = bytes.last_mut().expect("payload byte");
        *last ^= 1;
        fs::write(wal.path(), bytes).expect("corrupt");
        assert!(matches!(
            wal.recover_latest(),
            Err(WalError::Checksum { .. })
        ));
    }
}
