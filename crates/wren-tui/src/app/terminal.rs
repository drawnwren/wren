use super::*;

impl App {
    pub(super) fn open_terminal(
        &mut self,
        program: Option<&str>,
        arguments: &[Box<str>],
    ) -> Result<()> {
        let directory = env::current_dir().context("locate terminal working directory")?;
        self.open_terminal_in(program, arguments, &directory)
    }

    pub(super) fn open_terminal_in(
        &mut self,
        program: Option<&str>,
        arguments: &[Box<str>],
        directory: &Path,
    ) -> Result<()> {
        if self
            .terminal
            .as_ref()
            .is_some_and(|terminal| terminal.exit_code().is_none())
        {
            self.terminal_focused = true;
            self.message.clear();
            return Ok(());
        }
        let program = program
            .map(str::to_owned)
            .or_else(|| env::var("SHELL").ok())
            .unwrap_or_else(|| "sh".to_owned());
        let arguments = arguments
            .iter()
            .map(AsRef::<str>::as_ref)
            .collect::<Vec<_>>();
        let rows = u16::try_from(self.viewport_rows.saturating_sub(1))
            .unwrap_or(u16::MAX)
            .max(1);
        let columns = u16::try_from(self.viewport_columns)
            .unwrap_or(u16::MAX)
            .max(1);
        self.terminal = Some(PtySession::spawn_in(
            &program, &arguments, rows, columns, directory,
        )?);
        self.terminal_focused = true;
        self.terminal_escape_pending = false;
        self.message = format!("terminal: {program}");
        Ok(())
    }

    pub(super) fn handle_terminal_input(&mut self, input: TerminalInput) -> Result<()> {
        match input {
            TerminalInput::Resized { columns, rows } => self.resize_terminal(rows, columns),
            TerminalInput::Paste(text) => {
                if let Some(terminal) = &mut self.terminal {
                    terminal.send_input(text.as_bytes())?;
                }
            }
            TerminalInput::Key(key) => {
                if self.terminal_escape_pending {
                    self.terminal_escape_pending = false;
                    if key.control && matches!(key.code, TerminalKeyCode::Char('n' | 'N')) {
                        self.terminal_focused = false;
                        self.message = "terminal hidden; :terminal returns".to_owned();
                        return Ok(());
                    }
                    if let Some(terminal) = &mut self.terminal {
                        terminal.send_input(&[0x1c])?;
                    }
                }
                if key.control && key.code == TerminalKeyCode::Char('\\') {
                    self.terminal_escape_pending = true;
                    return Ok(());
                }
                if let Some(bytes) = terminal_key_bytes(key)
                    && let Some(terminal) = &mut self.terminal
                {
                    terminal.send_input(&bytes)?;
                }
            }
            event @ (TerminalInput::MouseScroll { .. }
            | TerminalInput::MouseClick { .. }
            | TerminalInput::MouseDrag { .. }
            | TerminalInput::MouseRelease { .. }) => {
                if let Some(terminal) = &mut self.terminal
                    && terminal.surface().accepts_sgr_mouse()
                {
                    terminal.send_input(&terminal_mouse_bytes(&event))?;
                }
            }
            TerminalInput::Ignored => {}
        }
        Ok(())
    }

    pub(super) fn resize_terminal(&mut self, rows: usize, columns: usize) {
        self.viewport_rows = rows.max(1);
        self.viewport_columns = columns.max(1);
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        let rows = u16::try_from(rows.saturating_sub(1))
            .unwrap_or(u16::MAX)
            .max(1);
        let columns = u16::try_from(columns).unwrap_or(u16::MAX).max(1);
        if let Err(error) = terminal.resize(rows, columns) {
            self.show_error(format!("terminal resize: {error}"));
        }
    }

    pub(super) fn take_normal_count(&mut self) -> Option<usize> {
        let count = match self.active.editor.pending_parse_state() {
            Some(ParseState::Count { value }) => usize::try_from(value.get()).ok(),
            _ => None,
        };
        if count.is_some() {
            self.active.editor.cancel_pending();
        }
        count
    }

