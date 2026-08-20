use super::*;

impl App {
    pub(super) fn active_language_server(&self) -> Option<LanguageServerInvocation> {
        language_server_invocation(self.active.document.presentation_path()).filter(|server| executable_exists(&server.program))
    }

    pub(super) fn schedule_workspace_lsp_start(&mut self) {
        self.lsp_start_due = self.lsp_start_candidate().map(|_| Instant::now() + LSP_START_IDLE_PERIOD);
    }

    pub(super) fn poll_lsp_start_due(&mut self) -> bool {
        if self.lsp_start_due.is_some_and(|due| Instant::now() >= due) {
            self.begin_lsp_start();
        }
        false
    }

    pub(super) fn begin_lsp_start(&mut self) {
        // A startup owns a live child as soon as its worker is spawned. Buffer
        // navigation must wait for it and attach the new document afterward,
        // never drop the receiver and implicitly kill/restart that child.
        if let Some(server) = self.lsp_start_candidate() {
            self.start_active_lsp(server);
        }
    }

    fn lsp_start_candidate(&mut self) -> Option<LanguageServerInvocation> {
        self.lsp_start_due = None;
        if self.lsp_job.is_some() || self.activate_available_lsp() {
            return None;
        }
        let server = self.active_language_server();
        if server.is_none() {
            // Unsupported buffers must not tear down the root workspace server.
            self.suspend_lsp_semantics();
        }
        server
    }

    fn start_active_lsp(&mut self, server: LanguageServerInvocation) {
        if self.pending_lsp_request.is_some_and(|request| request.hover) {
            self.pending_lsp_request = None;
        }
        #[cfg(test)]
        {
            let _ = server;
        }
        #[cfg(not(test))]
        {
            let Some(path) = self.active.document.presentation_path().map(Path::to_path_buf) else {
                return;
            };
            let root = self.lsp_root();
            let document_id = self.active.document_id;
            let revision = self.active.editor.revision();
            let text = self.active.editor.contents();
            let language_id = server.language_id.clone().into_boxed_str();
            let server_state = server.clone();
            let root_state = root.clone();
            let environment = env::vars().map(|(name, value)| (name.into_boxed_str(), value.into_boxed_str())).collect::<BTreeMap<_, _>>();
            match wren_scheduling::spawn_background_result("wren-lsp-start", move || {
                let result = connect_lsp(server_state, path, root_state, document_id, revision, text, environment).map_err(|error| error.to_string());
                Box::new(move |app: &mut App| app.finish_lsp_start(result)) as LspCompletion
            }) {
                Ok(receiver) => {
                    self.lsp_job = Some(LspJob { starting: true, language_id, navigation: None, receiver });
                }
                Err(error) => self.show_error(format!("start language server: {error}")),
            }
        }
    }

    pub(super) fn poll_lsp(&mut self) -> bool {
        let Some(job) = &self.lsp_job else {
            return false;
        };
        let completed = match poll_channel(&job.receiver) {
            Ok(Some(result)) => Ok(result),
            Ok(None) => return false,
            Err(ChannelDisconnected) => Err(job.starting),
        };
        self.lsp_job = None;
        match completed {
            Ok(complete) => complete(self),
            Err(starting) => {
                self.show_error(if starting { "language server startup worker disconnected" } else { "language server request worker disconnected" });
                if !starting {
                    self.begin_lsp_start();
                }
            }
        }
        true
    }

    pub(super) fn finish_lsp_start(&mut self, result: Result<PersistentLsp, String>) {
        match result {
            Ok(lsp) => {
                self.lsps.push(lsp);
                self.lsp_semantic_dirty = false;
                if !self.activate_available_lsp() {
                    match self.active_language_server() {
                        Some(server) => self.start_active_lsp(server),
                        // The completed root server remains owned by the
                        // workspace while a non-LSP buffer is active.
                        None => self.suspend_lsp_semantics(),
                    }
                }
                if self.lsp_ready_for_active() {
                    self.resume_pending_lsp_request();
                }
            }
            Err(error) => self.show_info(format!("language server unavailable: {error}")),
        }
    }

