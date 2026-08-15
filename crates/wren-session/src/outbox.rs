use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;
use wren_types::{ClientMutation, MutationId, MutationResult};

const MAGIC: &[u8; 8] = b"WRENOUT1";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum OutboxRecord {
    Mutation(ClientMutation),
    Durable(MutationId),
}

#[derive(Debug, Error)]
pub enum OutboxError {
    #[error("client outbox operation for {path} failed: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("client outbox record in {path} has an invalid checksum at byte {offset}")]
    Checksum { path: PathBuf, offset: usize },
    #[error("client outbox record in {path} is malformed at byte {offset}: {reason}")]
    Malformed {
        path: PathBuf,
        offset: usize,
        reason: String,
    },
    #[error("client outbox serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("mutation ID {mutation_id:?} was reused with different contents in the outbox")]
    MutationIdCollision { mutation_id: MutationId },
}

#[derive(Debug, Clone)]
pub struct MutationOutbox {
    path: PathBuf,
}

impl MutationOutbox {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn in_directory(directory: impl AsRef<Path>) -> Self {
        Self::new(directory.as_ref().join("mutations.wal"))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, mutation: &ClientMutation) -> Result<(), OutboxError> {
        let outstanding = self.outstanding()?;
        if let Some(existing) = outstanding
            .iter()
            .find(|existing| existing.mutation_id == mutation.mutation_id)
        {
            if existing != mutation {
                return Err(OutboxError::MutationIdCollision {
                    mutation_id: mutation.mutation_id,
                });
            }
            return Ok(());
        }
        self.append_record(&OutboxRecord::Mutation(mutation.clone()))
    }

    /// Returns true only when a durable acknowledgement compacted a known
    /// mutation. `Received` and rejection results never remove client data.
    pub fn observe_result(&self, result: &MutationResult) -> Result<bool, OutboxError> {
        let MutationResult::Durable { mutation_id, .. } = result else {
            return Ok(false);
        };
        if !self
            .outstanding()?
            .iter()
            .any(|mutation| mutation.mutation_id == *mutation_id)
        {
            return Ok(false);
        }
        self.append_record(&OutboxRecord::Durable(*mutation_id))?;
        self.compact()?;
        Ok(true)
    }

    pub fn outstanding(&self) -> Result<Vec<ClientMutation>, OutboxError> {
        let records = self.recover_records()?;
        let mut positions: HashMap<MutationId, usize> = HashMap::new();
        let mut mutations: Vec<Option<ClientMutation>> = Vec::new();
        for record in records {
            match record {
                OutboxRecord::Mutation(mutation) => {
                    if let Some(index) = positions.get(&mutation.mutation_id).copied() {
                        let existing =
                            mutations[index]
                                .as_ref()
                                .ok_or(OutboxError::MutationIdCollision {
                                    mutation_id: mutation.mutation_id,
                                })?;
                        if existing != &mutation {
                            return Err(OutboxError::MutationIdCollision {
                                mutation_id: mutation.mutation_id,
                            });
                        }
                    } else {
                        positions.insert(mutation.mutation_id, mutations.len());
                        mutations.push(Some(mutation));
                    }
                }
                OutboxRecord::Durable(mutation_id) => {
                    if let Some(index) = positions.remove(&mutation_id) {
                        mutations[index] = None;
                    }
                }
            }
        }
        Ok(mutations.into_iter().flatten().collect())
    }

