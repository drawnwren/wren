use std::path::{Path, PathBuf};

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

#[must_use]
#[derive(Debug, Clone)]
pub struct SessionJournal {
    records: crate::record::RecordStore,
}

impl SessionJournal {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { records: crate::record::RecordStore::new("session journal", path, MAGIC) }
    }

    pub fn in_directory(directory: impl AsRef<Path>) -> Self {
        Self::new(directory.as_ref().join("session.journal"))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.records.path()
    }

    pub(crate) fn append(&self, entry: &JournalEntry) -> Result<(), SessionJournalError> {
        self.records.append(entry)
    }

    pub(crate) fn append_many(&self, entries: &[JournalEntry]) -> Result<(), SessionJournalError> {
        self.records.append_many(entries)
    }

    pub(crate) fn recover(&self) -> Result<Vec<JournalEntry>, SessionJournalError> {
        self.records.recover()
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
