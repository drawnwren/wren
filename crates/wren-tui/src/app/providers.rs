use super::*;

impl App {
    /// Install the local Tree-sitter baseline before the first frame for a
    /// normal document. LSP semantic tokens remain fully asynchronous and
    /// refine these spans when the language server responds.
    pub(super) fn prime_active_syntax(&mut self) {
        let revision = self.active.editor.revision();
        if self.decorations.get(&self.active.buffer_id).is_some_and(|state| state.revision == revision) {
            return;
        }
        if !self.active.class.policy().whole_document_syntax {
            return;
        }
        let bundle = language_bundle(self.active.document.presentation_path());
        let language_id = bundle.language_id.clone();
        let frame = self.active.editor.frame();
        let text = frame.text.materialize_for_task();
        let spans = provider_decorations(highlight_text(&text, &language_id));
        self.decorations.insert(self.active.buffer_id, BufferDecorations::new(revision, spans));
        self.provider_refresh_ranges.entry(self.active.document_id).or_insert_with(|| Vec::with_capacity(4));
    }

    /// Reparse a bounded context around changed lines before the next frame.
    /// Existing full-buffer spans have already been transaction-mapped, so
    /// newly typed syntax appears immediately without a remote LSP round trip
    /// or a whole-file parse on every keypress.
    pub(super) fn refresh_changed_syntax(&mut self, transactions: &[Transaction]) {
        if transactions.iter().all(Transaction::is_empty) {
            return;
        }
        const MAX_PENDING_PROVIDER_TRANSACTIONS: usize = 64;
        let document_id = self.active.document_id;
        let pending_transactions = self.provider_pending_transactions.entry(document_id).or_default();
        if pending_transactions.len().saturating_add(transactions.len()) > MAX_PENDING_PROVIDER_TRANSACTIONS {
            pending_transactions.clear();
            self.provider_resync_required.insert(document_id);
        } else {
            pending_transactions.extend(transactions.iter().cloned());
        }
        let frame = self.active.editor.frame();
        let language_id = language_bundle(self.active.document.presentation_path()).language_id;
        let (mut targets, replacement) = changed_syntax_batch(&frame.text, transactions, &language_id);
        if targets.is_empty() {
            return;
        }
        targets.sort_by_key(|target| target.start);
        let pending = self.provider_refresh_ranges.entry(self.active.document_id).or_default();
        *pending =
            std::mem::take(pending).into_iter().filter_map(|range| transactions.iter().try_fold(range, map_range)).chain(targets.iter().cloned()).collect();
        merge_ranges(pending);
        let revision = self.active.editor.revision();
        let state = self.decorations.entry(self.active.buffer_id).or_insert_with(|| BufferDecorations::new(revision, Vec::new()));
        if transactions.first().is_some_and(|transaction| state.revision == transaction.base_revision()) {
            for transaction in transactions {
                let Some(next) = state.revision.next() else {
                    return;
                };
                state.map_through(transaction, next);
            }
            state.replace_ranges(revision, &targets, replacement);
        } else {
            let text = frame.text.materialize_for_task();
            *state = BufferDecorations::new(revision, provider_decorations(highlight_text(&text, &language_id)));
        }
        self.provider_refresh_due.insert(self.active.document_id, Instant::now() + Duration::from_millis(50));
    }

