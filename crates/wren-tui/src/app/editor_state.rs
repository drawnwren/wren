use super::*;

impl App {
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
            wal.barrier()
                .context("make recovery WAL durable before save")?;
        }
        let report = match path {
            Some(path) => self
                .active
                .document
                .save_as(path, &self.active.editor.contents()),
            None => self.active.document.save(&self.active.editor.contents()),
        }?;
        self.active.editor.mark_clean();
        self.active.base_hash = report.stamp.content_hash;
        save_undo_state(&mut self.active)?;
        if path.is_some() {
            if let Some(wal) = &self.active.wal {
                wal.clear()
                    .context("compact old recovery WAL after save-as")?;
            }
            self.active.wal = self
                .active
                .document
                .presentation_path()
                .map(LocalWal::for_document)
                .transpose()?
                .map(WalWorker::start);
        }
        if let Some(wal) = &self.active.wal {
            wal.clear().context("compact recovery WAL after save")?;
        }
        self.message = match report.warning {
            Some(SaveWarning::HardLinkReplaced { links }) => format!(
                "{} bytes written; warning: replaced one of {links} hard links",
                report.bytes_written
            ),
            None => format!("{} bytes written", report.bytes_written),
        };
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn status(&self) -> String {
        let mode = match self.active.editor.mode() {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Replace => "REPLACE",
            Mode::Visual => "VISUAL",
            Mode::VisualLine => "V-LINE",
        };
        let path = self.active.name();
        let changed = if self.active.editor.is_dirty() {
            " [+]"
        } else {
            ""
        };
        let readonly = if self.active.editor.is_read_only() {
            " [RO]"
        } else {
            ""
        };
        let eol = if self.active.mixed_line_endings {
            " [mixed EOL]"
        } else {
            ""
        };
        let (line, column) = self.active.editor.cursor_line_column();
        let class = match self.active.class {
            DocumentClass::Normal => "",
            DocumentClass::Large => " [large]",
            DocumentClass::Pathological => " [pathological]",
        };
        let detail = if self.message.is_empty() {
            String::new()
        } else {
            format!(" | {}", self.message)
        };
        let tab = self
            .views
            .tabs
            .iter()
            .position(|tab| tab.id == self.views.active_tab)
            .map_or(1, |index| index + 1);
        format!(
            " {mode} | {path}{changed}{readonly}{eol}{class} | {}:{} | b{} t{}/{}{detail}",
            line + 1,
            column + 1,
            self.active.buffer_id.get(),
            tab,
            self.views.tabs.len(),
        )
    }

    pub(super) fn status_overlay(&self) -> StatusOverlay {
        let (mode, styles) = self.status_mode_and_styles();
        StatusOverlay {
            left: self.left_status_segments(mode, styles),
            right: self.right_status_segments(styles),
        }
    }

    fn status_mode_and_styles(&self) -> ((&'static str, RgbColor), [CellStyle; 3]) {
        let mode = match self.active.editor.mode() {
            Mode::Normal => ("NORMAL", self.theme.blue),
            Mode::Insert => ("INSERT", self.theme.green),
            Mode::Replace => ("REPLACE", self.theme.red),
            Mode::Visual => ("VISUAL", self.theme.mauve),
            Mode::VisualLine => ("V-LINE", self.theme.mauve),
        };
        let styles = [
            CellStyle {
                bold: true,
                foreground: Some(CellColor::Rgb(self.theme.base)),
                background: Some(CellColor::Rgb(mode.1)),
                ..CellStyle::default()
            },
            CellStyle {
                bold: true,
                foreground: Some(CellColor::Rgb(self.theme.text)),
                background: Some(CellColor::Rgb(self.theme.surface1)),
                ..CellStyle::default()
            },
            CellStyle {
                foreground: Some(CellColor::Rgb(self.theme.text)),
                background: Some(CellColor::Rgb(self.theme.mantle)),
                ..CellStyle::default()
            },
        ];
        (mode, styles)
    }

    fn left_status_segments(
        &self,
        mode: (&str, RgbColor),
        [section_a, section_b, section_c]: [CellStyle; 3],
    ) -> Vec<StatusSegment> {
        let path = self.active.name();
        let mut left = vec![StatusSegment {
            text: format!(" {} ", mode.0).into(),
            style: section_a,
        }];
        if let Some(branch) = &self.active.git_branch {
            left.push(StatusSegment {
                text: format!("  {branch} ").into(),
                style: section_b,
            });
        }
        let diagnostic_count = self.active.document.presentation_path().map_or(0, |path| {
            self.diagnostics
                .iter()
                .filter(|diagnostic| same_path(&diagnostic.path, path))
                .count()
        });
        if diagnostic_count > 0 {
            left.push(StatusSegment {
                text: format!("  {diagnostic_count} ").into(),
                style: CellStyle {
                    foreground: Some(CellColor::Rgb(self.theme.yellow)),
                    ..section_b
                },
            });
        }
        left.push(StatusSegment {
            text: format!(" {path}{} ", self.active_status_flags()).into(),
            style: section_c,
        });
        if !self.message.is_empty() {
            left.push(StatusSegment {
                text: format!(" {} ", self.message).into(),
                style: CellStyle {
                    foreground: Some(CellColor::Rgb(self.theme.subtext0)),
                    ..section_c
                },
            });
        }
        left
    }

    fn right_status_segments(
        &self,
        [section_a, section_b, section_c]: [CellStyle; 3],
    ) -> Vec<StatusSegment> {
        let (line, column) = self.active.editor.cursor_line_column();
        let text = self.active.editor.text();
        let line_count = text.line_of_byte(text.len_bytes()).saturating_add(1);
        let progress = (line + 1)
            .saturating_mul(100)
            .checked_div(line_count)
            .unwrap_or(100);
        let language = language_bundle(self.active.document.presentation_path()).language_id;
        vec![
            StatusSegment {
                text: format!(" utf-8  unix  {language} ").into(),
                style: section_c,
            },
            StatusSegment {
                text: format!(" {progress}% ").into(),
                style: section_b,
            },
            StatusSegment {
                text: format!(" {}:{} ", line + 1, column + 1).into(),
                style: section_a,
            },
        ]
    }

    fn active_status_flags(&self) -> String {
        [
            self.active.editor.is_dirty().then_some(" [+]"),
            self.active.editor.is_read_only().then_some(" [RO]"),
            self.active.mixed_line_endings.then_some(" [mixed EOL]"),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    pub(super) fn expression_context(&self) -> ExpressionContext {
        let (line, column) = self.active.editor.cursor_line_column();
        let class = match self.active.class {
            DocumentClass::Normal => "normal",
            DocumentClass::Large => "large",
            DocumentClass::Pathological => "pathological",
        };
        ExpressionContext::new()
            .with("cursor.line", Value::Number((line + 1) as f64))
            .with("cursor.column", Value::Number((column + 1) as f64))
            .with("selection.nonempty", Value::Bool(false))
            .with("document.class", Value::String(class.to_owned()))
            .with("remote", Value::Bool(false))
            .with("workspace.trusted", Value::Bool(false))
            .with("os", Value::String(env::consts::OS.to_owned()))
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
        self.client_state_worker
            .barrier(self.client_state.clone())?;
        save_recent_files(&self.recent_files)?;
        Ok(())
    }

    pub(super) fn resolve_substitute(
        &self,
        pattern: &str,
        replacement: &str,
        flags: SubstituteFlags,
        ranges: Vec<Range<usize>>,
    ) -> Result<Substitute> {
        let persist_pattern = !pattern.is_empty();
        let needle = self.effective_search_pattern(pattern)?;
        if self.last_substitute.is_none() && has_unescaped_tilde(replacement) {
            bail!("no previous substitute replacement for ~");
        }
        let replacement = resolve_previous_replacement(
            replacement,
            self.last_substitute
                .as_ref()
                .map(|substitute| substitute.replacement.as_str()),
        );
        Ok(Substitute {
            needle,
            replacement,
            ranges,
            global: flags.global,
            case_override: substitute_case_override(flags),
            confirm: flags.confirm,
            print: flags.print,
            persist_pattern,
        })
    }

    pub(super) fn resolve_repeated_substitute(
        &self,
        use_search_pattern: bool,
        flags: Option<SubstituteFlags>,
        ranges: Vec<Range<usize>>,
    ) -> Result<Substitute> {
        let previous = self
            .last_substitute
            .as_ref()
            .ok_or_else(|| anyhow!("no previous substitute command"))?;
        let flags = flags.unwrap_or(previous.flags);
        let needle = if use_search_pattern {
            self.effective_search_pattern("")?
        } else {
            previous.needle.clone()
        };
        Ok(Substitute {
            needle,
            replacement: previous.replacement.clone(),
            ranges,
            global: flags.global,
            case_override: substitute_case_override(flags),
            confirm: flags.confirm,
            print: flags.print,
            persist_pattern: use_search_pattern,
        })
    }

    pub(super) fn start_substitution(&mut self, substitute: Substitute) -> Result<()> {
        if self.active_task.is_some() {
            bail!("a TaskCommand is already running");
        }
        let pattern = self
            .active
            .editor
            .compile_search_pattern(&substitute.needle, substitute.case_override)
            .with_context(|| format!("invalid substitution pattern {:?}", substitute.needle))?;
        self.synchronize_search(
            &substitute.needle,
            self.last_search_direction,
            substitute.persist_pattern,
        )?;
        self.last_substitute = Some(LastSubstitute {
            needle: substitute.needle.clone(),
            replacement: substitute.replacement.clone(),
            flags: SubstituteFlags {
                global: substitute.global,
                confirm: substitute.confirm,
                case_sensitive: match substitute.case_override {
                    CaseOverride::Default => None,
                    CaseOverride::Ignore => Some(false),
                    CaseOverride::Sensitive => Some(true),
                },
                print: substitute.print,
            },
        });
        if substitute.confirm {
            return self.begin_substitution_confirmation(substitute, pattern);
        }
        self.start_substitution_task(substitute, pattern)
    }

    pub(super) fn begin_substitution_confirmation(
        &mut self,
        substitute: Substitute,
        pattern: VimPattern,
    ) -> Result<()> {
        let text = self.active.editor.contents();
        let replacement = VimReplacement::new(substitute.replacement);
        let candidates = plan_substitution_edits(
            &text,
            &pattern,
            &replacement,
            &substitute.ranges,
            substitute.global,
            || Ok(()),
        )
        .map_err(|error| anyhow!(error.to_string()))?;
        self.substitute_confirmation = Some(SubstituteConfirmation {
            base_revision: self.active.editor.revision(),
            original_text: text,
            candidates,
            accepted: Vec::new(),
            index: 0,
            print: substitute.print,
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
        self.message = format!(
            "replace with {:?}? (y/n/a/q/l) [{}/{}]",
            compact(&candidate.insert, 40),
            confirmation.index + 1,
            confirmation.candidates.len()
        );
        Ok(())
    }

    pub(super) fn handle_substitution_confirmation(&mut self, key: TerminalKey) -> Result<()> {
        let Some(mut confirmation) = self.substitute_confirmation.take() else {
            return Ok(());
        };
        let finish = match key.code {
            TerminalKeyCode::Char('y' | 'Y') => {
                confirmation
                    .accepted
                    .push(confirmation.candidates[confirmation.index].clone());
                confirmation.index += 1;
                false
            }
            TerminalKeyCode::Char('n' | 'N') => {
                confirmation.index += 1;
                false
            }
            TerminalKeyCode::Char('a' | 'A') => {
                confirmation
                    .accepted
                    .extend_from_slice(&confirmation.candidates[confirmation.index..]);
                confirmation.index = confirmation.candidates.len();
                true
            }
            TerminalKeyCode::Char('l' | 'L') => {
                confirmation
                    .accepted
                    .push(confirmation.candidates[confirmation.index].clone());
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
        if finish {
            self.finish_substitution_confirmation()
        } else {
            self.advance_substitution_confirmation()
        }
    }

    pub(super) fn finish_substitution_confirmation(&mut self) -> Result<()> {
        let Some(confirmation) = self.substitute_confirmation.take() else {
            return Ok(());
        };
        let count = confirmation.accepted.len();
        if count == 0 {
            self.message = "0 substitutions".to_owned();
            return Ok(());
        }
        let transaction = Transaction::new(confirmation.base_revision, confirmation.accepted)?;
        let message = substitution_message(
            count,
            confirmation.print,
            &confirmation.original_text,
            &transaction,
        );
        self.active.editor.apply_transaction(transaction.clone())?;
        self.after_transaction(Some(transaction));
        self.message = message;
        Ok(())
    }

    pub(super) fn start_substitution_task(
        &mut self,
        substitute: Substitute,
        pattern: VimPattern,
    ) -> Result<()> {
        let task_id = CommandTaskId::new(self.next_task_id);
        self.next_task_id = self
            .next_task_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("task ID overflow"))?;
        let text = self.active.editor.contents();
        let base_revision = self.active.editor.revision();
        let replacement = VimReplacement::new(substitute.replacement);
        let ranges = substitute.ranges;
        let global = substitute.global;
        let print = substitute.print;
        let document_id = self.active.document_id;
        let cancellation = self.tasks.submit(
            CommandTask {
                task_id,
                affected_documents: vec![document_id],
                label: "range substitution".into(),
            },
            move |context| {
                let edits = plan_substitution_edits(
                    &text,
                    &pattern,
                    &replacement,
                    &ranges,
                    global,
                    || context.checkpoint(),
                )?;
                context.checkpoint()?;
                let count = edits.len();
                let mut effects = Effects {
                    messages: Vec::new(),
                    ..Effects::default()
                };
                if count > 0 {
                    let transaction = Transaction::new(base_revision, edits)
                        .map_err(|error| TaskFailure::Failed(error.to_string().into()))?;
                    effects.messages.push(
                        substitution_message(count, print, &text, &transaction).into_boxed_str(),
                    );
                    effects.edit_proposals.push(EditProposal {
                        document_id,
                        base_revision,
                        transactions: vec![transaction],
                        label: "regex substitution".into(),
                    });
                } else {
                    effects.messages.push("0 substitutions".into());
                }
                Ok(effects)
            },
        )?;
        self.active_task = Some(cancellation);
        self.message = format!("task {} running", task_id.get());
        Ok(())
    }

    pub(super) fn poll_task_results(&mut self) -> Result<bool> {
        let mut changed = false;
        while let Some(result) = self.tasks.try_result()? {
            changed = true;
            self.apply_task_result(result)?;
        }
        Ok(changed)
    }

    fn apply_task_result(&mut self, result: TaskResult) -> Result<()> {
        self.active_task = None;
        self.apply_editor_task_result(result.task.task_id, result.outcome)
    }

    fn apply_editor_task_result(
        &mut self,
        task_id: CommandTaskId,
        outcome: Result<Effects, TaskFailure>,
    ) -> Result<()> {
        match outcome {
            Ok(effects) => self.apply_editor_task_effects(task_id, effects),
            Err(TaskFailure::Cancelled) => {
                self.message = "task cancelled".to_owned();
                Ok(())
            }
            Err(error) => {
                self.show_error(error);
                Ok(())
            }
        }
    }

    fn apply_editor_task_effects(
        &mut self,
        task_id: CommandTaskId,
        effects: Effects,
    ) -> Result<()> {
        let active_document = self.active.document_id;
        for proposal in effects
            .edit_proposals
            .into_iter()
            .filter(|proposal| proposal.document_id == active_document)
        {
            if proposal.base_revision != self.active.editor.revision() {
                self.message = format!(
                    "task {} is stale at revision {}",
                    task_id.get(),
                    self.active.editor.revision().get()
                );
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