    pub fn compact(&self) -> Result<(), OutboxError> {
        let outstanding = self.outstanding()?;
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| self.io(source))?;
        let mut temporary = NamedTempFile::new_in(parent).map_err(|source| self.io(source))?;
        for mutation in outstanding {
            crate::record::write(
                temporary.as_file_mut(),
                MAGIC,
                &OutboxRecord::Mutation(mutation),
            )
            .map_err(|error| self.record(error))?;
        }
        temporary
            .as_file_mut()
            .flush()
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|source| self.io(source))?;
        temporary
            .persist(&self.path)
            .map_err(|error| self.io(error.error))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| self.io(source))?;
        Ok(())
    }

    fn append_record(&self, record: &OutboxRecord) -> Result<(), OutboxError> {
        crate::record::append(&self.path, MAGIC, record).map_err(|error| self.record(error))
    }

    fn recover_records(&self) -> Result<Vec<OutboxRecord>, OutboxError> {
        crate::record::recover(&self.path, MAGIC).map_err(|error| self.record(error))
    }

    fn io(&self, source: io::Error) -> OutboxError {
        OutboxError::Io {
            path: self.path.clone(),
            source,
        }
    }

    fn record(&self, error: crate::record::RecordError) -> OutboxError {
        match error {
            crate::record::RecordError::Io(source) => self.io(source),
            crate::record::RecordError::Checksum { offset } => OutboxError::Checksum {
                path: self.path.clone(),
                offset,
            },
            crate::record::RecordError::Malformed { offset, reason } => OutboxError::Malformed {
                path: self.path.clone(),
                offset,
                reason: reason.into(),
            },
            crate::record::RecordError::Serialization(error) => OutboxError::Serialization(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write as _;

    use tempfile::tempdir;
    use wren_types::{
        AcceptedDocument, ClientId, ClientSequence, DocumentId, DocumentMutation, DocumentRevision,
        Edit, LeaseEpoch, SemanticGroupId, SemanticGroupKind, SessionId, SessionSequence,
        StateDelta, Transaction,
    };

    use crate::{MutationSubmission, SessionAuthority, SessionJournal};

    use super::*;

    fn mutation() -> ClientMutation {
        ClientMutation {
            mutation_id: MutationId::new(10),
            client_id: ClientId::new(2),
            client_sequence: ClientSequence::new(1),
            state_deltas: vec![StateDelta::Register {
                name: 'a',
                text: "deleted".into(),
                linewise: false,
            }],
            documents: vec![DocumentMutation {
                document_id: DocumentId::new(4),
                lease_epoch: LeaseEpoch::new(1),
                base_revision: DocumentRevision::new(0),
                semantic_group_id: SemanticGroupId::new(10),
                semantic_group_kind: SemanticGroupKind::Operator,
                undo_parent: None,
                transactions: vec![
                    Transaction::new(DocumentRevision::new(0), vec![Edit::new(0..1, "")])
                        .expect("transaction"),
                ],
            }],
        }
    }

    fn durable() -> MutationResult {
        MutationResult::Durable {
            mutation_id: MutationId::new(10),
            client_sequence: ClientSequence::new(1),
            session_sequence: SessionSequence::new(1),
            documents: vec![AcceptedDocument {
                document_id: DocumentId::new(4),
                accepted_revision: DocumentRevision::new(1),
                canonical_transaction_hash: [0; 32],
            }],
        }
    }

    #[test]
    fn received_never_compacts_but_durable_does() {
        let directory = tempdir().expect("temporary directory");
        let outbox = MutationOutbox::in_directory(directory.path());
        outbox.append(&mutation()).expect("append");
        assert!(
            !outbox
                .observe_result(&MutationResult::Received {
                    mutation_id: MutationId::new(10),
                })
                .expect("received")
        );
        assert_eq!(outbox.outstanding().expect("outstanding"), vec![mutation()]);
        assert!(outbox.observe_result(&durable()).expect("durable"));
        assert!(outbox.outstanding().expect("compacted").is_empty());
    }

    #[test]
    fn torn_tail_preserves_the_last_complete_whole_mutation() {
        let directory = tempdir().expect("temporary directory");
        let outbox = MutationOutbox::in_directory(directory.path());
        outbox.append(&mutation()).expect("append");
        OpenOptions::new()
            .append(true)
            .open(outbox.path())
            .expect("open outbox")
            .write_all(b"WRENOUT1\x80")
            .expect("torn tail");
        assert_eq!(outbox.outstanding().expect("recover"), vec![mutation()]);
    }

    #[test]
    fn both_sides_can_crash_after_durable_and_reconcile_by_mutation_id() {
        let directory = tempdir().expect("temporary directory");
        let outbox = MutationOutbox::in_directory(directory.path().join("client"));
        let journal = SessionJournal::in_directory(directory.path().join("session"));
        let mut authority =
            SessionAuthority::open(journal.clone(), SessionId::new(1)).expect("authority");
        authority
            .register_document(DocumentId::new(4), "text", ClientId::new(2))
            .expect("document");
        let pending = mutation();
        outbox.append(&pending).expect("client durable");
        let first = authority.submit(pending.clone()).expect("remote durable");
        assert!(matches!(first, MutationSubmission::Accepted { .. }));

        // Simulate both processes dying before the client records the ack.
        drop(authority);
        let mut authority =
            SessionAuthority::open(journal, SessionId::new(1)).expect("session recovery");
        let replayed = outbox.outstanding().expect("client recovery");
        assert_eq!(replayed, vec![pending.clone()]);
        let retry = authority.submit(pending).expect("deduplicated retry");
        let durable = retry.durable().expect("durable result");
        outbox.observe_result(durable).expect("compact on durable");
        assert!(outbox.outstanding().expect("empty outbox").is_empty());
        assert_eq!(
            authority
                .document(DocumentId::new(4))
                .expect("document")
                .text,
            "ext"
        );
        assert_eq!(authority.client_state(ClientId::new(2)).len(), 1);
    }
}
