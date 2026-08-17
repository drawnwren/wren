use super::*;

pub(super) fn restore_client_state(
    buffer: &mut BufferState,
    state: &DurableClientState,
) -> Result<()> {
    for (name, register) in &state.registers {
        buffer
            .editor
            .restore_register(*name, register.text.clone(), register.linewise);
    }
    if let Some(pattern) = state.search_history.last() {
        buffer.editor.restore_search(
            pattern.clone(),
            if state.search_backward {
                SearchDirection::Backward
            } else {
                SearchDirection::Forward
            },
        )?;
    }
    for (name, mark) in &state.global_marks {
        if mark.document_id == buffer.document_id {
            buffer.editor.restore_mark(*name, mark.anchor.byte);
        }
    }
    if let Some(repeat) = &state.repeat_data {
        buffer.editor.restore_repeat_data(repeat)?;
    }
    for (name, recording) in &state.macro_recordings {
        let keys: Vec<KeyEvent> = serde_json::from_slice(&recording.raw_keys)
            .with_context(|| format!("restore macro {name}"))?;
        buffer.editor.restore_macro(*name, keys);
    }
    Ok(())
}

pub(super) fn sync_client_state(
    active: &mut BufferState,
    inactive: &mut [BufferState],
    state: &DurableClientState,
) -> Result<()> {
    restore_client_state(active, state)?;
    for buffer in inactive {
        restore_client_state(buffer, state)?;
    }
    Ok(())
}

pub(super) enum ClientStateMessage {
    Save(Box<DurableClientState>),
    Barrier {
        state: Box<DurableClientState>,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Stop,
}

pub(super) struct ClientStateWorker {
    sender: mpsc::SyncSender<ClientStateMessage>,
    join: Option<JoinHandle<()>>,
    _temporary: Option<tempfile::TempDir>,
}

impl ClientStateWorker {
    pub(super) fn open(client_id: ClientId) -> Result<(Self, DurableClientState)> {
        #[cfg(test)]
        let (directory, temporary) = {
            let temporary = tempfile::tempdir().context("create test client state")?;
            (temporary.path().to_path_buf(), Some(temporary))
        };
        #[cfg(not(test))]
        let (directory, temporary) = (client_state_directory()?, None);
        let store = ClientViewStateStore::new(directory);
        let state = store
            .load_durable(client_id)
            .context("load durable client state")?
            .unwrap_or_else(|| DurableClientState::new(client_id));
        let (sender, receiver) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("wren-client-state".to_owned())
            .spawn(move || client_state_loop(store, receiver))
            .context("spawn client state writer")?;
        Ok((
            Self {
                sender,
                join: Some(join),
                _temporary: temporary,
            },
            state,
        ))
    }

    pub(super) fn try_save(&self, state: DurableClientState) {
        let _ = self
            .sender
            .try_send(ClientStateMessage::Save(Box::new(state)));
    }

