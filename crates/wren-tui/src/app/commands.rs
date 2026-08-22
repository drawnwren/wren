use super::*;

struct CustomExPlugin {
    names: &'static [&'static str],
    execute: for<'a> fn(&mut App, std::str::SplitWhitespace<'a>) -> Result<()>,
}

const CUSTOM_EX_PLUGINS: &[CustomExPlugin] = &[
    CustomExPlugin { names: &["FormatToggle"], execute: |app, _| app.toggle_global_format() },
    CustomExPlugin { names: &["FormatToggle!"], execute: |app, _| app.toggle_buffer_format() },
    CustomExPlugin { names: &["colorscheme", "Catppuccin"], execute: App::set_colorscheme },
    CustomExPlugin { names: &["setcolor"], execute: App::set_theme_color },
    CustomExPlugin { names: &["Git"], execute: App::open_git },
    CustomExPlugin { names: &["Gwrite"], execute: |app, _| app.git_stage_buffer() },
    CustomExPlugin { names: &["Gdiffsplit"], execute: |app, _| app.git_diff_index() },
    CustomExPlugin { names: &["AvanteToggle"], execute: |app, _| app.toggle_agent_sidebar() },
    CustomExPlugin { names: &["Codex", "AvanteChat", "AvanteAsk"], execute: App::run_ai_prompt },
    CustomExPlugin { names: &["RustLsp"], execute: App::run_rust_lsp_command },
];

impl App {
    pub(super) fn execute_ex(&mut self, input: &str) -> Result<()> {
        let input = input.trim();
        if input.is_empty() {
            return Ok(());
        }
        if self.execute_custom_ex(input)? {
            return Ok(());
        }
        self.execute_ex_command(parse_ex(input)?)
    }

    fn execute_custom_ex(&mut self, input: &str) -> Result<bool> {
        let mut words = input.split_whitespace();
        let Some(name) = words.next() else {
            return Ok(false);
        };
        let Some(plugin) = CUSTOM_EX_PLUGINS.iter().find(|plugin| plugin.names.contains(&name)) else {
            return Ok(false);
        };
        (plugin.execute)(self, words)?;
        Ok(true)
    }

    fn toggle_global_format(&mut self) -> Result<()> {
        self.format_on_save = !self.format_on_save;
        self.set_message(format!("format-on-save globally {}", if self.format_on_save { "enabled" } else { "disabled" }))
    }

    fn toggle_buffer_format(&mut self) -> Result<()> {
        let document_id = self.active.document_id;
        let disabled = !self.format_disabled.remove(&document_id);
        self.format_disabled.extend(disabled.then_some(document_id));
        self.set_message(format!("format-on-save for buffer {}", if disabled { "disabled" } else { "enabled" }))
    }

    fn set_colorscheme(&mut self, mut words: std::str::SplitWhitespace<'_>) -> Result<()> {
        let requested = words.next().unwrap_or("catppuccin");
        let flavor = parse_catppuccin_flavor(requested).ok_or_else(|| anyhow!("unknown Catppuccin flavor {requested:?}"))?;
        self.theme = EditorTheme::for_flavor(flavor);
        self.set_message(format!("colorscheme catppuccin-{}", flavor.identifier()))
    }

    fn set_theme_color(&mut self, mut words: std::str::SplitWhitespace<'_>) -> Result<()> {
        let name = words.next().ok_or_else(|| anyhow!("usage: setcolor SLOT #RRGGBB"))?;
        let value = words.next().ok_or_else(|| anyhow!("usage: setcolor SLOT #RRGGBB"))?;
        let color = RgbColor::from_hex(value).ok_or_else(|| anyhow!("invalid RGB color {value:?}; expected #RRGGBB"))?;
        if !self.theme.set(name, color) {
            bail!("unknown Catppuccin color slot {name:?}");
        }
        self.set_message(format!("theme {name}=#{:02x}{:02x}{:02x}", color.red, color.green, color.blue))
    }

    fn open_git(&mut self, words: std::str::SplitWhitespace<'_>) -> Result<()> {
        let arguments = words.map(Box::<str>::from).collect::<Vec<_>>();
        let root = self.active_git_root()?;
        self.open_terminal_in(Some(git_ex_program(&arguments)), &arguments, &root)
    }

    fn run_ai_prompt(&mut self, words: std::str::SplitWhitespace<'_>) -> Result<()> {
        let prompt = words.collect::<Vec<_>>().join(" ");
        if prompt.is_empty() { self.open_agent_sidebar() } else { self.start_ai_task(&prompt) }
    }

    fn run_rust_lsp_command(&mut self, mut words: std::str::SplitWhitespace<'_>) -> Result<()> {
        match words.next() {
            Some("testables" | "test") => self.open_terminal(Some("cargo"), &["test".into()]),
            Some("debuggables" | "debug") => self.open_debug_repl(),
            _ => self.open_terminal(Some("cargo"), &["run".into()]),
        }
    }

