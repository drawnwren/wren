use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProviderDemandKey {
    pub(super) revision: DocumentRevision,
    pub(super) visible: Range<usize>,
    pub(super) near_viewport: Range<usize>,
}

impl From<&ProviderRefresh> for ProviderDemandKey {
    fn from(refresh: &ProviderRefresh) -> Self {
        Self { revision: refresh.revision, visible: refresh.visible.clone(), near_viewport: refresh.near_viewport.clone() }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ProviderRefresh {
    pub(super) buffer_id: BufferId,
    pub(super) document_id: DocumentId,
    pub(super) revision: DocumentRevision,
    pub(super) text: FrameText,
    /// A contiguous editor revision chain. When it cannot be replayed from
    /// the provider's revision, the worker deliberately resynchronizes with
    /// this refresh's snapshot instead of guessing.
    pub(super) transactions: Vec<Transaction>,
    pub(super) bundle: LanguageBundle,
    pub(super) visible: Range<usize>,
    pub(super) near_viewport: Range<usize>,
}

#[derive(Debug, Clone)]
pub(super) struct ProviderCompletion {
    pub(super) document_id: DocumentId,
    pub(super) revision: DocumentRevision,
    pub(super) text: FrameText,
    pub(super) bundle: LanguageBundle,
    pub(super) byte: usize,
}

pub(super) struct PersistentLsp {
    pub(super) document_id: DocumentId,
    pub(super) revision: DocumentRevision,
    pub(super) uri: String,
    pub(super) client: LspClient,
    pub(super) server: LanguageServerInvocation,
    pub(super) root: PathBuf,
    pub(super) open_documents: BTreeMap<DocumentId, LspOpenDocument>,
    pub(super) capabilities: LspCapabilities,
    pub(super) semantic_due: Option<Instant>,
}

#[derive(Debug, Clone)]
pub(super) struct LspOpenDocument {
    pub(super) uri: String,
    pub(super) revision: DocumentRevision,
}

pub(super) type LspCompletion = Box<dyn FnOnce(&mut App) + Send>;

pub(super) enum ProviderWorkerMessage {
    Refresh(Box<ProviderRefresh>),
    Complete(Box<ProviderCompletion>),
    HighlightNow(Box<ImmediateHighlight>),
    Wake,
    Stop,
}

pub(super) struct ImmediateHighlight {
    pub(super) document_id: DocumentId,
    pub(super) revision: DocumentRevision,
    pub(super) text: Arc<str>,
    pub(super) bundle: LanguageBundle,
    pub(super) reply: mpsc::SyncSender<Result<Vec<HighlightSpan>, String>>,
}

pub(super) enum ProviderWorkerResult {
    Decorations { buffer_id: BufferId, document_id: DocumentId, revision: DocumentRevision, spans: Vec<HighlightSpan>, ranges: Vec<Range<usize>> },
    Completion { document_id: DocumentId, session: CompletionSession },
    Failed { document_id: DocumentId, message: String },
}

pub(super) struct ProviderWorker {
    sender: Option<mpsc::SyncSender<ProviderWorkerMessage>>,
    immediate_sender: Option<mpsc::SyncSender<ProviderWorkerMessage>>,
    results: mpsc::Receiver<ProviderWorkerResult>,
    join: Option<JoinHandle<()>>,
}

pub(super) struct GitHunkRequest {
    pub(super) buffer_id: BufferId,
    pub(super) revision: DocumentRevision,
    pub(super) before: Arc<str>,
    pub(super) after: FrameText,
}

pub(super) struct PendingGitHunkRefresh {
    pub(super) due: Instant,
    pub(super) request: GitHunkRequest,
}

pub(super) struct GitHunkResult {
    pub(super) buffer_id: BufferId,
    pub(super) revision: DocumentRevision,
    pub(super) hunks: Vec<GitHunk>,
}

pub(super) struct GitHunkWorker {
    sender: mpsc::SyncSender<GitHunkRequest>,
    results: mpsc::Receiver<GitHunkResult>,
}

impl GitHunkWorker {
    pub(super) fn start() -> Result<Self> {
        Self::start_with_limits(wren_scheduling::RuntimeLimits::default())
    }

    fn start_with_limits(limits: wren_scheduling::RuntimeLimits) -> Result<Self> {
        let capacity = limits.provider_demand_documents.min(2).max(1);
        let (sender, requests) = mpsc::sync_channel(capacity);
        let (results, receiver) = mpsc::sync_channel(capacity);
        wren_scheduling::spawn_background("wren-git-hunks", move || git_hunk_loop(requests, results)).context("spawn Git hunk worker")?;
        Ok(Self { sender, results: receiver })
    }

    pub(super) fn refresh(&self, request: GitHunkRequest) {
        let _ = self.sender.try_send(request);
    }

    pub(super) fn try_result(&self) -> Option<GitHunkResult> {
        self.results.try_recv().ok()
    }
}

fn git_hunk_loop(requests: mpsc::Receiver<GitHunkRequest>, results: mpsc::SyncSender<GitHunkResult>) {
    while let Ok(request) = requests.recv() {
        let after = request.after.materialize_for_task();
        let hunks = git_hunks(&request.before, &after);
        if matches!(
            results.try_send(GitHunkResult { buffer_id: request.buffer_id, revision: request.revision, hunks }),
            Err(mpsc::TrySendError::Disconnected(_))
        ) {
            return;
        }
    }
}

pub(super) fn join_worker_thread(join: &mut Option<JoinHandle<()>>) {
    if let Some(join) = join.take() {
        let _ = join.join();
    }
}

impl ProviderWorker {
    pub(super) fn start() -> Result<Self> {
        Self::start_with_limits(wren_scheduling::RuntimeLimits::default())
    }

    fn start_with_limits(limits: wren_scheduling::RuntimeLimits) -> Result<Self> {
        let (sender, requests) = mpsc::sync_channel(limits.provider_revision_slots.max(1));
        let (immediate_sender, immediate_requests) = mpsc::sync_channel(limits.provider_demand_documents.div_ceil(2).max(1));
        let (results, receiver) = mpsc::sync_channel(limits.task_slots.saturating_add(limits.provider_demand_documents).max(1));
        #[cfg(not(test))]
        let executable = env::current_exe().context("locate provider executable")?;
        let join = wren_scheduling::spawn_background("wren-provider-supervisor", move || {
            #[cfg(test)]
            provider_actor_loop(requests, immediate_requests, results, limits.provider_demand_documents);
            #[cfg(not(test))]
            provider_process_loop(executable, requests, immediate_requests, results, limits.provider_demand_documents);
        })
        .context("spawn provider supervisor")?;
        Ok(Self { sender: Some(sender), immediate_sender: Some(immediate_sender), results: receiver, join: Some(join) })
    }

    pub(super) fn try_refresh(&self, refresh: ProviderRefresh) -> bool {
        self.sender.as_ref().is_some_and(|sender| sender.try_send(ProviderWorkerMessage::Refresh(Box::new(refresh))).is_ok())
    }

    pub(super) fn try_complete(&self, completion: ProviderCompletion) -> bool {
        self.sender.as_ref().is_some_and(|sender| sender.try_send(ProviderWorkerMessage::Complete(Box::new(completion))).is_ok())
    }

    pub(super) fn try_result(&self) -> Option<ProviderWorkerResult> {
        self.results.try_recv().ok()
    }

    pub(super) fn highlight_now(
        &self,
        document_id: DocumentId,
        revision: DocumentRevision,
        text: Arc<str>,
        bundle: LanguageBundle,
    ) -> Result<Vec<HighlightSpan>> {
        let (reply, response) = mpsc::sync_channel(1);
        let immediate_sender = self.immediate_sender.as_ref().ok_or_else(|| anyhow!("provider process stopped"))?;
        immediate_sender
            .try_send(ProviderWorkerMessage::HighlightNow(Box::new(ImmediateHighlight { document_id, revision, text, bundle, reply })))
            .map_err(|error| anyhow!("immediate provider highlight queue unavailable: {error}"))?;
        // Wake an idle provider without putting the synchronous request behind
        // already queued viewport or completion work. A full background queue
        // already guarantees that the worker is awake.
        let sender = self.sender.as_ref().ok_or_else(|| anyhow!("provider process stopped"))?;
        if matches!(sender.try_send(ProviderWorkerMessage::Wake), Err(mpsc::TrySendError::Disconnected(_))) {
            return Err(anyhow!("provider process stopped"));
        }
        response.recv_timeout(Duration::from_millis(200)).map_err(|_| anyhow!("immediate provider highlight timed out"))?.map_err(anyhow::Error::msg)
    }
}

impl Drop for ProviderWorker {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.try_send(ProviderWorkerMessage::Stop);
        }
        if let Some(sender) = self.immediate_sender.take() {
            let _ = sender.try_send(ProviderWorkerMessage::Stop);
        }
        // A bounded queue may already be full when teardown starts. Closing
        // both producers is the guaranteed stop signal in that case: after
        // draining any already accepted work the worker sees disconnection
        // instead of waiting forever for a Stop message it could not enqueue.
        join_worker_thread(&mut self.join);
    }
}

