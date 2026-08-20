#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
#[cfg(any(test, feature = "client"))]
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
#[cfg(any(test, feature = "benchmarking"))]
use wren_types::RemoteOpenState;
use wren_types::{ClientId, DocumentId, DocumentRevision, FileIdentity, LeaseEpoch, SessionEpoch, WorkspaceGeneration};
#[cfg(any(test, feature = "client"))]
use wren_types::{ClientMutation, MutationResult, Resume, ResumeResult, SaveRequest, Saved};

pub const REMOTE_PROTOCOL_MAJOR: u16 = wren_proto::PROTOCOL_MAJOR as u16;
pub const REMOTE_PROTOCOL_MINOR: u16 = wren_proto::PROTOCOL_MINOR as u16;
#[cfg(any(test, feature = "client"))]
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum RemoteError {
    #[error("remote I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("remote protocol transport failed: {0}")]
    Protocol(#[from] wren_proto::TransportError),
    #[error("incompatible remote protocol major {remote}; local is {local}")]
    IncompatibleProtocol { local: u16, remote: u16 },
    #[error("remote frame length {0} exceeds the configured maximum")]
    FrameTooLarge(usize),
    #[error("cache content failed hash validation")]
    HashMismatch,
    #[error("remote path escapes the workspace namespace")]
    InvalidPath,
    #[error("reconciliation has overlapping local and remote changes")]
    ReconcileConflict,
    #[error("cached revision does not match the authoritative head")]
    StaleCache,
    #[error("remote message encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error("remote peer rejected the request: {0}")]
    Peer(Box<str>),
    #[error("remote peer returned an unexpected response")]
    UnexpectedResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportLane {
    Control,
    Bulk,
}

#[cfg(any(test, feature = "client"))]
impl TransportLane {
    const fn argument(self) -> &'static str {
        ["control", "bulk"][self as usize]
    }
}

#[cfg(any(test, feature = "client"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSshSpec {
    pub executable: PathBuf,
    pub host: Box<str>,
    pub user: Option<Box<str>>,
    pub port: Option<u16>,
    pub identity_file: Option<PathBuf>,
    pub extra_options: Vec<Box<str>>,
    pub remote_session_program: Box<str>,
    pub remote_workspace: Option<PathBuf>,
    pub remote_state_dir: Option<PathBuf>,
}

#[cfg(any(test, feature = "client"))]
impl OpenSshSpec {
    #[must_use]
    pub fn arguments(&self, lane: TransportLane) -> Vec<String> {
        let mut arguments = vec![
            "-T".to_owned(),
            "-o".to_owned(),
            "ClearAllForwardings=yes".to_owned(),
            "-o".to_owned(),
            "ServerAliveInterval=5".to_owned(),
            "-o".to_owned(),
            "ServerAliveCountMax=3".to_owned(),
            "-o".to_owned(),
            "TCPKeepAlive=yes".to_owned(),
        ];
        if let Some(port) = self.port {
            arguments.extend(["-p".to_owned(), port.to_string()]);
        }
        if let Some(identity) = &self.identity_file {
            arguments.extend(["-i".to_owned(), identity.to_string_lossy().into_owned()]);
        }
        for option in &self.extra_options {
            arguments.extend(["-o".to_owned(), option.to_string()]);
        }
        let target = self.user.as_ref().map_or_else(|| self.host.to_string(), |user| format!("{user}@{}", self.host));
        arguments.push("--".to_owned());
        arguments.push(target);
        arguments.push(shell_quote(&self.remote_session_program));
        arguments.extend([
            shell_quote("--transport"),
            shell_quote(lane.argument()),
            shell_quote("--protocol"),
            shell_quote(&format!("{REMOTE_PROTOCOL_MAJOR}.{REMOTE_PROTOCOL_MINOR}")),
        ]);
        if let Some(workspace) = &self.remote_workspace {
            arguments.extend([shell_quote("--workspace"), shell_quote(&workspace.to_string_lossy())]);
        }
        if let Some(state_dir) = &self.remote_state_dir {
            arguments.extend([shell_quote("--state-dir"), shell_quote(&state_dir.to_string_lossy())]);
        }
        arguments
    }
}

#[cfg(any(test, feature = "client"))]
fn shell_quote(value: &str) -> String {
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':')) {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(any(test, feature = "client"))]
pub struct OpenSshChannel {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
}

#[cfg(any(test, feature = "client"))]
impl OpenSshChannel {
    pub fn connect(spec: &OpenSshSpec, lane: TransportLane) -> Result<Self, RemoteError> {
        let mut child =
            Command::new(&spec.executable).args(spec.arguments(lane)).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn()?;
        let input = child.stdin.take().ok_or_else(|| io::Error::other("ssh stdin unavailable"))?;
        let output = child.stdout.take().ok_or_else(|| io::Error::other("ssh stdout unavailable"))?;
        Ok(Self { child, input, output })
    }

    pub fn send(&mut self, envelope: &wren_proto::Envelope) -> Result<(), RemoteError> {
        wren_proto::write_envelope(&mut self.input, envelope, MAX_FRAME_BYTES)?;
        Ok(())
    }

