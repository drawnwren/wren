use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
    #[error(transparent)]
    Record(#[from] crate::record::DurableRecordError),
    #[error("mutation ID {mutation_id:?} was reused with different contents in the outbox")]
    MutationIdCollision { mutation_id: MutationId },
}

const COMPACT_AFTER_DURABLE_RECORDS: usize = 256;

#[derive(Debug, Default)]
struct OutboxState {
    outstanding: Vec<ClientMutation>,
    loaded: bool,
    durable_records_since_compaction: usize,
}

#[derive(Debug, Clone)]
pub struct MutationOutbox {
    path: PathBuf,
    state: Arc<Mutex<OutboxState>>,
}

impl MutationOutbox {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), state: Arc::new(Mutex::new(OutboxState::default())) }
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
        self.append_many(std::slice::from_ref(mutation)).map(|_| ())
    }

    /// Crash-safely records a group of mutations with one durability sync.
    /// Every mutation remains an independently checksummed WAL record.
    pub fn append_many(&self, mutations: &[ClientMutation]) -> Result<usize, OutboxError> {
        let mut state = self.lock_state()?;
        self.load_state(&mut state)?;
        let outstanding = &mut state.outstanding;
        let mut known = outstanding.iter().map(|mutation| (mutation.mutation_id, mutation)).collect::<HashMap<_, _>>();
        let mut additions = Vec::with_capacity(mutations.len());
        for mutation in mutations {
            if let Some(existing) = known.get(&mutation.mutation_id) {
                if *existing != mutation {
                    return Err(OutboxError::MutationIdCollision { mutation_id: mutation.mutation_id });
                }
                continue;
            }
            known.insert(mutation.mutation_id, mutation);
            additions.push(mutation.clone());
        }
        let records = additions.iter().cloned().map(OutboxRecord::Mutation).collect::<Vec<_>>();
        self.append_records(&records)?;
        let appended = additions.len();
        outstanding.extend(additions);
        Ok(appended)
    }

    /// Returns true only when a durable acknowledgement records a known
    /// mutation. `Received` and rejection results never remove client data.
    /// Durable records are crash-safe immediately; physical compaction is
    /// periodic so an acknowledgement does not rewrite the whole WAL.
    pub fn observe_result(&self, result: &MutationResult) -> Result<bool, OutboxError> {
        self.observe_results(std::slice::from_ref(result)).map(|acknowledged| acknowledged == 1)
    }

    /// Crash-safely records all known durable acknowledgements with one sync.
    /// Unknown, duplicate, received, and rejected results are ignored.
    pub fn observe_results(&self, results: &[MutationResult]) -> Result<usize, OutboxError> {
        let mut state = self.lock_state()?;
        self.load_state(&mut state)?;
        let outstanding = &state.outstanding;
        let mut known = outstanding.iter().map(|mutation| mutation.mutation_id).collect::<HashSet<_>>();
        let acknowledged = results
            .iter()
            .filter_map(|result| match result {
                MutationResult::Durable { mutation_id, .. } if known.remove(mutation_id) => Some(*mutation_id),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let records = acknowledged.iter().copied().map(OutboxRecord::Durable).collect::<Vec<_>>();
        self.append_records(&records)?;
        state.outstanding.retain(|mutation| !acknowledged.contains(&mutation.mutation_id));
        state.durable_records_since_compaction = state.durable_records_since_compaction.saturating_add(acknowledged.len());
        if state.durable_records_since_compaction >= COMPACT_AFTER_DURABLE_RECORDS {
            self.compact_outstanding(&state.outstanding)?;
            state.durable_records_since_compaction = 0;
        }
        Ok(acknowledged.len())
    }

    pub fn outstanding(&self) -> Result<Vec<ClientMutation>, OutboxError> {
        let mut state = self.lock_state()?;
        self.load_state(&mut state)?;
        Ok(state.outstanding.clone())
    }

    fn recover_outstanding(&self) -> Result<Vec<ClientMutation>, OutboxError> {
        let records = self.recover_records()?;
        let mut positions: HashMap<MutationId, usize> = HashMap::new();
        let mut mutations: Vec<Option<ClientMutation>> = Vec::new();
        for record in records {
            match record {
                OutboxRecord::Mutation(mutation) => {
                    if let Some(index) = positions.get(&mutation.mutation_id).copied() {
                        let existing = mutations[index].as_ref().ok_or(OutboxError::MutationIdCollision { mutation_id: mutation.mutation_id })?;
                        if existing != &mutation {
                            return Err(OutboxError::MutationIdCollision { mutation_id: mutation.mutation_id });
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
        let mut state = self.lock_state()?;
        self.load_state(&mut state)?;
        self.compact_outstanding(&state.outstanding)?;
        state.durable_records_since_compaction = 0;
        Ok(())
    }

    fn compact_outstanding(&self, outstanding: &[ClientMutation]) -> Result<(), OutboxError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| self.io(source))?;
        let mut temporary = NamedTempFile::new_in(parent).map_err(|source| self.io(source))?;
        for mutation in outstanding {
            self.records().write(temporary.as_file_mut(), &OutboxRecord::Mutation(mutation.clone()))?;
        }
        temporary.as_file_mut().flush().and_then(|()| temporary.as_file().sync_all()).map_err(|source| self.io(source))?;
        temporary.persist(&self.path).map_err(|error| self.io(error.error))?;
        File::open(parent).and_then(|directory| directory.sync_all()).map_err(|source| self.io(source))?;
        Ok(())
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, OutboxState>, OutboxError> {
        self.state.lock().map_err(|_| self.io(io::Error::other("mutation outbox state lock poisoned")))
    }

    fn load_state(&self, state: &mut OutboxState) -> Result<(), OutboxError> {
        if !state.loaded {
            state.outstanding = self.recover_outstanding()?;
            state.loaded = true;
        }
        Ok(())
    }

    fn append_records(&self, records: &[OutboxRecord]) -> Result<(), OutboxError> {
        self.records().append_many(records).map_err(Into::into)
    }

    fn recover_records(&self) -> Result<Vec<OutboxRecord>, OutboxError> {
        self.records().recover().map_err(Into::into)
    }

    fn io(&self, source: io::Error) -> OutboxError {
        self.records().error(source).into()
    }

    fn records(&self) -> crate::record::RecordStore<'_> {
        crate::record::RecordStore::new("client outbox", &self.path, MAGIC)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write as _;

    use tempfile::tempdir;
    use wren_types::{
        AcceptedDocument, ClientId, ClientSequence, DocumentId, DocumentMutation, DocumentRevision, Edit, LeaseEpoch, SemanticGroupId, SemanticGroupKind,
        SessionId, SessionSequence, StateDelta, Transaction,
    };

    use crate::{MutationSubmission, SessionAuthority, SessionJournal};

    use super::*;

    fn mutation() -> ClientMutation {
        ClientMutation {
            mutation_id: MutationId::new(10),
            client_id: ClientId::new(2),
            client_sequence: ClientSequence::new(1),
            state_deltas: vec![StateDelta::Register { name: 'a', text: "deleted".into(), linewise: false }],
            documents: vec![DocumentMutation {
                document_id: DocumentId::new(4),
                lease_epoch: LeaseEpoch::new(1),
                base_revision: DocumentRevision::new(0),
                semantic_group_id: SemanticGroupId::new(10),
                semantic_group_kind: SemanticGroupKind::Operator,
                undo_parent: None,
                transactions: vec![Transaction::new(DocumentRevision::new(0), vec![Edit::new(0..1, "")]).expect("transaction")],
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
    fn received_never_acknowledges_but_durable_does() {
        let directory = tempdir().expect("temporary directory");
        let outbox = MutationOutbox::in_directory(directory.path());
        outbox.append(&mutation()).expect("append");
        assert!(!outbox.observe_result(&MutationResult::Received { mutation_id: MutationId::new(10) }).expect("received"));
        assert_eq!(outbox.outstanding().expect("outstanding"), vec![mutation()]);
        assert!(outbox.observe_result(&durable()).expect("durable"));
        assert!(outbox.outstanding().expect("compacted").is_empty());
    }

    #[test]
    fn mutation_and_acknowledgement_batches_survive_reopen() {
        let directory = tempdir().expect("temporary directory");
        let outbox = MutationOutbox::in_directory(directory.path());
        let mutations = (1..=64_u64)
            .map(|sequence| {
                let mut pending = mutation();
                pending.mutation_id = MutationId::new(sequence);
                pending.client_sequence = ClientSequence::new(sequence);
                pending
            })
            .collect::<Vec<_>>();
        assert_eq!(outbox.append_many(&mutations).expect("append batch"), 64);
        drop(outbox);

        let reopened = MutationOutbox::in_directory(directory.path());
        assert_eq!(reopened.outstanding().expect("recover batch"), mutations);
        let acknowledgements = mutations
            .iter()
            .map(|mutation| MutationResult::Durable {
                mutation_id: mutation.mutation_id,
                client_sequence: mutation.client_sequence,
                session_sequence: SessionSequence::new(mutation.client_sequence.get()),
                documents: Vec::new(),
            })
            .collect::<Vec<_>>();
        assert_eq!(reopened.observe_results(&acknowledgements).expect("acknowledge batch"), 64);
        drop(reopened);
        assert!(MutationOutbox::in_directory(directory.path()).outstanding().expect("recover acknowledgements").is_empty());
    }

    #[test]
    fn colliding_id_rejects_a_whole_outbox_batch_before_writing() {
        let directory = tempdir().expect("temporary directory");
        let outbox = MutationOutbox::in_directory(directory.path());
        let first = mutation();
        let mut collision = first.clone();
        collision.state_deltas.clear();
        assert!(matches!(outbox.append_many(&[first, collision]), Err(OutboxError::MutationIdCollision { .. })));
        assert!(outbox.outstanding().expect("empty outbox").is_empty());
    }

    #[test]
    fn durable_records_compact_periodically_instead_of_rewriting_per_ack() {
        let directory = tempdir().expect("temporary directory");
        let outbox = MutationOutbox::in_directory(directory.path());
        for sequence in 1..=COMPACT_AFTER_DURABLE_RECORDS {
            let mutation_id = MutationId::new(sequence as u64);
            let mut pending = mutation();
            pending.mutation_id = mutation_id;
            outbox.append(&pending).expect("append");
            let mut acknowledged = durable();
            let MutationResult::Durable { mutation_id: durable_id, .. } = &mut acknowledged else {
                unreachable!();
            };
            *durable_id = mutation_id;
            assert!(outbox.observe_result(&acknowledged).expect("durable"));
        }

        assert_eq!(std::fs::metadata(outbox.path()).expect("compacted outbox").len(), 0);
        let reopened = MutationOutbox::in_directory(directory.path());
        assert!(reopened.outstanding().expect("reopened outbox").is_empty());
    }

    #[test]
    fn torn_tail_preserves_the_last_complete_whole_mutation() {
        let directory = tempdir().expect("temporary directory");
        let outbox = MutationOutbox::in_directory(directory.path());
        outbox.append(&mutation()).expect("append");
        OpenOptions::new().append(true).open(outbox.path()).expect("open outbox").write_all(b"WRENOUT1\x80").expect("torn tail");
        assert_eq!(outbox.outstanding().expect("recover"), vec![mutation()]);
    }

    #[test]
    fn both_sides_can_crash_after_durable_and_reconcile_by_mutation_id() {
        let directory = tempdir().expect("temporary directory");
        let outbox = MutationOutbox::in_directory(directory.path().join("client"));
        let journal = SessionJournal::in_directory(directory.path().join("session"));
        let mut authority = SessionAuthority::open(journal.clone(), SessionId::new(1)).expect("authority");
        authority.register_document(DocumentId::new(4), "text", ClientId::new(2)).expect("document");
        let pending = mutation();
        outbox.append(&pending).expect("client durable");
        let first = authority.submit(pending.clone()).expect("remote durable");
        assert!(matches!(first, MutationSubmission::Accepted { .. }));

        // Simulate both processes dying before the client records the ack.
        drop(authority);
        let mut authority = SessionAuthority::open(journal, SessionId::new(1)).expect("session recovery");
        let replayed = outbox.outstanding().expect("client recovery");
        assert_eq!(replayed, vec![pending.clone()]);
        let retry = authority.submit(pending).expect("deduplicated retry");
        let durable = retry.durable().expect("durable result");
        outbox.observe_result(durable).expect("compact on durable");
        assert!(outbox.outstanding().expect("empty outbox").is_empty());
        assert_eq!(authority.document(DocumentId::new(4)).expect("document").text(), "ext");
        assert_eq!(authority.client_state(ClientId::new(2)).len(), 1);
    }
}
