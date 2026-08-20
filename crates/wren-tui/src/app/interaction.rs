use super::*;

impl App {
    pub(super) fn start_ace_jump(&mut self) -> Result<()> {
        self.ace_jump = Some(AceJumpState::AwaitTarget);
        self.message = "jump to character: ".to_owned();
        Ok(())
    }

    pub(super) fn handle_ace_jump_key(&mut self, key: TerminalKey) -> Result<()> {
        let Some(state) = self.ace_jump.take() else {
            return Ok(());
        };
        if key.code == TerminalKeyCode::Escape {
            self.message.clear();
            return Ok(());
        }
        match state {
            AceJumpState::AwaitTarget => {
                let TerminalKeyCode::Char(target) = key.code else {
                    return self.set_message("jump cancelled".to_owned());
                };
                self.populate_ace_jump(target);
            }
            AceJumpState::AwaitLabel { target, mut prefix, targets } => {
                match key.code {
                    TerminalKeyCode::Backspace => {
                        prefix.pop();
                    }
                    TerminalKeyCode::Char(label) => prefix.push(label.to_ascii_lowercase()),
                    _ => return self.set_message("jump cancelled".to_owned()),
                }
                let matching = targets.iter().filter(|candidate| candidate.label.starts_with(&prefix)).collect::<Vec<_>>();
                match matching.as_slice() {
                    [candidate] => self.finish_ace_jump(candidate.byte),
                    [] => self.message = format!("no {target:?} jump labeled {prefix}"),
                    _ => {
                        self.message = format!("jump {target:?}: {prefix}");
                        self.ace_jump = Some(AceJumpState::AwaitLabel { target, prefix, targets });
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn populate_ace_jump(&mut self, target: char) {
        let text = self.active.editor.contents();
        let top_line = self.views.active_window().top_line;
        let visible_lines = self.viewport_rows.saturating_sub(1).max(1);
        let start = self.active.editor.text().byte_of_line(top_line);
        let end = self.active.editor.text().byte_of_line(top_line.saturating_add(visible_lines)).max(start).min(text.len());
        let cursor = self.active.editor.primary_cursor();
        let bytes = text[start..end]
            .char_indices()
            .filter(|(_, character)| *character == target)
            .map(|(relative, _)| start + relative)
            .filter(|byte| *byte != cursor)
            .collect::<Vec<_>>();
        if bytes.is_empty() {
            self.message = format!("no {target:?} in view");
            return;
        }
        if bytes.len() == 1 {
            self.finish_ace_jump(bytes[0]);
            return;
        }
        let labels = ace_jump_labels(bytes.len());
        let targets = bytes.into_iter().zip(labels).map(|(byte, label)| AceJumpTarget { byte, label: label.into_boxed_str() }).collect::<Vec<_>>();
        self.message = format!("jump {target:?}: type label");
        self.ace_jump = Some(AceJumpState::AwaitLabel { target, prefix: String::new(), targets });
    }

    pub(super) fn finish_ace_jump(&mut self, byte: usize) {
        let origin = self.current_jump_location();
        self.active.editor.set_cursor(byte);
        if let (Some(origin), Some(target)) = (origin, self.current_jump_location()) {
            self.record_navigation(origin, target);
        }
        self.ace_jump = None;
        self.message.clear();
    }

    pub(super) fn ace_jump_overlay(&self) -> Option<AceJumpOverlay> {
        let AceJumpState::AwaitLabel { prefix, targets, .. } = self.ace_jump.as_ref()? else {
            return None;
        };
        Some(AceJumpOverlay {
            targets: targets
                .iter()
                .filter_map(|target| target.label.strip_prefix(prefix).map(|suffix| AceJumpTarget { byte: target.byte, label: suffix.into() }))
                .collect(),
        })
    }

    pub(super) fn show_which_key(&mut self, prefix: &str) {
        let mut entries = BTreeMap::<String, String>::new();
        if let Some(binding) = self.keymap.leader.get(prefix).filter(|binding| self.binding_enabled(binding)) {
            entries.insert("(wait)".to_owned(), binding.description.to_string());
        }
        let mut next_keys = BTreeSet::new();
        for (sequence, binding) in &self.keymap.leader {
            if self.binding_enabled(binding)
                && let Some(rest) = sequence.strip_prefix(prefix)
                && !rest.is_empty()
                && let Some(next) = rest.chars().next()
            {
                next_keys.insert(next);
            }
        }
        for next in next_keys {
            let candidate = format!("{prefix}{next}");
            let exact = self.keymap.leader.get(candidate.as_str()).filter(|binding| self.binding_enabled(binding));
            let longer = self
                .keymap
                .leader
                .iter()
                .any(|(sequence, binding)| sequence.len() > candidate.len() && sequence.starts_with(candidate.as_str()) && self.binding_enabled(binding));
            let group = self.keymap.groups.get(candidate.as_str()).or_else(|| self.keymap.groups.get(next.to_string().as_str())).map_or("group", Box::as_ref);
            let description = match (exact, longer) {
                (Some(binding), true) => format!("+{group} / {}", binding.description),
                (Some(binding), false) => binding.description.to_string(),
                (None, true) => format!("+{group}"),
                (None, false) => continue,
            };
            entries.insert(if prefix.is_empty() && next == ' ' { "Space".to_owned() } else { next.to_string() }, description);
        }
        let title = if prefix.is_empty() { " NORMAL ".to_owned() } else { format!(" {} ", self.keymap.groups.get(prefix).map_or(prefix, Box::as_ref)) };
        self.show_key_hints(title, entries);
    }

    pub(super) fn show_normal_prefix_hints(&mut self, prefix: char) {
        let language = language_bundle(self.active.document.presentation_path()).language_id;
        let navigation = self
            .lsps
            .iter()
            .find(|lsp| lsp.server.language_id == language.as_ref())
            .map(|lsp| lsp.capabilities.navigation)
            .or_else(|| self.lsp_job.as_ref().filter(|job| job.language_id.as_ref() == language.as_ref()).and_then(|job| job.navigation));
        let entries = normal_prefix_hint_entries(prefix, navigation);
        if entries.is_empty() {
            return;
        }
        let title = PREFIX_TITLES.iter().find(|(candidate, _)| *candidate == prefix).map_or(" NORMAL ", |(_, title)| *title);
        self.show_key_hints(title.to_owned(), entries);
    }

    fn show_key_hints(&mut self, title: String, entries: BTreeMap<String, String>) {
        let width = entries.keys().map(|key| key.chars().count()).max().unwrap_or(1);
        let text = entries.iter().map(|(key, description)| format!("{key:>width$}  {description}")).collect::<Vec<_>>().join("\n");
        self.popup = Some(TextPopup::new(title, text));
        self.popup_deadline = None;
    }

    pub(super) fn poll_mapping_timeout(&mut self) -> Result<bool> {
        if self.leader_deadline.is_none_or(|deadline| Instant::now() < deadline) {
            return Ok(false);
        }
        self.leader_deadline = None;
        let Some(sequence) = self.leader_keys.take() else {
            return Ok(false);
        };
        self.close_editor_popup();
        let execute = self.keymap.leader.get(sequence.as_str()).filter(|binding| self.binding_enabled(binding)).map(|binding| binding.execute);
        if let Some(execute) = execute {
            execute(self)?;
        } else {
            self.message = format!("incomplete mapping <Space>{sequence}");
        }
        Ok(true)
    }

    pub(super) fn show_info(&mut self, information: impl std::fmt::Display) {
        self.show_message(Severity::Info, information);
    }

    pub(super) fn show_error(&mut self, error: impl std::fmt::Display) {
        self.show_message(Severity::Error, error);
    }

    pub(super) fn set_message(&mut self, message: impl Into<String>) -> Result<()> {
        self.message = message.into();
        Ok(())
    }

    pub(super) fn show_message(&mut self, severity: Severity, message: impl std::fmt::Display) {
        let message = message.to_string();
        self.record_debug_output(severity, &message);
        self.message = message.clone();
        if severity == Severity::Error {
            self.popup = Some(TextPopup::new("Error", message));
            self.popup_deadline = Some(Instant::now() + Duration::from_secs(8));
        }
    }

    pub(super) fn capture_debug_output(&mut self) {
        if self.message.is_empty() {
            return;
        }
        let message = self.message.clone();
        self.record_debug_output(Severity::Info, &message);
    }

    pub(super) fn record_debug_output(&mut self, severity: Severity, text: &str) {
        const MAX_ENTRIES: usize = 512;
        if text.trim().is_empty() || self.debug_messages.last().is_some_and(|(_, previous)| previous.as_ref() == text) {
            return;
        }
        self.debug_messages.push((severity, text.into()));
        let overflow = self.debug_messages.len().saturating_sub(MAX_ENTRIES);
        if overflow > 0 {
            self.debug_messages.drain(..overflow);
        }
    }

    pub(super) fn show_debug_output(&mut self) -> Result<()> {
        self.capture_debug_output();
        let text = if self.debug_messages.is_empty() {
            "No debug output has been recorded.".to_owned()
        } else {
            self.debug_messages
                .iter()
                .enumerate()
                .map(|(index, (severity, text))| {
                    let severity = severity.label().to_ascii_uppercase();
                    format!("{:04} [{severity}] {text}", index + 1)
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        self.open_messages_buffer(text)?;
        self.close_editor_popup();
        self.message.clear();
        Ok(())
    }

    pub(super) fn open_messages_buffer(&mut self, text: String) -> Result<()> {
        let document_id = DocumentId::new(
            stable_hash(b"wren:virtual-buffer\0".iter().copied().chain(MESSAGES_BUFFER_NAME.bytes()).chain(std::iter::once(0)).chain(text.bytes())).max(2),
        );
        let active_is_messages = self.active.has_display_name(MESSAGES_BUFFER_NAME);
        let inactive_index = self.inactive.iter().position(|buffer| buffer.has_display_name(MESSAGES_BUFFER_NAME));
        let buffer_id = match (active_is_messages, inactive_index) {
            (true, _) => self.active.buffer_id,
            (false, Some(index)) => self.inactive[index].buffer_id,
            (false, None) => self.views.add_buffer(),
        };
        let mut messages = match BufferState::virtual_buffer(buffer_id, document_id, MESSAGES_BUFFER_NAME, text) {
            Ok(messages) => messages,
            Err(error) => {
                return Err(error);
            }
        };
        apply_client_state(&mut messages, &self.client_state)?;
        if let Some(pattern) = self.client_state.search_history.last() {
            messages.editor.set_search(pattern.clone(), self.last_search_direction)?;
        }
        self.mutations.register(document_id, messages.editor.contents(), true)?;

        if active_is_messages {
            self.active = messages;
        } else {
            self.autosave_active_if_named()?;
            let previous = std::mem::replace(&mut self.active, messages);
            if let Some(index) = inactive_index {
                self.inactive[index] = previous;
            } else {
                self.inactive.push(previous);
            }
            self.views.set_active_buffer(buffer_id);
        }
        self.decorations.remove(&buffer_id);
        self.semantic_decorations.remove(&buffer_id);
        self.prime_active_syntax();
        self.begin_lsp_start();
        Ok(())
    }

    pub(super) fn poll_popup_timeout(&mut self) -> bool {
        if self.popup_deadline.is_none_or(|deadline| Instant::now() < deadline) {
            return false;
        }
        self.popup_deadline = None;
        self.popup.take().is_some()
    }

    pub(super) fn dispatch_key(&mut self, key: KeyEvent) {
        if matches!(self.active.editor.mode(), Mode::Insert | Mode::Replace)
            && (matches!(key.code, KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace | KeyCode::Delete)
                || matches!(key.code, KeyCode::Char(_) if key.modifiers.is_empty())
                || matches!(key.code, KeyCode::Char('w' | 'u') if key.modifiers == Modifiers::CONTROL))
        {
            match self.active.editor.handle_key(key) {
                Ok(transaction) => {
                    self.message.clear();
                    self.after_effect(transaction, Vec::new());
                }
                Err(error) => self.engine_error(error),
            }
            return;
        }
        let registers_before = self.active.editor.registers().map(|(name, value)| (name, (value.text.clone(), value.linewise))).collect();
        let marks_before = self.active.editor.marks().collect();
        let macros_before = self.active.editor.macros().map(|(name, keys)| (name, keys.to_vec())).collect();
        let repeat_before = self.active.editor.durable_repeat_data();
        match self.active.editor.handle_key(key) {
            Ok(transaction) => {
                self.message.clear();
                let mut state_deltas = changed_editor_state(&registers_before, &marks_before, &macros_before, &self.active.editor, self.active.document_id);
                let repeat_after = self.active.editor.durable_repeat_data();
                if repeat_after != repeat_before
                    && let Some(repeat) = repeat_after
                {
                    state_deltas.push(StateDelta::RepeatData(repeat));
                }
                self.after_effect(transaction, state_deltas);
            }
            Err(error) => self.engine_error(error),
        }
    }

    pub(super) fn engine_error(&mut self, error: EngineError) {
        match error {
            EngineError::InvalidGrammar { sequence, reason } => {
                self.active.editor.cancel_pending();
                self.show_info(format!("grammar rejected sequence {:?}: {reason}", sequence.iter().map(format_key_event).collect::<String>()));
            }
            error => self.show_error(error),
        }
    }

    pub(super) fn after_transaction(&mut self, transactions: impl IntoIterator<Item = Transaction>) {
        self.after_effect(transactions.into_iter().collect(), Vec::new());
    }

    pub(super) fn after_effect(&mut self, transactions: TransactionBatch, state_deltas: Vec<StateDelta>) {
        if transactions.is_empty() && state_deltas.is_empty() {
            return;
        }
        self.apply_state_deltas(&state_deltas);
        if !transactions.is_empty() {
            self.after_text_transactions(&transactions);
        }
        if let Err(error) = self.mutations.append(self.active.document_id, transactions, state_deltas) {
            self.show_error(format!("mutation outbox: {error}"));
        }
    }

    fn apply_state_deltas(&mut self, state_deltas: &[StateDelta]) {
        for delta in state_deltas {
            self.client_state.apply(delta);
        }
        if state_deltas.is_empty() {
            return;
        }
        if let Err(error) = sync_client_state(&mut self.active, &mut self.inactive, &self.client_state) {
            self.show_error(format!("client state: {error}"));
        }
        self.client_state_worker.try_save(self.client_state.clone());
    }

    fn after_text_transactions(&mut self, transactions: &[Transaction]) {
        if let Some(semantic) = self.semantic_decorations.get_mut(&self.active.buffer_id) {
            for transaction in transactions {
                if semantic.revision != transaction.base_revision() {
                    break;
                }
                if let Some(revision) = semantic.revision.next() {
                    semantic.map_through(transaction, revision);
                }
            }
        }
        self.refresh_changed_syntax(transactions);
        if let Some(before) = self.active.git_index_text.as_ref().map(Arc::clone) {
            self.schedule_git_hunk_refresh(GitHunkRequest {
                buffer_id: self.active.buffer_id,
                revision: self.active.editor.revision(),
                before,
                after: self.active.editor.frame().text,
            });
        } else {
            self.active.git_hunks.clear();
        }
        self.schedule_lsp_semantics();
        if let Some(wal) = &self.active.wal {
            wal.append_frame(self.active.base_hash, self.active.editor.revision().get(), self.active.editor.frame().text, self.active.editor.primary_cursor());
        }
    }

    fn schedule_lsp_semantics(&mut self) {
        if let Some(lsp) = self.active_lsp_mut() {
            if lsp.capabilities.semantic_legend.is_some() {
                lsp.semantic_due = Some(Instant::now() + LSP_SEMANTIC_IDLE_PERIOD);
            }
        } else if self.lsp_job.is_some() {
            self.lsp_semantic_dirty = true;
        }
    }
}

pub(super) fn normal_prefix_hint_entries(prefix: char, navigation: Option<LspNavigationCapabilities>) -> BTreeMap<String, String> {
    PREFIX_BINDINGS
        .iter()
        .filter(|binding| binding.prefix == prefix)
        .filter(|binding| binding.enabled.is_none_or(|enabled| enabled(navigation.unwrap_or_default())))
        .map(|binding| (binding.key.to_string(), binding.description.to_owned()))
        .collect()
}