    pub fn receive(&mut self) -> Result<Option<wren_proto::Envelope>, RemoteError> {
        Ok(wren_proto::read_envelope(&mut self.output, MAX_FRAME_BYTES)?)
    }

    fn request<T>(
        &mut self,
        request_id: u64,
        payload: wren_proto::envelope::Payload,
        mut decode: impl FnMut(wren_proto::envelope::Payload) -> Result<Option<T>, RemoteError>,
    ) -> Result<T, RemoteError> {
        self.send(&wren_proto::Envelope::new(request_id, payload))?;
        loop {
            let envelope = self.receive()?.ok_or(RemoteError::UnexpectedResponse)?;
            if envelope.request_id == 0 && matches!(envelope.payload, Some(wren_proto::envelope::Payload::SessionEvent(_))) {
                continue;
            }
            if envelope.request_id != request_id {
                return Err(RemoteError::UnexpectedResponse);
            }
            let payload = envelope.payload.ok_or(RemoteError::UnexpectedResponse)?;
            if let Some(result) = decode(payload)? {
                return Ok(result);
            }
        }
    }

    pub fn handshake(&mut self, request_id: u64, capabilities: &RemoteCapabilities) -> Result<RemoteCapabilities, RemoteError> {
        let remote = self.request(
            request_id,
            wren_proto::envelope::Payload::Hello(wren_proto::Hello {
                major: u32::from(capabilities.protocol_major),
                minor: u32::from(capabilities.protocol_minor),
                capabilities: capabilities.features.iter().map(ToString::to_string).collect(),
                max_frame_bytes: MAX_FRAME_BYTES as u64,
            }),
            |payload| {
                let wren_proto::envelope::Payload::HelloAck(ack) = payload else {
                    return Err(RemoteError::UnexpectedResponse);
                };
                Ok(Some(RemoteCapabilities {
                    protocol_major: u16::try_from(ack.major).unwrap_or(u16::MAX),
                    protocol_minor: u16::try_from(ack.minor).unwrap_or(u16::MAX),
                    features: ack.capabilities.into_iter().map(String::into_boxed_str).collect(),
                }))
            },
        )?;
        capabilities.negotiate(&remote)
    }

    pub fn call(&mut self, request_id: u64, call: &RemoteCall) -> Result<RemoteReply, RemoteError> {
        let body = serde_json::to_vec(call)?;
        self.request(request_id, wren_proto::envelope::Payload::RemoteCall(wren_proto::RemoteCall { body }), |payload| {
            let wren_proto::envelope::Payload::RemoteReply(reply) = payload else {
                return Err(RemoteError::UnexpectedResponse);
            };
            let reply: RemoteReply = serde_json::from_slice(&reply.body)?;
            match reply {
                RemoteReply::Failure { message } => Err(RemoteError::Peer(message)),
                reply => Ok(Some(reply)),
            }
        })
    }

    pub fn close(mut self) -> Result<(), RemoteError> {
        drop(self.input);
        let _ = self.child.wait()?;
        Ok(())
    }
}

#[cfg(any(test, feature = "client"))]
pub struct DualOpenSshTransport {
    pub control: OpenSshChannel,
    pub bulk: OpenSshChannel,
}

