use super::*;

impl App {
    pub(super) fn handle_input(&mut self, input: TerminalInput) -> Result<()> {
        self.foreground_frame_pending = true;
        if self.input_focus.is_terminal() {
            return self.handle_terminal_input(input);
        }
        if self.substitute_confirmation.is_some() {
            return self.handle_confirmation_input(input);
        }
        if self.agent_sidebar.visible && self.input_focus.is_agent() {
            return self.handle_agent_input(input);
        }
        if self.prompt.is_some() {
            return self.handle_prompt_input(input);
        }
        self.handle_editor_input(input)
    }

    fn handle_confirmation_input(&mut self, input: TerminalInput) -> Result<()> {
        let TerminalInput::Key(key) = input else {
            return Ok(());
        };
        self.handle_substitution_confirmation(key)
    }

    fn handle_prompt_input(&mut self, input: TerminalInput) -> Result<()> {
        match input {
            TerminalInput::Key(key) => self.handle_prompt_key(key),
            TerminalInput::Paste(text) => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.buffer.extend(text.chars().filter(|character| !character.is_control()));
                }
                self.update_prompt_picker()
            }
            TerminalInput::Mouse { action: MouseAction::Scroll(lines), column, row } => self.handle_mouse_scroll_at(lines, column, row),
            TerminalInput::Mouse { .. } | TerminalInput::Ignored | TerminalInput::Resized { .. } => Ok(()),
        }
    }

    fn handle_editor_input(&mut self, input: TerminalInput) -> Result<()> {
        match input {
            TerminalInput::Key(key) => self.handle_editor_key(key),
            TerminalInput::Paste(text) => {
                if matches!(self.active.editor.mode(), Mode::Insert | Mode::Replace) {
                    match self.active.editor.insert_text(&text) {
                        Ok(transaction) => self.after_transaction(transaction),
                        Err(error) => self.engine_error(error),
                    }
                }
                Ok(())
            }
            TerminalInput::Mouse { action: MouseAction::Scroll(lines), column, row } => self.handle_mouse_scroll_at(lines, column, row),
            // The application loop owns rendered geometry and handles clicks
            // through `handle_mouse_click` before generic input dispatch.
            TerminalInput::Mouse { .. } | TerminalInput::Ignored | TerminalInput::Resized { .. } => Ok(()),
        }
    }

    fn handle_mouse_scroll(&mut self, lines: isize) -> Result<()> {
        self.mouse_selection = None;
        self.ace_jump = None;
        if self.popup.as_ref().is_some_and(|popup| popup.cursor.is_some()) {
            self.move_popup_cursor_rows(lines);
        } else if self.prompt_kind_is_picker() {
            self.close_editor_popup();
            self.move_picker(lines);
        } else {
            self.close_editor_popup();
            self.scroll_view_line(lines.signum(), lines.unsigned_abs());
        }
        Ok(())
    }

    fn handle_mouse_scroll_at(&mut self, lines: isize, column: usize, row: usize) -> Result<()> {
        if self.agent_sidebar_at(column, ViewportLayout::terminal_sidebar_column_for_size(self.viewport_columns, self.viewport_rows)) {
            return self.send_agent_mouse_input(&TerminalInput::scroll(lines, column, row));
        }
        self.handle_mouse_scroll(lines)
    }

    fn agent_sidebar_at(&self, column: usize, start: Option<usize>) -> bool {
        self.agent_sidebar.visible && start.is_some_and(|start| column >= start)
    }

    pub(super) fn handle_mouse_click(&mut self, layout: &ViewportLayout, column: usize, row: usize) -> Result<()> {
        self.mouse_selection = None;
        if self.agent_sidebar_at(column, layout.terminal_sidebar_column()) {
            self.input_focus = InputFocus::Agent(AgentInputPrefix::None);
            self.popup = None;
            self.popup_deadline = None;
            return self.send_agent_mouse_input(&TerminalInput::click(column, row));
        }
        if self.input_focus.is_agent() {
            self.input_focus = InputFocus::Editor;
        }
        if self.popup.as_ref().is_some_and(|popup| popup.cursor.is_some()) {
            return Ok(());
        }
        self.close_editor_popup();
        if self.input_focus.is_terminal() || self.prompt.is_some() || self.completion.is_some() || self.debug_ui_visible {
            return Ok(());
        }
        let frames = buffer_frames(self, self.active.editor.frame());
        let Some(hit) = layout.hit_test_workspace(&self.views, &frames, column, row, 1) else {
            return Ok(());
        };
        self.views.focus_window_id(hit.window_id)?;
        self.activate_view_buffer()?;
        self.active.editor.set_cursor(hit.byte);
        self.views.active_window_mut().cursor_byte = hit.byte;
        self.mouse_selection = Some(MouseSelection { buffer_id: hit.buffer_id, anchor: hit.byte, dragged: false });
        self.ace_jump = None;
        self.normal_prefix = None;
        self.leader_keys = None;
        self.message.clear();
        Ok(())
    }

    pub(super) fn handle_mouse_drag(&mut self, layout: &ViewportLayout, column: usize, row: usize) -> Result<()> {
        if self.agent_sidebar_at(column, layout.terminal_sidebar_column()) {
            return self.send_agent_mouse_input(&TerminalInput::drag(column, row));
        }
        let Some(origin) = self.mouse_selection else {
            return Ok(());
        };
        if self.input_focus.is_terminal() || self.prompt.is_some() || self.popup.is_some() || self.completion.is_some() || self.debug_ui_visible {
            self.mouse_selection = None;
            return Ok(());
        }
        let frames = buffer_frames(self, self.active.editor.frame());
        let Some(hit) = layout.hit_test_workspace(&self.views, &frames, column, row, 1) else {
            return Ok(());
        };
        if hit.buffer_id != origin.buffer_id || hit.buffer_id != self.active.buffer_id {
            return Ok(());
        }
        self.active.editor.set_visual_selection(origin.anchor, hit.byte);
        if let Some(selection) = &mut self.mouse_selection {
            selection.dragged = true;
        }
        self.views.active_window_mut().cursor_byte = hit.byte;
        self.ace_jump = None;
        self.normal_prefix = None;
        self.leader_keys = None;
        self.message.clear();
        Ok(())
    }

    pub(super) fn handle_mouse_release(&mut self, layout: &ViewportLayout, column: usize, row: usize) -> Result<()> {
        if self.agent_sidebar_at(column, layout.terminal_sidebar_column()) {
            return self.send_agent_mouse_input(&TerminalInput::release(column, row));
        }
        if self.mouse_selection.is_some_and(|selection| selection.dragged) {
            self.handle_mouse_drag(layout, column, row)?;
        }
        self.mouse_selection = None;
        Ok(())
    }

    pub(super) fn take_clipboard_writes(&mut self) -> Vec<(char, Box<str>)> {
        let mut writes = self.active.editor.take_clipboard_writes();
        for buffer in &mut self.inactive {
            writes.extend(buffer.editor.take_clipboard_writes());
        }
        writes
    }

    pub(super) fn clipboard_register_for_paste(&self, input: &TerminalInput) -> Option<char> {
        let TerminalInput::Key(key) = input else {
            return None;
        };
        if self.prompt.is_some()
            || self.input_focus.is_terminal()
            || self.substitute_confirmation.is_some()
            || self.ace_jump.is_some()
            || self.leader_keys.is_some()
            || self.normal_prefix.is_some()
            || matches!(self.active.editor.mode(), Mode::Insert | Mode::Replace)
            || key.control()
            || key.alt()
            || key.super_key()
            || !matches!(key.code, TerminalKeyCode::Char('p' | 'P'))
        {
            return None;
        }
        match self.active.editor.pending_parse_state() {
            None | Some(ParseState::Count { .. }) => {}
            Some(ParseState::Register { .. }) if self.active.editor.pending_register_name().is_some() => {}
            Some(_) => return None,
        }
        match self.active.editor.pending_register_name() {
            Some(register @ ('+' | '*')) => Some(register),
            Some(_) => None,
            None => Some('+'),
        }
    }

    pub(super) fn restore_clipboard_register(&mut self, register: char, text: String) {
        self.active.editor.restore_register(register, text, false);
    }

    pub(super) fn handle_prompt_key(&mut self, key: TerminalKey) -> Result<()> {
        if self.prompt_kind_is_picker() && self.handle_picker_prompt_key(key)? {
            return Ok(());
        }
        match key.code {
            TerminalKeyCode::Escape => self.cancel_prompt()?,
            TerminalKeyCode::Backspace => self.backspace_prompt()?,
            TerminalKeyCode::Enter => self.submit_prompt()?,
            TerminalKeyCode::Up => self.navigate_prompt(-1),
            TerminalKeyCode::Down => self.navigate_prompt(1),
            TerminalKeyCode::Left if self.empty_file_browser_prompt() => self.browse_parent()?,
            TerminalKeyCode::Tab => self.complete_prompt(),
            TerminalKeyCode::Char('n' | 'N' | 'p' | 'P') if key.control() => self.complete_prompt(),
            TerminalKeyCode::Char(character) if !key.control() && !key.super_key() => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.buffer.push(character);
                }
                self.update_prompt_picker()?;
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_picker_prompt_key(&mut self, key: TerminalKey) -> Result<bool> {
        match (key.control(), key.code) {
            (_, TerminalKeyCode::PageUp) => self.move_picker(-10),
            (_, TerminalKeyCode::PageDown) => self.move_picker(10),
            (_, TerminalKeyCode::Right) => self.submit_prompt()?,
            (true, TerminalKeyCode::Char('u' | 'U')) => self.scroll_picker_preview(-4),
            (true, TerminalKeyCode::Char('d' | 'D')) => self.scroll_picker_preview(4),
            (true, TerminalKeyCode::Char('n' | 'N' | 'j' | 'J')) => self.move_picker(1),
            (true, TerminalKeyCode::Char('p' | 'P' | 'k' | 'K')) => self.move_picker(-1),
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn cancel_prompt(&mut self) -> Result<()> {
        if self.search_prompt_origin.is_some() {
            return self.cancel_search_prompt();
        }
        if self.prompt.as_ref().is_some_and(|prompt| prompt.kind == PromptKind::Picker(PickerSource::Grep)) {
            self.cancel_grep_picker();
        }
        self.prompt = None;
        self.message.clear();
        Ok(())
    }

    fn backspace_prompt(&mut self) -> Result<()> {
        if self.empty_file_browser_prompt() {
            return self.browse_parent();
        }
        if let Some(prompt) = &mut self.prompt {
            prompt.buffer.pop();
        }
        self.update_prompt_picker()
    }

    fn submit_prompt(&mut self) -> Result<()> {
        let prompt = self.prompt.take().ok_or_else(|| anyhow!("prompt vanished"))?;
        if prompt.kind == PromptKind::Picker(PickerSource::Grep) {
            self.cancel_grep_picker();
        }
        if let Err(error) = self.execute_prompt(prompt) {
            self.show_error(error);
        }
        Ok(())
    }

    fn navigate_prompt(&mut self, offset: isize) {
        if self.prompt_uses_picker_navigation() {
            self.move_picker(offset);
        } else {
            self.move_prompt_history(offset);
        }
    }

    fn prompt_uses_picker_navigation(&self) -> bool {
        self.prompt.as_ref().is_some_and(|prompt| prompt.kind.is_picker())
    }

    fn scroll_picker_preview(&mut self, offset: isize) {
        let last_line = self.picker_preview.lines().count().saturating_sub(1);
        self.picker_preview_scroll = self.picker_preview_scroll.saturating_add_signed(offset).min(last_line);
    }

    fn empty_file_browser_prompt(&self) -> bool {
        self.prompt.as_ref().is_some_and(|prompt| prompt.kind == PromptKind::Picker(PickerSource::Browser) && prompt.buffer.is_empty())
    }

    pub(super) fn prompt_kind_is_picker(&self) -> bool {
        self.prompt.as_ref().is_some_and(|prompt| prompt.kind.is_picker())
    }

    pub(super) fn begin_search_prompt(&mut self, kind: PromptKind) {
        let previous_search = self.active.editor.last_search().map(|(pattern, direction)| (pattern.into(), direction));
        self.search_prompt_origin =
            Some(SearchPromptOrigin { cursor: self.active.editor.primary_cursor(), previous_search, previous_highlight: self.search_highlight });
        self.prompt = Some(Prompt::new(kind));
        self.message.clear();
    }

    pub(super) fn synchronize_search(&mut self, pattern: &str, direction: SearchDirection, persist: bool) -> Result<()> {
        self.active.editor.restore_search(pattern, direction)?;
        for buffer in &mut self.inactive {
            buffer.editor.restore_search(pattern, direction)?;
        }
        self.last_search_direction = direction;
        self.search_highlight = true;
        let mut deltas = Vec::with_capacity(2);
        if persist {
            deltas.push(StateDelta::SearchPattern(pattern.to_owned().into()));
        }
        deltas.push(StateDelta::SearchDirection { backward: direction == SearchDirection::Backward });
        self.after_effect(None, deltas);
        Ok(())
    }

    pub(super) fn cancel_search_prompt(&mut self) -> Result<()> {
        if let Some(origin) = self.search_prompt_origin.take() {
            self.active.editor.set_cursor(origin.cursor);
            if let Some((pattern, direction)) = origin.previous_search {
                self.active.editor.restore_search(pattern, direction)?;
            } else {
                self.active.editor.clear_search();
            }
            self.search_highlight = origin.previous_highlight;
        }
        self.prompt = None;
        self.message.clear();
        Ok(())
    }

    pub(super) fn update_incremental_search(&mut self) -> Result<()> {
        let Some(prompt) = self.prompt.as_ref().filter(|prompt| prompt.kind.is_search()) else {
            return Ok(());
        };
        let Some(origin) = self.search_prompt_origin.as_ref() else {
            return Ok(());
        };
        let PromptKind::Search(direction) = prompt.kind else {
            return Ok(());
        };
        let query = prompt.buffer.clone();
        let cursor = origin.cursor;
        if query.is_empty() {
            self.active.editor.set_cursor(cursor);
            if let Some((pattern, direction)) = &origin.previous_search {
                self.active.editor.restore_search(pattern.clone(), *direction)?;
            } else {
                self.active.editor.clear_search();
            }
            self.search_highlight = origin.previous_highlight;
            self.message.clear();
            return Ok(());
        }
        let found = match self.active.editor.preview_search(&query, direction, cursor) {
            Ok(found) => found,
            Err(error) => {
                self.active.editor.set_cursor(cursor);
                let _ = self.active.editor.restore_search(query, direction);
                self.search_highlight = false;
                self.message = error.to_string();
                return Ok(());
            }
        };
        self.active.editor.restore_search(query.clone(), direction)?;
        self.search_highlight = true;
        if let Some(byte) = found {
            self.active.editor.set_cursor(byte);
            self.message = format!("{}{}", prompt.prefix(), query);
        } else {
            self.active.editor.set_cursor(cursor);
            self.message = format!("pattern not found: {query}");
        }
        Ok(())
    }

    pub(super) fn complete_prompt(&mut self) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        match prompt.kind {
            PromptKind::Command => {
                const EARLY_CUSTOM_COMMANDS: &[&str] = &["AvanteAsk", "AvanteChat", "Codex", "FormatToggle", "Catppuccin", "Git", "Gdiffsplit", "Gwrite"];
                const LATE_CUSTOM_COMMANDS: &[&str] = &["colorscheme", "setcolor"];
                if !prompt.buffer.contains(char::is_whitespace) {
                    let prefix = prompt.buffer.to_ascii_lowercase();
                    if let Some(command) = EARLY_CUSTOM_COMMANDS
                        .iter()
                        .chain(EX_COMMAND_COMPLETIONS)
                        .chain(LATE_CUSTOM_COMMANDS)
                        .find(|command| command.to_ascii_lowercase().starts_with(&prefix))
                    {
                        prompt.buffer = (*command).to_owned();
                    }
                    return;
                }
                let start = prompt.buffer.rfind(char::is_whitespace).map_or(0, |index| index + 1);
                let fragment = &prompt.buffer[start..];
                let candidate = complete_path(fragment);
                if let Some(candidate) = candidate {
                    prompt.buffer.replace_range(start.., &candidate);
                }
            }
            PromptKind::Search(_) => {
                let query = prompt.buffer.clone();
                let text = self.active.editor.contents();
                if let Some(word) = text
                    .split(|character: char| !character.is_alphanumeric() && character != '_')
                    .find(|word| word.len() > query.len() && word.starts_with(&query))
                {
                    prompt.buffer = word.to_owned();
                }
            }
            _ => {}
        }
    }

    pub(super) fn execute_prompt(&mut self, prompt: Prompt) -> Result<()> {
        match prompt.kind {
            PromptKind::Command => {
                self.after_effect(None, vec![StateDelta::CommandHistory(prompt.buffer.clone().into())]);
                self.execute_ex(&prompt.buffer)
            }
            PromptKind::Search(direction) => {
                let origin = self.search_prompt_origin.take();
                let pattern = if prompt.buffer.is_empty() {
                    origin
                        .as_ref()
                        .and_then(|origin| origin.previous_search.as_ref())
                        .map(|(pattern, _)| pattern.to_string())
                        .or_else(|| self.active.editor.last_search().map(|(pattern, _)| pattern.to_owned()))
                        .ok_or_else(|| anyhow!("no previous search pattern"))?
                } else {
                    prompt.buffer.clone()
                };
                if let Some(origin) = &origin {
                    self.active.editor.set_cursor(origin.cursor);
                }
                let found = match self.active.editor.search(&pattern, direction) {
                    Ok(found) => found,
                    Err(error) => {
                        if let Some(origin) = origin {
                            if let Some((pattern, direction)) = origin.previous_search {
                                let _ = self.active.editor.restore_search(pattern, direction);
                            } else {
                                self.active.editor.clear_search();
                            }
                            self.search_highlight = origin.previous_highlight;
                        }
                        return Err(error.into());
                    }
                };
                self.message = if found { format!("{}{pattern}", prompt.prefix()) } else { format!("pattern not found: {pattern}") };
                self.synchronize_search(&pattern, direction, !prompt.buffer.is_empty())?;
                Ok(())
            }
            PromptKind::Expression => {
                let value = evaluate_expression(&prompt.buffer, &self.expression_context())?;
                let text = value.to_editor_text();
                self.active.editor.set_register('=', text.clone(), false);
                self.after_effect(None, vec![StateDelta::Register { name: '=', text: text.clone().into(), linewise: false }]);
                self.message = format!("={text}");
                Ok(())
            }
            PromptKind::Picker(PickerSource::Files | PickerSource::Buffers | PickerSource::Recent) => {
                let path = self.picker_matches.get(self.picker_index).cloned().ok_or_else(|| anyhow!("no file matches {:?}", prompt.buffer))?;
                self.open_buffer(&path)
            }
            PromptKind::Picker(PickerSource::Browser) => {
                let path = self.picker_matches.get(self.picker_index).cloned().ok_or_else(|| anyhow!("no browser matches {:?}", prompt.buffer))?;
                if path.is_dir() { self.start_file_browser_at(&path) } else { self.open_buffer(&path) }
            }
            PromptKind::Picker(PickerSource::Grep) => self.open_selected_grep_result(&prompt.buffer),
            PromptKind::Picker(PickerSource::Jumps | PickerSource::Diagnostics) => self.open_selected_location(&prompt.buffer),
            PromptKind::Rename => self.rename_symbol(&prompt.buffer),
            PromptKind::ConditionalBreakpoint => {
                self.toggle_breakpoint(Some(prompt.buffer));
                Ok(())
            }
        }
    }

    pub(super) fn handle_editor_key(&mut self, key: TerminalKey) -> Result<()> {
        if self.ace_jump.is_some() {
            return self.handle_ace_jump_key(key);
        }
        // The which-key surface is represented as a popup, but its next key
        // belongs to the leader grammar rather than popup navigation.
        let before_completion: [EditorKeyHandler; 4] = [
            Self::handle_pending_leader,
            |app, key| Ok(app.handle_editor_popup_key(key)),
            |app, key| Ok(app.cancel_normal_input(key)),
            Self::handle_insert_completion_key,
        ];
        if first_consuming_handler(self, key, &before_completion)? {
            return Ok(());
        }
        self.clear_completion();
        let normal_handlers: [EditorKeyHandler; 3] = [Self::handle_normal_prefix, Self::handle_control_key, Self::handle_jump_key];
        if first_consuming_handler(self, key, &normal_handlers)? {
            return Ok(());
        }
        if self.tasks.is_document_blocked(self.active.document_id) && !is_navigation_key(key) {
            self.message = "document is waiting for a TaskCommand; Ctrl-C cancels".to_owned();
            return Ok(());
        }
        if self.handle_normal_special_key(key)? {
            return Ok(());
        }
        if let Some(event) = grammar_key(key) {
            self.dispatch_key(event);
        }
        Ok(())
    }

    pub(super) fn handle_editor_popup_key(&mut self, key: TerminalKey) -> bool {
        let Some(focused) = self.popup.as_ref().map(|popup| popup.cursor.is_some()) else {
            return false;
        };
        let plain_k = key.code == TerminalKeyCode::Char('K') && !key.control() && !key.alt() && !key.super_key();
        if !focused && plain_k && self.active.editor.mode() == Mode::Normal {
            if let Some(popup) = &mut self.popup {
                popup.cursor = Some((popup.scroll, 0));
            }
            self.popup_deadline = None;
            self.normal_prefix = None;
            self.leader_keys = None;
            self.leader_deadline = None;
            return true;
        }
        if !focused {
            let escape = key.code == TerminalKeyCode::Escape;
            self.close_editor_popup();
            if escape {
                self.normal_prefix = None;
                self.leader_keys = None;
                self.leader_deadline = None;
            }
            return escape;
        }
        if key.code == TerminalKeyCode::Escape || (key.code == TerminalKeyCode::Char('q') && !key.control() && !key.alt() && !key.super_key()) {
            self.close_editor_popup();
            self.normal_prefix = None;
            self.leader_keys = None;
            self.leader_deadline = None;
            return true;
        }
        self.navigate_focused_popup(key);
        true
    }

    fn close_editor_popup(&mut self) {
        if self.popup.take().is_none() {
            return;
        }
        self.popup_deadline = None;
    }

    fn navigate_focused_popup(&mut self, key: TerminalKey) {
        const POPUP_TAB_WIDTH: usize = 2;
        let terminal_width = self.viewport_columns;
        let visible_rows = self.viewport_rows.saturating_sub(3).max(1);
        let Some(popup) = &mut self.popup else {
            return;
        };
        let widths = popup.navigation_line_widths(terminal_width, POPUP_TAB_WIDTH);
        let maximum_row = widths.len().saturating_sub(1);
        let (mut row, mut column) = popup.cursor.unwrap_or((popup.scroll, 0));
        row = row.min(maximum_row);

        if key.alt() || key.super_key() {
            return;
        }
        let control_character = if key.control() {
            match key.code {
                TerminalKeyCode::Char(character) => Some(character.to_ascii_lowercase()),
                _ => None,
            }
        } else {
            None
        };
        match (key.code, control_character) {
            (TerminalKeyCode::Up | TerminalKeyCode::Char('k' | 'K'), None) => {
                row = row.saturating_sub(1);
            }
            (TerminalKeyCode::Down | TerminalKeyCode::Char('j' | 'J'), None) => {
                row = row.saturating_add(1).min(maximum_row);
            }
            (TerminalKeyCode::Left | TerminalKeyCode::Char('h' | 'H'), None) => {
                column = column.saturating_sub(1);
            }
            (TerminalKeyCode::Right | TerminalKeyCode::Char('l' | 'L'), None) => {
                column = column.saturating_add(1);
            }
            (TerminalKeyCode::PageUp, None) | (_, Some('b' | 'u')) => {
                let amount = if control_character == Some('u') { visible_rows.saturating_add(1) / 2 } else { visible_rows };
                row = row.saturating_sub(amount);
            }
            (TerminalKeyCode::PageDown, None) | (_, Some('d' | 'f')) => {
                let amount = if control_character == Some('d') { visible_rows.saturating_add(1) / 2 } else { visible_rows };
                row = row.saturating_add(amount).min(maximum_row);
            }
            (TerminalKeyCode::Char('g'), None) => row = 0,
            (TerminalKeyCode::Char('G'), None) => row = maximum_row,
            (TerminalKeyCode::Home | TerminalKeyCode::Char('0'), None) => column = 0,
            (TerminalKeyCode::End | TerminalKeyCode::Char('$'), None) => {
                column = widths[row].saturating_sub(1);
            }
            _ => return,
        }
        position_popup_cursor(popup, &widths, visible_rows, row, column);
    }

    fn move_popup_cursor_rows(&mut self, lines: isize) {
        const POPUP_TAB_WIDTH: usize = 2;
        let visible_rows = self.viewport_rows.saturating_sub(3).max(1);
        let Some(popup) = &mut self.popup else {
            return;
        };
        let widths = popup.navigation_line_widths(self.viewport_columns, POPUP_TAB_WIDTH);
        let maximum_row = widths.len().saturating_sub(1);
        let (row, column) = popup.cursor.unwrap_or((popup.scroll, 0));
        let row = if lines < 0 { row.saturating_sub(lines.unsigned_abs()) } else { row.saturating_add(lines.unsigned_abs()).min(maximum_row) };
        position_popup_cursor(popup, &widths, visible_rows, row, column);
    }

    pub(super) fn cancel_normal_input(&mut self, key: TerminalKey) -> bool {
        if self.active.editor.mode() != Mode::Normal || key.code != TerminalKeyCode::Escape {
            return false;
        }
        self.active.editor.cancel_pending();
        self.normal_prefix = None;
        self.leader_keys = None;
        self.leader_deadline = None;
        self.message.clear();
        true
    }

    pub(super) fn handle_insert_completion_key(&mut self, key: TerminalKey) -> Result<bool> {
        if self.active.editor.mode() != Mode::Insert {
            return Ok(false);
        }
        if key.code == TerminalKeyCode::Tab && !self.snippet_stops.is_empty() {
            self.move_snippet_stop(if key.shift() { -1 } else { 1 });
            return Ok(true);
        }
        if key.code == TerminalKeyCode::Enter && self.completion.is_some() && self.completion_selected {
            self.accept_completion()?;
            return Ok(true);
        }
        if !key.control() {
            return Ok(false);
        }
        match key.code {
            TerminalKeyCode::Char('n' | 'N' | 'p' | 'P') => {
                if self.completion.is_some() {
                    let direction = if matches!(key.code, TerminalKeyCode::Char('p' | 'P')) { -1 } else { 1 };
                    self.move_completion(direction);
                } else {
                    self.request_completion();
                }
            }
            TerminalKeyCode::Char(' ') => self.request_completion(),
            TerminalKeyCode::Char('e' | 'E') => {
                self.clear_completion();
                self.message = "completion cancelled".to_owned();
            }
            TerminalKeyCode::Char('b' | 'B' | 'f' | 'F') if self.completion.is_some() => {
                if matches!(key.code, TerminalKeyCode::Char('b' | 'B')) {
                    self.completion_documentation_scroll = self.completion_documentation_scroll.saturating_sub(4);
                } else {
                    self.completion_documentation_scroll =
                        self.completion_documentation_scroll.saturating_add(4).min(self.completion_documentation_lines().saturating_sub(1));
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(super) fn clear_completion(&mut self) {
        self.completion = None;
        self.completion_selected = false;
        self.completion_documentation_scroll = 0;
    }

    pub(super) fn handle_pending_leader(&mut self, key: TerminalKey) -> Result<bool> {
        if self.leader_keys.is_none() {
            return Ok(false);
        }
        if key.code == TerminalKeyCode::Escape {
            self.leader_keys = None;
            self.leader_deadline = None;
            self.message.clear();
            return Ok(true);
        }
        if !key.control()
            && !key.alt()
            && !key.super_key()
            && let TerminalKeyCode::Char(character) = key.code
        {
            self.handle_leader_character(character)?;
            return Ok(true);
        }
        self.leader_keys = None;
        self.leader_deadline = None;
        Ok(false)
    }

    pub(super) fn handle_normal_prefix(&mut self, key: TerminalKey) -> Result<bool> {
        if self.active.editor.mode() != Mode::Normal {
            return Ok(false);
        }
        let Some(prefix) = self.normal_prefix.take() else {
            return Ok(false);
        };
        if prefix == '\u{17}' {
            self.handle_window_prefix(key)?;
            return Ok(true);
        }
        if !key.control()
            && !key.alt()
            && !key.super_key()
            && let TerminalKeyCode::Char(character) = key.code
        {
            return self.handle_normal_prefix_pair(prefix, character, key);
        }
        self.dispatch_key(KeyEvent::character(prefix));
        Ok(false)
    }

    pub(super) fn handle_normal_prefix_pair(&mut self, prefix: char, character: char, key: TerminalKey) -> Result<bool> {
        match (prefix, character) {
            ('[', 'd') => self.move_diagnostic(-1)?,
            (']', 'd') => self.move_diagnostic(1)?,
            ('[', 'c') => self.move_git_hunk(-1)?,
            (']', 'c') => self.move_git_hunk(1)?,
            ('g', 'D') => self.lsp_location("textDocument/declaration")?,
            ('g', 'd') => self.lsp_location("textDocument/definition")?,
            ('g', 'i') => self.lsp_location("textDocument/implementation")?,
            ('g', 'r') => self.lsp_references()?,
            ('g', 'q') => self.format_text_width()?,
            ('g', ';' | ',') => {
                let older = character == ';';
                let count = self.take_normal_count().unwrap_or(1);
                if !self.navigate_change_count(older, count) {
                    self.message = if older { "at oldest change" } else { "at newest change" }.to_owned();
                }
            }
            ('z', 'z' | 't' | 'b') => {
                self.apply_z_count();
                let position = match character {
                    't' => ViewPosition::Top,
                    'b' => ViewPosition::Bottom,
                    _ => ViewPosition::Middle,
                };
                self.center_cursor_line(position);
            }
            ('Z', 'Z') => self.execute_ex("wq")?,
            ('Z', 'Q') => self.execute_ex("q!")?,
            _ => {
                self.dispatch_key(KeyEvent::character(prefix));
                if let Some(event) = grammar_key(key) {
                    self.dispatch_key(event);
                }
            }
        }
        Ok(true)
    }

    pub(super) fn handle_control_key(&mut self, key: TerminalKey) -> Result<bool> {
        if key.control() && matches!(key.code, TerminalKeyCode::Char('c' | 'C')) {
            if let Some(cancellation) = self.active_task.take() {
                cancellation.cancel();
                self.message = "cancelling task".to_owned();
            } else if matches!(self.active.editor.mode(), Mode::Insert | Mode::Replace) {
                self.dispatch_key(KeyEvent::plain(KeyCode::Escape));
            } else {
                self.active.editor.cancel_pending();
                self.message = "cancelled".to_owned();
            }
            return Ok(true);
        }
        if self.active.editor.mode() != Mode::Normal || !key.control() {
            return Ok(false);
        }
        let TerminalKeyCode::Char(character) = key.code else {
            return Ok(false);
        };
        let character = character.to_ascii_lowercase();
        if matches!(character, 'h' | 'j' | 'k' | 'l') {
            self.handle_control_window_key(character)?;
            return Ok(true);
        }
        if matches!(character, 'd' | 'u' | 'f' | 'b') {
            let full_page = matches!(character, 'f' | 'b');
            let direction = if matches!(character, 'u' | 'b') { -1 } else { 1 };
            let count = self.take_normal_count();
            self.scroll_page(direction, full_page, count);
            return Ok(true);
        }
        self.handle_remaining_control_key(character)
    }

    pub(super) fn handle_control_window_key(&mut self, character: char) -> Result<()> {
        let _ = self.take_normal_count();
        if character == 'k' && self.active_language_server().is_some() {
            return self.lsp_hover("textDocument/signatureHelp");
        }
        let direction = match character {
            'h' => WindowDirection::Left,
            'j' => WindowDirection::Down,
            'k' => WindowDirection::Up,
            _ => WindowDirection::Right,
        };
        self.views.focus_window(direction)?;
        self.activate_view_buffer()
    }

    pub(super) fn handle_remaining_control_key(&mut self, character: char) -> Result<bool> {
        match character {
            'w' => {
                self.normal_prefix = Some('\u{17}');
                self.message.clear();
                self.show_normal_prefix_hints('\u{17}');
            }
            'o' => {
                let count = self.take_normal_count().unwrap_or(1);
                if !self.navigate_jump_count(true, count)? {
                    self.message = "at oldest jump".to_owned();
                }
            }
            'e' | 'y' => {
                let count = self.take_normal_count().unwrap_or(1);
                self.scroll_view_line(if character == 'e' { 1 } else { -1 }, count);
            }
            'a' | 'x' => {
                let count = self.take_normal_count().unwrap_or(1);
                let direction = if character == 'a' { 1_i64 } else { -1_i64 };
                let delta = direction.saturating_mul(i64::try_from(count).unwrap_or(i64::MAX));
                let transaction = self.active.editor.adjust_number(delta)?;
                self.after_transaction(transaction);
            }
            'g' => {
                let _ = self.take_normal_count();
                self.show_file_info();
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(super) fn handle_jump_key(&mut self, key: TerminalKey) -> Result<bool> {
        if self.active.editor.mode() != Mode::Normal {
            return Ok(false);
        }
        if key.code == TerminalKeyCode::Tab {
            let count = self.take_normal_count().unwrap_or(1);
            if !self.navigate_jump_count(false, count)? {
                self.message = "at newest jump".to_owned();
            }
            return Ok(true);
        }
        if !matches!(key.code, TerminalKeyCode::PageUp | TerminalKeyCode::PageDown) {
            return Ok(false);
        }
        let count = self.take_normal_count();
        let direction = if key.code == TerminalKeyCode::PageUp { -1 } else { 1 };
        self.scroll_page(direction, true, count);
        Ok(true)
    }

    pub(super) fn handle_normal_special_key(&mut self, key: TerminalKey) -> Result<bool> {
        if key.control() && matches!(key.code, TerminalKeyCode::Char('s' | 'S')) {
            self.save(None)?;
            return Ok(true);
        }
        if self.active.editor.mode() != Mode::Normal || key.control() || key.alt() || key.super_key() {
            return Ok(false);
        }
        if key.code == TerminalKeyCode::Char(' ') {
            self.active.editor.cancel_pending();
            self.leader_keys = Some(String::new());
            self.leader_deadline = Some(Instant::now() + Duration::from_millis(500));
            self.message.clear();
            self.show_which_key("");
            return Ok(true);
        }
        if key.code == TerminalKeyCode::Char('=') && matches!(self.active.editor.pending_parse_state(), Some(ParseState::Register { .. })) {
            self.active.editor.cancel_pending();
            self.prompt = Some(Prompt::new(PromptKind::Expression));
            return Ok(true);
        }
        self.handle_normal_character(key)
    }

    pub(super) fn handle_normal_character(&mut self, key: TerminalKey) -> Result<bool> {
        match key.code {
            TerminalKeyCode::Char(prefix @ ('g' | '[' | ']' | 'z' | 'Z')) => {
                self.normal_prefix = Some(prefix);
                self.message.clear();
                self.show_normal_prefix_hints(prefix);
            }
            TerminalKeyCode::Char('K') => {
                let _ = self.take_normal_count();
                self.lsp_hover("textDocument/hover")?;
            }
            TerminalKeyCode::Char(character @ ('H' | 'M' | 'L')) => {
                let count = self.take_normal_count().unwrap_or(1);
                let position = match character {
                    'H' => ViewPosition::Top,
                    'L' => ViewPosition::Bottom,
                    _ => ViewPosition::Middle,
                };
                self.move_cursor_to_view(position, if character == 'M' { 1 } else { count });
            }
            TerminalKeyCode::Char(character @ ('*' | '#')) => {
                let count = self.take_normal_count().unwrap_or(1);
                self.search_word_under_cursor(character == '#', count);
            }
            TerminalKeyCode::Char(character @ (';' | ',')) => {
                let count = self.take_normal_count().unwrap_or(1);
                if !self.active.editor.repeat_find(character == ',', u32::try_from(count).unwrap_or(u32::MAX)) {
                    self.message = "no previous character search".to_owned();
                }
            }
            TerminalKeyCode::Char(':') => {
                self.prompt = Some(Prompt::new(PromptKind::Command));
                self.message.clear();
            }
            TerminalKeyCode::Char('/') => {
                self.begin_search_prompt(PromptKind::Search(SearchDirection::Forward));
            }
            TerminalKeyCode::Char('?') => {
                self.begin_search_prompt(PromptKind::Search(SearchDirection::Backward));
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(super) fn handle_leader_character(&mut self, character: char) -> Result<()> {
        let mut sequence = self.leader_keys.take().unwrap_or_default();
        self.leader_deadline = None;
        self.popup = None;
        self.popup_deadline = None;
        sequence.push(character);
        let exact = self.keymap.leader.get(sequence.as_str()).filter(|binding| self.binding_enabled(binding)).cloned();
        let has_longer = self
            .keymap
            .leader
            .iter()
            .any(|(candidate, binding)| candidate.len() > sequence.len() && candidate.starts_with(sequence.as_str()) && self.binding_enabled(binding));
        if has_longer {
            self.leader_keys = Some(sequence.clone());
            self.leader_deadline = Some(Instant::now() + Duration::from_millis(500));
            self.message.clear();
            self.show_which_key(&sequence);
        } else if let Some(binding) = exact {
            self.execute_runtime_command(&binding.invocation)?;
        } else {
            self.message = format!("no mapping for <Space>{sequence}");
        }
        Ok(())
    }

    pub(super) fn binding_enabled(&self, binding: &RuntimeBinding) -> bool {
        let Some(condition) = &binding.when else {
            return true;
        };
        let class = self.active.class.name();
        let language = language_bundle(self.active.document.presentation_path()).language_id;
        let context = ExpressionContext::new()
            .with("language", Value::String(language.into()))
            .with("remote", Value::Bool(false))
            .with("os", Value::String("macos".to_owned()))
            .with("selection.nonempty", Value::Bool(!self.active.editor.selection_byte_range().is_empty()))
            .with("lsp.available", Value::Bool(self.lsp_ready_for_active()))
            .with("document.class", Value::String(class.to_owned()))
            .with("workspace.trusted", Value::Bool(false));
        matches!(evaluate_expression(condition, &context), Ok(Value::Bool(true)))
    }

    pub(super) fn execute_runtime_command(&mut self, invocation: &CommandInvocation) -> Result<()> {
        let command = NATIVE_COMMANDS
            .iter()
            .find(|command| command.name == invocation.command.as_ref())
            .ok_or_else(|| anyhow!("validated command {} has no runtime implementation", invocation.command))?;
        (command.execute)(self)
    }

    pub(super) fn start_grep_word_picker(&mut self) -> Result<()> {
        let word = self.word_under_cursor().unwrap_or_default();
        if word.is_empty() {
            self.message = "no word under cursor".to_owned();
            Ok(())
        } else {
            self.start_grep_picker(&word)
        }
    }

    pub(super) fn toggle_debug_ui(&mut self) {
        self.debug_ui_visible = !self.debug_ui_visible;
        let state = if self.debug_ui_visible { "open" } else { "closed" };
        let breakpoints = self.breakpoints.values().map(BTreeMap::len).sum::<usize>();
        self.message = format!("debug UI {state} · {breakpoints} breakpoint(s)");
    }

    pub(super) fn open_conditional_breakpoint_prompt(&mut self) {
        self.prompt = Some(Prompt::new(PromptKind::ConditionalBreakpoint));
        self.message.clear();
    }

    pub(super) fn open_rename_prompt(&mut self) {
        self.prompt = Some(Prompt::new(PromptKind::Rename));
        self.message.clear();
    }
}

type EditorKeyHandler = fn(&mut App, TerminalKey) -> Result<bool>;

fn first_consuming_handler(app: &mut App, key: TerminalKey, handlers: &[EditorKeyHandler]) -> Result<bool> {
    for handler in handlers {
        if handler(app, key)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn position_popup_cursor(popup: &mut TextPopup, line_widths: &[usize], visible_rows: usize, row: usize, column: usize) {
    let maximum_row = line_widths.len().saturating_sub(1);
    let row = row.min(maximum_row);
    let column = column.min(line_widths.get(row).copied().unwrap_or_default().saturating_sub(1));
    if row < popup.scroll {
        popup.scroll = row;
    } else if row >= popup.scroll.saturating_add(visible_rows) {
        popup.scroll = row.saturating_add(1).saturating_sub(visible_rows);
    }
    popup.scroll = popup.scroll.min(line_widths.len().saturating_sub(visible_rows));
    popup.cursor = Some((row, column));
}
