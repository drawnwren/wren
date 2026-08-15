use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;
use wren_types::{
    DocumentId, DocumentRevision, ExpectedTarget, FileIdentity, PersistBatchId, ResourceOp,
    TransactionError, WorkspaceTransaction,
};

use crate::{LocalDocument, SaveError};

#[derive(Debug)]
pub struct WorkspaceDocument {
    pub document_id: DocumentId,
    pub path: PathBuf,
    pub text: String,
    pub revision: DocumentRevision,
    pub persisted_revision: DocumentRevision,
    local: LocalDocument,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistBatchState {
    Pending,
    Failed {
        completed_actions: usize,
        message: Box<str>,
    },
    Persisted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistBatchReport {
    pub batch_id: PersistBatchId,
    pub state: PersistBatchState,
    pub total_actions: usize,
    pub document_frontiers: Vec<(DocumentId, DocumentRevision)>,
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("workspace path {path} must be a relative path contained by the workspace")]
    UnsafePath { path: PathBuf },
    #[error("document {0:?} is already tracked")]
    DuplicateDocument(DocumentId),
    #[error("document {0:?} is not tracked")]
    UnknownDocument(DocumentId),
    #[error("document {document_id:?} occurs more than once in one workspace transaction")]
    DuplicateDocumentEdit { document_id: DocumentId },
    #[error(
        "document {document_id:?} expected revision {expected:?}, authoritative revision is {actual:?}"
    )]
    RevisionMismatch {
        document_id: DocumentId,
        expected: DocumentRevision,
        actual: DocumentRevision,
    },
    #[error("document {document_id:?} mutation is invalid: {reason}")]
    InvalidDocumentMutation {
        document_id: DocumentId,
        reason: Box<str>,
    },
    #[error("document {document_id:?} transaction failed validation: {source}")]
    Transaction {
        document_id: DocumentId,
        #[source]
        source: TransactionError,
    },
    #[error("resource precondition failed for {path}: {reason}")]
    ResourcePrecondition { path: PathBuf, reason: Box<str> },
    #[error("resource path {path} is already part of an unpersisted batch")]
    ResourceBusy { path: PathBuf },
    #[error("persist batch {0:?} does not exist")]
    UnknownBatch(PersistBatchId),
    #[error("workspace operation for {path} failed: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Save(#[from] SaveError),
    #[error("workspace counter overflow")]
    CounterOverflow,
}

#[derive(Debug, Clone)]
enum PersistAction {
    Document {
        document_id: DocumentId,
        frontier: DocumentRevision,
    },
    Resource(ResourceOp),
}

