use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProviderDemandKey {
    pub(super) revision: DocumentRevision,
    pub(super) visible_start: usize,
    pub(super) visible_end: usize,
    pub(super) near_start: usize,
    pub(super) near_end: usize,
}

impl From<&ProviderRefresh> for ProviderDemandKey {
    fn from(refresh: &ProviderRefresh) -> Self {
        Self {
            revision: refresh.revision,
            visible_start: refresh.visible.start,
            visible_end: refresh.visible.end,
            near_start: refresh.near_viewport.start,
            near_end: refresh.near_viewport.end,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ProviderRefresh {
    pub(super) buffer_id: BufferId,
    pub(super) document_id: DocumentId,
    pub(super) revision: DocumentRevision,
    pub(super) text: FrameText,
    pub(super) bundle: LanguageBundle,
    pub(super) visible: Range<usize>,
    pub(super) near_viewport: Range<usize>,
}

#[derive(Debug, Clone)]
pub(super) struct ProviderCompletion {
    pub(super) document_id: DocumentId,
    pub(super) revision: DocumentRevision,
    pub(super) text: Arc<str>,
    pub(super) bundle: LanguageBundle,
    pub(super) byte: usize,
}

#[derive(Debug, Clone)]
pub(super) struct LspCompletion {
    pub(super) revision: DocumentRevision,
    pub(super) replace: Range<usize>,
    pub(super) candidates: Vec<CompletionCandidate>,
}

pub(super) struct PersistentLsp {
    pub(super) document_id: DocumentId,
    pub(super) revision: DocumentRevision,
    pub(super) uri: String,
    pub(super) client: LspClient,
    pub(super) server: LanguageServerInvocation,
    pub(super) root: PathBuf,
    pub(super) open_documents: BTreeMap<DocumentId, LspOpenDocument>,
    pub(super) semantic_legend: Option<SemanticTokenLegend>,
    pub(super) semantic_due: Option<Instant>,
}

#[derive(Debug, Clone)]
pub(super) struct LspOpenDocument {
    pub(super) uri: String,
    pub(super) revision: DocumentRevision,
}

pub(super) enum LspBackgroundOperation {
    Location {
        method: String,
    },
    Hover {
        method: String,
        document_id: DocumentId,
        revision: DocumentRevision,
    },
    Semantic {
        buffer_id: BufferId,
        revision: DocumentRevision,
        text: Box<str>,
        legend: SemanticTokenLegend,
    },
}

pub(super) struct LspBackgroundResult {
    pub(super) lsp: PersistentLsp,
    pub(super) operation: LspBackgroundOperation,
    pub(super) outcome: Result<serde_json::Value, String>,
}

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
    pub(super) reply: mpsc::Sender<Result<Vec<HighlightSpan>, String>>,
}

pub(super) enum ProviderWorkerResult {
    Decorations {
        buffer_id: BufferId,
        document_id: DocumentId,
        revision: DocumentRevision,
        spans: Vec<HighlightSpan>,
        ranges: Vec<Range<usize>>,
    },
    Completion {
        document_id: DocumentId,
        session: CompletionSession,
    },
    Failed {
        document_id: DocumentId,
        message: String,
    },
}

pub(super) struct ProviderWorker {
    sender: mpsc::SyncSender<ProviderWorkerMessage>,
    immediate_sender: mpsc::Sender<ProviderWorkerMessage>,
    results: mpsc::Receiver<ProviderWorkerResult>,
    join: Option<JoinHandle<()>>,
}

pub(super) struct GitHunkRequest {
    pub(super) buffer_id: BufferId,
    pub(super) revision: DocumentRevision,
    pub(super) before: Arc<str>,
    pub(super) after: FrameText,
}

pub(super) struct GitHunkResult {
    pub(super) buffer_id: BufferId,
    pub(super) revision: DocumentRevision,
    pub(super) hunks: Vec<GitHunk>,
}

pub(super) enum GitHunkMessage {
    Refresh(GitHunkRequest),
    Stop,
}

pub(super) struct GitHunkWorker {
    sender: mpsc::Sender<GitHunkMessage>,
    results: mpsc::Receiver<GitHunkResult>,
    join: Option<JoinHandle<()>>,
}

impl GitHunkWorker {
    pub(super) fn start() -> Result<Self> {
        let (sender, requests) = mpsc::channel();
        let (results, receiver) = mpsc::channel();
        let join = thread::Builder::new()
            .name("wren-git-hunks".to_owned())
            .spawn(move || git_hunk_loop(requests, results))
            .context("spawn Git hunk worker")?;
        Ok(Self {
            sender,
            results: receiver,
            join: Some(join),
        })
    }

    pub(super) fn refresh(&self, request: GitHunkRequest) {
        let _ = self.sender.send(GitHunkMessage::Refresh(request));
    }

    pub(super) fn try_result(&self) -> Option<GitHunkResult> {
        self.results.try_recv().ok()
    }
}

impl Drop for GitHunkWorker {
    fn drop(&mut self) {
        let _ = self.sender.send(GitHunkMessage::Stop);
        join_worker_thread(&mut self.join);
    }
}

fn git_hunk_loop(requests: mpsc::Receiver<GitHunkMessage>, results: mpsc::Sender<GitHunkResult>) {
    while let Ok(message) = requests.recv() {
        let GitHunkMessage::Refresh(mut request) = message else {
            return;
        };
        loop {
            match requests.recv_timeout(Duration::from_millis(50)) {
                Ok(GitHunkMessage::Refresh(newer)) => request = newer,
                Ok(GitHunkMessage::Stop) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        let after = request.after.shared();
        let hunks = git_hunks(&request.before, &after);
        if results
            .send(GitHunkResult {
                buffer_id: request.buffer_id,
                revision: request.revision,
                hunks,
            })
            .is_err()
        {
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
        let (sender, requests) = mpsc::sync_channel(8);
        let (immediate_sender, immediate_requests) = mpsc::channel();
        let (results, receiver) = mpsc::channel();
        #[cfg(not(test))]
        let executable = env::current_exe().context("locate provider executable")?;
        let join = thread::Builder::new()
            .name("wren-provider-supervisor".to_owned())
            .spawn(move || {
                #[cfg(test)]
                provider_actor_loop(requests, immediate_requests, results);
                #[cfg(not(test))]
                provider_process_loop(executable, requests, immediate_requests, results);
            })
            .context("spawn provider supervisor")?;
        Ok(Self {
            sender,
            immediate_sender,
            results: receiver,
            join: Some(join),
        })
    }

    pub(super) fn try_refresh(&self, refresh: ProviderRefresh) -> bool {
        self.sender
            .try_send(ProviderWorkerMessage::Refresh(Box::new(refresh)))
            .is_ok()
    }

    pub(super) fn try_complete(&self, completion: ProviderCompletion) -> bool {
        self.sender
            .try_send(ProviderWorkerMessage::Complete(Box::new(completion)))
            .is_ok()
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
        let (reply, response) = mpsc::channel();
        self.immediate_sender
            .send(ProviderWorkerMessage::HighlightNow(Box::new(
                ImmediateHighlight {
                    document_id,
                    revision,
                    text,
                    bundle,
                    reply,
                },
            )))
            .map_err(|_| anyhow!("provider process stopped"))?;
        // Wake an idle provider without putting the synchronous request behind
        // already queued viewport or completion work. A full background queue
        // already guarantees that the worker is awake.
        if matches!(
            self.sender.try_send(ProviderWorkerMessage::Wake),
            Err(mpsc::TrySendError::Disconnected(_))
        ) {
            return Err(anyhow!("provider process stopped"));
        }
        response
            .recv_timeout(Duration::from_millis(200))
            .map_err(|_| anyhow!("provider first-frame highlight timed out"))?
            .map_err(anyhow::Error::msg)
    }
}

impl Drop for ProviderWorker {
    fn drop(&mut self) {
        let _ = self.sender.send(ProviderWorkerMessage::Stop);
        join_worker_thread(&mut self.join);
    }
}

#[cfg(not(test))]
fn provider_process_loop(
    executable: PathBuf,
    requests: mpsc::Receiver<ProviderWorkerMessage>,
    immediate_requests: mpsc::Receiver<ProviderWorkerMessage>,
    results: mpsc::Sender<ProviderWorkerResult>,
) {
    let supervisor = ProviderSupervisor::spawn_with_args(&executable, ["--internal-provider-host"]);
    let mut supervisor = match supervisor {
        Ok(supervisor) => supervisor,
        Err(error) => {
            provider_failures_until_stop(
                &requests,
                &immediate_requests,
                &results,
                error.to_string(),
            );
            return;
        }
    };
    if let Err(error) = supervisor.request(&ProviderRequest::Hello { protocol: 1 }) {
        provider_failures_until_stop(&requests, &immediate_requests, &results, error.to_string());
        return;
    }
    provider_loop(requests, immediate_requests, results, |request| {
        supervisor.request(request)
    });
}

#[cfg(test)]
fn provider_actor_loop(
    requests: mpsc::Receiver<ProviderWorkerMessage>,
    immediate_requests: mpsc::Receiver<ProviderWorkerMessage>,
    results: mpsc::Sender<ProviderWorkerResult>,
) {
    let mut actor = ProviderActor::default();
    provider_loop(requests, immediate_requests, results, |request| {
        actor.handle(request.clone())
    });
}

pub(super) fn provider_loop(
    requests: mpsc::Receiver<ProviderWorkerMessage>,
    immediate_requests: mpsc::Receiver<ProviderWorkerMessage>,
    results: mpsc::Sender<ProviderWorkerResult>,
    mut request: impl FnMut(&ProviderRequest) -> Result<ProviderResponse, wren_provider::ProviderError>,
) {
    // A viewport demand is not a document update. Keeping these identities
    // separate avoids serializing and reparsing the entire buffer on every
    // scroll while still replacing the provider snapshot on each revision.
    let mut uploaded =
        BTreeMap::<DocumentId, (DocumentRevision, wren_types::ProviderGeneration)>::new();
    loop {
        let Some(message) = next_provider_message(&requests, &immediate_requests) else {
            return;
        };
        match message {
            ProviderWorkerMessage::Refresh(refresh) => {
                let result = refresh_provider(*refresh, &mut uploaded, &mut request);
                if results.send(result).is_err() {
                    return;
                }
            }
            ProviderWorkerMessage::Complete(completion) => {
                let result = complete_provider(*completion, &mut uploaded, &mut request);
                if results.send(result).is_err() {
                    return;
                }
            }
            ProviderWorkerMessage::HighlightNow(highlight) => {
                highlight_immediately(*highlight, &mut uploaded, &mut request);
            }
            ProviderWorkerMessage::Wake => {}
            ProviderWorkerMessage::Stop => return,
        }
    }
}

type UploadedProviderDocuments =
    BTreeMap<DocumentId, (DocumentRevision, wren_types::ProviderGeneration)>;

fn next_provider_message(
    requests: &mpsc::Receiver<ProviderWorkerMessage>,
    immediate_requests: &mpsc::Receiver<ProviderWorkerMessage>,
) -> Option<ProviderWorkerMessage> {
    match immediate_requests.try_recv() {
        Ok(message) => Some(message),
        Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => requests.recv().ok(),
    }
}

fn unexpected_provider_response(
    context: &str,
    response: ProviderResponse,
) -> wren_provider::ProviderError {
    wren_provider::ProviderError::Json(serde_json::Error::io(std::io::Error::other(format!(
        "unexpected {context} response {response:?}"
    ))))
}

fn ensure_provider_document(
    document_id: DocumentId,
    revision: DocumentRevision,
    text: Arc<str>,
    bundle: LanguageBundle,
    uploaded: &mut UploadedProviderDocuments,
    request: &mut impl FnMut(&ProviderRequest) -> Result<ProviderResponse, wren_provider::ProviderError>,
) -> Result<(), wren_provider::ProviderError> {
    let identity = (revision, bundle.provider_generation());
    if uploaded.get(&document_id) == Some(&identity) {
        return Ok(());
    }
    match request(&ProviderRequest::UpdateDocument {
        document_id,
        revision,
        text: text.as_ref().into(),
        bundle,
    })? {
        ProviderResponse::Updated { .. } => {
            uploaded.insert(document_id, identity);
            Ok(())
        }
        response => Err(unexpected_provider_response("document update", response)),
    }
}

fn refresh_provider(
    refresh: ProviderRefresh,
    uploaded: &mut UploadedProviderDocuments,
    request: &mut impl FnMut(&ProviderRequest) -> Result<ProviderResponse, wren_provider::ProviderError>,
) -> ProviderWorkerResult {
    let document_id = refresh.document_id;
    let revision = refresh.revision;
    let text = refresh.text.shared();
    let outcome = ensure_provider_document(
        document_id,
        revision,
        text,
        refresh.bundle,
        uploaded,
        request,
    )
    .and_then(|()| {
        request(&ProviderRequest::Demand {
            document_id,
            demand: ProviderDemand {
                revision,
                visible: vec![refresh.visible],
                near_viewport: vec![refresh.near_viewport],
                priority: Priority::Visible,
            },
        })
    });
    let result = match outcome {
        Ok(ProviderResponse::Highlight(highlight))
            if highlight.freshness == Freshness::Fresh
                && matches!(
                    highlight.key,
                    FreshnessKey::Document { document_revision, .. }
                        if document_revision == revision
                ) =>
        {
            ProviderWorkerResult::Decorations {
                buffer_id: refresh.buffer_id,
                document_id,
                revision,
                spans: highlight.spans,
                ranges: highlight.requested_ranges,
            }
        }
        Ok(response) => ProviderWorkerResult::Failed {
            document_id,
            message: format!("stale or unexpected highlight {response:?}"),
        },
        Err(error) => ProviderWorkerResult::Failed {
            document_id,
            message: error.to_string(),
        },
    };
    if matches!(result, ProviderWorkerResult::Failed { .. }) {
        uploaded.remove(&document_id);
    }
    result
}

fn complete_provider(
    completion: ProviderCompletion,
    uploaded: &mut UploadedProviderDocuments,
    request: &mut impl FnMut(&ProviderRequest) -> Result<ProviderResponse, wren_provider::ProviderError>,
) -> ProviderWorkerResult {
    let document_id = completion.document_id;
    let revision = completion.revision;
    let outcome = ensure_provider_document(
        document_id,
        revision,
        completion.text,
        completion.bundle,
        uploaded,
        request,
    )
    .and_then(|()| {
        request(&ProviderRequest::Complete {
            document_id,
            revision,
            byte: completion.byte,
        })
    });
    let result = match outcome {
        Ok(ProviderResponse::Completion(result)) if result.freshness == Freshness::Fresh => {
            ProviderWorkerResult::Completion {
                document_id,
                session: CompletionSession {
                    revision,
                    replace: result.replace,
                    candidates: result.candidates,
                },
            }
        }
        Ok(response) => ProviderWorkerResult::Failed {
            document_id,
            message: format!("stale or unexpected completion {response:?}"),
        },
        Err(error) => ProviderWorkerResult::Failed {
            document_id,
            message: error.to_string(),
        },
    };
    if matches!(result, ProviderWorkerResult::Failed { .. }) {
        uploaded.remove(&document_id);
    }
    result
}

fn highlight_immediately(
    highlight: ImmediateHighlight,
    uploaded: &mut UploadedProviderDocuments,
    request: &mut impl FnMut(&ProviderRequest) -> Result<ProviderResponse, wren_provider::ProviderError>,
) {
    let ImmediateHighlight {
        document_id,
        revision,
        text,
        bundle,
        reply,
    } = highlight;
    let text_len = text.len();
    let outcome = ensure_provider_document(document_id, revision, text, bundle, uploaded, request)
        .and_then(|()| {
            request(&ProviderRequest::Demand {
                document_id,
                demand: ProviderDemand {
                    revision,
                    visible: std::iter::once(0..text_len).collect(),
                    near_viewport: Vec::new(),
                    priority: Priority::Visible,
                },
            })
        });
    let result = match outcome {
        Ok(ProviderResponse::Highlight(highlight)) if highlight.freshness == Freshness::Fresh => {
            Ok(highlight.spans)
        }
        Ok(response) => Err(format!("stale or unexpected highlight {response:?}")),
        Err(error) => Err(error.to_string()),
    };
    if result.is_err() {
        uploaded.remove(&document_id);
    }
    let _ = reply.send(result);
}

#[cfg(not(test))]
fn provider_failures_until_stop(
    requests: &mpsc::Receiver<ProviderWorkerMessage>,
    immediate_requests: &mpsc::Receiver<ProviderWorkerMessage>,
    results: &mpsc::Sender<ProviderWorkerResult>,
    message: String,
) {
    loop {
        let request = match immediate_requests.try_recv() {
            Ok(request) => request,
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {
                let Ok(request) = requests.recv() else {
                    return;
                };
                request
            }
        };
        match request {
            ProviderWorkerMessage::Refresh(refresh) => {
                let _ = results.send(ProviderWorkerResult::Failed {
                    document_id: refresh.document_id,
                    message: message.clone(),
                });
            }
            ProviderWorkerMessage::Complete(completion) => {
                let _ = results.send(ProviderWorkerResult::Failed {
                    document_id: completion.document_id,
                    message: message.clone(),
                });
            }
            ProviderWorkerMessage::HighlightNow(highlight) => {
                let ImmediateHighlight {
                    document_id, reply, ..
                } = *highlight;
                let _ = reply.send(Err(format!("document {document_id:?}: {message}")));
            }
            ProviderWorkerMessage::Wake => {}
            ProviderWorkerMessage::Stop => break,
        }
    }
}
