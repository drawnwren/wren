#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tempfile::NamedTempFile;
use thiserror::Error;
use wren_shmem::{SharedDocumentHeadReader, SharedHeadError};
use wren_types::{
    Anchor, ClientId, DocumentId, DurableJumpEntry, HeadValidation, PublishedViewportKey, ResumeViewState, SemanticGroupId, SessionEpoch, StateDelta,
};
use wren_view::DesiredGrid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedViewport {
    pub session_epoch: SessionEpoch,
    pub document_id: DocumentId,
    pub key: PublishedViewportKey,
    pub grid: DesiredGrid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewportRestore {
    Correct(Arc<DesiredGrid>),
    Stale(HeadValidation),
    Missing,
}

const HISTORY_LIMIT: usize = 1_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableRegister {
    pub text: Box<str>,
    pub linewise: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableGlobalMark {
    pub document_id: DocumentId,
    pub anchor: Anchor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableMacroRecording {
    pub raw_keys: Vec<u8>,
    pub lowered_ir: Vec<u8>,
}

/// Materialized, compact client-owned state. It is intentionally separate
/// from document text and can be restored before a session reconnects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableClientState {
    pub client_id: ClientId,
    pub registers: BTreeMap<char, DurableRegister>,
    pub search_history: Vec<Box<str>>,
    #[serde(default)]
    pub search_backward: bool,
    pub command_history: Vec<Box<str>>,
    pub global_marks: BTreeMap<char, DurableGlobalMark>,
    pub undo_branch_heads: BTreeMap<DocumentId, Option<SemanticGroupId>>,
    pub repeat_data: Option<Vec<u8>>,
    #[serde(default)]
    pub macro_recordings: BTreeMap<char, DurableMacroRecording>,
    #[serde(default)]
    pub jump_list: Vec<DurableJumpEntry>,
    #[serde(default)]
    pub jump_index: Option<usize>,
}

impl DurableClientState {
    #[must_use]
    pub fn new(client_id: ClientId) -> Self {
        Self {
            client_id,
            registers: BTreeMap::new(),
            search_history: Vec::new(),
            search_backward: false,
            command_history: Vec::new(),
            global_marks: BTreeMap::new(),
            undo_branch_heads: BTreeMap::new(),
            repeat_data: None,
            macro_recordings: BTreeMap::new(),
            jump_list: Vec::new(),
            jump_index: None,
        }
    }

    pub fn apply(&mut self, delta: &StateDelta) {
        match delta {
            StateDelta::Register { name, text, linewise } => {
                self.registers.insert(*name, DurableRegister { text: text.clone(), linewise: *linewise });
            }
            StateDelta::SearchPattern(pattern) => {
                push_history(&mut self.search_history, pattern.clone());
            }
            StateDelta::SearchDirection { backward } => self.search_backward = *backward,
            StateDelta::CommandHistory(command) => {
                push_history(&mut self.command_history, command.clone());
            }
            StateDelta::GlobalMark { name, document_id, anchor } => {
                self.global_marks.insert(*name, DurableGlobalMark { document_id: *document_id, anchor: *anchor });
            }
            StateDelta::UndoBranchHead { document_id, semantic_group_id } => {
                self.undo_branch_heads.insert(*document_id, *semantic_group_id);
            }
            StateDelta::RepeatData(data) => self.repeat_data = Some(data.clone()),
            StateDelta::MacroRecording { name, raw_keys, lowered_ir } => {
                self.macro_recordings.insert(*name, DurableMacroRecording { raw_keys: raw_keys.clone(), lowered_ir: lowered_ir.clone() });
            }
            StateDelta::JumpList { entries, current } => {
                self.jump_list.clone_from(entries);
                self.jump_index = *current;
            }
        }
    }
}

fn push_history(history: &mut Vec<Box<str>>, entry: Box<str>) {
    if history.last() == Some(&entry) {
        return;
    }
    history.push(entry);
    if history.len() > HISTORY_LIMIT {
        history.drain(..history.len() - HISTORY_LIMIT);
    }
}

#[derive(Debug, Error)]
pub enum ClientStateError {
    #[error("client state operation for {path} failed: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("client state at {path} failed its checksum")]
    Checksum { path: PathBuf },
    #[error("client state serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    SharedHead(#[from] SharedHeadError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored<T> {
    checksum: [u8; 32],
    value: T,
}

#[derive(Debug, Deserialize)]
struct RawStored<'a> {
    checksum: [u8; 32],
    #[serde(borrow)]
    value: &'a RawValue,
}

#[derive(Debug, Clone)]
pub struct ClientViewStateStore {
    directory: PathBuf,
}

impl ClientViewStateStore {
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self { directory: directory.into() }
    }

    pub fn save_resume(&self, state: &ResumeViewState) -> Result<(), ClientStateError> {
        self.save(&self.resume_path(state.client_id.get()), state)
    }

    pub fn save_durable(&self, state: &DurableClientState) -> Result<(), ClientStateError> {
        self.save(&self.durable_path(state.client_id.get()), state)
    }

    pub fn load_durable(&self, client_id: ClientId) -> Result<Option<DurableClientState>, ClientStateError> {
        self.load(&self.durable_path(client_id.get()))
    }

    pub fn load_resume(&self, client_id: wren_types::ClientId) -> Result<Option<ResumeViewState>, ClientStateError> {
        self.load(&self.resume_path(client_id.get()))
    }

    pub fn save_viewport(&self, viewport: &PublishedViewport) -> Result<(), ClientStateError> {
        self.save(&self.viewport_path(viewport.key.client_id.get(), viewport.key.view_id.get(), viewport.document_id.get()), viewport)
    }

    pub fn load_viewport(
        &self,
        client_id: wren_types::ClientId,
        view_id: wren_types::ViewId,
        document_id: DocumentId,
    ) -> Result<Option<PublishedViewport>, ClientStateError> {
        self.load(&self.viewport_path(client_id.get(), view_id.get(), document_id.get()))
    }

    pub fn restore_correct_viewport(
        &self,
        expected_key: &PublishedViewportKey,
        resume: &ResumeViewState,
        session_epoch: SessionEpoch,
        heads: &SharedDocumentHeadReader,
    ) -> Result<ViewportRestore, ClientStateError> {
        let Some(viewport) = self.load_viewport(expected_key.client_id, expected_key.view_id, resume.document_id)? else {
            return Ok(ViewportRestore::Missing);
        };
        if viewport.key != *expected_key
            || viewport.session_epoch != session_epoch
            || viewport.document_id != resume.document_id
            || viewport.key.document_revision != resume.document_revision
            || viewport.grid.width != expected_key.columns
            || viewport.grid.height != expected_key.rows
        {
            return Ok(ViewportRestore::Stale(HeadValidation::Unknown));
        }
        let validation = heads.validate(session_epoch, resume)?;
        if validation == HeadValidation::Correct { Ok(ViewportRestore::Correct(Arc::new(viewport.grid))) } else { Ok(ViewportRestore::Stale(validation)) }
    }

    fn save<T: Serialize>(&self, path: &Path, value: &T) -> Result<(), ClientStateError> {
        fs::create_dir_all(&self.directory).map_err(|source| io_error(&self.directory, source))?;
        let value_bytes = serde_json::to_vec(value)?;
        let stored = Stored { checksum: *blake3::hash(&value_bytes).as_bytes(), value };
        let bytes = serde_json::to_vec(&stored)?;
        let mut temporary = NamedTempFile::new_in(&self.directory).map_err(|source| io_error(path, source))?;
        temporary.write_all(&bytes).and_then(|()| temporary.flush()).and_then(|()| temporary.as_file().sync_all()).map_err(|source| io_error(path, source))?;
        temporary.persist(path).map_err(|error| io_error(path, error.error))?;
        File::open(&self.directory).and_then(|directory| directory.sync_all()).map_err(|source| io_error(&self.directory, source))
    }

    fn load<T>(&self, path: &Path) -> Result<Option<T>, ClientStateError>
    where
        T: for<'de> Deserialize<'de>,
    {
        let mut bytes = Vec::new();
        match File::open(path) {
            Ok(mut file) => file.read_to_end(&mut bytes).map_err(|source| io_error(path, source))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(path, error)),
        };
        let stored: RawStored<'_> = serde_json::from_slice(&bytes)?;
        if blake3::hash(stored.value.get().as_bytes()).as_bytes() != &stored.checksum {
            return Err(ClientStateError::Checksum { path: path.to_path_buf() });
        }
        Ok(Some(serde_json::from_str(stored.value.get())?))
    }

    fn resume_path(&self, client: u64) -> PathBuf {
        self.directory.join(format!("resume-{client}.json"))
    }

    fn durable_path(&self, client: u64) -> PathBuf {
        self.directory.join(format!("durable-{client}.json"))
    }

    fn viewport_path(&self, client: u64, view: u64, document: u64) -> PathBuf {
        self.directory.join(format!("viewport-{client}-{view}-{document}.json"))
    }
}