#[cfg(not(test))]
fn provider_process_loop(
    executable: PathBuf,
    requests: mpsc::Receiver<ProviderWorkerMessage>,
    immediate_requests: mpsc::Receiver<ProviderWorkerMessage>,
    results: mpsc::SyncSender<ProviderWorkerResult>,
    demand_capacity: usize,
) {
    let supervisor = ProviderSupervisor::spawn_with_args(&executable, ["--internal-provider-host"]);
    let mut supervisor = match supervisor {
        Ok(supervisor) => supervisor,
        Err(error) => {
            let message = error.to_string();
            return provider_loop_with_demand_capacity(requests, immediate_requests, results, demand_capacity, move |_| Err(provider_error(&message)));
        }
    };
    if let Err(error) = supervisor.request(&ProviderRequest::Hello { protocol: 1 }) {
        let message = error.to_string();
        return provider_loop_with_demand_capacity(requests, immediate_requests, results, demand_capacity, move |_| Err(provider_error(&message)));
    }
    provider_loop_with_demand_capacity(requests, immediate_requests, results, demand_capacity, |request| supervisor.request(request));
}

#[cfg(test)]
fn provider_actor_loop(
    requests: mpsc::Receiver<ProviderWorkerMessage>,
    immediate_requests: mpsc::Receiver<ProviderWorkerMessage>,
    results: mpsc::SyncSender<ProviderWorkerResult>,
    demand_capacity: usize,
) {
    let mut actor = ProviderActor::default();
    provider_loop_with_demand_capacity(requests, immediate_requests, results, demand_capacity, |request| actor.handle(request.clone()));
}