#[cfg(any(test, feature = "client"))]
impl DualOpenSshTransport {
    pub fn connect(spec: &OpenSshSpec) -> Result<Self, RemoteError> {
        let mut control = OpenSshChannel::connect(spec, TransportLane::Control)?;
        match OpenSshChannel::connect(spec, TransportLane::Bulk) {
            Ok(bulk) => Ok(Self { control, bulk }),
            Err(error) => {
                let _ = control.child.kill();
                Err(error)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum RemoteCall {
    Heartbeat { nonce: u64 },
    Manifest { generation: WorkspaceGeneration },
    Open { document_id: DocumentId, client_id: ClientId, path: Box<str>, cached_hash: Option<[u8; 32]> },
    Blob { hash: [u8; 32] },
    Search { needle: Box<str>, maximum_results: usize },
}

impl RemoteCall {
    #[must_use]
    pub const fn lane(&self) -> TransportLane {
        match self {
            Self::Heartbeat { .. } | Self::Manifest { .. } | Self::Open { .. } => TransportLane::Control,
            Self::Blob { .. } | Self::Search { .. } => TransportLane::Bulk,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteOpened {
    pub document_id: DocumentId,
    pub revision: DocumentRevision,
    pub lease_epoch: LeaseEpoch,
    pub session_epoch: SessionEpoch,
    pub content_hash: [u8; 32],
    pub file_identity: FileIdentity,
    pub size: u64,
    pub cached_hash_valid: bool,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum RemoteReply {
    Heartbeat { nonce: u64 },
    Manifest { manifest: MerkleManifest },
    Opened { opened: RemoteOpened },
    Blob { hash: [u8; 32], bytes: Vec<u8> },
    Search { hits: Vec<SearchHit> },
    Failure { message: Box<str> },
}

pub const REMOTE_CAPABILITIES: &[&str] = &["remote.manifest.v1", "remote.open.v1", "remote.blob.v1", "remote.search.v1", "remote.heartbeat.v1"];

#[cfg(any(test, feature = "client"))]
pub struct RemoteWorkspaceClient {
    spec: OpenSshSpec,
    transport: DualOpenSshTransport,
    next_request_id: u64,
    pub negotiated_control: RemoteCapabilities,
    pub negotiated_bulk: RemoteCapabilities,
}

#[cfg(any(test, feature = "client"))]
impl RemoteWorkspaceClient {
    pub fn connect(spec: &OpenSshSpec) -> Result<Self, RemoteError> {
        let (transport, negotiated_control, negotiated_bulk) = Self::connect_transport(spec)?;
        Ok(Self { spec: spec.clone(), transport, next_request_id: 3, negotiated_control, negotiated_bulk })
    }

    fn connect_transport(spec: &OpenSshSpec) -> Result<(DualOpenSshTransport, RemoteCapabilities, RemoteCapabilities), RemoteError> {
        let mut transport = DualOpenSshTransport::connect(spec)?;
        let offered = RemoteCapabilities {
            protocol_major: REMOTE_PROTOCOL_MAJOR,
            protocol_minor: REMOTE_PROTOCOL_MINOR,
            features: REMOTE_CAPABILITIES.iter().map(|feature| Box::<str>::from(*feature)).collect(),
        };
        let negotiated_control = transport.control.handshake(1, &offered)?;
        let negotiated_bulk = transport.bulk.handshake(2, &offered)?;
        Ok((transport, negotiated_control, negotiated_bulk))
    }

    fn request_id(&mut self) -> u64 {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        request_id
    }

    fn call(&mut self, call: RemoteCall) -> Result<RemoteReply, RemoteError> {
        let request_id = self.request_id();
        match call.lane() {
            TransportLane::Control => self.transport.control.call(request_id, &call),
            TransportLane::Bulk => self.transport.bulk.call(request_id, &call),
        }
    }

    pub fn manifest(&mut self, generation: WorkspaceGeneration) -> Result<MerkleManifest, RemoteError> {
        match self.call(RemoteCall::Manifest { generation })? {
            RemoteReply::Manifest { manifest } => Ok(manifest),
            _ => Err(RemoteError::UnexpectedResponse),
        }
    }

    /// Round-trips an application-level heartbeat over the latency-sensitive
    /// control lane. OpenSSH keepalives detect a dead transport; this message
    /// additionally proves that the remote session loop is responsive.
    pub fn heartbeat(&mut self, nonce: u64) -> Result<(), RemoteError> {
        match self.call(RemoteCall::Heartbeat { nonce })? {
            RemoteReply::Heartbeat { nonce: returned_nonce } if returned_nonce == nonce => Ok(()),
            _ => Err(RemoteError::UnexpectedResponse),
        }
    }

    /// Re-establishes both SSH lanes and resumes causal session state using
    /// the caller's durable frontier and outstanding mutation identifiers.
    /// The old lanes remain installed until both replacements have completed
    /// protocol negotiation and the control lane has accepted `Resume`.
    pub fn reconnect(&mut self, resume: &Resume) -> Result<ResumeResult, RemoteError> {
        let (mut replacement, negotiated_control, negotiated_bulk) = Self::connect_transport(&self.spec)?;
        let request_id = self.request_id();
        let result = replacement.control.request(request_id, wren_proto::envelope::Payload::Resume(resume.clone()), |payload| {
            let wren_proto::envelope::Payload::ResumeResult(result) = payload else {
                return Err(RemoteError::UnexpectedResponse);
            };
            Ok(Some(result))
        })?;
        let previous = std::mem::replace(&mut self.transport, replacement);
        self.negotiated_control = negotiated_control;
        self.negotiated_bulk = negotiated_bulk;
        let _ = previous.control.close();
        let _ = previous.bulk.close();
        Ok(result)
    }

    pub fn open(
        &mut self,
        document_id: DocumentId,
        client_id: ClientId,
        path: impl Into<Box<str>>,
        cached_hash: Option<[u8; 32]>,
    ) -> Result<RemoteOpened, RemoteError> {
        match self.call(RemoteCall::Open { document_id, client_id, path: path.into(), cached_hash })? {
            RemoteReply::Opened { opened } => Ok(opened),
            _ => Err(RemoteError::UnexpectedResponse),
        }
    }

    pub fn blob(&mut self, hash: [u8; 32]) -> Result<Vec<u8>, RemoteError> {
        match self.call(RemoteCall::Blob { hash })? {
            RemoteReply::Blob { hash: returned_hash, bytes } if returned_hash == hash && blake3::hash(&bytes).as_bytes() == &hash => Ok(bytes),
            RemoteReply::Blob { .. } => Err(RemoteError::HashMismatch),
            _ => Err(RemoteError::UnexpectedResponse),
        }
    }

    pub fn search(&mut self, needle: impl Into<Box<str>>, maximum_results: usize) -> Result<Vec<SearchHit>, RemoteError> {
        match self.call(RemoteCall::Search { needle: needle.into(), maximum_results })? {
            RemoteReply::Search { hits } => Ok(hits),
            _ => Err(RemoteError::UnexpectedResponse),
        }
    }

    pub fn submit(&mut self, mutation: &ClientMutation) -> Result<MutationResult, RemoteError> {
        let request_id = self.request_id();
        self.transport.control.request(request_id, wren_proto::envelope::Payload::ClientMutation(mutation.clone()), |payload| {
            let wren_proto::envelope::Payload::MutationResult(result) = payload else {
                return Err(RemoteError::UnexpectedResponse);
            };
            Ok((!matches!(result, MutationResult::Received { .. })).then_some(result))
        })
    }

    pub fn save(&mut self, request: &SaveRequest) -> Result<Saved, RemoteError> {
        let request_id = self.request_id();
        self.transport.control.request(request_id, wren_proto::envelope::Payload::SaveRequest(request.clone()), |payload| {
            let wren_proto::envelope::Payload::Saved(saved) = payload else {
                return Err(RemoteError::UnexpectedResponse);
            };
            Ok(Some(saved))
        })
    }

    pub fn close(self) -> Result<(), RemoteError> {
        let DualOpenSshTransport { control, bulk } = self.transport;
        let control_result = control.close();
        let bulk_result = bulk.close();
        control_result.and(bulk_result)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteCapabilities {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub features: BTreeSet<Box<str>>,
}

impl RemoteCapabilities {
    pub fn negotiate(&self, remote: &Self) -> Result<Self, RemoteError> {
        if remote.protocol_major != self.protocol_major {
            return Err(RemoteError::IncompatibleProtocol { local: self.protocol_major, remote: remote.protocol_major });
        }
        Ok(Self {
            protocol_major: self.protocol_major,
            protocol_minor: self.protocol_minor.min(remote.protocol_minor),
            features: self.features.intersection(&remote.features).cloned().collect(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManifestEntryKind {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteIdentity {
    pub device: u64,
    pub inode: u64,
    pub modified_nanos: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: Box<str>,
    pub kind: ManifestEntryKind,
    pub mode: u32,
    pub symlink_target: Option<Box<str>>,
    pub size: u64,
    pub identity: RemoteIdentity,
    pub content_hash: Option<[u8; 32]>,
    pub tree_hash: Option<[u8; 32]>,
    pub generation: WorkspaceGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MerkleManifest {
    pub generation: WorkspaceGeneration,
    pub root_hash: [u8; 32],
    pub entries: BTreeMap<Box<str>, ManifestEntry>,
}

impl MerkleManifest {
    pub fn scan(root: &Path, generation: WorkspaceGeneration) -> Result<Self, RemoteError> {
        let canonical_root = root.canonicalize()?;
        let mut entries = BTreeMap::new();
        let root_hash = scan_directory(&canonical_root, &canonical_root, generation, &mut entries)?;
        Ok(Self { generation, root_hash, entries })
    }
}

fn scan_directory(
    root: &Path,
    directory: &Path,
    generation: WorkspaceGeneration,
    entries: &mut BTreeMap<Box<str>, ManifestEntry>,
) -> Result<[u8; 32], RemoteError> {
    let mut children = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(fs::DirEntry::file_name);
    let mut tree = blake3::Hasher::new();
    for child in children {
        let path = child.path();
        let metadata = fs::symlink_metadata(&path)?;
        let relative = path.strip_prefix(root).map_err(|_| RemoteError::InvalidPath)?;
        let key = normalized_relative(relative)?;
        let file_type = metadata.file_type();
        let (kind, symlink_target, content_hash, tree_hash) = match (file_type.is_symlink(), file_type.is_dir()) {
            (true, _) => {
                let target = fs::read_link(&path)?.to_string_lossy().into_owned().into_boxed_str();
                let hash = *blake3::hash(target.as_bytes()).as_bytes();
                (ManifestEntryKind::Symlink, Some(target), Some(hash), None)
            }
            (false, true) => {
                let hash = scan_directory(root, &path, generation, entries)?;
                (ManifestEntryKind::Directory, None, None, Some(hash))
            }
            (false, false) => {
                let hash = hash_file(&path)?;
                (ManifestEntryKind::File, None, Some(hash), None)
            }
        };
        tree.update(key.as_bytes());
        tree.update(&[kind as u8]);
        tree.update(content_hash.as_ref().or(tree_hash.as_ref()).unwrap_or(&[0; 32]));
        entries.insert(
            key.clone().into_boxed_str(),
            ManifestEntry {
                path: key.into_boxed_str(),
                kind,
                mode: metadata_mode(&metadata),
                symlink_target,
                size: metadata.len(),
                identity: metadata_identity(&metadata),
                content_hash,
                tree_hash,
                generation,
            },
        );
    }
    Ok(*tree.finalize().as_bytes())
}

fn normalized_relative(path: &Path) -> Result<String, RemoteError> {
    let components = path
        .components()
        .map(|component| match component {
            std::path::Component::Normal(value) => Ok(value.to_string_lossy().into_owned()),
            _ => Err(RemoteError::InvalidPath),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(components.join("/"))
}

fn hash_file(path: &Path) -> Result<[u8; 32], RemoteError> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(*hasher.finalize().as_bytes())
}

fn metadata_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.mode()
}

fn modified_nanos(metadata: &fs::Metadata) -> u64 {
    metadata.modified().ok().and_then(|time| time.duration_since(UNIX_EPOCH).ok()).map_or(0, |duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
}

fn metadata_identity(metadata: &fs::Metadata) -> RemoteIdentity {
    use std::os::unix::fs::MetadataExt;
    RemoteIdentity { device: metadata.dev(), inode: metadata.ino(), modified_nanos: modified_nanos(metadata) }
}

#[cfg(any(test, feature = "benchmarking"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentChunk {
    pub range: std::ops::Range<usize>,
    pub hash: [u8; 32],
}

#[cfg(any(test, feature = "benchmarking"))]
#[must_use]
pub fn fastcdc_chunks(bytes: &[u8], min: usize, average: usize, max: usize) -> Vec<ContentChunk> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let min = min.clamp(fastcdc::v2020::MINIMUM_MIN, fastcdc::v2020::MINIMUM_MAX);
    let average = average.max(min.next_power_of_two()).clamp(fastcdc::v2020::AVERAGE_MIN, fastcdc::v2020::AVERAGE_MAX);
    let max = max.max(average).clamp(fastcdc::v2020::MAXIMUM_MIN, fastcdc::v2020::MAXIMUM_MAX);
    fastcdc::v2020::FastCDC::new(bytes, min, average, max)
        .map(|chunk| {
            let range = chunk.offset..chunk.offset + chunk.length;
            ContentChunk { hash: *blake3::hash(&bytes[range.clone()]).as_bytes(), range }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct BlobCache {
    root: PathBuf,
    maximum_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct RemoteWorkspaceStorage {
    root: PathBuf,
    cache: BlobCache,
}

impl RemoteWorkspaceStorage {
    pub fn open(root: impl AsRef<Path>, cache_root: PathBuf, maximum_cache_bytes: u64) -> Result<Self, RemoteError> {
        let root = root.as_ref().canonicalize()?;
        if !root.is_dir() {
            return Err(RemoteError::InvalidPath);
        }
        Ok(Self { root, cache: BlobCache::open(cache_root, maximum_cache_bytes)? })
    }

    pub fn manifest(&self, generation: WorkspaceGeneration) -> Result<MerkleManifest, RemoteError> {
        MerkleManifest::scan(&self.root, generation)
    }

    pub fn workspace_path(&self, relative: &str) -> Result<PathBuf, RemoteError> {
        let relative_path = Path::new(relative);
        if relative_path.as_os_str().is_empty() || relative_path.is_absolute() {
            return Err(RemoteError::InvalidPath);
        }
        normalized_relative(relative_path)?;
        let candidate = self.root.join(relative_path);
        let checked = if candidate.exists() {
            candidate.canonicalize()?
        } else {
            let parent = candidate.parent().ok_or(RemoteError::InvalidPath)?;
            let parent = parent.canonicalize()?;
            parent.join(candidate.file_name().ok_or(RemoteError::InvalidPath)?)
        };
        if !checked.starts_with(&self.root) {
            return Err(RemoteError::InvalidPath);
        }
        Ok(checked)
    }

    pub fn cache_bytes(&self, bytes: &[u8]) -> Result<[u8; 32], RemoteError> {
        self.cache.put(bytes)
    }

    pub fn blob(&self, hash: [u8; 32]) -> Result<Option<Vec<u8>>, RemoteError> {
        self.cache.get(hash)
    }

    pub fn search(&self, needle: &str, maximum_results: usize) -> Result<Vec<SearchHit>, RemoteError> {
        if needle.is_empty() || maximum_results == 0 {
            return Ok(Vec::new());
        }
        let manifest = self.manifest(WorkspaceGeneration::new(0))?;
        let mut hits = Vec::new();
        for entry in manifest.entries.values() {
            if entry.kind != ManifestEntryKind::File {
                continue;
            }
            let bytes = fs::read(self.workspace_path(&entry.path)?)?;
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            for (line, line_text) in text.lines().enumerate() {
                for (column, matched) in line_text.match_indices(needle) {
                    hits.push(SearchHit { path: entry.path.clone(), line, byte_range: column..column + matched.len(), preview: line_text.into() });
                    if hits.len() >= maximum_results {
                        return Ok(hits);
                    }
                }
            }
        }
        Ok(hits)
    }
}

impl BlobCache {
    pub fn open(root: PathBuf, maximum_bytes: u64) -> Result<Self, RemoteError> {
        fs::create_dir_all(&root)?;
        set_private_directory(&root)?;
        Ok(Self { root, maximum_bytes })
    }

    pub fn put(&self, bytes: &[u8]) -> Result<[u8; 32], RemoteError> {
        let hash = *blake3::hash(bytes).as_bytes();
        let destination = self.path_for(hash);
        if !destination.exists() {
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_nanos());
            let temporary = self.root.join(format!(".tmp-{}-{nonce}", std::process::id()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
            let mut file = options.open(&temporary)?;
            file.write_all(bytes)?;
            // This content-addressed cache is reconstructible from the
            // authoritative workspace file. Closing before the atomic rename
            // makes it process-crash safe without adding a second durability
            // flush to the persisted-save critical path.
            drop(file);
            match fs::rename(&temporary, &destination) {
                Ok(()) => {}
                Err(_) if destination.exists() => {
                    fs::remove_file(&temporary)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        self.collect_garbage(Some(hash))?;
        Ok(hash)
    }

    pub fn get(&self, hash: [u8; 32]) -> Result<Option<Vec<u8>>, RemoteError> {
        let path = self.path_for(hash);
        let mut bytes = Vec::new();
        match File::open(&path) {
            Ok(mut file) => file.read_to_end(&mut bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if blake3::hash(&bytes).as_bytes() != &hash {
            return Err(RemoteError::HashMismatch);
        }
        touch(&path)?;
        Ok(Some(bytes))
    }

    pub fn collect_garbage(&self, protected: Option<[u8; 32]>) -> Result<u64, RemoteError> {
        let protected = protected.map(|hash| self.path_for(hash));
        let mut entries = Vec::new();
        let mut total = 0_u64;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.file_name().is_some_and(|name| name.to_string_lossy().starts_with('.')) {
                continue;
            }
            let metadata = entry.metadata()?;
            if metadata.is_file() {
                total = total.saturating_add(metadata.len());
                entries.push((metadata.modified().unwrap_or(UNIX_EPOCH), metadata.len(), path));
            }
        }
        entries.sort_by_key(|(modified, _, _)| *modified);
        for (_, size, path) in entries {
            if total <= self.maximum_bytes {
                break;
            }
            if protected.as_ref() == Some(&path) {
                continue;
            }
            fs::remove_file(path)?;
            total = total.saturating_sub(size);
        }
        Ok(total)
    }

    #[must_use]
    pub fn path_for(&self, hash: [u8; 32]) -> PathBuf {
        self.root.join(blake3::Hash::from_bytes(hash).to_hex().as_str())
    }
}

fn set_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn touch(path: &Path) -> io::Result<()> {
    filetime::set_file_mtime(path, filetime::FileTime::now())
}

#[cfg(test)]
pub fn reuse_git_index_blob(workspace: &Path, relative_path: &Path, expected_hash: [u8; 32]) -> Result<Option<Vec<u8>>, RemoteError> {
    let path = normalized_relative(relative_path)?;
    let output = Command::new("git").current_dir(workspace).args(["show", &format!(":./{path}")]).stderr(Stdio::null()).output()?;
    if !output.status.success() || blake3::hash(&output.stdout).as_bytes() != &expected_hash {
        return Ok(None);
    }
    Ok(Some(output.stdout))
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatFingerprint {
    pub identity: RemoteIdentity,
    pub size: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationReason {
    WatchSuspicion,
    ExplicitReload,
    SaveIdentityChanged,
    Reconnect,
    WatchOverflow,
    WatchGap,
    ManualRefresh,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoherenceDecision {
    Unchanged,
    StatChangedHashRequired,
    FullScanRequired,
}

#[cfg(test)]
#[derive(Debug, Default)]
pub struct CoherenceTracker {
    known: BTreeMap<Box<str>, StatFingerprint>,
    watch_sequence: u64,
}

#[cfg(test)]
impl CoherenceTracker {
    pub fn seed(&mut self, path: impl Into<Box<str>>, fingerprint: StatFingerprint) {
        self.known.insert(path.into(), fingerprint);
    }

    #[must_use]
    pub fn observe_watch(&mut self, sequence: u64, path: &str, current: StatFingerprint) -> CoherenceDecision {
        if self.watch_sequence != 0 && sequence != self.watch_sequence.saturating_add(1) {
            self.watch_sequence = sequence;
            return CoherenceDecision::FullScanRequired;
        }
        self.watch_sequence = sequence;
        match self.known.insert(path.into(), current) {
            Some(previous) if previous == current => CoherenceDecision::Unchanged,
            _ => CoherenceDecision::StatChangedHashRequired,
        }
    }

    #[must_use]
    pub const fn verification(_reason: VerificationReason) -> CoherenceDecision {
        CoherenceDecision::StatChangedHashRequired
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub path: Box<str>,
    pub line: usize,
    pub byte_range: std::ops::Range<usize>,
    pub preview: Box<str>,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyDocument {
    pub revision: DocumentRevision,
    pub bytes: Vec<u8>,
    pub symbols: Vec<Box<str>>,
}

#[cfg(test)]
#[derive(Debug, Default)]
pub struct DirtyOverlay {
    documents: BTreeMap<Box<str>, DirtyDocument>,
}

#[cfg(test)]
impl DirtyOverlay {
    pub fn update(&mut self, path: impl Into<Box<str>>, document: DirtyDocument) {
        self.documents.insert(path.into(), document);
    }

    pub fn remove(&mut self, path: &str) {
        self.documents.remove(path);
    }

    #[must_use]
    pub fn merge_search(&self, persisted: Vec<SearchHit>, needle: &str) -> Vec<SearchHit> {
        let dirty_paths = self.documents.keys().map(AsRef::as_ref).collect::<BTreeSet<_>>();
        let mut merged = persisted.into_iter().filter(|hit| !dirty_paths.contains(hit.path.as_ref())).collect::<Vec<_>>();
        for (path, document) in &self.documents {
            let text = String::from_utf8_lossy(&document.bytes);
            for (line, line_text) in text.lines().enumerate() {
                for (column, matched) in line_text.match_indices(needle) {
                    merged.push(SearchHit { path: path.clone(), line, byte_range: column..column + matched.len(), preview: line_text.into() });
                }
            }
        }
        merged.sort_by(|left, right| (left.path.as_ref(), left.line, left.byte_range.start).cmp(&(right.path.as_ref(), right.line, right.byte_range.start)));
        merged
    }

    #[must_use]
    pub fn merge_symbols(&self, persisted: Vec<(Box<str>, Box<str>)>) -> Vec<(Box<str>, Box<str>)> {
        let dirty_paths = self.documents.keys().map(AsRef::as_ref).collect::<BTreeSet<_>>();
        let mut merged = persisted.into_iter().filter(|(path, _)| !dirty_paths.contains(path.as_ref())).collect::<Vec<_>>();
        for (path, document) in &self.documents {
            merged.extend(document.symbols.iter().cloned().map(|symbol| (path.clone(), symbol)));
        }
        merged.sort();
        merged
    }
}

#[cfg(any(test, feature = "benchmarking"))]
#[derive(Debug, Clone)]
pub struct RemoteMaterializer {
    revision: DocumentRevision,
    expected_size: u64,
    expected_hash: [u8; 32],
    bytes: Vec<u8>,
}

#[cfg(any(test, feature = "benchmarking"))]
impl RemoteMaterializer {
    #[must_use]
    pub fn new(revision: DocumentRevision, expected_size: u64, expected_hash: [u8; 32]) -> Self {
        Self { revision, expected_size, expected_hash, bytes: Vec::new() }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<RemoteOpenState, RemoteError> {
        let next_length = self.bytes.len().saturating_add(chunk.len());
        if u64::try_from(next_length).unwrap_or(u64::MAX) > self.expected_size {
            return Err(RemoteError::FrameTooLarge(next_length));
        }
        self.bytes.extend_from_slice(chunk);
        Ok(self.state())
    }

    #[must_use]
    pub fn state(&self) -> RemoteOpenState {
        RemoteOpenState::Progressive {
            authoritative_revision: self.revision,
            received_bytes: u64::try_from(self.bytes.len()).unwrap_or(u64::MAX),
            total_bytes: self.expected_size,
        }
    }

    pub fn finish(self) -> Result<(RemoteOpenState, Vec<u8>), RemoteError> {
        if u64::try_from(self.bytes.len()).unwrap_or(u64::MAX) != self.expected_size || blake3::hash(&self.bytes).as_bytes() != &self.expected_hash {
            return Err(RemoteError::HashMismatch);
        }
        Ok((RemoteOpenState::Materialized { authoritative_revision: self.revision, content_hash: self.expected_hash }, self.bytes))
    }
}

#[cfg(test)]
pub fn validate_cached_open(cached_revision: DocumentRevision, authoritative_revision: Option<DocumentRevision>) -> Result<RemoteOpenState, RemoteError> {
    match authoritative_revision {
        Some(revision) if revision == cached_revision => Ok(RemoteOpenState::CachedHeadValidated { revision }),
        Some(_) => Err(RemoteError::StaleCache),
        None => Ok(RemoteOpenState::CachedAwaitingHead { cached_revision }),
    }
}

#[cfg(test)]
pub fn reconcile_three_way(base: &str, local: &str, remote: &str) -> Result<String, RemoteError> {
    let base_lines = base.split_inclusive('\n').collect::<Vec<_>>();
    let local_lines = local.split_inclusive('\n').collect::<Vec<_>>();
    let remote_lines = remote.split_inclusive('\n').collect::<Vec<_>>();
    let mut merged = String::new();
    for group in merge3::Merge3::new(&base_lines, &local_lines, &remote_lines).merge_groups() {
        let lines = match group {
            merge3::MergeGroup::Unchanged(lines) | merge3::MergeGroup::Same(lines) | merge3::MergeGroup::A(lines) | merge3::MergeGroup::B(lines) => lines,
            merge3::MergeGroup::Conflict(..) => return Err(RemoteError::ReconcileConflict),
        };
        merged.extend(lines.iter().copied());
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_uses_independent_control_and_bulk_commands() {
        let spec = OpenSshSpec {
            executable: "/usr/bin/ssh".into(),
            host: "host.example".into(),
            user: Some("wren".into()),
            port: Some(2222),
            identity_file: Some("/tmp/id".into()),
            extra_options: vec!["BatchMode=yes".into()],
            remote_session_program: "wren-sessiond".into(),
            remote_workspace: None,
            remote_state_dir: None,
        };
        let control = spec.arguments(TransportLane::Control);
        let bulk = spec.arguments(TransportLane::Bulk);
        assert_ne!(control, bulk);
        assert!(control.windows(2).any(|pair| pair == ["--transport", "control"]));
        assert!(bulk.windows(2).any(|pair| pair == ["--transport", "bulk"]));
        assert!(control.windows(2).any(|pair| pair == ["-o", "ServerAliveInterval=5"]));
        assert_eq!(shell_quote("plain/path"), "plain/path");
        assert_eq!(shell_quote("work dir/it's"), "'work dir/it'\\''s'");
    }

    #[test]
    fn capability_negotiation_intersects_minor_features() {
        let local = RemoteCapabilities { protocol_major: 1, protocol_minor: 2, features: ["manifest".into(), "fastcdc".into()].into_iter().collect() };
        let remote = RemoteCapabilities { protocol_major: 1, protocol_minor: 1, features: ["manifest".into(), "other".into()].into_iter().collect() };
        let negotiated = local.negotiate(&remote).expect("compatible");
        assert_eq!(negotiated.protocol_minor, 1);
        assert_eq!(negotiated.features, ["manifest".into()].into_iter().collect());
    }

    #[test]
    fn manifest_is_merkle_hashed_and_does_not_follow_symlinks() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::create_dir(directory.path().join("src")).expect("mkdir");
        fs::write(directory.path().join("src/main.rs"), "fn main() {}\n").expect("write");
        #[cfg(unix)]
        std::os::unix::fs::symlink("src/main.rs", directory.path().join("link")).expect("symlink");
        let first = MerkleManifest::scan(directory.path(), WorkspaceGeneration::new(1)).expect("manifest");
        assert!(first.entries.contains_key("src/main.rs"));
        #[cfg(unix)]
        assert_eq!(first.entries["link"].kind, ManifestEntryKind::Symlink);
        fs::write(directory.path().join("src/main.rs"), "fn changed() {}\n").expect("change");
        let second = MerkleManifest::scan(directory.path(), WorkspaceGeneration::new(2)).expect("manifest");
        assert_ne!(first.root_hash, second.root_hash);
    }

    #[test]
    fn chunking_and_private_lru_cache_are_content_addressed() {
        let bytes = vec![b'x'; 64 * 1024];
        let chunks = fastcdc_chunks(&bytes, 1_024, 4_096, 8_192);
        assert!(chunks.len() >= 8);
        assert_eq!(chunks.first().map(|chunk| chunk.range.start), Some(0));
        assert_eq!(chunks.last().map(|chunk| chunk.range.end), Some(bytes.len()));

        let directory = tempfile::tempdir().expect("tempdir");
        let cache = BlobCache::open(directory.path().join("cache"), 80 * 1_024).expect("cache");
        let hash = cache.put(&bytes).expect("put");
        assert_eq!(cache.get(hash).expect("get"), Some(bytes));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(cache.path_for(hash)).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn coherence_uses_watchers_as_hints_and_gaps_force_scans() {
        let fingerprint = StatFingerprint { identity: RemoteIdentity { device: 1, inode: 2, modified_nanos: 3 }, size: 4 };
        let mut tracker = CoherenceTracker::default();
        tracker.seed("a", fingerprint);
        assert_eq!(tracker.observe_watch(1, "a", fingerprint), CoherenceDecision::Unchanged);
        assert_eq!(tracker.observe_watch(3, "a", fingerprint), CoherenceDecision::FullScanRequired);
        assert_eq!(CoherenceTracker::verification(VerificationReason::Reconnect), CoherenceDecision::StatChangedHashRequired);
    }

    #[test]
    fn dirty_search_replaces_persisted_hits_for_the_same_path() {
        let mut overlay = DirtyOverlay::default();
        overlay
            .update("src/a.rs", DirtyDocument { revision: DocumentRevision::new(2), bytes: b"fresh needle\n".to_vec(), symbols: vec!["fresh_symbol".into()] });
        let hits = overlay.merge_search(vec![SearchHit { path: "src/a.rs".into(), line: 9, byte_range: 0..6, preview: "stale needle".into() }], "needle");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 0);
        assert_eq!(hits[0].preview.as_ref(), "fresh needle");
    }

    #[test]
    fn progressive_open_blocks_editing_until_hash_verified() {
        let bytes = b"authoritative";
        let mut materializer = RemoteMaterializer::new(DocumentRevision::new(7), bytes.len() as u64, *blake3::hash(bytes).as_bytes());
        let progressive = materializer.push(&bytes[..4]).expect("chunk");
        assert!(!progressive.editing_enabled());
        materializer.push(&bytes[4..]).expect("chunk");
        let (materialized, result) = materializer.finish().expect("finish");
        assert!(materialized.editing_enabled());
        assert_eq!(result, bytes);
    }

    #[test]
    fn reconnect_reconciliation_merges_disjoint_lines_and_rejects_conflicts() {
        let base = "one\ntwo\nthree\n";
        let local = "ONE\ntwo\nthree\n";
        let remote = "one\ntwo\nTHREE\n";
        assert_eq!(reconcile_three_way(base, local, remote).expect("merge"), "ONE\ntwo\nTHREE\n");
        assert!(reconcile_three_way(base, "one\nLOCAL\nthree\n", "one\nREMOTE\nthree\n").is_err());
    }
}