    pub(super) fn apply_z_count(&mut self) {
        let Some(count) = self.take_normal_count() else {
            return;
        };
        let line = count.saturating_sub(1);
        let start = self.active.editor.text().byte_of_line(line);
        self.active.editor.set_cursor(start);
        self.dispatch_key(KeyEvent::character('^'));
    }

    pub(super) fn navigate_jump_count(&mut self, backward: bool, count: usize) -> Result<bool> {
        let mut moved = false;
        for _ in 0..count {
            if self.navigate_global_jump(backward)? {
                moved = true;
                continue;
            }
            if !self.active.editor.navigate_jump(backward) {
                break;
            }
            moved = true;
        }
        Ok(moved)
    }

    pub(super) fn navigate_change_count(&mut self, backward: bool, count: usize) -> bool {
        let mut moved = false;
        for _ in 0..count {
            if !self.active.editor.navigate_change(backward) {
                break;
            }
            moved = true;
        }
        moved
    }

    pub(super) fn handle_window_prefix(&mut self, key: TerminalKey) -> Result<()> {
        let count = self.take_normal_count().unwrap_or(1);
        let direction = match key.code {
            TerminalKeyCode::Char('h' | 'H') | TerminalKeyCode::Left => Some(WindowDirection::Left),
            TerminalKeyCode::Char('j' | 'J') | TerminalKeyCode::Down => Some(WindowDirection::Down),
            TerminalKeyCode::Char('k' | 'K') | TerminalKeyCode::Up => Some(WindowDirection::Up),
            TerminalKeyCode::Char('l' | 'L') | TerminalKeyCode::Right => {
                Some(WindowDirection::Right)
            }
            _ => None,
        };
        if let Some(direction) = direction {
            self.views.focus_window(direction)?;
            self.activate_view_buffer()?;
            self.message.clear();
            return Ok(());
        }
        match key.code {
            TerminalKeyCode::Char('s' | 'S') => {
                self.views.split_active(SplitAxis::Horizontal)?;
                self.message.clear();
            }
            TerminalKeyCode::Char('v' | 'V') => {
                self.views.split_active(SplitAxis::Vertical)?;
                self.message.clear();
            }
            TerminalKeyCode::Char('c' | 'C' | 'q' | 'Q') => {
                if let Err(error) = self.views.close_active_window() {
                    self.show_error(error);
                } else {
                    self.activate_view_buffer()?;
                    self.message.clear();
                }
            }
            TerminalKeyCode::Char('o' | 'O') => {
                self.views.only_active_window()?;
                self.message.clear();
            }
            TerminalKeyCode::Char('w' | 'W') => {
                self.views.cycle_window(
                    if key.shift { -1 } else { 1_isize }
                        .saturating_mul(isize::try_from(count).unwrap_or(isize::MAX)),
                )?;
                self.activate_view_buffer()?;
                self.message.clear();
            }
            TerminalKeyCode::Char('=') => {
                self.views.equalize_windows()?;
                self.message.clear();
            }
            TerminalKeyCode::Escape => self.message.clear(),
            _ => self.message = "unknown Ctrl-W command".to_owned(),
        }
        Ok(())
    }

    pub(super) fn scroll_page(&mut self, direction: isize, full_page: bool, count: Option<usize>) {
        let content_rows = self.viewport_rows.saturating_sub(1).max(1);
        let amount = if full_page {
            content_rows
                .saturating_sub(2)
                .max(1)
                .saturating_mul(count.unwrap_or(1))
        } else if let Some(count) = count {
            count
        } else {
            content_rows.checked_div(2).unwrap_or(1).max(1)
        };
        let text_store = self.active.editor.text();
        let line_count = text_store
            .line_of_byte(text_store.len_bytes())
            .saturating_add(1);
        let last_line = line_count.saturating_sub(1);
        let current_line = self.active.editor.cursor_line_column().0;
        let current_top = self.views.active_window().top_line;
        let (next_line, next_top) = if direction < 0 {
            (
                current_line.saturating_sub(amount),
                current_top.saturating_sub(amount),
            )
        } else {
            (
                current_line.saturating_add(amount).min(last_line),
                current_top
                    .saturating_add(amount)
                    .min(last_line.saturating_sub(content_rows.saturating_sub(1))),
            )
        };
        let column = self.active.editor.cursor_line_column().1;
        let start = self.active.editor.text().byte_of_line(next_line);
        let end = self
            .active
            .editor
            .text()
            .byte_of_line(next_line + 1)
            .saturating_sub(usize::from(next_line < last_line));
        let line = self.active.editor.text().slice(start..end);
        let relative = line
            .char_indices()
            .nth(column)
            .map_or(end.saturating_sub(start), |(byte, _)| byte);
        self.active
            .editor
            .set_cursor(start.saturating_add(relative));
        self.views.active_window_mut().top_line = next_top;
        self.message.clear();
    }