#[derive(Debug)]
struct PersistBatch {
    id: PersistBatchId,
    actions: Vec<PersistAction>,
    completed: usize,
    state: PersistBatchState,
    touched_paths: BTreeSet<PathBuf>,
    document_frontiers: Vec<(DocumentId, DocumentRevision)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VirtualResource {
    Existing(FileIdentity),
    PendingCreate,
}

/// Workspace-side all-or-nothing memory executor with explicit, retryable
/// best-effort persistence. Document text commits before disk I/O and is never
/// rolled back merely because one filesystem operation fails.
#[derive(Debug)]
pub struct WorkspaceExecutor {
    root: PathBuf,
    documents: BTreeMap<DocumentId, WorkspaceDocument>,
    batches: BTreeMap<PersistBatchId, PersistBatch>,
    busy_paths: BTreeSet<PathBuf>,
    next_batch: u64,
}

impl WorkspaceExecutor {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root).map_err(|source| io_error(&root, source))?;
        let root = fs::canonicalize(&root).map_err(|source| io_error(&root, source))?;
        Ok(Self {
            root,
            documents: BTreeMap::new(),
            batches: BTreeMap::new(),
            busy_paths: BTreeSet::new(),
            next_batch: 1,
        })
    }

    pub fn track_document(
        &mut self,
        document_id: DocumentId,
        relative_path: impl AsRef<Path>,
    ) -> Result<&WorkspaceDocument, WorkspaceError> {
        if self.documents.contains_key(&document_id) {
            return Err(WorkspaceError::DuplicateDocument(document_id));
        }
        let relative = normalize_relative(relative_path.as_ref())?;
        let absolute = self.root.join(&relative);
        let (local, opened) = LocalDocument::open_or_new(&absolute)?;
        self.documents.insert(
            document_id,
            WorkspaceDocument {
                document_id,
                path: relative,
                text: opened.text,
                revision: DocumentRevision::new(0),
                persisted_revision: DocumentRevision::new(0),
                local,
            },
        );
        self.documents
            .get(&document_id)
            .ok_or(WorkspaceError::UnknownDocument(document_id))
    }

    #[must_use]
    pub fn document(&self, document_id: DocumentId) -> Option<&WorkspaceDocument> {
        self.documents.get(&document_id)
    }

    pub fn apply(
        &mut self,
        transaction: &WorkspaceTransaction,
    ) -> Result<PersistBatchReport, WorkspaceError> {
        let mut staged_documents = Vec::with_capacity(transaction.document_edits.len());
        let mut document_ids = BTreeSet::new();
        for edit in &transaction.document_edits {
            if !document_ids.insert(edit.document_id) {
                return Err(WorkspaceError::DuplicateDocumentEdit {
                    document_id: edit.document_id,
                });
            }
            let document = self
                .documents
                .get(&edit.document_id)
                .ok_or(WorkspaceError::UnknownDocument(edit.document_id))?;
            if edit.base_revision != document.revision {
                return Err(WorkspaceError::RevisionMismatch {
                    document_id: edit.document_id,
                    expected: edit.base_revision,
                    actual: document.revision,
                });
            }
            edit.validate()
                .map_err(|error| WorkspaceError::InvalidDocumentMutation {
                    document_id: edit.document_id,
                    reason: error.to_string().into(),
                })?;
            let mut text = document.text.clone();
            let mut revision = document.revision;
            for semantic in &edit.transactions {
                semantic.validate_for_text(&text).map_err(|source| {
                    WorkspaceError::Transaction {
                        document_id: edit.document_id,
                        source,
                    }
                })?;
                text = semantic.apply_to_string(&text).map_err(|source| {
                    WorkspaceError::Transaction {
                        document_id: edit.document_id,
                        source,
                    }
                })?;
                revision = revision.next().ok_or(WorkspaceError::CounterOverflow)?;
            }
            staged_documents.push((edit.document_id, text, revision));
        }

        let (resource_paths, _) = self.validate_resource_ops(&transaction.resource_ops)?;
        for path in &resource_paths {
            if self.busy_paths.contains(path) {
                return Err(WorkspaceError::ResourceBusy { path: path.clone() });
            }
        }

        // No authoritative memory changes occur before every document and
        // resource precondition has passed.
        for (document_id, text, revision) in &staged_documents {
            let document = self
                .documents
                .get_mut(document_id)
                .ok_or(WorkspaceError::UnknownDocument(*document_id))?;
            document.text.clone_from(text);
            document.revision = *revision;
        }

        let id = PersistBatchId::new(self.next_batch);
        self.next_batch = self
            .next_batch
            .checked_add(1)
            .ok_or(WorkspaceError::CounterOverflow)?;
        let document_frontiers = staged_documents
            .iter()
            .map(|(document_id, _, revision)| (*document_id, *revision))
            .collect::<Vec<_>>();
        let mut actions = document_frontiers
            .iter()
            .map(|(document_id, frontier)| PersistAction::Document {
                document_id: *document_id,
                frontier: *frontier,
            })
            .collect::<Vec<_>>();
        actions.extend(
            transaction
                .resource_ops
                .iter()
                .cloned()
                .map(PersistAction::Resource),
        );
        self.busy_paths.extend(resource_paths.iter().cloned());
        let batch = PersistBatch {
            id,
            actions,
            completed: 0,
            state: PersistBatchState::Pending,
            touched_paths: resource_paths,
            document_frontiers,
        };
        let report = batch.report();
        self.batches.insert(id, batch);
        Ok(report)
    }

    pub fn persist(
        &mut self,
        batch_id: PersistBatchId,
    ) -> Result<PersistBatchReport, WorkspaceError> {
        let (start, actions) = {
            let batch = self
                .batches
                .get(&batch_id)
                .ok_or(WorkspaceError::UnknownBatch(batch_id))?;
            if batch.state == PersistBatchState::Persisted {
                return Ok(batch.report());
            }
            (batch.completed, batch.actions.clone())
        };

        for (index, action) in actions.into_iter().enumerate().skip(start) {
            if let Err(error) = self.persist_action(&action) {
                let message: Box<str> = error.to_string().into();
                let batch = self
                    .batches
                    .get_mut(&batch_id)
                    .ok_or(WorkspaceError::UnknownBatch(batch_id))?;
                batch.state = PersistBatchState::Failed {
                    completed_actions: index,
                    message,
                };
                batch.completed = index;
                return Ok(batch.report());
            }
            let batch = self
                .batches
                .get_mut(&batch_id)
                .ok_or(WorkspaceError::UnknownBatch(batch_id))?;
            batch.completed = index.saturating_add(1);
            batch.state = PersistBatchState::Pending;
        }

        let touched = {
            let batch = self
                .batches
                .get_mut(&batch_id)
                .ok_or(WorkspaceError::UnknownBatch(batch_id))?;
            batch.state = PersistBatchState::Persisted;
            batch.touched_paths.clone()
        };
        for path in touched {
            self.busy_paths.remove(&path);
        }
        self.batches
            .get(&batch_id)
            .map(PersistBatch::report)
            .ok_or(WorkspaceError::UnknownBatch(batch_id))
    }

    #[must_use]
    pub fn batch(&self, batch_id: PersistBatchId) -> Option<PersistBatchReport> {
        self.batches.get(&batch_id).map(PersistBatch::report)
    }

    fn persist_action(&mut self, action: &PersistAction) -> Result<(), WorkspaceError> {
        match action {
            PersistAction::Document {
                document_id,
                frontier,
            } => {
                let document = self
                    .documents
                    .get_mut(document_id)
                    .ok_or(WorkspaceError::UnknownDocument(*document_id))?;
                document.local.save(&document.text)?;
                document.persisted_revision = *frontier;
                Ok(())
            }
            PersistAction::Resource(resource) => self.persist_resource(resource),
        }
    }

    fn validate_resource_ops(
        &self,
        operations: &[ResourceOp],
    ) -> Result<(BTreeSet<PathBuf>, BTreeMap<PathBuf, VirtualResource>), WorkspaceError> {
        let mut touched = BTreeSet::new();
        let mut resources = BTreeMap::new();
        for operation in operations {
            for raw in operation_paths(operation) {
                let relative = normalize_relative(Path::new(raw))?;
                touched.insert(relative.clone());
                if let Entry::Vacant(entry) = resources.entry(relative.clone()) {
                    let absolute = self.root.join(&relative);
                    if let Some(identity) = identity_if_exists(&absolute)? {
                        entry.insert(VirtualResource::Existing(identity));
                    }
                }
            }
        }
        for operation in operations {
            match operation {
                ResourceOp::Create {
                    path,
                    expected_absent,
                } => {
                    let path = normalize_relative(Path::new(path.as_ref()))?;
                    if *expected_absent && resources.contains_key(&path) {
                        return Err(precondition(&path, "create target exists"));
                    }
                    resources.insert(path, VirtualResource::PendingCreate);
                }
                ResourceOp::Rename {
                    from,
                    to,
                    expected_source_identity,
                    expected_target,
                } => {
                    let from = normalize_relative(Path::new(from.as_ref()))?;
                    let to = normalize_relative(Path::new(to.as_ref()))?;
                    expect_identity(&resources, &from, expected_source_identity)?;
                    match expected_target {
                        ExpectedTarget::Absent if resources.contains_key(&to) => {
                            return Err(precondition(&to, "rename target exists"));
                        }
                        ExpectedTarget::Identity(identity) => {
                            expect_identity(&resources, &to, identity)?;
                        }
                        ExpectedTarget::Absent => {}
                    }
                    let source = resources
                        .remove(&from)
                        .ok_or_else(|| precondition(&from, "rename source is absent"))?;
                    resources.insert(to, source);
                }
                ResourceOp::Delete {
                    path,
                    expected_identity,
                } => {
                    let path = normalize_relative(Path::new(path.as_ref()))?;
                    expect_identity(&resources, &path, expected_identity)?;
                    resources.remove(&path);
                }
            }
        }
        Ok((touched, resources))
    }

    fn persist_resource(&self, operation: &ResourceOp) -> Result<(), WorkspaceError> {
        match operation {
            ResourceOp::Create {
                path,
                expected_absent,
            } => {
                let path = self.absolute(path)?;
                if *expected_absent && identity_if_exists(&path)?.is_some() {
                    return Err(precondition(
                        &path,
                        "create target appeared before persistence",
                    ));
                }
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
                }
                OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&path)
                    .and_then(|file| file.sync_all())
                    .map_err(|source| io_error(&path, source))?;
                sync_parent(&path)
            }
            ResourceOp::Rename {
                from,
                to,
                expected_source_identity,
                expected_target,
            } => {
                let from = self.absolute(from)?;
                let to = self.absolute(to)?;
                expect_path_identity(&from, expected_source_identity)?;
                match expected_target {
                    ExpectedTarget::Absent if identity_if_exists(&to)?.is_some() => {
                        return Err(precondition(
                            &to,
                            "rename target appeared before persistence",
                        ));
                    }
                    ExpectedTarget::Identity(identity) => expect_path_identity(&to, identity)?,
                    ExpectedTarget::Absent => {}
                }
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
                }
                fs::rename(&from, &to).map_err(|source| io_error(&from, source))?;
                sync_parent(&from)?;
                sync_parent(&to)
            }
            ResourceOp::Delete {
                path,
                expected_identity,
            } => {
                let path = self.absolute(path)?;
                expect_path_identity(&path, expected_identity)?;
                let metadata =
                    fs::symlink_metadata(&path).map_err(|source| io_error(&path, source))?;
                if metadata.file_type().is_dir() {
                    fs::remove_dir(&path).map_err(|source| io_error(&path, source))?;
                } else {
                    fs::remove_file(&path).map_err(|source| io_error(&path, source))?;
                }
                sync_parent(&path)
            }
        }
    }

    fn absolute(&self, raw: &str) -> Result<PathBuf, WorkspaceError> {
        normalize_relative(Path::new(raw)).map(|path| self.root.join(path))
    }
}

