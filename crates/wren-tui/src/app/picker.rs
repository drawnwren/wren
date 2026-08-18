use super::*;

const GREP_PICKER_RESULT_LIMIT: usize = 10_000;

fn launch_grep_picker_task(request: GrepPickerRequest) -> Result<GrepPickerTask> {
    let generation = request.generation;
    let query = request.query.clone();
    let child = Arc::new(Mutex::new(None));
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_child = Arc::clone(&child);
    let worker_cancelled = Arc::clone(&cancelled);
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("wren-live-grep".to_owned())
        .spawn(move || {
            wren_scheduling::mark_background();
            let result = run_grep_picker_search(request, &worker_child, &worker_cancelled);
            let _ = sender.send(result);
        })
        .context("start repository search worker")?;
    Ok(GrepPickerTask {
        generation,
        query,
        child,
        cancelled,
        receiver,
    })
}

fn run_grep_picker_search(
    request: GrepPickerRequest,
    child_slot: &Mutex<Option<std::process::Child>>,
    cancelled: &AtomicBool,
) -> std::result::Result<Vec<QuickfixEntry>, String> {
    let root = git_root_for(&request.root).unwrap_or(request.root);
    if cancelled.load(Ordering::Acquire) {
        return Err("cancelled".to_owned());
    }
    let mut child = Command::new("rg")
        .current_dir(&root)
        .args(["--vimgrep", "--no-messages", "--"])
        .arg(&request.query)
        .arg(".")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("start rg: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "rg stdout was not captured".to_owned())?;
    {
        let mut slot = child_slot
            .lock()
            .map_err(|_| "repository search child lock is poisoned".to_owned())?;
        if cancelled.load(Ordering::Acquire) {
            let _ = child.kill();
            return Err("cancelled".to_owned());
        }
        *slot = Some(child);
    }

    let mut entries = Vec::new();
    let mut read_error = None;
    for line in BufReader::new(stdout).lines() {
        match line {
            Ok(line) => {
                if let Some(mut entry) = parse_vimgrep_line(&line) {
                    if entry.path.is_relative() {
                        entry.path = root.join(entry.path);
                    }
                    entries.push(entry);
                    if entries.len() == GREP_PICKER_RESULT_LIMIT {
                        break;
                    }
                }
            }
            Err(error) => {
                read_error = Some(error.to_string());
                break;
            }
        }
    }
    let mut child = child_slot
        .lock()
        .map_err(|_| "repository search child lock is poisoned".to_owned())?
        .take()
        .ok_or_else(|| "repository search child vanished".to_owned())?;
    let limited = entries.len() == GREP_PICKER_RESULT_LIMIT;
    if limited || cancelled.load(Ordering::Acquire) {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|error| format!("wait for rg: {error}"))?;
    if cancelled.load(Ordering::Acquire) {
        return Err("cancelled".to_owned());
    }
    if let Some(error) = read_error {
        return Err(format!("read rg output: {error}"));
    }
    if !limited && !status.success() && status.code() != Some(1) {
        return Err(format!("rg exited with {status}"));
    }
    Ok(entries)
}

impl App {
    pub(super) fn start_file_picker(&mut self, query: &str) -> Result<()> {
        self.picker_directory = None;
        let output = Command::new("rg")
            .args(["--files", "--null"])
            .output()
            .context("enumerate workspace files with rg")?;
        if !output.status.success() {
            bail!(
                "file enumeration failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        self.picker_files = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .take(100_000)
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect();
        self.last_picker_source = Some(PickerSource::Files);
        self.begin_path_picker(query);
        Ok(())
    }

    pub(super) fn picker_overlay(&self) -> Option<PickerOverlay> {
        let prompt = self
            .prompt
            .as_ref()
            .filter(|prompt| prompt.kind.is_picker())?;
        let source = self.last_picker_source.unwrap_or(PickerSource::Files);
        let title = match source {
            PickerSource::Files => "Find Files".to_owned(),
            PickerSource::Browser => self.picker_directory.as_ref().map_or_else(
                || "File Browser".to_owned(),
                |path| format!("File Browser · {}", path.display()),
            ),
            PickerSource::Grep => "Live Grep".to_owned(),
            PickerSource::Buffers => "Buffers".to_owned(),
            PickerSource::Recent => "Oldfiles".to_owned(),
            PickerSource::Jumps => "Jumplist".to_owned(),
            PickerSource::Diagnostics => "Diagnostics".to_owned(),
        };
        let rows = match prompt.kind {
            PromptKind::FilePicker | PromptKind::FileBrowser => {
                let workspace_root = self.workspace_root();
                self.picker_matches
                    .iter()
                    .map(|path| {
                        let is_directory = path.is_dir();
                        let label = if prompt.kind == PromptKind::FileBrowser {
                            path.file_name().map_or_else(
                                || path.display().to_string(),
                                |name| name.to_string_lossy().into_owned(),
                            )
                        } else if let Ok(relative) = path.strip_prefix(&workspace_root) {
                            relative.display().to_string()
                        } else if path.is_absolute() {
                            path.file_name().map_or_else(
                                || path.display().to_string(),
                                |name| name.to_string_lossy().into_owned(),
                            )
                        } else {
                            path.display().to_string()
                        };
                        let detail = if is_directory {
                            "directory".to_owned()
                        } else if path.is_absolute() && !path.starts_with(&workspace_root) {
                            path.parent()
                                .map_or_else(String::new, |parent| parent.display().to_string())
                        } else {
                            String::new()
                        };
                        PickerOverlayRow {
                            label: format!("{label}{}", if is_directory { "/" } else { "" }).into(),
                            detail: detail.into(),
                        }
                    })
                    .collect()
            }
            PromptKind::Grep => self
                .quickfix
                .iter()
                .map(|entry| PickerOverlayRow {
                    label: format!("{}:{}:{}", entry.path.display(), entry.line, entry.column)
                        .into(),
                    detail: compact(&entry.text, 80).into(),
                })
                .collect(),
            PromptKind::Location => self
                .filtered_locations(&prompt.buffer)
                .into_iter()
                .map(|entry| PickerOverlayRow {
                    label: format!("{}:{}:{}", entry.path.display(), entry.line, entry.column)
                        .into(),
                    detail: compact(&entry.text, 80).into(),
                })
                .collect(),
            _ => return None,
        };
        Some(PickerOverlay {
            title: title.into(),
            prompt: prompt.buffer.as_str().into(),
            rows,
            selected: self.picker_index,
            preview_title: self.picker_preview_title.as_str().into(),
            preview: self.picker_preview.as_str().into(),
            preview_scroll: self.picker_preview_scroll,
            preview_highlight_line: self.picker_preview_highlight_line,
            preview_decorations: self.picker_preview_decorations.clone(),
            footer: "↑/↓ select  ⏎ open  C-u/d preview  Esc close".into(),
        })
    }

    fn selected_picker_target(&self) -> Option<(PathBuf, Option<QuickfixEntry>)> {
        let prompt = self.prompt.as_ref()?;
        match prompt.kind {
            PromptKind::FilePicker | PromptKind::FileBrowser => self
                .picker_matches
                .get(self.picker_index)
                .cloned()
                .map(|path| (path, None)),
            PromptKind::Grep | PromptKind::Location => self
                .selected_picker_entry()
                .map(|entry| (entry.path.clone(), Some(entry))),
            _ => None,
        }
    }

    fn selected_picker_entry(&self) -> Option<QuickfixEntry> {
        let prompt = self.prompt.as_ref()?;
        match prompt.kind {
            PromptKind::Grep => self.quickfix.get(self.picker_index).cloned(),
            PromptKind::Location => self
                .filtered_locations(&prompt.buffer)
                .get(self.picker_index)
                .cloned(),
            _ => None,
        }
    }

    pub(super) fn refresh_picker_preview(&mut self) {
        self.picker_preview_scroll = 0;
        self.picker_preview_highlight_line = None;
        self.picker_preview_decorations.clear();
        let Some((path, selected_entry)) = self.selected_picker_target() else {
            self.picker_preview_title = "No preview".to_owned();
            self.picker_preview = "No matching entries".to_owned();
            return;
        };
        self.picker_preview_title = path.display().to_string();
        if path.is_dir() {
            self.picker_preview = std::fs::read_dir(&path).map_or_else(
                |error| format!("Unable to preview directory: {error}"),
                |entries| {
                    let mut entries = entries
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .collect::<Vec<_>>();
                    entries.sort_by_key(|entry| {
                        (!entry.is_dir(), entry.file_name().map(ToOwned::to_owned))
                    });
                    entries
                        .into_iter()
                        .take(2_000)
                        .map(|entry| {
                            format!(
                                "{}{}",
                                entry.file_name().map_or_else(
                                    || entry.display().to_string(),
                                    |name| { name.to_string_lossy().into_owned() }
                                ),
                                if entry.is_dir() { "/" } else { "" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                },
            );
            return;
        }
        let in_memory = std::iter::once(&self.active)
            .chain(self.inactive.iter())
            .find(|buffer| {
                buffer
                    .document
                    .presentation_path()
                    .is_some_and(|buffer_path| same_path(buffer_path, &path))
            })
            .map(|buffer| buffer.editor.contents());
        self.picker_preview = in_memory.unwrap_or_else(|| match std::fs::read(&path) {
            Ok(bytes) if bytes.iter().take(8_192).any(|byte| *byte == 0) => {
                format!("Binary file · {} bytes", bytes.len())
            }
            Ok(bytes) => {
                let truncated = bytes.len() > 1024 * 1024;
                let visible = &bytes[..bytes.len().min(1024 * 1024)];
                let mut text = String::from_utf8_lossy(visible).into_owned();
                if truncated {
                    text.push_str("\n… preview truncated at 1 MiB");
                }
                text
            }
            Err(error) => format!("Unable to preview file: {error}"),
        });
        let bundle = language_bundle(Some(&path));
        let revision = DocumentRevision::new(0);
        let spans = self
            .provider
            .highlight_now(
                stable_document_id(Some(&path)),
                revision,
                Arc::from(self.picker_preview.as_str()),
                bundle,
            )
            .unwrap_or_else(|_| lexical_highlight_text(&self.picker_preview));
        self.picker_preview_decorations = spans
            .into_iter()
            .map(|span| provider_decoration(span, self.theme))
            .collect();
        if let Some(entry) = selected_entry {
            let line = entry.line.saturating_sub(1);
            self.picker_preview_scroll = line.saturating_sub(5);
            if let Some(range) = entry.selection_byte_range(&self.picker_preview) {
                self.picker_preview_decorations.push(DecorationSpan {
                    range,
                    style: CellStyle {
                        foreground: None,
                        background: Some(CellColor::Rgb(self.theme.surface0)),
                        ..CellStyle::default()
                    },
                    priority: u32::MAX,
                });
            } else {
                self.picker_preview_highlight_line = Some(line);
            }
        }
    }

    pub(super) fn start_file_browser(&mut self) -> Result<()> {
        let directory = env::current_dir().context("locate current directory")?;
        self.start_file_browser_at(&directory)
    }

    pub(super) fn start_file_browser_at(&mut self, directory: &Path) -> Result<()> {
        let directory = std::fs::canonicalize(directory)
            .with_context(|| format!("open browser directory {}", directory.display()))?;
        let mut entries = std::fs::read_dir(&directory)
            .with_context(|| format!("read browser directory {}", directory.display()))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        entries.sort_by_key(|path| (!path.is_dir(), path.file_name().map(ToOwned::to_owned)));
        self.picker_files = entries
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        self.picker_directory = Some(directory);
        self.last_picker_source = Some(PickerSource::Browser);
        self.prompt = Some(Prompt::new(PromptKind::FileBrowser));
        self.update_file_picker();
        Ok(())
    }

    pub(super) fn browse_parent(&mut self) -> Result<()> {
        let parent = self
            .picker_directory
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        if let Some(parent) = parent {
            self.start_file_browser_at(&parent)?;
        }
        Ok(())
    }

    pub(super) fn start_buffer_picker(&mut self) -> Result<()> {
        self.picker_directory = None;
        self.picker_files = std::iter::once(&self.active)
            .chain(self.inactive.iter())
            .filter_map(|buffer| buffer.document.presentation_path())
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        if self.picker_files.is_empty() {
            self.message = "no named buffers".to_owned();
            return Ok(());
        }
        self.last_picker_source = Some(PickerSource::Buffers);
        self.begin_path_picker("");
        Ok(())
    }

    pub(super) fn start_recent_picker(&mut self) -> Result<()> {
        self.picker_files = self
            .recent_files
            .iter()
            .filter(|path| path.exists())
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        if self.picker_files.is_empty() {
            self.message = "oldfiles is empty".to_owned();
            return Ok(());
        }
        self.last_picker_source = Some(PickerSource::Recent);
        self.begin_path_picker("");
        Ok(())
    }

    pub(super) fn start_jumplist_picker(&mut self) -> Result<()> {
        self.quickfix = if self.jump_history.is_empty() {
            let Some(path) = self
                .active
                .document
                .presentation_path()
                .map(Path::to_path_buf)
            else {
                self.message = "jumplist has no named locations".to_owned();
                return Ok(());
            };
            self.active
                .editor
                .jumplist()
                .enumerate()
                .map(|(index, byte)| {
                    let line = self.active.editor.text().line_of_byte(byte);
                    let line_start = self.active.editor.text().byte_of_line(line);
                    QuickfixEntry {
                        path: path.clone(),
                        line: line + 1,
                        column: byte.saturating_sub(line_start) + 1,
                        selection_end: None,
                        column_utf16: false,
                        text: format!("jump {}", index + 1),
                    }
                })
                .collect()
        } else {
            self.jump_history
                .iter()
                .enumerate()
                .filter_map(|(index, jump)| {
                    let buffer = if self
                        .active
                        .document
                        .presentation_path()
                        .is_some_and(|path| same_path(path, &jump.path))
                    {
                        Some(&self.active)
                    } else {
                        self.inactive.iter().find(|buffer| {
                            buffer
                                .document
                                .presentation_path()
                                .is_some_and(|path| same_path(path, &jump.path))
                        })
                    }?;
                    let line = buffer.editor.text().line_of_byte(jump.byte);
                    let line_start = buffer.editor.text().byte_of_line(line);
                    Some(QuickfixEntry {
                        path: jump.path.clone(),
                        line: line + 1,
                        column: jump.byte.saturating_sub(line_start) + 1,
                        selection_end: None,
                        column_utf16: false,
                        text: format!("jump {}", index + 1),
                    })
                })
                .collect()
        };
        self.start_location_picker(PickerSource::Jumps, "")
    }

    pub(super) fn start_diagnostic_picker(&mut self) -> Result<()> {
        self.refresh_diagnostics()?;
        self.quickfix = self
            .diagnostics
            .iter()
            .map(DiagnosticEntry::quickfix)
            .collect();
        self.start_location_picker(PickerSource::Diagnostics, "")
    }

    pub(super) fn start_location_picker(
        &mut self,
        source: PickerSource,
        query: &str,
    ) -> Result<()> {
        if self.quickfix.is_empty() {
            self.message = match source {
                PickerSource::Jumps => "jumplist is empty".to_owned(),
                PickerSource::Diagnostics => "no diagnostics".to_owned(),
                _ => "no locations".to_owned(),
            };
            return Ok(());
        }
        self.last_picker_source = Some(source);
        self.last_picker_query = query.to_owned();
        self.picker_index = 0;
        self.prompt = Some(Prompt {
            kind: PromptKind::Location,
            buffer: query.to_owned(),
            history_index: None,
        });
        self.update_location_picker();
        Ok(())
    }

    pub(super) fn begin_path_picker(&mut self, query: &str) {
        self.picker_directory = None;
        self.prompt = Some(Prompt {
            kind: PromptKind::FilePicker,
            buffer: query.to_owned(),
            history_index: None,
        });
        self.update_file_picker();
    }

    pub(super) fn start_grep_picker(&mut self, query: &str) -> Result<()> {
        self.last_picker_source = Some(PickerSource::Grep);
        self.prompt = Some(Prompt {
            kind: PromptKind::Grep,
            buffer: query.to_owned(),
            history_index: None,
        });
        self.picker_index = 0;
        self.update_grep_picker();
        Ok(())
    }

    pub(super) fn update_prompt_picker(&mut self) -> Result<()> {
        match self.prompt.as_ref().map(|prompt| prompt.kind) {
            Some(PromptKind::FilePicker | PromptKind::FileBrowser) => {
                self.update_file_picker();
                Ok(())
            }
            Some(PromptKind::Grep) => {
                self.update_grep_picker();
                Ok(())
            }
            Some(PromptKind::Location) => {
                self.update_location_picker();
                Ok(())
            }
            Some(PromptKind::Command) => {
                self.update_inccommand_preview();
                Ok(())
            }
            Some(PromptKind::SearchForward | PromptKind::SearchBackward) => {
                self.update_incremental_search()
            }
            _ => Ok(()),
        }
    }

    pub(super) fn update_inccommand_preview(&mut self) {
        let Some(command) = self
            .prompt
            .as_ref()
            .filter(|prompt| prompt.kind == PromptKind::Command)
            .map(|prompt| prompt.buffer.clone())
        else {
            return;
        };
        let Some(parsed) = parse_inccommand_substitute(&command) else {
            self.message.clear();
            return;
        };
        let substitute = match parsed {
            ExCommand::Substitute {
                range,
                pattern,
                replacement,
                flags,
            } => self.resolve_byte_range(range.as_ref()).and_then(|range| {
                self.resolve_substitute(&pattern, &replacement, flags, vec![range])
            }),
            ExCommand::SubstituteRepeat {
                range,
                use_search_pattern,
                flags,
            } => self.resolve_byte_range(range.as_ref()).and_then(|range| {
                self.resolve_repeated_substitute(use_search_pattern, flags, vec![range])
            }),
            _ => return,
        };
        let Ok(substitute) = substitute else {
            self.message.clear();
            return;
        };
        let text = self.active.editor.contents();
        let Ok(pattern) = self
            .active
            .editor
            .compile_search_pattern(&substitute.needle, substitute.case_override)
        else {
            self.message = "inccommand: invalid pattern".to_owned();
            return;
        };
        let replacement = VimReplacement::new(substitute.replacement);
        let Ok(edits) = plan_substitution_edits(
            &text,
            &pattern,
            &replacement,
            &substitute.ranges,
            substitute.global,
            || Ok(()),
        ) else {
            self.message.clear();
            return;
        };
        if edits.is_empty() {
            self.message = "inccommand: 0 substitutions".to_owned();
            return;
        }
        let Ok(transaction) = Transaction::new(self.active.editor.revision(), edits) else {
            self.message.clear();
            return;
        };
        self.message = format!(
            "inccommand: {}",
            substitution_message(transaction.edit_count(), true, &text, &transaction)
        );
    }

    pub(super) fn update_file_picker(&mut self) {
        let Some(prompt) = self.prompt.as_ref().filter(|prompt| {
            matches!(
                prompt.kind,
                PromptKind::FilePicker | PromptKind::FileBrowser
            )
        }) else {
            return;
        };
        let query = prompt.buffer.clone();
        self.last_picker_query.clone_from(&query);
        self.picker_matches = fuzzy_rank(&query, self.picker_files.iter().map(String::as_str))
            .into_iter()
            .take(128)
            .map(PathBuf::from)
            .collect();
        self.picker_index = 0;
        self.update_file_picker_message();
        self.refresh_picker_preview();
    }

    pub(super) fn update_grep_picker(&mut self) {
        let Some(query) = self
            .prompt
            .as_ref()
            .filter(|prompt| prompt.kind == PromptKind::Grep)
            .map(|prompt| prompt.buffer.clone())
        else {
            return;
        };
        self.last_picker_query.clone_from(&query);
        self.picker_index = 0;
        self.grep_generation = self.grep_generation.saturating_add(1);
        self.grep_pending = None;
        self.grep_due = None;
        if let Some(task) = self.grep_task.take() {
            task.cancel();
        }
        if query.is_empty() {
            self.quickfix.clear();
            self.update_grep_picker_message();
            self.refresh_picker_preview();
            return;
        }
        self.quickfix.clear();
        self.picker_preview_title = "Searching…".to_owned();
        self.picker_preview = "Repository search is running in the background".to_owned();
        self.picker_preview_decorations.clear();
        self.message = format!("searching for {query:?}…");
        self.grep_pending = Some(GrepPickerRequest {
            generation: self.grep_generation,
            query,
            root: self
                .active
                .document
                .presentation_path()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.root_workspace.clone()),
        });
        self.grep_due = Some(Instant::now() + Duration::from_millis(50));
    }

    pub(super) fn cancel_grep_picker(&mut self) {
        self.grep_due = None;
        self.grep_pending = None;
        if let Some(task) = self.grep_task.take() {
            task.cancel();
        }
    }

    pub(super) fn poll_grep_picker(&mut self) -> bool {
        let mut changed = false;
        if let Some(task) = &self.grep_task {
            let result = match task.receiver.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("repository search worker disconnected".to_owned()))
                }
            };
            if let Some(result) = result {
                let task = self.grep_task.take().expect("grep task exists");
                let current = task.generation == self.grep_generation
                    && self.prompt.as_ref().is_some_and(|prompt| {
                        prompt.kind == PromptKind::Grep && prompt.buffer == task.query
                    });
                if current {
                    match result {
                        Ok(entries) => {
                            self.quickfix = entries;
                            self.picker_index = 0;
                            self.update_grep_picker_message();
                            self.refresh_picker_preview();
                        }
                        Err(error) => self.show_info(format!("live grep: {error}")),
                    }
                    changed = true;
                }
            }
        }
        if self.grep_task.is_none() && self.grep_due.is_some_and(|due| Instant::now() >= due) {
            self.grep_due = None;
            if let Some(request) = self.grep_pending.take() {
                match launch_grep_picker_task(request) {
                    Ok(task) => self.grep_task = Some(task),
                    Err(error) => {
                        self.show_info(format!("live grep: {error:#}"));
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    pub(super) fn open_selected_grep_result(&mut self, query: &str) -> Result<()> {
        let entry = self
            .quickfix
            .get(self.picker_index)
            .cloned()
            .ok_or_else(|| anyhow!("no grep matches for {query:?}"))?;
        self.navigate_to_entry(&entry)?;
        self.message = format!("{}:{}: {}", entry.line, entry.column, entry.text);
        Ok(())
    }

    pub(super) fn open_selected_location(&mut self, query: &str) -> Result<()> {
        let entry = self
            .filtered_locations(query)
            .get(self.picker_index)
            .cloned()
            .ok_or_else(|| anyhow!("no matching location"))?;
        self.navigate_to_entry(&entry)?;
        self.message = format!("{}:{}: {}", entry.line, entry.column, entry.text);
        Ok(())
    }

    pub(super) fn filtered_locations(&self, query: &str) -> Vec<QuickfixEntry> {
        if query.is_empty() {
            return self.quickfix.clone();
        }
        let candidates = self
            .quickfix
            .iter()
            .map(QuickfixEntry::display)
            .collect::<Vec<_>>();
        let ranked = fuzzy_rank(query, candidates.iter().map(String::as_str));
        ranked
            .into_iter()
            .filter_map(|label| {
                candidates
                    .iter()
                    .position(|candidate| candidate == label)
                    .and_then(|index| self.quickfix.get(index).cloned())
            })
            .collect()
    }

    pub(super) fn update_location_picker(&mut self) {
        let query = self
            .prompt
            .as_ref()
            .map_or_else(String::new, |prompt| prompt.buffer.clone());
        self.last_picker_query.clone_from(&query);
        let locations = self.filtered_locations(&query);
        self.picker_index = self.picker_index.min(locations.len().saturating_sub(1));
        self.message = locations.get(self.picker_index).map_or_else(
            || "no matching locations".to_owned(),
            |entry| {
                format!(
                    "[{}/{}] {}",
                    self.picker_index + 1,
                    locations.len(),
                    entry.display()
                )
            },
        );
        self.refresh_picker_preview();
    }

    pub(super) fn resume_picker(&mut self) -> Result<()> {
        let query = self.last_picker_query.clone();
        match self.last_picker_source.unwrap_or(PickerSource::Files) {
            PickerSource::Files => self.start_file_picker(&query),
            PickerSource::Browser => self.start_file_browser(),
            PickerSource::Grep => self.start_grep_picker(&query),
            PickerSource::Buffers => self.start_buffer_picker(),
            PickerSource::Recent => self.start_recent_picker(),
            PickerSource::Jumps => self.start_jumplist_picker(),
            PickerSource::Diagnostics => self.start_diagnostic_picker(),
        }
    }

    pub(super) fn word_under_cursor(&self) -> Option<String> {
        let text = self.active.editor.contents();
        let cursor = self.active.editor.primary_cursor().min(text.len());
        let mut start = cursor;
        while start > 0 {
            let previous = text[..start].char_indices().next_back()?.0;
            let character = text[previous..start].chars().next()?;
            if !character.is_alphanumeric() && character != '_' {
                break;
            }
            start = previous;
        }
        let mut end = cursor;
        while end < text.len() {
            let character = text[end..].chars().next()?;
            if !character.is_alphanumeric() && character != '_' {
                break;
            }
            end += character.len_utf8();
        }
        (start < end).then(|| text[start..end].to_owned())
    }

    pub(super) fn move_picker(&mut self, direction: isize) {
        let location_query = self.prompt.as_ref().and_then(|prompt| {
            (prompt.kind == PromptKind::Location).then_some(prompt.buffer.as_str())
        });
        let length = if let Some(query) = location_query {
            self.filtered_locations(query).len()
        } else if self
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.kind == PromptKind::Grep)
        {
            self.quickfix.len()
        } else {
            self.picker_matches.len()
        };
        if length == 0 {
            self.picker_index = 0;
        } else if direction < 0 {
            self.picker_index = self.picker_index.saturating_sub(1);
        } else {
            self.picker_index = self.picker_index.saturating_add(1).min(length - 1);
        }
        if self
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.kind == PromptKind::Grep)
        {
            self.update_grep_picker_message();
        } else if self
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.kind == PromptKind::Location)
        {
            self.update_location_picker();
        } else {
            self.update_file_picker_message();
        }
        self.refresh_picker_preview();
    }

    pub(super) fn update_grep_picker_message(&mut self) {
        self.message = self.quickfix.get(self.picker_index).map_or_else(
            || "no grep matches".to_owned(),
            |entry| {
                format!(
                    "[{}/{}] {}:{}:{}  {}",
                    self.picker_index + 1,
                    self.quickfix.len(),
                    entry.path.display(),
                    entry.line,
                    entry.column,
                    compact(&entry.text, 80)
                )
            },
        );
    }

    pub(super) fn update_file_picker_message(&mut self) {
        self.message = if self.picker_matches.is_empty() {
            "no matching files".to_owned()
        } else {
            let selected = self.picker_matches[self.picker_index].display();
            let nearby = self
                .picker_matches
                .iter()
                .skip(self.picker_index.saturating_add(1))
                .take(3)
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("  ");
            format!(
                "[{}/{}] {selected}{}",
                self.picker_index + 1,
                self.picker_matches.len(),
                if nearby.is_empty() {
                    String::new()
                } else {
                    format!("  ·  {nearby}")
                }
            )
        };
    }

    pub(super) fn buffer(&self, buffer_id: BufferId) -> Option<&BufferState> {
        if self.active.buffer_id == buffer_id {
            Some(&self.active)
        } else {
            self.inactive
                .iter()
                .find(|buffer| buffer.buffer_id == buffer_id)
        }
    }

    pub(super) fn move_prompt_history(&mut self, direction: isize) {
        let Some(prompt) = &self.prompt else {
            return;
        };
        let history = match prompt.kind {
            PromptKind::Command => &self.client_state.command_history,
            PromptKind::SearchForward | PromptKind::SearchBackward => {
                &self.client_state.search_history
            }
            PromptKind::Expression
            | PromptKind::FilePicker
            | PromptKind::FileBrowser
            | PromptKind::Grep
            | PromptKind::Location
            | PromptKind::Rename
            | PromptKind::ConditionalBreakpoint => return,
        };
        if history.is_empty() {
            return;
        }
        let current = prompt.history_index.unwrap_or(history.len());
        let next = if direction < 0 {
            current.saturating_sub(1)
        } else {
            current.saturating_add(1).min(history.len())
        };
        if let Some(prompt) = &mut self.prompt {
            prompt.history_index = (next < history.len()).then_some(next);
            prompt.buffer = history
                .get(next)
                .map_or_else(String::new, ToString::to_string);
        }
    }
}
