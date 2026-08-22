use super::*;

impl App {
    pub(super) fn start_task(
        &mut self,
        affected_documents: Vec<DocumentId>,
        label: impl Into<Box<str>>,
        started: &str,
        work: impl FnOnce(CommandTaskId, &mut TaskContext) -> Result<Effects, TaskFailure> + Send + 'static,
    ) -> Result<()> {
        if self.active_task.is_some() {
            bail!("a TaskCommand is already running");
        }
        let task_id = CommandTaskId::new(self.next_task_id);
        self.next_task_id = self.next_task_id.checked_add(1).ok_or_else(|| anyhow!("task ID overflow"))?;
        self.active_task = Some(self.tasks.submit(CommandTask { task_id, affected_documents, label: label.into() }, move |context| work(task_id, context))?);
        self.set_message(format!("{started}{} running", task_id.get()))
    }

    pub(super) fn save(&mut self, path: Option<&Path>) -> Result<()> {
        if self.tasks.is_document_blocked(self.active.document_id) {
            bail!("document has a pending TaskCommand; wait or cancel before saving");
        }
        if self.format_on_save
            && !self.format_disabled.contains(&self.active.document_id)
            && path.is_none()
            && let Err(error) = self.format_active_sync(false)
        {
            self.show_error(format!("format-on-save: {error}"));
        }
        if let Some(wal) = &self.active.wal {
            wal.barrier().context("make recovery WAL durable before save")?;
        }
        let streamed = self.active.editor.text().is_mapped_piece_text();
        let mut materialized = None;
        let report = if streamed {
            match path {
                Some(path) => self.active.document.save_store_as(path, self.active.editor.text()),
                None => self.active.document.save_store(self.active.editor.text()),
            }
        } else {
            let text = self.active.editor.contents();
            materialized = Some(text);
            match path {
                Some(path) => self.active.document.save_as(path, materialized.as_deref().unwrap_or_default()),
                None => self.active.document.save(materialized.as_deref().unwrap_or_default()),
            }
        };
        let report = match report {
            Ok(report) => report,
            Err(wren_session::SaveError::ExternalChange { path: changed, reason }) if path.is_none() => {
                let ours = materialized.unwrap_or_else(|| self.active.editor.contents());
                return self.present_save_conflict(changed, reason, ours);
            }
            Err(error) => return Err(error.into()),
        };
        let text = materialized.unwrap_or_else(|| self.active.editor.contents());
        self.finish_save(report, path.is_some(), text)
    }

    fn finish_save(&mut self, report: wren_session::SaveReport, save_as: bool, text: String) -> Result<()> {
        self.active.editor.mark_clean();
        self.active.base_hash = report.stamp.content_hash;
        self.active.base_text = Arc::from(text);
        save_undo_state(&mut self.active)?;
        if save_as {
            if let Some(wal) = &self.active.wal {
                wal.clear().context("compact old recovery WAL after save-as")?;
            }
            self.active.wal = self.active.document.presentation_path().map(LocalWal::for_document).transpose()?.map(WalWorker::start).transpose()?;
        }
        if let Some(wal) = &self.active.wal {
            wal.clear().context("compact recovery WAL after save")?;
        }
        self.message = match report.warning {
            Some(SaveWarning::HardLinkReplaced { links }) => {
                format!("{} bytes written; warning: replaced one of {links} hard links", report.bytes_written)
            }
            None => format!("{} bytes written", report.bytes_written),
        };
        Ok(())
    }

    fn present_save_conflict(&mut self, path: PathBuf, reason: String, ours: String) -> Result<()> {
        let theirs = self.active.document.read_current_text()?;
        self.save_conflict =
            Some(SaveConflict { path: path.clone(), base: Arc::clone(&self.active.base_text), ours: Arc::from(ours), theirs: Arc::from(theirs) });
        self.popup = Some(TextPopup::new(
            "File changed on disk",
            format!(
                "{}\n{}\n\n1  take theirs — discard local edits\n2  take ours — overwrite disk\n3  merge — open a semantic three-way merge pane\n4  replay — take theirs and replay local edits\nEsc  keep editing without resolving",
                path.display(),
                reason,
            ),
        ));
        self.popup_deadline = None;
        self.message = "file changed on disk; choose 1 theirs, 2 ours, 3 merge, or 4 replay".to_owned();
        Ok(())
    }