    pub(super) fn scroll_view_line(&mut self, direction: isize, count: usize) {
        let content_rows = self.viewport_rows.saturating_sub(1).max(1);
        let text_store = self.active.editor.text();
        let last_line = text_store.line_of_byte(text_store.len_bytes());
        let max_top = last_line.saturating_sub(content_rows.saturating_sub(1));
        let current_top = self.views.active_window().top_line;
        let next_top = if direction < 0 {
            current_top.saturating_sub(count)
        } else {
            current_top.saturating_add(count).min(max_top)
        };
        self.views.active_window_mut().top_line = next_top;
        let cursor_line = self.active.editor.cursor_line_column().0;
        if cursor_line < next_top {
            self.set_cursor_line(next_top);
        } else if cursor_line >= next_top.saturating_add(content_rows) {
            self.set_cursor_line(next_top.saturating_add(content_rows - 1).min(last_line));
        }
        self.message.clear();
    }

    pub(super) fn move_cursor_to_view(&mut self, position: ViewPosition, count: usize) {
        let top = self.views.active_window().top_line;
        let content_rows = self.viewport_rows.saturating_sub(1).max(1);
        let text_store = self.active.editor.text();
        let last_line = text_store.line_of_byte(text_store.len_bytes());
        let offset = match position {
            ViewPosition::Top => count.saturating_sub(1),
            ViewPosition::Middle => content_rows / 2,
            ViewPosition::Bottom => content_rows.saturating_sub(count),
        };
        self.set_cursor_line(top.saturating_add(offset).min(last_line));
        self.message.clear();
    }

    pub(super) fn center_cursor_line(&mut self, position: ViewPosition) {
        let line = self.active.editor.cursor_line_column().0;
        let content_rows = self.viewport_rows.saturating_sub(1).max(1);
        let top = match position {
            ViewPosition::Top => line,
            ViewPosition::Middle => line.saturating_sub(content_rows / 2),
            ViewPosition::Bottom => line.saturating_sub(content_rows.saturating_sub(1)),
        };
        self.views.active_window_mut().top_line = top;
        self.message.clear();
    }

    pub(super) fn set_cursor_line(&mut self, line: usize) {
        let column = self.active.editor.cursor_line_column().1;
        let start = self.active.editor.text().byte_of_line(line);
        let raw_end = self.active.editor.text().byte_of_line(line + 1);
        let raw_line = self.active.editor.text().slice(start..raw_end);
        let end = raw_end.saturating_sub(usize::from(raw_line.as_bytes().last() == Some(&b'\n')));
        let line_without_ending = &raw_line[..end.saturating_sub(start)];
        let relative = line_without_ending
            .char_indices()
            .nth(column)
            .map_or(end.saturating_sub(start), |(byte, _)| byte);
        self.active
            .editor
            .set_cursor(start.saturating_add(relative));
    }

    pub(super) fn search_word_under_cursor(&mut self, backward: bool, count: usize) {
        let Some(word) = self.word_under_cursor() else {
            self.message = "no word under cursor".to_owned();
            return;
        };
        let direction = if backward {
            SearchDirection::Backward
        } else {
            SearchDirection::Forward
        };
        let pattern = format!(r"\<{word}\>");
        let found = match self.active.editor.search(&pattern, direction) {
            Ok(found) => found,
            Err(error) => {
                self.show_error(error);
                return;
            }
        };
        self.message = if found {
            for _ in 1..count {
                if !self.active.editor.search_next(false) {
                    break;
                }
            }
            format!("{}{}", if backward { '#' } else { '*' }, word)
        } else {
            format!("pattern not found: {word}")
        };
        if let Err(error) = self.synchronize_search(&pattern, direction, true) {
            self.show_error(error);
        }
    }

