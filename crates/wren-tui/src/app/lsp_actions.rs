use super::*;

impl App {
    pub(super) fn active_language_server(&self) -> Option<LanguageServerInvocation> {
        language_server_invocation(self.active.document.presentation_path()).filter(|server| executable_exists(&server.program))
    }

    pub(super) fn active_lsp_navigation_capabilities(&self) -> Option<LspNavigationCapabilities> {
        self.lsp.as_ref().filter(|lsp| lsp.document_id == self.active.document_id).map(|lsp| lsp.capabilities.navigation).or_else(|| {
            let language = language_bundle(self.active.document.presentation_path()).language_id;
            self.lsp_navigation_capabilities.get(language.as_ref()).copied()
        })
    }

    pub(super) fn ensure_workspace_lsp_started(&mut self) {
        self.begin_lsp_start();
    }

    pub(super) fn schedule_workspace_lsp_start(&mut self) {
        if self.lsp_background.is_some() || self.lsp_start.is_some() {
            self.lsp_start_due = None;
            return;
        }
        if self.activate_available_lsp() {
            self.lsp_start_due = None;
        } else if self.active_language_server().is_some() {
            self.lsp_start_due = Some(Instant::now() + LSP_START_IDLE_PERIOD);
        } else {
            self.lsp_start_due = None;
            self.suspend_lsp_semantics();
        }
    }

    pub(super) fn poll_lsp_start_due(&mut self) -> Result<bool> {
        if self.lsp_start_due.is_some_and(|due| Instant::now() >= due) {
            self.begin_lsp_start();
        }
        Ok(false)
    }

    pub(super) fn begin_lsp_start(&mut self) {
        self.lsp_start_due = None;
        // A startup owns a live child as soon as its worker is spawned. Buffer
        // navigation must wait for it and attach the new document afterward,
        // never drop the receiver and implicitly kill/restart that child.
        if self.lsp_background.is_some() || self.lsp_start.is_some() {
            return;
        }
        if self.activate_available_lsp() {
            return;
        }
        // Visiting a help, generated, or otherwise unsupported buffer must
        // not tear down the root workspace's already-running server.
        let Some(server) = self.active_language_server() else {
            self.suspend_lsp_semantics();
            return;
        };
        self.start_active_lsp(server);
    }

    fn start_active_lsp(&mut self, server: LanguageServerInvocation) {
        if let Some(lsp) = self.lsp.take() {
            self.parked_lsps.push(lsp);
        }
        if matches!(self.pending_lsp_request.as_ref(), Some(PendingLspRequest::Hover(_))) {
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
            let server_state = server.clone();
            let root_state = root.clone();
            let environment = env::vars().map(|(name, value)| (name.into_boxed_str(), value.into_boxed_str())).collect::<BTreeMap<_, _>>();
            let (sender, receiver) = mpsc::channel();
            match thread::Builder::new().name("wren-lsp-start".to_owned()).spawn(move || {
                wren_scheduling::mark_background();
                let result = spawn_lsp_client(&server, &path, &root, revision, &text, environment)
                    .map(|(client, uri, capabilities)| {
                        let open_documents = BTreeMap::from([(document_id, LspOpenDocument { uri: uri.clone(), revision })]);
                        PersistentLsp {
                            document_id,
                            revision,
                            uri,
                            client,
                            server: server_state,
                            root: root_state,
                            open_documents,
                            semantic_due: capabilities.semantic_legend.as_ref().map(|_| Instant::now() + LSP_SEMANTIC_IDLE_PERIOD),
                            capabilities,
                        }
                    })
                    .map_err(|error| error.to_string());
                let _ = sender.send(result);
            }) {
                Ok(_) => self.lsp_start = Some(receiver),
                Err(error) => self.show_error(format!("start language server: {error}")),
            }
        }
    }

