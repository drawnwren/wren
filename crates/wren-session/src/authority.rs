use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use thiserror::Error;
use wren_text::{DefaultText, TextStore};
#[cfg(test)]
use wren_types::StateCheckpoint;
use wren_types::{
    AcceptedDocument, ClientId, ClientMutation, ClientSequence, DocumentFrontier, DocumentId, DocumentRevision, EventOrigin, LeaseEpoch, LeaseGrant,
    MutationId, MutationResult, MutationValidationError, OfflinePolicy, Resume, ResumeResult, SessionEpoch, SessionEvent, SessionEventPayload, SessionId,
    SessionSequence, StateDelta, Transaction, WorkspaceGeneration,
};

use crate::journal::{JournalEntry, RegisteredDocument};
use crate::{SessionJournal, SessionJournalError};

#[derive(Debug, Clone)]
pub struct AuthorityDocument {
    pub document_id: DocumentId,
    text: DefaultText,
    pub revision: DocumentRevision,
    pub lease: LeaseGrant,
    // Rebase history is deliberately independent from the replay event window.
    // Session events may be compacted or invalidated by an epoch change, while
    // an online writer can still present an older document frontier.
    history: Vec<Transaction>,
}

impl AuthorityDocument {
    /// Materializes the authoritative text for external consumers such as a
    /// save or snapshot. Mutation staging itself remains rope-backed.
    #[must_use]
    pub fn text(&self) -> String {
        self.text.slice(0..self.text.len_bytes()).into_owned()
    }

    #[must_use]
    pub fn text_equals(&self, expected: &str) -> bool {
        self.text.slice(0..self.text.len_bytes()) == expected
    }

    #[must_use]
    pub fn delta_since(&self, base: DocumentRevision) -> Vec<Transaction> {
        if base >= self.revision {
            return Vec::new();
        }
        self.history.iter().filter(|transaction| transaction.base_revision() >= base).cloned().collect()
    }

    fn apply(&mut self, transactions: &[Transaction]) -> Result<(), AuthorityError> {
        for transaction in transactions {
            transaction
                .validate_boundaries(self.text.len_bytes(), |offset| self.text.is_char_boundary(offset))
                .map_err(|error| AuthorityError::Replay(format!("transaction for {:?} could not apply: {error}", self.document_id)))?;
            self.text.apply(transaction);
            self.revision = self.revision.next().ok_or(AuthorityError::CounterOverflow)?;
            self.history.push(transaction.clone());
        }
        Ok(())
    }
}

/// A validated submission either reaches its durable result or returns the
/// protocol-level rejection that prevented it. `Received` is a transport ack,
/// so it is emitted by the connection rather than duplicated in this value.
pub type MutationSubmission = Result<MutationResult, MutationResult>;