    pub(super) fn show_file_info(&mut self) {
        let text = self.active.editor.text();
        let line_count = text.line_of_byte(text.len_bytes()).saturating_add(1);
        let (line, column) = self.active.editor.cursor_line_column();
        let percent = (line + 1)
            .saturating_mul(100)
            .checked_div(line_count)
            .unwrap_or(100);
        self.message = format!(
            "\"{}\"{} {} line(s) --{}%-- {}:{}",
            self.active.name(),
            if self.active.editor.is_dirty() {
                " [Modified]"
            } else {
                ""
            },
            line_count,
            percent,
            line + 1,
            column + 1
        );
    }

    pub(super) fn poll_terminal(&mut self) -> Result<bool> {
        let Some(terminal) = &mut self.terminal else {
            return Ok(false);
        };
        let changed = terminal.poll()?;
        if changed && let Some(code) = terminal.exit_code() {
            self.terminal_focused = false;
            self.message = format!("terminal exited with status {code}");
        }
        Ok(changed)
    }

    pub(super) fn terminal_frame(&self) -> wren_engine::EngineFrame {
        let Some(terminal) = &self.terminal else {
            return wren_engine::EngineFrame {
                text: "terminal unavailable".into(),
                cursor_byte: 0,
            };
        };
        let text = terminal.surface().contents();
        let (row, column) = terminal.surface().cursor_position();
        let row_start = text
            .match_indices('\n')
            .nth(usize::from(row).saturating_sub(1))
            .map_or(0, |(byte, _)| byte + 1);
        let row_end = text[row_start..]
            .find('\n')
            .map_or(text.len(), |offset| row_start + offset);
        let cursor_byte = text[row_start..row_end]
            .char_indices()
            .nth(usize::from(column))
            .map_or(row_end, |(offset, _)| row_start + offset);
        wren_engine::EngineFrame {
            text: text.into(),
            cursor_byte,
        }
    }

    pub(super) fn desired_terminal_grid(&self, layout: &mut ViewportLayout) -> DesiredGrid {
        let placeholder = wren_engine::EngineFrame {
            text: String::new().into(),
            cursor_byte: 0,
        };
        let mut grid = layout.desired_editor_grid(&placeholder, &self.terminal_status(), None);
        let Some(terminal) = &self.terminal else {
            return grid;
        };
        let surface = terminal.surface();
        let (_, surface_columns) = surface.size();
        let content_rows = grid.height.saturating_sub(1);
        let columns = usize::from(surface_columns).min(grid.width);
        for row in 0..content_rows {
            let mut cells = Vec::with_capacity(columns);
            let mut column = 0_usize;
            while column < columns {
                let Some(source) = surface.cell(
                    u16::try_from(row).unwrap_or(u16::MAX),
                    u16::try_from(column).unwrap_or(u16::MAX),
                ) else {
                    break;
                };
                if source.wide_continuation {
                    column = column.saturating_add(1);
                    continue;
                }
                let width = if source.wide { 2 } else { 1 };
                if column.saturating_add(width) > columns {
                    break;
                }
                cells.push(ViewCell {
                    grapheme: if source.contents.is_empty() {
                        " ".into()
                    } else {
                        source.contents.into()
                    },
                    width: u8::try_from(width).unwrap_or(1),
                    style: CellStyle {
                        bold: source.bold,
                        italic: source.italic,
                        underline: source.underline,
                        strikethrough: false,
                        reverse: source.reverse,
                        foreground: Some(terminal_cell_color(source.foreground, self.theme.text)),
                        background: Some(terminal_cell_color(source.background, self.theme.base)),
                    },
                });
                column = column.saturating_add(width);
            }
            cells.extend((column..grid.width).map(|_| ViewCell {
                grapheme: " ".into(),
                width: 1,
                style: CellStyle {
                    foreground: Some(CellColor::Rgb(self.theme.text)),
                    background: Some(CellColor::Rgb(self.theme.base)),
                    ..CellStyle::default()
                },
            }));
            grid.rows[row] = Arc::new(CellRow { cells });
        }
        let (cursor_row, cursor_column) = surface.cursor_position();
        grid.cursor = (
            usize::from(cursor_column).min(grid.width.saturating_sub(1)),
            usize::from(cursor_row).min(content_rows.saturating_sub(1)),
        );
        grid
    }