    pub(super) fn poll_lsp_start(&mut self) -> Result<bool> {
        let Some(receiver) = &self.lsp_start else {
            return Ok(false);
        };
        let result = match poll_channel(receiver) {
            Ok(Some(result)) => result,
            Ok(None) => return Ok(false),
            Err(ChannelDisconnected) => {
                self.lsp_start = None;
                self.show_error("language server startup worker disconnected");
                return Ok(true);
            }
        };
        self.lsp_start = None;
        let lsp = match result {
            Ok(lsp) => lsp,
            Err(error) => {
                // Server availability is environmental: an incomplete build
                // tree may legitimately make clangd or another analyzer
                // decline startup. Keep the detail in :messages without
                // blocking ordinary editing with an error float.
                self.show_info(format!("language server unavailable: {error}"));
                return Ok(true);
            }
        };
        self.lsp_navigation_capabilities.insert(lsp.server.language_id.clone().into_boxed_str(), lsp.capabilities.navigation);
        self.lsp = Some(lsp);
        self.lsp_semantic_dirty = false;
        if self.activate_started_lsp() {
            self.resume_pending_lsp_request();
        }
        Ok(true)
    }

    fn activate_available_lsp(&mut self) -> bool {
        let Some(server) = language_server_invocation(self.active.document.presentation_path()) else {
            return false;
        };
        let root = self.lsp_root();
        if !self.select_reusable_lsp(&server, &root) {
            return false;
        }
        if let Err(error) = self.open_active_document_on_lsp(&server) {
            // A protocol failure invalidates the selected client. This is the
            // sole owner of that recovery policy.
            self.lsp = None;
            self.show_info(format!("language server unavailable: {error}"));
            return false;
        }
        true
    }

    fn activate_started_lsp(&mut self) -> bool {
        if self.activate_available_lsp() {
            return true;
        }
        let Some(server) = self.active_language_server() else {
            // The completed root server remains owned by the workspace while
            // a non-LSP buffer is active.
            self.suspend_lsp_semantics();
            return false;
        };
        self.start_active_lsp(server);
        self.lsp_ready_for_active()
    }

    fn suspend_lsp_semantics(&mut self) {
        if let Some(lsp) = &mut self.lsp {
            lsp.semantic_due = None;
        }
        self.lsp_semantic_dirty = false;
    }

    fn resume_pending_lsp_request(&mut self) {
        let Some(request) = self.pending_lsp_request.take() else {
            return;
        };
        let method = request.method().to_owned();
        let result = match request {
            PendingLspRequest::Hover(_) => self.lsp_hover_ready(&method),
            PendingLspRequest::Location(_) => self.start_lsp_location_request(&method),
        };
        self.report_lsp_request_result(&method, result);
    }

    fn queue_lsp_request(&mut self, request: PendingLspRequest) -> bool {
        let busy = self.lsp_background.is_some();
        if !busy && self.lsp.as_ref().is_some_and(|lsp| lsp.document_id == self.active.document_id) {
            return false;
        }
        if !busy && self.lsp_start.is_none() {
            self.begin_lsp_start();
        }
        if !busy && self.lsp_start.is_none() {
            return false;
        }
        let starting = !busy;
        self.message = match (&request, starting) {
            (PendingLspRequest::Hover(_), true) => String::new(),
            _ => format!("language server {}; {} queued", if starting { "starting" } else { "busy" }, request.label()),
        };
        if matches!(&request, PendingLspRequest::Hover(_)) && starting {
            self.popup = None;
            self.popup_deadline = None;
        }
        self.pending_lsp_request = Some(request);
        true
    }

    fn report_lsp_request_result(&mut self, method: &str, result: Result<()>) {
        if let Err(error) = result {
            self.show_error(format!("{method}: {error}"));
        }
    }

    pub(super) fn lsp_ready_for_active(&self) -> bool {
        self.lsp.as_ref().is_some_and(|lsp| lsp.document_id == self.active.document_id)
    }

    fn select_reusable_lsp(&mut self, server: &LanguageServerInvocation, root: &Path) -> bool {
        let current_matches = self.lsp.as_ref().is_some_and(|lsp| &lsp.server == server && lsp.root == root);
        if current_matches {
            return true;
        }
        let Some(index) = self.parked_lsps.iter().position(|lsp| &lsp.server == server && lsp.root == root) else {
            return false;
        };
        let replacement = self.parked_lsps.swap_remove(index);
        if let Some(current) = self.lsp.replace(replacement) {
            self.parked_lsps.push(current);
        }
        true
    }