#[derive(Debug, Error)]
pub enum AuthorityError {
    #[error(transparent)]
    Journal(#[from] SessionJournalError),
    #[error("invalid client mutation: {0}")]
    InvalidMutation(#[from] MutationValidationError),
    #[error("session journal belongs to session {actual:?}, not {expected:?}")]
    SessionMismatch { expected: SessionId, actual: SessionId },
    #[error("session journal is missing its initialization record")]
    MissingInitialization,
    #[error("session journal contains more than one initialization record")]
    DuplicateInitialization,
    #[error("document {0:?} is already registered")]
    DocumentAlreadyRegistered(DocumentId),
    #[error("document {0:?} is not registered")]
    UnknownDocument(DocumentId),
    #[error("mutation ID {mutation_id:?} was reused with different contents")]
    MutationIdCollision { mutation_id: MutationId },
    #[error("client {client_id:?} sequence must advance beyond {previous:?}, received {actual:?}")]
    StaleClientSequence { client_id: ClientId, previous: ClientSequence, actual: ClientSequence },
    #[error("checkpoint for client {client_id:?} is through {through:?}, beyond durable sequence {durable:?}")]
    CheckpointAhead { client_id: ClientId, through: ClientSequence, durable: ClientSequence },
    #[error("session journal replay is inconsistent: {0}")]
    Replay(String),
    #[error("numeric session counter overflow")]
    CounterOverflow,
    #[error("serialize canonical mutation: {0}")]
    CanonicalSerialization(serde_json::Error),
}

#[derive(Debug, Clone)]
struct DedupEntry {
    mutation_hash: [u8; 32],
    durable: MutationResult,
}

#[derive(Debug, Clone)]
pub struct SessionAuthority {
    journal: SessionJournal,
    session_id: SessionId,
    session_epoch: SessionEpoch,
    workspace_generation: WorkspaceGeneration,
    session_sequence: SessionSequence,
    retained_after: SessionSequence,
    documents: BTreeMap<DocumentId, AuthorityDocument>,
    state: BTreeMap<ClientId, Vec<StateDelta>>,
    highest_client_sequence: BTreeMap<ClientId, ClientSequence>,
    dedup: HashMap<MutationId, DedupEntry>,
    events: Vec<SessionEvent>,
    event_retention_limit: usize,
}

/// The authority's durable prepare record. It owns only the document roots,
/// client sequence entries, dedup records, state deltas, and events touched by
/// one submission burst; untouched session state continues to be read from
/// the live authority until the single journal frontier succeeds.
#[derive(Debug)]
struct PreparedCommit<'a> {
    authority: &'a SessionAuthority,
    session_epoch: SessionEpoch,
    workspace_generation: WorkspaceGeneration,
    session_sequence: SessionSequence,
    retained_after: SessionSequence,
    documents: BTreeMap<DocumentId, AuthorityDocument>,
    state: BTreeMap<ClientId, Vec<StateDelta>>,
    highest_client_sequence: BTreeMap<ClientId, ClientSequence>,
    dedup: HashMap<MutationId, DedupEntry>,
    events: Vec<SessionEvent>,
    reset_events: bool,
    journal_entries: Vec<JournalEntry>,
    submissions: Vec<MutationSubmission>,
}

impl<'a> PreparedCommit<'a> {
    fn new(authority: &'a SessionAuthority, capacity: usize) -> Self {
        Self {
            authority,
            session_epoch: authority.session_epoch,
            workspace_generation: authority.workspace_generation,
            session_sequence: authority.session_sequence,
            retained_after: authority.retained_after,
            documents: BTreeMap::new(),
            state: BTreeMap::new(),
            highest_client_sequence: BTreeMap::new(),
            dedup: HashMap::new(),
            events: Vec::new(),
            reset_events: false,
            journal_entries: Vec::with_capacity(capacity),
            submissions: Vec::with_capacity(capacity),
        }
    }

    fn previous_submission(&self, mutation: &ClientMutation, mutation_hash: [u8; 32]) -> Result<Option<MutationSubmission>, AuthorityError> {
        let existing = self.dedup.get(&mutation.mutation_id).or_else(|| self.authority.dedup.get(&mutation.mutation_id));
        let Some(existing) = existing else {
            return Ok(None);
        };
        if existing.mutation_hash != mutation_hash {
            return Err(AuthorityError::MutationIdCollision { mutation_id: mutation.mutation_id });
        }
        Ok(Some(Ok(existing.durable.clone())))
    }

    fn validate_client_sequence(&self, mutation: &ClientMutation) -> Result<(), AuthorityError> {
        let previous = self.highest_client_sequence.get(&mutation.client_id).or_else(|| self.authority.highest_client_sequence.get(&mutation.client_id));
        if let Some(previous) = previous
            && mutation.client_sequence <= *previous
        {
            return Err(AuthorityError::StaleClientSequence { client_id: mutation.client_id, previous: *previous, actual: mutation.client_sequence });
        }
        Ok(())
    }

    fn stage_documents(&self, mutation: &ClientMutation) -> Result<Result<BTreeMap<DocumentId, AuthorityDocument>, MutationResult>, AuthorityError> {
        let mut staged = BTreeMap::new();
        for document_mutation in &mutation.documents {
            let Some(document) = self.documents.get(&document_mutation.document_id).or_else(|| self.authority.documents.get(&document_mutation.document_id))
            else {
                return Ok(Err(MutationResult::Conflict {
                    document_id: document_mutation.document_id,
                    reason: "document is not registered in this session".into(),
                }));
            };
            if document_mutation.lease_epoch != document.lease.lease_epoch || mutation.client_id != document.lease.holder_id {
                return Ok(Err(MutationResult::LeaseLost { document_id: document_mutation.document_id, current_lease_epoch: document.lease.lease_epoch }));
            }
            if document_mutation.base_revision != document.revision {
                return Ok(Err(MutationResult::RebaseRequired {
                    mutation_id: mutation.mutation_id,
                    document_id: document_mutation.document_id,
                    authoritative_revision: document.revision,
                    delta_since_base: document.delta_since(document_mutation.base_revision),
                }));
            }
            let mut update = document.clone();
            update.apply(&document_mutation.transactions)?;
            staged.insert(document_mutation.document_id, update);
        }
        Ok(Ok(staged))
    }

