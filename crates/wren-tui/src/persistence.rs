use super::*;

pub(super) fn apply_client_state(buffer: &mut BufferState, state: &DurableClientState) -> Result<()> {
    let document_len = buffer.editor.frame().text.len();
    let editor_state = buffer.editor.state_mut();
    state.registers.iter().for_each(|(name, register)| editor_state.set_register(*name, register.text.clone(), register.linewise));
    state
        .global_marks
        .iter()
        .filter(|(_, mark)| mark.document_id == buffer.document_id)
        .for_each(|(name, mark)| editor_state.set_mark(*name, mark.anchor.byte, document_len));
    if let Some(repeat) = &state.repeat_data {
        editor_state.set_repeat_data(repeat)?;
    }
    for (name, recording) in &state.macro_recordings {
        let keys: Vec<KeyEvent> = serde_json::from_slice(&recording.raw_keys).with_context(|| format!("restore macro {name}"))?;
        editor_state.set_macro(*name, keys);
    }
    if let Some(pattern) = state.search_history.last() {
        buffer.editor.set_search(pattern.clone(), if state.search_backward { SearchDirection::Backward } else { SearchDirection::Forward })?;
    }
    Ok(())
}

pub(super) fn sync_client_state(active: &mut BufferState, inactive: &mut [BufferState], state: &DurableClientState) -> Result<()> {
    apply_client_state(active, state)?;
    for buffer in inactive {
        apply_client_state(buffer, state)?;
    }
    Ok(())
}

type PersistenceReply = mpsc::Sender<Result<(), String>>;

pub(super) enum WorkerControl {
    Barrier(PersistenceReply),
    Stop(PersistenceReply),
}

impl WorkerControl {
    fn acknowledge(self, error: &Option<String>) -> bool {
        let (reply, stop) = match self {
            Self::Barrier(reply) => (reply, false),
            Self::Stop(reply) => (reply, true),
        };
        let _ = reply.send(persistence_status(error));
        stop
    }
}

pub(super) trait PersistenceMessage {
    fn control(control: WorkerControl) -> Self;
}

pub(super) struct PersistenceWorker<M: PersistenceMessage> {
    sender: mpsc::SyncSender<M>,
    join: Option<JoinHandle<()>>,
    #[cfg(test)]
    _temporary: Option<tempfile::TempDir>,
}

impl<M: PersistenceMessage + Send + 'static> PersistenceWorker<M> {
    fn spawn(
        name: &str,
        capacity: usize,
        #[cfg(test)] temporary: Option<tempfile::TempDir>,
        run: impl FnOnce(mpsc::Receiver<M>) + Send + 'static,
    ) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let join = wren_scheduling::spawn_background(name, move || run(receiver)).with_context(|| format!("spawn {name}"))?;
        Ok(Self {
            sender,
            join: Some(join),
            #[cfg(test)]
            _temporary: temporary,
        })
    }

    fn request(&self, make: impl FnOnce(PersistenceReply) -> M) -> Result<()> {
        let (reply, response) = mpsc::channel();
        self.sender.send(make(reply)).map_err(|_| anyhow!("persistence worker stopped"))?;
        response.recv().map_err(|_| anyhow!("persistence worker did not acknowledge"))?.map_err(anyhow::Error::msg)
    }

    pub(super) fn barrier(&self) -> Result<()> {
        self.request(|reply| M::control(WorkerControl::Barrier(reply)))
    }
}

impl<M: PersistenceMessage> Drop for PersistenceWorker<M> {
    fn drop(&mut self) {
        let (reply, response) = mpsc::channel();
        let _ = self.sender.send(M::control(WorkerControl::Stop(reply)));
        let _ = response.recv();
        join_worker_thread(&mut self.join);
    }
}

pub(super) enum LatestWrite<T> {
    Write(T),
    Clear(PersistenceReply),
    Control(WorkerControl),
}