    fn activate_available_lsp(&mut self) -> bool {
        let Some(server) = language_server_invocation(self.active.document.presentation_path()) else {
            return false;
        };
        let root = self.lsp_root();
        let Some(index) = self.lsps.iter().position(|lsp| lsp.server == server && lsp.root == root) else { return false };
        if let Err(error) = self.open_active_document_on_lsp(index, &server) {
            // A protocol failure invalidates the selected client. This is the
            // sole owner of that recovery policy.
            self.lsps.swap_remove(index);
            self.show_info(format!("language server unavailable: {error}"));
            return false;
        }
        true
    }

    fn suspend_lsp_semantics(&mut self) {
        for lsp in &mut self.lsps {
            lsp.semantic_due = None;
        }
        self.lsp_semantic_dirty = false;
    }

    fn resume_pending_lsp_request(&mut self) {
        let Some(request) = self.pending_lsp_request.take() else {
            return;
        };
        let method = request.method;
        if let Err(error) = self.start_lsp_cursor_request(request) {
            self.show_error(format!("{method}: {error}"));
        }
    }

    fn queue_lsp_request(&mut self, request: PendingLspRequest) -> bool {
        if self.active_lsp_index().is_some() {
            return false;
        }
        if self.lsp_job.is_none() {
            self.begin_lsp_start();
        }
        let Some(starting) = self.lsp_job.as_ref().map(|job| job.starting) else {
            return false;
        };
        self.message = match (&request, starting) {
            (_, true) if request.hover => String::new(),
            _ => format!("language server {}; {} queued", if starting { "starting" } else { "busy" }, request.label()),
        };
        if request.hover && starting {
            self.close_editor_popup();
        }
        self.pending_lsp_request = Some(request);
        true
    }

    pub(super) fn lsp_ready_for_active(&self) -> bool {
        self.active_lsp_index().is_some()
    }

    pub(super) fn active_lsp_index(&self) -> Option<usize> {
        let server = language_server_invocation(self.active.document.presentation_path())?;
        let root = self.lsp_root();
        self.lsps.iter().position(|lsp| lsp.server == server && lsp.root == root && lsp.document_id == self.active.document_id)
    }

    pub(super) fn active_lsp_mut(&mut self) -> Option<&mut PersistentLsp> {
        let index = self.active_lsp_index()?;
        self.lsps.get_mut(index)
    }

    fn open_active_document_on_lsp(&mut self, index: usize, server: &LanguageServerInvocation) -> Result<()> {
        let document_id = self.active.document_id;
        let revision = self.active.editor.revision();
        let uri = file_uri(self.active.document.presentation_path().ok_or_else(|| anyhow!("LSP action needs a named buffer"))?);
        let lsp = self.lsps.get_mut(index).ok_or_else(|| anyhow!("selected language server disappeared"))?;
        if let Some(open) = lsp.open_documents.get(&document_id) {
            lsp.document_id = document_id;
            lsp.revision = open.revision;
            lsp.uri.clone_from(&open.uri);
        } else {
            lsp.client.did_open(&uri, &server.language_id, i64::try_from(revision.get()).unwrap_or(i64::MAX), &self.active.editor.contents())?;
            lsp.document_id = document_id;
            lsp.revision = revision;
            lsp.uri.clone_from(&uri);
            lsp.open_documents.insert(document_id, LspOpenDocument { uri, revision });
        }
        lsp.semantic_due = lsp.capabilities.semantic_legend.as_ref().map(|_| Instant::now() + LSP_SEMANTIC_IDLE_PERIOD);
        Ok(())
    }

    fn connect_active_lsp(&mut self) -> Result<PersistentLsp> {
        let server = self.active_language_server().ok_or_else(|| {
            let language = language_bundle(self.active.document.presentation_path()).language_id;
            anyhow!("no installed language server for {language}")
        })?;
        let path = self.active.document.presentation_path().ok_or_else(|| anyhow!("LSP action needs a named buffer"))?.to_path_buf();
        let root = self.lsp_root();
        let environment = env::vars().map(|(name, value)| (name.into_boxed_str(), value.into_boxed_str())).collect();
        connect_lsp(server, path, root, self.active.document_id, self.active.editor.revision(), self.active.editor.contents(), environment)
    }