    pub(super) fn handle_save_conflict_key(&mut self, key: TerminalKey) -> Result<bool> {
        let Some(conflict) = self.save_conflict.clone() else {
            return Ok(false);
        };
        if key.command_modified() {
            return Ok(true);
        }
        match key.code {
            TerminalKeyCode::Escape | TerminalKeyCode::Char('q' | 'Q') => {
                self.save_conflict = None;
                self.close_editor_popup();
                self.message = "save conflict left unresolved; local edits are still intact".to_owned();
            }
            TerminalKeyCode::Char('1') => self.take_theirs(conflict)?,
            TerminalKeyCode::Char('2') => self.take_ours(conflict)?,
            TerminalKeyCode::Char('3') => self.open_semantic_merge(conflict)?,
            TerminalKeyCode::Char('4') => self.replay_ours(conflict)?,
            _ => self.message = "save conflict: press 1 theirs, 2 ours, 3 merge, 4 replay, or Esc".to_owned(),
        }
        Ok(true)
    }

    fn take_theirs(&mut self, _conflict: SaveConflict) -> Result<()> {
        let document_id = self.active.document_id;
        let text = self.active.reload_from_disk()?;
        self.mutations.register(document_id, text, true)?;
        if let Some(wal) = &self.active.wal {
            wal.clear().context("clear recovery WAL after taking disk version")?;
        }
        self.decorations.remove(&self.active.buffer_id);
        self.semantic_decorations.remove(&self.active.buffer_id);
        self.provider_submitted.remove(&document_id);
        self.provider_refresh_due.remove(&document_id);
        self.provider_refresh_ranges.remove(&document_id);
        self.save_conflict = None;
        self.close_editor_popup();
        self.message = "took the current disk version; local edits were discarded".to_owned();
        self.prime_active_syntax();
        Ok(())
    }

    fn take_ours(&mut self, _conflict: SaveConflict) -> Result<()> {
        let text = self.active.editor.contents();
        let report = self.active.document.save_ours(&text)?;
        self.save_conflict = None;
        self.close_editor_popup();
        self.finish_save(report, false, text)
    }

    fn open_semantic_merge(&mut self, conflict: SaveConflict) -> Result<()> {
        let buffer_id = self.views.add_buffer();
        let merge_document_id = DocumentId::new(stable_hash(format!("merge:{}:{}", conflict.path.display(), buffer_id.get()).bytes()).max(2));
        // Open the merge pane first, then derive the merge from the exact
        // snapshot it tracked. If a writer changes the file during this step,
        // the next `:write` sees the normal precondition failure rather than
        // saving a merge made against a stale "theirs" value.
        let mut merge = BufferState::merge_buffer(buffer_id, merge_document_id, &conflict.path)?;
        let merged = semantic_three_way_merge(&conflict.base, &conflict.ours, &merge.base_text);
        merge.editor = BufferState::configured_editor(&merged.text, false)?;
        merge.editor.mark_dirty();
        apply_client_state(&mut merge, &self.client_state)?;
        self.mutations.register(merge_document_id, merged.text, true)?;
        self.views.split_active(SplitAxis::Vertical)?;
        let previous = std::mem::replace(&mut self.active, merge);
        self.inactive.push(previous);
        self.views.set_active_buffer(buffer_id);
        self.save_conflict = None;
        self.close_editor_popup();
        self.message = if merged.conflicts == 0 {
            "semantic merge pane opened beside the original; review it, then :write".to_owned()
        } else {
            format!("semantic merge pane opened with {} conflict block(s); resolve <<<<<<< / ======= / >>>>>>>, then :write", merged.conflicts)
        };
        self.prime_active_syntax();
        Ok(())
    }

    fn replay_ours(&mut self, conflict: SaveConflict) -> Result<()> {
        let theirs = self.refresh_conflict_theirs(&conflict)?;
        let merged = semantic_three_way_merge(&conflict.base, &conflict.ours, &theirs);
        if merged.conflicts > 0 {
            return self.open_semantic_merge(conflict);
        }
        // Reload immediately before applying the replay. A second writer is
        // allowed to race this action; if it did, retain both versions and ask
        // again rather than replaying against a stale snapshot.
        let current = self.active.reload_from_disk()?;
        if current.as_str() != theirs.as_ref() {
            // `reload_from_disk` adopted the newest disk revision, but the
            // local edits still describe a change from the original conflict
            // base. Retain that base for the retry so a second writer cannot
            // make us replay local edits against the wrong ancestor.
            self.active.base_text = Arc::clone(&conflict.base);
            return self.present_save_conflict(conflict.path, "file changed again while preparing replay".to_owned(), conflict.ours.to_string());
        }
        let document_id = self.active.document_id;
        self.mutations.register(document_id, current.clone(), true)?;
        let transaction = Transaction::new(self.active.editor.revision(), vec![Edit::new(0..current.len(), merged.text)])?;
        self.active.editor.apply_transaction(transaction.clone())?;
        self.after_transaction([transaction]);
        self.save_conflict = None;
        self.close_editor_popup();
        self.message = "took the disk version and replayed the local edits; review and :write".to_owned();
        Ok(())
    }

