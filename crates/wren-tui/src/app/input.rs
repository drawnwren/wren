use super::*;

impl App {
    pub(super) fn handle_input(&mut self, input: TerminalInput) -> Result<()> {
        if self.terminal_focused {
            return self.handle_terminal_input(input);
        }
        if self.substitute_confirmation.is_some() {
            return match input {
                TerminalInput::Key(key) => self.handle_substitution_confirmation(key),
                TerminalInput::Paste(_)
                | TerminalInput::MouseScroll { .. }
                | TerminalInput::MouseClick { .. }
                | TerminalInput::MouseDrag { .. }
                | TerminalInput::MouseRelease { .. }
                | TerminalInput::Ignored
                | TerminalInput::Resized { .. } => Ok(()),
            };
        }
        match input {
            TerminalInput::Key(key) if self.prompt.is_some() => self.handle_prompt_key(key),
            TerminalInput::Paste(text) if self.prompt.is_some() => {
                if let Some(prompt) = &mut self.prompt {
                    prompt
                        .buffer
                        .extend(text.chars().filter(|character| !character.is_control()));
                }
                self.update_prompt_picker()
            }
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
            TerminalInput::MouseScroll { lines, .. } => {
                self.mouse_selection = None;
                self.ace_jump = None;
                if let Some(popup) = &mut self.popup {
                    if lines < 0 {
                        popup.scroll = popup.scroll.saturating_sub(lines.unsigned_abs());
                    } else {
                        popup.scroll = popup
                            .scroll
                            .saturating_add(lines.unsigned_abs())
                            .min(popup.text.lines().count().saturating_sub(1));
                    }
                } else if self.prompt_kind_is_picker() {
                    self.move_picker(lines);
                } else {
                    self.scroll_view_line(lines.signum(), lines.unsigned_abs());
                }
                Ok(())
            }
            // The application loop owns rendered geometry and handles clicks
            // through `handle_mouse_click` before generic input dispatch.
            TerminalInput::MouseClick { .. }
            | TerminalInput::MouseDrag { .. }
            | TerminalInput::MouseRelease { .. } => Ok(()),
            TerminalInput::Ignored | TerminalInput::Resized { .. } => Ok(()),
        }
    }

    pub(super) fn handle_mouse_click(
        &mut self,
        layout: &ViewportLayout,
        column: usize,
        row: usize,
    ) -> Result<()> {
        self.mouse_selection = None;
        if self.terminal_focused
            || self.prompt.is_some()
            || self.popup.is_some()
            || self.completion.is_some()
            || self.debug_ui_visible
        {
            return Ok(());
        }
        let mut frames = Vec::with_capacity(self.inactive.len() + 1);
        frames.push((self.active.buffer_id, self.active.editor.frame()));
        frames.extend(
            self.inactive
                .iter()
                .map(|buffer| (buffer.buffer_id, buffer.editor.frame())),
        );
        let Some(hit) = layout.hit_test_workspace(&self.views, &frames, column, row, 1) else {
            return Ok(());
        };
        self.views.focus_window_id(hit.window_id)?;
        self.activate_view_buffer()?;
        self.active.editor.set_cursor(hit.byte);
        self.views.active_window_mut().cursor_byte = hit.byte;
        self.mouse_selection = Some(MouseSelection {
            buffer_id: hit.buffer_id,
            anchor: hit.byte,
            dragged: false,
        });
        self.ace_jump = None;
        self.normal_prefix = None;
        self.leader_keys = None;
        self.message.clear();
        Ok(())
    }