fn io_error(path: &Path, source: io::Error) -> ClientStateError {
    ClientStateError::Io { path: path.to_path_buf(), source }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;
    use wren_shmem::SharedDocumentHeadWriter;
    use wren_types::{ClientId, ConfigGeneration, DocumentHead, DocumentRevision, SelRange, SelectionSet, ViewId};
    use wren_view::CellRow;

    use super::*;

    fn resume(revision: u64) -> ResumeViewState {
        ResumeViewState {
            client_id: ClientId::new(1),
            view_id: ViewId::new(2),
            document_id: DocumentId::new(3),
            document_revision: DocumentRevision::new(revision),
            selections: SelectionSet { primary: 0, ranges: vec![SelRange { anchor: 0, head: 0 }] },
            top_line: 0,
            rows: 4,
            columns: 20,
            config_generation: ConfigGeneration::new(5),
        }
    }

    fn key(revision: u64) -> PublishedViewportKey {
        PublishedViewportKey {
            client_id: ClientId::new(1),
            view_id: ViewId::new(2),
            document_revision: DocumentRevision::new(revision),
            rows: 4,
            columns: 20,
            theme_hash: [7; 32],
            config_generation: ConfigGeneration::new(5),
            renderer_version: 1,
        }
    }

    #[test]
    fn correct_frame_requires_full_key_and_shared_head_agreement() {
        let directory = tempdir().expect("directory");
        let store = ClientViewStateStore::new(directory.path().join("client"));
        let head_path = directory.path().join("heads.link");
        let writer = SharedDocumentHeadWriter::create(&head_path, 4).expect("writer");
        writer
            .publish(&[DocumentHead { session_epoch: SessionEpoch::new(1), document_id: DocumentId::new(3), authoritative_revision: DocumentRevision::new(9) }])
            .expect("head");
        let heads = SharedDocumentHeadReader::open(&head_path).expect("reader");
        let viewport = PublishedViewport {
            session_epoch: SessionEpoch::new(1),
            document_id: DocumentId::new(3),
            key: key(9),
            grid: DesiredGrid {
                epoch: 1,
                width: 20,
                height: 4,
                rows: (0..4).map(|_| Arc::new(CellRow::default())).collect(),
                cursor: (0, 0),
                raster_overlay: None,
            },
        };
        store.save_resume(&resume(9)).expect("save resume");
        store.save_viewport(&viewport).expect("save viewport");
        assert!(matches!(store.restore_correct_viewport(&key(9), &resume(9), SessionEpoch::new(1), &heads).expect("restore"), ViewportRestore::Correct(_)));
        let mut changed_theme = key(9);
        changed_theme.theme_hash = [8; 32];
        assert!(matches!(
            store.restore_correct_viewport(&changed_theme, &resume(9), SessionEpoch::new(1), &heads).expect("stale key"),
            ViewportRestore::Stale(_)
        ));
    }

    #[test]
    fn checksums_reject_corrupted_client_state() {
        let directory = tempdir().expect("directory");
        let store = ClientViewStateStore::new(directory.path());
        store.save_resume(&resume(9)).expect("save");
        let path = directory.path().join("resume-1.json");
        let mut bytes = fs::read(&path).expect("read");
        let middle = bytes.len() / 2;
        bytes[middle] ^= 1;
        fs::write(path, bytes).expect("corrupt");
        assert!(store.load_resume(ClientId::new(1)).is_err());
    }

    #[test]
    fn durable_state_materializes_histories_registers_marks_undo_and_repeat() {
        let directory = tempdir().expect("directory");
        let store = ClientViewStateStore::new(directory.path());
        let mut state = DurableClientState::new(ClientId::new(7));
        for delta in [
            StateDelta::Register { name: 'a', text: "value".into(), linewise: true },
            StateDelta::SearchPattern("needle".into()),
            StateDelta::SearchDirection { backward: true },
            StateDelta::CommandHistory("write".into()),
            StateDelta::GlobalMark { name: 'A', document_id: DocumentId::new(3), anchor: wren_types::Anchor { byte: 9, bias: wren_types::Bias::Right } },
            StateDelta::UndoBranchHead { document_id: DocumentId::new(3), semantic_group_id: Some(SemanticGroupId::new(11)) },
            StateDelta::RepeatData(vec![1, 2, 3]),
            StateDelta::MacroRecording { name: 'q', raw_keys: vec![4, 5], lowered_ir: vec![6, 7] },
            StateDelta::JumpList {
                entries: vec![wren_types::DurableJumpEntry {
                    document_id: DocumentId::new(3),
                    anchor: wren_types::Anchor { byte: 12, bias: wren_types::Bias::Right },
                    path_hint: Some("/workspace/main.rs".into()),
                }],
                current: Some(0),
            },
        ] {
            state.apply(&delta);
        }
        store.save_durable(&state).expect("save durable");
        assert_eq!(store.load_durable(ClientId::new(7)).expect("load durable"), Some(state));
    }

    #[test]
    fn durable_state_loads_after_adding_defaulted_fields() {
        #[derive(Serialize)]
        struct LegacyDurableClientState {
            client_id: ClientId,
            registers: BTreeMap<char, DurableRegister>,
            search_history: Vec<Box<str>>,
            command_history: Vec<Box<str>>,
            global_marks: BTreeMap<char, DurableGlobalMark>,
            undo_branch_heads: BTreeMap<DocumentId, Option<SemanticGroupId>>,
            repeat_data: Option<Vec<u8>>,
        }

        let directory = tempdir().expect("directory");
        let store = ClientViewStateStore::new(directory.path());
        let legacy = LegacyDurableClientState {
            client_id: ClientId::new(7),
            registers: BTreeMap::new(),
            search_history: vec!["needle".into()],
            command_history: Vec::new(),
            global_marks: BTreeMap::new(),
            undo_branch_heads: BTreeMap::new(),
            repeat_data: None,
        };
        store.save(&directory.path().join("durable-7.json"), &legacy).expect("save legacy durable state");

        let loaded = store.load_durable(ClientId::new(7)).expect("load legacy durable state").expect("durable state");
        assert_eq!(loaded.search_history, vec![Box::<str>::from("needle")]);
        assert!(!loaded.search_backward);
        assert!(loaded.macro_recordings.is_empty());
        assert!(loaded.jump_list.is_empty());
        assert_eq!(loaded.jump_index, None);
    }
}
