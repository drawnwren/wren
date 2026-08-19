use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple", target_os = "netbsd", target_os = "openbsd"))]
use std::os::unix::fs::OpenOptionsExt as _;

use serde::{Deserialize, Serialize};
use wren_types::{
    ClientMutation, DocumentFrontier, LeaseGrant, MutationResult, SessionEpoch, SessionEvent, SessionId, SessionSequence, StateDelta, WorkspaceGeneration,
};

const MAGIC: &[u8; 8] = b"WRENSES1";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegisteredDocument {
    pub frontier: DocumentFrontier,
    pub text: String,
    pub lease: LeaseGrant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum JournalEntry {
    Initialized { session_id: SessionId, session_epoch: SessionEpoch, workspace_generation: WorkspaceGeneration },
    DocumentRegistered(RegisteredDocument),
    MutationCommitted { mutation: ClientMutation, durable: MutationResult, events: Vec<SessionEvent> },
    LeaseChanged { grant: LeaseGrant, event: SessionEvent },
    StateCheckpointed { client_id: wren_types::ClientId, through_client_sequence: wren_types::ClientSequence, state: Vec<StateDelta> },
    ContinuityBroken { new_session_epoch: SessionEpoch, workspace_generation: WorkspaceGeneration, retained_after: SessionSequence },
}

pub type SessionJournalError = crate::record::DurableRecordError;

#[derive(Debug, Clone)]
pub struct SessionJournal {
    path: PathBuf,
    writer: Arc<Mutex<Option<File>>>,
}

impl SessionJournal {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), writer: Arc::new(Mutex::new(None)) }
    }

    #[must_use]
    pub fn in_directory(directory: impl AsRef<Path>) -> Self {
        Self::new(directory.as_ref().join("session.journal"))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn append(&self, entry: &JournalEntry) -> Result<(), SessionJournalError> {
        self.append_value(entry)
    }

    pub(crate) fn append_many(&self, entries: &[JournalEntry]) -> Result<(), SessionJournalError> {
        self.append_values(entries)
    }

    fn append_value(&self, entry: &impl Serialize) -> Result<(), SessionJournalError> {
        self.append_values(std::slice::from_ref(entry))
    }

    fn append_values<T: Serialize>(&self, entries: &[T]) -> Result<(), SessionJournalError> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut writer = self.writer.lock().map_err(|_| self.io(io::Error::other("session journal writer lock poisoned")))?;
        if writer.is_none() {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| self.io(error))?;
            }
            let mut options = OpenOptions::new();
            options.create(true).append(true);
            #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple", target_os = "netbsd", target_os = "openbsd"))]
            options.custom_flags(nix::fcntl::OFlag::O_DSYNC.bits());
            *writer = Some(options.open(&self.path).map_err(|error| self.io(error))?);
        }
        let file = writer.as_mut().ok_or_else(|| self.io(io::Error::other("session journal writer unavailable")))?;
        self.records().write_many(file, entries)?;
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple", target_os = "netbsd", target_os = "openbsd"))]
        {
            // O_DSYNC makes completion of the record write itself the durable
            // frontier; issuing a second sync syscall adds latency but no
            // stronger guarantee.
            Ok(())
        }
        #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple", target_os = "netbsd", target_os = "openbsd")))]
        {
            file.sync_data().map_err(|error| self.io(error))
        }
    }

    pub(crate) fn recover(&self) -> Result<Vec<JournalEntry>, SessionJournalError> {
        self.records().recover()
    }

    fn io(&self, source: io::Error) -> SessionJournalError {
        self.records().error(source)
    }

    fn records(&self) -> crate::record::RecordStore<'_> {
        crate::record::RecordStore::new("session journal", &self.path, MAGIC)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write as _;

    use tempfile::tempdir;

    use super::*;

    fn initialized() -> JournalEntry {
        JournalEntry::Initialized { session_id: SessionId::new(1), session_epoch: SessionEpoch::new(1), workspace_generation: WorkspaceGeneration::new(1) }
    }

    #[test]
    fn recovers_complete_records_and_ignores_a_torn_tail() {
        let directory = tempdir().expect("temporary directory");
        let journal = SessionJournal::in_directory(directory.path());
        journal.append(&initialized()).expect("append");
        OpenOptions::new().append(true).open(journal.path()).expect("open journal").write_all(b"WRENSES1\x20").expect("append torn record");
        assert_eq!(journal.recover().expect("recover"), vec![initialized()]);
    }

    #[test]
    fn checksum_corruption_is_never_silently_replayed() {
        let directory = tempdir().expect("temporary directory");
        let journal = SessionJournal::in_directory(directory.path());
        journal.append(&initialized()).expect("append");
        let mut bytes = fs::read(journal.path()).expect("read journal");
        *bytes.last_mut().expect("payload") ^= 1;
        fs::write(journal.path(), bytes).expect("corrupt journal");
        assert!(matches!(journal.recover(), Err(SessionJournalError::Checksum { .. })));
    }
}