    pub(super) fn barrier(&self, state: DurableClientState) -> Result<()> {
        let (reply, response) = mpsc::channel();
        self.sender
            .send(ClientStateMessage::Barrier {
                state: Box::new(state),
                reply,
            })
            .map_err(|_| anyhow!("client state writer stopped"))?;
        response
            .recv()
            .map_err(|_| anyhow!("client state writer did not acknowledge barrier"))?
            .map_err(anyhow::Error::msg)
    }
}

impl Drop for ClientStateWorker {
    fn drop(&mut self) {
        let _ = self.sender.send(ClientStateMessage::Stop);
        join_worker_thread(&mut self.join);
    }
}

pub(super) fn client_state_loop(
    store: ClientViewStateStore,
    receiver: mpsc::Receiver<ClientStateMessage>,
) {
    let mut error: Option<String> = None;
    loop {
        let Ok(message) = receiver.recv() else {
            break;
        };
        match message {
            ClientStateMessage::Save(mut state) => {
                enum Completion {
                    Save,
                    Barrier(mpsc::Sender<Result<(), String>>),
                    Stop,
                }
                let completion = loop {
                    match receiver.recv_timeout(Duration::from_millis(10)) {
                        Ok(ClientStateMessage::Save(newer)) => state = newer,
                        Ok(ClientStateMessage::Barrier {
                            state: newest,
                            reply,
                        }) => {
                            state = newest;
                            break Completion::Barrier(reply);
                        }
                        Ok(ClientStateMessage::Stop)
                        | Err(mpsc::RecvTimeoutError::Disconnected) => break Completion::Stop,
                        Err(mpsc::RecvTimeoutError::Timeout) => break Completion::Save,
                    }
                };
                let retry = matches!(completion, Completion::Barrier(_));
                if (error.is_none() || retry)
                    && let Err(current) = store.save_durable(&state)
                {
                    error = Some(current.to_string());
                }
                match completion {
                    Completion::Save => {}
                    Completion::Barrier(reply) => {
                        let _ = reply.send(error.clone().map_or(Ok(()), Err));
                    }
                    Completion::Stop => break,
                }
            }
            ClientStateMessage::Barrier { state, reply } => {
                if let Err(current) = store.save_durable(&state) {
                    error = Some(current.to_string());
                }
                let _ = reply.send(error.clone().map_or(Ok(()), Err));
            }
            ClientStateMessage::Stop => break,
        }
    }
}

#[cfg(not(test))]
pub(super) fn client_state_directory() -> Result<PathBuf> {
    if let Some(directory) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(directory).join("wren"));
    }
    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".local/state/wren"));
    }
    Ok(env::current_dir()
        .context("locate current directory for client state")?
        .join(".wren-state"))
}

#[cfg(not(test))]
pub(super) fn load_recent_files() -> Vec<PathBuf> {
    let Ok(path) = client_state_directory().map(|directory| directory.join("oldfiles")) else {
        return Vec::new();
    };
    std::fs::read(path).map_or_else(
        |_| Vec::new(),
        |contents| {
            contents
                .split(|byte| *byte == 0)
                .filter(|entry| !entry.is_empty())
                .map(|entry| PathBuf::from(String::from_utf8_lossy(entry).into_owned()))
                .filter(|path| path.is_absolute())
                .take(100)
                .collect()
        },
    )
}