    fn refresh_conflict_theirs(&mut self, conflict: &SaveConflict) -> Result<Arc<str>> {
        let current: Arc<str> = Arc::from(self.active.document.read_current_text()?);
        if current != conflict.theirs {
            self.save_conflict = Some(SaveConflict { theirs: Arc::clone(&current), ..conflict.clone() });
            self.popup = Some(TextPopup::new(
                "File changed again",
                format!(
                    "{} changed again while resolving this conflict. The choices now use the newest disk version.\n\n1 take theirs\n2 take ours\n3 merge\n4 replay\nEsc cancel",
                    conflict.path.display()
                ),
            ));
            self.popup_deadline = None;
        }
        Ok(current)
    }

    pub(super) fn status_overlay(&self) -> StatusOverlay {
        let mode = match self.active.editor.mode() {
            Mode::Normal => ("NORMAL", self.theme.color(CatppuccinColor::Blue)),
            Mode::Insert => ("INSERT", self.theme.color(CatppuccinColor::Green)),
            Mode::Replace => ("REPLACE", self.theme.color(CatppuccinColor::Red)),
            Mode::Visual => ("VISUAL", self.theme.color(CatppuccinColor::Mauve)),
            Mode::VisualLine => ("V-LINE", self.theme.color(CatppuccinColor::Mauve)),
        };
        let styles = [
            CellStyle::rgb(self.theme.color(CatppuccinColor::Base), mode.1).with_bold(),
            CellStyle::rgb(self.theme.color(CatppuccinColor::Text), self.theme.color(CatppuccinColor::Surface1)).with_bold(),
            CellStyle::rgb(self.theme.color(CatppuccinColor::Text), self.theme.color(CatppuccinColor::Mantle)),
        ];
        let [section_a, section_b, section_c] = styles;
        let path = self.active.name();
        let mut left = vec![StatusSegment { text: format!(" {} ", mode.0).into(), style: section_a }];
        if let Some(branch) = &self.active.git_branch {
            left.push(StatusSegment { text: format!("  {branch} ").into(), style: section_b });
        }
        let diagnostic_count =
            self.active.document.presentation_path().map_or(0, |path| self.diagnostics.iter().filter(|diagnostic| same_path(&diagnostic.path, path)).count());
        if diagnostic_count > 0 {
            left.push(StatusSegment {
                text: format!("  {diagnostic_count} ").into(),
                style: section_b.with_foreground(CellColor::Rgb(self.theme.color(CatppuccinColor::Yellow))),
            });
        }
        let flags: String = [
            self.active.editor.is_dirty().then_some(" [+]"),
            self.active.editor.is_read_only().then_some(" [RO]"),
            self.active.mixed_line_endings.then_some(" [mixed EOL]"),
        ]
        .into_iter()
        .flatten()
        .collect();
        left.push(StatusSegment { text: format!(" {path}{flags} ").into(), style: section_c });
        if !self.message.is_empty() {
            left.push(StatusSegment {
                text: format!(" {} ", self.message).into(),
                style: section_c.with_foreground(CellColor::Rgb(self.theme.color(CatppuccinColor::Subtext0))),
            });
        }
        let (line, column) = self.active.editor.cursor_line_column();
        let text = self.active.editor.text();
        let line_count = text.line_of_byte(text.len_bytes()).saturating_add(1);
        let progress = (line + 1).saturating_mul(100).checked_div(line_count).unwrap_or(100);
        let language = language_bundle(self.active.document.presentation_path()).language_id;
        let right = vec![
            StatusSegment { text: format!(" utf-8  unix  {language} ").into(), style: section_c },
            StatusSegment { text: format!(" {progress}% ").into(), style: section_b },
            StatusSegment { text: format!(" {}:{} ", line + 1, column + 1).into(), style: section_a },
        ];
        StatusOverlay { left, right }
    }

