use super::*;

impl App {
    pub(super) fn open(path: Option<&Path>, line: Option<usize>) -> Result<Self> {
        let (document, opened) = match path {
            Some(path) => LocalDocument::open_or_new(path)
                .with_context(|| format!("open {}", path.display()))?,
            None => LocalDocument::unnamed(),
        };
        #[cfg(not(test))]
        let wal = document
            .presentation_path()
            .map(LocalWal::for_document)
            .transpose()
            .context("locate recovery WAL")?;
        #[cfg(test)]
        let wal = None;
        Self::from_opened(document, opened, line, wal)
    }

    pub(super) fn from_opened(
        document: LocalDocument,
        opened: OpenedDocument,
        line: Option<usize>,
        wal: Option<LocalWal>,
    ) -> Result<Self> {
        #[cfg(not(test))]
        wren_scheduling::mark_interactive();
        let buffer_id = BufferId::new(1);
        let document_id = stable_document_id(document.presentation_path());
        let (mut active, opened_message) =
            BufferState::from_opened(buffer_id, document_id, document, opened, line, wal)?;
        let mut messages = Vec::new();
        push_message(&mut messages, opened_message);
        let (client_state_worker, client_state) = ClientStateWorker::open(ClientId::new(1))?;
        if let Err(error) = restore_client_state(&mut active, &client_state) {
            messages.push(format!("client state: {error}"));
        }
        let name = active.name();
        let jump_history: Vec<JumpLocation> = client_state
            .jump_list
            .iter()
            .filter_map(|entry| {
                entry.path_hint.as_deref().map(|path| JumpLocation {
                    document_id: entry.document_id,
                    path: PathBuf::from(path),
                    byte: entry.anchor.byte,
                })
            })
            .collect();
        let jump_index = client_state
            .jump_index
            .filter(|index| *index < jump_history.len());
        let root_workspace = env::current_dir()
            .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
            .unwrap_or_else(|_| PathBuf::from("."));
        let mutations = MutationWorker::start(&root_workspace)?;
        mutations.register(
            document_id,
            active.editor.frame().text.as_ref().to_owned(),
            !active.editor.is_dirty(),
        )?;
        let (theme_flavor, theme, theme_message) = load_theme();
        push_message(&mut messages, theme_message);
        let (keymap, keymap_message) = load_keymap();
        push_message(&mut messages, keymap_message);
        let last_search_direction = if client_state.search_backward {
            SearchDirection::Backward
        } else {
            SearchDirection::Forward
        };
        let mut app = Self {
            active,
            inactive: Vec::new(),
            views: ClientViewModel::new(document_id, name),
            quickfix: Vec::new(),
            jump_history,
            jump_index,
            mutations,
            client_state,
            client_state_worker,
            provider: ProviderWorker::start()?,
            git_worker: GitHunkWorker::start()?,
            pending_git_refreshes: Vec::new(),
            provider_submitted: BTreeMap::new(),
            provider_refresh_due: BTreeMap::new(),
            provider_refresh_ranges: BTreeMap::new(),
            decorations: BTreeMap::new(),
            semantic_decorations: BTreeMap::new(),
            prompt: None,
            search_prompt_origin: None,
            last_search_direction,
            search_highlight: false,
            last_substitute: None,
            substitute_confirmation: None,
            message: messages.join("; "),
            tasks: TaskRunner::new(1, 8)?,
            active_task: None,
            next_task_id: 1,
            terminal: None,
            terminal_focused: false,
            terminal_escape_pending: false,
            mouse_selection: None,
            picker_files: Vec::new(),
            picker_matches: Vec::new(),
            picker_index: 0,
            picker_directory: None,
            picker_preview_title: String::new(),
            picker_preview: String::new(),
            picker_preview_scroll: 0,
            picker_preview_highlight_line: None,
            picker_preview_decorations: Vec::new(),
            grep_generation: 0,
            grep_due: None,
            grep_pending: None,
            grep_task: None,
            popup: None,
            popup_deadline: None,
            ace_jump: None,
            completion: None,
            completion_index: 0,
            completion_selected: false,
            completion_documentation_scroll: 0,
            snippet_stops: Vec::new(),
            snippet_stop_index: 0,
            lsp_completion: None,
            lsp: None,
            parked_lsps: Vec::new(),
            lsp_navigation_capabilities: BTreeMap::new(),
            lsp_start_due: None,
            lsp_start: None,
            lsp_background: None,
            lsp_semantic_dirty: false,
            pending_lsp_hover: None,
            pending_lsp_location: None,
            leader_keys: None,
            leader_deadline: None,
            keymap,
            normal_prefix: None,
            last_picker_query: String::new(),
            last_picker_source: None,
            recent_files: load_recent_files(),
            diagnostics: Vec::new(),
            format_on_save: true,
            format_disabled: BTreeSet::new(),
            breakpoints: BTreeMap::new(),
            root_workspace: root_workspace.clone(),
            workspace_folders: vec![root_workspace],
            debug_ui_visible: false,
            agent_terminal: None,
            agent_terminal_escape_pending: false,
            agent_window_prefix_pending: false,
            agent_sidebar: AgentSidebarState::default(),
            last_staged_patch: None,
            theme_flavor,
            theme,
            viewport_rows: 24,
            viewport_columns: 80,
            realtime_decorations_prepared: Cell::new(false),
            startup_screen: RefCell::new(StartupScreen::default()),
            started_at: Instant::now(),
            foreground_frame_pending: false,
            quit: false,
        };
        app.capture_debug_output();
        app.record_active_file();
        app.prime_active_syntax();
        app.prepare_realtime_paths()?;
        app.active.editor.prepare_realtime_navigation();
        // Automatic analysis begins after the first idle window. Explicit LSP
        // actions still start it immediately, while continuous input never
        // competes with server startup for an interactive frame.
        app.schedule_workspace_lsp_start();
        Ok(app)
    }