    pub(super) fn active_lsp_client(&mut self) -> Result<(&mut LspClient, &str)> {
        if let Some(job) = &self.lsp_job {
            bail!("language server is {}", if job.starting { "starting" } else { "completing another request" });
        }
        if !self.lsp_ready_for_active() && !self.activate_available_lsp() {
            let lsp = self.connect_active_lsp()?;
            self.lsps.push(lsp);
        }
        let document_id = self.active.document_id;
        let revision = self.active.editor.revision();
        let text = self.active.editor.contents();
        let index = self.active_lsp_index().ok_or_else(|| anyhow!("selected language server disappeared"))?;
        let lsp = &mut self.lsps[index];
        update_lsp_document(lsp, document_id, revision, &text).map_err(anyhow::Error::msg)?;
        Ok((&mut lsp.client, &lsp.uri))
    }

    pub(super) fn lsp_position(&self) -> LspPosition {
        let text = self.active.editor.contents();
        let cursor = self.active.editor.primary_cursor().min(text.len());
        let line = self.active.editor.text().line_of_byte(cursor);
        let start = self.active.editor.text().byte_of_line(line);
        let character = wren_position::byte_column_to_utf16(&text[start..cursor], cursor - start).unwrap_or_default();
        LspPosition { line: u32::try_from(line).unwrap_or(u32::MAX), character: u32::try_from(character).unwrap_or(u32::MAX) }
    }

    pub(super) fn lsp_request_at_cursor(&mut self, method: &str, extra: serde_json::Value) -> Result<serde_json::Value> {
        let position = self.lsp_position();
        let (client, uri) = self.active_lsp_client()?;
        request_lsp_at(client, uri, method, position, extra)
    }