    pub(super) fn execute_ex_command(&mut self, command: ExCommand) -> Result<()> {
        match command {
            ExCommand::Goto { address } => {
                let line = self.resolve_address(&address)?;
                self.active.editor.set_cursor(self.active.editor.text().byte_of_line(line));
                if let Some((pattern, direction)) = address_search_pattern(&address) {
                    let persist = !pattern.is_empty();
                    let pattern = self.effective_search_pattern(pattern)?;
                    self.synchronize_search(&pattern, direction, persist)?;
                }
                Ok(())
            }
            ExCommand::Substitute { range, pattern, replacement, flags } => {
                let range = self.resolve_byte_range(range.as_ref())?;
                let substitute = self.resolve_substitute(&pattern, &replacement, flags, vec![range])?;
                self.start_substitution(substitute)
            }
            ExCommand::SubstituteRepeat { range, use_search_pattern, flags } => {
                let range = self.resolve_byte_range(range.as_ref())?;
                let substitute = self.resolve_repeated_substitute(use_search_pattern, flags, vec![range])?;
                self.start_substitution(substitute)
            }
            ExCommand::Global { range, invert, pattern, command } => self.execute_global(range.as_ref(), invert, &pattern, *command),
            ExCommand::Normal { range, keys, .. } => self.execute_normal(range.as_ref(), &keys),
            ExCommand::Write { range: Some(range), path: Some(path), .. } => self.write_range(&range, Path::new(path.as_ref())),
            ExCommand::Write { range: Some(_), path: None, .. } => Err(anyhow!("ranged :write requires a destination path")),
            ExCommand::Write { all: true, .. } => self.save_all(),
            ExCommand::Write { path, .. } => self.save(path.as_deref().map(Path::new)),
            ExCommand::WriteQuit { path, .. } => {
                self.save(path.as_deref().map(Path::new))?;
                self.quit = true;
                Ok(())
            }
            ExCommand::Quit { all, bang } => {
                let dirty = self.active.editor.is_dirty() || all && self.inactive.iter().any(|buffer| buffer.editor.is_dirty());
                if dirty && !bang {
                    self.show_error("E37: unsaved changes; use :q!")
                } else {
                    self.quit = true
                };
                Ok(())
            }
            ExCommand::Edit { bang, path } => match path {
                None => {
                    self.show_error("usage: :e[!] FILE");
                    Ok(())
                }
                Some(_) if self.active.editor.is_dirty() && !bang => {
                    self.show_error("E37: unsaved changes; use :e!");
                    Ok(())
                }
                Some(path) => self.open_buffer(Path::new(path.as_ref())),
            },
            ExCommand::Buffer { action, bang, target } => self.buffer_command(action, bang, target.as_deref()),
            ExCommand::Split { vertical, path } => {
                if let Some(path) = path {
                    self.open_buffer(Path::new(path.as_ref()))?;
                }
                let (axis, message) =
                    if vertical { (SplitAxis::Vertical, "vertical split created") } else { (SplitAxis::Horizontal, "horizontal split created") };
                self.views.split_active(axis)?;
                self.set_message(message.to_owned())
            }
            ExCommand::Close { bang } => {
                if self.active.editor.is_dirty() && !bang {
                    self.show_error("E37: unsaved changes; use :close!");
                    return Ok(());
                }
                self.views.close_active_window()?;
                self.activate_view_buffer()
            }
            ExCommand::Tab { action, path } => self.tab_command(action, path.as_deref()),
            ExCommand::Undo => {
                let transaction = self.active.editor.undo()?;
                self.after_transaction(transaction);
                Ok(())
            }
            ExCommand::Redo => {
                let transaction = self.active.editor.redo()?;
                self.after_transaction(transaction);
                Ok(())
            }
            ExCommand::Echo { expression } => {
                let value = evaluate_expression(&expression, &self.expression_context())?;
                self.set_message(expression_editor_text(&value))
            }
            ExCommand::Registers { names } => {
                let entries = self
                    .active
                    .editor
                    .registers()
                    .filter(|(name, _)| names.is_empty() || names.contains(*name))
                    .map(|(name, value)| format!("\"{name} {}", compact(value.text.as_ref(), 24)))
                    .collect();
                self.set_message(joined_entries(entries, "no registers"))
            }
            ExCommand::Marks { names } => {
                let entries = self
                    .active
                    .editor
                    .marks()
                    .filter(|(name, _)| names.is_empty() || names.contains(*name))
                    .map(|(name, byte)| format!("'{name}={byte}"))
                    .collect();
                self.set_message(joined_entries(entries, "no marks"))
            }
            ExCommand::NoHighlight => {
                self.search_highlight = false;
                self.message.clear();
                Ok(())
            }
            ExCommand::Help { topic } => self.set_message(
                topic.map_or_else(|| "run `wren --help` for the command reference".to_owned(), |topic| format!("help for {topic}: run `wren --help`")),
            ),
            ExCommand::Messages => self.show_debug_output(),
            ExCommand::Grep { pattern, paths } => self.grep(&pattern, &paths),
            ExCommand::Cdo { command } => self.execute_cdo(*command),
            ExCommand::ConvertUtf8 if self.active.document.encoding() == DocumentEncoding::Utf8 => self.set_message("document is already UTF-8".to_owned()),
            ExCommand::ConvertUtf8 => {
                let converted = self.active.document.convert_to_utf8()?;
                self.active.editor.set_read_only(false);
                let transaction = Transaction::new(self.active.editor.revision(), vec![Edit::new(0..self.active.editor.text().len_bytes(), converted)])?;
                self.active.editor.apply_transaction(transaction.clone())?;
                self.after_transaction(Some(transaction));
                self.set_message("converted invalid bytes to explicit UTF-8 \\xNN escapes".to_owned())
            }
            ExCommand::Terminal { program, arguments } => self.open_terminal(program.as_deref(), &arguments),
            ExCommand::Make { program, arguments } => self.start_make_task(&program, &arguments),
            ExCommand::Format { program, arguments } => self.start_format_task(&program, &arguments),
            ExCommand::Find { query } => self.start_file_picker(&query),
        }
    }

    pub(super) fn effective_search_pattern(&self, pattern: &str) -> Result<String> {
        if pattern.is_empty() {
            self.active.editor.last_search().map(|(pattern, _)| pattern.to_owned()).ok_or_else(|| anyhow!("no previous search pattern"))
        } else {
            Ok(pattern.to_owned())
        }
    }

