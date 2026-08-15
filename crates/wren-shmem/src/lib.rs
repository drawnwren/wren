#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

use mmap_io::{MemoryMappedFile, MmapMode};
#[cfg(unix)]
use nix::{
    fcntl::OFlag,
    sys::{
        mman::{shm_open, shm_unlink},
        stat::Mode,
    },
};

use thiserror::Error;
use wren_types::{
    DocumentHead, DocumentId, DocumentRevision, HeadValidation, ResumeViewState, SessionEpoch,
};

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
    #[error("shared head table writer lock was poisoned")]
    Poisoned,
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
    #[cfg(unix)]
    shared_memory_name: Box<str>,
}

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
        formatter
            .debug_struct("SharedDocumentHeadWriter")
            .field("path", &self.path)
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for SharedDocumentHeadReader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedDocumentHeadReader")
            .field("path", &self.path)
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl SharedDocumentHeadWriter {
    pub fn create(path: impl AsRef<Path>, capacity: usize) -> Result<Self, SharedHeadError> {
        validate_capacity(capacity)?;
        let path = path.as_ref().to_path_buf();
        create_parent(&path)?;
        let owner = acquire_owner(&path)?;
        let (table, shared_memory_name) = create_backing(&path, true, capacity)?;
        write_snapshot(&table, &path, &encode_snapshot(capacity, 2, &[]))?;
        Ok(Self {
            path,
            capacity,
            state: Mutex::new(WriterState {
                generation: 2,
                table,
            }),
            owner,
            #[cfg(unix)]
            shared_memory_name,
        })
    }

    /// Reinitializes a stale table only after taking its process-lifetime
    /// ownership lock. A valid live table is never replaced.
    pub fn create_or_replace_stale(
        path: impl AsRef<Path>,
        capacity: usize,
    ) -> Result<Self, SharedHeadError> {
        validate_capacity(capacity)?;
        let path = path.as_ref().to_path_buf();
        create_parent(&path)?;
        let owner = acquire_owner(&path)?;
        let (table, shared_memory_name) = create_backing(&path, false, capacity)?;
        write_snapshot(&table, &path, &encode_snapshot(capacity, 2, &[]))?;
        Ok(Self {
            path,
            capacity,
            state: Mutex::new(WriterState {
                generation: 2,
                table,
            }),
            owner,
            #[cfg(unix)]
            shared_memory_name,
        })
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn publish(&self, heads: &[DocumentHead]) -> Result<u64, SharedHeadError> {
        if heads.len() > self.capacity {
            return Err(SharedHeadError::Capacity {
                actual: heads.len(),
                capacity: self.capacity,
            });
        }
        let mut state = self.state.lock().map_err(|_| SharedHeadError::Poisoned)?;
        let writing = state
            .generation
            .checked_add(1)
            .ok_or(SharedHeadError::GenerationOverflow)?;
        let complete = state
            .generation
            .checked_add(2)
            .ok_or(SharedHeadError::GenerationOverflow)?;
        write_published_snapshot(
            &state.table,
            &self.path,
            &encode_snapshot(self.capacity, writing, heads),
            complete,
        )?;
        state.generation = complete;
        Ok(complete)
    }
}

impl Drop for SharedDocumentHeadWriter {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = shm_unlink(self.shared_memory_name.as_ref());
        let _ = self.owner.unlock();
    }
}

impl SharedDocumentHeadReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SharedHeadError> {
        let path = path.as_ref().to_path_buf();
        let table = open_backing(&path)?;
        let bytes = read_snapshot(&table, &path)?;
        let capacity = decode_header(&bytes, &path)?.capacity;
        Ok(Self {
            path,
            capacity,
            table,
        })
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn snapshot(&self) -> Result<(u64, Vec<DocumentHead>), SharedHeadError> {
        let bytes = read_snapshot(&self.table, &self.path)?;
        let header = decode_header(&bytes, &self.path)?;
        if header.capacity != self.capacity {
            return Err(SharedHeadError::InvalidFormat {
                path: self.path.clone(),
            });
        }
        let mut heads = Vec::with_capacity(header.count);
        for index in 0..header.count {
            let offset = entry_offset(index);
            if word_at(&bytes, offset + 3).is_none_or(|occupied| occupied == 0) {
                continue;
            }
            heads.push(DocumentHead {
                session_epoch: SessionEpoch::new(required_word(&bytes, offset, &self.path)?),
                document_id: DocumentId::new(required_word(&bytes, offset + 1, &self.path)?),
                authoritative_revision: DocumentRevision::new(required_word(
                    &bytes,
                    offset + 2,
                    &self.path,
                )?),
            });
        }
        Ok((header.generation, heads))
    }

