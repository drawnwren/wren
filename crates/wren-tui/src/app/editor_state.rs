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
        let report = match path {
            Some(path) => self.active.document.save_as(path, &self.active.editor.contents()),
            None => self.active.document.save(&self.active.editor.contents()),
        }?;
        self.active.editor.mark_clean();
        self.active.base_hash = report.stamp.content_hash;
        save_undo_state(&mut self.active)?;
        if path.is_some() {
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