    /// Prepare the exact first-insert decoration states for both the active
    /// viewport and the prefetched document end. This fills only the retained
    /// visible-slice cache; document text, provider authority, and revisions
    /// remain unchanged.
    pub(super) fn prepare_realtime_decoration_updates(&self) {
        if self.realtime_decorations_prepared.get()
            || self.active.editor.mode() != Mode::Normal
            || self.views.window_count() != 1
            || !self.active.class.policy().whole_document_syntax
        {
            return;
        }
        self.realtime_decorations_prepared.set(true);

        let revision = self.active.editor.revision();
        let frame = self.active.editor.frame();
        let language_id = language_bundle(self.active.document.presentation_path()).language_id;
        let rows = self.viewport_rows.max(1);
        let margin = 3.min(rows.saturating_sub(1) / 2);
        let initial_top = self.views.active_window().top_line;
        let end = self.active.editor.document_end_byte();
        let end_line = frame.text.line_of_byte(end);
        let end_line_start = frame.text.byte_of_line(end_line);
        let previous_line = end_line.checked_sub(1).map(|line| {
            let start = frame.text.byte_of_line(line);
            let content_end = end_line_start.saturating_sub(1).max(start);
            start + (end - end_line_start).min(content_end - start)
        });
        let mut candidates = vec![self.active.editor.primary_cursor(), end];
        candidates.extend(previous_line);
        candidates.sort_unstable();
        candidates.dedup();
        for cursor in candidates {
            let Ok(transaction) = Transaction::new(revision, vec![Edit::new(cursor..cursor, "x")]) else {
                continue;
            };
            let Ok(preview) = frame.text.edited(&transaction) else {
                continue;
            };
            let cursor_line = preview.line_of_byte(cursor.saturating_add(1).min(preview.len()));
            let top_line = viewport_top_with_margin(initial_top, cursor_line, rows, margin);
            let start = preview.byte_of_line(top_line);
            let visible = start..preview.byte_of_line(top_line.saturating_add(rows).saturating_add(1));

            if let Some(syntax) = self.decorations.get(&self.active.buffer_id).filter(|state| state.revision == revision) {
                let (targets, replacement) = changed_syntax(&preview, &transaction, &language_id);
                syntax.prepare_replaced_visible(&transaction, &targets, replacement, visible.clone());
            }
            if let Some(semantic) = self.semantic_decorations.get(&self.active.buffer_id).filter(|state| state.revision == revision) {
                semantic.prepare_mapped_visible(&transaction, visible);
            }
        }
    }

    pub(super) fn schedule_provider_refreshes(&mut self, viewport_height: usize) {
        let viewport_height = viewport_height.max(1);
        let mut line_ranges: BTreeMap<BufferId, (usize, usize)> = BTreeMap::new();
        let mut windows = Vec::with_capacity(self.views.window_count());
        self.views.visit_windows(|window| windows.push((window.id, window.buffer_id, window.top_line)));
        for (window_id, buffer_id, top_line) in windows {
            let Some(buffer) = self.buffer(buffer_id) else {
                continue;
            };
            let cursor_line = buffer.editor.cursor_line_column().0;
            let margin = 3.min(viewport_height.saturating_sub(1) / 2);
            let effective_top = viewport_top_with_margin(top_line, cursor_line, viewport_height, margin);
            if let Some(window) = self.views.window_mut(window_id) {
                window.top_line = effective_top;
            }
            let range = line_ranges.entry(buffer_id).or_insert((effective_top, effective_top + viewport_height));
            range.0 = range.0.min(effective_top);
            range.1 = range.1.max(effective_top + viewport_height);
        }
        let refreshes = line_ranges
            .into_iter()
            .filter_map(|(buffer_id, (top_line, bottom_line))| {
                let buffer = self.buffer(buffer_id)?;
                let full_syntax_is_current = buffer.class.policy().whole_document_syntax
                    && self.decorations.get(&buffer_id).is_some_and(|state| state.revision == buffer.editor.revision());
                let pending_refresh = self.provider_refresh_ranges.get(&buffer.document_id).filter(|ranges| !ranges.is_empty());
                if full_syntax_is_current && pending_refresh.is_none() {
                    return None;
                }
                let text_store = buffer.editor.text();
                let (visible, near_viewport) = if full_syntax_is_current {
                    let pending = pending_refresh?;
                    let start = pending.first()?.start;
                    let end = pending.last()?.end.max(start);
                    (start..end, start..end)
                } else {
                    let visible_start = text_store.byte_of_line(top_line);
                    let visible_end = text_store.byte_of_line(bottom_line).max(visible_start);
                    let near_start = text_store.byte_of_line(top_line.saturating_sub(viewport_height));
                    let near_end = text_store.byte_of_line(bottom_line.saturating_add(viewport_height)).max(visible_end);
                    (visible_start..visible_end, near_start..near_end)
                };
                Some(ProviderRefresh {
                    buffer_id,
                    document_id: buffer.document_id,
                    revision: buffer.editor.revision(),
                    text: buffer.editor.frame().text,
                    transactions: if self.provider_resync_required.contains(&buffer.document_id) {
                        Vec::new()
                    } else {
                        self.provider_pending_transactions.get(&buffer.document_id).cloned().unwrap_or_default()
                    },
                    bundle: language_bundle(buffer.document.presentation_path()),
                    visible,
                    near_viewport,
                })
            })
            .collect::<Vec<_>>();
        let now = Instant::now();
        for refresh in refreshes {
            self.submit_provider_refresh(refresh, now);
        }
    }