impl PersistBatch {
    fn report(&self) -> PersistBatchReport {
        PersistBatchReport {
            batch_id: self.id,
            state: self.state.clone(),
            total_actions: self.actions.len(),
            document_frontiers: self.document_frontiers.clone(),
        }
    }
}

fn operation_paths(operation: &ResourceOp) -> Vec<&str> {
    match operation {
        ResourceOp::Create { path, .. } | ResourceOp::Delete { path, .. } => {
            vec![path.as_ref()]
        }
        ResourceOp::Rename { from, to, .. } => vec![from.as_ref(), to.as_ref()],
    }
}

fn normalize_relative(path: &Path) -> Result<PathBuf, WorkspaceError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(WorkspaceError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => normalized.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(WorkspaceError::UnsafePath {
                    path: path.to_path_buf(),
                });
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(WorkspaceError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    Ok(normalized)
}

fn expect_identity(
    resources: &BTreeMap<PathBuf, VirtualResource>,
    path: &Path,
    expected: &FileIdentity,
) -> Result<(), WorkspaceError> {
    match resources.get(path) {
        Some(VirtualResource::Existing(actual)) if actual == expected => Ok(()),
        Some(VirtualResource::Existing(_)) => Err(precondition(path, "file identity changed")),
        Some(VirtualResource::PendingCreate) => Err(precondition(
            path,
            "newly-created resource has no established identity",
        )),
        None => Err(precondition(path, "resource is absent")),
    }
}

fn expect_path_identity(path: &Path, expected: &FileIdentity) -> Result<(), WorkspaceError> {
    match identity_if_exists(path)? {
        Some(actual) if &actual == expected => Ok(()),
        Some(_) => Err(precondition(path, "file identity changed")),
        None => Err(precondition(path, "resource is absent")),
    }
}

fn identity_if_exists(path: &Path) -> Result<Option<FileIdentity>, WorkspaceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(file_identity(&metadata))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error(path, source)),
    }
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        device: metadata.dev(),
        file: metadata.ino(),
        generation: 0,
    }
}

