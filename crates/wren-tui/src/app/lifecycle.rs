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
            ai_transcript: String::new(),
            active_ai_task: None,
            last_staged_patch: None,
            theme_flavor,
            theme,
            viewport_rows: 24,
            viewport_columns: 80,
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

        // Exercise the same visible-span cardinality as the live buffer while
        // retaining only a disposable subset of its immutable decorations.
        if let Some(state) = self.decorations.get(&self.active.buffer_id) {
            let frame = self.active.editor.frame();
            const PREPARED_VIEWPORT_ROWS: usize = 64;
            let last_line = frame.text.line_of_byte(frame.text.len());
            for top_line in [0, last_line.saturating_sub(PREPARED_VIEWPORT_ROWS)] {
                prepare_decoration_mapping(
                    state,
                    &frame.text,
                    self.active.editor.revision(),
                    top_line,
                    PREPARED_VIEWPORT_ROWS,
                    self.theme,
                )?;
            }
        }

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

fn prepare_decoration_mapping(
    state: &BufferDecorations,
    frame: &FrameText,
    revision: DocumentRevision,
    top_line: usize,
    rows: usize,
    theme: CatppuccinPalette,
) -> Result<()> {
    let start = frame.byte_of_line(top_line);
    let visible = start..frame.byte_of_line(top_line.saturating_add(rows).saturating_add(1));
    let mut decorations = BufferDecorations::new(revision, state.spans_in(visible.clone()));
    let transaction = Transaction::new(revision, vec![Edit::new(start..start, "x")])?;
    let preview = frame.edited(&transaction)?;
    let changed = start..preview.byte_of_line(top_line.saturating_add(2));
    let replacement = lexical_highlight_text(preview.slice(changed.clone()).as_ref())
        .into_iter()
        .map(|mut span| {
            span.range.start = span.range.start.saturating_add(start);
            span.range.end = span.range.end.saturating_add(start);
            provider_decoration(span, theme)
        })
        .collect();
    decorations.replace_after_transaction(
        &transaction,
        revision
            .next()
            .ok_or_else(|| anyhow!("realtime preparation revision overflow"))?,
        std::slice::from_ref(&changed),
        replacement,
    );
    std::hint::black_box(decorations.spans_in(start..visible.end.saturating_add(1)));
    Ok(())
}

fn push_message(messages: &mut Vec<String>, message: String) {
    if !message.is_empty() {
        messages.push(message);
    }
}
