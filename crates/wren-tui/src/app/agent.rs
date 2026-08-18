use super::*;

impl App {
    pub(super) fn toggle_agent_sidebar(&mut self) -> Result<()> {
        if self.agent_sidebar.visible {
            self.agent_sidebar.visible = false;
            self.agent_sidebar.focused = false;
            self.message = "Oh My Pi pane hidden".to_owned();
            return Ok(());
        }
        self.open_agent_sidebar()
    }

    pub(super) fn open_agent_sidebar(&mut self) -> Result<()> {
        let program = env::var("WREN_AGENT_TUI").unwrap_or_else(|_| "omp".to_owned());
        self.open_agent_sidebar_in(&program, &[])
    }

    pub(super) fn open_agent_sidebar_in(
        &mut self,
        program: &str,
        arguments: &[&str],
    ) -> Result<()> {
        self.popup = None;
        self.popup_deadline = None;
        self.prompt = None;
        if self
            .agent_terminal
            .as_ref()
            .is_none_or(|terminal| terminal.exit_code().is_some())
        {
            let (rows, columns) = self.agent_terminal_dimensions();
            self.agent_terminal = Some(PtySession::spawn_in(
                program,
                arguments,
                rows,
                columns,
                &self.root_workspace,
            )?);
        }
        self.agent_sidebar.visible = true;
        self.agent_sidebar.focused = true;
        self.agent_terminal_escape_pending = false;
        self.agent_window_prefix_pending = false;
        self.resize_agent_terminal();
        self.message = "Oh My Pi · Ctrl-\\ Ctrl-N returns to editor".to_owned();
        Ok(())
    }