    fn events_for(&mut self, mutation: &ClientMutation) -> Result<(Vec<SessionEvent>, SessionSequence), AuthorityError> {
        let mut value = self.session_sequence.get();
        let mut next = || -> Result<SessionSequence, AuthorityError> {
            value = value.checked_add(1).ok_or(AuthorityError::CounterOverflow)?;
            Ok(SessionSequence::new(value))
        };
        let payloads = mutation
            .documents
            .iter()
            .map(|document| {
                Ok::<_, AuthorityError>(SessionEventPayload::DocumentDelta {
                    document_id: document.document_id,
                    accepted_revision: document.accepted_revision()?,
                    transactions: document.transactions.clone(),
                })
            })
            .chain(mutation.state_deltas.iter().map(|delta| Ok::<_, AuthorityError>(SessionEventPayload::StateDelta(delta.clone()))));
        let events = payloads
            .map(|payload| Ok(SessionEvent { session_sequence: next()?, origin: EventOrigin::Client(mutation.client_id), payload: payload? }))
            .collect::<Result<Vec<_>, AuthorityError>>()?;
        let final_sequence = events.last().map_or(self.session_sequence, |event| event.session_sequence);
        self.session_sequence = final_sequence;
        Ok((events, final_sequence))
    }

    fn install_accepted(
        &mut self,
        mutation: &ClientMutation,
        documents: BTreeMap<DocumentId, AuthorityDocument>,
        durable: MutationResult,
        events: Vec<SessionEvent>,
        mutation_hash: [u8; 32],
    ) -> Result<(), AuthorityError> {
        self.documents.extend(documents);
        self.state.entry(mutation.client_id).or_default().extend(mutation.state_deltas.iter().cloned());
        self.highest_client_sequence.insert(mutation.client_id, mutation.client_sequence);
        self.dedup.insert(mutation.mutation_id, DedupEntry { mutation_hash, durable });
        self.events.extend(events);
        if self.authority.events.len().saturating_add(self.events.len()) > self.authority.event_retention_limit {
            let next = |value: u64| value.checked_add(1).ok_or(AuthorityError::CounterOverflow);
            self.session_epoch = SessionEpoch::new(next(self.session_epoch.get())?);
            self.workspace_generation = WorkspaceGeneration::new(next(self.workspace_generation.get())?);
            self.retained_after = self.session_sequence;
            self.journal_entries.push(JournalEntry::ContinuityBroken {
                new_session_epoch: self.session_epoch,
                workspace_generation: self.workspace_generation,
                retained_after: self.retained_after,
            });
            self.events.clear();
            self.reset_events = true;
        }
        Ok(())
    }
}

impl SessionAuthority {
    pub fn open(journal: SessionJournal, session_id: SessionId) -> Result<Self, AuthorityError> {
        let entries = journal.recover()?;
        if entries.is_empty() {
            let initialized = JournalEntry::Initialized { session_id, session_epoch: SessionEpoch::new(1), workspace_generation: WorkspaceGeneration::new(1) };
            journal.append(&initialized)?;
            return Ok(Self::empty(journal, session_id));
        }

        let mut authority = Self::empty(journal, session_id);
        let mut initialized = false;
        for entry in entries {
            match entry {
                JournalEntry::Initialized { session_id: actual, session_epoch, workspace_generation } => {
                    if initialized {
                        return Err(AuthorityError::DuplicateInitialization);
                    }
                    if actual != session_id {
                        return Err(AuthorityError::SessionMismatch { expected: session_id, actual });
                    }
                    authority.session_epoch = session_epoch;
                    authority.workspace_generation = workspace_generation;
                    initialized = true;
                }
                _entry if !initialized => return Err(AuthorityError::MissingInitialization),
                entry => authority.replay(entry)?,
            }
        }
        Ok(authority)
    }

    fn empty(journal: SessionJournal, session_id: SessionId) -> Self {
        Self {
            journal,
            session_id,
            session_epoch: SessionEpoch::new(1),
            workspace_generation: WorkspaceGeneration::new(1),
            session_sequence: SessionSequence::new(0),
            retained_after: SessionSequence::new(0),
            documents: BTreeMap::new(),
            state: BTreeMap::new(),
            highest_client_sequence: BTreeMap::new(),
            dedup: HashMap::new(),
            events: Vec::new(),
            event_retention_limit: 10_000,
        }
    }

    #[must_use]
    #[cfg(test)]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    #[must_use]
    pub const fn session_epoch(&self) -> SessionEpoch {
        self.session_epoch
    }