    fn submit_provider_refresh(&mut self, refresh: ProviderRefresh, now: Instant) {
        let key = ProviderDemandKey::from(&refresh);
        let already_submitted = self.provider_submitted.get(&refresh.document_id) == Some(&key);
        let still_debouncing = self.provider_refresh_due.get(&refresh.document_id).is_some_and(|due| now < *due);
        if already_submitted || still_debouncing {
            return;
        }
        let document_id = refresh.document_id;
        if self.provider.try_refresh(refresh) {
            self.provider_submitted.insert(document_id, key);
            self.provider_refresh_due.remove(&document_id);
            self.provider_pending_transactions.remove(&document_id);
            self.provider_resync_required.remove(&document_id);
        }
    }

    pub(super) fn poll_provider_results(&mut self) -> bool {
        let mut changed = false;
        while let Some(result) = self.provider.try_result() {
            match result {
                ProviderWorkerResult::Decorations { buffer_id, document_id, revision, spans, ranges } => {
                    let current_revision = self.buffer(buffer_id).map(|buffer| buffer.editor.revision());
                    if current_revision != Some(revision) {
                        continue;
                    }
                    let spans = spans.into_iter().map(provider_decoration).collect::<Vec<_>>();
                    let state = self.decorations.entry(buffer_id).or_insert_with(|| BufferDecorations::new(revision, Vec::new()));
                    if state.revision == revision {
                        state.state.replace_ranges(&ranges, spans);
                    } else {
                        *state = BufferDecorations::new(revision, spans);
                    }
                    self.provider_refresh_ranges.entry(document_id).or_insert_with(|| Vec::with_capacity(4)).clear();
                    changed = true;
                    self.provider_submitted.entry(document_id).or_insert(ProviderDemandKey { revision, visible: 0..0, near_viewport: 0..0 });
                }
                ProviderWorkerResult::Failed { document_id, message } => {
                    self.provider_submitted.remove(&document_id);
                    self.provider_resync_required.insert(document_id);
                    self.show_error(format!("provider: {message}"));
                    changed = true;
                }
                ProviderWorkerResult::Completion { document_id, mut session } => {
                    if document_id == self.active.document_id && session.revision == self.active.editor.revision() {
                        if let Some(lsp) = self.lsp_completion.take()
                            && lsp.revision == session.revision
                        {
                            session = CompletionSession::merge(session.revision, session.replace, session.candidates, lsp.candidates);
                        }
                        self.completion_index = 0;
                        self.completion_selected = false;
                        self.completion_documentation_scroll = 0;
                        self.completion = Some(session);
                        self.update_completion_message();
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    pub(super) fn poll_git_hunk_results(&mut self) -> bool {
        self.flush_due_git_hunk_refreshes();
        let mut changed = false;
        while let Some(result) = self.git_worker.try_result() {
            let buffer = if self.active.buffer_id == result.buffer_id {
                Some(&mut self.active)
            } else {
                self.inactive.iter_mut().find(|buffer| buffer.buffer_id == result.buffer_id)
            };
            let Some(buffer) = buffer else {
                continue;
            };
            if buffer.editor.revision() == result.revision && buffer.git_hunks != result.hunks {
                buffer.git_hunks = result.hunks;
                changed = true;
            }
        }
        changed
    }

    pub(super) fn schedule_git_hunk_refresh(&mut self, request: GitHunkRequest) {
        let due = Instant::now() + GIT_HUNK_IDLE_PERIOD;
        if let Some(pending) = self.pending_git_refreshes.iter_mut().find(|pending| pending.request.buffer_id == request.buffer_id) {
            *pending = PendingGitHunkRefresh { due, request };
        } else {
            self.pending_git_refreshes.push(PendingGitHunkRefresh { due, request });
        }
    }

    fn flush_due_git_hunk_refreshes(&mut self) {
        let now = Instant::now();
        let mut index = 0;
        while index < self.pending_git_refreshes.len() {
            if self.pending_git_refreshes[index].due <= now {
                let pending = self.pending_git_refreshes.swap_remove(index);
                self.git_worker.refresh(pending.request);
            } else {
                index += 1;
            }
        }
    }

    pub(super) fn request_completion(&mut self) {
        let (replace, local_candidates) = self.local_completion_candidates();
        self.lsp_completion = self.request_lsp_completion().ok().flatten();
        if !local_candidates.is_empty() {
            if let Some(completion) = &mut self.lsp_completion {
                completion.candidates.extend(local_candidates);
            } else {
                self.lsp_completion = Some(CompletionSession { revision: self.active.editor.revision(), replace, candidates: local_candidates });
            }
        }
        if let Some(lsp) = &self.lsp_completion {
            self.completion = Some(CompletionSession::merge(lsp.revision, lsp.replace.clone(), Vec::new(), lsp.candidates.clone()));
            self.completion_selected = false;
            self.completion_index = 0;
            self.completion_documentation_scroll = 0;
        }
        let completion = ProviderCompletion {
            document_id: self.active.document_id,
            revision: self.active.editor.revision(),
            text: self.active.editor.frame().text,
            bundle: language_bundle(self.active.document.presentation_path()),
            byte: self.active.editor.primary_cursor(),
        };
        if self.provider.try_complete(completion) {
            self.message = "completion…".to_owned();
        } else {
            self.message = "completion queue is busy".to_owned();
        }
    }

    pub(super) fn local_completion_candidates(&self) -> (Range<usize>, Vec<CompletionCandidate>) {
        let text = self.active.editor.contents();
        let cursor = self.active.editor.primary_cursor().min(text.len());
        let word_start = identifier_prefix_start(&text, cursor);
        let mut candidates = self.path_completion_candidates(&text, cursor);
        candidates.extend(self.vsnip_completion_candidates(&text[word_start..cursor], word_start..cursor));
        (word_start..cursor, candidates)
    }

    pub(super) fn path_completion_candidates(&self, text: &str, cursor: usize) -> Vec<CompletionCandidate> {
        let token_start = text[..cursor]
            .char_indices()
            .rev()
            .find(|(_, character)| character.is_whitespace() || matches!(character, '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'))
            .map_or(0, |(byte, character)| byte + character.len_utf8());
        let token = &text[token_start..cursor];
        if token.len() > 512 || token.contains("::") {
            return Vec::new();
        }
        let (typed_directory, name_prefix) = token.rsplit_once('/').map_or(("", token), |(directory, name)| (&token[..directory.len() + 1], name));
        let expanded_directory = typed_directory.strip_prefix("~/").map_or_else(
            || {
                let directory = Path::new(typed_directory);
                if directory.is_absolute() { directory.to_path_buf() } else { env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(directory) }
            },
            |relative| env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join(relative),
        );
        let Ok(entries) = std::fs::read_dir(&expanded_directory) else {
            return Vec::new();
        };
        let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
        entries.sort_by_key(|entry| (!entry.path().is_dir(), entry.file_name()));
        entries
            .into_iter()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name_prefix.is_empty() && !name.to_ascii_lowercase().starts_with(&name_prefix.to_ascii_lowercase()) {
                    return None;
                }
                let directory = entry.path().is_dir();
                let label = format!("{name}{}", if directory { "/" } else { "" });
                Some(CompletionCandidate {
                    label: label.clone().into(),
                    insert: format!("{typed_directory}{label}").into(),
                    source: "path".into(),
                    detail: if directory { "Directory" } else { "File" }.into(),
                    documentation: entry.path().display().to_string().into(),
                    replace: Some(token_start..cursor),
                    snippet: None,
                })
            })
            .take(64)
            .collect()
    }

    pub(super) fn vsnip_completion_candidates(&self, prefix: &str, replace: Range<usize>) -> Vec<CompletionCandidate> {
        let language = language_bundle(self.active.document.presentation_path()).language_id;
        let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
            return Vec::new();
        };
        let paths = [home.join(".vsnip").join(format!("{language}.json")), home.join(".config/nvim/snippets").join(format!("{language}.json"))];
        let mut candidates = Vec::new();
        for path in paths.into_iter().filter(|path| path.exists()) {
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(snippets) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&source) else {
                continue;
            };
            for (name, snippet) in snippets {
                let prefixes = snippet.get("prefix").map_or_else(
                    || vec![name.as_str()],
                    |value| {
                        value
                            .as_array()
                            .map_or_else(|| value.as_str().into_iter().collect(), |values| values.iter().filter_map(serde_json::Value::as_str).collect())
                    },
                );
                let body = snippet.get("body").map_or_else(String::new, |body| {
                    body.as_array().map_or_else(
                        || body.as_str().unwrap_or_default().to_owned(),
                        |lines| lines.iter().filter_map(serde_json::Value::as_str).collect::<Vec<_>>().join("\n"),
                    )
                });
                if body.is_empty() {
                    continue;
                }
                let description = snippet.get("description").map(render_lsp_text).unwrap_or_else(|| name.clone());
                for snippet_prefix in prefixes {
                    if !prefix.is_empty() && !snippet_prefix.to_ascii_lowercase().starts_with(&prefix.to_ascii_lowercase()) {
                        continue;
                    }
                    candidates.push(CompletionCandidate {
                        label: snippet_prefix.into(),
                        insert: expand_lsp_snippet(&body).into(),
                        source: "vsnip".into(),
                        detail: name.as_str().into(),
                        documentation: description.as_str().into(),
                        replace: Some(replace.clone()),
                        snippet: Some(body.as_str().into()),
                    });
                }
            }
        }
        candidates
    }

    pub(super) fn completion_overlay(&self) -> Option<CompletionOverlay> {
        let session = self.completion.as_ref()?;
        if session.candidates.is_empty() {
            return None;
        }
        let selected = self.completion_selected.then_some(self.completion_index.min(session.candidates.len() - 1));
        let documentation = selected.and_then(|index| session.candidates.get(index)).map_or("", |candidate| candidate.documentation.as_ref());
        Some(CompletionOverlay {
            rows: session
                .candidates
                .iter()
                .map(|candidate| MenuOverlayRow { label: candidate.label.clone(), detail: candidate.detail.clone(), source: Some(candidate.source.clone()) })
                .collect(),
            selected,
            documentation: documentation.into(),
            documentation_scroll: self.completion_documentation_scroll,
        })
    }

    pub(super) fn move_completion(&mut self, direction: isize) {
        let Some(session) = &self.completion else {
            return;
        };
        self.completion_index = match (session.candidates.len(), direction.is_negative()) {
            (0, _) => 0,
            (length, true) => self.completion_index.checked_sub(1).unwrap_or(length - 1),
            (length, false) => (self.completion_index + 1) % length,
        };
        self.completion_selected = !session.candidates.is_empty();
        self.completion_documentation_scroll = 0;
        self.update_completion_message();
    }

    pub(super) fn update_completion_message(&mut self) {
        self.message = self.completion.as_ref().map_or_else(
            || "no completion".to_owned(),
            |session| {
                session.candidates.get(self.completion_index).map_or_else(
                    || "no completion candidates".to_owned(),
                    |candidate| {
                        format!(
                            "completion [{}/{}] {} · {} · Enter accepts, Ctrl-N/P cycles",
                            self.completion_index + 1,
                            session.candidates.len(),
                            candidate.label,
                            candidate.source
                        )
                    },
                )
            },
        );
    }

    pub(super) fn accept_completion(&mut self) -> Result<()> {
        let Some(session) = self.completion.take() else {
            return Ok(());
        };
        let candidate = session.candidates.get(self.completion_index).cloned();
        let replace_start =
            candidate.as_ref().map_or(session.replace.start, |candidate| candidate.replace.as_ref().map_or(session.replace.start, |range| range.start));
        let transaction = session.accept(self.active.editor.revision(), self.completion_index)?;
        if let Some(transaction) = transaction {
            self.active.editor.apply_transaction(transaction.clone())?;
            self.after_transaction(Some(transaction));
            if let Some(snippet) = candidate.and_then(|candidate| candidate.snippet) {
                let (_, stops) = expand_lsp_snippet_with_stops(&snippet);
                self.snippet_stops = stops.into_iter().map(|range| replace_start + range.start..replace_start + range.end).collect();
                self.snippet_stop_index = 0;
                if let Some(range) = self.snippet_stops.first().cloned() {
                    self.active.editor.set_selection_range(range);
                }
            }
            self.message = "completion accepted".to_owned();
        }
        Ok(())
    }

    pub(super) fn move_snippet_stop(&mut self, direction: isize) {
        if self.snippet_stops.is_empty() {
            return;
        }
        match (direction.is_negative(), self.snippet_stop_index + 1 >= self.snippet_stops.len()) {
            (true, _) => self.snippet_stop_index = self.snippet_stop_index.saturating_sub(1),
            (false, true) => {
                if let Some(range) = self.snippet_stops.last() {
                    self.active.editor.set_cursor(range.end);
                }
                self.snippet_stops.clear();
                self.snippet_stop_index = 0;
                return;
            }
            (false, false) => self.snippet_stop_index += 1,
        }
        if let Some(range) = self.snippet_stops.get(self.snippet_stop_index).cloned() {
            self.active.editor.set_selection_range(range);
        }
    }
}

fn changed_syntax(text: &FrameText, transaction: &Transaction, language_id: &str) -> (Vec<Range<usize>>, Vec<DecorationSpan>) {
    changed_syntax_ranges(
        text,
        transaction.edits().iter().filter_map(|edit| transaction.map_range(edit.range.clone(), Bias::Left, Bias::Right).ok()),
        language_id,
    )
}

fn changed_syntax_batch(text: &FrameText, transactions: &[Transaction], language_id: &str) -> (Vec<Range<usize>>, Vec<DecorationSpan>) {
    let changed = transactions.iter().enumerate().flat_map(|(index, transaction)| {
        transaction.edits().iter().filter_map(move |edit| {
            let range = transaction.map_range(edit.range.clone(), Bias::Left, Bias::Right).ok()?;
            transactions[index + 1..].iter().try_fold(range, |range, following| {
                Some(following.map_offset(range.start, Bias::Left).ok()?..following.map_offset(range.end, Bias::Right).ok()?)
            })
        })
    });
    changed_syntax_ranges(text, changed, language_id)
}

fn changed_syntax_ranges(text: &FrameText, changed: impl IntoIterator<Item = Range<usize>>, language_id: &str) -> (Vec<Range<usize>>, Vec<DecorationSpan>) {
    let text_len = text.len();
    let mut targets = changed
        .into_iter()
        .map(|range| {
            let start = range.start;
            let end = range.end;
            let start_line = text.line_of_byte(start.min(text_len));
            let end_line = text.line_of_byte(end.min(text_len));
            let target_start_line = start_line.saturating_sub(1);
            let target_end_line = end_line.saturating_add(2);
            text.byte_of_line(target_start_line)..text.byte_of_line(target_end_line).max(start).min(text_len)
        })
        .collect::<Vec<_>>();
    targets.sort_by_key(|target| target.start);
    merge_ranges(&mut targets);
    let replacement = targets
        .iter()
        .flat_map(|target| {
            let slice = text.slice(target.clone());
            let spans = provider_decorations(highlight_text(slice.as_ref(), language_id));
            spans.into_iter().map(|mut span| {
                span.range.start = span.range.start.saturating_add(target.start);
                span.range.end = span.range.end.saturating_add(target.start);
                span
            })
        })
        .collect();
    (targets, replacement)
}

fn viewport_top_with_margin(top_line: usize, cursor_line: usize, viewport_height: usize, margin: usize) -> usize {
    let minimum = cursor_line.saturating_add(margin).saturating_add(1).saturating_sub(viewport_height);
    let maximum = cursor_line.saturating_sub(margin).max(minimum);
    top_line.clamp(minimum, maximum)
}
