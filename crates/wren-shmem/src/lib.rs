#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
#[cfg(any(test, feature = "benchmarking"))]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use mmap_io::{MemoryMappedFile, MmapMode};
use parking_lot::Mutex;

use thiserror::Error;
use wren_types::DocumentHead;
#[cfg(any(test, feature = "benchmarking"))]
use wren_types::{DocumentId, DocumentRevision, HeadValidation, ResumeViewState, SessionEpoch};

const MAGIC: u64 = u64::from_le_bytes(*b"WRENHEAD");
const VERSION: u64 = 2;
const MAX_CAPACITY: usize = 65_535;
const HEADER_WORDS: usize = 4;
const ENTRY_WORDS: usize = 4;
const MAGIC_WORD: usize = 0;
const VERSION_CAPACITY_WORD: usize = 1;
const GENERATION_WORD: usize = 2;
const COUNT_WORD: usize = 3;

#[derive(Debug, Error)]
pub enum SharedHeadError {
    #[error("shared head table I/O for {path} failed: {message}")]
    Io { path: PathBuf, message: Box<str> },
    #[error("shared head table {path} has invalid magic or version")]
    InvalidFormat { path: PathBuf },
    #[error("shared head table capacity must be in 1..={max}, received {actual}")]
    InvalidCapacity { actual: usize, max: usize },
    #[error("{actual} document heads exceed shared table capacity {capacity}")]
    Capacity { actual: usize, capacity: usize },
    #[error("shared head table generation overflow")]
    GenerationOverflow,
    #[error("shared head table {path} is still owned by a live session")]
    AlreadyActive { path: PathBuf },
}

pub struct SharedDocumentHeadWriter {
    path: PathBuf,
    capacity: usize,
    state: Mutex<WriterState>,
    owner: File,
}

#[cfg(any(test, feature = "benchmarking"))]
pub struct SharedDocumentHeadReader {
    path: PathBuf,
    capacity: usize,
    table: MemoryMappedFile,
}

struct WriterState {
    generation: u64,
    table: MemoryMappedFile,
}

impl fmt::Debug for SharedDocumentHeadWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("SharedDocumentHeadWriter").field("path", &self.path).field("capacity", &self.capacity).finish_non_exhaustive()
    }
}

#[cfg(any(test, feature = "benchmarking"))]
impl fmt::Debug for SharedDocumentHeadReader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("SharedDocumentHeadReader").field("path", &self.path).field("capacity", &self.capacity).finish_non_exhaustive()
    }
}

impl SharedDocumentHeadWriter {
    pub fn create(path: impl AsRef<Path>, capacity: usize) -> Result<Self, SharedHeadError> {
        Self::create_with_policy(path.as_ref(), capacity, true)
    }

    /// Reinitializes a stale table only after taking its process-lifetime
    /// ownership lock. A valid live table is never replaced.
    pub fn create_or_replace_stale(path: impl AsRef<Path>, capacity: usize) -> Result<Self, SharedHeadError> {
        Self::create_with_policy(path.as_ref(), capacity, false)
    }

    fn create_with_policy(path: &Path, capacity: usize, fail_if_exists: bool) -> Result<Self, SharedHeadError> {
        validate_capacity(capacity)?;
        let path = path.to_path_buf();
        create_parent(&path)?;
        let owner = acquire_owner(&path)?;
        let table = create_backing(&path, fail_if_exists, capacity)?;
        write_snapshot(&table, &path, capacity, 1, 2, &[])?;
        Ok(Self { path, capacity, state: Mutex::new(WriterState { generation: 2, table }), owner })
    }

    #[must_use]
    #[cfg(any(test, feature = "benchmarking"))]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    #[must_use]
    #[cfg(any(test, feature = "benchmarking"))]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn publish(&self, heads: &[DocumentHead]) -> Result<u64, SharedHeadError> {
        if heads.len() > self.capacity {
            return Err(SharedHeadError::Capacity { actual: heads.len(), capacity: self.capacity });
        }
        let mut state = self.state.lock();
        let writing = state.generation.checked_add(1).ok_or(SharedHeadError::GenerationOverflow)?;
        let complete = state.generation.checked_add(2).ok_or(SharedHeadError::GenerationOverflow)?;
        write_snapshot(&state.table, &self.path, self.capacity, writing, complete, heads)?;
        state.generation = complete;
        Ok(complete)
    }
}

impl Drop for SharedDocumentHeadWriter {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = self.owner.unlock();
    }
}