    #[must_use]
    #[cfg(test)]
    pub const fn workspace_generation(&self) -> WorkspaceGeneration {
        self.workspace_generation
    }

    #[must_use]
    pub const fn session_sequence(&self) -> SessionSequence {
        self.session_sequence
    }

    pub fn document(&self, document_id: DocumentId) -> Option<&AuthorityDocument> {
        self.documents.get(&document_id)
    }

    #[must_use]
    pub fn document_heads(&self) -> Vec<wren_types::DocumentHead> {
        self.documents
            .values()
            .map(|document| wren_types::DocumentHead {
                session_epoch: self.session_epoch,
                document_id: document.document_id,
                authoritative_revision: document.revision,
            })
            .collect()
    }

    #[must_use]
    #[cfg(test)]
    pub const fn retained_after(&self) -> SessionSequence {
        self.retained_after
    }

    #[must_use]
    #[cfg(test)]
    pub const fn event_retention_limit(&self) -> usize {
        self.event_retention_limit
    }

    #[must_use]
    pub fn events_after(&self, sequence: SessionSequence) -> Vec<SessionEvent> {
        self.events.iter().filter(|event| event.session_sequence > sequence).cloned().collect()
    }

    #[cfg(test)]
    pub fn set_event_retention_limit(&mut self, limit: usize) -> Result<(), AuthorityError> {
        self.event_retention_limit = limit.max(1);
        self.enforce_event_retention()
    }

    #[must_use]
    #[cfg(test)]
    pub fn client_state(&self, client_id: ClientId) -> &[StateDelta] {
        self.state.get(&client_id).map_or(&[], Vec::as_slice)
    }

    #[must_use]
    pub fn highest_client_sequence(&self, client_id: ClientId) -> ClientSequence {
        self.highest_client_sequence.get(&client_id).copied().unwrap_or(ClientSequence::new(0))
    }

    pub fn register_document(&mut self, document_id: DocumentId, text: impl Into<String>, holder_id: ClientId) -> Result<LeaseGrant, AuthorityError> {
        if self.documents.contains_key(&document_id) {
            return Err(AuthorityError::DocumentAlreadyRegistered(document_id));
        }
        let lease = LeaseGrant { document_id, lease_epoch: LeaseEpoch::new(1), holder_id, offline_policy: OfflinePolicy::LocalBranch };
        let registered =
            RegisteredDocument { frontier: DocumentFrontier { document_id, revision: DocumentRevision::new(0) }, text: text.into(), lease: lease.clone() };
        self.journal.append(&JournalEntry::DocumentRegistered(registered.clone()))?;
        self.install_registered(registered)?;
        Ok(lease)
    }

    pub fn submit(&mut self, mutation: ClientMutation) -> Result<MutationSubmission, AuthorityError> {
        let mut submissions = self.submit_batch(vec![mutation])?;
        submissions.pop().ok_or_else(|| AuthorityError::Replay("single mutation submission produced no result".to_owned()))
    }

    /// Validates an ordered burst against a private staged authority, writes
    /// every accepted commit as independent journal records in one durable
    /// write, and only then publishes the staged state. An I/O or validation
    /// error leaves the live authority unchanged.
    pub fn submit_batch(&mut self, mutations: Vec<ClientMutation>) -> Result<Vec<MutationSubmission>, AuthorityError> {
        let mut prepared = PreparedCommit::new(self, mutations.len());
        for mutation in mutations {
            mutation.validate()?;
            let mutation_hash = canonical_hash(&mutation)?;
            if let Some(submission) = prepared.previous_submission(&mutation, mutation_hash)? {
                prepared.submissions.push(submission);
                continue;
            }
            prepared.validate_client_sequence(&mutation)?;
            let staged_documents = match prepared.stage_documents(&mutation)? {
                Ok(documents) => documents,
                Err(rejection) => {
                    prepared.submissions.push(Err(rejection));
                    continue;
                }
            };
            let (events, final_sequence) = prepared.events_for(&mutation)?;
            let durable = durable_result(&mutation, final_sequence)?;

            prepared.journal_entries.push(JournalEntry::MutationCommitted { mutation: mutation.clone(), durable: durable.clone(), events: events.clone() });
            prepared.install_accepted(&mutation, staged_documents, durable.clone(), events, mutation_hash)?;
            prepared.submissions.push(Ok(durable));
        }

        // The O_DSYNC batch write is the durable frontier. Only after it
        // succeeds may memory expose any accepted state or Durable result.
        let PreparedCommit {
            authority: _,
            session_epoch,
            workspace_generation,
            session_sequence,
            retained_after,
            documents,
            state,
            highest_client_sequence,
            dedup,
            events,
            reset_events,
            journal_entries,
            submissions,
        } = prepared;
        self.journal.append_many(&journal_entries)?;
        self.documents.extend(documents);
        for (client_id, state) in state {
            self.state.entry(client_id).or_default().extend(state);
        }
        self.highest_client_sequence.extend(highest_client_sequence);
        self.dedup.extend(dedup);
        self.session_epoch = session_epoch;
        self.workspace_generation = workspace_generation;
        self.session_sequence = session_sequence;
        self.retained_after = retained_after;
        if reset_events {
            self.events.clear();
        }
        self.events.extend(events);
        Ok(submissions)
    }