impl<T> PersistenceMessage for LatestWrite<T> {
    fn control(control: WorkerControl) -> Self {
        Self::Control(control)
    }
}

type ClientStateMessage = LatestWrite<Box<DurableClientState>>;
pub(super) type ClientStateWorker = PersistenceWorker<ClientStateMessage>;

impl PersistenceWorker<ClientStateMessage> {
    pub(super) fn open(client_id: ClientId) -> Result<(Self, DurableClientState)> {
        #[cfg(test)]
        let temporary = tempfile::tempdir().context("create test client state")?;
        #[cfg(test)]
        let directory = temporary.path().to_path_buf();
        #[cfg(not(test))]
        let directory = client_state_directory()?;
        let store = ClientViewStateStore::new(directory);
        let state = store.load_durable(client_id).context("load durable client state")?.unwrap_or_else(|| DurableClientState::new(client_id));
        let worker = Self::spawn(
            "wren-client-state",
            1,
            #[cfg(test)]
            Some(temporary),
            move |receiver| {
                latest_write_loop(receiver, Duration::from_millis(10), |state| store.save_durable(&state).map_err(Into::into), || Ok(()));
            },
        )?;
        Ok((worker, state))
    }

    pub(super) fn try_save(&self, state: DurableClientState) {
        let _ = self.sender.try_send(LatestWrite::Write(Box::new(state)));
    }

    pub(super) fn flush_state(&self, state: DurableClientState) -> Result<()> {
        self.sender.send(LatestWrite::Write(Box::new(state))).map_err(|_| anyhow!("client state writer stopped"))?;
        self.barrier()
    }
}