    pub(super) fn terminal_status(&self) -> String {
        self.terminal.as_ref().map_or_else(
            || " TERMINAL | unavailable".to_owned(),
            |terminal| {
                let state = terminal
                    .exit_code()
                    .map_or_else(|| "running".to_owned(), |code| format!("exit {code}"));
                format!(
                    " TERMINAL | {state} | {} bytes | Ctrl-\\ Ctrl-N returns",
                    terminal.bytes_read()
                )
            },
        )
    }

    pub(super) fn show_ai_transcript(&mut self) {
        let (text, decorations) = lsp_popup_markdown(&self.ai_transcript, self.theme);
        self.popup = Some(TextPopup {
            title: "Avante · Codex".into(),
            text: text.into(),
            scroll: 0,
            cursor: None,
            decorations,
        });
        self.popup_deadline = None;
    }

    pub(super) fn start_ai_task(&mut self, prompt: &str) -> Result<()> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            self.prompt = Some(Prompt::new(PromptKind::Ai));
            return Ok(());
        }
        if self.active_task.is_some() {
            bail!("a TaskCommand is already running");
        }
        let task_id = CommandTaskId::new(self.next_task_id);
        self.next_task_id = self
            .next_task_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("task ID overflow"))?;
        let root = self
            .active
            .document
            .presentation_path()
            .and_then(|path| git_root_for(path).ok())
            .or_else(|| self.workspace_folders.first().cloned())
            .unwrap_or_else(|| PathBuf::from("."));
        let (line, column) = self.active.editor.cursor_line_column();
        let context = self.active.document.presentation_path().map_or_else(
            || prompt.to_owned(),
            |path| {
                format!(
                    "The active editor file is {} at line {}, column {}.\n\n{}",
                    path.display(),
                    line + 1,
                    column + 1,
                    prompt
                )
            },
        );
        let mut environment = BTreeMap::new();
        if let Ok(path) = env::var("PATH") {
            environment.insert("PATH".into(), path.into());
        }
        let arguments = vec![
            "exec".into(),
            "--sandbox".into(),
            "read-only".into(),
            "--color".into(),
            "never".into(),
            "--skip-git-repo-check".into(),
            "-C".into(),
            root.to_string_lossy().into_owned().into_boxed_str(),
            context.into_boxed_str(),
        ];
        let spec = WorkflowTaskSpec {
            program: "codex".into(),
            arguments,
            environment,
            visibility: DocumentVisibility::Persisted,
            save: SavePolicy::Never,
            max_output_bytes: 4 * 1024 * 1024,
        };
        let cancellation = self.tasks.submit(
            CommandTask {
                task_id,
                affected_documents: Vec::new(),
                label: "Codex assistant".into(),
            },
            move |context| {
                context.checkpoint()?;
                let token = context.cancellation_token();
                let output = TaskSupervisor::new(true)
                    .run_until_cancelled(&spec, || token.is_cancelled())
                    .map_err(|error| TaskFailure::Failed(error.to_string().into()))?;
                context.checkpoint()?;
                if output.cancelled {
                    return Err(TaskFailure::Cancelled);
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if output.status != Some(0) {
                    let detail = if stderr.trim().is_empty() {
                        stdout.trim()
                    } else {
                        stderr.trim()
                    };
                    return Err(TaskFailure::Failed(
                        format!("Codex failed: {detail}").into(),
                    ));
                }
                Ok(Effects {
                    messages: vec![stdout.trim().to_owned().into_boxed_str()],
                    ..Effects::default()
                })
            },
        )?;
        self.active_task = Some(cancellation);
        self.active_ai_task = Some(task_id);
        self.popup = None;
        self.popup_deadline = None;
        self.message = "Codex is thinking…".to_owned();
        Ok(())
    }

    pub(super) fn start_make_task(&mut self, program: &str, arguments: &[Box<str>]) -> Result<()> {
        if self.active_task.is_some() {
            bail!("a TaskCommand is already running");
        }
        let task_id = CommandTaskId::new(self.next_task_id);
        self.next_task_id = self
            .next_task_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("task ID overflow"))?;
        let mut environment = BTreeMap::new();
        if let Ok(path) = env::var("PATH") {
            environment.insert("PATH".into(), path.into());
        }
        let spec = WorkflowTaskSpec {
            program: program.into(),
            arguments: arguments.to_vec(),
            environment,
            visibility: DocumentVisibility::Persisted,
            save: SavePolicy::Never,
            max_output_bytes: 1024 * 1024,
        };
        let cancellation = self.tasks.submit(
            CommandTask {
                task_id,
                affected_documents: Vec::new(),
                label: format!("make {program}").into(),
            },
            move |context| {
                context.checkpoint()?;
                let token = context.cancellation_token();
                let output = TaskSupervisor::new(true)
                    .run_until_cancelled(&spec, || token.is_cancelled())
                    .map_err(|error| TaskFailure::Failed(error.to_string().into()))?;
                context.checkpoint()?;
                if output.cancelled {
                    return Err(TaskFailure::Cancelled);
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let detail = if stderr.trim().is_empty() {
                    stdout.trim()
                } else {
                    stderr.trim()
                };
                let status = output
                    .status
                    .map_or_else(|| "signal".to_owned(), |status| status.to_string());
                Ok(Effects {
                    messages: vec![
                        format!(
                            "task {task_id:?} status {status}: {}",
                            if detail.is_empty() {
                                "no output"
                            } else {
                                detail
                            }
                        )
                        .into(),
                    ],
                    ..Effects::default()
                })
            },
        )?;
        self.active_task = Some(cancellation);
        self.message = format!("task {} running", task_id.get());
        Ok(())
    }

    pub(super) fn start_format_task(
        &mut self,
        program: &str,
        arguments: &[Box<str>],
    ) -> Result<()> {
        if self.active_task.is_some() {
            bail!("a TaskCommand is already running");
        }
        if self.active.editor.is_read_only() {
            bail!("document is read-only");
        }
        let task_id = CommandTaskId::new(self.next_task_id);
        self.next_task_id = self
            .next_task_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("task ID overflow"))?;
        let document_id = self.active.document_id;
        let base_revision = self.active.editor.revision();
        let input = self.active.editor.contents();
        let mut environment = BTreeMap::new();
        if let Ok(path) = env::var("PATH") {
            environment.insert("PATH".into(), path.into());
        }
        let spec = WorkflowTaskSpec {
            program: program.into(),
            arguments: arguments.to_vec(),
            environment,
            visibility: DocumentVisibility::Persisted,
            save: SavePolicy::Never,
            max_output_bytes: input.len().saturating_mul(4).max(1024 * 1024),
        };
        let cancellation = self.tasks.submit(
            CommandTask {
                task_id,
                affected_documents: vec![document_id],
                label: format!("format {program}").into(),
            },
            move |context| {
                context.checkpoint()?;
                let token = context.cancellation_token();
                let formatted = run_formatter_until_cancelled(
                    &spec,
                    true,
                    document_id,
                    base_revision,
                    base_revision,
                    &input,
                    || token.is_cancelled(),
                )
                .map_err(|error| match error {
                    WorkflowError::Cancelled => TaskFailure::Cancelled,
                    error => TaskFailure::Failed(error.to_string().into()),
                })?;
                context.checkpoint()?;
                let edit_proposals = if formatted.edits.is_empty() {
                    Vec::new()
                } else {
                    vec![EditProposal {
                        document_id,
                        base_revision,
                        transactions: vec![
                            Transaction::new(base_revision, formatted.edits)
                                .map_err(|error| TaskFailure::Failed(error.to_string().into()))?,
                        ],
                        label: "formatter".into(),
                    }]
                };
                Ok(Effects {
                    edit_proposals,
                    messages: vec!["format complete".into()],
                    ..Effects::default()
                })
            },
        )?;
        self.active_task = Some(cancellation);
        self.message = format!("formatter task {} running", task_id.get());
        Ok(())
    }

    pub(super) fn format_active_language(&mut self) -> Result<()> {
        if self.format_active_sync(true)? {
            self.message = "format complete".to_owned();
        }
        Ok(())
    }

    pub(super) fn format_text_width(&mut self) -> Result<()> {
        let text = self.active.editor.contents();
        let (range, was_visual) =
            if matches!(self.active.editor.mode(), Mode::Visual | Mode::VisualLine) {
                (self.active.editor.selection_byte_range(), true)
            } else {
                let current = self.active.editor.cursor_line_column().0;
                let mut first = current;
                while first > 0 {
                    let start = self.active.editor.text().byte_of_line(first - 1);
                    let end = self.active.editor.text().byte_of_line(first);
                    if text[start..end].trim().is_empty() {
                        break;
                    }
                    first -= 1;
                }
                let mut last = current + 1;
                let line_count = self.active.editor.text().line_of_byte(text.len()) + 1;
                while last < line_count {
                    let start = self.active.editor.text().byte_of_line(last);
                    let end = self.active.editor.text().byte_of_line(last + 1);
                    if text[start..end].trim().is_empty() {
                        break;
                    }
                    last += 1;
                }
                (
                    self.active.editor.text().byte_of_line(first)
                        ..self.active.editor.text().byte_of_line(last),
                    false,
                )
            };
        let source = text.get(range.clone()).unwrap_or_default();
        let formatted = wrap_editor_text(source, 79);
        if formatted == source {
            self.message = "text already fits textwidth=79".to_owned();
            return Ok(());
        }
        if was_visual {
            self.dispatch_key(KeyEvent::plain(KeyCode::Escape));
        }
        let transaction = Transaction::new(
            self.active.editor.revision(),
            vec![Edit::new(range, formatted)],
        )?;
        self.active.editor.apply_transaction(transaction.clone())?;
        self.after_transaction(Some(transaction));
        self.message = "formatted to textwidth=79".to_owned();
        Ok(())
    }

    pub(super) fn format_active_sync(&mut self, explicit: bool) -> Result<bool> {
        if self.active.editor.is_read_only() {
            if explicit {
                self.message = "document is read-only".to_owned();
            }
            return Ok(false);
        }
        let Some(path) = self.active.document.presentation_path() else {
            if explicit {
                self.message = "formatter needs a named buffer".to_owned();
            }
            return Ok(false);
        };
        let Some(invocation) = formatter_invocation(path) else {
            return self.lsp_format_sync(explicit);
        };
        if !executable_exists(&invocation.program) {
            if explicit {
                self.message = format!("formatter {} is not installed", invocation.program);
            }
            return Ok(false);
        }
        let input = self.active.editor.contents();
        let mut child = Command::new(&invocation.program)
            .args(&invocation.arguments)
            .current_dir(self.workspace_root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("start formatter {}", invocation.program))?;
        child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("formatter stdin is unavailable"))?
            .write_all(input.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!(
                "{} failed: {}",
                invocation.program,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let formatted = String::from_utf8(output.stdout)
            .with_context(|| format!("{} returned non-UTF-8", invocation.program))?;
        if formatted == input {
            return Ok(true);
        }
        let transaction = Transaction::new(
            self.active.editor.revision(),
            vec![Edit::new(0..input.len(), formatted)],
        )?;
        self.active.editor.apply_transaction(transaction.clone())?;
        self.after_transaction(Some(transaction));
        Ok(true)
    }

    pub(super) fn lsp_format_sync(&mut self, explicit: bool) -> Result<bool> {
        if self.active_language_server().is_none() {
            if explicit {
                self.message = format!(
                    "no formatter configured for {}",
                    language_bundle(self.active.document.presentation_path()).language_id
                );
            }
            return Ok(false);
        }
        let (mut client, uri) = self.start_lsp()?;
        let result = client.request(
            "textDocument/formatting",
            serde_json::json!({
                "textDocument": {"uri": uri},
                "options": {"tabSize": 2, "insertSpaces": true}
            }),
        )?;
        let edits: Vec<LspTextEdit> = serde_json::from_value(result)?;
        let revision = self.active.editor.revision();
        let text = self.active.editor.contents();
        let lowered =
            lower_lsp_text_edits(self.active.document_id, revision, revision, &text, edits)?;
        if !lowered.edits.is_empty() {
            let transaction = Transaction::new(revision, lowered.edits)?;
            self.active.editor.apply_transaction(transaction.clone())?;
            self.after_transaction(Some(transaction));
        }
        Ok(true)
    }
}