    pub(super) fn expression_context(&self) -> ExpressionContext {
        let (line, column) = self.active.editor.cursor_line_column();
        let class = self.active.class.name();
        ExpressionContext::new()
            .with("cursor.line", (line + 1) as f64)
            .with("cursor.column", (column + 1) as f64)
            .with("selection.nonempty", false)
            .with("document.class", class)
            .with("remote", false)
            .with("workspace.trusted", false)
            .with("os", "macos")
    }

    pub(super) fn flush_wal(&self) -> Result<()> {
        if let Some(wal) = &self.active.wal {
            wal.barrier().context("flush recovery WAL")?;
        }
        for buffer in &self.inactive {
            if let Some(wal) = &buffer.wal {
                wal.barrier().context("flush recovery WAL")?;
            }
        }
        self.mutations.barrier()?;
        self.client_state_worker.flush_state(self.client_state.clone())?;
        save_recent_files(&self.recent_files)?;
        Ok(())
    }

    pub(super) fn resolve_substitute(&self, pattern: &str, replacement: &str, flags: SubstituteFlags, ranges: Vec<Range<usize>>) -> Result<Substitute> {
        let persist_pattern = !pattern.is_empty();
        let needle = self.effective_search_pattern(pattern)?;
        if self.last_substitute.is_none() && has_unescaped_tilde(replacement) {
            bail!("no previous substitute replacement for ~");
        }
        let replacement = resolve_previous_replacement(replacement, self.last_substitute.as_ref().map(|substitute| substitute.replacement.as_str()));
        Ok(Substitute { needle, replacement, ranges, flags, persist_pattern })
    }

    pub(super) fn resolve_repeated_substitute(
        &self,
        use_search_pattern: bool,
        flags: Option<SubstituteFlags>,
        ranges: Vec<Range<usize>>,
    ) -> Result<Substitute> {
        let previous = self.last_substitute.as_ref().ok_or_else(|| anyhow!("no previous substitute command"))?;
        let flags = flags.unwrap_or(previous.flags);
        let needle = if use_search_pattern { self.effective_search_pattern("")? } else { previous.needle.clone() };
        Ok(Substitute { needle, replacement: previous.replacement.clone(), ranges, flags, persist_pattern: use_search_pattern })
    }

    pub(super) fn start_substitution(&mut self, substitute: Substitute) -> Result<()> {
        if self.active_task.is_some() {
            bail!("a TaskCommand is already running");
        }
        let pattern = self
            .active
            .editor
            .compile_search_pattern(&substitute.needle, substitute_case_override(substitute.flags))
            .with_context(|| format!("invalid substitution pattern {:?}", substitute.needle))?;
        self.synchronize_search(&substitute.needle, self.last_search_direction, substitute.persist_pattern)?;
        self.last_substitute = Some(LastSubstitute { needle: substitute.needle.clone(), replacement: substitute.replacement.clone(), flags: substitute.flags });
        if substitute.flags.confirm {
            return self.begin_substitution_confirmation(substitute, pattern);
        }
        self.start_substitution_task(substitute, pattern)
    }

    pub(super) fn begin_substitution_confirmation(&mut self, substitute: Substitute, pattern: VimPattern) -> Result<()> {
        let text = self.active.editor.contents();
        let replacement = VimReplacement::new(substitute.replacement);
        let candidates = plan_substitution_edits(&text, &pattern, &replacement, &substitute.ranges, substitute.flags.global, || Ok(()))
            .map_err(|error| anyhow!(error.to_string()))?;
        self.substitute_confirmation = Some(SubstituteConfirmation {
            base_revision: self.active.editor.revision(),
            original_text: text,
            candidates,
            accepted: Vec::new(),
            index: 0,
            print: substitute.flags.print,
        });
        self.advance_substitution_confirmation()
    }

    pub(super) fn advance_substitution_confirmation(&mut self) -> Result<()> {
        let Some(confirmation) = self.substitute_confirmation.as_ref() else {
            return Ok(());
        };
        if confirmation.index >= confirmation.candidates.len() {
            return self.finish_substitution_confirmation();
        }
        let candidate = &confirmation.candidates[confirmation.index];
        self.active.editor.set_cursor(candidate.range.start);
        self.set_message(format!(
            "replace with {:?}? (y/n/a/q/l) [{}/{}]",
            compact(&candidate.insert, 40),
            confirmation.index + 1,
            confirmation.candidates.len()
        ))
    }

