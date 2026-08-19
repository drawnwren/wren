use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 8] = b"WRENWAL1";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredState {
    pub base_hash: [u8; 32],
    pub revision: u64,
    pub text: String,
    pub cursor: usize,
}

pub type WalError = crate::record::DurableRecordError;

#[derive(Debug, Clone)]
pub struct LocalWal {
    path: PathBuf,
}

impl LocalWal {
    #[must_use]
    pub fn in_directory(directory: impl AsRef<Path>, document_key: &[u8]) -> Self {
        let name = format!("{}.wal", blake3::hash(document_key).to_hex());
        Self { path: directory.as_ref().join(name) }
    }

    pub fn for_document(path: &Path) -> Result<Self, WalError> {
        let state = state_directory().ok_or_else(|| WalError::Malformed {
            store: "WAL",
            path: path.to_path_buf(),
            offset: 0,
            reason: "neither XDG_STATE_HOME nor HOME is set".into(),
        })?;
        Ok(Self::in_directory(state.join("wren/outbox/local"), path.as_os_str().as_encoded_bytes()))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, state: &RecoveredState) -> Result<(), WalError> {
        self.records().append(state)
    }

    pub fn recover_latest(&self) -> Result<Option<RecoveredState>, WalError> {
        self.records().recover().map(|records: Vec<RecoveredState>| records.into_iter().next_back())
    }

    pub fn clear(&self) -> Result<(), WalError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(self.io(error)),
        }
    }

    fn io(&self, source: io::Error) -> WalError {
        self.records().error(source)
    }

    fn records(&self) -> crate::record::RecordStore<'_> {
        crate::record::RecordStore::new("WAL", &self.path, MAGIC)
    }
}

fn state_directory() -> Option<PathBuf> {
    env::var_os("XDG_STATE_HOME").map(PathBuf::from).or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write as _;

    use tempfile::tempdir;

    use super::*;

    fn state(revision: u64, text: &str) -> RecoveredState {
        RecoveredState { base_hash: *blake3::hash(b"base").as_bytes(), revision, text: text.to_owned(), cursor: text.len() }
    }

    #[test]
    fn recovers_last_complete_synced_record() {
        let directory = tempdir().expect("temporary directory");
        let wal = LocalWal::in_directory(directory.path(), b"document");
        wal.append(&state(1, "one")).expect("first append");
        wal.append(&state(2, "two")).expect("second append");
        assert_eq!(wal.recover_latest().expect("recover"), Some(state(2, "two")));
    }

    #[test]
    fn ignores_a_torn_trailing_record_and_clear_is_idempotent() {
        let directory = tempdir().expect("temporary directory");
        let wal = LocalWal::in_directory(directory.path(), b"document");
        wal.append(&state(1, "safe")).expect("append");
        OpenOptions::new().append(true).open(wal.path()).expect("open").write_all(b"WRENWAL1\x40").expect("torn write");
        assert_eq!(wal.recover_latest().expect("recover"), Some(state(1, "safe")));
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
        assert!(matches!(wal.recover_latest(), Err(WalError::Checksum { .. })));
    }
}