    pub fn validate(
        &self,
        session_epoch: SessionEpoch,
        state: &ResumeViewState,
    ) -> Result<HeadValidation, SharedHeadError> {
        let (_, heads) = self.snapshot()?;
        let Some(head) = heads
            .into_iter()
            .find(|head| head.document_id == state.document_id)
        else {
            return Ok(HeadValidation::Unknown);
        };
        if head.session_epoch == session_epoch
            && head.authoritative_revision == state.document_revision
        {
            Ok(HeadValidation::Correct)
        } else {
            Ok(HeadValidation::Stale {
                authoritative: head,
            })
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

fn unpack_version_capacity(word: u64) -> (u64, usize) {
    (word & u64::from(u32::MAX), (word >> 32) as usize)
}

fn validate_capacity(capacity: usize) -> Result<(), SharedHeadError> {
    if capacity == 0 || capacity > MAX_CAPACITY {
        Err(SharedHeadError::InvalidCapacity {
            actual: capacity,
            max: MAX_CAPACITY,
        })
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct SnapshotHeader {
    capacity: usize,
    generation: u64,
    count: usize,
}

fn encode_snapshot(capacity: usize, generation: u64, heads: &[DocumentHead]) -> Vec<u8> {
    debug_assert!(capacity <= MAX_CAPACITY);
    debug_assert!(heads.len() <= capacity);
    let word_count = HEADER_WORDS + capacity * ENTRY_WORDS;
    let mut words = vec![0_u64; word_count];
    words[MAGIC_WORD] = MAGIC;
    words[VERSION_CAPACITY_WORD] = pack_version_capacity(capacity);
    words[GENERATION_WORD] = generation;
    words[COUNT_WORD] = heads.len() as u64;
    for (index, head) in heads.iter().enumerate() {
        let offset = entry_offset(index);
        words[offset] = head.session_epoch.get();
        words[offset + 1] = head.document_id.get();
        words[offset + 2] = head.authoritative_revision.get();
        words[offset + 3] = 1;
    }
    let byte_count = snapshot_bytes(capacity);
    let mut bytes = Vec::with_capacity(byte_count);
    for word in words {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    bytes
}

fn decode_header(bytes: &[u8], path: &Path) -> Result<SnapshotHeader, SharedHeadError> {
    if word_at(bytes, MAGIC_WORD) != Some(MAGIC) {
        return Err(invalid_format(path));
    }
    let packed = required_word(bytes, VERSION_CAPACITY_WORD, path)?;
    let (version, capacity) = unpack_version_capacity(packed);
    if version != VERSION || validate_capacity(capacity).is_err() {
        return Err(invalid_format(path));
    }
    let generation = required_word(bytes, GENERATION_WORD, path)?;
    if !generation.is_multiple_of(2) {
        return Err(invalid_format(path));
    }
    let count = usize::try_from(required_word(bytes, COUNT_WORD, path)?)
        .map_err(|_| invalid_format(path))?;
    if count > capacity {
        return Err(invalid_format(path));
    }
    if snapshot_bytes(capacity) != bytes.len() {
        return Err(invalid_format(path));
    }
    Ok(SnapshotHeader {
        capacity,
        generation,
        count,
    })
}

fn word_at(bytes: &[u8], index: usize) -> Option<u64> {
    let start = index.checked_mul(size_of::<u64>())?;
    let end = start.checked_add(size_of::<u64>())?;
    let word: [u8; 8] = bytes.get(start..end)?.try_into().ok()?;
    Some(u64::from_le_bytes(word))
}

fn required_word(bytes: &[u8], index: usize, path: &Path) -> Result<u64, SharedHeadError> {
    word_at(bytes, index).ok_or_else(|| invalid_format(path))
}

fn read_snapshot(file: &MemoryMappedFile, path: &Path) -> Result<Vec<u8>, SharedHeadError> {
    let file_length = file.len();
    if file_length < snapshot_bytes(1) as u64 {
        return Err(invalid_format(path));
    }
    let generation_offset = (GENERATION_WORD * size_of::<u64>()) as u64;
    for _ in 0..1_024 {
        let before = file
            .atomic_u64(generation_offset)
            .map_err(|error| storage_error(path, error))?
            .load(Ordering::Acquire);
        if !before.is_multiple_of(2) {
            std::thread::yield_now();
            continue;
        }
        let header = file
            .as_slice(0, (HEADER_WORDS * size_of::<u64>()) as u64)
            .map_err(|error| storage_error(path, error))?;
        if word_at(&header, MAGIC_WORD) != Some(MAGIC) {
            return Err(invalid_format(path));
        }
        let packed = word_at(&header, VERSION_CAPACITY_WORD).ok_or_else(|| invalid_format(path))?;
        let (version, capacity) = unpack_version_capacity(packed);
        if version != VERSION || validate_capacity(capacity).is_err() {
            return Err(invalid_format(path));
        }
        let snapshot_length = snapshot_bytes(capacity) as u64;
        if snapshot_length > file_length {
            return Err(invalid_format(path));
        }
        let bytes = file
            .as_slice(0, snapshot_length)
            .map_err(|error| storage_error(path, error))?
            .to_vec();
        let after = file
            .atomic_u64(generation_offset)
            .map_err(|error| storage_error(path, error))?
            .load(Ordering::Acquire);
        if before == after && after.is_multiple_of(2) {
            return Ok(bytes);
        }
        std::thread::yield_now();
    }
    Err(storage_error(
        path,
        "shared-memory seqlock did not stabilize",
    ))
}

#[cfg(unix)]
fn create_backing(
    path: &Path,
    create_new_locator: bool,
    capacity: usize,
) -> Result<(MemoryMappedFile, Box<str>), SharedHeadError> {
    static NEXT_NAME: AtomicU64 = AtomicU64::new(1);
    let mut last_error = None;
    for _ in 0..128 {
        let serial = NEXT_NAME.fetch_add(1, Ordering::Relaxed);
        let name = format!("/wren-heads-{}-{serial}", std::process::id());
        match shm_open(
            name.as_str(),
            OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_RDWR,
            Mode::S_IRUSR | Mode::S_IWUSR,
        ) {
            Ok(descriptor) => {
                let file = File::from(descriptor);
                file.set_len(snapshot_bytes(capacity) as u64)
                    .map_err(|error| storage_error(path, error))?;
                let mut locator = OpenOptions::new();
                locator.write(true);
                if create_new_locator {
                    locator.create_new(true);
                } else {
                    locator.create(true).truncate(true);
                }
                let write_locator = (|| {
                    let mut locator = locator
                        .open(path)
                        .map_err(|error| storage_error(path, error))?;
                    restrict_permissions(path)?;
                    locator
                        .write_all(name.as_bytes())
                        .map_err(|error| storage_error(path, error))?;
                    locator
                        .sync_all()
                        .map_err(|error| storage_error(path, error))
                })();
                if let Err(error) = write_locator {
                    let _ = shm_unlink(name.as_str());
                    return Err(error);
                }
                let mapping = MemoryMappedFile::from_file(file, MmapMode::ReadWrite, path)
                    .map_err(|error| storage_error(path, error))?;
                return Ok((mapping, name.into_boxed_str()));
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(storage_error(
        path,
        last_error.map_or_else(
            || "no shared-memory name available".to_owned(),
            |error| error.to_string(),
        ),
    ))
}

#[cfg(not(unix))]
fn create_backing(
    path: &Path,
    create_new: bool,
    capacity: usize,
) -> Result<(MemoryMappedFile, ()), SharedHeadError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    let table = options
        .open(path)
        .map_err(|error| storage_error(path, error))?;
    table
        .set_len(snapshot_bytes(capacity) as u64)
        .map_err(|error| storage_error(path, error))?;
    restrict_permissions(path)?;
    let mapping = MemoryMappedFile::from_file(table, MmapMode::ReadWrite, path)
        .map_err(|error| storage_error(path, error))?;
    Ok((mapping, ()))
}

#[cfg(unix)]
fn open_backing(path: &Path) -> Result<MemoryMappedFile, SharedHeadError> {
    let name = fs::read_to_string(path).map_err(|error| storage_error(path, error))?;
    if name.is_empty() || !name.starts_with('/') || name.contains('\n') || name.contains('\0') {
        return Err(invalid_format(path));
    }
    let descriptor = shm_open(name.as_str(), OFlag::O_RDONLY, Mode::empty())
        .map_err(|error| storage_error(path, error))?;
    MemoryMappedFile::from_file(File::from(descriptor), MmapMode::ReadOnly, path)
        .map_err(|error| storage_error(path, error))
}

#[cfg(not(unix))]
fn open_backing(path: &Path) -> Result<MemoryMappedFile, SharedHeadError> {
    MemoryMappedFile::open_ro(path).map_err(|error| storage_error(path, error))
}

fn write_snapshot(
    file: &MemoryMappedFile,
    path: &Path,
    bytes: &[u8],
) -> Result<(), SharedHeadError> {
    file.update_region(0, bytes)
        .map_err(|error| storage_error(path, error))
}

fn write_published_snapshot(
    file: &MemoryMappedFile,
    path: &Path,
    writing_bytes: &[u8],
    complete_generation: u64,
) -> Result<(), SharedHeadError> {
    let start = GENERATION_WORD * size_of::<u64>();
    file.atomic_u64(start as u64)
        .map_err(|error| storage_error(path, error))?
        .store(
            word_at(writing_bytes, GENERATION_WORD).ok_or_else(|| invalid_format(path))?,
            Ordering::Release,
        );
    file.update_region(0, &writing_bytes[..start])
        .map_err(|error| storage_error(path, error))?;
    file.update_region(
        (start + size_of::<u64>()) as u64,
        &writing_bytes[start + size_of::<u64>()..],
    )
    .map_err(|error| storage_error(path, error))?;
    file.atomic_u64(start as u64)
        .map_err(|error| storage_error(path, error))?
        .store(complete_generation, Ordering::Release);
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
    let owner = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&owner_path)
        .map_err(|error| storage_error(&owner_path, error))?;
    restrict_permissions(&owner_path)?;
    match owner.try_lock() {
        Ok(()) => Ok(owner),
        Err(TryLockError::WouldBlock) => Err(SharedHeadError::AlreadyActive {
            path: path.to_path_buf(),
        }),
        Err(TryLockError::Error(error)) => Err(storage_error(&owner_path, error)),
    }
}

fn invalid_format(path: &Path) -> SharedHeadError {
    SharedHeadError::InvalidFormat {
        path: path.to_path_buf(),
    }
}

fn storage_error(path: &Path, error: impl std::fmt::Display) -> SharedHeadError {
    SharedHeadError::Io {
        path: path.to_path_buf(),
        message: error.to_string().into(),
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<(), SharedHeadError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| storage_error(path, error))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<(), SharedHeadError> {
    Ok(())
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
            selections: SelectionSet {
                primary: 0,
                ranges: vec![SelRange { anchor: 0, head: 0 }],
            },
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
            .publish(&[DocumentHead {
                session_epoch: SessionEpoch::new(4),
                document_id: DocumentId::new(3),
                authoritative_revision: DocumentRevision::new(9),
            }])
            .expect("publish");
        let reader = SharedDocumentHeadReader::open(&path).expect("reader");
        assert_eq!(
            reader
                .validate(SessionEpoch::new(4), &state(9))
                .expect("correct"),
            HeadValidation::Correct
        );
        assert!(matches!(
            reader
                .validate(SessionEpoch::new(4), &state(8))
                .expect("stale"),
            HeadValidation::Stale { .. }
        ));
        let mut unknown = state(9);
        unknown.document_id = DocumentId::new(99);
        assert_eq!(
            reader
                .validate(SessionEpoch::new(4), &unknown)
                .expect("unknown"),
            HeadValidation::Unknown
        );
    }

    #[test]
    fn capacity_and_permissions_are_enforced() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("heads.link");
        let writer = SharedDocumentHeadWriter::create(&path, 1).expect("writer");
        assert!(matches!(
            writer.publish(&[
                DocumentHead {
                    session_epoch: SessionEpoch::new(1),
                    document_id: DocumentId::new(1),
                    authoritative_revision: DocumentRevision::new(1),
                },
                DocumentHead {
                    session_epoch: SessionEpoch::new(1),
                    document_id: DocumentId::new(2),
                    authoritative_revision: DocumentRevision::new(1),
                },
            ]),
            Err(SharedHeadError::Capacity { .. })
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn stale_tables_are_replaced_but_live_writers_are_preserved() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("heads.link");
        fs::write(&path, "stale-os-id").expect("stale link");
        let writer = SharedDocumentHeadWriter::create_or_replace_stale(&path, 2)
            .expect("replace stale link");
        assert!(matches!(
            SharedDocumentHeadWriter::create_or_replace_stale(&path, 2),
            Err(SharedHeadError::AlreadyActive { .. })
        ));
        assert_eq!(writer.capacity(), 2);
        drop(writer);
        SharedDocumentHeadWriter::create_or_replace_stale(&path, 3)
            .expect("replace table after writer exit");
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
        file.set_len(snapshot_bytes(MAX_CAPACITY) as u64 + 1)
            .expect("oversized table");
        assert!(matches!(
            SharedDocumentHeadReader::open(&path),
            Err(SharedHeadError::InvalidFormat { .. })
        ));
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
        fs::write(
            output,
            format!(
                "{}:{}:{}",
                head.session_epoch.get(),
                head.document_id.get(),
                head.authoritative_revision.get()
            ),
        )
        .expect("write child output");
    }
}