    #[cfg(test)]
    pub fn grant_lease(&mut self, document_id: DocumentId, holder_id: ClientId, offline_policy: OfflinePolicy) -> Result<LeaseGrant, AuthorityError> {
        let current = self.documents.get(&document_id).ok_or(AuthorityError::UnknownDocument(document_id))?;
        let lease_epoch = current.lease.lease_epoch.get().checked_add(1).map(LeaseEpoch::new).ok_or(AuthorityError::CounterOverflow)?;
        let grant = LeaseGrant { document_id, lease_epoch, holder_id, offline_policy };
        let sequence = self.next_sequence()?;
        let event = SessionEvent { session_sequence: sequence, origin: EventOrigin::Workspace, payload: SessionEventPayload::LeaseChange(grant.clone()) };
        self.journal.append(&JournalEntry::LeaseChanged { grant: grant.clone(), event: event.clone() })?;
        self.documents.get_mut(&document_id).ok_or(AuthorityError::UnknownDocument(document_id))?.lease = grant.clone();
        self.session_sequence = sequence;
        self.events.push(event);
        Ok(grant)
    }

    pub fn resume(&self, request: &Resume) -> ResumeResult {
        if request.session_id != self.session_id
            || request.session_epoch != self.session_epoch
            || request.last_session_sequence < self.retained_after
            || request.last_session_sequence > self.session_sequence
        {
            return self.snapshot_required();
        }
        ResumeResult::Replay { events: self.events.iter().filter(|event| event.session_sequence > request.last_session_sequence).cloned().collect() }
    }

    pub fn break_event_continuity(&mut self) -> Result<SessionEpoch, AuthorityError> {
        let (new_session_epoch, workspace_generation, boundary) = self.next_continuity()?;
        self.journal.append(&boundary)?;
        self.session_epoch = new_session_epoch;
        self.workspace_generation = workspace_generation;
        self.retained_after = self.session_sequence;
        self.events.clear();
        Ok(new_session_epoch)
    }

    fn next_continuity(&self) -> Result<(SessionEpoch, WorkspaceGeneration, JournalEntry), AuthorityError> {
        let next = |value: u64| value.checked_add(1).ok_or(AuthorityError::CounterOverflow);
        let new_session_epoch = SessionEpoch::new(next(self.session_epoch.get())?);
        let workspace_generation = WorkspaceGeneration::new(next(self.workspace_generation.get())?);
        let boundary = JournalEntry::ContinuityBroken { new_session_epoch, workspace_generation, retained_after: self.session_sequence };
        Ok((new_session_epoch, workspace_generation, boundary))
    }