#[cfg(test)]
pub(super) fn provider_loop(
    requests: mpsc::Receiver<ProviderWorkerMessage>,
    immediate_requests: mpsc::Receiver<ProviderWorkerMessage>,
    results: mpsc::SyncSender<ProviderWorkerResult>,
    request: impl FnMut(&ProviderRequest) -> Result<ProviderResponse, wren_provider::ProviderError>,
) {
    provider_loop_with_demand_capacity(requests, immediate_requests, results, wren_scheduling::RuntimeLimits::default().provider_demand_documents, request);
}

fn provider_loop_with_demand_capacity(
    requests: mpsc::Receiver<ProviderWorkerMessage>,
    immediate_requests: mpsc::Receiver<ProviderWorkerMessage>,
    results: mpsc::SyncSender<ProviderWorkerResult>,
    demand_capacity: usize,
    mut request: impl FnMut(&ProviderRequest) -> Result<ProviderResponse, wren_provider::ProviderError>,
) {
    // Revision payloads and viewport demand deliberately have different
    // delivery semantics. The former remains attached to the newest demand
    // for its document; the latter is scheduled by the provider's bounded
    // latest-wins queue. Keeping the payload map pruned with queue evictions
    // prevents an unbounded side queue from defeating the scheduler.
    let mut uploaded = UploadedProviderDocuments::new();
    let mut demands = wren_provider::LatestDemandQueue::new(demand_capacity);
    let mut pending_refreshes = BTreeMap::new();
    let mut controls = std::collections::VecDeque::new();
    loop {
        let message = match immediate_requests.try_recv() {
            Ok(message) => Some(message),
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {
                if let Some(queued) = demands.pop()
                    && let Some(refresh) = pending_refreshes.remove(&queued.document_id)
                {
                    let result = refresh_provider(refresh, &mut uploaded, &mut request);
                    if matches!(results.try_send(result), Err(mpsc::TrySendError::Disconnected(_))) {
                        return;
                    }
                    continue;
                }
                controls.pop_front()
            }
        }
        .or_else(|| requests.recv().ok());
        let Some(message) = message else {
            return;
        };
        let result = match message {
            ProviderWorkerMessage::Refresh(refresh) => {
                schedule_provider_refresh(*refresh, &mut demands, &mut pending_refreshes);
                // Drain the bounded ingress before parsing so a typing burst
                // produces one newest demand rather than redundant provider
                // parses. Completion and control work keep FIFO ordering.
                while let Ok(message) = requests.try_recv() {
                    match message {
                        ProviderWorkerMessage::Refresh(refresh) => schedule_provider_refresh(*refresh, &mut demands, &mut pending_refreshes),
                        control => controls.push_back(control),
                    }
                }
                None
            }
            ProviderWorkerMessage::Complete(completion) => Some(complete_provider(*completion, &mut uploaded, &mut request)),
            ProviderWorkerMessage::HighlightNow(highlight) => {
                highlight_immediately(*highlight, &mut uploaded, &mut request);
                None
            }
            ProviderWorkerMessage::Wake => None,
            ProviderWorkerMessage::Stop => return,
        };
        if result.is_some_and(|result| matches!(results.try_send(result), Err(mpsc::TrySendError::Disconnected(_)))) {
            return;
        }
    }
}