    pub(super) fn request_lsp_completion(&mut self) -> Result<Option<CompletionSession>> {
        if self.active_language_server().is_none() {
            return Ok(None);
        }
        let result = self.lsp_request_at_cursor("textDocument/completion", serde_json::json!({"context": {"triggerKind": 1}}))?;
        let items = result.as_array().or_else(|| result.get("items").and_then(serde_json::Value::as_array));
        let Some(items) = items else {
            return Ok(None);
        };
        let candidates = items
            .iter()
            .filter_map(|item| {
                let label = item.get("label")?.as_str()?;
                let raw_insert = item.pointer("/textEdit/newText").or_else(|| item.get("insertText")).and_then(serde_json::Value::as_str).unwrap_or(label);
                let snippet = item.get("insertTextFormat").and_then(serde_json::Value::as_u64) == Some(2);
                Some(CompletionCandidate {
                    label: label.into(),
                    insert: if snippet { expand_lsp_snippet(raw_insert).into_boxed_str() } else { raw_insert.into() },
                    source: "lsp".into(),
                    detail: item.get("detail").and_then(serde_json::Value::as_str).unwrap_or("LSP").into(),
                    documentation: item.get("documentation").map(render_lsp_text).unwrap_or_default().into_boxed_str(),
                    replace: None,
                    snippet: snippet.then(|| raw_insert.into()),
                })
            })
            .take(256)
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(None);
        }
        let text = self.active.editor.contents();
        let cursor = self.active.editor.primary_cursor().min(text.len());
        let start = identifier_prefix_start(&text, cursor);
        Ok(Some(CompletionSession { revision: self.active.editor.revision(), replace: start..cursor, candidates }))
    }

    pub(super) fn dispatch_lsp_cursor_request(&mut self, request: PendingLspRequest) -> Result<()> {
        if self.queue_lsp_request(request) { Ok(()) } else { self.start_lsp_cursor_request(request) }
    }

    fn start_lsp_cursor_request(&mut self, request: PendingLspRequest) -> Result<()> {
        let document_id = self.active.document_id;
        let revision = self.active.editor.revision();
        let position = self.lsp_position();
        let name = request.label();
        let message = if request.hover { "loading hover…" } else { "finding definition…" };
        self.start_lsp_background_task(name, message, move |lsp, _, _, _, prepared| {
            let response = prepared.and_then(|()| {
                lsp.client
                    .request(
                        request.method,
                        serde_json::json!({
                            "textDocument": {"uri": lsp.uri},
                            "position": position,
                        }),
                    )
                    .map_err(|error| error.to_string())
            });
            request.completion(document_id, revision, response)
        })
    }

    fn start_lsp_background_task(
        &mut self,
        name: &str,
        message: &str,
        task: impl FnOnce(&mut PersistentLsp, DocumentId, DocumentRevision, &str, Result<(), String>) -> LspCompletion + Send + 'static,
    ) -> Result<()> {
        let Some(index) = self.active_lsp_index() else {
            let language = language_bundle(self.active.document.presentation_path()).language_id;
            bail!("no ready language server for {language}");
        };
        let mut lsp = self.lsps.swap_remove(index);
        let language_id = lsp.server.language_id.clone().into_boxed_str();
        let navigation = Some(lsp.capabilities.navigation);
        let document_id = self.active.document_id;
        let revision = self.active.editor.revision();
        let text = self.active.editor.contents().into_boxed_str();
        let receiver = wren_scheduling::spawn_background_result(format!("wren-lsp-{name}"), move || {
            let prepared = update_lsp_document(&mut lsp, document_id, revision, &text);
            let complete = task(&mut lsp, document_id, revision, &text, prepared);
            Box::new(move |app: &mut App| app.finish_lsp_background(lsp, complete)) as LspCompletion
        })
        .with_context(|| format!("spawn asynchronous {name} request"))?;
        self.lsp_job = Some(LspJob { starting: false, language_id, navigation, receiver });
        if !message.is_empty() {
            self.message = message.to_owned();
        }
        Ok(())
    }

    pub(super) fn finish_lsp_location(&mut self, method: &str, result: &serde_json::Value) -> Result<()> {
        let locations = parse_lsp_locations(result)?;
        if locations.is_empty() {
            return self.set_message(format!("{method}: no location"));
        }
        self.quickfix = locations;
        if self.quickfix.len() == 1 {
            let entry = self.quickfix[0].clone();
            self.navigate_to_entry(&entry)?;
            self.message = entry.display();
        } else {
            self.start_location_picker(PickerSource::Jumps, "")?;
        }
        Ok(())
    }

    pub(super) fn finish_lsp_background(&mut self, mut lsp: PersistentLsp, complete: LspCompletion) {
        if self.lsp_semantic_dirty && lsp.capabilities.semantic_legend.is_some() {
            lsp.semantic_due = Some(Instant::now() + LSP_SEMANTIC_IDLE_PERIOD);
        }
        self.lsp_semantic_dirty = false;
        self.lsps.push(lsp);
        complete(self);
        self.begin_lsp_start();
        if self.lsp_ready_for_active() {
            self.resume_pending_lsp_request();
        }
    }

    pub(super) fn poll_lsp_semantic_due(&mut self) -> Result<bool> {
        if self.lsp_job.is_some() || self.pending_lsp_request.is_some() {
            return Ok(false);
        }
        let Some(index) = self.active_lsp_index() else { return Ok(false) };
        let due = self.lsps[index].semantic_due;
        if due.is_none_or(|due| Instant::now() < due) {
            return Ok(false);
        }
        let Some(legend) = self.lsps[index].capabilities.semantic_legend.clone() else {
            self.lsps[index].semantic_due = None;
            return Ok(false);
        };
        self.lsps[index].semantic_due = None;
        let buffer_id = self.active.buffer_id;
        self.start_lsp_background_task("semantic", "", move |lsp, _, revision, text, prepared| {
            let decorations = prepared.and_then(|()| {
                let response = lsp
                    .client
                    .request("textDocument/semanticTokens/full", serde_json::json!({"textDocument": {"uri": lsp.uri}}))
                    .map_err(|error| error.to_string())?;
                let spans = parse_semantic_tokens(text, &response, &legend).into_iter().map(provider_decoration).collect();
                Ok(BufferDecorations::new(revision, spans))
            });
            semantic_lsp_completion(buffer_id, revision, decorations)
        })?;
        Ok(false)
    }

    pub(super) fn lsp_references(&mut self) -> Result<()> {
        let result = self.lsp_request_at_cursor("textDocument/references", serde_json::json!({"context": {"includeDeclaration": true}}))?;
        self.quickfix = parse_lsp_locations(&result)?;
        self.start_location_picker(PickerSource::Jumps, "")
    }

    pub(super) fn finish_lsp_hover(&mut self, method: &str, result: &serde_json::Value) {
        let rendered = render_lsp_text(result);
        if rendered.is_empty() {
            self.close_editor_popup();
            self.message = format!("{method}: no information");
        } else {
            let (text, decorations) = lsp_popup_markdown(&rendered);
            self.popup = Some(TextPopup::new("", text).with_decorations(decorations));
            self.popup_deadline = Some(Instant::now() + Duration::from_secs(6));
            self.message.clear();
        }
    }

    pub(super) fn rename_symbol(&mut self, new_name: &str) -> Result<()> {
        if new_name.trim().is_empty() {
            return self.set_message("rename cancelled".to_owned());
        }
        let edit = self.lsp_request_at_cursor("textDocument/rename", serde_json::json!({"newName": new_name}))?;
        self.apply_lsp_workspace_edit(&edit)?;
        self.set_message(format!("renamed symbol to {new_name}"))
    }

    pub(super) fn apply_lsp_workspace_edit(&mut self, workspace_edit: &serde_json::Value) -> Result<()> {
        self.apply_workspace_edit(serde_json::from_value(workspace_edit.clone())?)
    }

    fn apply_workspace_edit(&mut self, workspace_edit: WorkspaceEdit) -> Result<()> {
        for (uri, edits) in workspace_text_edits(workspace_edit) {
            let path = path_from_file_uri(&uri)?;
            self.open_buffer(&path)?;
            self.apply_lsp_text_edits(edits)?;
        }
        Ok(())
    }

    pub(super) fn apply_lsp_text_edits(&mut self, edits: Vec<LspTextEdit>) -> Result<bool> {
        let revision = self.active.editor.revision();
        let text = self.active.editor.contents();
        let edits = lower_lsp_text_edits(self.active.document_id, revision, revision, &text, edits)?.edits;
        let Some(transaction) = (!edits.is_empty()).then(|| Transaction::new(revision, edits)).transpose()? else { return Ok(false) };
        self.active.editor.apply_transaction(transaction.clone())?;
        self.after_transaction(Some(transaction));
        Ok(true)
    }

    pub(super) fn lsp_code_action(&mut self) -> Result<()> {
        let position = self.lsp_position();
        let result = {
            let (client, uri) = self.active_lsp_client()?;
            client.request(
                "textDocument/codeAction",
                serde_json::json!({
                    "textDocument": {"uri": uri},
                    "range": {"start": position, "end": position},
                    "context": {"diagnostics": []}
                }),
            )?
        };
        let actions: CodeActionResponse = serde_json::from_value(result)?;
        let action = actions
            .iter()
            .find(|action| matches!(action, CodeActionOrCommand::CodeAction(action) if action.is_preferred == Some(true)))
            .or_else(|| actions.first());
        let Some(action) = action else {
            return self.set_message("no code actions".to_owned());
        };
        let (title, edit, command) = match action {
            CodeActionOrCommand::CodeAction(action) => (action.title.clone(), action.edit.clone(), action.command.clone()),
            CodeActionOrCommand::Command(command) => (command.title.clone(), None, Some(command.clone())),
        };
        if let Some(edit) = edit {
            self.apply_workspace_edit(edit)?;
        }
        let (client, _) = self.active_lsp_client()?;
        execute_lsp_command(client, command.as_ref())?;
        self.set_message(title)
    }

    pub(super) fn lsp_code_lens(&mut self) -> Result<()> {
        let (client, uri) = self.active_lsp_client()?;
        let result = client.request("textDocument/codeLens", serde_json::json!({"textDocument": {"uri": uri}}))?;
        let Some(mut lens) = serde_json::from_value::<Option<Vec<CodeLens>>>(result)?.and_then(|lenses| lenses.into_iter().next()) else {
            return self.set_message("no code lens at buffer".to_owned());
        };
        if lens.command.is_none() {
            lens = serde_json::from_value(client.request("codeLens/resolve", serde_json::to_value(lens)?)?)?;
        }
        let title = lens.command.as_ref().map_or_else(|| "code lens".to_owned(), |command| command.title.clone());
        execute_lsp_command(client, lens.command.as_ref())?;
        self.set_message(title)
    }

    pub(super) fn lsp_workspace_folder(&mut self, _method: &str, add: bool) -> Result<()> {
        let folder = self.workspace_root();
        if add {
            if !self.workspace_folders.iter().any(|path| same_path(path, &folder)) {
                self.workspace_folders.push(folder.clone());
            }
            self.message = format!("workspace folder added: {}", folder.display());
        } else {
            self.workspace_folders.retain(|path| !same_path(path, &folder));
            self.message = format!("workspace folder removed: {}", folder.display());
        }
        Ok(())
    }

    pub(super) fn list_workspace_folders(&mut self) -> Result<()> {
        self.message = if self.workspace_folders.is_empty() {
            "no workspace folders".to_owned()
        } else {
            self.workspace_folders.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(" · ")
        };
        Ok(())
    }

    pub(super) fn execute_cdo(&mut self, command: ExCommand) -> Result<()> {
        let entries = self.quickfix.clone();
        if entries.is_empty() {
            return self.set_message("quickfix list is empty".to_owned());
        }
        for entry in entries {
            self.open_buffer(&entry.path)?;
            self.active.editor.set_cursor(self.active.editor.text().byte_of_line(entry.line.saturating_sub(1)));
            self.execute_ex_command(command.clone())?;
        }
        self.set_message("cdo complete".to_owned())
    }
}