    pub(super) fn shows_startup_screen(&self) -> bool {
        !self.terminal_focused
            && self.active.document.presentation_path().is_none()
            && self.active.display_name.is_none()
            && self.active.editor.text().len_bytes() == 0
            && !self.active.editor.is_dirty()
            && self.inactive.is_empty()
            && self.views.buffers.len() == 1
            && self.views.windows.len() == 1
            && self.views.tabs.len() == 1
    }

    /// Fault the first local input and edit paths with an independent store.
    /// The scratch editor is intentionally not a clone: `TextStore` clones may
    /// share mutable authority, while startup preparation must never touch the
    /// live document.
    pub(super) fn prepare_realtime_paths(&mut self) -> Result<()> {
        let source = "pub fn warm() { let value = 1; value }\n";
        let store = DefaultText::from_reader(Cursor::new(source.as_bytes()))
            .context("create realtime preparation text store")?;
        let mut editor = Editor::with_contents(store, source.to_owned());
        let _ = editor.handle_key(KeyEvent::character('i'))?;
        let _ = editor
            .handle_key(KeyEvent::character('x'))?
            .ok_or_else(|| anyhow!("realtime preparation edit produced no transaction"))?;
        let _ = editor.handle_key(KeyEvent::plain(KeyCode::Backspace))?;
        let _ = editor.handle_key(KeyEvent::plain(KeyCode::Escape))?;
        std::hint::black_box(editor.frame());

        // A boundary motion warms App dispatch without changing selections,
        // history, or durable state. Preserve the user-visible message.
        let cursor = self.active.editor.primary_cursor();
        let message = std::mem::take(&mut self.message);
        self.active.editor.set_cursor(0);
        self.dispatch_key(KeyEvent::character('h'));
        self.active.editor.set_cursor(cursor);
        self.message = message;
        Ok(())
    }
}

fn push_message(messages: &mut Vec<String>, message: String) {
    if !message.is_empty() {
        messages.push(message);
    }
}