struct UploadedProviderDocument {
    revision: DocumentRevision,
    generation: wren_types::ProviderGeneration,
    text: FrameText,
}

type UploadedProviderDocuments = BTreeMap<DocumentId, UploadedProviderDocument>;

fn schedule_provider_refresh(refresh: ProviderRefresh, demands: &mut wren_provider::LatestDemandQueue, pending: &mut BTreeMap<DocumentId, ProviderRefresh>) {
    let document_id = refresh.document_id;
    let demand = ProviderDemand {
        revision: refresh.revision,
        visible: vec![refresh.visible.clone()],
        near_viewport: vec![refresh.near_viewport.clone()],
        priority: Priority::Visible,
    };
    if let Some(previous) = pending.remove(&document_id) {
        pending.insert(document_id, coalesce_refresh(previous, refresh));
    } else {
        pending.insert(document_id, refresh);
    }
    if let Some(evicted) = demands.push(document_id, demand) {
        pending.remove(&evicted);
    }
}

fn coalesce_refresh(previous: ProviderRefresh, mut newest: ProviderRefresh) -> ProviderRefresh {
    if newest.revision < previous.revision {
        return previous;
    }
    let mut transactions = previous.transactions;
    transactions.extend(newest.transactions);
    newest.transactions = transactions;
    newest
}

fn unexpected_provider_response(context: &str, response: ProviderResponse) -> wren_provider::ProviderError {
    provider_error(&format!("unexpected {context} response {response:?}"))
}

fn provider_error(message: &str) -> wren_provider::ProviderError {
    wren_provider::ProviderError::Json(serde_json::Error::io(std::io::Error::other(message.to_owned())))
}

fn ensure_provider_document(
    document_id: DocumentId,
    revision: DocumentRevision,
    text: FrameText,
    bundle: LanguageBundle,
    transactions: &[Transaction],
    uploaded: &mut UploadedProviderDocuments,
    request: &mut impl FnMut(&ProviderRequest) -> Result<ProviderResponse, wren_provider::ProviderError>,
) -> Result<(), wren_provider::ProviderError> {
    let generation = bundle.provider_generation();
    if uploaded.get(&document_id).is_some_and(|document| document.revision == revision && document.generation == generation) {
        return Ok(());
    }
    let (context, update) = if let Some(document) = uploaded.get(&document_id)
        && document.generation == generation
        && document.text.same_snapshot(&text)
    {
        ("document revision advance", ProviderRequest::AdvanceDocumentRevision { document_id, from_revision: document.revision, revision, generation })
    } else if let Some(document) = uploaded.get(&document_id)
        && document.generation == generation
        && transactions_form_chain(document.revision, revision, transactions)
    {
        (
            "document transaction replay",
            ProviderRequest::ApplyTransactions { document_id, from_revision: document.revision, revision, generation, transactions: transactions.to_vec() },
        )
    } else {
        ("document open", ProviderRequest::OpenDocument { document_id, revision, text: text.materialize_for_task().as_ref().into(), bundle })
    };
    match request(&update)? {
        ProviderResponse::Updated { .. } => {
            uploaded.insert(document_id, UploadedProviderDocument { revision, generation, text });
            Ok(())
        }
        response => Err(unexpected_provider_response(context, response)),
    }
}