fn connect_lsp(
    server: LanguageServerInvocation,
    path: PathBuf,
    root: PathBuf,
    document_id: DocumentId,
    revision: DocumentRevision,
    text: String,
    environment: BTreeMap<Box<str>, Box<str>>,
) -> Result<PersistentLsp> {
    let (client, uri, capabilities) = spawn_lsp_client(&server, &path, &root, revision, &text, environment)?;
    let open_documents = BTreeMap::from([(document_id, LspOpenDocument { uri: uri.clone(), revision })]);
    Ok(PersistentLsp {
        document_id,
        revision,
        uri,
        client,
        server,
        root,
        open_documents,
        semantic_due: capabilities.semantic_legend.as_ref().map(|_| Instant::now() + LSP_SEMANTIC_IDLE_PERIOD),
        capabilities,
    })
}

pub(super) fn semantic_lsp_completion(buffer_id: BufferId, revision: DocumentRevision, decorations: Result<BufferDecorations, String>) -> LspCompletion {
    Box::new(move |app| match decorations {
        Ok(spans) if app.buffer(buffer_id).is_some_and(|buffer| buffer.editor.revision() == revision) => {
            app.semantic_decorations.insert(buffer_id, spans);
        }
        Ok(_) => {}
        Err(error) => app.show_info(format!("textDocument/semanticTokens/full unavailable: {error}")),
    })
}