fn latest_write_loop<T>(
    receiver: mpsc::Receiver<LatestWrite<T>>,
    quiet_period: Duration,
    mut write: impl FnMut(T) -> Result<()>,
    mut clear: impl FnMut() -> Result<()>,
) {
    let mut error: Option<String> = None;
    let mut messages = DeferredMessages::new(&receiver);
    while let Some(message) = messages.next() {
        match message {
            LatestWrite::Write(value) => {
                let mut latest = value;
                collect_latest(&mut messages, &mut latest, quiet_period, |message, latest| match message {
                    LatestWrite::Write(value) => {
                        *latest = value;
                        None
                    }
                    control => Some(control),
                });
                record_persistence_error(&mut error, &write(latest));
            }
            LatestWrite::Clear(reply) => {
                if error.is_none() {
                    record_persistence_error(&mut error, &clear());
                }
                let _ = reply.send(persistence_status(&error));
            }
            LatestWrite::Control(control) => {
                if control.acknowledge(&error) {
                    break;
                }
            }
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
    Ok(env::current_dir().context("locate current directory for client state")?.join(".wren-state"))
}

#[cfg(test)]
pub(super) fn load_recent_files() -> Vec<PathBuf> {
    Vec::new()
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
pub(super) fn save_recent_files(_paths: &[PathBuf]) -> Result<()> {
    Ok(())
}

#[cfg(not(test))]
pub(super) fn save_recent_files(paths: &[PathBuf]) -> Result<()> {
    let path = client_state_directory()?.join("oldfiles");
    let mut contents = Vec::new();
    for path in paths.iter().take(100) {
        contents.extend_from_slice(path.to_string_lossy().as_bytes());
        contents.push(0);
    }
    replace_file(&path, contents)
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
    let hash = stable_hash(canonical.to_string_lossy().bytes());
    Ok(client_state_directory()?.join("undo").join(format!("{hash:016x}.json")))
}

#[cfg(test)]
pub(super) fn load_undo_state(_document: &Path, _base_hash: [u8; 32]) -> Result<Option<DurableUndoState>> {
    Ok(None)
}

#[cfg(not(test))]
pub(super) fn load_undo_state(document: &Path, base_hash: [u8; 32]) -> Result<Option<DurableUndoState>> {
    let path = undo_state_path(document)?;
    let contents = match std::fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let stored: UndoStateFile = serde_json::from_slice(&contents).with_context(|| format!("decode {}", path.display()))?;
    Ok((stored.base_hash == base_hash).then_some(stored.state))
}

#[cfg(test)]
pub(super) fn save_undo_state(_buffer: &mut BufferState) -> Result<()> {
    Ok(())
}

#[cfg(not(test))]
pub(super) fn save_undo_state(buffer: &mut BufferState) -> Result<()> {
    let Some(document) = buffer.document.presentation_path() else {
        return Ok(());
    };
    let path = undo_state_path(document)?;
    let contents = serde_json::to_vec(&UndoStateFile { base_hash: buffer.base_hash, state: buffer.editor.durable_undo_state() })?;
    replace_file(&path, contents)
}

#[cfg(not(test))]
fn replace_file(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    let directory = path.parent().ok_or_else(|| anyhow!("state path has no parent"))?;
    std::fs::create_dir_all(directory).with_context(|| format!("create {}", directory.display()))?;
    let mut name = path.file_name().ok_or_else(|| anyhow!("state path has no file name"))?.to_os_string();
    name.push(".tmp");
    let temporary = path.with_file_name(name);
    std::fs::write(&temporary, contents).with_context(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))
}

pub(super) enum MutationMessage {
    Register { document_id: DocumentId, text: String, replace_stale: bool, reply: mpsc::Sender<Result<(), String>> },
    Append { document_id: DocumentId, transactions: TransactionBatch, state_deltas: Vec<StateDelta> },
    Control(WorkerControl),
}

impl PersistenceMessage for MutationMessage {
    fn control(control: WorkerControl) -> Self {
        Self::Control(control)
    }
}

const MAX_MUTATION_BATCH: usize = 512;
const DURABILITY_QUEUE_CAPACITY: usize = 4_096;
// Local-durable work is asynchronous relative to the optimistic replica. A
// short quiet period forms the architecture's batching envelope; Barrier and
// Stop still cut the batch immediately for save/suspend/exit frontiers.
const DURABILITY_GROUP_COMMIT_QUIET_PERIOD: Duration = Duration::from_millis(50);

pub(super) type MutationWorker = PersistenceWorker<MutationMessage>;

impl PersistenceWorker<MutationMessage> {
    pub(super) fn start(_workspace: &Path) -> Result<Self> {
        #[cfg(test)]
        let temporary = tempfile::tempdir().context("create test session state")?;
        #[cfg(test)]
        let (session_directory, outbox_directory) = (temporary.path().join("session"), temporary.path().join("outbox"));
        #[cfg(not(test))]
        let (session_directory, outbox_directory) = {
            let workspace_key = format!("{:016x}", stable_document_id(Some(_workspace)).get());
            let root = client_state_directory()?;
            (root.join("sessions").join(&workspace_key), root.join("outbox").join(workspace_key))
        };
        std::fs::create_dir_all(&session_directory).with_context(|| format!("create durable session directory {}", session_directory.display()))?;
        std::fs::create_dir_all(&outbox_directory).with_context(|| format!("create durable outbox directory {}", outbox_directory.display()))?;
        let authority = SessionAuthority::open(SessionJournal::in_directory(session_directory), SessionId::new(1))?;
        let outbox = MutationOutbox::in_directory(outbox_directory);
        Self::spawn(
            "wren-in-process-session",
            DURABILITY_QUEUE_CAPACITY,
            #[cfg(test)]
            Some(temporary),
            move |receiver| {
                mutation_loop(authority, outbox, receiver);
            },
        )
    }

    pub(super) fn register(&self, document_id: DocumentId, text: String, replace_stale: bool) -> Result<()> {
        let (reply, response) = mpsc::channel();
        self.sender.send(MutationMessage::Register { document_id, text, replace_stale, reply }).map_err(|_| anyhow!("in-process session stopped"))?;
        response.recv().map_err(|_| anyhow!("in-process session did not register document"))?.map_err(anyhow::Error::msg)
    }

    pub(super) fn append(&self, document_id: DocumentId, transactions: TransactionBatch, state_deltas: Vec<StateDelta>) -> Result<()> {
        if transactions.is_empty() && state_deltas.is_empty() {
            return Ok(());
        }
        self.sender.send(MutationMessage::Append { document_id, transactions, state_deltas }).map_err(|_| anyhow!("in-process session stopped"))
    }
}

pub(super) fn mutation_loop(mut authority: SessionAuthority, outbox: MutationOutbox, receiver: mpsc::Receiver<MutationMessage>) {
    let client_id = ClientId::new(1);
    let mut error = replay_outstanding_mutations(&mut authority, &outbox).err().map(|current| current.to_string());
    let mut next_sequence = authority.highest_client_sequence(client_id).get().saturating_add(1);
    let mut messages = DeferredMessages::new(&receiver);
    while let Some(message) = messages.next() {
        match message {
            MutationMessage::Register { document_id, text, replace_stale, reply } => {
                let result = register_mutation_document(&mut authority, &outbox, client_id, next_sequence, document_id, text, replace_stale);
                if let Ok(sequence) = result {
                    next_sequence = sequence;
                }
                record_persistence_error(&mut error, &result);
                let _ = reply.send(result.map(|_| ()));
            }
            MutationMessage::Append { document_id, transactions, state_deltas } => {
                let batch = collect_mutation_batch(&mut messages, (document_id, transactions, state_deltas));
                if error.is_some() {
                    continue;
                }
                let batch_len = u64::try_from(batch.len()).unwrap_or(u64::MAX);
                let result = submit_local_mutations(&mut authority, &outbox, client_id, next_sequence, batch);
                match result {
                    Ok(()) => next_sequence = next_sequence.saturating_add(batch_len),
                    Err(current) => error = Some(current.to_string()),
                }
            }
            MutationMessage::Control(control) => {
                if control.acknowledge(&error) {
                    break;
                }
            }
        }
    }
}

struct DeferredMessages<'a, T> {
    receiver: &'a mpsc::Receiver<T>,
    deferred: Option<T>,
}

impl<'a, T> DeferredMessages<'a, T> {
    fn new(receiver: &'a mpsc::Receiver<T>) -> Self {
        Self { receiver, deferred: None }
    }

    fn next(&mut self) -> Option<T> {
        self.deferred.take().or_else(|| self.receiver.recv().ok())
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<T, mpsc::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }

    fn defer(&mut self, message: T) {
        debug_assert!(self.deferred.is_none());
        self.deferred = Some(message);
    }
}

fn collect_latest<T, M>(messages: &mut DeferredMessages<'_, M>, latest: &mut T, quiet_period: Duration, select: impl Fn(M, &mut T) -> Option<M>) {
    while let Ok(message) = messages.recv_timeout(quiet_period) {
        if let Some(control) = select(message, latest) {
            messages.defer(control);
            break;
        }
    }
}

fn register_mutation_document(
    authority: &mut SessionAuthority,
    outbox: &MutationOutbox,
    client_id: ClientId,
    next_sequence: u64,
    document_id: DocumentId,
    text: String,
    replace_stale: bool,
) -> Result<u64, String> {
    match authority.document(document_id) {
        Some(document) if document.text_equals(&text) => Ok(next_sequence),
        // Clean disk and generated buffers are current source snapshots, so an
        // external change may advance their stale authority through the normal
        // checksummed mutation path. A buffer restored from an unsaved WAL sets
        // `replace_stale` false: disagreement then remains a real conflict and
        // never discards recovered user edits.
        Some(document) if replace_stale => {
            let replacement = Transaction::new(document.revision, vec![Edit::new(0..document.text().len(), text)]).map_err(|error| error.to_string())?;
            submit_local_mutations(authority, outbox, client_id, next_sequence, vec![(document_id, std::iter::once(replacement).collect(), Vec::new())])
                .map_err(|error| error.to_string())?;
            next_sequence.checked_add(1).ok_or_else(|| "client mutation sequence overflow".to_owned())
        }
        Some(_) => Err(format!("durable session text for {document_id:?} differs from local recovery; explicit reconciliation is required")),
        None => authority.register_document(document_id, text, client_id).map(|_| next_sequence).map_err(|current| current.to_string()),
    }
}

fn collect_mutation_batch(messages: &mut DeferredMessages<'_, MutationMessage>, first: PendingMutation) -> Vec<PendingMutation> {
    let mut batch = vec![first];
    while batch.len() < MAX_MUTATION_BATCH {
        match messages.recv_timeout(DURABILITY_GROUP_COMMIT_QUIET_PERIOD) {
            Ok(MutationMessage::Append { document_id, transactions, state_deltas }) => batch.push((document_id, transactions, state_deltas)),
            Ok(control) => {
                messages.defer(control);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    batch
}

fn record_persistence_error<T, E: ToString>(error: &mut Option<String>, result: &Result<T, E>) {
    if let Err(current) = result {
        *error = Some(current.to_string());
    }
}

fn persistence_status(error: &Option<String>) -> Result<(), String> {
    error.clone().map_or(Ok(()), Err)
}

pub(super) fn replay_outstanding_mutations(authority: &mut SessionAuthority, outbox: &MutationOutbox) -> Result<()> {
    for mutation in outbox.outstanding()? {
        match authority.submit(mutation)? {
            Ok(durable) => {
                if !outbox.observe_result(&durable)? {
                    bail!("replayed durable mutation was missing from the client outbox");
                }
            }
            Err(result) => {
                bail!("outstanding mutation requires reconciliation: {result:?}");
            }
        }
    }
    Ok(())
}

type PendingMutation = (DocumentId, TransactionBatch, Vec<StateDelta>);

fn submit_local_mutations(
    authority: &mut SessionAuthority,
    outbox: &MutationOutbox,
    client_id: ClientId,
    first_sequence: u64,
    pending: Vec<PendingMutation>,
) -> Result<()> {
    let mut document_revisions = std::collections::BTreeMap::new();
    let mut mutations = Vec::with_capacity(pending.len());
    for (offset, (document_id, mut transactions, state_deltas)) in pending.into_iter().enumerate() {
        let offset = u64::try_from(offset).context("mutation batch exceeds u64")?;
        let sequence = first_sequence.checked_add(offset).ok_or_else(|| anyhow!("client mutation sequence overflow"))?;
        let document = authority.document(document_id).ok_or_else(|| anyhow!("document is not registered"))?;
        let revision = document_revisions.entry(document_id).or_insert(document.revision);
        let mut documents = Vec::new();
        if !transactions.is_empty() {
            let base_revision = *revision;
            for transaction in &mut transactions {
                transaction.rebase(*revision);
                *revision = revision.next().ok_or_else(|| anyhow!("document revision overflow"))?;
            }
            documents.push(DocumentMutation {
                document_id,
                lease_epoch: document.lease.lease_epoch,
                base_revision,
                semantic_group_id: SemanticGroupId::new(sequence),
                semantic_group_kind: SemanticGroupKind::Operator,
                undo_parent: None,
                transactions: transactions.into_vec(),
            });
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
            Ok(acknowledged) => durable.push(acknowledged),
            Err(result) => rejection = Some(result),
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

pub(super) struct WalWorker {
    worker: PersistenceWorker<LatestWrite<PendingWalFrame>>,
}

impl WalWorker {
    pub(super) fn start(wal: LocalWal) -> Result<Self> {
        let durable = wal.clone();
        let worker = PersistenceWorker::spawn(
            "wren-wal",
            DURABILITY_QUEUE_CAPACITY,
            #[cfg(test)]
            None,
            move |receiver| {
                latest_write_loop(
                    receiver,
                    DURABILITY_GROUP_COMMIT_QUIET_PERIOD,
                    |frame| persist_wal_frame(&wal, frame),
                    || durable.clear().map_err(Into::into),
                );
            },
        )?;
        Ok(Self { worker })
    }

    pub(super) fn append_frame(&self, base_hash: [u8; 32], revision: u64, text: FrameText, cursor: usize) {
        let _ = self.worker.sender.send(LatestWrite::Write((base_hash, revision, text, cursor)));
    }

    pub(super) fn barrier(&self) -> Result<()> {
        self.worker.barrier()
    }

    pub(super) fn clear(&self) -> Result<()> {
        self.worker.request(LatestWrite::Clear)
    }
}

type PendingWalFrame = ([u8; 32], u64, FrameText, usize);

fn persist_wal_frame(wal: &LocalWal, frame: PendingWalFrame) -> Result<()> {
    let (base_hash, revision, text, cursor) = frame;
    let text = text.materialize_for_task();
    if *blake3::hash(text.as_bytes()).as_bytes() == base_hash {
        wal.clear()?;
    } else {
        wal.append(&RecoveredState { base_hash, revision, text: text.to_string(), cursor })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    fn client_state_quiet_period_keeps_the_latest_state_and_barrier() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = ClientViewStateStore::new(directory.path());
        let inspect = store.clone();
        let (sender, receiver) = mpsc::channel::<ClientStateMessage>();
        let worker = thread::spawn(move || {
            latest_write_loop(receiver, Duration::from_millis(10), |state| store.save_durable(&state).map_err(Into::into), || Ok(()));
        });
        let mut stale = DurableClientState::new(ClientId::new(1));
        stale.apply(&StateDelta::CommandHistory("stale".into()));
        sender.send(LatestWrite::Write(Box::new(stale))).expect("stale state");
        let mut latest = DurableClientState::new(ClientId::new(1));
        latest.apply(&StateDelta::CommandHistory("latest".into()));
        sender.send(LatestWrite::Write(Box::new(latest.clone()))).expect("latest state");
        let (reply, response) = mpsc::channel();
        sender.send(LatestWrite::Control(WorkerControl::Barrier(reply))).expect("barrier request");
        response.recv().expect("barrier response").expect("barrier");
        let (reply, response) = mpsc::channel();
        sender.send(LatestWrite::Control(WorkerControl::Stop(reply))).expect("stop");
        response.recv().expect("stop response").expect("stop");
        worker.join().expect("client state worker");
        assert_eq!(inspect.load_durable(ClientId::new(1)).expect("load state").expect("durable state"), latest);
    }

    #[test]
    fn mutation_group_commit_flushes_every_edit_at_a_barrier() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let journal = SessionJournal::in_directory(directory.path().join("session"));
        let authority = SessionAuthority::open(journal.clone(), SessionId::new(1)).expect("open authority");
        let outbox = MutationOutbox::in_directory(directory.path().join("outbox"));
        let inspect_outbox = outbox.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || mutation_loop(authority, outbox, receiver));

        let (reply, response) = mpsc::channel();
        sender
            .send(MutationMessage::Register { document_id: DocumentId::new(1), text: "base".to_owned(), replace_stale: true, reply })
            .expect("register request");
        response.recv().expect("register response").expect("register");
        for _ in 0..128 {
            sender
                .send(MutationMessage::Append {
                    document_id: DocumentId::new(1),
                    transactions: vec![Transaction::new(DocumentRevision::new(0), vec![Edit::new(0..0, "x")]).expect("transaction")].into(),
                    state_deltas: Vec::new(),
                })
                .expect("append request");
        }
        let (reply, response) = mpsc::channel();
        sender.send(MutationMessage::Control(WorkerControl::Barrier(reply))).expect("barrier request");
        response.recv().expect("barrier response").expect("barrier");
        let (reply, response) = mpsc::channel();
        sender.send(MutationMessage::Control(WorkerControl::Stop(reply))).expect("stop request");
        response.recv().expect("stop response").expect("stop");
        worker.join().expect("mutation worker");

        assert!(inspect_outbox.outstanding().expect("outbox").is_empty());
        let recovered = SessionAuthority::open(journal, SessionId::new(1)).expect("recover authority");
        assert_eq!(recovered.document(DocumentId::new(1)).expect("document").text(), format!("{}base", "x".repeat(128)));
    }

    #[test]
    fn one_editor_event_persists_its_complete_revision_chain() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let journal = SessionJournal::in_directory(directory.path().join("session"));
        let authority = SessionAuthority::open(journal.clone(), SessionId::new(1)).expect("open authority");
        let outbox = MutationOutbox::in_directory(directory.path().join("outbox"));
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || mutation_loop(authority, outbox, receiver));

        let document_id = DocumentId::new(2);
        let (reply, response) = mpsc::channel();
        sender.send(MutationMessage::Register { document_id, text: "ab".to_owned(), replace_stale: true, reply }).expect("register request");
        response.recv().expect("register response").expect("register");
        sender
            .send(MutationMessage::Append {
                document_id,
                transactions: vec![
                    Transaction::new(DocumentRevision::new(0), vec![Edit::new(0..0, "x")]).expect("first transaction"),
                    Transaction::new(DocumentRevision::new(1), vec![Edit::new(1..1, "y")]).expect("second transaction"),
                ]
                .into(),
                state_deltas: Vec::new(),
            })
            .expect("append revision chain");
        let (reply, response) = mpsc::channel();
        sender.send(MutationMessage::Control(WorkerControl::Stop(reply))).expect("stop request");
        response.recv().expect("stop response").expect("stop");
        worker.join().expect("mutation worker");

        let recovered = SessionAuthority::open(journal, SessionId::new(1)).expect("recover authority");
        let document = recovered.document(document_id).expect("document");
        assert_eq!(document.revision, DocumentRevision::new(2));
        assert_eq!(document.text(), "xyab");
    }

    #[test]
    fn clean_registration_tracks_external_file_replacement_safely() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let journal = SessionJournal::in_directory(directory.path().join("session"));
        let mut authority = SessionAuthority::open(journal.clone(), SessionId::new(1)).expect("open authority");
        let outbox = MutationOutbox::in_directory(directory.path().join("outbox"));
        let client_id = ClientId::new(1);
        let document_id = DocumentId::new(7);
        authority.register_document(document_id, "stale", client_id).expect("register stale snapshot");

        let next_sequence = register_mutation_document(&mut authority, &outbox, client_id, 1, document_id, "current disk".to_owned(), true)
            .expect("replace stale clean snapshot");

        assert_eq!(next_sequence, 2);
        assert!(outbox.outstanding().expect("outbox").is_empty());
        let document = authority.document(document_id).expect("document");
        assert_eq!(document.revision, DocumentRevision::new(1));
        assert_eq!(document.text(), "current disk");

        let error = register_mutation_document(&mut authority, &outbox, client_id, next_sequence, document_id, "new conflict".to_owned(), false)
            .expect_err("recovered state must not be replaced");
        assert!(error.contains("explicit reconciliation is required"));

        let next_sequence = register_mutation_document(&mut authority, &outbox, client_id, next_sequence, document_id, "new disk".to_owned(), true)
            .expect("a newer clean disk snapshot remains authoritative");
        assert_eq!(next_sequence, 3);

        let recovered = SessionAuthority::open(journal, SessionId::new(1)).expect("recover authority");
        assert_eq!(recovered.document(document_id).expect("document").text(), "new disk");
    }
}
