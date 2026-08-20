use super::*;

impl App {
    pub(super) fn open_terminal(&mut self, program: Option<&str>, arguments: &[Box<str>]) -> Result<()> {
        let directory = env::current_dir().context("locate terminal working directory")?;
        self.open_terminal_in(program, arguments, &directory)
    }

    pub(super) fn open_terminal_in(&mut self, program: Option<&str>, arguments: &[Box<str>], directory: &Path) -> Result<()> {
        if self.terminal.as_ref().is_some_and(|terminal| terminal.exit_code().is_none()) {
            self.input_focus = InputFocus::Terminal { escape_pending: false };
            self.message.clear();
            return Ok(());
        }
        let program = program.map(str::to_owned).or_else(|| env::var("SHELL").ok()).unwrap_or_else(|| "sh".to_owned());
        let arguments = arguments.iter().map(AsRef::<str>::as_ref).collect::<Vec<_>>();
        let rows = u16::try_from(self.viewport_rows.saturating_sub(1)).unwrap_or(u16::MAX).max(1);
        let columns = u16::try_from(self.viewport_columns).unwrap_or(u16::MAX).max(1);
        self.terminal = Some(PtySession::spawn_in(&program, &arguments, rows, columns, directory)?);
        self.input_focus = InputFocus::Terminal { escape_pending: false };
        self.set_message(format!("terminal: {program}"))
    }

    pub(super) fn handle_terminal_input(&mut self, input: TerminalInput) -> Result<()> {
        match input {
            TerminalInput::Resized(dimensions) => self.resize_terminal_to(dimensions),
            TerminalInput::Paste(text) => send_pty(&mut self.terminal, text.as_bytes())?,
            TerminalInput::Key(key) => {
                let escape_pending = matches!(self.input_focus, InputFocus::Terminal { escape_pending: true });
                self.input_focus = InputFocus::Terminal { escape_pending: false };
                if escape_pending {
                    if key.control() && matches!(key.code, TerminalKeyCode::Char('n' | 'N')) {
                        self.input_focus = InputFocus::Editor;
                        self.message = "terminal hidden; :terminal returns".to_owned();
                        return Ok(());
                    }
                    send_pty(&mut self.terminal, &[0x1c])?;
                }
                if key.control() && key.code == TerminalKeyCode::Char('\\') {
                    self.input_focus = InputFocus::Terminal { escape_pending: true };
                    return Ok(());
                }
                if let Some(bytes) = terminal_key_bytes(key) {
                    send_pty(&mut self.terminal, &bytes)?;
                }
            }
            event @ TerminalInput::Mouse { .. } => send_pty_mouse(&mut self.terminal, &event)?,
            TerminalInput::Ignored => {}
        }
        Ok(())
    }

    pub(super) fn resize_terminal(&mut self, rows: usize, columns: usize) {
        self.viewport_rows = rows.max(1);
        self.viewport_columns = columns.max(1);
        let terminal_error = self.terminal.as_mut().and_then(|terminal| {
            let rows = u16::try_from(rows.saturating_sub(1)).unwrap_or(u16::MAX).max(1);
            let columns = u16::try_from(columns).unwrap_or(u16::MAX).max(1);
            terminal.resize(rows, columns).err()
        });
        if let Some(error) = terminal_error {
            self.show_error(format!("terminal resize: {error}"));
        }
        self.resize_agent_terminal();
    }

    pub(super) fn resize_terminal_to(&mut self, dimensions: TerminalDimensions) {
        if let Some(cell_height_to_width) = dimensions.cell_height_to_width() {
            self.startup_screen.get_mut().set_cell_height_to_width(cell_height_to_width);
        }
        self.resize_terminal(dimensions.rows, dimensions.columns);
    }