fn transactions_form_chain(from_revision: DocumentRevision, revision: DocumentRevision, transactions: &[Transaction]) -> bool {
    if transactions.is_empty() {
        return false;
    }
    let mut current = from_revision;
    for transaction in transactions {
        if transaction.base_revision() != current {
            return false;
        }
        let Some(next) = current.next() else {
            return false;
        };
        current = next;
    }
    current == revision
}

fn refresh_provider(
    refresh: ProviderRefresh,
    uploaded: &mut UploadedProviderDocuments,
    request: &mut impl FnMut(&ProviderRequest) -> Result<ProviderResponse, wren_provider::ProviderError>,
) -> ProviderWorkerResult {
    let document_id = refresh.document_id;
    let revision = refresh.revision;
    let outcome = ensure_provider_document(document_id, revision, refresh.text, refresh.bundle, &refresh.transactions, uploaded, request).and_then(|()| {
        request(&ProviderRequest::Demand {
            document_id,
            demand: ProviderDemand { revision, visible: vec![refresh.visible], near_viewport: vec![refresh.near_viewport], priority: Priority::Visible },
        })
    });
    checked_provider_response(document_id, uploaded, outcome, |response| match response {
        ProviderResponse::Highlight(highlight)
            if highlight.freshness == Freshness::Fresh
                && matches!(
                    highlight.key,
                    FreshnessKey::Document { document_revision, .. }
                        if document_revision == revision
                ) =>
        {
            Ok(ProviderWorkerResult::Decorations {
                buffer_id: refresh.buffer_id,
                document_id,
                revision,
                spans: highlight.spans,
                ranges: highlight.requested_ranges,
            })
        }
        response => Err(format!("stale or unexpected highlight {response:?}")),
    })
    .unwrap_or_else(|message| ProviderWorkerResult::Failed { document_id, message })
}

fn complete_provider(
    completion: ProviderCompletion,
    uploaded: &mut UploadedProviderDocuments,
    request: &mut impl FnMut(&ProviderRequest) -> Result<ProviderResponse, wren_provider::ProviderError>,
) -> ProviderWorkerResult {
    let document_id = completion.document_id;
    let revision = completion.revision;
    let outcome = ensure_provider_document(document_id, revision, completion.text, completion.bundle, &[], uploaded, request)
        .and_then(|()| request(&ProviderRequest::Complete { document_id, revision, byte: completion.byte }));
    checked_provider_response(document_id, uploaded, outcome, |response| match response {
        ProviderResponse::Completion(result) if result.freshness == Freshness::Fresh => Ok(ProviderWorkerResult::Completion {
            document_id,
            session: CompletionSession { revision, replace: result.replace, candidates: result.candidates },
        }),
        response => Err(format!("stale or unexpected completion {response:?}")),
    })
    .unwrap_or_else(|message| ProviderWorkerResult::Failed { document_id, message })
}

fn highlight_immediately(
    highlight: ImmediateHighlight,
    uploaded: &mut UploadedProviderDocuments,
    request: &mut impl FnMut(&ProviderRequest) -> Result<ProviderResponse, wren_provider::ProviderError>,
) {
    let ImmediateHighlight { document_id, revision, text, bundle, reply } = highlight;
    let text_len = text.len();
    let outcome = ensure_provider_document(document_id, revision, FrameText::from(text.as_ref()), bundle, &[], uploaded, request).and_then(|()| {
        request(&ProviderRequest::Demand {
            document_id,
            demand: ProviderDemand { revision, visible: std::iter::once(0..text_len).collect(), near_viewport: Vec::new(), priority: Priority::Visible },
        })
    });
    let result = checked_provider_response(document_id, uploaded, outcome, |response| match response {
        ProviderResponse::Highlight(highlight) if highlight.freshness == Freshness::Fresh => Ok(highlight.spans),
        response => Err(format!("stale or unexpected highlight {response:?}")),
    });
    let _ = reply.send(result);
}

fn checked_provider_response<T>(
    document_id: DocumentId,
    uploaded: &mut UploadedProviderDocuments,
    response: Result<ProviderResponse, wren_provider::ProviderError>,
    accept: impl FnOnce(ProviderResponse) -> Result<T, String>,
) -> Result<T, String> {
    let result = response.map_err(|error| error.to_string()).and_then(accept);
    if result.is_err() {
        uploaded.remove(&document_id);
    }
    result
}