    pub(super) fn start_ai_task(&mut self, prompt: &str) -> Result<()> {
        self.open_agent_sidebar()?;
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Ok(());
        }
        let terminal = self
            .agent_terminal
            .as_mut()
            .ok_or_else(|| anyhow!("Oh My Pi terminal did not start"))?;
        terminal.send_input(prompt.as_bytes())?;
        terminal.send_input(b"\r")?;
        Ok(())
    }

    pub(super) fn poll_agent_terminal(&mut self) -> Result<bool> {
        let Some(terminal) = &mut self.agent_terminal else {
            return Ok(false);
        };
        let changed = terminal.poll()?;
        if changed && let Some(code) = terminal.exit_code() {
            self.agent_sidebar.focused = false;
            self.message = format!("Oh My Pi exited with status {code}");
        }
        Ok(changed)
    }

    pub(super) fn apply_agent_sidebar(
        &self,
        layout: &mut ViewportLayout,
        grid: DesiredGrid,
    ) -> DesiredGrid {
        if !self.agent_sidebar.visible {
            return grid;
        }
        let Some(terminal) = &self.agent_terminal else {
            return grid;
        };
        let Some(column) = layout.terminal_sidebar_column() else {
            return grid;
        };
        let columns = self
            .viewport_columns
            .saturating_sub(column.saturating_add(1));
        let rows = self.terminal_surface_rows(terminal, self.viewport_rows, columns);
        let (cursor_row, cursor_column) = terminal.surface().cursor_position();
        layout.apply_terminal_sidebar(
            grid,
            TerminalSidebar {
                rows: &rows,
                cursor: (usize::from(cursor_column), usize::from(cursor_row)),
                focused: self.agent_sidebar.focused,
            },
        )
    }

    pub(super) fn handle_agent_input(&mut self, input: TerminalInput) -> Result<()> {
        match input {
            TerminalInput::Resized { columns, rows } => {
                self.viewport_rows = rows.max(1);
                self.viewport_columns = columns.max(1);
                self.resize_agent_terminal();
            }
            TerminalInput::Paste(text) => {
                if let Some(terminal) = &mut self.agent_terminal {
                    terminal.send_input(text.as_bytes())?;
                }
            }
            TerminalInput::Key(key) => self.handle_agent_key(key)?,
            event @ (TerminalInput::MouseScroll { .. }
            | TerminalInput::MouseClick { .. }
            | TerminalInput::MouseDrag { .. }
            | TerminalInput::MouseRelease { .. }) => self.send_agent_mouse_input(&event)?,
            TerminalInput::Ignored => {}
        }
        Ok(())
    }

    fn handle_agent_key(&mut self, key: TerminalKey) -> Result<()> {
        if self.agent_window_prefix_pending {
            self.agent_window_prefix_pending = false;
            if self.handle_agent_window_key(key) {
                return Ok(());
            }
            if let Some(terminal) = &mut self.agent_terminal {
                terminal.send_input(&[0x17])?;
            }
        }
        if self.agent_terminal_escape_pending {
            self.agent_terminal_escape_pending = false;
            if key.control && matches!(key.code, TerminalKeyCode::Char('n' | 'N')) {
                self.agent_sidebar.focused = false;
                self.message = "Oh My Pi remains open; click the pane to return".to_owned();
                return Ok(());
            }
            if let Some(terminal) = &mut self.agent_terminal {
                terminal.send_input(&[0x1c])?;
            }
        }
        if key.control && key.code == TerminalKeyCode::Char('\\') {
            self.agent_terminal_escape_pending = true;
            return Ok(());
        }
        if key.control && matches!(key.code, TerminalKeyCode::Char('w' | 'W')) {
            self.agent_window_prefix_pending = true;
            self.message = "window: h editor · q hide harness".to_owned();
            return Ok(());
        }
        if let Some(bytes) = terminal_key_bytes(key)
            && let Some(terminal) = &mut self.agent_terminal
        {
            terminal.send_input(&bytes)?;
        }
        Ok(())
    }

    fn handle_agent_window_key(&mut self, key: TerminalKey) -> bool {
        match key.code {
            TerminalKeyCode::Char('h' | 'H' | 'w' | 'W') | TerminalKeyCode::Left => {
                self.agent_sidebar.focused = false;
                self.message = "editor window focused".to_owned();
            }
            TerminalKeyCode::Char('q' | 'Q' | 'c' | 'C') => {
                self.agent_sidebar.visible = false;
                self.agent_sidebar.focused = false;
                self.message = "Oh My Pi pane hidden; session remains alive".to_owned();
            }
            TerminalKeyCode::Char('j' | 'J' | 'k' | 'K' | 'l' | 'L')
            | TerminalKeyCode::Down
            | TerminalKeyCode::Up
            | TerminalKeyCode::Right => {
                self.message = "no window in that direction".to_owned();
            }
            TerminalKeyCode::Escape => self.message.clear(),
            _ => return false,
        }
        true
    }

    pub(super) fn send_agent_mouse_input(&mut self, event: &TerminalInput) -> Result<()> {
        let Some(local_event) = self.agent_local_mouse_event(event) else {
            return Ok(());
        };
        let Some(terminal) = &mut self.agent_terminal else {
            return Ok(());
        };
        if terminal.surface().accepts_sgr_mouse() {
            terminal.send_input(&terminal_mouse_bytes(&local_event))?;
        }
        Ok(())
    }

    pub(super) fn agent_local_mouse_event(&self, event: &TerminalInput) -> Option<TerminalInput> {
        let start = ViewportLayout::terminal_sidebar_column_for_size(
            self.viewport_columns,
            self.viewport_rows,
        )?
        .saturating_add(1);
        let local_column = |column: usize| column.saturating_sub(start);
        match event {
            TerminalInput::MouseClick { column, row } => Some(TerminalInput::MouseClick {
                column: local_column(*column),
                row: *row,
            }),
            TerminalInput::MouseDrag { column, row } => Some(TerminalInput::MouseDrag {
                column: local_column(*column),
                row: *row,
            }),
            TerminalInput::MouseRelease { column, row } => Some(TerminalInput::MouseRelease {
                column: local_column(*column),
                row: *row,
            }),
            TerminalInput::MouseScroll { lines, column, row } => Some(TerminalInput::MouseScroll {
                lines: *lines,
                column: local_column(*column),
                row: *row,
            }),
            TerminalInput::Key(_)
            | TerminalInput::Paste(_)
            | TerminalInput::Resized { .. }
            | TerminalInput::Ignored => None,
        }
    }

    pub(super) fn resize_agent_terminal(&mut self) {
        let (rows, columns) = self.agent_terminal_dimensions();
        let error = self
            .agent_terminal
            .as_mut()
            .and_then(|terminal| terminal.resize(rows, columns).err());
        if let Some(error) = error {
            self.show_error(format!("Oh My Pi resize: {error}"));
        }
    }

    fn agent_terminal_dimensions(&self) -> (u16, u16) {
        let inner_columns = ViewportLayout::terminal_sidebar_column_for_size(
            self.viewport_columns,
            self.viewport_rows,
        )
        .map_or(1, |column| {
            self.viewport_columns
                .saturating_sub(column.saturating_add(1))
        });
        (
            u16::try_from(self.viewport_rows).unwrap_or(u16::MAX).max(1),
            u16::try_from(inner_columns).unwrap_or(u16::MAX).max(1),
        )
    }
}