    pub(super) fn resolve_address(&self, address: &ExAddress) -> Result<usize> {
        let text = self.active.editor.contents();
        let current = self.active.editor.cursor_line_column().0;
        let last = self.active.editor.text().line_of_byte(text.len());
        let line = match address {
            ExAddress::Current => current,
            ExAddress::Last => last,
            ExAddress::Line(line) => line.saturating_sub(1).min(last),
            ExAddress::Mark(name) => {
                self.active.editor.mark(*name).map(|byte| self.active.editor.text().line_of_byte(byte)).ok_or_else(|| anyhow!("mark '{name} is not set"))?
            }
            ExAddress::SearchForward(pattern) => {
                let cursor = self.active.editor.primary_cursor().min(text.len());
                let pattern = self.effective_search_pattern(pattern)?;
                self.active
                    .editor
                    .preview_search(&pattern, SearchDirection::Forward, cursor)?
                    .map(|byte| self.active.editor.text().line_of_byte(byte))
                    .ok_or_else(|| anyhow!("pattern not found: {pattern}"))?
            }
            ExAddress::SearchBackward(pattern) => {
                let cursor = self.active.editor.primary_cursor().min(text.len());
                let pattern = self.effective_search_pattern(pattern)?;
                self.active
                    .editor
                    .preview_search(&pattern, SearchDirection::Backward, cursor)?
                    .map(|byte| self.active.editor.text().line_of_byte(byte))
                    .ok_or_else(|| anyhow!("pattern not found: {pattern}"))?
            }
            ExAddress::Offset { base, delta } => {
                let base = self.resolve_address(base)?;
                base.saturating_add_signed(*delta as isize).min(last)
            }
        };
        Ok(line)
    }

    pub(super) fn resolve_line_range(&self, range: Option<&ExRange>) -> Result<Range<usize>> {
        let current = self.active.editor.cursor_line_column().0;
        let last = self.active.editor.text().line_of_byte(self.active.editor.text().len_bytes());
        let (start, end) = if let Some(range) = range {
            let start = self.resolve_address(&range.start)?;
            let end = range.end.as_ref().map_or(Ok(start), |address| self.resolve_address(address))?;
            (start.min(end), start.max(end))
        } else {
            (current, current)
        };
        Ok(start.min(last)..end.min(last).saturating_add(1))
    }

    pub(super) fn resolve_byte_range(&self, range: Option<&ExRange>) -> Result<Range<usize>> {
        let lines = self.resolve_line_range(range)?;
        Ok(self.active.editor.text().byte_of_line(lines.start)..self.active.editor.text().byte_of_line(lines.end))
    }

    pub(super) fn execute_normal(&mut self, range: Option<&ExRange>, keys: &str) -> Result<()> {
        let lines: Vec<_> = self.resolve_line_range(range)?.collect();
        for line in lines.into_iter().rev() {
            self.active.editor.set_cursor(self.active.editor.text().byte_of_line(line));
            for key in ex_normal_keys(keys) {
                self.dispatch_key(key);
            }
        }
        Ok(())
    }

    pub(super) fn execute_global(&mut self, range: Option<&ExRange>, invert: bool, pattern: &str, command: ExCommand) -> Result<()> {
        let lines = if range.is_some() {
            self.resolve_line_range(range)?
        } else {
            0..self.active.editor.text().line_of_byte(self.active.editor.text().len_bytes()).saturating_add(1)
        };
        let persist_pattern = !pattern.is_empty();
        let pattern = self.effective_search_pattern(pattern)?;
        let compiled = self.active.editor.compile_search_pattern(&pattern, CaseOverride::Default)?;
        let text = self.active.editor.contents();
        let selected = lines
            .filter_map(|line| {
                let start = self.active.editor.text().byte_of_line(line);
                let end = self.active.editor.text().byte_of_line(line.saturating_add(1));
                (compiled.is_match(&text[start..end]) != invert).then_some((line, start..end))
            })
            .collect::<Vec<_>>();
        self.synchronize_search(&pattern, SearchDirection::Forward, persist_pattern)?;
        let selected_ranges = || selected.iter().map(|(_, range)| range.clone()).collect();
        match &command {
            ExCommand::Substitute { pattern, replacement, flags, .. } => {
                let substitute = self.resolve_substitute(pattern, replacement, *flags, selected_ranges())?;
                return self.start_substitution(substitute);
            }
            ExCommand::SubstituteRepeat { use_search_pattern, flags, .. } => {
                let substitute = self.resolve_repeated_substitute(*use_search_pattern, *flags, selected_ranges())?;
                return self.start_substitution(substitute);
            }
            _ => {}
        }
        for (line, _) in selected.into_iter().rev() {
            self.active.editor.set_cursor(self.active.editor.text().byte_of_line(line));
            match &command {
                ExCommand::Normal { keys, .. } => {
                    for key in ex_normal_keys(keys) {
                        self.dispatch_key(key);
                    }
                }
                _ => self.execute_ex_command(command.clone())?,
            }
        }
        Ok(())
    }

    pub(super) fn open_buffer(&mut self, path: &Path) -> Result<()> {
        let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if self.active.document.presentation_path().is_some_and(|open| same_path(open, &resolved)) {
            self.record_file(&resolved);
            return Ok(());
        }
        if let Some(index) = self.inactive.iter().position(|buffer| buffer.document.presentation_path().is_some_and(|open| same_path(open, &resolved))) {
            self.switch_to_inactive(index)?;
            self.record_file(&resolved);
            return Ok(());
        }
        let document_id = stable_document_id(Some(&resolved));
        let buffer_id = self.views.add_buffer();
        let (mut buffer, message) = BufferState::open(buffer_id, document_id, Some(&resolved), None)?;
        apply_client_state(&mut buffer, &self.client_state)?;
        if let Some(pattern) = self.client_state.search_history.last() {
            buffer.editor.set_search(pattern.clone(), self.last_search_direction)?;
        }
        let replace_stale = !buffer.editor.is_dirty();
        self.mutations.register(document_id, buffer.editor.contents(), replace_stale)?;
        self.autosave_active_if_named()?;
        let previous = std::mem::replace(&mut self.active, buffer);
        self.inactive.push(previous);
        self.views.set_active_buffer(buffer_id);
        self.message = message;
        self.record_active_file();
        self.prime_active_syntax();
        self.begin_lsp_start();
        Ok(())
    }