    #[cfg(test)]
    fn enforce_event_retention(&mut self) -> Result<(), AuthorityError> {
        if self.events.len() > self.event_retention_limit {
            self.break_event_continuity()?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn checkpoint_state(&mut self, checkpoint: wren_types::StateCheckpoint) -> Result<(), AuthorityError> {
        let durable = self.highest_client_sequence.get(&checkpoint.client_id).copied().unwrap_or(ClientSequence::new(0));
        if checkpoint.through_client_sequence > durable {
            return Err(AuthorityError::CheckpointAhead { client_id: checkpoint.client_id, through: checkpoint.through_client_sequence, durable });
        }
        self.journal.append(&JournalEntry::StateCheckpointed {
            client_id: checkpoint.client_id,
            through_client_sequence: checkpoint.through_client_sequence,
            state: checkpoint.state.clone(),
        })?;
        self.state.insert(checkpoint.client_id, checkpoint.state);
        Ok(())
    }

    fn snapshot_required(&self) -> ResumeResult {
        ResumeResult::SnapshotRequired {
            new_session_epoch: self.session_epoch,
            workspace_generation: self.workspace_generation,
            document_heads: self
                .documents
                .values()
                .map(|document| DocumentFrontier { document_id: document.document_id, revision: document.revision })
                .collect(),
        }
    }

    #[cfg(test)]
    fn next_sequence(&self) -> Result<SessionSequence, AuthorityError> {
        self.session_sequence.get().checked_add(1).map(SessionSequence::new).ok_or(AuthorityError::CounterOverflow)
    }

    fn replay(&mut self, entry: JournalEntry) -> Result<(), AuthorityError> {
        match entry {
            JournalEntry::Initialized { .. } => Err(AuthorityError::DuplicateInitialization),
            JournalEntry::DocumentRegistered(registered) => self.install_registered(registered),
            JournalEntry::MutationCommitted { mutation, durable, events } => {
                let mutation_hash = canonical_hash(&mutation)?;
                for document_mutation in &mutation.documents {
                    let document = self
                        .documents
                        .get_mut(&document_mutation.document_id)
                        .ok_or_else(|| AuthorityError::Replay(format!("mutation references missing document {:?}", document_mutation.document_id)))?;
                    if document.revision != document_mutation.base_revision {
                        return Err(AuthorityError::Replay(format!(
                            "document {:?} expected revision {:?}, journal has {:?}",
                            document.document_id, document.revision, document_mutation.base_revision
                        )));
                    }
                    document.apply(&document_mutation.transactions)?;
                }
                self.install_committed(&mutation, durable, events, mutation_hash)
            }
            JournalEntry::LeaseChanged { grant, event } => {
                let document_id = grant.document_id;
                self.documents.get_mut(&document_id).ok_or(AuthorityError::UnknownDocument(document_id))?.lease = grant;
                self.session_sequence = event.session_sequence;
                self.events.push(event);
                Ok(())
            }
            JournalEntry::StateCheckpointed { client_id, through_client_sequence, state } => {
                self.state.insert(client_id, state);
                self.highest_client_sequence.insert(client_id, through_client_sequence);
                Ok(())
            }
            JournalEntry::ContinuityBroken { new_session_epoch, workspace_generation, retained_after } => {
                self.session_epoch = new_session_epoch;
                self.workspace_generation = workspace_generation;
                self.retained_after = retained_after;
                self.events.clear();
                Ok(())
            }
        }
    }

    fn install_registered(&mut self, registered: RegisteredDocument) -> Result<(), AuthorityError> {
        let document_id = registered.frontier.document_id;
        if self.documents.contains_key(&document_id) {
            return Err(AuthorityError::DocumentAlreadyRegistered(document_id));
        }
        self.documents.insert(
            document_id,
            AuthorityDocument {
                document_id,
                text: text_store(registered.text),
                revision: registered.frontier.revision,
                lease: registered.lease,
                history: Vec::new(),
            },
        );
        Ok(())
    }

    fn install_committed(
        &mut self,
        mutation: &ClientMutation,
        durable: MutationResult,
        events: Vec<SessionEvent>,
        mutation_hash: [u8; 32],
    ) -> Result<(), AuthorityError> {
        let final_sequence = match &durable {
            MutationResult::Durable { session_sequence, .. } => *session_sequence,
            _ => {
                return Err(AuthorityError::Replay("committed mutation did not contain a Durable result".to_owned()));
            }
        };
        self.state.entry(mutation.client_id).or_default().extend(mutation.state_deltas.iter().cloned());
        self.highest_client_sequence.insert(mutation.client_id, mutation.client_sequence);
        self.session_sequence = final_sequence;
        self.events.extend(events);
        self.dedup.insert(mutation.mutation_id, DedupEntry { mutation_hash, durable });
        Ok(())
    }
}

fn text_store(text: String) -> DefaultText {
    DefaultText::from_string(text)
}

fn durable_result(mutation: &ClientMutation, session_sequence: SessionSequence) -> Result<MutationResult, AuthorityError> {
    let documents = mutation
        .documents
        .iter()
        .map(|document| {
            Ok(AcceptedDocument {
                document_id: document.document_id,
                accepted_revision: document.accepted_revision()?,
                canonical_transaction_hash: canonical_hash(document)?,
            })
        })
        .collect::<Result<Vec<_>, AuthorityError>>()?;
    Ok(MutationResult::Durable { mutation_id: mutation.mutation_id, client_sequence: mutation.client_sequence, session_sequence, documents })
}

fn canonical_hash(value: &impl Serialize) -> Result<[u8; 32], AuthorityError> {
    serde_json::to_vec(value).map(|bytes| *blake3::hash(&bytes).as_bytes()).map_err(AuthorityError::CanonicalSerialization)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use wren_types::{Edit, SemanticGroupId, SemanticGroupKind};

    use super::*;

    fn mutation(mutation_id: u64, client_sequence: u64, lease_epoch: u64, base_revision: u64, insert: &str) -> ClientMutation {
        ClientMutation {
            mutation_id: MutationId::new(mutation_id),
            client_id: ClientId::new(7),
            client_sequence: ClientSequence::new(client_sequence),
            state_deltas: vec![StateDelta::Register { name: '"', text: insert.into(), linewise: false }],
            documents: vec![wren_types::DocumentMutation {
                document_id: DocumentId::new(11),
                lease_epoch: LeaseEpoch::new(lease_epoch),
                base_revision: DocumentRevision::new(base_revision),
                semantic_group_id: SemanticGroupId::new(mutation_id),
                semantic_group_kind: SemanticGroupKind::InsertRun,
                undo_parent: None,
                transactions: vec![Transaction::new(DocumentRevision::new(base_revision), vec![Edit::new(0..0, insert)]).expect("transaction")],
            }],
        }
    }

    fn open_authority(directory: &std::path::Path) -> SessionAuthority {
        SessionAuthority::open(SessionJournal::in_directory(directory), SessionId::new(3)).expect("open authority")
    }

    fn authority_with_document(directory: &std::path::Path, text: &str) -> SessionAuthority {
        let mut authority = open_authority(directory);
        authority.register_document(DocumentId::new(11), text, ClientId::new(7)).expect("register document");
        authority
    }

    fn document_text(authority: &SessionAuthority, document: u64) -> String {
        authority.document(DocumentId::new(document)).expect("registered document").text()
    }

    #[test]
    fn durable_follows_journal_sync_and_duplicate_ids_are_not_reapplied() {
        let directory = tempdir().expect("temporary directory");
        let mut first = authority_with_document(directory.path(), "base");
        let original = mutation(19, 1, 1, 0, "x");
        let submission = first.submit(original.clone()).expect("submit");
        assert!(matches!(submission, Ok(MutationResult::Durable { .. })));
        assert_eq!(document_text(&first, 11), "xbase");
        drop(first);

        let mut recovered = open_authority(directory.path());
        assert_eq!(document_text(&recovered, 11), "xbase");
        let duplicate = recovered.submit(original).expect("retry after lost ack");
        assert!(duplicate.is_ok());
        assert_eq!(document_text(&recovered, 11), "xbase");
    }

    #[test]
    fn ordered_batch_is_durable_as_independent_recoverable_commits() {
        let directory = tempdir().expect("temporary directory");
        let mut authority = authority_with_document(directory.path(), "base");
        let mutations = (0..128_u64).map(|index| mutation(index + 1, index + 1, 1, index, "x")).collect::<Vec<_>>();
        let submissions = authority.submit_batch(mutations).expect("submit mutation batch");
        assert_eq!(submissions.len(), 128);
        assert!(submissions.iter().all(|submission| matches!(submission, Ok(MutationResult::Durable { .. }))));
        let expected = format!("{}base", "x".repeat(128));
        assert_eq!(document_text(&authority, 11), expected);

        drop(authority);
        let recovered = open_authority(directory.path());
        assert_eq!(document_text(&recovered, 11), expected);
        assert_eq!(recovered.document(DocumentId::new(11)).expect("recovered document").revision, DocumentRevision::new(128));
    }

    #[test]
    fn mutation_is_atomic_across_documents_and_state() {
        let directory = tempdir().expect("temporary directory");
        let mut authority = authority_with_document(directory.path(), "a");
        authority.register_document(DocumentId::new(12), "b", ClientId::new(7)).expect("register second");
        let mut proposed = mutation(1, 1, 1, 0, "x");
        proposed.documents.push(wren_types::DocumentMutation {
            document_id: DocumentId::new(12),
            lease_epoch: LeaseEpoch::new(99),
            base_revision: DocumentRevision::new(0),
            semantic_group_id: SemanticGroupId::new(1),
            semantic_group_kind: SemanticGroupKind::WorkspaceRefactor,
            undo_parent: None,
            transactions: vec![Transaction::new(DocumentRevision::new(0), vec![Edit::new(0..0, "y")]).expect("transaction")],
        });
        assert!(matches!(authority.submit(proposed).expect("rejection"), Err(MutationResult::LeaseLost { .. })));
        assert_eq!((document_text(&authority, 11), document_text(&authority, 12)), ("a".to_owned(), "b".to_owned()));
        assert_eq!(authority.session_sequence(), SessionSequence::new(0));
    }

    #[test]
    fn stale_revision_returns_delta_and_stale_lease_is_fenced() {
        let directory = tempdir().expect("temporary directory");
        let mut authority = authority_with_document(directory.path(), "a");
        let _ = authority.submit(mutation(1, 1, 1, 0, "x")).expect("first mutation");
        assert!(matches!(
            authority.submit(mutation(2, 2, 1, 0, "y")).expect("rebase"),
            Err(MutationResult::RebaseRequired {
                authoritative_revision,
                ref delta_since_base,
                ..
            }) if authoritative_revision == DocumentRevision::new(1) && delta_since_base.len() == 1
        ));
        authority.grant_lease(DocumentId::new(11), ClientId::new(8), OfflinePolicy::LocalBranch).expect("move lease");
        assert!(matches!(
            authority.submit(mutation(3, 2, 1, 1, "z")).expect("fence"),
            Err(MutationResult::LeaseLost {
                current_lease_epoch,
                ..
            }) if current_lease_epoch == LeaseEpoch::new(2)
        ));
    }

    #[test]
    fn resume_replays_contiguous_events_or_requires_a_snapshot_after_epoch_change() {
        let directory = tempdir().expect("temporary directory");
        let mut authority = authority_with_document(directory.path(), "a");
        let _ = authority.submit(mutation(1, 1, 1, 0, "x")).expect("mutation");
        let resume = Resume {
            session_id: SessionId::new(3),
            session_epoch: SessionEpoch::new(1),
            last_session_sequence: SessionSequence::new(0),
            document_frontiers: Vec::new(),
            outstanding_mutation_ids: Vec::new(),
        };
        assert!(matches!(
            authority.resume(&resume),
            ResumeResult::Replay { ref events } if events.len() == 2
        ));
        authority.break_event_continuity().expect("compact event history");
        assert!(matches!(
            authority.resume(&resume),
            ResumeResult::SnapshotRequired {
                new_session_epoch,
                ref document_heads,
                ..
            } if new_session_epoch == SessionEpoch::new(2)
                && document_heads[0].revision == DocumentRevision::new(1)
        ));
        assert!(matches!(
            authority
                .submit(mutation(2, 2, 1, 0, "y"))
                .expect("rebase after event compaction"),
            Err(MutationResult::RebaseRequired {
                ref delta_since_base,
                ..
            }) if delta_since_base.len() == 1
        ));
        drop(authority);
        let recovered = open_authority(directory.path());
        assert_eq!(recovered.session_epoch(), SessionEpoch::new(2));
        assert_eq!(document_text(&recovered, 11), "xa");
    }

    #[test]
    fn client_state_checkpoint_is_durable_and_cannot_run_ahead_of_mutations() {
        let directory = tempdir().expect("temporary directory");
        let mut authority = authority_with_document(directory.path(), "a");
        let _ = authority.submit(mutation(1, 1, 1, 0, "x")).expect("mutation");
        let state = vec![StateDelta::SearchPattern("needle".into())];
        authority
            .checkpoint_state(StateCheckpoint { client_id: ClientId::new(7), through_client_sequence: ClientSequence::new(1), state: state.clone() })
            .expect("checkpoint");
        assert!(matches!(
            authority.checkpoint_state(StateCheckpoint { client_id: ClientId::new(7), through_client_sequence: ClientSequence::new(2), state: Vec::new() }),
            Err(AuthorityError::CheckpointAhead { .. })
        ));
        drop(authority);
        assert_eq!(open_authority(directory.path()).client_state(ClientId::new(7)), state);
    }

    #[test]
    fn bounded_event_retention_publishes_frontier_and_advances_epoch() {
        let directory = tempdir().expect("temporary directory");
        let mut authority = authority_with_document(directory.path(), "a");
        authority.set_event_retention_limit(1).expect("retention policy");
        let _ = authority.submit(mutation(1, 1, 1, 0, "x")).expect("mutation");
        assert_eq!(authority.session_epoch(), SessionEpoch::new(2));
        assert_eq!(authority.retained_after(), SessionSequence::new(2));
        assert!(authority.events_after(SessionSequence::new(0)).is_empty());
        assert_eq!(authority.event_retention_limit(), 1);

        drop(authority);
        let recovered = open_authority(directory.path());
        assert_eq!(recovered.session_epoch(), SessionEpoch::new(2));
        assert_eq!(recovered.retained_after(), SessionSequence::new(2));
    }
}