#[cfg(any(test, feature = "benchmarking"))]
impl SharedDocumentHeadReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SharedHeadError> {
        let path = path.as_ref().to_path_buf();
        let table = open_backing(&path)?;
        let (header, _) = read_snapshot(&table, &path)?;
        let capacity = header.capacity;
        Ok(Self { path, capacity, table })
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn snapshot(&self) -> Result<(u64, Vec<DocumentHead>), SharedHeadError> {
        let (header, heads) = read_snapshot(&self.table, &self.path)?;
        if header.capacity != self.capacity {
            return Err(SharedHeadError::InvalidFormat { path: self.path.clone() });
        }
        Ok((header.generation, heads))
    }

    pub fn validate(&self, session_epoch: SessionEpoch, state: &ResumeViewState) -> Result<HeadValidation, SharedHeadError> {
        let (_, heads) = self.snapshot()?;
        let Some(head) = heads.into_iter().find(|head| head.document_id == state.document_id) else {
            return Ok(HeadValidation::Unknown);
        };
        if head.session_epoch == session_epoch && head.authoritative_revision == state.document_revision {
            Ok(HeadValidation::Correct)
        } else {
            Ok(HeadValidation::Stale { authoritative: head })
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

const fn snapshot_bytes(entry_count: usize) -> usize {
    (HEADER_WORDS + entry_count * ENTRY_WORDS) * size_of::<u64>()
}

fn entry_offset(index: usize) -> usize {
    HEADER_WORDS + index * ENTRY_WORDS
}

fn pack_version_capacity(capacity: usize) -> u64 {
    VERSION | ((capacity as u64) << 32)
}

#[cfg(any(test, feature = "benchmarking"))]
fn unpack_version_capacity(word: u64) -> (u64, usize) {
    (word & u64::from(u32::MAX), (word >> 32) as usize)
}

fn validate_capacity(capacity: usize) -> Result<(), SharedHeadError> {
    if capacity == 0 || capacity > MAX_CAPACITY { Err(SharedHeadError::InvalidCapacity { actual: capacity, max: MAX_CAPACITY }) } else { Ok(()) }
}

#[cfg(any(test, feature = "benchmarking"))]
#[derive(Debug, Clone, Copy)]
struct SnapshotHeader {
    capacity: usize,
    generation: u64,
    count: usize,
}

#[cfg(any(test, feature = "benchmarking"))]
fn decode_header(words: &[AtomicU64], path: &Path, generation: u64) -> Result<SnapshotHeader, SharedHeadError> {
    if words[MAGIC_WORD].load(Ordering::Relaxed) != MAGIC {
        return Err(invalid_format(path));
    }
    let packed = words[VERSION_CAPACITY_WORD].load(Ordering::Relaxed);
    let (version, capacity) = unpack_version_capacity(packed);
    if version != VERSION || validate_capacity(capacity).is_err() {
        return Err(invalid_format(path));
    }
    let count = usize::try_from(words[COUNT_WORD].load(Ordering::Relaxed)).map_err(|_| invalid_format(path))?;
    if count > capacity || snapshot_bytes(capacity) != words.len() * size_of::<u64>() {
        return Err(invalid_format(path));
    }
    Ok(SnapshotHeader { capacity, generation, count })
}

#[cfg(any(test, feature = "benchmarking"))]
fn read_snapshot(file: &MemoryMappedFile, path: &Path) -> Result<(SnapshotHeader, Vec<DocumentHead>), SharedHeadError> {
    let file_length = file.len();
    if file_length < snapshot_bytes(1) as u64 || !file_length.is_multiple_of(size_of::<u64>() as u64) {
        return Err(invalid_format(path));
    }
    let word_count = usize::try_from(file_length / size_of::<u64>() as u64).map_err(|_| invalid_format(path))?;
    let words = file.atomic_u64_slice(0, word_count).map_err(|error| storage_error(path, error))?;
    for _ in 0..1_024 {
        let before = words[GENERATION_WORD].load(Ordering::Acquire);
        if !before.is_multiple_of(2) {
            std::thread::yield_now();
            continue;
        }
        let header = decode_header(&words, path, before)?;
        let heads = (0..header.count)
            .map(|index| {
                let offset = entry_offset(index);
                if words[offset + 3].load(Ordering::Relaxed) == 0 {
                    return Err(invalid_format(path));
                }
                Ok(DocumentHead {
                    session_epoch: SessionEpoch::new(words[offset].load(Ordering::Relaxed)),
                    document_id: DocumentId::new(words[offset + 1].load(Ordering::Relaxed)),
                    authoritative_revision: DocumentRevision::new(words[offset + 2].load(Ordering::Relaxed)),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let after = words[GENERATION_WORD].load(Ordering::Acquire);
        if before == after && after.is_multiple_of(2) {
            return Ok((header, heads));
        }
        std::thread::yield_now();
    }
    Err(storage_error(path, "shared-memory seqlock did not stabilize"))
}

fn create_backing(path: &Path, create_new: bool, capacity: usize) -> Result<MemoryMappedFile, SharedHeadError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    let file = options.open(path).map_err(|error| storage_error(path, error))?;
    file.set_len(snapshot_bytes(capacity) as u64).map_err(|error| storage_error(path, error))?;
    restrict_permissions(path)?;
    MemoryMappedFile::from_file(file, MmapMode::ReadWrite, path).map_err(|error| storage_error(path, error))
}

#[cfg(any(test, feature = "benchmarking"))]
fn open_backing(path: &Path) -> Result<MemoryMappedFile, SharedHeadError> {
    MemoryMappedFile::builder(path).mode(MmapMode::ReadOnly).open().map_err(|error| storage_error(path, error))
}

fn write_snapshot(
    file: &MemoryMappedFile,
    path: &Path,
    capacity: usize,
    writing_generation: u64,
    complete_generation: u64,
    heads: &[DocumentHead],
) -> Result<(), SharedHeadError> {
    let words = file.atomic_u64_slice(0, HEADER_WORDS + capacity * ENTRY_WORDS).map_err(|error| storage_error(path, error))?;
    words[GENERATION_WORD].store(writing_generation, Ordering::Release);
    words[MAGIC_WORD].store(MAGIC, Ordering::Relaxed);
    words[VERSION_CAPACITY_WORD].store(pack_version_capacity(capacity), Ordering::Relaxed);
    for (index, head) in heads.iter().enumerate() {
        let offset = entry_offset(index);
        for (word, value) in [head.session_epoch.get(), head.document_id.get(), head.authoritative_revision.get(), 1].into_iter().enumerate() {
            words[offset + word].store(value, Ordering::Relaxed);
        }
    }
    words[COUNT_WORD].store(heads.len() as u64, Ordering::Relaxed);
    words[GENERATION_WORD].store(complete_generation, Ordering::Release);
    Ok(())
}

fn create_parent(path: &Path) -> Result<(), SharedHeadError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| storage_error(path, error))?;
    }
    Ok(())
}

fn owner_path(path: &Path) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(".owner");
    PathBuf::from(value)
}

fn acquire_owner(path: &Path) -> Result<File, SharedHeadError> {
    let owner_path = owner_path(path);
    let owner = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&owner_path).map_err(|error| storage_error(&owner_path, error))?;
    restrict_permissions(&owner_path)?;
    match owner.try_lock() {
        Ok(()) => Ok(owner),
        Err(TryLockError::WouldBlock) => Err(SharedHeadError::AlreadyActive { path: path.to_path_buf() }),
        Err(TryLockError::Error(error)) => Err(storage_error(&owner_path, error)),
    }
}

#[cfg(any(test, feature = "benchmarking"))]
fn invalid_format(path: &Path) -> SharedHeadError {
    SharedHeadError::InvalidFormat { path: path.to_path_buf() }
}

fn storage_error(path: &Path, error: impl std::fmt::Display) -> SharedHeadError {
    SharedHeadError::Io { path: path.to_path_buf(), message: error.to_string().into() }
}

fn restrict_permissions(path: &Path) -> Result<(), SharedHeadError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| storage_error(path, error))
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::process::Command;
    use std::sync::Arc;
    use std::thread;

    use tempfile::tempdir;
    use wren_types::{ClientId, ConfigGeneration, SelRange, SelectionSet, ViewId};

    use super::*;

    fn state(revision: u64) -> ResumeViewState {
        ResumeViewState {
            client_id: ClientId::new(1),
            view_id: ViewId::new(2),
            document_id: DocumentId::new(3),
            document_revision: DocumentRevision::new(revision),
            selections: SelectionSet { primary: 0, ranges: vec![SelRange { anchor: 0, head: 0 }] },
            top_line: 0,
            rows: 40,
            columns: 120,
            config_generation: ConfigGeneration::new(1),
        }
    }

    #[test]
    fn shared_table_validates_correct_stale_and_unknown_frontiers() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("heads.link");
        let writer = SharedDocumentHeadWriter::create(&path, 8).expect("writer");
        writer
            .publish(&[DocumentHead { session_epoch: SessionEpoch::new(4), document_id: DocumentId::new(3), authoritative_revision: DocumentRevision::new(9) }])
            .expect("publish");
        let reader = SharedDocumentHeadReader::open(&path).expect("reader");
        assert_eq!(reader.validate(SessionEpoch::new(4), &state(9)).expect("correct"), HeadValidation::Correct);
        assert!(matches!(reader.validate(SessionEpoch::new(4), &state(8)).expect("stale"), HeadValidation::Stale { .. }));
        let mut unknown = state(9);
        unknown.document_id = DocumentId::new(99);
        assert_eq!(reader.validate(SessionEpoch::new(4), &unknown).expect("unknown"), HeadValidation::Unknown);
    }

    #[test]
    fn capacity_and_permissions_are_enforced() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("heads.link");
        let writer = SharedDocumentHeadWriter::create(&path, 1).expect("writer");
        assert!(matches!(
            writer.publish(&[
                DocumentHead { session_epoch: SessionEpoch::new(1), document_id: DocumentId::new(1), authoritative_revision: DocumentRevision::new(1) },
                DocumentHead { session_epoch: SessionEpoch::new(1), document_id: DocumentId::new(2), authoritative_revision: DocumentRevision::new(1) },
            ]),
            Err(SharedHeadError::Capacity { .. })
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).expect("metadata").permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn stale_tables_are_replaced_but_live_writers_are_preserved() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("heads.link");
        fs::write(&path, "stale-os-id").expect("stale link");
        let writer = SharedDocumentHeadWriter::create_or_replace_stale(&path, 2).expect("replace stale link");
        assert!(matches!(SharedDocumentHeadWriter::create_or_replace_stale(&path, 2), Err(SharedHeadError::AlreadyActive { .. })));
        assert_eq!(writer.capacity(), 2);
        drop(writer);
        SharedDocumentHeadWriter::create_or_replace_stale(&path, 3).expect("replace table after writer exit");
    }

    #[test]
    fn concurrent_snapshots_are_complete_and_internally_consistent() {
        const PUBLISH_COUNT: u64 = 100;
        let directory = tempdir().expect("directory");
        let path = directory.path().join("heads.link");
        let writer = Arc::new(SharedDocumentHeadWriter::create(&path, 2).expect("writer"));
        let reader = Arc::new(SharedDocumentHeadReader::open(&path).expect("reader"));
        let publishing_writer = Arc::clone(&writer);
        let publisher = thread::spawn(move || {
            for revision in 1..=PUBLISH_COUNT {
                publishing_writer
                    .publish(&[
                        DocumentHead {
                            session_epoch: SessionEpoch::new(revision),
                            document_id: DocumentId::new(revision),
                            authoritative_revision: DocumentRevision::new(revision),
                        },
                        DocumentHead {
                            session_epoch: SessionEpoch::new(revision),
                            document_id: DocumentId::new(revision + 1),
                            authoritative_revision: DocumentRevision::new(revision),
                        },
                    ])
                    .expect("publish");
            }
        });
        while !publisher.is_finished() {
            let (_, heads) = reader.snapshot().expect("complete snapshot");
            if let [first, second] = heads.as_slice() {
                assert_eq!(first.session_epoch, second.session_epoch);
                assert_eq!(first.authoritative_revision, second.authoritative_revision);
                assert_eq!(first.document_id.get() + 1, second.document_id.get());
            } else {
                assert!(heads.is_empty());
            }
        }
        publisher.join().expect("publisher");
        let (_, heads) = reader.snapshot().expect("final snapshot");
        assert_eq!(heads[0].authoritative_revision.get(), PUBLISH_COUNT);
    }

    #[test]
    fn oversized_tables_are_rejected_before_reading_their_contents() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("heads.link");
        let file = File::create(&path).expect("table");
        file.set_len(snapshot_bytes(MAX_CAPACITY) as u64 + 1).expect("oversized table");
        assert!(matches!(SharedDocumentHeadReader::open(&path), Err(SharedHeadError::InvalidFormat { .. })));
    }

    #[test]
    fn a_separate_process_reads_the_published_generation() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("heads.link");
        let output = directory.path().join("child-output");
        let writer = SharedDocumentHeadWriter::create(&path, 4).expect("writer");
        writer
            .publish(&[DocumentHead {
                session_epoch: SessionEpoch::new(5),
                document_id: DocumentId::new(3),
                authoritative_revision: DocumentRevision::new(11),
            }])
            .expect("publish");
        let status = Command::new(env::current_exe().expect("test executable"))
            .arg("--ignored")
            .arg("--exact")
            .arg("tests::shared_head_child_reader")
            .env("WREN_SHMEM_TEST_PATH", &path)
            .env("WREN_SHMEM_TEST_OUTPUT", &output)
            .status()
            .expect("child process");
        assert!(status.success());
        assert_eq!(fs::read_to_string(output).expect("child output"), "5:3:11");
    }

    #[test]
    #[ignore = "spawned only by a_separate_process_reads_the_published_generation"]
    fn shared_head_child_reader() {
        let path = env::var_os("WREN_SHMEM_TEST_PATH").expect("shared path");
        let output = env::var_os("WREN_SHMEM_TEST_OUTPUT").expect("output path");
        let reader = SharedDocumentHeadReader::open(path).expect("child reader");
        let (_, heads) = reader.snapshot().expect("child snapshot");
        let head = heads.first().expect("head");
        fs::write(output, format!("{}:{}:{}", head.session_epoch.get(), head.document_id.get(), head.authoritative_revision.get()))
            .expect("write child output");
    }
}