    pub(super) fn current_jump_location(&self) -> Option<DurableJumpEntry> {
        self.active.document.presentation_path().map(|path| DurableJumpEntry {
            document_id: self.active.document_id,
            anchor: Anchor { byte: self.active.editor.primary_cursor(), bias: Bias::Right },
            path_hint: Some(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()).to_string_lossy().into_owned().into_boxed_str()),
        })
    }

    pub(super) fn entry_cursor_byte(&self, entry: &QuickfixEntry) -> usize {
        let line = entry.line.saturating_sub(1);
        let start = self.active.editor.text().byte_of_line(line);
        let end = self.active.editor.text().byte_of_line(line.saturating_add(1));
        let text = self.active.editor.contents();
        let line_text = text.get(start..end).unwrap_or_default();
        let column = entry.column.saturating_sub(1);
        if !entry.column_utf16 {
            return start.saturating_add(column.min(line_text.len()));
        }
        start.saturating_add(utf16_column_to_byte(line_text, column).unwrap_or(line_text.len()))
    }

    pub(super) fn record_navigation(&mut self, origin: DurableJumpEntry, target: DurableJumpEntry) {
        if origin == target {
            return;
        }
        let retained = self.jump_index.map_or(self.jump_history.len(), |index| index.saturating_add(1));
        self.jump_history.truncate(retained);
        if self.jump_history.last() != Some(&origin) {
            self.jump_history.push(origin);
        }
        if self.jump_history.last() != Some(&target) {
            self.jump_history.push(target);
        }
        self.jump_index = self.jump_history.len().checked_sub(1);
        self.persist_jump_list();
    }

    pub(super) fn navigate_to_entry(&mut self, entry: &QuickfixEntry) -> Result<()> {
        let origin = self.current_jump_location();
        self.open_buffer(&entry.path)?;
        let byte = self.entry_cursor_byte(entry);
        self.active.editor.set_cursor(byte);
        if let (Some(origin), Some(target)) = (origin, self.current_jump_location()) {
            self.record_navigation(origin, target);
        }
        Ok(())
    }

    pub(super) fn navigate_global_jump(&mut self, backward: bool) -> Result<bool> {
        let Some(index) = self.jump_index else {
            return Ok(false);
        };
        let next = if backward { index.checked_sub(1) } else { index.checked_add(1).filter(|next| *next < self.jump_history.len()) };
        let Some(next) = next else {
            return Ok(false);
        };
        let target = self.jump_history[next].clone();
        self.open_buffer(Path::new(target.path_hint.as_deref().ok_or_else(|| anyhow!("jump has no path"))?))?;
        self.active.editor.set_cursor(target.anchor.byte);
        self.jump_index = Some(next);
        self.persist_jump_list();
        self.message = format!("jump {} of {}", next + 1, self.jump_history.len());
        Ok(true)
    }

    pub(super) fn persist_jump_list(&mut self) {
        self.after_effect(TransactionBatch::new(), vec![StateDelta::JumpList { entries: self.jump_history.clone(), current: self.jump_index }]);
    }

    pub(super) fn record_active_file(&mut self) {
        if let Some(path) = self.active.document.presentation_path().map(Path::to_path_buf) {
            self.record_file(&path);
        }
    }

    pub(super) fn record_file(&mut self, path: &Path) {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.recent_files.retain(|recent| !same_path(recent, &path));
        self.recent_files.insert(0, path);
        self.recent_files.truncate(100);
        if let Err(error) = save_recent_files(&self.recent_files) {
            self.show_error(format!("oldfiles: {error}"));
        }
    }

    pub(super) fn switch_to_inactive(&mut self, index: usize) -> Result<()> {
        self.autosave_active_if_named()?;
        let buffer = self.inactive.get_mut(index).ok_or_else(|| anyhow!("buffer index disappeared"))?;
        std::mem::swap(&mut self.active, buffer);
        self.views.set_active_buffer(self.active.buffer_id);
        self.message = self.active.name();
        self.prime_active_syntax();
        self.begin_lsp_start();
        Ok(())
    }

    pub(super) fn activate_view_buffer(&mut self) -> Result<()> {
        let wanted = self.views.active_buffer();
        if wanted == self.active.buffer_id {
            return Ok(());
        }
        let index = self.inactive.iter().position(|buffer| buffer.buffer_id == wanted).ok_or_else(|| anyhow!("view references a missing buffer"))?;
        self.switch_to_inactive(index)
    }

    pub(super) fn buffer_command(&mut self, action: BufferAction, bang: bool, target: Option<&str>) -> Result<()> {
        if action == BufferAction::Delete {
            if self.active.editor.is_dirty() && !bang {
                self.show_error("E89: unsaved changes; use :bdelete!");
                return Ok(());
            }
            let Some(mut replacement) = self.inactive.pop() else {
                return self.set_message("cannot delete the final buffer".to_owned());
            };
            let deleted = self.active.buffer_id;
            std::mem::swap(&mut self.active, &mut replacement);
            self.views.remove_buffer(deleted, self.active.buffer_id);
            self.views.set_active_buffer(self.active.buffer_id);
            return self.set_message("buffer deleted".to_owned());
        }

        let mut ids: Vec<_> = self.inactive.iter().map(|buffer| buffer.buffer_id).chain(std::iter::once(self.active.buffer_id)).collect();
        ids.sort_by_key(|id| id.get());
        let current = ids.iter().position(|id| *id == self.active.buffer_id).unwrap_or(0);
        let wanted = match action {
            BufferAction::Next => ids[(current + 1) % ids.len()],
            BufferAction::Previous => ids[(current + ids.len() - 1) % ids.len()],
            BufferAction::First => ids[0],
            BufferAction::Last => ids[ids.len() - 1],
            BufferAction::Select => {
                let Some(target) = target else {
                    return self.set_message(format!("buffer {}: {}", self.active.buffer_id.get(), self.active.name()));
                };
                let numeric = target.parse::<u64>().ok();
                self.inactive
                    .iter()
                    .chain(std::iter::once(&self.active))
                    .find(|buffer| numeric == Some(buffer.buffer_id.get()) || buffer.name().contains(target))
                    .map(|buffer| buffer.buffer_id)
                    .ok_or_else(|| anyhow!("no matching buffer: {target}"))?
            }
            BufferAction::Delete => unreachable!(),
        };
        if wanted != self.active.buffer_id {
            let index = self.inactive.iter().position(|buffer| buffer.buffer_id == wanted).ok_or_else(|| anyhow!("buffer disappeared"))?;
            self.switch_to_inactive(index)?;
        }
        Ok(())
    }

    pub(super) fn tab_command(&mut self, action: TabAction, path: Option<&str>) -> Result<()> {
        match action {
            TabAction::New => {
                if let Some(path) = path {
                    self.open_buffer(Path::new(path))?;
                }
                self.views.new_tab(self.active.buffer_id);
            }
            TabAction::Next => self.views.cycle_tab(1),
            TabAction::Previous => self.views.cycle_tab(-1),
            TabAction::First => {
                if let Some(tab) = self.views.tabs.first() {
                    self.views.active_tab = tab.id;
                }
            }
            TabAction::Last => {
                if let Some(tab) = self.views.tabs.last() {
                    self.views.active_tab = tab.id;
                }
            }
            TabAction::Close => self.views.close_active_tab()?,
        }
        self.activate_view_buffer()
    }

    pub(super) fn save_all(&mut self) -> Result<()> {
        let mut saved = 0;
        if self.active.editor.is_dirty() {
            save_buffer(&mut self.active)?;
            saved += 1;
        }
        for buffer in &mut self.inactive {
            if buffer.editor.is_dirty() {
                save_buffer(buffer)?;
                saved += 1;
            }
        }
        self.set_message(format!("{saved} buffer(s) written"))
    }

    pub(super) fn autosave_active_if_named(&mut self) -> Result<()> {
        if self.active.editor.is_dirty() && !self.active.editor.is_read_only() && self.active.document.presentation_path().is_some() {
            if self.format_on_save
                && !self.format_disabled.contains(&self.active.document_id)
                && let Err(error) = self.format_active_sync(false)
            {
                self.show_error(format!("format-on-save: {error}"));
            }
            save_buffer(&mut self.active)?;
        }
        Ok(())
    }

    pub(super) fn write_range(&mut self, range: &ExRange, path: &Path) -> Result<()> {
        let range = self.resolve_byte_range(Some(range))?;
        let contents = self.active.editor.contents();
        let selected = contents.get(range).ok_or_else(|| anyhow!("resolved write range is not on UTF-8 boundaries"))?;
        let (mut target, opened) = LocalDocument::open_or_new(path).with_context(|| format!("open range destination {}", path.display()))?;
        if opened.read_only {
            bail!("range destination {} is not editable UTF-8", path.display());
        }
        let report = target.save(selected)?;
        self.set_message(format!("{} bytes written to {}", report.bytes_written, path.display()))
    }

    pub(super) fn grep(&mut self, pattern: &str, paths: &[Box<str>]) -> Result<()> {
        let root = self.workspace_root();
        self.populate_grep_results(pattern, paths, &root)?;
        self.set_message(format!("{} grep result(s)", self.quickfix.len()))
    }

    pub(super) fn populate_grep_results(&mut self, pattern: &str, paths: &[Box<str>], root: &Path) -> Result<()> {
        if pattern.is_empty() {
            self.quickfix.clear();
            return Ok(());
        }
        let mut command = Command::new("rg");
        command.current_dir(root).arg("--vimgrep").arg("--").arg(pattern);
        if paths.is_empty() {
            command.arg(".");
        } else {
            command.args(paths.iter().map(AsRef::<str>::as_ref));
        }
        let output = command.output().context("run native rg search")?;
        if !output.status.success() && output.status.code() != Some(1) {
            bail!("rg failed: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        self.quickfix = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(parse_vimgrep_line)
            .map(|mut entry| {
                if entry.path.is_relative() {
                    entry.path = root.join(entry.path);
                }
                entry
            })
            .take(10_000)
            .collect();
        Ok(())
    }

    pub(super) fn workspace_root(&self) -> PathBuf {
        let start = self
            .active
            .document
            .presentation_path()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let root = git_root_for(&start).unwrap_or(start);
        std::fs::canonicalize(&root).unwrap_or(root)
    }

    pub(super) fn lsp_root(&self) -> PathBuf {
        self.root_workspace.clone()
    }

    pub(super) fn refresh_diagnostics(&mut self) -> Result<()> {
        let Some(path) = self.active.document.presentation_path().map(Path::to_path_buf) else {
            self.diagnostics.clear();
            return Ok(());
        };
        let Some(invocation) = diagnostic_invocation(&path, &self.workspace_root()) else {
            self.diagnostics.clear();
            return Ok(());
        };
        if !executable_exists(&invocation.program) {
            self.diagnostics.clear();
            return self.set_message(format!("diagnostic tool {} is not installed", invocation.program));
        }
        let output = Command::new(&invocation.program)
            .args(&invocation.arguments)
            .current_dir(&invocation.directory)
            .output()
            .with_context(|| format!("run diagnostics with {}", invocation.program))?;
        let combined = format!("{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        self.diagnostics = combined.lines().filter_map(|line| parse_diagnostic_line(line, &invocation.directory)).collect();
        if self.diagnostics.is_empty() && !output.status.success() {
            let detail = combined.lines().find(|line| !line.trim().is_empty()).unwrap_or("diagnostic command failed").trim();
            self.diagnostics.push(QuickfixEntry::diagnostic(path, 1, 1, Severity::Error, detail));
        }
        self.diagnostics.sort_by(|left, right| {
            left.severity
                .cmp(&right.severity)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.line.cmp(&right.line))
                .then_with(|| left.column.cmp(&right.column))
        });
        Ok(())
    }

    pub(super) fn show_cursor_diagnostic(&mut self) -> Result<()> {
        self.refresh_diagnostics()?;
        let path = self.active.document.presentation_path();
        let cursor_line = self.active.editor.cursor_line_column().0 + 1;
        let diagnostic = self
            .diagnostics
            .iter()
            .filter(|entry| path.is_some_and(|path| same_path(path, &entry.path)))
            .min_by_key(|entry| entry.line.abs_diff(cursor_line))
            .map_or_else(|| "no diagnostics".to_owned(), |entry| format!("{} {}:{}: {}", entry.severity.label(), entry.line, entry.column, entry.text));
        if diagnostic == "no diagnostics" {
            self.close_editor_popup();
            self.message = diagnostic;
        } else {
            self.popup = Some(TextPopup::new("diagnostic", diagnostic));
            self.popup_deadline = None;
            self.message.clear();
        }
        Ok(())
    }

    pub(super) fn move_diagnostic(&mut self, direction: isize) -> Result<()> {
        self.refresh_diagnostics()?;
        let Some(path) = self.active.document.presentation_path().map(Path::to_path_buf) else {
            return self.set_message("no diagnostics".to_owned());
        };
        let current = self.active.editor.cursor_line_column().0 + 1;
        let entries = self.diagnostics.iter().filter(|entry| same_path(&entry.path, &path)).collect::<Vec<_>>();
        let selected = if direction < 0 {
            entries.iter().rev().find(|entry| entry.line < current).or_else(|| entries.last())
        } else {
            entries.iter().find(|entry| entry.line > current).or_else(|| entries.first())
        };
        let Some(entry) = selected.copied() else {
            return self.set_message("no diagnostics".to_owned());
        };
        let line_start = self.active.editor.text().byte_of_line(entry.line.saturating_sub(1));
        self.active.editor.set_cursor(line_start.saturating_add(entry.column.saturating_sub(1)));
        self.set_message(format!("{}: {}", entry.severity.label(), entry.text))
    }

    pub(super) fn active_git_file(&self) -> Result<(PathBuf, PathBuf)> {
        let path = self.active.document.presentation_path().map(Path::to_path_buf).ok_or_else(|| anyhow!("Git action needs a named buffer"))?;
        let root = git_root_for(&path)?;
        let relative = path.strip_prefix(&root).with_context(|| format!("{} is outside {}", path.display(), root.display()))?.to_path_buf();
        Ok((root, relative))
    }

    pub(super) fn active_git_root(&self) -> Result<PathBuf> {
        match self.active.document.presentation_path() {
            Some(path) => git_root_for(path),
            None => git_root_for(&env::current_dir().context("locate current workspace")?),
        }
    }

    pub(super) fn active_git_patch(&self) -> Result<(PathBuf, Vec<u8>)> {
        let (root, relative) = self.active_git_file()?;
        if !git_path_tracked(&root, &relative)? {
            let output = Command::new("git")
                .current_dir(&root)
                .args(["add", "--intent-to-add", "--"])
                .arg(&relative)
                .output()
                .context("mark untracked file as intent-to-add")?;
            if !output.status.success() {
                bail!("git add --intent-to-add: {}", String::from_utf8_lossy(&output.stderr).trim());
            }
        }
        let before = git_index_contents(&root, &relative)?;
        let after = self.active.editor.contents();
        let patch = make_git_patch(&root, &relative, &before, &after)?;
        Ok((root, patch))
    }

    pub(super) fn selected_git_patch(&self) -> Result<(PathBuf, Vec<u8>)> {
        let (root, patch) = self.active_git_patch()?;
        let (cursor_line, _) = self.active.editor.cursor_line_column();
        let line_range = matches!(self.active.editor.mode(), Mode::Visual | Mode::VisualLine).then(|| {
            let region = self.active.editor.selection_byte_range();
            let start = self.active.editor.text().line_of_byte(region.start) + 1;
            let end = self.active.editor.text().line_of_byte(region.end.saturating_sub(1)) + 1;
            start..end.saturating_add(1)
        });
        let selected = select_git_hunk(&patch, cursor_line + 1, line_range.as_ref())?;
        Ok((root, selected))
    }

    pub(super) fn git_stage_hunk(&mut self) -> Result<()> {
        let (root, patch) = self.selected_git_patch()?;
        git_apply_patch(&root, &patch, true, false)?;
        self.last_staged_patch = Some(patch);
        self.refresh_active_git_baseline();
        self.set_message("staged hunk".to_owned())
    }

    pub(super) fn git_reset_hunk(&mut self) -> Result<()> {
        let (root, relative) = self.active_git_file()?;
        let before = git_index_contents(&root, &relative)?;
        let after = self.active.editor.contents();
        let cursor = u32::try_from(self.active.editor.cursor_line_column().0).unwrap_or(u32::MAX);
        let hunks = git_hunks(&before, &after);
        let hunk = hunks
            .iter()
            .find(|hunk| {
                let end = hunk.after.end.max(hunk.after.start.saturating_add(1));
                hunk.after.start <= cursor && cursor < end
            })
            .ok_or_else(|| anyhow!("cursor is not in a changed Git hunk"))?;
        let after_range = byte_range_of_lines(&after, hunk.after.start as usize..hunk.after.end as usize);
        let before_range = byte_range_of_lines(&before, hunk.before.start as usize..hunk.before.end as usize);
        let replacement = before.get(before_range).unwrap_or_default().to_owned();
        let transaction = Transaction::new(self.active.editor.revision(), vec![Edit::new(after_range, replacement)])?;
        self.active.editor.apply_transaction(transaction.clone())?;
        self.after_transaction(Some(transaction));
        self.set_message("reset hunk".to_owned())
    }

    pub(super) fn git_stage_buffer(&mut self) -> Result<()> {
        let (root, patch) = self.active_git_patch()?;
        if patch.is_empty() {
            return self.set_message("buffer has no changes".to_owned());
        }
        git_apply_patch(&root, &patch, true, false)?;
        self.last_staged_patch = Some(patch);
        self.refresh_active_git_baseline();
        self.set_message("staged buffer".to_owned())
    }

    pub(super) fn git_undo_stage_hunk(&mut self) -> Result<()> {
        let Some(patch) = self.last_staged_patch.take() else {
            return self.set_message("no staged hunk to undo".to_owned());
        };
        let (root, _) = self.active_git_file()?;
        git_apply_patch(&root, &patch, true, true)?;
        self.refresh_active_git_baseline();
        self.set_message("undid staged hunk".to_owned())
    }

    pub(super) fn git_preview_hunk(&mut self) -> Result<()> {
        let (_, patch) = self.selected_git_patch()?;
        self.popup = Some(TextPopup::new("Git hunk", String::from_utf8_lossy(&patch).into_owned()));
        self.popup_deadline = None;
        self.message.clear();
        Ok(())
    }

    pub(super) fn git_blame_line(&mut self) -> Result<()> {
        let (root, relative) = self.active_git_file()?;
        let line = self.active.editor.cursor_line_column().0 + 1;
        let output = Command::new("git")
            .current_dir(root)
            .args(["--no-pager", "blame", "--date=short", "-L"])
            .arg(format!("{line},{line}"))
            .arg("--")
            .arg(relative)
            .output()
            .context("run git blame")?;
        if !output.status.success() {
            bail!("git blame: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        self.popup = Some(TextPopup::new("Git blame", String::from_utf8_lossy(&output.stdout).trim()));
        self.popup_deadline = None;
        self.message.clear();
        Ok(())
    }

    pub(super) fn git_diff_index(&mut self) -> Result<()> {
        let (root, relative) = self.active_git_file()?;
        let root = root.to_string_lossy().into_owned().into_boxed_str();
        let relative = relative.to_string_lossy().into_owned().into_boxed_str();
        self.open_terminal(Some("git"), &["-C".into(), root, "--no-pager".into(), "diff".into(), "--".into(), relative])
    }

    pub(super) fn move_git_hunk(&mut self, direction: isize) -> Result<()> {
        let current = u32::try_from(self.active.editor.cursor_line_column().0).unwrap_or(u32::MAX);
        let selected = if direction < 0 {
            self.active.git_hunks.iter().rev().find(|hunk| hunk.after.start < current).or_else(|| self.active.git_hunks.last())
        } else {
            self.active.git_hunks.iter().find(|hunk| hunk.after.start > current).or_else(|| self.active.git_hunks.first())
        };
        let Some(hunk) = selected else {
            return self.set_message("no Git hunks".to_owned());
        };
        self.active.editor.set_cursor(self.active.editor.text().byte_of_line(hunk.after.start as usize));
        self.message = format!(
            "hunk -{},{} +{},{}",
            hunk.before.start + 1,
            hunk.before.end.saturating_sub(hunk.before.start),
            hunk.after.start + 1,
            hunk.after.end.saturating_sub(hunk.after.start)
        );
        Ok(())
    }

    pub(super) fn refresh_active_git_baseline(&mut self) {
        self.active.git_index_text = self.active_git_file().ok().and_then(|(root, relative)| git_index_contents(&root, &relative).ok()).map(Arc::from);
        self.active.refresh_git_hunks();
    }

    pub(super) fn toggle_breakpoint(&mut self, condition: Option<String>) -> Result<()> {
        let Some(path) = self.active.document.presentation_path().map(Path::to_path_buf) else {
            return self.set_message("breakpoints need a named buffer");
        };
        let line = self.active.editor.cursor_line_column().0 + 1;
        let file = self.breakpoints.entry(path.clone()).or_default();
        if file.remove(&line).is_some() {
            self.message = format!("removed breakpoint at {}:{line}", path.display());
        } else {
            let label = condition.as_deref().filter(|value| !value.is_empty()).map_or_else(String::new, |value| format!(" if {value}"));
            file.insert(line, condition.filter(|value| !value.is_empty()));
            self.message = format!("breakpoint at {}:{line}{label}", path.display());
        }
        Ok(())
    }

    pub(super) fn debug_overlay(&self) -> DebugOverlay {
        let breakpoints = self
            .breakpoints
            .iter()
            .flat_map(|(path, lines)| {
                lines.iter().map(move |(line, condition)| {
                    format!("● {}:{line}{}", path.display(), condition.as_deref().map_or_else(String::new, |condition| format!(" if {condition}")))
                })
            })
            .collect::<Vec<_>>()
            .join("\n");
        let (line, column) = self.active.editor.cursor_line_column();
        let stacks = format!("▾ current thread\n  {}:{}:{}", self.active.name(), line + 1, column + 1);
        let repl = self
            .terminal
            .as_ref()
            .map(|_| self.terminal_frame().text.materialize_for_task().to_string())
            .unwrap_or_else(|| "Press <Space>dl to start the debugger REPL".to_owned());
        let limits = wren_scheduling::RuntimeLimits::for_terminal(self.viewport_columns, self.viewport_rows);
        let runtime_limits = format!(
            "frames={} mutations={} provider-revisions={} provider-demands={} tasks={}\ncontrol={} KiB bulk={} KiB snapshots={} MiB row-cache={} KiB",
            limits.frame_slots,
            limits.pending_mutations,
            limits.provider_revision_slots,
            limits.provider_demand_documents,
            limits.task_slots,
            limits.control_frame_bytes / 1024,
            limits.bulk_chunk_bytes / 1024,
            limits.provider_snapshot_bytes / (1024 * 1024),
            limits.retained_row_bytes / 1024,
        );
        DebugOverlay {
            panels: [
                DebugPanel { title: "Scopes", text: "▸ Locals\n▸ Arguments\n▸ Registers".into() },
                DebugPanel { title: "Breakpoints", text: if breakpoints.is_empty() { "No breakpoints".into() } else { breakpoints.into() } },
                DebugPanel { title: "Stacks", text: stacks.into() },
                DebugPanel { title: "Limits", text: runtime_limits.into() },
                DebugPanel { title: "REPL", text: repl.into() },
                DebugPanel { title: "Console", text: if self.message.is_empty() { "Debugger console".into() } else { self.message.clone().into() } },
            ],
        }
    }

    pub(super) fn open_debug_repl(&mut self) -> Result<()> {
        let path = self.active.document.presentation_path().map(|path| path.to_string_lossy().into_owned());
        let language = language_bundle(self.active.document.presentation_path()).language_id;
        let (program, arguments): (&str, Vec<Box<str>>) = match language.as_ref() {
            "python" if executable_exists("python3") => {
                ("python3", path.map_or_else(|| vec!["-m".into(), "pdb".into()], |path| vec!["-m".into(), "pdb".into(), path.into_boxed_str()]))
            }
            "go" if executable_exists("dlv") => ("dlv", vec!["debug".into()]),
            "rust" | "c" | "cpp" if executable_exists("lldb") => ("lldb", path.map_or_else(Vec::new, |path| vec![path.into_boxed_str()])),
            _ => {
                return self.set_message(format!("no installed debugger for {language}"));
            }
        };
        self.open_terminal(Some(program), &arguments)
    }

    pub(super) fn run_debug_action(&mut self, action: &str) -> Result<()> {
        if self.terminal.is_none() {
            self.open_debug_repl()?;
        }
        let command = match action {
            "dc" => "continue\n",
            "ds" => "step\n",
            "dn" => "next\n",
            "do" => "finish\n",
            "dr" => "run\n",
            _ => return Ok(()),
        };
        if let Some(terminal) = &mut self.terminal {
            terminal.send_input(command.as_bytes())?;
            self.input_focus = InputFocus::Terminal { escape_pending: false };
            self.message = format!("debug {action}");
        }
        Ok(())
    }

    pub(super) fn open_hoogle(&mut self) -> Result<()> {
        let query = self.word_under_cursor().unwrap_or_default();
        if query.is_empty() {
            return self.set_message("no Haskell identifier under cursor".to_owned());
        }
        let url = format!("https://hoogle.haskell.org/?hoogle={}", url_encode(&query));
        Command::new("open").arg(&url).spawn().with_context(|| format!("open {url}"))?;
        self.set_message(format!("Hoogle: {query}"))
    }

    pub(super) fn hoogle_signature(&mut self) -> Result<()> {
        let query = self.word_under_cursor().unwrap_or_default();
        if query.is_empty() {
            return self.set_message("no Haskell identifier under cursor".to_owned());
        }
        if !executable_exists("hoogle") {
            return self.set_message("hoogle is not installed".to_owned());
        }
        let output = Command::new("hoogle").args(["--count=1", "--color=false", "--"]).arg(&query).output().context("run Hoogle")?;
        self.set_message(String::from_utf8_lossy(&output.stdout).lines().next().unwrap_or("no Hoogle result").to_owned())
    }

    pub(super) fn open_haskell_repl(&mut self, package: bool) -> Result<()> {
        if package && executable_exists("cabal") {
            return self.open_terminal(Some("cabal"), &["repl".into()]);
        }
        let arguments = if package {
            Vec::new()
        } else {
            self.active.document.presentation_path().map(|path| vec![path.to_string_lossy().into_owned().into_boxed_str()]).unwrap_or_default()
        };
        self.open_terminal(Some("ghci"), &arguments)
    }

    pub(super) fn quit_repl(&mut self) -> Result<()> {
        if let Some(terminal) = &mut self.terminal {
            terminal.send_input(b":quit\n")?;
            self.message = "REPL quit requested".to_owned();
        } else {
            self.message = "no active REPL".to_owned();
        }
        Ok(())
    }

    pub(super) fn evaluate_in_repl(&mut self) -> Result<()> {
        let text = self.active.editor.contents();
        let expression = if matches!(self.active.editor.mode(), Mode::Visual | Mode::VisualLine) {
            text.get(self.active.editor.selection_byte_range()).unwrap_or_default().to_owned()
        } else {
            let line = self.active.editor.cursor_line_column().0;
            let start = self.active.editor.text().byte_of_line(line);
            let end = self.active.editor.text().byte_of_line(line + 1);
            text[start..end].trim_end().to_owned()
        };
        if self.terminal.is_none() {
            self.open_haskell_repl(false)?;
        }
        if let Some(terminal) = &mut self.terminal {
            terminal.send_input(expression.as_bytes())?;
            terminal.send_input(b"\n")?;
            self.input_focus = InputFocus::Terminal { escape_pending: false };
        }
        Ok(())
    }
}

fn joined_entries(entries: Vec<String>, empty: &str) -> String {
    if entries.is_empty() { empty.to_owned() } else { entries.join("  ") }
}