#[cfg(test)]
pub(super) fn load_recent_files() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(not(test))]
pub(super) fn save_recent_files(paths: &[PathBuf]) -> Result<()> {
    let directory = client_state_directory()?;
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create oldfiles directory {}", directory.display()))?;
    let path = directory.join("oldfiles");
    let temporary = directory.join("oldfiles.tmp");
    let mut contents = Vec::new();
    for path in paths.iter().take(100) {
        contents.extend_from_slice(path.to_string_lossy().as_bytes());
        contents.push(0);
    }
    std::fs::write(&temporary, contents)
        .with_context(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
pub(super) fn save_recent_files(_paths: &[PathBuf]) -> Result<()> {
    Ok(())
}

#[cfg_attr(test, allow(dead_code))]
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct UndoStateFile {
    base_hash: [u8; 32],
    state: DurableUndoState,
}

#[cfg(not(test))]
pub(super) fn undo_state_path(document: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(document).unwrap_or_else(|_| document.to_path_buf());
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in canonical.to_string_lossy().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Ok(client_state_directory()?
        .join("undo")
        .join(format!("{hash:016x}.json")))
}

#[cfg(not(test))]
pub(super) fn load_undo_state(
    document: &Path,
    base_hash: [u8; 32],
) -> Result<Option<DurableUndoState>> {
    let path = undo_state_path(document)?;
    let contents = match std::fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let stored: UndoStateFile =
        serde_json::from_slice(&contents).with_context(|| format!("decode {}", path.display()))?;
    Ok((stored.base_hash == base_hash).then_some(stored.state))
}

#[cfg(test)]
pub(super) fn load_undo_state(
    _document: &Path,
    _base_hash: [u8; 32],
) -> Result<Option<DurableUndoState>> {
    Ok(None)
}

#[cfg(not(test))]
pub(super) fn save_undo_state(buffer: &mut BufferState) -> Result<()> {
    let Some(document) = buffer.document.presentation_path() else {
        return Ok(());
    };
    let path = undo_state_path(document)?;
    let directory = path
        .parent()
        .ok_or_else(|| anyhow!("undo state path has no parent"))?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("create {}", directory.display()))?;
    let temporary = path.with_extension("json.tmp");
    let contents = serde_json::to_vec(&UndoStateFile {
        base_hash: buffer.base_hash,
        state: buffer.editor.durable_undo_state(),
    })?;
    std::fs::write(&temporary, contents)
        .with_context(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
pub(super) fn save_undo_state(_buffer: &mut BufferState) -> Result<()> {
    Ok(())
}

pub(super) enum MutationMessage {
    Register {
        document_id: DocumentId,
        text: String,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Append {
        document_id: DocumentId,
        transaction: Option<Transaction>,
        state_deltas: Vec<StateDelta>,
    },
    Barrier(mpsc::Sender<Result<(), String>>),
    Stop(mpsc::Sender<Result<(), String>>),
}

const MAX_MUTATION_BATCH: usize = 512;
// Local-durable work is asynchronous relative to the optimistic replica. A
// short quiet period forms the architecture's batching envelope; Barrier and
// Stop still cut the batch immediately for save/suspend/exit frontiers.
const DURABILITY_GROUP_COMMIT_QUIET_PERIOD: Duration = Duration::from_millis(50);

pub(super) struct MutationWorker {
    sender: mpsc::Sender<MutationMessage>,
    join: Option<JoinHandle<()>>,
    _temporary: Option<tempfile::TempDir>,
}

impl MutationWorker {
    pub(super) fn start(_workspace: &Path) -> Result<Self> {
        #[cfg(test)]
        let (session_directory, outbox_directory, temporary) = {
            let temporary = tempfile::tempdir().context("create test session state")?;
            (
                temporary.path().join("session"),
                temporary.path().join("outbox"),
                Some(temporary),
            )
        };
        #[cfg(not(test))]
        let (session_directory, outbox_directory, temporary) = {
            let workspace_key = format!("{:016x}", stable_document_id(Some(_workspace)).get());
            let root = client_state_directory()?;
            (
                root.join("sessions").join(&workspace_key),
                root.join("outbox").join(workspace_key),
                None,
            )
        };
        std::fs::create_dir_all(&session_directory).with_context(|| {
            format!(
                "create durable session directory {}",
                session_directory.display()
            )
        })?;
        std::fs::create_dir_all(&outbox_directory).with_context(|| {
            format!(
                "create durable outbox directory {}",
                outbox_directory.display()
            )
        })?;
        let authority = SessionAuthority::open(
            SessionJournal::in_directory(session_directory),
            SessionId::new(1),
        )?;
        let outbox = MutationOutbox::in_directory(outbox_directory);
        let (sender, receiver) = mpsc::channel();
        let join = thread::Builder::new()
            .name("wren-in-process-session".to_owned())
            .spawn(move || mutation_loop(authority, outbox, receiver))
            .context("spawn in-process mutation session")?;
        Ok(Self {
            sender,
            join: Some(join),
            _temporary: temporary,
        })
    }

    pub(super) fn register(&self, document_id: DocumentId, text: String) -> Result<()> {
        let (reply, response) = mpsc::channel();
        self.sender
            .send(MutationMessage::Register {
                document_id,
                text,
                reply,
            })
            .map_err(|_| anyhow!("in-process session stopped"))?;
        response
            .recv()
            .map_err(|_| anyhow!("in-process session did not register document"))?
            .map_err(anyhow::Error::msg)
    }

    pub(super) fn append(
        &self,
        document_id: DocumentId,
        transaction: Option<Transaction>,
        state_deltas: Vec<StateDelta>,
    ) -> Result<()> {
        if transaction.is_none() && state_deltas.is_empty() {
            return Ok(());
        }
        self.sender
            .send(MutationMessage::Append {
                document_id,
                transaction,
                state_deltas,
            })
            .map_err(|_| anyhow!("in-process session stopped"))
    }

    pub(super) fn barrier(&self) -> Result<()> {
        let (reply, response) = mpsc::channel();
        self.sender
            .send(MutationMessage::Barrier(reply))
            .map_err(|_| anyhow!("in-process session stopped"))?;
        response
            .recv()
            .map_err(|_| anyhow!("in-process session did not acknowledge barrier"))?
            .map_err(anyhow::Error::msg)
    }
}

impl Drop for MutationWorker {
    fn drop(&mut self) {
        let (reply, response) = mpsc::channel();
        let _ = self.sender.send(MutationMessage::Stop(reply));
        let _ = response.recv();
        join_worker_thread(&mut self.join);
    }
}

pub(super) fn mutation_loop(
    mut authority: SessionAuthority,
    outbox: MutationOutbox,
    receiver: mpsc::Receiver<MutationMessage>,
) {
    let client_id = ClientId::new(1);
    let mut error = replay_outstanding_mutations(&mut authority, &outbox)
        .err()
        .map(|current| current.to_string());
    let mut next_sequence = authority
        .highest_client_sequence(client_id)
        .get()
        .saturating_add(1);
    let mut pending = None;
    loop {
        let message = match pending.take() {
            Some(message) => message,
            None => match receiver.recv() {
                Ok(message) => message,
                Err(_) => break,
            },
        };
        match message {
            MutationMessage::Register {
                document_id,
                text,
                reply,
            } => {
                let result = if let Some(document) = authority.document(document_id) {
                    if document.text_equals(&text) {
                        Ok(())
                    } else {
                        Err(format!(
                            "durable session text for {document_id:?} differs from local recovery; explicit reconciliation is required"
                        ))
                    }
                } else {
                    authority
                        .register_document(document_id, text, client_id)
                        .map(|_| ())
                        .map_err(|current| current.to_string())
                };
                if let Err(current) = &result {
                    error = Some(current.clone());
                }
                let _ = reply.send(result);
            }
            MutationMessage::Append {
                document_id,
                transaction,
                state_deltas,
            } => {
                let mut batch = vec![(document_id, transaction, state_deltas)];
                while batch.len() < MAX_MUTATION_BATCH {
                    match receiver.recv_timeout(DURABILITY_GROUP_COMMIT_QUIET_PERIOD) {
                        Ok(MutationMessage::Append {
                            document_id,
                            transaction,
                            state_deltas,
                        }) => batch.push((document_id, transaction, state_deltas)),
                        Ok(control) => {
                            pending = Some(control);
                            break;
                        }
                        Err(
                            mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected,
                        ) => {
                            break;
                        }
                    }
                }
                if error.is_some() {
                    continue;
                }
                let batch_len = u64::try_from(batch.len()).unwrap_or(u64::MAX);
                let result = submit_local_mutations(
                    &mut authority,
                    &outbox,
                    client_id,
                    next_sequence,
                    batch,
                );
                if let Err(current) = result {
                    error = Some(current.to_string());
                } else {
                    next_sequence = next_sequence.saturating_add(batch_len);
                }
            }
            MutationMessage::Barrier(reply) => {
                let _ = reply.send(error.clone().map_or(Ok(()), Err));
            }
            MutationMessage::Stop(reply) => {
                let _ = reply.send(error.clone().map_or(Ok(()), Err));
                break;
            }
        }
    }
}

pub(super) fn replay_outstanding_mutations(
    authority: &mut SessionAuthority,
    outbox: &MutationOutbox,
) -> Result<()> {
    for mutation in outbox.outstanding()? {
        match authority.submit(mutation)? {
            MutationSubmission::Accepted { durable, .. } => {
                if !outbox.observe_result(&durable)? {
                    bail!("replayed durable mutation was missing from the client outbox");
                }
            }
            MutationSubmission::Rejected(result) => {
                bail!("outstanding mutation requires reconciliation: {result:?}");
            }
        }
    }
    Ok(())
}

type PendingMutation = (DocumentId, Option<Transaction>, Vec<StateDelta>);

fn submit_local_mutations(
    authority: &mut SessionAuthority,
    outbox: &MutationOutbox,
    client_id: ClientId,
    first_sequence: u64,
    pending: Vec<PendingMutation>,
) -> Result<()> {
    let mut document_revisions = std::collections::BTreeMap::new();
    let mut mutations = Vec::with_capacity(pending.len());
    for (offset, (document_id, transaction, state_deltas)) in pending.into_iter().enumerate() {
        let offset = u64::try_from(offset).context("mutation batch exceeds u64")?;
        let sequence = first_sequence
            .checked_add(offset)
            .ok_or_else(|| anyhow!("client mutation sequence overflow"))?;
        let document = authority
            .document(document_id)
            .ok_or_else(|| anyhow!("document is not registered"))?;
        let revision = document_revisions
            .entry(document_id)
            .or_insert(document.revision);
        let mut documents = Vec::new();
        if let Some(mut transaction) = transaction {
            transaction.base_revision = *revision;
            documents.push(DocumentMutation {
                document_id,
                lease_epoch: document.lease.lease_epoch,
                base_revision: *revision,
                semantic_group_id: SemanticGroupId::new(sequence),
                semantic_group_kind: SemanticGroupKind::Operator,
                undo_parent: None,
                transactions: vec![transaction],
            });
            *revision = revision
                .next()
                .ok_or_else(|| anyhow!("document revision overflow"))?;
        }
        mutations.push(ClientMutation {
            mutation_id: MutationId::new(sequence),
            client_id,
            client_sequence: ClientSequence::new(sequence),
            state_deltas,
            documents,
        });
    }

    outbox.append_many(&mutations)?;
    let submissions = authority.submit_batch(mutations)?;
    let mut durable = Vec::with_capacity(submissions.len());
    let mut rejection = None;
    for submission in submissions {
        match submission {
            MutationSubmission::Accepted {
                durable: acknowledged,
                ..
            } => durable.push(acknowledged),
            MutationSubmission::Rejected(result) => rejection = Some(result),
        }
    }
    let acknowledged = outbox.observe_results(&durable)?;
    if acknowledged != durable.len() {
        bail!("durable mutation batch was missing from the client outbox");
    }
    if let Some(result) = rejection {
        bail!("in-process mutation rejected: {result:?}");
    }
    Ok(())
}

pub(super) enum WalMessage {
    AppendFrame {
        base_hash: [u8; 32],
        revision: u64,
        text: FrameText,
        cursor: usize,
    },
    Clear(mpsc::Sender<Result<(), String>>),
    Barrier(mpsc::Sender<Result<(), String>>),
    Stop(mpsc::Sender<Result<(), String>>),
}

pub(super) struct WalWorker {
    sender: mpsc::Sender<WalMessage>,
    join: Option<JoinHandle<()>>,
}

impl WalWorker {
    pub(super) fn start(wal: LocalWal) -> Self {
        let (sender, receiver) = mpsc::channel();
        let join = thread::Builder::new()
            .name("wren-wal".to_owned())
            .spawn(move || wal_loop(&wal, receiver))
            .ok();
        Self { sender, join }
    }

    pub(super) fn append_frame(
        &self,
        base_hash: [u8; 32],
        revision: u64,
        text: FrameText,
        cursor: usize,
    ) {
        let _ = self.sender.send(WalMessage::AppendFrame {
            base_hash,
            revision,
            text,
            cursor,
        });
    }

    pub(super) fn barrier(&self) -> Result<()> {
        self.request(WalMessage::Barrier)
    }

    pub(super) fn clear(&self) -> Result<()> {
        self.request(WalMessage::Clear)
    }

    fn request(
        &self,
        make: impl FnOnce(mpsc::Sender<Result<(), String>>) -> WalMessage,
    ) -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        self.sender
            .send(make(sender))
            .map_err(|_| anyhow!("recovery WAL worker stopped"))?;
        receiver
            .recv()
            .map_err(|_| anyhow!("recovery WAL worker did not acknowledge"))?
            .map_err(|error| anyhow!(error))
    }
}

impl Drop for WalWorker {
    fn drop(&mut self) {
        let (sender, receiver) = mpsc::channel();
        let _ = self.sender.send(WalMessage::Stop(sender));
        let _ = receiver.recv();
        join_worker_thread(&mut self.join);
    }
}

pub(super) fn wal_loop(wal: &LocalWal, receiver: mpsc::Receiver<WalMessage>) {
    let mut error: Option<String> = None;
    let mut pending = None;
    loop {
        let message = match pending.take() {
            Some(message) => message,
            None => match receiver.recv() {
                Ok(message) => message,
                Err(_) => break,
            },
        };
        match message {
            WalMessage::AppendFrame {
                base_hash,
                revision,
                text,
                cursor,
            } => {
                let mut latest = (base_hash, revision, text, cursor);
                loop {
                    match receiver.recv_timeout(DURABILITY_GROUP_COMMIT_QUIET_PERIOD) {
                        Ok(WalMessage::AppendFrame {
                            base_hash,
                            revision,
                            text,
                            cursor,
                        }) => latest = (base_hash, revision, text, cursor),
                        Ok(control) => {
                            pending = Some(control);
                            break;
                        }
                        Err(
                            mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected,
                        ) => break,
                    }
                }
                let (base_hash, revision, text, cursor) = latest;
                let text = text.shared();
                let result = if *blake3::hash(text.as_bytes()).as_bytes() == base_hash {
                    wal.clear()
                } else {
                    wal.append(&RecoveredState {
                        base_hash,
                        revision,
                        text: text.to_string(),
                        cursor,
                    })
                };
                if let Err(current) = result {
                    error = Some(current.to_string());
                }
            }
            WalMessage::Clear(reply) => {
                if error.is_none()
                    && let Err(current) = wal.clear()
                {
                    error = Some(current.to_string());
                }
                let _ = reply.send(error.clone().map_or(Ok(()), Err));
            }
            WalMessage::Barrier(reply) => {
                let _ = reply.send(error.clone().map_or(Ok(()), Err));
            }
            WalMessage::Stop(reply) => {
                let _ = reply.send(error.clone().map_or(Ok(()), Err));
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_state_quiet_period_keeps_the_latest_state_and_barrier() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = ClientViewStateStore::new(directory.path());
        let inspect = store.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || client_state_loop(store, receiver));
        let mut stale = DurableClientState::new(ClientId::new(1));
        stale.apply(&StateDelta::CommandHistory("stale".into()));
        sender
            .send(ClientStateMessage::Save(Box::new(stale)))
            .expect("stale state");
        let mut latest = DurableClientState::new(ClientId::new(1));
        latest.apply(&StateDelta::CommandHistory("latest".into()));
        sender
            .send(ClientStateMessage::Save(Box::new(latest.clone())))
            .expect("latest state");
        let (reply, response) = mpsc::channel();
        sender
            .send(ClientStateMessage::Barrier {
                state: Box::new(latest.clone()),
                reply,
            })
            .expect("barrier request");
        response.recv().expect("barrier response").expect("barrier");
        sender.send(ClientStateMessage::Stop).expect("stop");
        worker.join().expect("client state worker");
        assert_eq!(
            inspect
                .load_durable(ClientId::new(1))
                .expect("load state")
                .expect("durable state"),
            latest
        );
    }

    #[test]
    fn mutation_group_commit_flushes_every_edit_at_a_barrier() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let journal = SessionJournal::in_directory(directory.path().join("session"));
        let authority =
            SessionAuthority::open(journal.clone(), SessionId::new(1)).expect("open authority");
        let outbox = MutationOutbox::in_directory(directory.path().join("outbox"));
        let inspect_outbox = outbox.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || mutation_loop(authority, outbox, receiver));

        let (reply, response) = mpsc::channel();
        sender
            .send(MutationMessage::Register {
                document_id: DocumentId::new(1),
                text: "base".to_owned(),
                reply,
            })
            .expect("register request");
        response
            .recv()
            .expect("register response")
            .expect("register");
        for _ in 0..128 {
            sender
                .send(MutationMessage::Append {
                    document_id: DocumentId::new(1),
                    transaction: Some(
                        Transaction::new(DocumentRevision::new(0), vec![Edit::new(0..0, "x")])
                            .expect("transaction"),
                    ),
                    state_deltas: Vec::new(),
                })
                .expect("append request");
        }
        let (reply, response) = mpsc::channel();
        sender
            .send(MutationMessage::Barrier(reply))
            .expect("barrier request");
        response.recv().expect("barrier response").expect("barrier");
        let (reply, response) = mpsc::channel();
        sender
            .send(MutationMessage::Stop(reply))
            .expect("stop request");
        response.recv().expect("stop response").expect("stop");
        worker.join().expect("mutation worker");

        assert!(inspect_outbox.outstanding().expect("outbox").is_empty());
        let recovered =
            SessionAuthority::open(journal, SessionId::new(1)).expect("recover authority");
        assert_eq!(
            recovered
                .document(DocumentId::new(1))
                .expect("document")
                .text(),
            format!("{}base", "x".repeat(128))
        );
    }
}