    pub(super) fn handle_mouse_drag(
        &mut self,
        layout: &ViewportLayout,
        column: usize,
        row: usize,
    ) -> Result<()> {
        let Some(origin) = self.mouse_selection else {
            return Ok(());
        };
        if self.terminal_focused
            || self.prompt.is_some()
            || self.popup.is_some()
            || self.completion.is_some()
            || self.debug_ui_visible
        {
            self.mouse_selection = None;
            return Ok(());
        }
        let mut frames = Vec::with_capacity(self.inactive.len() + 1);
        frames.push((self.active.buffer_id, self.active.editor.frame()));
        frames.extend(
            self.inactive
                .iter()
                .map(|buffer| (buffer.buffer_id, buffer.editor.frame())),
        );
        let Some(hit) = layout.hit_test_workspace(&self.views, &frames, column, row, 1) else {
            return Ok(());
        };
        if hit.buffer_id != origin.buffer_id || hit.buffer_id != self.active.buffer_id {
            return Ok(());
        }
        self.active
            .editor
            .set_visual_selection(origin.anchor, hit.byte);
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

    pub(super) fn handle_mouse_release(
        &mut self,
        layout: &ViewportLayout,
        column: usize,
        row: usize,
    ) -> Result<()> {
        if self
            .mouse_selection
            .is_some_and(|selection| selection.dragged)
        {
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
            || self.terminal_focused
            || self.substitute_confirmation.is_some()
            || self.ace_jump.is_some()
            || self.leader_keys.is_some()
            || self.normal_prefix.is_some()
            || matches!(self.active.editor.mode(), Mode::Insert | Mode::Replace)
            || key.control
            || key.alt
            || key.super_key
            || !matches!(key.code, TerminalKeyCode::Char('p' | 'P'))
        {
            return None;
        }
        match self.active.editor.pending_parse_state() {
            None | Some(ParseState::Count { .. }) => {}
            Some(ParseState::Register { .. })
                if self.active.editor.pending_register_name().is_some() => {}
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
        match key.code {
            TerminalKeyCode::Escape => {
                if self.search_prompt_origin.is_some() {
                    self.cancel_search_prompt()?;
                } else {
                    self.prompt = None;
                    self.message.clear();
                }
            }
            TerminalKeyCode::Backspace => {
                if self.prompt.as_ref().is_some_and(|prompt| {
                    prompt.kind == PromptKind::FileBrowser && prompt.buffer.is_empty()
                }) {
                    self.browse_parent()?;
                    return Ok(());
                }
                if let Some(prompt) = &mut self.prompt {
                    prompt.buffer.pop();
                }
                self.update_prompt_picker()?;
            }
            TerminalKeyCode::Enter => {
                let prompt = self
                    .prompt
                    .take()
                    .ok_or_else(|| anyhow!("prompt vanished"))?;
                if let Err(error) = self.execute_prompt(prompt) {
                    self.show_error(error);
                }
            }
            TerminalKeyCode::Up => {
                if self.prompt.as_ref().is_some_and(|prompt| {
                    matches!(
                        prompt.kind,
                        PromptKind::FilePicker
                            | PromptKind::FileBrowser
                            | PromptKind::Grep
                            | PromptKind::Location
                    )
                }) {
                    self.move_picker(-1);
                } else {
                    self.move_prompt_history(-1);
                }
            }
            TerminalKeyCode::Down => {
                if self.prompt.as_ref().is_some_and(|prompt| {
                    matches!(
                        prompt.kind,
                        PromptKind::FilePicker
                            | PromptKind::FileBrowser
                            | PromptKind::Grep
                            | PromptKind::Location
                    )
                }) {
                    self.move_picker(1);
                } else {
                    self.move_prompt_history(1);
                }
            }
            TerminalKeyCode::PageUp if self.prompt_kind_is_picker() => self.move_picker(-10),
            TerminalKeyCode::PageDown if self.prompt_kind_is_picker() => self.move_picker(10),
            TerminalKeyCode::Char('u' | 'U') if key.control && self.prompt_kind_is_picker() => {
                self.picker_preview_scroll = self.picker_preview_scroll.saturating_sub(4);
            }
            TerminalKeyCode::Char('d' | 'D') if key.control && self.prompt_kind_is_picker() => {
                self.picker_preview_scroll = self
                    .picker_preview_scroll
                    .saturating_add(4)
                    .min(self.picker_preview.lines().count().saturating_sub(1));
            }
            TerminalKeyCode::Char('n' | 'N' | 'j' | 'J')
                if key.control && self.prompt_kind_is_picker() =>
            {
                self.move_picker(1)
            }
            TerminalKeyCode::Char('p' | 'P' | 'k' | 'K')
                if key.control && self.prompt_kind_is_picker() =>
            {
                self.move_picker(-1)
            }
            TerminalKeyCode::Left
                if self.prompt.as_ref().is_some_and(|prompt| {
                    prompt.kind == PromptKind::FileBrowser && prompt.buffer.is_empty()
                }) =>
            {
                self.browse_parent()?
            }
            TerminalKeyCode::Right if self.prompt_kind_is_picker() => {
                let prompt = self
                    .prompt
                    .take()
                    .ok_or_else(|| anyhow!("prompt vanished"))?;
                if let Err(error) = self.execute_prompt(prompt) {
                    self.show_error(error);
                }
            }
            TerminalKeyCode::Tab => self.complete_prompt(),
            TerminalKeyCode::Char('n' | 'N' | 'p' | 'P') if key.control => self.complete_prompt(),
            TerminalKeyCode::Char(character) if !key.control && !key.super_key => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.buffer.push(character);
                }
                self.update_prompt_picker()?;
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn prompt_kind_is_picker(&self) -> bool {
        self.prompt
            .as_ref()
            .is_some_and(|prompt| prompt.kind.is_picker())
    }

    pub(super) fn begin_search_prompt(&mut self, kind: PromptKind) {
        let previous_search = self
            .active
            .editor
            .last_search()
            .map(|(pattern, direction)| (pattern.into(), direction));
        self.search_prompt_origin = Some(SearchPromptOrigin {
            cursor: self.active.editor.primary_cursor(),
            previous_search,
            previous_highlight: self.search_highlight,
        });
        self.prompt = Some(Prompt::new(kind));
        self.message.clear();
    }

    pub(super) fn synchronize_search(
        &mut self,
        pattern: &str,
        direction: SearchDirection,
        persist: bool,
    ) -> Result<()> {
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
        deltas.push(StateDelta::SearchDirection {
            backward: direction == SearchDirection::Backward,
        });
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
        let Some(prompt) = self.prompt.as_ref().filter(|prompt| {
            matches!(
                prompt.kind,
                PromptKind::SearchForward | PromptKind::SearchBackward
            )
        }) else {
            return Ok(());
        };
        let Some(origin) = self.search_prompt_origin.as_ref() else {
            return Ok(());
        };
        let direction = if prompt.kind == PromptKind::SearchForward {
            SearchDirection::Forward
        } else {
            SearchDirection::Backward
        };
        let query = prompt.buffer.clone();
        let cursor = origin.cursor;
        if query.is_empty() {
            self.active.editor.set_cursor(cursor);
            if let Some((pattern, direction)) = &origin.previous_search {
                self.active
                    .editor
                    .restore_search(pattern.clone(), *direction)?;
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
        self.active
            .editor
            .restore_search(query.clone(), direction)?;
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
                const COMMANDS: &[&str] = &[
                    "AvanteAsk",
                    "AvanteChat",
                    "Codex",
                    "FormatToggle",
                    "Catppuccin",
                    "Git",
                    "Gdiffsplit",
                    "Gwrite",
                    "bdelete",
                    "buffer",
                    "cdo",
                    "close",
                    "debuglog",
                    "edit",
                    "find",
                    "format",
                    "grep",
                    "help",
                    "make",
                    "marks",
                    "messages",
                    "nohlsearch",
                    "normal",
                    "quit",
                    "registers",
                    "redo",
                    "split",
                    "tabnew",
                    "terminal",
                    "colorscheme",
                    "setcolor",
                    "undo",
                    "vsplit",
                    "write",
                    "wq",
                ];
                if !prompt.buffer.contains(char::is_whitespace) {
                    let prefix = prompt.buffer.to_ascii_lowercase();
                    if let Some(command) = COMMANDS
                        .iter()
                        .find(|command| command.to_ascii_lowercase().starts_with(&prefix))
                    {
                        prompt.buffer = (*command).to_owned();
                    }
                    return;
                }
                let start = prompt
                    .buffer
                    .rfind(char::is_whitespace)
                    .map_or(0, |index| index + 1);
                let fragment = &prompt.buffer[start..];
                let candidate = complete_path(fragment);
                if let Some(candidate) = candidate {
                    prompt.buffer.replace_range(start.., &candidate);
                }
            }
            PromptKind::SearchForward | PromptKind::SearchBackward => {
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
                self.after_effect(
                    None,
                    vec![StateDelta::CommandHistory(prompt.buffer.clone().into())],
                );
                self.execute_ex(&prompt.buffer)
            }
            PromptKind::SearchForward | PromptKind::SearchBackward => {
                let direction = if prompt.kind == PromptKind::SearchForward {
                    SearchDirection::Forward
                } else {
                    SearchDirection::Backward
                };
                let origin = self.search_prompt_origin.take();
                let pattern = if prompt.buffer.is_empty() {
                    origin
                        .as_ref()
                        .and_then(|origin| origin.previous_search.as_ref())
                        .map(|(pattern, _)| pattern.to_string())
                        .or_else(|| {
                            self.active
                                .editor
                                .last_search()
                                .map(|(pattern, _)| pattern.to_owned())
                        })
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
                self.message = if found {
                    format!("{}{pattern}", prompt.prefix())
                } else {
                    format!("pattern not found: {pattern}")
                };
                self.synchronize_search(&pattern, direction, !prompt.buffer.is_empty())?;
                Ok(())
            }
            PromptKind::Expression => {
                let value = evaluate_expression(&prompt.buffer, &self.expression_context())?;
                let text = value.to_editor_text();
                self.active.editor.set_register('=', text.clone(), false);
                self.after_effect(
                    None,
                    vec![StateDelta::Register {
                        name: '=',
                        text: text.clone().into(),
                        linewise: false,
                    }],
                );
                self.message = format!("={text}");
                Ok(())
            }
            PromptKind::FilePicker => {
                let path = self
                    .picker_matches
                    .get(self.picker_index)
                    .cloned()
                    .ok_or_else(|| anyhow!("no file matches {:?}", prompt.buffer))?;
                self.open_buffer(&path)
            }
            PromptKind::FileBrowser => {
                let path = self
                    .picker_matches
                    .get(self.picker_index)
                    .cloned()
                    .ok_or_else(|| anyhow!("no browser matches {:?}", prompt.buffer))?;
                if path.is_dir() {
                    self.start_file_browser_at(&path)
                } else {
                    self.open_buffer(&path)
                }
            }
            PromptKind::Grep => self.open_selected_grep_result(&prompt.buffer),
            PromptKind::Location => self.open_selected_location(&prompt.buffer),
            PromptKind::Rename => self.rename_symbol(&prompt.buffer),
            PromptKind::ConditionalBreakpoint => {
                self.toggle_breakpoint(Some(prompt.buffer));
                Ok(())
            }
            PromptKind::Ai => self.start_ai_task(&prompt.buffer),
        }
    }

    pub(super) fn handle_editor_key(&mut self, key: TerminalKey) -> Result<()> {
        if self.ace_jump.is_some() {
            return self.handle_ace_jump_key(key);
        }
        if self.dismiss_editor_popup(key)
            || self.cancel_normal_input(key)
            || self.handle_insert_completion_key(key)?
        {
            return Ok(());
        }
        self.clear_completion();
        if self.handle_pending_leader(key)? || self.handle_normal_prefix(key)? {
            return Ok(());
        }
        if self.handle_control_key(key)? || self.handle_jump_key(key)? {
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

    pub(super) fn dismiss_editor_popup(&mut self, key: TerminalKey) -> bool {
        let dismisses_popup = key.code == TerminalKeyCode::Escape
            || (key.code == TerminalKeyCode::Char('K')
                && !key.control
                && !key.alt
                && !key.super_key);
        if !dismisses_popup || self.popup.take().is_none() {
            return false;
        }
        self.popup_deadline = None;
        self.leader_keys = None;
        self.leader_deadline = None;
        true
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
            self.move_snippet_stop(if key.shift { -1 } else { 1 });
            return Ok(true);
        }
        if key.code == TerminalKeyCode::Enter
            && self.completion.is_some()
            && self.completion_selected
        {
            self.accept_completion()?;
            return Ok(true);
        }
        if !key.control {
            return Ok(false);
        }
        match key.code {
            TerminalKeyCode::Char('n' | 'N' | 'p' | 'P') => {
                if self.completion.is_some() {
                    let direction = if matches!(key.code, TerminalKeyCode::Char('p' | 'P')) {
                        -1
                    } else {
                        1
                    };
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
                    self.completion_documentation_scroll =
                        self.completion_documentation_scroll.saturating_sub(4);
                } else {
                    self.completion_documentation_scroll = self
                        .completion_documentation_scroll
                        .saturating_add(4)
                        .min(self.completion_documentation_lines().saturating_sub(1));
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
        if !key.control
            && !key.alt
            && !key.super_key
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
        if !key.control
            && !key.alt
            && !key.super_key
            && let TerminalKeyCode::Char(character) = key.code
        {
            return self.handle_normal_prefix_pair(prefix, character, key);
        }
        self.dispatch_key(KeyEvent::character(prefix));
        Ok(false)
    }

    pub(super) fn handle_normal_prefix_pair(
        &mut self,
        prefix: char,
        character: char,
        key: TerminalKey,
    ) -> Result<bool> {
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
                    self.message = if older {
                        "at oldest change"
                    } else {
                        "at newest change"
                    }
                    .to_owned();
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
        if key.control && matches!(key.code, TerminalKeyCode::Char('c' | 'C')) {
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
        if self.active.editor.mode() != Mode::Normal || !key.control {
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
            let direction = if matches!(character, 'u' | 'b') {
                -1
            } else {
                1
            };
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
                self.message =
                    "window: h/j/k/l focus · s/v split · c close · o only · w next".to_owned();
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
        if !matches!(
            key.code,
            TerminalKeyCode::PageUp | TerminalKeyCode::PageDown
        ) {
            return Ok(false);
        }
        let count = self.take_normal_count();
        let direction = if key.code == TerminalKeyCode::PageUp {
            -1
        } else {
            1
        };
        self.scroll_page(direction, true, count);
        Ok(true)
    }

    pub(super) fn handle_normal_special_key(&mut self, key: TerminalKey) -> Result<bool> {
        if key.control && matches!(key.code, TerminalKeyCode::Char('s' | 'S')) {
            self.save(None)?;
            return Ok(true);
        }
        if self.active.editor.mode() != Mode::Normal || key.control || key.alt || key.super_key {
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
        if key.code == TerminalKeyCode::Char('=')
            && matches!(
                self.active.editor.pending_parse_state(),
                Some(ParseState::Register { .. })
            )
        {
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
                if !self
                    .active
                    .editor
                    .repeat_find(character == ',', u32::try_from(count).unwrap_or(u32::MAX))
                {
                    self.message = "no previous character search".to_owned();
                }
            }
            TerminalKeyCode::Char(':') => {
                self.prompt = Some(Prompt::new(PromptKind::Command));
                self.message.clear();
            }
            TerminalKeyCode::Char('/') => self.begin_search_prompt(PromptKind::SearchForward),
            TerminalKeyCode::Char('?') => self.begin_search_prompt(PromptKind::SearchBackward),
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
        let exact = self
            .keymap
            .leader
            .get(sequence.as_str())
            .filter(|binding| self.binding_enabled(binding))
            .cloned();
        let has_longer = self.keymap.leader.iter().any(|(candidate, binding)| {
            candidate.len() > sequence.len()
                && candidate.starts_with(sequence.as_str())
                && self.binding_enabled(binding)
        });
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
        let class = match self.active.class {
            DocumentClass::Normal => "normal",
            DocumentClass::Large => "large",
            DocumentClass::Pathological => "pathological",
        };
        let language = language_bundle(self.active.document.presentation_path()).language_id;
        let context = ExpressionContext::new()
            .with("language", Value::String(language.into()))
            .with("remote", Value::Bool(false))
            .with("os", Value::String(env::consts::OS.to_owned()))
            .with(
                "selection.nonempty",
                Value::Bool(!self.active.editor.selection_byte_range().is_empty()),
            )
            .with("lsp.available", Value::Bool(self.lsp_ready_for_active()))
            .with("document.class", Value::String(class.to_owned()))
            .with("workspace.trusted", Value::Bool(false));
        matches!(
            evaluate_expression(condition, &context),
            Ok(Value::Bool(true))
        )
    }

    pub(super) fn execute_runtime_command(&mut self, invocation: &CommandInvocation) -> Result<()> {
        match invocation.command.as_ref() {
            "selection.line" => self.dispatch_key(KeyEvent::character('V')),
            "editor.quit" => self.execute_ex("q")?,
            "file.write" => self.save(None)?,
            "search.clear" => {
                self.search_highlight = false;
                self.message.clear();
            }
            "picker.buffers" => self.start_buffer_picker()?,
            "jump.ace" => self.start_ace_jump(),
            "format.document" => self.format_active_language()?,
            "picker.files" => self.start_file_picker("")?,
            "picker.browser" => self.start_file_browser()?,
            "picker.resume" => self.resume_picker()?,
            "picker.recent" => self.start_recent_picker()?,
            "picker.grep" => self.start_grep_picker("")?,
            "picker.grep_word" => {
                let word = self.word_under_cursor().unwrap_or_default();
                if word.is_empty() {
                    self.message = "no word under cursor".to_owned();
                } else {
                    self.start_grep_picker(&word)?;
                }
            }
            "picker.jumplist" => self.start_jumplist_picker()?,
            "picker.diagnostics" => self.start_diagnostic_picker()?,
            "diagnostic.show" => self.show_cursor_diagnostic()?,
            "debug.toggle" => {
                self.debug_ui_visible = !self.debug_ui_visible;
                self.message = format!(
                    "debug UI {} · {} breakpoint(s)",
                    if self.debug_ui_visible {
                        "open"
                    } else {
                        "closed"
                    },
                    self.breakpoints.values().map(BTreeMap::len).sum::<usize>()
                );
            }
            "debug.breakpoint" => self.toggle_breakpoint(None),
            "debug.conditional_breakpoint" => {
                self.prompt = Some(Prompt::new(PromptKind::ConditionalBreakpoint));
                self.message.clear();
            }
            "debug.repl" => self.open_debug_repl()?,
            "debug.continue" => self.run_debug_action("dc")?,
            "debug.step_into" => self.run_debug_action("ds")?,
            "debug.step_over" => self.run_debug_action("dn")?,
            "debug.step_out" => self.run_debug_action("do")?,
            "debug.restart" => self.run_debug_action("dr")?,
            "git.stage_hunk" => self.git_stage_hunk()?,
            "git.reset_hunk" => self.git_reset_hunk()?,
            "git.stage_buffer" => self.git_stage_buffer()?,
            "git.undo_stage" => self.git_undo_stage_hunk()?,
            "git.preview_hunk" => self.git_preview_hunk()?,
            "git.blame_line" => self.git_blame_line()?,
            "git.diff_index" => self.git_diff_index()?,
            "lsp.rename" => {
                self.prompt = Some(Prompt::new(PromptKind::Rename));
                self.message.clear();
            }
            "lsp.code_action" => self.lsp_code_action()?,
            "lsp.type_definition" => self.lsp_location("textDocument/typeDefinition")?,
            "workspace.add_folder" => {
                self.lsp_workspace_folder("workspace/didChangeWorkspaceFolders", true)?;
            }
            "workspace.remove_folder" => {
                self.lsp_workspace_folder("workspace/didChangeWorkspaceFolders", false)?;
            }
            "workspace.list_folders" => self.list_workspace_folders(),
            "haskell.hoogle" => self.open_hoogle()?,
            "haskell.signature" => self.hoogle_signature()?,
            "haskell.code_lens" => self.lsp_code_lens()?,
            "haskell.repl_package" => self.open_haskell_repl(true)?,
            "haskell.repl_file" => self.open_haskell_repl(false)?,
            "haskell.repl_quit" => self.quit_repl()?,
            "repl.evaluate" => self.evaluate_in_repl()?,
            command => bail!("validated command {command} has no runtime implementation"),
        }
        Ok(())
    }
}
