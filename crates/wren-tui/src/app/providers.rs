use super::*;

impl App {
    /// Install the local Tree-sitter baseline before the first frame for a
    /// normal document. LSP semantic tokens remain fully asynchronous and
    /// refine these spans when the language server responds.
    pub(super) fn prime_active_syntax(&mut self) {
        let revision = self.active.editor.revision();
        if self
            .decorations
            .get(&self.active.buffer_id)
            .is_some_and(|state| state.revision == revision)
        {
            return;
        }
        if !self.active.class.policy().whole_document_syntax {
            return;
        }
        let bundle = language_bundle(self.active.document.presentation_path());
        let frame = self.active.editor.frame();
        let text = frame.text.as_ref();
        let spans = self
            .provider
            .highlight_now(
                self.active.document_id,
                revision,
                frame.text.shared(),
                bundle,
            )
            .unwrap_or_else(|_| lexical_highlight_text(text))
            .into_iter()
            .map(|span| provider_decoration(span, self.theme))
            .collect::<Vec<_>>();
        self.decorations.insert(
            self.active.buffer_id,
            BufferDecorations::new(revision, spans),
        );
    }

    /// Reparse a bounded context around changed lines before the next frame.
    /// Existing full-buffer spans have already been transaction-mapped, so
    /// newly typed syntax appears immediately without a remote LSP round trip
    /// or a whole-file parse on every keypress.
    pub(super) fn refresh_changed_syntax(&mut self, transaction: &Transaction) {
        if transaction.edits.is_empty() {
            return;
        }
        let text_store = self.active.editor.text();
        let text_len = text_store.len_bytes();
        let mut targets = transaction
            .edits
            .iter()
            .filter_map(|edit| {
                let start = transaction.map_offset(edit.range.start, Bias::Left).ok()?;
                let end = transaction.map_offset(edit.range.end, Bias::Right).ok()?;
                let start_line = text_store.line_of_byte(start.min(text_len));
                let end_line = text_store.line_of_byte(end.min(text_len));
                let target_start_line = start_line.saturating_sub(1);
                let target_end_line = end_line.saturating_add(2);
                let target = text_store.byte_of_line(target_start_line)
                    ..text_store
                        .byte_of_line(target_end_line)
                        .max(start)
                        .min(text_len);
                let context = text_store.byte_of_line(target_start_line.saturating_sub(32))
                    ..text_store
                        .byte_of_line(target_end_line.saturating_add(32))
                        .max(target.end)
                        .min(text_len);
                Some((target, context))
            })
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return;
        }
        targets.sort_by_key(|(target, _)| target.start);
        let frame = self.active.editor.frame();
        let text = frame.text.as_ref();
        let language = language_bundle(self.active.document.presentation_path()).language_id;
        let mut replacement = Vec::new();
        for (target, context) in &targets {
            let Some(source) = text.get(context.clone()) else {
                continue;
            };
            replacement.extend(
                highlight_text(source, &language)
                    .into_iter()
                    .map(|mut span| {
                        span.range.start = span.range.start.saturating_add(context.start);
                        span.range.end = span.range.end.saturating_add(context.start);
                        span
                    })
                    .filter(|span| span.range.start < target.end && target.start < span.range.end)
                    .map(|span| provider_decoration(span, self.theme)),
            );
        }
        let revision = self.active.editor.revision();
        let state = self
            .decorations
            .entry(self.active.buffer_id)
            .or_insert_with(|| BufferDecorations::new(revision, Vec::new()));
        state.spans.retain(|span| {
            !targets
                .iter()
                .any(|(target, _)| span.range.start < target.end && target.start < span.range.end)
        });
        state.spans.extend(replacement);
        state
            .spans
            .sort_by_key(|span| (span.range.start, std::cmp::Reverse(span.range.end)));
        state.spans.dedup();
        state.revision = revision;
        state.rebuild_index();
    }

    pub(super) fn schedule_provider_refreshes(&mut self, viewport_height: usize) {
        let viewport_height = viewport_height.max(1);
        let mut line_ranges: BTreeMap<BufferId, (usize, usize)> = BTreeMap::new();
        let windows = self
            .views
            .windows
            .iter()
            .enumerate()
            .map(|(index, window)| (index, window.buffer_id, window.top_line))
            .collect::<Vec<_>>();
        for (window_index, buffer_id, top_line) in windows {
            let Some(buffer) = self.buffer(buffer_id) else {
                continue;
            };
            let cursor_line = buffer.editor.cursor_line_column().0;
            let margin = 3.min(viewport_height.saturating_sub(1) / 2);
            let effective_top = if cursor_line < top_line.saturating_add(margin) {
                cursor_line.saturating_sub(margin)
            } else if cursor_line.saturating_add(margin) >= top_line.saturating_add(viewport_height)
            {
                cursor_line
                    .saturating_add(margin)
                    .saturating_add(1)
                    .saturating_sub(viewport_height)
            } else {
                top_line
            };
            if let Some(window) = self.views.windows.get_mut(window_index) {
                window.top_line = effective_top;
            }
            let range = line_ranges
                .entry(buffer_id)
                .or_insert((effective_top, effective_top + viewport_height));
            range.0 = range.0.min(effective_top);
            range.1 = range.1.max(effective_top + viewport_height);
        }
        let refreshes = line_ranges
            .into_iter()
            .filter_map(|(buffer_id, (top_line, bottom_line))| {
                let buffer = self.buffer(buffer_id)?;
                let text_store = buffer.editor.text();
                let visible_start = text_store.byte_of_line(top_line);
                let visible_end = text_store.byte_of_line(bottom_line).max(visible_start);
                let near_start = text_store.byte_of_line(top_line.saturating_sub(viewport_height));
                let near_end = text_store
                    .byte_of_line(bottom_line.saturating_add(viewport_height))
                    .max(visible_end);
                Some(ProviderRefresh {
                    buffer_id,
                    document_id: buffer.document_id,
                    revision: buffer.editor.revision(),
                    text: buffer.editor.frame().text,
                    bundle: language_bundle(buffer.document.presentation_path()),
                    visible: visible_start..visible_end,
                    near_viewport: near_start..near_end,
                })
            })
            .collect::<Vec<_>>();
        for refresh in refreshes {
            let key = ProviderDemandKey::from(&refresh);
            if self.provider_submitted.get(&refresh.document_id) == Some(&key) {
                continue;
            }
            if self.provider.try_refresh(refresh.clone()) {
                self.provider_submitted.insert(refresh.document_id, key);
            }
        }
    }

    pub(super) fn poll_provider_results(&mut self) -> bool {
        let mut changed = false;
        while let Some(result) = self.provider.try_result() {
            match result {
                ProviderWorkerResult::Decorations {
                    buffer_id,
                    document_id,
                    revision,
                    spans,
                    ranges,
                } => {
                    let current_revision = self
                        .buffer(buffer_id)
                        .map(|buffer| buffer.editor.revision());
                    if current_revision != Some(revision) {
                        continue;
                    }
                    let spans = spans
                        .into_iter()
                        .map(|span| provider_decoration(span, self.theme))
                        .collect::<Vec<_>>();
                    let mut merged = self
                        .decorations
                        .get(&buffer_id)
                        .filter(|state| state.revision == revision)
                        .map_or_else(Vec::new, |state| state.spans.clone());
                    merged.retain(|span| {
                        !ranges.iter().any(|range| {
                            span.range.start < range.end && range.start < span.range.end
                        })
                    });
                    merged.extend(spans);
                    // Paint broader parent captures first so narrower semantic
                    // captures at the same start offset win deterministically.
                    merged
                        .sort_by_key(|span| (span.range.start, std::cmp::Reverse(span.range.end)));
                    merged.dedup();
                    let next = BufferDecorations::new(revision, merged);
                    if self.decorations.get(&buffer_id) != Some(&next) {
                        self.decorations.insert(buffer_id, next);
                        changed = true;
                    }
                    self.provider_submitted
                        .entry(document_id)
                        .or_insert(ProviderDemandKey {
                            revision,
                            visible_start: 0,
                            visible_end: 0,
                            near_start: 0,
                            near_end: 0,
                        });
                }
                ProviderWorkerResult::Failed {
                    document_id,
                    message,
                } => {
                    self.provider_submitted.remove(&document_id);
                    self.show_error(format!("provider: {message}"));
                    changed = true;
                }
                ProviderWorkerResult::Completion {
                    document_id,
                    mut session,
                } => {
                    if document_id == self.active.document_id
                        && session.revision == self.active.editor.revision()
                    {
                        if let Some(lsp) = self.lsp_completion.take()
                            && lsp.revision == session.revision
                        {
                            session = CompletionSession::merge(
                                session.revision,
                                session.replace,
                                session.candidates,
                                lsp.candidates,
                            );
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
        let mut changed = false;
        while let Some(result) = self.git_worker.try_result() {
            let buffer = if self.active.buffer_id == result.buffer_id {
                Some(&mut self.active)
            } else {
                self.inactive
                    .iter_mut()
                    .find(|buffer| buffer.buffer_id == result.buffer_id)
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

    pub(super) fn request_completion(&mut self) {
        let (replace, local_candidates) = self.local_completion_candidates();
        self.lsp_completion = self.request_lsp_completion().ok().flatten();
        if !local_candidates.is_empty() {
            if let Some(completion) = &mut self.lsp_completion {
                completion.candidates.extend(local_candidates);
            } else {
                self.lsp_completion = Some(LspCompletion {
                    revision: self.active.editor.revision(),
                    replace,
                    candidates: local_candidates,
                });
            }
        }
        if let Some(lsp) = &self.lsp_completion {
            self.completion = Some(CompletionSession::merge(
                lsp.revision,
                lsp.replace.clone(),
                Vec::new(),
                lsp.candidates.clone(),
            ));
            self.completion_selected = false;
            self.completion_index = 0;
            self.completion_documentation_scroll = 0;
        }
        let completion = ProviderCompletion {
            document_id: self.active.document_id,
            revision: self.active.editor.revision(),
            text: self.active.editor.frame().text.shared(),
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
        let word_start = text[..cursor]
            .char_indices()
            .rev()
            .take_while(|(_, character)| character.is_alphanumeric() || *character == '_')
            .last()
            .map_or(cursor, |(byte, _)| byte);
        let mut candidates = self.path_completion_candidates(&text, cursor);
        candidates.extend(
            self.vsnip_completion_candidates(&text[word_start..cursor], word_start..cursor),
        );
        (word_start..cursor, candidates)
    }

    pub(super) fn path_completion_candidates(
        &self,
        text: &str,
        cursor: usize,
    ) -> Vec<CompletionCandidate> {
        let token_start = text[..cursor]
            .char_indices()
            .rev()
            .find(|(_, character)| {
                character.is_whitespace()
                    || matches!(
                        character,
                        '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
                    )
            })
            .map_or(0, |(byte, character)| byte + character.len_utf8());
        let token = &text[token_start..cursor];
        if token.len() > 512 || token.contains("::") {
            return Vec::new();
        }
        let (typed_directory, name_prefix) = token
            .rsplit_once('/')
            .map_or(("", token), |(directory, name)| {
                (&token[..directory.len() + 1], name)
            });
        let expanded_directory = if let Some(relative) = typed_directory.strip_prefix("~/") {
            env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(relative)
        } else if Path::new(typed_directory).is_absolute() {
            PathBuf::from(typed_directory)
        } else {
            env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(typed_directory)
        };
        let Ok(entries) = std::fs::read_dir(&expanded_directory) else {
            return Vec::new();
        };
        let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
        entries.sort_by_key(|entry| (!entry.path().is_dir(), entry.file_name()));
        entries
            .into_iter()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name_prefix.is_empty()
                    && !name
                        .to_ascii_lowercase()
                        .starts_with(&name_prefix.to_ascii_lowercase())
                {
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

    pub(super) fn vsnip_completion_candidates(
        &self,
        prefix: &str,
        replace: Range<usize>,
    ) -> Vec<CompletionCandidate> {
        let language = language_bundle(self.active.document.presentation_path()).language_id;
        let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
            return Vec::new();
        };
        let paths = [
            home.join(".vsnip").join(format!("{language}.json")),
            home.join(".config/nvim/snippets")
                .join(format!("{language}.json")),
        ];
        let mut candidates = Vec::new();
        for path in paths.into_iter().filter(|path| path.exists()) {
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(snippets) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&source)
            else {
                continue;
            };
            for (name, snippet) in snippets {
                let prefixes = snippet.get("prefix").map_or_else(
                    || vec![name.as_str()],
                    |value| {
                        value.as_array().map_or_else(
                            || value.as_str().into_iter().collect(),
                            |values| {
                                values
                                    .iter()
                                    .filter_map(serde_json::Value::as_str)
                                    .collect()
                            },
                        )
                    },
                );
                let body = snippet.get("body").map_or_else(String::new, |body| {
                    body.as_array().map_or_else(
                        || body.as_str().unwrap_or_default().to_owned(),
                        |lines| {
                            lines
                                .iter()
                                .filter_map(serde_json::Value::as_str)
                                .collect::<Vec<_>>()
                                .join("\n")
                        },
                    )
                });
                if body.is_empty() {
                    continue;
                }
                let description = snippet
                    .get("description")
                    .map(render_lsp_text)
                    .unwrap_or_else(|| name.clone());
                for snippet_prefix in prefixes {
                    if !prefix.is_empty()
                        && !snippet_prefix
                            .to_ascii_lowercase()
                            .starts_with(&prefix.to_ascii_lowercase())
                    {
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
        let selected = self
            .completion_selected
            .then_some(self.completion_index.min(session.candidates.len() - 1));
        let documentation = selected
            .and_then(|index| session.candidates.get(index))
            .map_or("", |candidate| candidate.documentation.as_ref());
        Some(CompletionOverlay {
            rows: session
                .candidates
                .iter()
                .map(|candidate| CompletionOverlayRow {
                    label: candidate.label.clone(),
                    detail: candidate.detail.clone(),
                    source: candidate.source.clone(),
                })
                .collect(),
            selected,
            documentation: documentation.into(),
            documentation_scroll: self.completion_documentation_scroll,
        })
    }

    pub(super) fn completion_documentation_lines(&self) -> usize {
        self.completion
            .as_ref()
            .and_then(|session| session.candidates.get(self.completion_index))
            .map_or(0, |candidate| candidate.documentation.lines().count())
    }

    pub(super) fn move_completion(&mut self, direction: isize) {
        let Some(session) = &self.completion else {
            return;
        };
        if session.candidates.is_empty() {
            self.completion_index = 0;
        } else if direction < 0 {
            self.completion_index = self
                .completion_index
                .checked_sub(1)
                .unwrap_or(session.candidates.len() - 1);
        } else {
            self.completion_index = (self.completion_index + 1) % session.candidates.len();
        }
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
        let replace_start = candidate
            .as_ref()
            .map_or(session.replace.start, |candidate| {
                candidate
                    .replace
                    .as_ref()
                    .map_or(session.replace.start, |range| range.start)
            });
        let transaction = session.accept(self.active.editor.revision(), self.completion_index)?;
        if let Some(transaction) = transaction {
            self.active.editor.apply_transaction(transaction.clone())?;
            self.after_transaction(Some(transaction));
            if let Some(snippet) = candidate.and_then(|candidate| candidate.snippet) {
                let (_, stops) = expand_lsp_snippet_with_stops(&snippet);
                self.snippet_stops = stops
                    .into_iter()
                    .map(|range| replace_start + range.start..replace_start + range.end)
                    .collect();
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
        if direction < 0 {
            self.snippet_stop_index = self.snippet_stop_index.saturating_sub(1);
        } else if self.snippet_stop_index + 1 >= self.snippet_stops.len() {
            if let Some(range) = self.snippet_stops.last() {
                self.active.editor.set_cursor(range.end);
            }
            self.snippet_stops.clear();
            self.snippet_stop_index = 0;
            return;
        } else {
            self.snippet_stop_index += 1;
        }
        if let Some(range) = self.snippet_stops.get(self.snippet_stop_index).cloned() {
            self.active.editor.set_selection_range(range);
        }
    }
}