fn request_lsp_at(client: &mut LspClient, uri: &str, method: &str, position: LspPosition, extra: serde_json::Value) -> Result<serde_json::Value> {
    let mut parameters = serde_json::json!({"textDocument": {"uri": uri}, "position": position});
    if let (Some(target), Some(extra)) = (parameters.as_object_mut(), extra.as_object()) {
        target.extend(extra.clone());
    }
    client.request(method, parameters).map_err(Into::into)
}

fn workspace_text_edits(edit: WorkspaceEdit) -> BTreeMap<String, Vec<LspTextEdit>> {
    let mut grouped = edit.changes.unwrap_or_default().into_iter().map(|(uri, edits)| (uri.as_str().to_owned(), edits)).collect::<BTreeMap<_, _>>();
    let changes = match edit.document_changes {
        Some(DocumentChanges::Edits(edits)) => edits,
        Some(DocumentChanges::Operations(operations)) => operations
            .into_iter()
            .filter_map(|operation| match operation {
                DocumentChangeOperation::Edit(edit) => Some(edit),
                DocumentChangeOperation::Op(_) => None,
            })
            .collect(),
        None => Vec::new(),
    };
    for change in changes {
        grouped.entry(change.text_document.uri.as_str().to_owned()).or_default().extend(change.edits.into_iter().map(|edit| match edit {
            OneOf::Left(edit) => edit,
            OneOf::Right(edit) => edit.text_edit,
        }));
    }
    grouped
}

fn update_lsp_document(lsp: &mut PersistentLsp, document_id: DocumentId, revision: DocumentRevision, text: &str) -> Result<(), String> {
    if lsp.revision == revision {
        return Ok(());
    }
    lsp.client.did_change_full(&lsp.uri, i64::try_from(revision.get()).unwrap_or(i64::MAX), text).map_err(|error| error.to_string())?;
    lsp.revision = revision;
    if let Some(open) = lsp.open_documents.get_mut(&document_id) {
        open.revision = revision;
    }
    Ok(())
}

fn execute_lsp_command(client: &mut LspClient, command: Option<&LspCommand>) -> Result<()> {
    if let Some(command) = command {
        let _ = client.request(
            "workspace/executeCommand",
            serde_json::json!({"command": command.command, "arguments": command.arguments.as_deref().unwrap_or_default()}),
        )?;
    }
    Ok(())
}
