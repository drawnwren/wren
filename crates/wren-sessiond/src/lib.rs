#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;
use wren_proto::{
    DEFAULT_MAX_FRAME_BYTES, Envelope, HelloAck, PROTOCOL_MAJOR, PROTOCOL_MINOR, ProtocolError,
    TransportError, envelope, read_envelope, write_envelope,
};
use wren_remote::{
    RemoteCall, RemoteError, RemoteOpened, RemoteReply, RemoteWorkspaceStorage, TransportLane,
};
use wren_session::{
    AuthorityError, LocalDocument, MutationSubmission, SaveError, SessionAuthority,
};
use wren_shmem::{SharedDocumentHeadWriter, SharedHeadError};

const CAPABILITIES: &[&str] = &[
    "mutation.v1",
    "resume.v1",
    "durability.received-durable.v1",
    "lease-fencing.v1",
    "session-events.v1",
    "bounded-retention.v1",
    "open-document.v1",
    "remote.manifest.v1",
    "remote.open.v1",
    "remote.blob.v1",
    "remote.search.v1",
    "remote.heartbeat.v1",
];

#[derive(Debug, Error)]
pub enum ServerError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Authority(#[from] AuthorityError),
    #[error("session authority lock is poisoned")]
    Poisoned,
    #[error("the first frame on a control connection must be Hello")]
    ExpectedHello,
    #[error("client protocol major {actual} is incompatible with server major {expected}")]
    IncompatibleMajor { actual: u32, expected: u32 },
    #[error("unexpected client payload {0}")]
    UnexpectedPayload(&'static str),
    #[error("fault injection stopped the session after Received and before journal commit")]
    InjectedCrashAfterReceived,
    #[error(transparent)]
    SharedHeads(#[from] SharedHeadError),
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error(transparent)]
    Save(#[from] SaveError),
    #[error("remote message encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error("remote document {0:?} has not been bound to a workspace path")]
    UnboundDocument(wren_types::DocumentId),
    #[error("save frontier {requested:?} does not match authoritative revision {actual:?}")]
    SaveFrontier {
        requested: wren_types::DocumentRevision,
        actual: wren_types::DocumentRevision,
    },
    #[error("save precondition does not match the opened file")]
    SavePrecondition,
    #[error("remote document {document_id:?} is already bound to {existing}, not {requested}")]
    PathBindingConflict {
        document_id: wren_types::DocumentId,
        existing: Box<str>,
        requested: Box<str>,
    },
    #[error("remote workspace state I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
struct RemoteWorkspace {
    lane: TransportLane,
    storage: RemoteWorkspaceStorage,
    documents: BTreeMap<wren_types::DocumentId, LocalDocument>,
    paths: BTreeMap<u64, Box<str>>,
    bindings_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SessionServer {
    authority: Arc<Mutex<SessionAuthority>>,
    max_frame_bytes: usize,
    crash_after_received: Arc<AtomicBool>,
    head_writer: Option<Arc<SharedDocumentHeadWriter>>,
    remote_workspace: Option<Arc<Mutex<RemoteWorkspace>>>,
}

impl SessionServer {
    #[must_use]
    pub fn new(authority: SessionAuthority) -> Self {
        Self {
            authority: Arc::new(Mutex::new(authority)),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            crash_after_received: Arc::new(AtomicBool::new(false)),
            head_writer: None,
            remote_workspace: None,
        }
    }

    #[must_use]
    pub fn with_max_frame_bytes(mut self, max_frame_bytes: usize) -> Self {
        self.max_frame_bytes = max_frame_bytes;
        self
    }

    pub fn with_head_writer(
        mut self,
        writer: Arc<SharedDocumentHeadWriter>,
    ) -> Result<Self, ServerError> {
        writer.publish(&self.authority()?.document_heads())?;
        self.head_writer = Some(writer);
        Ok(self)
    }

    pub fn with_remote_workspace(
        mut self,
        root: impl AsRef<Path>,
        cache_root: PathBuf,
        maximum_cache_bytes: u64,
        lane: TransportLane,
    ) -> Result<Self, ServerError> {
        let bindings_path = cache_root
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("document-bindings.json");
        let paths = match fs::read(&bindings_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
        self.remote_workspace = Some(Arc::new(Mutex::new(RemoteWorkspace {
            lane,
            storage: RemoteWorkspaceStorage::open(root, cache_root, maximum_cache_bytes)?,
            documents: BTreeMap::new(),
            paths,
            bindings_path,
        })));
        Ok(self)
    }

    pub fn authority(&self) -> Result<MutexGuard<'_, SessionAuthority>, ServerError> {
        self.authority.lock().map_err(|_| ServerError::Poisoned)
    }

    /// Arms a one-shot protocol fault used by the crash-recovery matrix. The
    /// next valid mutation receives its informational `Received`, then the
    /// connection terminates before authority submission or journal append.
    pub fn inject_crash_after_next_received(&self) {
        self.crash_after_received.store(true, Ordering::Release);
    }

    pub fn serve_connection<S>(&self, stream: &mut S) -> Result<(), ServerError>
    where
        S: Read + Write,
    {
        let negotiated_limit = self.negotiate(stream)?;
        while let Some(request) = read_envelope(stream, negotiated_limit)? {
            self.serve_request(stream, request, negotiated_limit)?;
        }
        Ok(())
    }

    fn negotiate<S>(&self, stream: &mut S) -> Result<usize, ServerError>
    where
        S: Read + Write,
    {
        let hello_envelope =
            read_envelope(stream, self.max_frame_bytes)?.ok_or(ServerError::ExpectedHello)?;
        let envelope::Payload::Hello(hello) = hello_envelope
            .payload
            .ok_or(ProtocolError::MissingField("Envelope.payload"))?
        else {
            return Err(ServerError::ExpectedHello);
        };
        if hello.major != PROTOCOL_MAJOR {
            return Err(ServerError::IncompatibleMajor {
                actual: hello.major,
                expected: PROTOCOL_MAJOR,
            });
        }
        let client_limit = usize::try_from(hello.max_frame_bytes).unwrap_or(usize::MAX);
        let negotiated_limit = self.max_frame_bytes.min(client_limit.max(1));
        let capabilities = CAPABILITIES
            .iter()
            .filter(|capability| hello.capabilities.iter().any(|value| value == **capability))
            .map(|value| (*value).to_owned())
            .collect();
        write_envelope(
            stream,
            &Envelope::new(
                hello_envelope.request_id,
                envelope::Payload::HelloAck(HelloAck {
                    major: PROTOCOL_MAJOR,
                    minor: PROTOCOL_MINOR,
                    capabilities,
                    max_frame_bytes: negotiated_limit as u64,
                }),
            ),
            negotiated_limit,
        )?;
        Ok(negotiated_limit)
    }

    fn serve_request(
        &self,
        stream: &mut (impl Read + Write),
        request: Envelope,
        limit: usize,
    ) -> Result<(), ServerError> {
        let request_id = request.request_id;
        match request
            .payload
            .ok_or(ProtocolError::MissingField("Envelope.payload"))?
        {
            envelope::Payload::ClientMutation(mutation) => {
                self.handle_mutation(stream, request_id, mutation.try_into()?, limit)
            }
            envelope::Payload::Resume(resume) => {
                self.handle_resume(stream, request_id, resume, limit)
            }
            envelope::Payload::OpenDocument(open) => {
                self.handle_open_document(stream, request_id, open, limit)
            }
            envelope::Payload::RemoteCall(call) => {
                let call: RemoteCall = serde_json::from_slice(&call.body)?;
                let reply = self.handle_remote_call(call);
                self.send_remote_reply(stream, request_id, reply, limit)
            }
            envelope::Payload::SaveRequest(request) => {
                let saved = self.save_remote_document(&request.try_into()?)?;
                self.write_payload(
                    stream,
                    request_id,
                    envelope::Payload::Saved(wren_proto::Saved::from(&saved)),
                    limit,
                )
            }
            envelope::Payload::Hello(_) => {
                Err(ServerError::UnexpectedPayload("Hello after handshake"))
            }
            envelope::Payload::HelloAck(_) => {
                Err(ServerError::UnexpectedPayload("HelloAck from client"))
            }
            envelope::Payload::MutationResult(_) => {
                Err(ServerError::UnexpectedPayload("MutationResult from client"))
            }
            envelope::Payload::SessionEvent(_) => {
                Err(ServerError::UnexpectedPayload("SessionEvent from client"))
            }
            envelope::Payload::ResumeResult(_) => {
                Err(ServerError::UnexpectedPayload("ResumeResult from client"))
            }
            envelope::Payload::Saved(_) => Err(ServerError::UnexpectedPayload("Saved from client")),
            envelope::Payload::DocumentOpened(_) => {
                Err(ServerError::UnexpectedPayload("DocumentOpened from client"))
            }
            envelope::Payload::RemoteReply(_) => {
                Err(ServerError::UnexpectedPayload("RemoteReply from client"))
            }
        }
    }

    fn handle_mutation(
        &self,
        stream: &mut impl Write,
        request_id: u64,
        mutation: wren_types::ClientMutation,
        limit: usize,
    ) -> Result<(), ServerError> {
        let previous_sequence = self.authority()?.session_sequence();
        self.send_result(
            stream,
            request_id,
            &wren_types::MutationResult::Received {
                mutation_id: mutation.mutation_id,
            },
            limit,
        )?;
        if self.crash_after_received.swap(false, Ordering::AcqRel) {
            return Err(ServerError::InjectedCrashAfterReceived);
        }
        let submission = self.authority()?.submit(mutation)?;
        if matches!(&submission, MutationSubmission::Accepted { .. }) {
            self.publish_heads()?;
        }
        match submission {
            MutationSubmission::Accepted { durable, .. } => {
                self.send_result(stream, request_id, &durable, limit)?;
                for event in self.authority()?.events_after(previous_sequence) {
                    self.write_payload(
                        stream,
                        0,
                        envelope::Payload::SessionEvent(wren_proto::SessionEvent::from(&event)),
                        limit,
                    )?;
                }
                Ok(())
            }
            MutationSubmission::Rejected(result) => {
                self.send_result(stream, request_id, &result, limit)
            }
        }
    }

    fn handle_resume(
        &self,
        stream: &mut impl Write,
        request_id: u64,
        resume: wren_proto::Resume,
        limit: usize,
    ) -> Result<(), ServerError> {
        let result = self.authority()?.resume(&resume.into());
        self.write_payload(
            stream,
            request_id,
            envelope::Payload::ResumeResult(wren_proto::ResumeResult::from(&result)),
            limit,
        )
    }

    fn handle_open_document(
        &self,
        stream: &mut impl Write,
        request_id: u64,
        open: wren_proto::OpenDocument,
        limit: usize,
    ) -> Result<(), ServerError> {
        let document_id = wren_types::DocumentId::new(open.document_id);
        let client_id = wren_types::ClientId::new(open.client_id);
        {
            let mut authority = self.authority()?;
            if authority.document(document_id).is_none() {
                authority.register_document(document_id, open.text, client_id)?;
            }
        }
        self.publish_heads()?;
        let authority = self.authority()?;
        let document = authority
            .document(document_id)
            .ok_or(AuthorityError::UnknownDocument(document_id))?;
        self.write_payload(
            stream,
            request_id,
            envelope::Payload::DocumentOpened(wren_proto::DocumentOpened {
                document_id: document_id.get(),
                revision: document.revision.get(),
                lease_epoch: document.lease.lease_epoch.get(),
                session_epoch: authority.session_epoch().get(),
            }),
            limit,
        )
    }

    fn write_payload(
        &self,
        stream: &mut impl Write,
        request_id: u64,
        payload: envelope::Payload,
        limit: usize,
    ) -> Result<(), ServerError> {
        write_envelope(stream, &Envelope::new(request_id, payload), limit)?;
        Ok(())
    }

    fn send_result(
        &self,
        stream: &mut impl Write,
        request_id: u64,
        result: &wren_types::MutationResult,
        limit: usize,
    ) -> Result<(), ServerError> {
        write_envelope(
            stream,
            &Envelope::new(
                request_id,
                envelope::Payload::MutationResult(wren_proto::MutationResult::from(result)),
            ),
            limit,
        )?;
        Ok(())
    }

    fn send_remote_reply(
        &self,
        stream: &mut impl Write,
        request_id: u64,
        reply: Result<RemoteReply, ServerError>,
        limit: usize,
    ) -> Result<(), ServerError> {
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => RemoteReply::Failure {
                message: error.to_string().into_boxed_str(),
            },
        };
        write_envelope(
            stream,
            &Envelope::new(
                request_id,
                envelope::Payload::RemoteReply(wren_proto::RemoteReply {
                    body: serde_json::to_vec(&reply)?,
                }),
            ),
            limit,
        )?;
        Ok(())
    }

    fn handle_remote_call(&self, call: RemoteCall) -> Result<RemoteReply, ServerError> {
        let workspace = self
            .remote_workspace
            .as_ref()
            .ok_or(ServerError::UnexpectedPayload(
                "RemoteCall on a local-only session",
            ))?;
        match call {
            RemoteCall::Heartbeat { nonce } => {
                let workspace = workspace.lock().map_err(|_| ServerError::Poisoned)?;
                if workspace.lane != TransportLane::Control {
                    return Err(ServerError::UnexpectedPayload(
                        "heartbeat request on the bulk lane",
                    ));
                }
                Ok(RemoteReply::Heartbeat { nonce })
            }
            RemoteCall::Manifest { generation } => {
                let workspace = workspace.lock().map_err(|_| ServerError::Poisoned)?;
                if workspace.lane != TransportLane::Control {
                    return Err(ServerError::UnexpectedPayload(
                        "manifest request on the bulk lane",
                    ));
                }
                Ok(RemoteReply::Manifest {
                    manifest: workspace.storage.manifest(generation)?,
                })
            }
            RemoteCall::Open {
                document_id,
                client_id,
                path,
                cached_hash,
            } => self.handle_remote_open(workspace, document_id, client_id, path, cached_hash),
            RemoteCall::Blob { hash } => {
                let workspace = workspace.lock().map_err(|_| ServerError::Poisoned)?;
                if workspace.lane != TransportLane::Bulk {
                    return Err(ServerError::UnexpectedPayload(
                        "blob request on the control lane",
                    ));
                }
                let bytes = workspace
                    .storage
                    .blob(hash)?
                    .ok_or(RemoteError::HashMismatch)?;
                Ok(RemoteReply::Blob { hash, bytes })
            }
            RemoteCall::Search {
                needle,
                maximum_results,
            } => {
                let workspace = workspace.lock().map_err(|_| ServerError::Poisoned)?;
                if workspace.lane != TransportLane::Bulk {
                    return Err(ServerError::UnexpectedPayload(
                        "search request on the control lane",
                    ));
                }
                Ok(RemoteReply::Search {
                    hits: workspace
                        .storage
                        .search(&needle, maximum_results.min(100_000))?,
                })
            }
        }
    }

    fn handle_remote_open(
        &self,
        workspace: &Arc<Mutex<RemoteWorkspace>>,
        document_id: wren_types::DocumentId,
        client_id: wren_types::ClientId,
        path: Box<str>,
        cached_hash: Option<[u8; 32]>,
    ) -> Result<RemoteReply, ServerError> {
        let (document, opened) = {
            let workspace = workspace.lock().map_err(|_| ServerError::Poisoned)?;
            if workspace.lane != TransportLane::Control {
                return Err(ServerError::UnexpectedPayload(
                    "open request on the bulk lane",
                ));
            }
            if let Some(existing) = workspace.paths.get(&document_id.get())
                && existing.as_ref() != path.as_ref()
            {
                return Err(ServerError::PathBindingConflict {
                    document_id,
                    existing: existing.clone(),
                    requested: path,
                });
            }
            LocalDocument::open_or_new(workspace.storage.workspace_path(&path)?)?
        };
        let (text, revision, lease_epoch, session_epoch) = {
            let mut authority = self.authority()?;
            if authority.document(document_id).is_none() {
                authority.register_document(document_id, opened.text.clone(), client_id)?;
            }
            let session_epoch = authority.session_epoch();
            let authoritative = authority
                .document(document_id)
                .ok_or(AuthorityError::UnknownDocument(document_id))?;
            (
                authoritative.text.clone(),
                authoritative.revision,
                authoritative.lease.lease_epoch,
                session_epoch,
            )
        };
        let file_identity =
            document
                .stamp()
                .map(remote_file_identity)
                .unwrap_or(wren_types::FileIdentity {
                    device: 0,
                    file: 0,
                    generation: 0,
                });
        let content_hash = {
            let mut workspace = workspace.lock().map_err(|_| ServerError::Poisoned)?;
            let content_hash = workspace.storage.cache_bytes(text.as_bytes())?;
            workspace.documents.insert(document_id, document);
            if workspace.paths.insert(document_id.get(), path).is_none() {
                persist_bindings(&workspace)?;
            }
            content_hash
        };
        self.publish_heads()?;
        Ok(RemoteReply::Opened {
            opened: RemoteOpened {
                document_id,
                revision,
                lease_epoch,
                session_epoch,
                content_hash,
                file_identity,
                size: u64::try_from(text.len()).unwrap_or(u64::MAX),
                cached_hash_valid: cached_hash == Some(content_hash),
                read_only: opened.read_only,
            },
        })
    }

    fn save_remote_document(
        &self,
        request: &wren_types::SaveRequest,
    ) -> Result<wren_types::Saved, ServerError> {
        let workspace = self
            .remote_workspace
            .as_ref()
            .ok_or(ServerError::UnexpectedPayload(
                "SaveRequest before workspace document binding",
            ))?;
        let (text, revision) = {
            let authority = self.authority()?;
            let document = authority
                .document(request.document_id)
                .ok_or(AuthorityError::UnknownDocument(request.document_id))?;
            (document.text.clone(), document.revision)
        };
        if revision != request.required_frontier {
            return Err(ServerError::SaveFrontier {
                requested: request.required_frontier,
                actual: revision,
            });
        }
        let mut workspace = workspace.lock().map_err(|_| ServerError::Poisoned)?;
        if workspace.lane != TransportLane::Control {
            return Err(ServerError::UnexpectedPayload(
                "SaveRequest on the bulk lane",
            ));
        }
        let document = workspace
            .documents
            .get_mut(&request.document_id)
            .ok_or(ServerError::UnboundDocument(request.document_id))?;
        let expected_identity =
            document
                .stamp()
                .map(remote_file_identity)
                .unwrap_or(wren_types::FileIdentity {
                    device: 0,
                    file: 0,
                    generation: 0,
                });
        if request.expected_content_hash != document.base_hash()
            || request.expected_file_identity != expected_identity
        {
            return Err(ServerError::SavePrecondition);
        }
        let report = document.save(&text)?;
        let new_content_hash = workspace.storage.cache_bytes(text.as_bytes())?;
        Ok(wren_types::Saved {
            document_id: request.document_id,
            persisted_frontier: revision,
            new_file_identity: remote_file_identity(&report.stamp),
            new_content_hash,
        })
    }

    fn publish_heads(&self) -> Result<(), ServerError> {
        if let Some(writer) = &self.head_writer {
            writer.publish(&self.authority()?.document_heads())?;
        }
        Ok(())
    }
}

fn remote_file_identity(stamp: &wren_session::FileStamp) -> wren_types::FileIdentity {
    wren_types::FileIdentity {
        device: stamp.identity.first,
        file: stamp.identity.second,
        generation: stamp.len,
    }
}

fn persist_bindings(workspace: &RemoteWorkspace) -> Result<(), ServerError> {
    let bytes = serde_json::to_vec(&workspace.paths)?;
    let temporary = workspace.bindings_path.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, &workspace.bindings_path)?;
    if let Some(parent) = workspace.bindings_path.parent() {
        OpenOptions::new().read(true).open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::thread;

    use tempfile::tempdir;
    use wren_proto::{
        ClientMutation as WireMutation, Hello, MutationResult as WireResult, OpenDocument,
    };
    use wren_session::SessionJournal;
    use wren_shmem::{SharedDocumentHeadReader, SharedDocumentHeadWriter};
    use wren_types::{
        ClientId, ClientMutation, ClientSequence, DocumentId, DocumentMutation, DocumentRevision,
        Edit, LeaseEpoch, MutationId, MutationResult, SemanticGroupId, SemanticGroupKind,
        SessionId, Transaction,
    };

    use super::*;

    fn mutation() -> ClientMutation {
        ClientMutation {
            mutation_id: MutationId::new(1),
            client_id: ClientId::new(7),
            client_sequence: ClientSequence::new(1),
            state_deltas: Vec::new(),
            documents: vec![DocumentMutation {
                document_id: DocumentId::new(9),
                lease_epoch: LeaseEpoch::new(1),
                base_revision: DocumentRevision::new(0),
                semantic_group_id: SemanticGroupId::new(1),
                semantic_group_kind: SemanticGroupKind::InsertRun,
                undo_parent: None,
                transactions: vec![
                    Transaction::new(DocumentRevision::new(0), vec![Edit::new(0..0, "socket ")])
                        .expect("transaction"),
                ],
            }],
        }
    }

    #[test]
    fn socket_round_trip_negotiates_then_emits_received_before_durable() {
        let directory = tempdir().expect("temporary directory");
        let journal = SessionJournal::in_directory(directory.path());
        let authority =
            SessionAuthority::open(journal.clone(), SessionId::new(1)).expect("authority");
        let head_path = directory.path().join("heads.link");
        let head_writer =
            Arc::new(SharedDocumentHeadWriter::create(&head_path, 8).expect("shared head writer"));
        let server = SessionServer::new(authority)
            .with_head_writer(head_writer)
            .expect("publish heads");
        let (mut client, mut daemon) = UnixStream::pair().expect("socket pair");
        let task_server = server.clone();
        let task = thread::spawn(move || task_server.serve_connection(&mut daemon));

        write_envelope(
            &mut client,
            &Envelope::new(
                10,
                envelope::Payload::Hello(Hello {
                    major: PROTOCOL_MAJOR,
                    minor: PROTOCOL_MINOR,
                    capabilities: CAPABILITIES
                        .iter()
                        .map(|value| (*value).to_owned())
                        .collect(),
                    max_frame_bytes: DEFAULT_MAX_FRAME_BYTES as u64,
                }),
            ),
            DEFAULT_MAX_FRAME_BYTES,
        )
        .expect("hello");
        assert!(matches!(
            read_envelope(&mut client, DEFAULT_MAX_FRAME_BYTES)
                .expect("ack")
                .expect("ack frame")
                .payload,
            Some(envelope::Payload::HelloAck(_))
        ));

        write_envelope(
            &mut client,
            &Envelope::new(
                10,
                envelope::Payload::OpenDocument(OpenDocument {
                    document_id: 9,
                    client_id: 7,
                    text: "document".to_owned(),
                }),
            ),
            DEFAULT_MAX_FRAME_BYTES,
        )
        .expect("open document");
        let opened = read_envelope(&mut client, DEFAULT_MAX_FRAME_BYTES)
            .expect("opened")
            .expect("opened frame");
        assert!(matches!(
            opened.payload,
            Some(envelope::Payload::DocumentOpened(ref opened))
                if opened.document_id == 9 && opened.lease_epoch == 1
        ));

        write_envelope(
            &mut client,
            &Envelope::new(
                11,
                envelope::Payload::ClientMutation(WireMutation::from(&mutation())),
            ),
            DEFAULT_MAX_FRAME_BYTES,
        )
        .expect("mutation");
        let first = read_envelope(&mut client, DEFAULT_MAX_FRAME_BYTES)
            .expect("received")
            .expect("received frame");
        let second = read_envelope(&mut client, DEFAULT_MAX_FRAME_BYTES)
            .expect("durable")
            .expect("durable frame");
        let Some(envelope::Payload::MutationResult(first)) = first.payload else {
            panic!("expected received result");
        };
        let Some(envelope::Payload::MutationResult(second)) = second.payload else {
            panic!("expected durable result");
        };
        assert!(matches!(
            MutationResult::try_from(first).expect("received conversion"),
            MutationResult::Received { .. }
        ));
        assert!(matches!(
            MutationResult::try_from(second).expect("durable conversion"),
            MutationResult::Durable { .. }
        ));
        let event = read_envelope(&mut client, DEFAULT_MAX_FRAME_BYTES)
            .expect("session event")
            .expect("session event frame");
        assert_eq!(event.request_id, 0);
        assert!(matches!(
            event.payload,
            Some(envelope::Payload::SessionEvent(_))
        ));
        drop(client);
        task.join()
            .expect("server thread")
            .expect("serve connection");

        let recovered = SessionAuthority::open(journal, SessionId::new(1)).expect("reopen");
        assert_eq!(
            recovered
                .document(DocumentId::new(9))
                .expect("document")
                .text,
            "socket document"
        );
        let (_, heads) = SharedDocumentHeadReader::open(&head_path)
            .expect("head reader")
            .snapshot()
            .expect("head snapshot");
        assert_eq!(heads[0].authoritative_revision, DocumentRevision::new(1));
        let _ = std::mem::size_of::<WireResult>();
    }

    #[test]
    fn crash_after_received_leaves_no_commit_and_retry_is_durable() {
        let directory = tempdir().expect("temporary directory");
        let journal = SessionJournal::in_directory(directory.path());
        let mut authority =
            SessionAuthority::open(journal.clone(), SessionId::new(1)).expect("authority");
        authority
            .register_document(DocumentId::new(9), "document", ClientId::new(7))
            .expect("document");
        let server = SessionServer::new(authority);
        server.inject_crash_after_next_received();
        let task_server = server.clone();
        let (mut client, mut daemon) = UnixStream::pair().expect("socket pair");
        let task = thread::spawn(move || task_server.serve_connection(&mut daemon));
        write_envelope(
            &mut client,
            &Envelope::new(
                1,
                envelope::Payload::Hello(Hello {
                    major: PROTOCOL_MAJOR,
                    minor: PROTOCOL_MINOR,
                    capabilities: CAPABILITIES
                        .iter()
                        .map(|value| (*value).to_owned())
                        .collect(),
                    max_frame_bytes: DEFAULT_MAX_FRAME_BYTES as u64,
                }),
            ),
            DEFAULT_MAX_FRAME_BYTES,
        )
        .expect("hello");
        read_envelope(&mut client, DEFAULT_MAX_FRAME_BYTES)
            .expect("hello ack")
            .expect("ack frame");
        write_envelope(
            &mut client,
            &Envelope::new(
                2,
                envelope::Payload::ClientMutation(WireMutation::from(&mutation())),
            ),
            DEFAULT_MAX_FRAME_BYTES,
        )
        .expect("mutation");
        let received = read_envelope(&mut client, DEFAULT_MAX_FRAME_BYTES)
            .expect("received")
            .expect("received frame");
        let Some(envelope::Payload::MutationResult(received)) = received.payload else {
            panic!("expected Received result");
        };
        assert!(matches!(
            MutationResult::try_from(received).expect("result"),
            MutationResult::Received { .. }
        ));
        drop(client);
        assert!(matches!(
            task.join().expect("server thread"),
            Err(ServerError::InjectedCrashAfterReceived)
        ));
        drop(server);

        let mut recovered =
            SessionAuthority::open(journal, SessionId::new(1)).expect("reopen authority");
        assert_eq!(
            recovered
                .document(DocumentId::new(9))
                .expect("document")
                .text,
            "document"
        );
        assert!(matches!(
            recovered.submit(mutation()).expect("retry"),
            MutationSubmission::Accepted {
                durable: MutationResult::Durable { .. },
                ..
            }
        ));
        assert_eq!(
            recovered
                .document(DocumentId::new(9))
                .expect("retried document")
                .text,
            "socket document"
        );
    }
}
