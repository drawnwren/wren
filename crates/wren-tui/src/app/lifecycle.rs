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
        mutations.register(document_id, active.editor.frame().text.as_ref().to_owned())?;
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
            ai_transcript: String::new(),
            active_ai_task: None,
            last_staged_patch: None,
            theme_flavor,
            theme,
            viewport_rows: 24,
            viewport_columns: 80,
            quit: false,
        };
        app.capture_debug_output();
        app.record_active_file();
        app.prime_active_syntax();
        app.active.editor.prepare_realtime_navigation();
        app.schedule_lsp_start();
        Ok(app)
    }
}

fn push_message(messages: &mut Vec<String>, message: String) {
    if !message.is_empty() {
        messages.push(message);
    }
}