    fn open_active_document_on_lsp(&mut self, server: &LanguageServerInvocation) -> Result<()> {
        let document_id = self.active.document_id;
        let revision = self.active.editor.revision();
        let uri = file_uri(self.active.document.presentation_path().ok_or_else(|| anyhow!("LSP action needs a named buffer"))?);
        let lsp = self.lsp.as_mut().ok_or_else(|| anyhow!("selected language server disappeared"))?;
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

    pub(super) fn start_lsp(&mut self) -> Result<(LspClient, String)> {
        let server = self.active_language_server().ok_or_else(|| {
            let language = language_bundle(self.active.document.presentation_path()).language_id;
            anyhow!("no installed language server for {language}")
        })?;
        let path = self.active.document.presentation_path().ok_or_else(|| anyhow!("LSP action needs a named buffer"))?;
        let root = self.lsp_root();
        let environment = env::vars().map(|(name, value)| (name.into_boxed_str(), value.into_boxed_str())).collect();
        let language_id = server.language_id.clone().into_boxed_str();
        let (client, uri, capabilities) = spawn_lsp_client(&server, path, &root, self.active.editor.revision(), &self.active.editor.contents(), environment)?;
        self.lsp_navigation_capabilities.insert(language_id, capabilities.navigation);
        Ok((client, uri))
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
        if self.lsp_background.is_some() {
            bail!("language server is completing another request");
        }
        let document_id = self.active.document_id;
        let revision = self.active.editor.revision();
        let text = self.active.editor.contents();
        let position = self.lsp_position();
        let persistent = self.lsp.as_ref().is_some_and(|lsp| lsp.document_id == document_id);
        if persistent {
            let lsp = self.lsp.as_mut().expect("persistent LSP was checked");
            if lsp.revision != revision {
                lsp.client.did_change_full(&lsp.uri, i64::try_from(revision.get()).unwrap_or(i64::MAX), &text)?;
                lsp.revision = revision;
                if let Some(open) = lsp.open_documents.get_mut(&document_id) {
                    open.revision = revision;
                }
            }
            let mut parameters = serde_json::json!({
                "textDocument": {"uri": lsp.uri},
                "position": position,
            });
            if let (Some(target), Some(extra)) = (parameters.as_object_mut(), extra.as_object()) {
                target.extend(extra.clone());
            }
            return lsp.client.request(method, parameters).map_err(Into::into);
        }
        let (mut client, uri) = self.start_lsp()?;
        let mut parameters = serde_json::json!({
            "textDocument": {"uri": uri},
            "position": position,
        });
        if let (Some(target), Some(extra)) = (parameters.as_object_mut(), extra.as_object()) {
            target.extend(extra.clone());
        }
        client.request(method, parameters).map_err(Into::into)
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
        let mut start = cursor;
        while start > 0 {
            let previous = text[..start].char_indices().next_back().map_or(0, |(byte, _)| byte);
            let character = text[previous..start].chars().next().unwrap_or(' ');
            if !character.is_alphanumeric() && character != '_' {
                break;
            }
            start = previous;
        }
        Ok(Some(CompletionSession { revision: self.active.editor.revision(), replace: start..cursor, candidates }))
    }

    pub(super) fn lsp_location(&mut self, method: &str) -> Result<()> {
        if self.queue_lsp_request(PendingLspRequest::Location(method.to_owned())) { Ok(()) } else { self.start_lsp_location_request(method) }
    }

    pub(super) fn start_lsp_location_request(&mut self, method: &str) -> Result<()> {
        let position = self.lsp_position();
        let method = method.to_owned();
        self.start_lsp_background_task("definition", "finding definition…", move |lsp, _, _, _, prepared| LspBackgroundOutcome::Location {
            method: method.clone(),
            response: prepared.and_then(|()| {
                lsp.client
                    .request(
                        &method,
                        serde_json::json!({
                            "textDocument": {"uri": lsp.uri},
                            "position": position,
                        }),
                    )
                    .map_err(|error| error.to_string())
            }),
        })
    }

    fn start_lsp_background_task(
        &mut self,
        name: &str,
        message: &str,
        task: impl FnOnce(&mut PersistentLsp, DocumentId, DocumentRevision, &str, Result<(), String>) -> LspBackgroundOutcome + Send + 'static,
    ) -> Result<()> {
        let Some(mut lsp) = self.lsp.take() else {
            let language = language_bundle(self.active.document.presentation_path()).language_id;
            bail!("no ready language server for {language}");
        };
        let document_id = self.active.document_id;
        let revision = self.active.editor.revision();
        let text = self.active.editor.contents().into_boxed_str();
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name(format!("wren-lsp-{name}"))
            .spawn(move || {
                wren_scheduling::mark_background();
                let prepared = update_lsp_document(&mut lsp, document_id, revision, &text);
                let outcome = task(&mut lsp, document_id, revision, &text, prepared);
                let _ = sender.send(LspBackgroundResult { lsp, outcome });
            })
            .with_context(|| format!("spawn asynchronous {name} request"))?;
        self.lsp_background = Some(receiver);
        if !message.is_empty() {
            self.message = message.to_owned();
        }
        Ok(())
    }

    pub(super) fn finish_lsp_location(&mut self, method: &str, result: &serde_json::Value) -> Result<()> {
        let locations = parse_lsp_locations(result)?;
        if locations.is_empty() {
            self.message = format!("{method}: no location");
            return Ok(());
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

    pub(super) fn poll_lsp_background(&mut self) -> Result<bool> {
        let Some(receiver) = &self.lsp_background else {
            return Ok(false);
        };
        let result = match poll_channel(receiver) {
            Ok(Some(result)) => result,
            Ok(None) => return Ok(false),
            Err(ChannelDisconnected) => {
                self.lsp_background = None;
                self.show_error("language server request worker disconnected");
                self.begin_lsp_start();
                return Ok(true);
            }
        };
        self.lsp_background = None;
        self.lsp = Some(result.lsp);
        if self.lsp_semantic_dirty {
            if let Some(lsp) = &mut self.lsp
                && lsp.capabilities.semantic_legend.is_some()
            {
                lsp.semantic_due = Some(Instant::now() + LSP_SEMANTIC_IDLE_PERIOD);
            }
            self.lsp_semantic_dirty = false;
        }
        self.apply_lsp_background_outcome(result.outcome);
        self.begin_lsp_start();
        if self.lsp_ready_for_active() {
            self.resume_pending_lsp_request();
        }
        Ok(true)
    }

    pub(super) fn apply_lsp_background_outcome(&mut self, outcome: LspBackgroundOutcome) {
        match outcome {
            LspBackgroundOutcome::Location { method, response } => match response {
                Ok(value) => {
                    if let Err(error) = self.finish_lsp_location(&method, &value) {
                        self.show_error(format!("{method}: {error}"));
                    }
                }
                Err(error) => self.show_error(format!("{method}: {error}")),
            },
            LspBackgroundOutcome::Hover { method, document_id, revision, response } => match response {
                Ok(value) if self.active.document_id == document_id && self.active.editor.revision() == revision => {
                    self.finish_lsp_hover(&method, &value);
                }
                Ok(_) => {}
                Err(error) => self.show_error(format!("{method}: {error}")),
            },
            LspBackgroundOutcome::Semantic { buffer_id, revision, decorations } => match decorations {
                Ok(spans) if self.buffer(buffer_id).is_some_and(|buffer| buffer.editor.revision() == revision) => {
                    self.semantic_decorations.insert(buffer_id, spans);
                }
                Ok(_) => {}
                Err(error) => self.show_info(format!("textDocument/semanticTokens/full unavailable: {error}")),
            },
        }
    }

    pub(super) fn poll_lsp_semantic_due(&mut self) -> Result<bool> {
        if self.lsp_background.is_some()
            || self.lsp_start.is_some()
            || self.pending_lsp_request.is_some()
            || self.lsp.as_ref().is_none_or(|lsp| lsp.document_id != self.active.document_id)
        {
            return Ok(false);
        }
        let due = self.lsp.as_ref().and_then(|lsp| lsp.semantic_due);
        if due.is_none_or(|due| Instant::now() < due) {
            return Ok(false);
        }
        let Some(legend) = self.lsp.as_ref().and_then(|lsp| lsp.capabilities.semantic_legend.clone()) else {
            if let Some(lsp) = &mut self.lsp {
                lsp.semantic_due = None;
            }
            return Ok(false);
        };
        if let Some(lsp) = &mut self.lsp {
            lsp.semantic_due = None;
        }
        let buffer_id = self.active.buffer_id;
        let theme = self.theme;
        self.start_lsp_background_task("semantic", "", move |lsp, _, revision, text, prepared| {
            let decorations = prepared.and_then(|()| {
                let response = lsp
                    .client
                    .request("textDocument/semanticTokens/full", serde_json::json!({"textDocument": {"uri": lsp.uri}}))
                    .map_err(|error| error.to_string())?;
                let spans = parse_semantic_tokens(text, &response, &legend).into_iter().map(|span| provider_decoration(span, theme)).collect();
                Ok(BufferDecorations::new(revision, spans))
            });
            LspBackgroundOutcome::Semantic { buffer_id, revision, decorations }
        })?;
        Ok(false)
    }

    pub(super) fn lsp_references(&mut self) -> Result<()> {
        let result = self.lsp_request_at_cursor("textDocument/references", serde_json::json!({"context": {"includeDeclaration": true}}))?;
        self.quickfix = parse_lsp_locations(&result)?;
        self.start_location_picker(PickerSource::Jumps, "")
    }

    pub(super) fn lsp_hover(&mut self, method: &str) -> Result<()> {
        if self.queue_lsp_request(PendingLspRequest::Hover(method.to_owned())) { Ok(()) } else { self.lsp_hover_ready(method) }
    }

    pub(super) fn lsp_hover_ready(&mut self, method: &str) -> Result<()> {
        let document_id = self.active.document_id;
        let revision = self.active.editor.revision();
        let position = self.lsp_position();
        let method = method.to_owned();
        self.start_lsp_background_task("hover", "loading hover…", move |lsp, _, _, _, prepared| LspBackgroundOutcome::Hover {
            method: method.clone(),
            document_id,
            revision,
            response: prepared.and_then(|()| {
                lsp.client
                    .request(
                        &method,
                        serde_json::json!({
                            "textDocument": {"uri": lsp.uri},
                            "position": position,
                        }),
                    )
                    .map_err(|error| error.to_string())
            }),
        })
    }

    pub(super) fn finish_lsp_hover(&mut self, method: &str, result: &serde_json::Value) {
        let rendered = render_lsp_text(result);
        if rendered.is_empty() {
            self.popup = None;
            self.popup_deadline = None;
            self.message = format!("{method}: no information");
        } else {
            let (text, decorations) = lsp_popup_markdown(&rendered, self.theme);
            self.popup = Some(TextPopup::new("", text).with_decorations(decorations));
            self.popup_deadline = Some(Instant::now() + Duration::from_secs(6));
            self.message.clear();
        }
    }

    pub(super) fn rename_symbol(&mut self, new_name: &str) -> Result<()> {
        if new_name.trim().is_empty() {
            self.message = "rename cancelled".to_owned();
            return Ok(());
        }
        let edit = self.lsp_request_at_cursor("textDocument/rename", serde_json::json!({"newName": new_name}))?;
        self.apply_lsp_workspace_edit(&edit)?;
        self.message = format!("renamed symbol to {new_name}");
        Ok(())
    }

    pub(super) fn apply_lsp_workspace_edit(&mut self, workspace_edit: &serde_json::Value) -> Result<()> {
        for (uri, edits) in workspace_text_edits(serde_json::from_value(workspace_edit.clone())?) {
            let path = path_from_file_uri(&uri)?;
            self.open_buffer(&path)?;
            let revision = self.active.editor.revision();
            let text = self.active.editor.contents();
            let lowered = lower_lsp_text_edits(self.active.document_id, revision, revision, &text, edits)?;
            if lowered.edits.is_empty() {
                continue;
            }
            let transaction = Transaction::new(revision, lowered.edits)?;
            self.active.editor.apply_transaction(transaction.clone())?;
            self.after_transaction(Some(transaction));
        }
        Ok(())
    }

    pub(super) fn lsp_code_action(&mut self) -> Result<()> {
        let position = self.lsp_position();
        let (mut client, uri) = self.start_lsp()?;
        let result = client.request(
            "textDocument/codeAction",
            serde_json::json!({
                "textDocument": {"uri": uri},
                "range": {"start": position, "end": position},
                "context": {"diagnostics": []}
            }),
        )?;
        let Some(actions) = result.as_array() else {
            self.message = "no code actions".to_owned();
            return Ok(());
        };
        let action = actions.iter().find(|action| action.get("isPreferred").and_then(serde_json::Value::as_bool) == Some(true)).or_else(|| actions.first());
        let Some(action) = action else {
            self.message = "no code actions".to_owned();
            return Ok(());
        };
        if let Some(edit) = action.get("edit") {
            self.apply_lsp_workspace_edit(edit)?;
        }
        if let Some((identifier, arguments)) = code_action_command(action) {
            let _ = client.request("workspace/executeCommand", serde_json::json!({"command": identifier, "arguments": arguments}))?;
        }
        self.message = action.get("title").and_then(serde_json::Value::as_str).map_or_else(|| "code action applied".to_owned(), |title| title.to_owned());
        Ok(())
    }

    pub(super) fn lsp_code_lens(&mut self) -> Result<()> {
        let (mut client, uri) = self.start_lsp()?;
        let result = client.request("textDocument/codeLens", serde_json::json!({"textDocument": {"uri": uri}}))?;
        let Some(lens) = result.as_array().and_then(|lenses| lenses.first()) else {
            self.message = "no code lens at buffer".to_owned();
            return Ok(());
        };
        let command = if lens.get("command").is_some() {
            lens.get("command").cloned().unwrap_or_default()
        } else {
            client.request("codeLens/resolve", lens.clone())?.get("command").cloned().unwrap_or_default()
        };
        let title = command.get("title").and_then(serde_json::Value::as_str).unwrap_or("code lens").to_owned();
        if let Some(identifier) = command.get("command").and_then(serde_json::Value::as_str) {
            let _ = client.request(
                "workspace/executeCommand",
                serde_json::json!({
                    "command": identifier,
                    "arguments": command.get("arguments").cloned().unwrap_or_else(|| serde_json::json!([]))
                }),
            )?;
        }
        self.message = title;
        Ok(())
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

    pub(super) fn list_workspace_folders(&mut self) {
        self.message = if self.workspace_folders.is_empty() {
            "no workspace folders".to_owned()
        } else {
            self.workspace_folders.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(" · ")
        };
    }

    pub(super) fn execute_cdo(&mut self, command: ExCommand) -> Result<()> {
        let entries = self.quickfix.clone();
        if entries.is_empty() {
            self.message = "quickfix list is empty".to_owned();
            return Ok(());
        }
        for entry in entries {
            self.open_buffer(&entry.path)?;
            self.active.editor.set_cursor(self.active.editor.text().byte_of_line(entry.line.saturating_sub(1)));
            self.execute_ex_command(command.clone())?;
        }
        self.message = "cdo complete".to_owned();
        Ok(())
    }
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

fn code_action_command(action: &serde_json::Value) -> Option<(&str, serde_json::Value)> {
    let command = action.get("command")?;
    let (identifier, arguments_owner) = command
        .as_str()
        .map(|identifier| (identifier, action))
        .or_else(|| command.get("command").and_then(serde_json::Value::as_str).map(|identifier| (identifier, command)))?;
    let arguments = arguments_owner.get("arguments").cloned().unwrap_or_else(|| serde_json::json!([]));
    Some((identifier, arguments))
}