#[cfg(not(unix))]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::time::UNIX_EPOCH;
    let generation = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos() as u64);
    FileIdentity {
        device: 0,
        file: metadata.len(),
        generation,
    }
}

fn sync_parent(path: &Path) -> Result<(), WorkspaceError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error(parent, source))
}

fn precondition(path: &Path, reason: impl Into<Box<str>>) -> WorkspaceError {
    WorkspaceError::ResourcePrecondition {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

fn io_error(path: &Path, source: io::Error) -> WorkspaceError {
    WorkspaceError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;
    use wren_types::{
        DocumentMutation, Edit, LeaseEpoch, SemanticGroupId, SemanticGroupKind, Transaction,
    };

    use super::*;

    fn document_edit(id: DocumentId, base: u64, insert: &str) -> DocumentMutation {
        DocumentMutation {
            document_id: id,
            lease_epoch: LeaseEpoch::new(1),
            base_revision: DocumentRevision::new(base),
            semantic_group_id: SemanticGroupId::new(1),
            semantic_group_kind: SemanticGroupKind::WorkspaceRefactor,
            undo_parent: None,
            transactions: vec![
                Transaction::new(DocumentRevision::new(base), vec![Edit::new(0..0, insert)])
                    .expect("transaction"),
            ],
        }
    }

    fn identity(path: &Path) -> FileIdentity {
        file_identity(&fs::symlink_metadata(path).expect("metadata"))
    }

    #[test]
    fn validates_every_precondition_before_mutating_any_document() {
        let directory = tempdir().expect("workspace");
        fs::write(directory.path().join("a"), "a").expect("fixture");
        fs::write(directory.path().join("source"), "s").expect("fixture");
        let mut workspace = WorkspaceExecutor::new(directory.path()).expect("workspace");
        workspace
            .track_document(DocumentId::new(1), "a")
            .expect("track");
        let transaction = WorkspaceTransaction {
            document_edits: vec![document_edit(DocumentId::new(1), 0, "x")],
            resource_ops: vec![ResourceOp::Delete {
                path: "source".into(),
                expected_identity: FileIdentity {
                    device: 99,
                    file: 99,
                    generation: 99,
                },
            }],
        };
        assert!(matches!(
            workspace.apply(&transaction),
            Err(WorkspaceError::ResourcePrecondition { .. })
        ));
        let document = workspace.document(DocumentId::new(1)).expect("document");
        assert_eq!(document.text, "a");
        assert_eq!(document.revision, DocumentRevision::new(0));
    }

    #[test]
    fn commits_memory_atomically_then_persists_document_and_resource_ops() {
        let directory = tempdir().expect("workspace");
        let document_path = directory.path().join("a");
        let source = directory.path().join("source");
        fs::write(&document_path, "a").expect("fixture");
        fs::write(&source, "s").expect("fixture");
        let source_identity = identity(&source);
        let mut workspace = WorkspaceExecutor::new(directory.path()).expect("workspace");
        workspace
            .track_document(DocumentId::new(1), "a")
            .expect("track");
        let report = workspace
            .apply(&WorkspaceTransaction {
                document_edits: vec![document_edit(DocumentId::new(1), 0, "x")],
                resource_ops: vec![ResourceOp::Rename {
                    from: "source".into(),
                    to: "renamed".into(),
                    expected_source_identity: source_identity,
                    expected_target: ExpectedTarget::Absent,
                }],
            })
            .expect("apply");
        assert_eq!(
            fs::read_to_string(&document_path).expect("disk before"),
            "a"
        );
        assert_eq!(
            workspace.document(DocumentId::new(1)).expect("memory").text,
            "xa"
        );
        let persisted = workspace.persist(report.batch_id).expect("persist");
        assert_eq!(persisted.state, PersistBatchState::Persisted);
        assert_eq!(fs::read_to_string(document_path).expect("disk after"), "xa");
        assert_eq!(
            fs::read_to_string(directory.path().join("renamed")).expect("renamed"),
            "s"
        );
    }

    #[test]
    fn partial_persist_failure_is_marked_and_retry_keeps_memory_state() {
        let directory = tempdir().expect("workspace");
        fs::write(directory.path().join("a"), "a").expect("fixture");
        let doomed = directory.path().join("doomed");
        fs::write(&doomed, "d").expect("fixture");
        let doomed_identity = identity(&doomed);
        let mut workspace = WorkspaceExecutor::new(directory.path()).expect("workspace");
        workspace
            .track_document(DocumentId::new(1), "a")
            .expect("track");
        let report = workspace
            .apply(&WorkspaceTransaction {
                document_edits: vec![document_edit(DocumentId::new(1), 0, "x")],
                resource_ops: vec![ResourceOp::Delete {
                    path: "doomed".into(),
                    expected_identity: doomed_identity,
                }],
            })
            .expect("apply");
        fs::remove_file(&doomed).expect("external race");
        let failed = workspace
            .persist(report.batch_id)
            .expect("reported failure");
        assert!(matches!(
            failed.state,
            PersistBatchState::Failed {
                completed_actions: 1,
                ..
            }
        ));
        assert_eq!(
            workspace.document(DocumentId::new(1)).expect("memory").text,
            "xa"
        );
        fs::write(&doomed, "d").expect("restore path");
        // Restoring the content creates a different identity. The retry must
        // stay failed instead of deleting an unrelated replacement.
        let retry = workspace.persist(report.batch_id).expect("retry report");
        assert!(matches!(retry.state, PersistBatchState::Failed { .. }));
        assert_eq!(fs::read_to_string(doomed).expect("replacement"), "d");
    }

    #[test]
    fn rejects_paths_that_escape_the_workspace() {
        let directory = tempdir().expect("workspace");
        let mut workspace = WorkspaceExecutor::new(directory.path()).expect("workspace");
        assert!(matches!(
            workspace.apply(&WorkspaceTransaction {
                document_edits: Vec::new(),
                resource_ops: vec![ResourceOp::Create {
                    path: "../escape".into(),
                    expected_absent: true,
                }],
            }),
            Err(WorkspaceError::UnsafePath { .. })
        ));
    }
}