    pub(super) fn take_normal_count(&mut self) -> Option<usize> {
        let count = match self.active.editor.pending_parse_state() {
            Some(ParseState::Count(value)) => usize::try_from(value.get()).ok(),
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

    pub(super) fn scroll_page(&mut self, direction: isize, full_page: bool, count: Option<usize>) {
        let content_rows = self.viewport_rows.saturating_sub(1).max(1);
        let amount = if full_page {
            content_rows.saturating_sub(2).max(1).saturating_mul(count.unwrap_or(1))
        } else if let Some(count) = count {
            count
        } else {
            content_rows.checked_div(2).unwrap_or(1).max(1)
        };
        let text_store = self.active.editor.text();
        let line_count = text_store.line_of_byte(text_store.len_bytes()).saturating_add(1);
        let last_line = line_count.saturating_sub(1);
        let current_line = self.active.editor.cursor_line_column().0;
        let current_top = self.views.active_window().top_line;
        let (next_line, next_top) = if direction < 0 {
            (current_line.saturating_sub(amount), current_top.saturating_sub(amount))
        } else {
            (
                current_line.saturating_add(amount).min(last_line),
                current_top.saturating_add(amount).min(last_line.saturating_sub(content_rows.saturating_sub(1))),
            )
        };
        let column = self.active.editor.cursor_line_column().1;
        let start = self.active.editor.text().byte_of_line(next_line);
        let end = self.active.editor.text().byte_of_line(next_line + 1).saturating_sub(usize::from(next_line < last_line));
        let line = self.active.editor.text().slice(start..end);
        let relative = line.char_indices().nth(column).map_or(end.saturating_sub(start), |(byte, _)| byte);
        self.active.editor.set_cursor(start.saturating_add(relative));
        self.views.active_window_mut().top_line = next_top;
        self.message.clear();
    }

    pub(super) fn scroll_view_line(&mut self, direction: isize, count: usize) {
        let content_rows = self.viewport_rows.saturating_sub(1).max(1);
        let text_store = self.active.editor.text();
        let last_line = text_store.line_of_byte(text_store.len_bytes());
        let max_top = last_line.saturating_sub(content_rows.saturating_sub(1));
        let current_top = self.views.active_window().top_line;
        let next_top = if direction < 0 { current_top.saturating_sub(count) } else { current_top.saturating_add(count).min(max_top) };
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
        let relative = line_without_ending.char_indices().nth(column).map_or(end.saturating_sub(start), |(byte, _)| byte);
        self.active.editor.set_cursor(start.saturating_add(relative));
    }

    pub(super) fn search_word_under_cursor(&mut self, backward: bool, count: usize) {
        let Some(word) = self.word_under_cursor() else {
            self.message = "no word under cursor".to_owned();
            return;
        };
        let direction = if backward { SearchDirection::Backward } else { SearchDirection::Forward };
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
        let percent = (line + 1).saturating_mul(100).checked_div(line_count).unwrap_or(100);
        self.message = format!(
            "\"{}\"{} {} line(s) --{}%-- {}:{}",
            self.active.name(),
            if self.active.editor.is_dirty() { " [Modified]" } else { "" },
            line_count,
            percent,
            line + 1,
            column + 1
        );
    }

    pub(super) fn poll_terminal(&mut self) -> Result<bool> {
        let (changed, exit) = poll_pty(&mut self.terminal)?;
        if let Some(code) = exit {
            self.input_focus = InputFocus::Editor;
            self.message = format!("terminal exited with status {code}");
        }
        Ok(changed)
    }

    pub(super) fn terminal_frame(&self) -> wren_engine::EngineFrame {
        let Some(terminal) = &self.terminal else {
            return wren_engine::EngineFrame::new("terminal unavailable", 0);
        };
        let text = terminal.surface().contents();
        let (row, column) = terminal.surface().cursor_position();
        let row_start = text.match_indices('\n').nth(usize::from(row).saturating_sub(1)).map_or(0, |(byte, _)| byte + 1);
        let row_end = text[row_start..].find('\n').map_or(text.len(), |offset| row_start + offset);
        let cursor_byte = text[row_start..row_end].char_indices().nth(usize::from(column)).map_or(row_end, |(offset, _)| row_start + offset);
        wren_engine::EngineFrame::new(text, cursor_byte)
    }

    pub(super) fn desired_terminal_grid(&self, layout: &mut ViewportLayout) -> DesiredGrid {
        let placeholder = wren_engine::EngineFrame::new("", 0);
        let mut grid = layout.desired_editor_grid(&placeholder, &self.terminal_status(), None);
        let Some(terminal) = &self.terminal else {
            return grid;
        };
        let surface = terminal.surface();
        let content_rows = grid.height.saturating_sub(1);
        for (row, cells) in self.terminal_surface_rows(terminal, content_rows, grid.width).into_iter().enumerate() {
            grid.rows[row] = Arc::new(cells);
        }
        let (cursor_row, cursor_column) = surface.cursor_position();
        grid.cursor = (usize::from(cursor_column).min(grid.width.saturating_sub(1)), usize::from(cursor_row).min(content_rows.saturating_sub(1)));
        grid
    }

    pub(super) fn terminal_surface_rows(&self, terminal: &PtySession, rows: usize, columns: usize) -> Vec<CellRow> {
        let surface = terminal.surface();
        let (_, surface_columns) = surface.size();
        let columns = usize::from(surface_columns).min(columns);
        (0..rows)
            .map(|row| {
                let mut cells = Vec::with_capacity(columns);
                let mut column = 0_usize;
                while column < columns {
                    let Some(source) = surface.cell(u16::try_from(row).unwrap_or(u16::MAX), u16::try_from(column).unwrap_or(u16::MAX)) else {
                        break;
                    };
                    if source.is_wide_continuation() {
                        column = column.saturating_add(1);
                        continue;
                    }
                    let width = if source.is_wide() { 2 } else { 1 };
                    if column.saturating_add(width) > columns {
                        break;
                    }
                    cells.push(ViewCell {
                        grapheme: if source.contents().is_empty() { " ".into() } else { source.contents().into() },
                        width: u8::try_from(width).unwrap_or(1),
                        style: CellStyle {
                            attributes: u8::from(source.bold())
                                | u8::from(source.italic()) << 1
                                | u8::from(source.underline()) << 2
                                | u8::from(source.inverse()) << 4,
                            foreground: Some(terminal_cell_color(source.fgcolor(), self.theme.color(CatppuccinColor::Text))),
                            background: Some(terminal_cell_color(source.bgcolor(), self.theme.color(CatppuccinColor::Base))),
                        },
                    });
                    column = column.saturating_add(width);
                }
                cells.extend((column..columns).map(|_| ViewCell {
                    grapheme: " ".into(),
                    width: 1,
                    style: CellStyle::rgb(self.theme.color(CatppuccinColor::Text), self.theme.color(CatppuccinColor::Base)),
                }));
                CellRow { cells }
            })
            .collect()
    }

    pub(super) fn terminal_status(&self) -> String {
        self.terminal.as_ref().map_or_else(
            || " TERMINAL | unavailable".to_owned(),
            |terminal| {
                let state = terminal.exit_code().map_or_else(|| "running".to_owned(), |code| format!("exit {code}"));
                format!(" TERMINAL | {state} | {} bytes | Ctrl-\\ Ctrl-N returns", terminal.bytes_read())
            },
        )
    }

    pub(super) fn start_make_task(&mut self, program: &str, arguments: &[Box<str>]) -> Result<()> {
        let spec = WorkflowTaskSpec::persisted(program, arguments.to_vec(), inherited_path_environment(), 1024 * 1024);
        self.start_task(Vec::new(), format!("make {program}"), "task ", move |task_id, context| {
            context.checkpoint()?;
            let token = context.cancellation_token();
            let output =
                TaskSupervisor::new(true).run_until_cancelled(&spec, || token.is_cancelled()).map_err(|error| TaskFailure::Failed(error.to_string().into()))?;
            context.checkpoint()?;
            if output.cancelled {
                return Err(TaskFailure::Cancelled);
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
            let status = output.status.map_or_else(|| "signal".to_owned(), |status| status.to_string());
            Ok(Effects {
                messages: vec![format!("task {task_id:?} status {status}: {}", if detail.is_empty() { "no output" } else { detail }).into()],
                ..Effects::default()
            })
        })
    }

    pub(super) fn start_format_task(&mut self, program: &str, arguments: &[Box<str>]) -> Result<()> {
        if self.active.editor.is_read_only() {
            bail!("document is read-only");
        }
        let document_id = self.active.document_id;
        let base_revision = self.active.editor.revision();
        let input = self.active.editor.contents();
        let spec = WorkflowTaskSpec::persisted(program, arguments.to_vec(), inherited_path_environment(), input.len().saturating_mul(4).max(1024 * 1024));
        self.start_task(vec![document_id], format!("format {program}"), "formatter task ", move |_, context| {
            context.checkpoint()?;
            let token = context.cancellation_token();
            let formatted =
                run_formatter_until_cancelled(&spec, true, document_id, base_revision, base_revision, &input, || token.is_cancelled()).map_err(|error| {
                    match error {
                        WorkflowError::Cancelled => TaskFailure::Cancelled,
                        error => TaskFailure::Failed(error.to_string().into()),
                    }
                })?;
            context.checkpoint()?;
            let edit_proposals = if formatted.edits.is_empty() {
                Vec::new()
            } else {
                vec![EditProposal {
                    document_id,
                    base_revision,
                    transactions: vec![Transaction::new(base_revision, formatted.edits).map_err(|error| TaskFailure::Failed(error.to_string().into()))?],
                    label: "formatter".into(),
                }]
            };
            Ok(Effects { edit_proposals, messages: vec!["format complete".into()] })
        })
    }

    pub(super) fn format_active_language(&mut self) -> Result<()> {
        if self.format_active_sync(true)? {
            self.message = "format complete".to_owned();
        }
        Ok(())
    }

    pub(super) fn format_text_width(&mut self) -> Result<()> {
        let text = self.active.editor.contents();
        let (range, was_visual) = if matches!(self.active.editor.mode(), Mode::Visual | Mode::VisualLine) {
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
            (self.active.editor.text().byte_of_line(first)..self.active.editor.text().byte_of_line(last), false)
        };
        let source = text.get(range.clone()).unwrap_or_default();
        let formatted = wrap_editor_text(source, 79);
        if formatted == source {
            return self.set_message("text already fits textwidth=79".to_owned());
        }
        if was_visual {
            self.dispatch_key(KeyEvent::plain(KeyCode::Escape));
        }
        let transaction = Transaction::new(self.active.editor.revision(), vec![Edit::new(range, formatted)])?;
        self.active.editor.apply_transaction(transaction.clone())?;
        self.after_transaction(Some(transaction));
        self.set_message("formatted to textwidth=79".to_owned())
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
        child.stdin.take().ok_or_else(|| anyhow!("formatter stdin is unavailable"))?.write_all(input.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!("{} failed: {}", invocation.program, String::from_utf8_lossy(&output.stderr).trim());
        }
        let formatted = String::from_utf8(output.stdout).with_context(|| format!("{} returned non-UTF-8", invocation.program))?;
        if formatted == input {
            return Ok(true);
        }
        let transaction = Transaction::new(self.active.editor.revision(), vec![Edit::new(0..input.len(), formatted)])?;
        self.active.editor.apply_transaction(transaction.clone())?;
        self.after_transaction(Some(transaction));
        Ok(true)
    }

    pub(super) fn lsp_format_sync(&mut self, explicit: bool) -> Result<bool> {
        if self.active_language_server().is_none() {
            if explicit {
                self.message = format!("no formatter configured for {}", language_bundle(self.active.document.presentation_path()).language_id);
            }
            return Ok(false);
        }
        let (client, uri) = self.active_lsp_client()?;
        let result = client.request(
            "textDocument/formatting",
            serde_json::json!({
                "textDocument": {"uri": uri},
                "options": {"tabSize": 2, "insertSpaces": true}
            }),
        )?;
        let edits: Vec<LspTextEdit> = serde_json::from_value(result)?;
        self.apply_lsp_text_edits(edits)?;
        Ok(true)
    }
}

fn inherited_path_environment() -> BTreeMap<Box<str>, Box<str>> {
    env::var("PATH").map(|path| BTreeMap::from([("PATH".into(), path.into())])).unwrap_or_default()
}

pub(super) fn poll_pty(terminal: &mut Option<PtySession>) -> Result<(bool, Option<u32>)> {
    let Some(terminal) = terminal else { return Ok((false, None)) };
    let changed = terminal.poll()?;
    Ok((changed, changed.then_some(terminal.exit_code()).flatten()))
}

pub(super) fn send_pty(terminal: &mut Option<PtySession>, bytes: &[u8]) -> Result<()> {
    terminal.as_mut().map_or(Ok(()), |terminal| terminal.send_input(bytes)).map_err(Into::into)
}

pub(super) fn send_pty_mouse(terminal: &mut Option<PtySession>, event: &TerminalInput) -> Result<()> {
    let Some(terminal) = terminal.as_mut().filter(|terminal| {
        terminal.surface().mouse_protocol_mode() != vt100::MouseProtocolMode::None
            && terminal.surface().mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr
    }) else {
        return Ok(());
    };
    terminal.send_input(&terminal_mouse_bytes(event)).map_err(Into::into)
}