    pub(super) fn handle_substitution_confirmation(&mut self, key: TerminalKey) -> Result<()> {
        let Some(mut confirmation) = self.substitute_confirmation.take() else {
            return Ok(());
        };
        let finish = match key.code {
            TerminalKeyCode::Char('y' | 'Y') => {
                confirmation.accepted.push(confirmation.candidates[confirmation.index].clone());
                confirmation.index += 1;
                false
            }
            TerminalKeyCode::Char('n' | 'N') => {
                confirmation.index += 1;
                false
            }
            TerminalKeyCode::Char('a' | 'A') => {
                confirmation.accepted.extend_from_slice(&confirmation.candidates[confirmation.index..]);
                confirmation.index = confirmation.candidates.len();
                true
            }
            TerminalKeyCode::Char('l' | 'L') => {
                confirmation.accepted.push(confirmation.candidates[confirmation.index].clone());
                confirmation.index = confirmation.candidates.len();
                true
            }
            TerminalKeyCode::Char('q' | 'Q') | TerminalKeyCode::Escape => true,
            _ => {
                self.message = "substitute confirmation: y=yes n=no a=all q=quit l=last".to_owned();
                self.substitute_confirmation = Some(confirmation);
                return Ok(());
            }
        };
        self.substitute_confirmation = Some(confirmation);
        if finish { self.finish_substitution_confirmation() } else { self.advance_substitution_confirmation() }
    }

    pub(super) fn finish_substitution_confirmation(&mut self) -> Result<()> {
        let Some(confirmation) = self.substitute_confirmation.take() else {
            return Ok(());
        };
        let count = confirmation.accepted.len();
        if count == 0 {
            return self.set_message("0 substitutions".to_owned());
        }
        let transaction = Transaction::new(confirmation.base_revision, confirmation.accepted)?;
        let message = substitution_message(count, confirmation.print, &confirmation.original_text, &transaction);
        self.active.editor.apply_transaction(transaction.clone())?;
        self.after_transaction(Some(transaction));
        self.set_message(message)
    }

    pub(super) fn start_substitution_task(&mut self, substitute: Substitute, pattern: VimPattern) -> Result<()> {
        let text = self.active.editor.contents();
        let base_revision = self.active.editor.revision();
        let replacement = VimReplacement::new(substitute.replacement);
        let ranges = substitute.ranges;
        let global = substitute.flags.global;
        let print = substitute.flags.print;
        let document_id = self.active.document_id;
        self.start_task(vec![document_id], "range substitution", "task ", move |_, context| {
            let edits = plan_substitution_edits(&text, &pattern, &replacement, &ranges, global, || context.checkpoint())?;
            context.checkpoint()?;
            let count = edits.len();
            let mut effects = Effects { messages: Vec::new(), ..Effects::default() };
            if count > 0 {
                let transaction = Transaction::new(base_revision, edits).map_err(|error| TaskFailure::Failed(error.to_string().into()))?;
                effects.messages.push(substitution_message(count, print, &text, &transaction).into_boxed_str());
                effects.edit_proposals.push(EditProposal { document_id, base_revision, transactions: vec![transaction], label: "regex substitution".into() });
            } else {
                effects.messages.push("0 substitutions".into());
            }
            Ok(effects)
        })
    }

    pub(super) fn poll_task_results(&mut self) -> Result<bool> {
        let mut changed = false;
        while let Some(result) = self.tasks.try_result() {
            changed = true;
            self.active_task = None;
            self.apply_editor_task_result(result.task.task_id, result.outcome)?;
        }
        Ok(changed)
    }

    fn apply_editor_task_result(&mut self, task_id: CommandTaskId, outcome: Result<Effects, TaskFailure>) -> Result<()> {
        match outcome {
            Ok(effects) => self.apply_editor_task_effects(task_id, effects),
            Err(TaskFailure::Cancelled) => self.set_message("task cancelled".to_owned()),
            Err(error) => {
                self.show_error(error);
                Ok(())
            }
        }
    }

    fn apply_editor_task_effects(&mut self, task_id: CommandTaskId, effects: Effects) -> Result<()> {
        let active_document = self.active.document_id;
        for proposal in effects.edit_proposals.into_iter().filter(|proposal| proposal.document_id == active_document) {
            if proposal.base_revision != self.active.editor.revision() {
                self.message = format!("task {} is stale at revision {}", task_id.get(), self.active.editor.revision().get());
                continue;
            }
            for transaction in proposal.transactions {
                self.active.editor.apply_transaction(transaction.clone())?;
                self.after_transaction(Some(transaction));
            }
        }
        if let Some(message) = effects.messages.last() {
            self.message = message.to_string();
        }
        Ok(())
    }
}
