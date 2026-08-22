use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{BufRead, BufReader, Cursor, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(test)]
use std::sync::Mutex;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use ls_types::{CodeActionOrCommand, CodeActionResponse, CodeLens, Command as LspCommand, DocumentChangeOperation, DocumentChanges, OneOf, WorkspaceEdit};
use merge3::{Merge3, MergeGroup};
use parking_lot::Mutex as LocalMutex;
use serde::{Deserialize, Serialize};
use wren_client_state::{ClientViewStateStore, DurableClientState};
use wren_command::{CancellationToken, TaskContext, TaskFailure, TaskRunner};
use wren_config::{CommandRegistry, WorkspaceTrust, executable_hash, parse_and_validate};
use wren_engine::{
    CaseOverride, DurableUndoState, Editor, EditorState, EngineError, FrameText, Mode, SearchDirection, TransactionBatch, VimPattern, VimReplacement,
    resolve_previous_replacement,
};
use wren_grammar::{
    BufferAction, ExAddress, ExCommand, ExRange, ExpressionContext, KeyCode, KeyEvent, Modifiers, ParseState, SubstituteFlags, TabAction, Value,
    evaluate_expression, ex_command_completions, expression_editor_text, parse_ex,
};
use wren_position::{utf16_column_to_byte, utf16_position_to_byte};
use wren_presenter::Presenter;
#[cfg(test)]
use wren_provider::ProviderActor;
#[cfg(not(test))]
use wren_provider::ProviderSupervisor;
use wren_provider::{
    CompletionCandidate, CompletionSession, HighlightSpan, ProviderRequest, ProviderResponse, bundled_language_id, fuzzy_rank, highlight_text,
    lexical_highlight_text,
};
use wren_session::{DocumentEncoding, LocalDocument, LocalWal, MutationOutbox, OpenedDocument, RecoveredState, SaveWarning, SessionAuthority, SessionJournal};
use wren_term::{ClipboardSelection, MouseAction, SystemTerminalBackend, TerminaBackend, TerminalDimensions, TerminalInput, TerminalKey, TerminalKeyCode};
use wren_text::{DefaultText, TextStore};
use wren_types::{
    Anchor, Bias, BufferId, ClientId, ClientMutation, ClientSequence, CommandClass, CommandSchema, CommandTask, CommandTaskId, DocumentClass, DocumentId,
    DocumentMutation, DocumentRevision, DurableJumpEntry, Edit, EditProposal, Effects, Freshness, FreshnessKey, LanguageBundle, MutationId, Priority,
    ProviderDemand, SemanticGroupId, SemanticGroupKind, SessionId, StateDelta, Transaction, identifier_prefix_start, identifier_range, merge_ranges,
    ranges_overlap, stable_hash,
};
#[cfg(test)]
use wren_view::CatppuccinPalette;
use wren_view::{
    AceJumpOverlay, AceJumpTarget, CatppuccinColor, CatppuccinFlavor, Cell as ViewCell, CellColor, CellRow, CellStyle, ClientViewModel, CompletionOverlay,
    DebugOverlay, DebugPanel, DecorationSpan, DesiredGrid, EditorTheme, LineDecoration, MenuOverlayRow, PickerOverlay, RgbColor, SharedDecorations, SplitAxis,
    StatusOverlay, StatusSegment, TerminalSidebar, TextPopup, ViewportLayout, WindowDirection,
};
use wren_workflow::{
    GitHunk, LspClient, LspPosition, LspTextEdit, PtySession, TaskSpec as WorkflowTaskSpec, TaskSupervisor, TerminalColor, WorkflowError, git_hunks,
    lower_lsp_text_edits, run_formatter_until_cancelled,
};

mod provider_worker;
use provider_worker::*;

mod syntax;
use syntax::*;

mod tooling;
use tooling::*;

mod git;
use git::*;

mod lsp;
use lsp::*;

mod persistence;
use persistence::*;

mod startup;
use startup::{ANIMATION_FRAME_PERIOD as STARTUP_ANIMATION_FRAME_PERIOD, StartupScreen};

#[path = "app/agent.rs"]
mod app_agent;
#[path = "app/commands.rs"]
mod app_commands;
#[path = "app/editor_state.rs"]
mod app_editor_state;
#[path = "app/input.rs"]
mod app_input;
#[path = "app/interaction.rs"]
mod app_interaction;
#[path = "app/lifecycle.rs"]
mod app_lifecycle;
#[path = "app/lsp_actions.rs"]
mod app_lsp_actions;
#[path = "app/picker.rs"]
mod app_picker;
#[path = "app/providers.rs"]
mod app_providers;
#[path = "app/terminal.rs"]
mod app_terminal;

#[cfg(feature = "benchmarking")]
mod latency;
#[cfg(feature = "benchmarking")]
pub use latency::{
    ProductionLatencyReport, ProductionLatencySample, TilingPerformanceReport, TilingPerformanceSample, run_production_latency_probe,
    run_tiling_performance_probe,
};

const HELP: &str = include_str!("help.txt");

const MESSAGES_BUFFER_NAME: &str = "[Messages]";
const GIT_HUNK_IDLE_PERIOD: Duration = Duration::from_millis(50);
const LSP_START_IDLE_PERIOD: Duration = Duration::from_millis(750);
const LSP_SEMANTIC_IDLE_PERIOD: Duration = Duration::from_millis(750);
type PresenterBackend = TerminaBackend<std::io::Stdout>;

pub fn main_entry() -> Result<()> {
    if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--internal-provider-host")) {
        wren_scheduling::mark_background();
        wren_provider::serve(std::io::stdin().lock(), std::io::stdout().lock())?;
        return Ok(());
    }
    let cli = Cli::parse(env::args().skip(1))?;
    if cli.help {
        print!("{HELP}");
        return Ok(());
    }
    if cli.version {
        println!("wren {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    run_editor(&cli)
}

fn run_editor(cli: &Cli) -> Result<()> {
    let mut app = App::open(cli.path.as_deref(), cli.line)?;
    let mut terminal = SystemTerminalBackend::open().context("open interactive terminal")?;
    let dimensions = terminal.size().context("query terminal size")?;
    let (columns, rows) = (dimensions.columns, dimensions.rows);
    let presenter = Presenter::start(TerminaBackend::new(std::io::stdout()))?;
    let mut layout = ViewportLayout::new(columns, rows);
    layout.configure_dotfile_profile();
    app.resize_terminal_to(dimensions);
    app.schedule_provider_refreshes(layout.height);
    presenter.publish(desired_frame(&mut layout, &app))?;
    let mut pending_input = None;
    let mut next_startup_frame = Instant::now() + STARTUP_ANIMATION_FRAME_PERIOD;

    while !app.quit {
        let mut needs_render = false;
        let input = match pending_input.take() {
            Some(input) => Some(input),
            None => terminal.poll_input(Some(Duration::from_millis(4))).context("read terminal input")?,
        };
        if let Some(input) = input {
            let (input, next) = coalesce_mouse_scroll_input(input, |timeout| terminal.poll_input(Some(timeout)).context("drain terminal input"))?;
            pending_input = next;
            needs_render |= handle_terminal_event(input, &mut app, &mut terminal, &presenter, &mut layout);
        }
        // Quitting is a local editor action. Once accepted, leave the event
        // loop before polling providers or language servers so their state can
        // never delay or turn a successful quit into an error exit.
        if app.quit {
            break;
        }
        if !std::mem::take(&mut app.foreground_frame_pending) {
            needs_render |= poll_app_work(&mut app)?;
        }
        let now = Instant::now();
        if app.shows_startup_screen() && now >= next_startup_frame {
            while next_startup_frame <= now {
                next_startup_frame += STARTUP_ANIMATION_FRAME_PERIOD;
            }
            needs_render = true;
        } else if !app.shows_startup_screen() {
            next_startup_frame = now + STARTUP_ANIMATION_FRAME_PERIOD;
        }
        app.capture_debug_output();
        presenter.check_failure()?;
        // Provider refinement is independently debounced from rendering. Poll
        // its due time even while input is idle so Tree-sitter catches up after
        // a typing burst without requiring another keypress.
        app.schedule_provider_refreshes(layout.height);
        if !app.quit && needs_render {
            presenter.publish(desired_frame(&mut layout, &app))?;
        }
    }
    app.flush_wal()?;
    presenter.finish()?;
    Ok(())
}

const fn clipboard_selection(register: char) -> ClipboardSelection {
    if register == '*' { ClipboardSelection::Primary } else { ClipboardSelection::Clipboard }
}

fn load_clipboard_for_input(app: &mut App, terminal: &mut SystemTerminalBackend, event: &TerminalInput) {
    let Some(register) = app.clipboard_register_for_paste(event) else {
        return;
    };
    let clipboard = match terminal.paste_osc52(clipboard_selection(register), Duration::from_secs(1)) {
        Ok(Some(text)) => Some(text),
        Ok(None) => system_clipboard_text(register),
        Err(error) => {
            let fallback = system_clipboard_text(register);
            if fallback.is_none() {
                app.show_error(format!("clipboard: {error}"));
            }
            fallback
        }
    };
    if let Some(text) = clipboard {
        app.set_clipboard_register(register, text);
    }
}

fn handle_terminal_event(
    event: TerminalInput,
    app: &mut App,
    terminal: &mut SystemTerminalBackend,
    output: &Presenter<PresenterBackend>,
    layout: &mut ViewportLayout,
) -> bool {
    if let TerminalInput::Resized(dimensions) = event {
        layout.resize(dimensions.columns, dimensions.rows);
        app.resize_terminal_to(dimensions);
        return true;
    }
    if matches!(event, TerminalInput::Ignored) {
        return false;
    }
    load_clipboard_for_input(app, terminal, &event);
    app.foreground_frame_pending = true;
    let result = match event {
        TerminalInput::Mouse { action: action @ (MouseAction::Click | MouseAction::Drag | MouseAction::Release), column, row } => {
            app.handle_mouse_pointer(layout, action, column, row)
        }
        event => app.handle_input(event),
    };
    if let Err(error) = result {
        app.show_error(error);
    }
    app.capture_debug_output();
    for (register, text) in app.take_clipboard_writes() {
        if let Err(error) = output.try_copy_osc52(clipboard_selection(register), text) {
            app.show_error(format!("clipboard: {error}"));
        }
    }
    true
}

type AppPollPlugin = fn(&mut App) -> Result<bool>;

const APP_POLL_PLUGINS: &[AppPollPlugin] = &[
    App::poll_task_results,
    App::poll_agent_terminal,
    |app| Ok(app.poll_provider_results()),
    |app| Ok(app.poll_git_hunk_results()),
    |app| Ok(app.poll_grep_picker()),
    |app| Ok(app.poll_lsp_start_due()),
    |app| Ok(app.poll_lsp()),
    App::poll_lsp_semantic_due,
    App::poll_terminal,
    App::poll_mapping_timeout,
    |app| Ok(app.poll_popup_timeout()),
];

fn poll_app_work(app: &mut App) -> Result<bool> {
    APP_POLL_PLUGINS.iter().try_fold(false, |changed, poll| Ok(changed | poll(app)?))
}

fn coalesce_mouse_scroll_input(
    first: TerminalInput,
    mut poll: impl FnMut(Duration) -> Result<Option<TerminalInput>>,
) -> Result<(TerminalInput, Option<TerminalInput>)> {
    let TerminalInput::Mouse { action: MouseAction::Scroll(mut lines), mut column, mut row } = first else {
        return Ok((first, None));
    };
    // Ghostty can emit wheel events faster than a frame can be presented, and
    // Termina can expose bytes from one burst over several decoder reads. Give
    // that decoder a tiny scroll-only grace period, then drain a bounded burst
    // into one viewport transaction so old events never queue behind renders.
    for _ in 0..255 {
        match poll(Duration::from_millis(2))? {
            Some(TerminalInput::Mouse { action: MouseAction::Scroll(next), column: next_column, row: next_row }) => {
                lines = lines.saturating_add(next);
                column = next_column;
                row = next_row;
            }
            Some(TerminalInput::Ignored) => {}
            Some(input) => {
                return Ok((TerminalInput::Mouse { action: MouseAction::Scroll(lines), column, row }, Some(input)));
            }
            None => break,
        }
    }
    Ok((TerminalInput::Mouse { action: MouseAction::Scroll(lines), column, row }, None))
}

#[cfg(test)]
fn input_requires_render(input: &TerminalInput) -> bool {
    !matches!(input, TerminalInput::Ignored)
}

fn desired_frame(layout: &mut ViewportLayout, app: &App) -> DesiredGrid {
    layout.set_theme(app.theme);
    layout.set_terminal_sidebar_visible(app.agent_sidebar_visible && !app.input_focus.is_terminal());
    if app.input_focus.is_terminal() {
        let mut grid = app.desired_terminal_grid(layout);
        grid.resolve_theme(app.theme);
        return grid;
    }
    let frame = app.active.editor.frame();
    layout.ensure_cursor_visible(&frame, 1);
    let frames = buffer_frames(app, frame);
    let prompt = prompt_text(app);
    let mut decorations = syntax_decorations(app);
    add_search_decorations(app, &mut decorations);
    let line_decorations = add_buffer_decorations(app, &mut decorations);
    add_selection_decoration(app, &mut decorations);
    let decoration_layers = decorations.finish();
    let decorations = layout.compose_shared_decoration_layers(&decoration_layers);
    let mut grid = layout.desired_workspace_grid_with_shared_decorations(&app.views, &frames, &decorations, &line_decorations, " ", prompt.as_deref());
    if app.shows_startup_screen() {
        grid = app.startup_screen.borrow_mut().paint(grid, app.started_at.elapsed(), app.theme);
    }
    prepare_realtime_view_updates(layout, app, &frames, &decorations, &line_decorations);
    prefetch_document_end(layout, app, &frames, &line_decorations);
    let mut grid = apply_editor_overlays(layout, app, grid, prompt.is_none());
    grid.resolve_theme(app.theme);
    grid
}

fn prepare_realtime_view_updates(
    layout: &mut ViewportLayout,
    app: &App,
    frames: &[(BufferId, wren_engine::EngineFrame)],
    decorations: &[(BufferId, SharedDecorations)],
    line_decorations: &[(BufferId, Vec<LineDecoration>)],
) {
    let window = app.views.active_window();
    let Some(frame) = frames.iter().find_map(|(buffer_id, frame)| (*buffer_id == window.buffer_id).then_some(frame)) else {
        return;
    };
    let spans = decorations.iter().find_map(|(buffer_id, spans)| (*buffer_id == window.buffer_id).then_some(spans.as_slice())).unwrap_or_default();
    let lines = line_decorations.iter().find_map(|(buffer_id, lines)| (*buffer_id == window.buffer_id).then_some(lines.as_slice())).unwrap_or_default();
    layout.prepare_workspace_realtime_updates(window.id, frame, spans, lines);
    app.prepare_realtime_decoration_updates();
}

fn prefetch_document_end(
    layout: &mut ViewportLayout,
    app: &App,
    frames: &[(BufferId, wren_engine::EngineFrame)],
    line_decorations: &[(BufferId, Vec<LineDecoration>)],
) {
    if app.active.editor.revision().get() != 0
        || app.active.editor.mode() != Mode::Normal
        || app.views.window_count() != 1
        || !app.active.class.policy().whole_document_syntax
        || language_bundle(app.active.document.presentation_path()).language_id.as_ref() == "markdown"
    {
        return;
    }
    let rows = app.viewport_rows.max(1);
    let text = app.active.editor.text();
    let last_line = text.line_of_byte(text.len_bytes());
    if last_line <= rows.saturating_mul(2) {
        return;
    }
    let margin = 3.min(rows.saturating_sub(1) / 2);
    let top_line = last_line.saturating_add(margin).saturating_add(1).saturating_sub(rows);
    let window = app.views.active_window();
    if window.top_line != 0 {
        return;
    }
    let Some(frame) = frames.iter().find_map(|(buffer_id, frame)| (*buffer_id == app.active.buffer_id).then_some(frame)) else {
        return;
    };
    let page = rows.saturating_sub(1).checked_div(2).unwrap_or(1).max(1);
    let previous_top = top_line.saturating_sub(page);
    let previous_cursor = text.byte_of_line(last_line.saturating_sub(page));
    for (target_top, cursor_byte) in [(previous_top, previous_cursor), (top_line, app.active.editor.document_end_byte())] {
        prefetch_editor_viewport(layout, app, window.id, frame, target_top, cursor_byte, line_decorations);
    }
}

fn prefetch_editor_viewport(
    layout: &mut ViewportLayout,
    app: &App,
    window_id: wren_types::WindowId,
    frame: &wren_engine::EngineFrame,
    top_line: usize,
    cursor_byte: usize,
    line_decorations: &[(BufferId, Vec<LineDecoration>)],
) {
    let text = app.active.editor.text();
    let rows = app.viewport_rows.max(1);
    let start = text.byte_of_line(top_line);
    let syntax_end = text.byte_of_line(top_line.saturating_add(rows).saturating_add(1)).max(start);
    let range = start..syntax_end;
    let mut decorations = DecorationSetBuilder::default();
    for state in [app.decorations.get(&app.active.buffer_id), app.semantic_decorations.get(&app.active.buffer_id)]
        .into_iter()
        .flatten()
        .filter(|state| state.revision == app.active.editor.revision())
    {
        decorations.push_shared(app.active.buffer_id, state.spans_in_shared(range.clone()));
    }
    if app.search_highlight {
        let search_end = text.byte_of_line(top_line.saturating_add(rows));
        decorations.push(app.active.buffer_id, search_decorations(app, &app.active, start..search_end));
    }
    if let Some(path) = app.active.document.presentation_path() {
        let mut spans = Vec::new();
        let mut ignored_lines = Vec::new();
        add_diagnostic_decorations(app, &app.active, path, &mut spans, &mut ignored_lines);
        spans.sort_by(decoration_order);
        spans.dedup();
        decorations.push(app.active.buffer_id, spans);
    }
    let decoration_layers = decorations.finish();
    let decorations = layout.compose_shared_decoration_layers(&decoration_layers);
    let spans = decorations.iter().find_map(|(buffer_id, spans)| (*buffer_id == app.active.buffer_id).then_some(spans.as_slice())).unwrap_or_default();
    let frame = wren_engine::EngineFrame::new(frame.text.clone(), cursor_byte);
    let lines = line_decorations.iter().find_map(|(buffer_id, lines)| (*buffer_id == app.active.buffer_id).then_some(lines.as_slice())).unwrap_or_default();
    layout.prefetch_workspace_viewport(window_id, &frame, top_line, spans, lines);
}

#[cfg(test)]
#[derive(Default)]
struct DesiredFrameTimings {
    stages: [Duration; 8],
    total: Duration,
}

#[cfg(test)]
impl std::fmt::Display for DesiredFrameTimings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [inputs, syntax, search, buffer_decorations, selection, decoration_merge, render, overlays] = self.stages;
        write!(
            formatter,
            "total={:?} inputs={:?} syntax={:?} search={:?} buffer_decorations={:?} selection={:?} decoration_merge={:?} render={:?} overlays={:?}",
            self.total, inputs, syntax, search, buffer_decorations, selection, decoration_merge, render, overlays,
        )
    }
}

#[cfg(test)]
fn desired_frame_profiled(layout: &mut ViewportLayout, app: &App) -> (DesiredGrid, DesiredFrameTimings) {
    let total_at = Instant::now();
    let mut stage_at = total_at;
    let mut timings = DesiredFrameTimings::default();
    let mut measure = |stage| {
        let now = Instant::now();
        timings.stages[stage] = now.duration_since(stage_at);
        stage_at = now;
    };
    layout.set_theme(app.theme);
    layout.set_terminal_sidebar_visible(app.agent_sidebar_visible && !app.input_focus.is_terminal());
    if app.input_focus.is_terminal() {
        let mut grid = app.desired_terminal_grid(layout);
        grid.resolve_theme(app.theme);
        return (grid, timings);
    }
    let frame = app.active.editor.frame();
    layout.ensure_cursor_visible(&frame, 1);
    let frames = buffer_frames(app, frame);
    let prompt = prompt_text(app);
    measure(0);
    let mut decorations = syntax_decorations(app);
    measure(1);
    add_search_decorations(app, &mut decorations);
    measure(2);
    let line_decorations = add_buffer_decorations(app, &mut decorations);
    measure(3);
    add_selection_decoration(app, &mut decorations);
    measure(4);
    let decorations = layout.compose_shared_decoration_layers(&decorations.finish());
    measure(5);
    let mut grid = layout.desired_workspace_grid_with_shared_decorations(&app.views, &frames, &decorations, &line_decorations, " ", prompt.as_deref());
    measure(6);
    if app.shows_startup_screen() {
        grid = app.startup_screen.borrow_mut().paint(grid, app.started_at.elapsed(), app.theme);
    }
    prepare_realtime_view_updates(layout, app, &frames, &decorations, &line_decorations);
    prefetch_document_end(layout, app, &frames, &line_decorations);
    let mut grid = apply_editor_overlays(layout, app, grid, prompt.is_none());
    grid.resolve_theme(app.theme);
    measure(7);
    timings.total = total_at.elapsed();
    (grid, timings)
}

fn buffer_frames(app: &App, active: wren_engine::EngineFrame) -> Vec<(BufferId, wren_engine::EngineFrame)> {
    std::iter::once((app.active.buffer_id, active)).chain(app.inactive.iter().map(|buffer| (buffer.buffer_id, buffer.editor.frame()))).collect()
}

fn prompt_text(app: &App) -> Option<String> {
    app.prompt.as_ref().filter(|prompt| !prompt.kind.is_picker()).map(|prompt| {
        let input = prompt.display();
        if app.message.is_empty() { input } else { format!("{input}  │  {}", app.message) }
    })
}

fn decoration_bucket<T>(decorations: &mut Vec<(BufferId, Vec<T>)>, buffer_id: BufferId) -> &mut Vec<T> {
    let index = decorations.iter().position(|(candidate, _)| *candidate == buffer_id).unwrap_or_else(|| {
        decorations.push((buffer_id, Vec::new()));
        decorations.len() - 1
    });
    &mut decorations[index].1
}

#[derive(Default)]
struct DecorationSetBuilder {
    buffers: Vec<(BufferId, Vec<SharedDecorations>)>,
}

impl DecorationSetBuilder {
    fn push(&mut self, buffer_id: BufferId, spans: Vec<DecorationSpan>) {
        if spans.is_empty() {
            return;
        }
        let layers = decoration_bucket(&mut self.buffers, buffer_id);
        layers.push(Arc::new(spans));
    }

    fn push_shared(&mut self, buffer_id: BufferId, spans: SharedDecorations) {
        if spans.is_empty() {
            return;
        }
        decoration_bucket(&mut self.buffers, buffer_id).push(spans);
    }

    fn finish(self) -> Vec<(BufferId, Vec<SharedDecorations>)> {
        self.buffers
    }
}

fn syntax_decorations(app: &App) -> DecorationSetBuilder {
    let mut decorations = DecorationSetBuilder::default();
    for (buffer_id, state) in app.decorations.iter().chain(&app.semantic_decorations) {
        if let Some(spans) = app.buffer(*buffer_id).and_then(|buffer| {
            (buffer.editor.revision() == state.revision).then(|| visible_byte_range(app, *buffer_id)).flatten().map(|range| state.spans_in_shared(range))
        }) {
            decorations.push_shared(*buffer_id, spans);
        }
    }
    decorations
}

fn visible_byte_range(app: &App, buffer_id: BufferId) -> Option<Range<usize>> {
    let buffer = app.buffer(buffer_id)?;
    let viewport_rows = app.viewport_rows.max(1);
    let mut visible: Option<Range<usize>> = None;
    app.views.visit_windows(|window| {
        if window.buffer_id == buffer_id {
            let start = buffer.editor.text().byte_of_line(window.top_line);
            let end = buffer.editor.text().byte_of_line(window.top_line.saturating_add(viewport_rows).saturating_add(1)).max(start);
            visible = Some(visible.take().map_or(start..end, |previous| previous.start.min(start)..previous.end.max(end)));
        }
    });
    visible
}

fn add_search_decorations(app: &App, decorations: &mut DecorationSetBuilder) {
    if !app.search_highlight {
        return;
    }
    let previewing = app.prompt.as_ref().is_some_and(|prompt| prompt.kind.is_search());
    app.views.visit_windows(|window| {
        if previewing && window.buffer_id != app.active.buffer_id {
            return;
        }
        let Some(buffer) = app.buffer(window.buffer_id) else {
            return;
        };
        let visible_start = buffer.editor.text().byte_of_line(window.top_line);
        let visible_end = buffer.editor.text().byte_of_line(window.top_line.saturating_add(app.viewport_rows.max(1)));
        let spans = search_decorations(app, buffer, visible_start..visible_end);
        decorations.push(window.buffer_id, spans);
    });
}

fn search_decorations(app: &App, buffer: &BufferState, range: Range<usize>) -> Vec<DecorationSpan> {
    buffer
        .editor
        .search_match_ranges(range, 4_096)
        .into_iter()
        .map(|range| {
            let current = buffer.buffer_id == app.active.buffer_id && range.start == app.active.editor.primary_cursor();
            DecorationSpan::new(
                range,
                CellStyle::themed(CatppuccinColor::Crust, if current { CatppuccinColor::Peach } else { CatppuccinColor::Yellow }),
                3_000_000,
            )
        })
        .collect()
}

fn add_git_decorations(buffer: &BufferState, lines: &mut Vec<LineDecoration>) {
    lines.extend(buffer.git_hunks.iter().map(|hunk| {
        let line = if hunk.after.start == hunk.after.end { hunk.after.start.saturating_sub(1) } else { hunk.after.start } as usize;
        let color = match (hunk.before.start == hunk.before.end, hunk.after.start == hunk.after.end) {
            (true, _) => CatppuccinColor::Green,
            (false, true) => CatppuccinColor::Red,
            (false, false) => CatppuccinColor::Yellow,
        };
        let sign = if hunk.after.start == hunk.after.end { '_' } else { '│' };
        LineDecoration { line, sign, style: CellStyle::default().with_foreground(CellColor::Theme(color)).with_bold() }
    }));
}

fn add_diagnostic_decorations(app: &App, buffer: &BufferState, path: &Path, spans: &mut Vec<DecorationSpan>, lines: &mut Vec<LineDecoration>) {
    for diagnostic in app.diagnostics.iter().filter(|entry| same_path(&entry.path, path)) {
        let start = buffer.editor.text().byte_of_line(diagnostic.line.saturating_sub(1));
        let end = buffer.editor.text().byte_of_line(diagnostic.line).saturating_sub(1).max(start.saturating_add(1)).min(buffer.editor.text().len_bytes());
        let color = match diagnostic.severity {
            Severity::Error => CatppuccinColor::Red,
            Severity::Warning => CatppuccinColor::Yellow,
            Severity::Info => CatppuccinColor::Blue,
            Severity::Hint | Severity::None => CatppuccinColor::Teal,
        };
        spans.push(DecorationSpan::new(start..end, CellStyle::default().with_foreground(CellColor::Theme(color)).with_underline(), 2_000_000));
        let sign = match diagnostic.severity {
            Severity::Error => 'E',
            Severity::Warning => 'W',
            Severity::Info => 'I',
            Severity::Hint | Severity::None => 'H',
        };
        lines.push(LineDecoration {
            line: diagnostic.line.saturating_sub(1),
            sign,
            style: CellStyle::default().with_foreground(CellColor::Theme(color)).with_bold(),
        });
    }
}

fn add_breakpoint_decorations(app: &App, path: &Path, lines: &mut Vec<LineDecoration>) {
    let Some(breakpoints) = app.breakpoints.get(path) else {
        return;
    };
    lines.extend(breakpoints.keys().map(|line| LineDecoration {
        line: line.saturating_sub(1),
        sign: '●',
        style: CellStyle::themed(CatppuccinColor::Red, CatppuccinColor::Surface0).with_bold(),
    }));
}

fn add_buffer_decorations(app: &App, decorations: &mut DecorationSetBuilder) -> Vec<(BufferId, Vec<LineDecoration>)> {
    let mut line_decorations = Vec::new();
    for buffer in std::iter::once(&app.active).chain(app.inactive.iter()) {
        let Some(path) = buffer.document.presentation_path() else {
            continue;
        };
        let lines = decoration_bucket(&mut line_decorations, buffer.buffer_id);
        let mut added = Vec::new();
        add_git_decorations(buffer, lines);
        add_diagnostic_decorations(app, buffer, path, &mut added, lines);
        add_breakpoint_decorations(app, path, lines);
        added.sort_by(decoration_order);
        added.dedup();
        decorations.push(buffer.buffer_id, added);
    }
    line_decorations
}

fn add_selection_decoration(app: &App, decorations: &mut DecorationSetBuilder) {
    if !matches!(app.active.editor.mode(), Mode::Visual | Mode::VisualLine) {
        return;
    }
    let selection = app.active.editor.selection_byte_range();
    if selection.is_empty() {
        return;
    }
    decorations.push(
        app.active.buffer_id,
        vec![DecorationSpan::new(
            selection,
            CellStyle::default().without_foreground().with_background(CellColor::Theme(CatppuccinColor::Surface2)),
            // Selection is appended after syntax and semantic decorations, so
            // tying their maximum priority lets it own the background while
            // retaining the highlighter's foreground and text attributes.
            u32::MAX,
        )],
    );
}

fn apply_editor_overlays(layout: &mut ViewportLayout, app: &App, grid: DesiredGrid, show_status: bool) -> DesiredGrid {
    let grid = if show_status { layout.apply_status_overlay(grid, &app.status_overlay()) } else { grid };
    let grid =
        if let Some(overlay) = app.ace_jump_overlay() { layout.apply_ace_jump_overlay(grid, &app.views, &app.active.editor.frame(), &overlay) } else { grid };
    let grid = if app.debug_ui_visible { layout.apply_debug_overlay(grid, &app.debug_overlay()) } else { grid };
    let grid = app.apply_agent_sidebar(layout, grid);
    if let Some(picker) = app.picker_overlay() {
        layout.apply_picker_overlay(grid, &picker)
    } else if let Some(completion) = app.completion_overlay() {
        layout.apply_completion_overlay(grid, &completion)
    } else if let Some(popup) = &app.popup {
        layout.apply_text_popup(grid, popup)
    } else {
        grid
    }
}

#[derive(Debug, Default)]
struct Cli {
    path: Option<PathBuf>,
    line: Option<usize>,
    help: bool,
    version: bool,
}

impl Cli {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self> {
        let mut cli = Self::default();
        for argument in arguments {
            match argument.as_str() {
                "-h" | "--help" => cli.help = true,
                "-V" | "--version" => cli.version = true,
                "--" => {}
                value if value.starts_with('+') && value.len() > 1 => {
                    cli.line = Some(value[1..].parse::<usize>().with_context(|| format!("invalid line argument {value}"))?);
                }
                value if value.starts_with('-') => bail!("unknown option {value}"),
                value if cli.path.is_none() => cli.path = Some(PathBuf::from(value)),
                value => bail!("unexpected extra path {value}"),
            }
        }
        Ok(cli)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    Command,
    Search(SearchDirection),
    Expression,
    Picker(PickerSource),
    Rename,
    ConditionalBreakpoint,
}

impl PromptKind {
    const fn is_picker(self) -> bool {
        matches!(self, Self::Picker(_))
    }

    const fn picker_source(self) -> Option<PickerSource> {
        match self {
            Self::Picker(source) => Some(source),
            _ => None,
        }
    }

    const fn is_search(self) -> bool {
        matches!(self, Self::Search(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Prompt {
    kind: PromptKind,
    buffer: String,
    history_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchPromptOrigin {
    cursor: usize,
    previous_search: Option<(Box<str>, SearchDirection)>,
    previous_highlight: bool,
}

impl Prompt {
    fn new(kind: PromptKind) -> Self {
        Self { kind, buffer: String::new(), history_index: None }
    }

    fn display(&self) -> String {
        format!("{}{}", self.prefix(), self.buffer)
    }

    const fn prefix(&self) -> &'static str {
        match self.kind {
            PromptKind::Command => ":",
            PromptKind::Search(SearchDirection::Forward) => "/",
            PromptKind::Search(SearchDirection::Backward) => "?",
            PromptKind::Expression => "=",
            PromptKind::Picker(PickerSource::Browser) => "browse> ",
            PromptKind::Picker(PickerSource::Grep) => "grep> ",
            PromptKind::Picker(PickerSource::Jumps | PickerSource::Diagnostics) => "jump> ",
            PromptKind::Picker(_) => "find> ",
            PromptKind::Rename => "rename> ",
            PromptKind::ConditionalBreakpoint => "break if> ",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerSource {
    Files,
    Browser,
    Grep,
    Buffers,
    Recent,
    Jumps,
    Diagnostics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PickerItem {
    Path(PathBuf),
    Location(QuickfixEntry),
}

impl PickerItem {
    fn path(&self) -> &Path {
        match self {
            Self::Path(path) => path,
            Self::Location(entry) => &entry.path,
        }
    }

    fn location(&self) -> Option<&QuickfixEntry> {
        match self {
            Self::Location(entry) => Some(entry),
            Self::Path(_) => None,
        }
    }

    fn search_text(&self) -> String {
        match self {
            Self::Path(path) => path.to_string_lossy().into_owned(),
            Self::Location(entry) => entry.display(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewPosition {
    Top,
    Middle,
    Bottom,
}

struct PrefixBinding {
    prefix: char,
    key: char,
    description: &'static str,
    execute: fn(&mut App, char, char) -> Result<()>,
    enabled: Option<fn(LspNavigationCapabilities) -> bool>,
}

impl PrefixBinding {
    const fn new(prefix: char, key: char, description: &'static str, execute: fn(&mut App, char, char) -> Result<()>) -> Self {
        Self { prefix, key, description, execute, enabled: None }
    }

    const fn when(mut self, enabled: fn(LspNavigationCapabilities) -> bool) -> Self {
        self.enabled = Some(enabled);
        self
    }
}

const PREFIX_TITLES: &[(char, &str)] = &[('g', " goto "), ('[', " previous "), (']', " next "), ('z', " viewport "), ('Z', " quit "), ('\u{17}', " window ")];

const PREFIX_BINDINGS: &[PrefixBinding] = &[
    PrefixBinding::new('g', 'g', "first line / [count] line", App::execute_grammar_prefix),
    PrefixBinding::new('g', 'e', "previous word end", App::execute_grammar_prefix),
    PrefixBinding::new('g', 'E', "previous WORD end", App::execute_grammar_prefix),
    PrefixBinding::new('g', '0', "line start", App::execute_grammar_prefix),
    PrefixBinding::new('g', '$', "line end", App::execute_grammar_prefix),
    PrefixBinding::new('g', '_', "last nonblank", App::execute_grammar_prefix),
    PrefixBinding::new('g', 'q', "format to text width", |app, _, _| app.format_text_width()),
    PrefixBinding::new('g', ';', "older change", |app, _, _| app.execute_change_prefix(true)),
    PrefixBinding::new('g', ',', "newer change", |app, _, _| app.execute_change_prefix(false)),
    PrefixBinding::new('g', 'D', "declaration", |app, _, _| app.dispatch_lsp_cursor_request(PendingLspRequest::DECLARATION))
        .when(|navigation| navigation.declaration),
    PrefixBinding::new('g', 'd', "definition", |app, _, _| app.dispatch_lsp_cursor_request(PendingLspRequest::DEFINITION))
        .when(|navigation| navigation.definition),
    PrefixBinding::new('g', 'i', "implementation", |app, _, _| app.dispatch_lsp_cursor_request(PendingLspRequest::IMPLEMENTATION))
        .when(|navigation| navigation.implementation),
    PrefixBinding::new('g', 'r', "references", |app, _, _| app.lsp_references()).when(|navigation| navigation.references),
    PrefixBinding::new('[', 'c', "previous Git hunk", |app, _, _| app.move_git_hunk(-1)),
    PrefixBinding::new('[', 'd', "previous diagnostic", |app, _, _| app.move_diagnostic(-1)),
    PrefixBinding::new(']', 'c', "next Git hunk", |app, _, _| app.move_git_hunk(1)),
    PrefixBinding::new(']', 'd', "next diagnostic", |app, _, _| app.move_diagnostic(1)),
    PrefixBinding::new('z', 'b', "cursor line at bottom", |app, _, _| app.execute_center_prefix(ViewPosition::Bottom)),
    PrefixBinding::new('z', 't', "cursor line at top", |app, _, _| app.execute_center_prefix(ViewPosition::Top)),
    PrefixBinding::new('z', 'z', "cursor line centered", |app, _, _| app.execute_center_prefix(ViewPosition::Middle)),
    PrefixBinding::new('Z', 'Q', "quit without saving", |app, _, _| app.execute_ex("q!")),
    PrefixBinding::new('Z', 'Z', "write and quit", |app, _, _| app.execute_ex("wq")),
    PrefixBinding::new('\u{17}', 'h', "focus left", |app, _, _| app.focus_prefixed_window(WindowDirection::Left)),
    PrefixBinding::new('\u{17}', 'j', "focus down", |app, _, _| app.focus_prefixed_window(WindowDirection::Down)),
    PrefixBinding::new('\u{17}', 'k', "focus up / signature help", |app, _, _| app.focus_prefixed_window(WindowDirection::Up)),
    PrefixBinding::new('\u{17}', 'l', "focus right", |app, _, _| app.focus_prefixed_window(WindowDirection::Right)),
    PrefixBinding::new('\u{17}', 's', "horizontal split", |app, _, _| app.execute_split_prefix(SplitAxis::Horizontal)),
    PrefixBinding::new('\u{17}', 'v', "vertical split", |app, _, _| app.execute_split_prefix(SplitAxis::Vertical)),
    PrefixBinding::new('\u{17}', 'c', "close window", |app, _, _| app.execute_close_prefix()),
    PrefixBinding::new('\u{17}', 'q', "close window", |app, _, _| app.execute_close_prefix()),
    PrefixBinding::new('\u{17}', 'o', "close other windows", |app, _, _| app.execute_only_prefix()),
    PrefixBinding::new('\u{17}', 'w', "next window", |app, _, _| app.execute_cycle_prefix(1)),
    PrefixBinding::new('\u{17}', 'W', "previous window", |app, _, _| app.execute_cycle_prefix(-1)),
    PrefixBinding::new('\u{17}', '=', "equalize windows", |app, _, _| app.execute_equalize_prefix()),
];

fn prefix_binding(prefix: char, key: char) -> Option<&'static PrefixBinding> {
    PREFIX_BINDINGS.iter().find(|binding| binding.prefix == prefix && binding.key == key)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AceJumpState {
    AwaitTarget,
    AwaitLabel { target: char, prefix: String, targets: Vec<AceJumpTarget> },
}

#[cfg(not(test))]
#[derive(Debug, Default, Deserialize)]
struct ThemeConfig {
    flavor: Option<String>,
    #[serde(default)]
    colors: BTreeMap<String, String>,
}

#[cfg(not(test))]
fn config_file(name: &str) -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|directory| directory.join("wren").join(name))
}

#[cfg(not(test))]
fn load_user_config<T>(path: Option<PathBuf>, kind: &str, fallback: impl FnOnce() -> T, parse: impl FnOnce(&str) -> Result<T>) -> (T, String) {
    let Some(path) = path.filter(|path| path.exists()) else { return (fallback(), String::new()) };
    match std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display())).and_then(|source| parse(&source)) {
        Ok(config) => (config, String::new()),
        Err(error) => (fallback(), format!("{kind} config {}: {error:#}", path.display())),
    }
}

#[cfg(test)]
fn load_theme() -> (EditorTheme, String) {
    (EditorTheme::for_flavor(CatppuccinFlavor::Mocha), String::new())
}

#[cfg(not(test))]
fn load_theme() -> (EditorTheme, String) {
    let (config, mut message) = load_user_config(config_file("theme.toml"), "theme", ThemeConfig::default, |source| Ok(toml::from_str(source)?));
    let requested = env::var("WREN_CATPPUCCIN_FLAVOR").ok().or(config.flavor).unwrap_or_else(|| "mocha".to_owned());
    let flavor = parse_catppuccin_flavor(&requested).unwrap_or_else(|| {
        message = format!("unknown Catppuccin flavor {requested:?}; using mocha");
        CatppuccinFlavor::Mocha
    });
    let mut palette = EditorTheme::for_flavor(flavor);
    for (name, value) in config.colors {
        let Some(color) = RgbColor::from_hex(&value) else {
            message = format!("invalid theme color {name}={value:?}");
            continue;
        };
        if !palette.set(&name, color) {
            message = format!("unknown theme color slot {name:?}");
        }
    }
    (palette, message)
}

fn parse_catppuccin_flavor(value: &str) -> Option<CatppuccinFlavor> {
    let value = value.to_ascii_lowercase();
    let value = value.strip_prefix("catppuccin-").unwrap_or(&value);
    if value == "catppuccin" { Some(CatppuccinFlavor::Mocha) } else { value.parse().ok() }
}

#[derive(Debug, Clone)]
struct RuntimeBinding {
    execute: fn(&mut App) -> Result<()>,
    when: Option<Box<str>>,
    description: Box<str>,
}

struct NativeCommand {
    sequence: &'static str,
    name: &'static str,
    description: &'static str,
    class: CommandClass,
    execute: fn(&mut App) -> Result<()>,
}

impl NativeCommand {
    const fn new(sequence: &'static str, name: &'static str, description: &'static str, class: CommandClass, execute: fn(&mut App) -> Result<()>) -> Self {
        Self { sequence, name, description, class, execute }
    }
}

#[derive(Debug, Clone)]
struct RuntimeKeymap {
    leader: BTreeMap<Box<str>, RuntimeBinding>,
    groups: BTreeMap<Box<str>, Box<str>>,
}

impl RuntimeKeymap {
    fn defaults() -> Self {
        let mut leader = BTreeMap::new();
        for command in NATIVE_COMMANDS {
            leader.insert(
                Box::<str>::from(command.sequence),
                RuntimeBinding { execute: command.execute, when: None, description: Box::<str>::from(command.description) },
            );
        }
        Self {
            leader,
            groups: [
                ("f", "find"),
                ("d", "debug"),
                ("g", "git"),
                ("h", "haskell"),
                ("r", "repl"),
                ("c", "code"),
                ("e", "evaluate/diagnostic"),
                ("w", "write/workspace"),
                ("j", "jump"),
            ]
            .into_iter()
            .map(|(prefix, description)| (prefix.into(), description.into()))
            .collect(),
        }
    }

    fn overlay_user_config(&mut self, source: &str) -> Result<()> {
        let schemas = native_command_schemas();
        let registry = CommandRegistry::new(schemas.clone());
        let parsed: wren_config::Config = toml::from_str(source).context("parse user configuration")?;
        let config =
            parse_and_validate(source, &registry, WorkspaceTrust::Trusted { executable_hash: executable_hash(&parsed) }).map_err(|error| anyhow!(error))?;
        let descriptions: BTreeMap<&str, &str> = schemas.iter().map(|schema| (schema.name.as_ref(), schema.description.as_ref())).collect();
        if let Some(bindings) = config.keys.get("normal") {
            for (keys, binding) in bindings {
                let Some(sequence) = normalize_leader_sequence(keys) else {
                    continue;
                };
                let invocation = registry.validate(&binding.command, &binding.args).map_err(|error| anyhow!(error))?;
                let command = NATIVE_COMMANDS
                    .iter()
                    .find(|command| command.name == invocation.command.as_ref())
                    .ok_or_else(|| anyhow!("validated command {} has no runtime implementation", invocation.command))?;
                let description = descriptions.get(binding.command.as_str()).copied().unwrap_or(binding.command.as_str());
                self.leader.insert(
                    sequence.into_boxed_str(),
                    RuntimeBinding { execute: command.execute, when: binding.when.as_deref().map(Box::<str>::from), description: description.into() },
                );
            }
        }
        Ok(())
    }
}

const NATIVE_COMMANDS: &[NativeCommand] = &[
    NativeCommand::new(" ", "selection.line", "select line", CommandClass::Realtime, |app| {
        app.dispatch_key(KeyEvent::character('V'));
        Ok(())
    }),
    NativeCommand::new("q", "editor.quit", "quit", CommandClass::Realtime, |app| app.execute_ex("q")),
    NativeCommand::new("w", "file.write", "write", CommandClass::Task, |app| app.save(None)),
    NativeCommand::new("x", "search.clear", "clear search", CommandClass::Realtime, |app| {
        app.search_highlight = false;
        app.message.clear();
        Ok(())
    }),
    NativeCommand::new("b", "picker.buffers", "buffers", CommandClass::Task, App::start_buffer_picker),
    NativeCommand::new("a", "ai.chat", "Oh My Pi pane", CommandClass::Realtime, App::toggle_agent_sidebar),
    NativeCommand::new("jj", "jump.ace", "visible character", CommandClass::Realtime, App::start_ace_jump),
    NativeCommand::new("f", "format.document", "format", CommandClass::Task, App::format_active_language),
    NativeCommand::new("ff", "picker.files", "files", CommandClass::Task, |app| app.start_file_picker("")),
    NativeCommand::new("fb", "picker.browser", "file browser", CommandClass::Task, App::start_file_browser),
    NativeCommand::new("f.", "picker.resume", "resume picker", CommandClass::Task, App::resume_picker),
    NativeCommand::new("fo", "picker.recent", "recent files", CommandClass::Task, App::start_recent_picker),
    NativeCommand::new("fr", "picker.grep", "grep Git root", CommandClass::Task, |app| app.start_grep_picker("")),
    NativeCommand::new("fw", "picker.grep_word", "grep word", CommandClass::Task, App::start_grep_word_picker),
    NativeCommand::new("fj", "picker.jumplist", "jumplist", CommandClass::Task, App::start_jumplist_picker),
    NativeCommand::new("fd", "picker.diagnostics", "diagnostics", CommandClass::Task, App::start_diagnostic_picker),
    NativeCommand::new("e", "diagnostic.show", "diagnostic float", CommandClass::Realtime, App::show_cursor_diagnostic),
    NativeCommand::new("ea", "repl.evaluate", "evaluate selection", CommandClass::Task, App::evaluate_in_repl),
    NativeCommand::new("dt", "debug.toggle", "toggle UI", CommandClass::Task, App::toggle_debug_ui),
    NativeCommand::new("db", "debug.breakpoint", "breakpoint", CommandClass::Task, |app| app.toggle_breakpoint(None)),
    NativeCommand::new("dB", "debug.conditional_breakpoint", "conditional breakpoint", CommandClass::Task, App::open_conditional_breakpoint_prompt),
    NativeCommand::new("dl", "debug.repl", "REPL", CommandClass::Task, App::open_debug_repl),
    NativeCommand::new("dc", "debug.continue", "continue", CommandClass::Task, |app| app.run_debug_action("dc")),
    NativeCommand::new("ds", "debug.step_into", "step into", CommandClass::Task, |app| app.run_debug_action("ds")),
    NativeCommand::new("dn", "debug.step_over", "step over", CommandClass::Task, |app| app.run_debug_action("dn")),
    NativeCommand::new("do", "debug.step_out", "step out", CommandClass::Task, |app| app.run_debug_action("do")),
    NativeCommand::new("dr", "debug.restart", "restart", CommandClass::Task, |app| app.run_debug_action("dr")),
    NativeCommand::new("gs", "git.stage_hunk", "stage hunk", CommandClass::Task, App::git_stage_hunk),
    NativeCommand::new("gr", "git.reset_hunk", "reset hunk", CommandClass::Task, App::git_reset_hunk),
    NativeCommand::new("gS", "git.stage_buffer", "stage buffer", CommandClass::Task, App::git_stage_buffer),
    NativeCommand::new("gu", "git.undo_stage", "undo stage", CommandClass::Task, App::git_undo_stage_hunk),
    NativeCommand::new("gp", "git.preview_hunk", "preview hunk", CommandClass::Task, App::git_preview_hunk),
    NativeCommand::new("gb", "git.blame_line", "blame line", CommandClass::Task, App::git_blame_line),
    NativeCommand::new("gd", "git.diff_index", "diff index", CommandClass::Task, App::git_diff_index),
    NativeCommand::new("rn", "lsp.rename", "rename", CommandClass::Task, App::open_rename_prompt),
    NativeCommand::new("ca", "lsp.code_action", "code action", CommandClass::Task, App::lsp_code_action),
    NativeCommand::new("D", "lsp.type_definition", "type definition", CommandClass::Task, |app| {
        app.dispatch_lsp_cursor_request(PendingLspRequest::TYPE_DEFINITION)
    }),
    NativeCommand::new("Wa", "workspace.add_folder", "add workspace folder", CommandClass::Realtime, |app| {
        app.lsp_workspace_folder("workspace/didChangeWorkspaceFolders", true)
    }),
    NativeCommand::new("Wr", "workspace.remove_folder", "remove workspace folder", CommandClass::Realtime, |app| {
        app.lsp_workspace_folder("workspace/didChangeWorkspaceFolders", false)
    }),
    NativeCommand::new("Wl", "workspace.list_folders", "list workspace folders", CommandClass::Realtime, App::list_workspace_folders),
    NativeCommand::new("hw", "haskell.hoogle", "Hoogle", CommandClass::Task, App::open_hoogle),
    NativeCommand::new("hs", "haskell.signature", "signature", CommandClass::Task, App::hoogle_signature),
    NativeCommand::new("hl", "haskell.code_lens", "code lens", CommandClass::Task, App::lsp_code_lens),
    NativeCommand::new("rr", "haskell.repl_package", "package GHCi", CommandClass::Task, |app| app.open_haskell_repl(true)),
    NativeCommand::new("rf", "haskell.repl_file", "file GHCi", CommandClass::Task, |app| app.open_haskell_repl(false)),
    NativeCommand::new("rq", "haskell.repl_quit", "quit GHCi", CommandClass::Task, App::quit_repl),
];

fn native_command_schemas() -> Vec<CommandSchema> {
    NATIVE_COMMANDS
        .iter()
        .map(|command| CommandSchema { name: command.name.into(), description: command.description.into(), class: command.class, arguments: Vec::new() })
        .collect()
}

fn normalize_leader_sequence(keys: &str) -> Option<String> {
    let keys = keys.trim();
    let tail = keys.strip_prefix("space ").or_else(|| keys.strip_prefix("<space>"))?;
    let sequence: String = tail.split_whitespace().map(|token| if token == "space" { " " } else { token }).collect();
    (!sequence.is_empty()).then_some(sequence)
}

#[cfg(test)]
fn load_keymap() -> (RuntimeKeymap, String) {
    (RuntimeKeymap::defaults(), String::new())
}

#[cfg(not(test))]
fn load_keymap() -> (RuntimeKeymap, String) {
    load_user_config(env::var_os("WREN_CONFIG").map(PathBuf::from).or_else(|| config_file("config.toml")), "keymap", RuntimeKeymap::defaults, |source| {
        let mut keymap = RuntimeKeymap::defaults();
        keymap.overlay_user_config(source)?;
        Ok(keymap)
    })
}

struct BufferState {
    buffer_id: BufferId,
    document_id: DocumentId,
    editor: Editor,
    document: LocalDocument,
    class: DocumentClass,
    mixed_line_endings: bool,
    wal: Option<WalWorker>,
    base_hash: [u8; 32],
    /// The normalized content that established `base_hash`. It is retained so
    /// an external-save conflict can be resolved as a real three-way merge,
    /// rather than guessing from the current editor buffer.
    base_text: Arc<str>,
    git_index_text: Option<Arc<str>>,
    git_hunks: Vec<GitHunk>,
    git_branch: Option<Box<str>>,
    display_name: Option<Box<str>>,
}

#[derive(Debug, Clone, Copy)]
struct MouseSelection {
    buffer_id: BufferId,
    anchor: usize,
    dragged: bool,
}

impl BufferState {
    fn open(buffer_id: BufferId, document_id: DocumentId, path: Option<&Path>, line: Option<usize>) -> Result<(Self, String)> {
        let (document, opened) = app_lifecycle::open_document(path)?;
        #[cfg(not(test))]
        let wal = document.presentation_path().map(LocalWal::for_document).transpose().context("locate recovery WAL")?;
        #[cfg(test)]
        let wal = None;
        Self::from_opened(buffer_id, document_id, document, opened, line, wal)
    }

    fn from_opened(
        buffer_id: BufferId,
        document_id: DocumentId,
        document: LocalDocument,
        opened: OpenedDocument,
        line: Option<usize>,
        wal: Option<LocalWal>,
    ) -> Result<(Self, String)> {
        let base_hash = document.base_hash();
        let base_text: Arc<str> = Arc::from(opened.text.as_str());
        let git_index_text = document.presentation_path().and_then(|path| {
            let root = git_root_for(path).ok()?;
            let relative = path.strip_prefix(&root).ok()?;
            git_index_contents(&root, relative).ok().map(Arc::from)
        });
        let git_branch = document.presentation_path().and_then(git_branch_for).map(String::into_boxed_str);
        let recovered = wal
            .as_ref()
            .map(LocalWal::recover_latest)
            .transpose()
            .context("read recovery WAL")?
            .flatten()
            .filter(|state| state.base_hash == base_hash && !opened.read_only);
        let (text, recovered_cursor, recovered_revision) =
            recovered.map(|state| (state.text, Some(state.cursor), Some(state.revision))).unwrap_or((opened.text, None, None));
        let indent = detect_indent_style(&text);
        let initial_git_hunks = git_index_text.as_ref().map_or_else(Vec::new, |index_text| git_hunks(index_text, &text));
        let store = if recovered_revision.is_none() {
            document
                .mapped_text_store()
                .context("open eligible mapped text store")?
                .unwrap_or(DefaultText::from_reader(Cursor::new(text.as_bytes())).context("create text store")?)
        } else {
            DefaultText::from_reader(Cursor::new(text.as_bytes())).context("create recovery text store")?
        };
        let mut editor = Editor::new(store);
        editor.set_search_options(true, true);
        editor.set_clipboard_unnamed(true);
        editor.set_indent_options(indent.expand_tabs, indent.width, indent.width, true);
        editor.set_expand_region_keys(true);
        editor.set_read_only(opened.read_only);
        match (recovered_cursor, line) {
            (Some(cursor), _) => {
                editor.set_cursor(cursor);
                editor.mark_dirty();
            }
            (None, Some(line)) => editor.set_cursor(editor.text().byte_of_line(line.saturating_sub(1))),
            (None, None) => {}
        }
        if recovered_revision.is_none()
            && let Some(path) = document.presentation_path()
            && let Some(undo) = load_undo_state(path, base_hash)?
        {
            let mut state = EditorState::default();
            state.set_undo(undo);
            editor.restore(state)?;
        }
        let message = match (recovered_revision, opened.read_only) {
            (Some(revision), _) => format!("recovered unsaved revision {revision}"),
            (None, true) => format!("read-only {:?} byte view", opened.encoding),
            (None, false) => String::new(),
        };
        Ok((
            Self {
                buffer_id,
                document_id,
                editor,
                document,
                class: opened.class,
                mixed_line_endings: opened.mixed_line_endings,
                wal: wal.map(WalWorker::start).transpose()?,
                base_hash,
                base_text,
                git_index_text,
                git_hunks: initial_git_hunks,
                git_branch,
                display_name: None,
            },
            message,
        ))
    }

    fn virtual_buffer(buffer_id: BufferId, document_id: DocumentId, name: &str, text: String) -> Result<Self> {
        let (document, mut opened) = LocalDocument::unnamed();
        opened.text = text;
        opened.read_only = true;
        let (mut buffer, _) = Self::from_opened(buffer_id, document_id, document, opened, None, None)?;
        buffer.display_name = Some(name.into());
        Ok(buffer)
    }

    fn merge_buffer(buffer_id: BufferId, document_id: DocumentId, path: &Path) -> Result<Self> {
        let (document, opened) = LocalDocument::open(path)?;
        let (mut buffer, _) = Self::from_opened(buffer_id, document_id, document, opened, None, None)?;
        let name = format!("Merge: {}", path.display());
        buffer.display_name = Some(name.into_boxed_str());
        Ok(buffer)
    }

    fn configured_editor(text: &str, read_only: bool) -> Result<Editor> {
        let indent = detect_indent_style(text);
        let store = DefaultText::from_reader(Cursor::new(text.as_bytes())).context("create text store")?;
        let mut editor = Editor::new(store);
        editor.set_search_options(true, true);
        editor.set_clipboard_unnamed(true);
        editor.set_indent_options(indent.expand_tabs, indent.width, indent.width, true);
        editor.set_expand_region_keys(true);
        editor.set_read_only(read_only);
        Ok(editor)
    }

    /// Discards the local replica in favour of a newly opened disk snapshot.
    /// The caller registers the returned text with the mutation worker before
    /// accepting further local edits.
    fn reload_from_disk(&mut self) -> Result<String> {
        let opened = self.document.reload()?;
        let text = opened.text;
        self.editor = Self::configured_editor(&text, opened.read_only)?;
        self.class = opened.class;
        self.mixed_line_endings = opened.mixed_line_endings;
        self.base_hash = self.document.base_hash();
        self.base_text = Arc::from(text.as_str());
        self.git_index_text = self.document.presentation_path().and_then(|path| {
            let root = git_root_for(path).ok()?;
            let relative = path.strip_prefix(&root).ok()?;
            git_index_contents(&root, relative).ok().map(Arc::from)
        });
        self.git_hunks = self.git_index_text.as_ref().map_or_else(Vec::new, |index| git_hunks(index, &text));
        self.git_branch = self.document.presentation_path().and_then(git_branch_for).map(String::into_boxed_str);
        Ok(text)
    }

    fn name(&self) -> String {
        if let Some(name) = &self.display_name {
            return name.to_string();
        }
        self.document.presentation_path().map_or_else(|| "[No Name]".to_owned(), |path| path.display().to_string())
    }

    fn has_display_name(&self, name: &str) -> bool {
        self.display_name.as_deref() == Some(name)
    }

    fn refresh_git_hunks(&mut self) {
        self.git_hunks = self.git_index_text.as_ref().map_or_else(Vec::new, |index| {
            let text = self.editor.frame().text.materialize_for_task();
            git_hunks(index, &text)
        });
    }
}

struct App {
    active: BufferState,
    inactive: Vec<BufferState>,
    views: ClientViewModel,
    quickfix: Vec<QuickfixEntry>,
    jump_history: Vec<DurableJumpEntry>,
    jump_index: Option<usize>,
    mutations: MutationWorker,
    client_state: DurableClientState,
    client_state_worker: ClientStateWorker,
    provider: ProviderWorker,
    git_worker: GitHunkWorker,
    pending_git_refreshes: Vec<PendingGitHunkRefresh>,
    provider_submitted: BTreeMap<DocumentId, ProviderDemandKey>,
    provider_refresh_due: BTreeMap<DocumentId, Instant>,
    provider_refresh_ranges: BTreeMap<DocumentId, Vec<Range<usize>>>,
    provider_pending_transactions: BTreeMap<DocumentId, Vec<Transaction>>,
    provider_resync_required: BTreeSet<DocumentId>,
    decorations: BTreeMap<BufferId, BufferDecorations>,
    semantic_decorations: BTreeMap<BufferId, BufferDecorations>,
    prompt: Option<Prompt>,
    search_prompt_origin: Option<SearchPromptOrigin>,
    last_search_direction: SearchDirection,
    search_highlight: bool,
    last_substitute: Option<LastSubstitute>,
    substitute_confirmation: Option<SubstituteConfirmation>,
    save_conflict: Option<SaveConflict>,
    message: String,
    debug_messages: Vec<(Severity, Box<str>)>,
    tasks: TaskRunner,
    active_task: Option<CancellationToken>,
    next_task_id: u64,
    terminal: Option<PtySession>,
    input_focus: InputFocus,
    mouse_selection: Option<MouseSelection>,
    picker_candidates: Vec<PickerItem>,
    picker_items: Vec<PickerItem>,
    picker_index: usize,
    picker_directory: Option<PathBuf>,
    picker_preview_title: String,
    picker_preview: String,
    picker_preview_scroll: usize,
    picker_preview_highlight_line: Option<usize>,
    picker_preview_decorations: Vec<DecorationSpan>,
    grep_generation: u64,
    grep_due: Option<Instant>,
    grep_pending: Option<GrepPickerRequest>,
    grep_task: Option<GrepPickerTask>,
    popup: Option<TextPopup>,
    popup_deadline: Option<Instant>,
    ace_jump: Option<AceJumpState>,
    completion: Option<CompletionSession>,
    completion_index: usize,
    completion_selected: bool,
    completion_documentation_scroll: usize,
    snippet_stops: Vec<Range<usize>>,
    snippet_stop_index: usize,
    lsp_completion: Option<CompletionSession>,
    lsps: Vec<PersistentLsp>,
    lsp_job: Option<LspJob>,
    lsp_start_due: Option<Instant>,
    lsp_semantic_dirty: bool,
    pending_lsp_request: Option<PendingLspRequest>,
    leader_keys: Option<String>,
    leader_deadline: Option<Instant>,
    keymap: RuntimeKeymap,
    normal_prefix: Option<char>,
    last_picker_query: String,
    last_picker_source: Option<PickerSource>,
    recent_files: Vec<PathBuf>,
    diagnostics: Vec<QuickfixEntry>,
    format_on_save: bool,
    format_disabled: BTreeSet<DocumentId>,
    breakpoints: BTreeMap<PathBuf, BTreeMap<usize, Option<String>>>,
    root_workspace: PathBuf,
    workspace_folders: Vec<PathBuf>,
    debug_ui_visible: bool,
    agent_terminal: Option<PtySession>,
    agent_sidebar_visible: bool,
    last_staged_patch: Option<Vec<u8>>,
    theme: EditorTheme,
    viewport_rows: usize,
    viewport_columns: usize,
    realtime_decorations_prepared: Cell<bool>,
    startup_screen: RefCell<StartupScreen>,
    started_at: Instant,
    foreground_frame_pending: bool,
    quit: bool,
}

#[derive(Debug, Clone)]
struct SaveConflict {
    path: PathBuf,
    base: Arc<str>,
    ours: Arc<str>,
    theirs: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThreeWayMerge {
    text: String,
    conflicts: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum AgentInputPrefix {
    #[default]
    None,
    TerminalEscape,
    Window,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum InputFocus {
    Agent(AgentInputPrefix),
    #[default]
    Editor,
    Terminal {
        escape_pending: bool,
    },
}

impl InputFocus {
    const fn is_agent(self) -> bool {
        matches!(self, Self::Agent(_))
    }

    const fn is_terminal(self) -> bool {
        matches!(self, Self::Terminal { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingLspRequest {
    method: &'static str,
    hover: bool,
}

struct LspJob {
    starting: bool,
    language_id: Box<str>,
    navigation: Option<LspNavigationCapabilities>,
    receiver: mpsc::Receiver<LspCompletion>,
}

impl PendingLspRequest {
    const HOVER: Self = Self::new("textDocument/hover", true);
    const SIGNATURE_HELP: Self = Self::new("textDocument/signatureHelp", true);
    const DECLARATION: Self = Self::new("textDocument/declaration", false);
    const DEFINITION: Self = Self::new("textDocument/definition", false);
    const TYPE_DEFINITION: Self = Self::new("textDocument/typeDefinition", false);
    const IMPLEMENTATION: Self = Self::new("textDocument/implementation", false);

    const fn new(method: &'static str, hover: bool) -> Self {
        Self { method, hover }
    }

    const fn label(self) -> &'static str {
        if self.hover { "hover" } else { "definition" }
    }

    fn completion(self, document_id: DocumentId, revision: DocumentRevision, response: Result<serde_json::Value, String>) -> LspCompletion {
        Box::new(move |app| {
            let method = self.method;
            let value = match response {
                Ok(value) => value,
                Err(error) => return app.show_error(format!("{method}: {error}")),
            };
            match self.hover {
                false => {
                    if let Err(error) = app.finish_lsp_location(method, &value) {
                        app.show_error(format!("{method}: {error}"));
                    }
                }
                true if app.active.document_id == document_id && app.active.editor.revision() == revision => {
                    app.finish_lsp_hover(method, &value);
                }
                true => {}
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedVisibleDecorations {
    range: Range<usize>,
    state: DecorationState,
    spans: SharedDecorations,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DecorationState {
    transforms: Vec<Transaction>,
    overrides: Vec<DecorationSpan>,
    invalidated: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BufferDecorations {
    revision: DocumentRevision,
    /// Provider-owned baseline spans in the coordinate space before
    /// `transforms`. Ordinary edits update the small visible override set,
    /// rather than rewriting every span in a large file.
    spans: Vec<DecorationSpan>,
    prefix_max_end: Vec<usize>,
    state: DecorationState,
    visible_cache: RefCell<Vec<CachedVisibleDecorations>>,
}

const MAX_CACHED_DECORATION_TRANSFORMS: usize = 2;
const MAX_CACHED_DECORATION_OVERRIDES: usize = 128;
const MAX_CACHED_INVALIDATED_RANGES: usize = 8;
const VISIBLE_DECORATION_CACHE_CAPACITY: usize = 8;

impl DecorationState {
    fn new() -> Self {
        Self { transforms: Vec::with_capacity(4), overrides: Vec::with_capacity(64), invalidated: Vec::with_capacity(4) }
    }

    fn advance(&mut self, transaction: &Transaction) {
        self.overrides = std::mem::take(&mut self.overrides).into_iter().filter_map(|span| map_decoration_span(span, transaction)).collect();
        self.invalidated = std::mem::take(&mut self.invalidated).into_iter().filter_map(|range| map_range(range, transaction)).collect();
        if self.transforms.last().is_some_and(|previous| previous.is_mapping_inverse(transaction)) {
            self.transforms.pop();
        } else {
            self.transforms.push(transaction.clone());
        }
    }

    fn replace_ranges(&mut self, ranges: &[Range<usize>], mut spans: Vec<DecorationSpan>) {
        self.overrides.retain(|span| !ranges.iter().any(|range| ranges_overlap(&span.range, range)));
        spans.sort_by(decoration_order);
        spans.dedup();
        self.overrides = merge_sorted_decorations(std::mem::take(&mut self.overrides).into_iter(), spans.into_iter());
        self.invalidated.extend(ranges.iter().cloned());
        merge_ranges(&mut self.invalidated);
    }

    fn cacheable(&self) -> bool {
        self.transforms.len() <= MAX_CACHED_DECORATION_TRANSFORMS
            && self.overrides.len() <= MAX_CACHED_DECORATION_OVERRIDES
            && self.invalidated.len() <= MAX_CACHED_INVALIDATED_RANGES
    }

    fn same_mapping(&self, other: &Self) -> bool {
        self.transforms.len() == other.transforms.len()
            && self.transforms.iter().zip(&other.transforms).all(|(left, right)| left.has_same_mapping_as(right))
            && self.overrides == other.overrides
            && self.invalidated == other.invalidated
    }
}

impl BufferDecorations {
    fn new(revision: DocumentRevision, mut spans: Vec<DecorationSpan>) -> Self {
        spans.sort_by_key(|span| (span.range.start, std::cmp::Reverse(span.range.end)));
        spans.dedup();
        let mut decorations =
            Self { revision, spans, prefix_max_end: Vec::new(), state: DecorationState::new(), visible_cache: RefCell::new(Vec::with_capacity(8)) };
        decorations.rebuild_index();
        decorations
    }

    fn rebuild_index(&mut self) {
        let mut maximum_end = 0;
        self.prefix_max_end = self
            .spans
            .iter()
            .map(|span| {
                maximum_end = maximum_end.max(span.range.end);
                maximum_end
            })
            .collect();
    }

    fn map_through(&mut self, transaction: &Transaction, revision: DocumentRevision) {
        self.state.advance(transaction);
        self.revision = revision;
    }

    #[cfg(test)]
    fn replace_after_transaction(&mut self, transaction: &Transaction, revision: DocumentRevision, ranges: &[Range<usize>], replacement: Vec<DecorationSpan>) {
        self.state.advance(transaction);
        self.replace_ranges(revision, ranges, replacement);
    }

    fn replace_ranges(&mut self, revision: DocumentRevision, ranges: &[Range<usize>], replacement: Vec<DecorationSpan>) {
        self.state.replace_ranges(ranges, replacement);
        self.revision = revision;
    }

    #[cfg(test)]
    fn spans_in(&self, range: Range<usize>) -> Vec<DecorationSpan> {
        self.spans_in_shared(range).as_ref().clone()
    }

    fn spans_in_shared(&self, range: Range<usize>) -> SharedDecorations {
        if let Some(spans) = self.cached_visible_spans(&range, &self.state) {
            return spans;
        }
        let spans = Arc::new(self.spans_in_state(range.clone(), &self.state));
        if !self.state.cacheable() {
            return spans;
        }
        self.remember_visible_state(CachedVisibleDecorations { range, state: self.state.clone(), spans: Arc::clone(&spans) });
        spans
    }

    fn prepare_mapped_visible(&self, transaction: &Transaction, range: Range<usize>) {
        self.prepare_visible_state(range, self.state_after(transaction));
    }

    fn prepare_replaced_visible(&self, transaction: &Transaction, ranges: &[Range<usize>], replacement: Vec<DecorationSpan>, range: Range<usize>) {
        let mut state = self.state_after(transaction);
        state.replace_ranges(ranges, replacement);
        self.prepare_visible_state(range, state);
    }

    fn state_after(&self, transaction: &Transaction) -> DecorationState {
        let mut state = self.state.clone();
        state.advance(transaction);
        state
    }

    fn prepare_visible_state(&self, range: Range<usize>, state: DecorationState) {
        if !state.cacheable() || self.cached_visible_spans(&range, &state).is_some() {
            return;
        }
        let spans = Arc::new(self.spans_in_state(range.clone(), &state));
        self.remember_visible_state(CachedVisibleDecorations { range, state, spans });
    }

    fn cached_visible_spans(&self, range: &Range<usize>, state: &DecorationState) -> Option<SharedDecorations> {
        state.cacheable().then(|| {
            self.visible_cache
                .borrow()
                .iter()
                .rev()
                .find(|cached| cached.range == *range && cached.state.same_mapping(state))
                .map(|cached| Arc::clone(&cached.spans))
        })?
    }

    fn remember_visible_state(&self, state: CachedVisibleDecorations) {
        let mut cache = self.visible_cache.borrow_mut();
        cache.push(state);
        if cache.len() > VISIBLE_DECORATION_CACHE_CAPACITY {
            cache.remove(0);
        }
    }

    fn spans_in_state(&self, range: Range<usize>, state: &DecorationState) -> Vec<DecorationSpan> {
        let base_range = state.transforms.iter().rev().fold(range.clone(), unmap_range);
        let first = self.prefix_max_end.partition_point(|maximum_end| *maximum_end <= base_range.start);
        let last = self.spans.partition_point(|span| span.range.start < base_range.end);
        let spans = self.spans[first..last].iter().cloned().filter_map(|span| {
            let span = state.transforms.iter().try_fold(span, map_decoration_span)?;
            (ranges_overlap(&span.range, &range) && !state.invalidated.iter().any(|invalidated| ranges_overlap(&span.range, invalidated))).then_some(span)
        });
        merge_sorted_decorations(spans, state.overrides.iter().filter(|span| ranges_overlap(&span.range, &range)).cloned())
    }
}

fn map_decoration_span(span: DecorationSpan, transaction: &Transaction) -> Option<DecorationSpan> {
    let range = transaction.map_range(span.range, Bias::Left, Bias::Right).ok()?;
    (!range.is_empty()).then_some(DecorationSpan::new(range, span.style, span.priority))
}

fn map_range(range: Range<usize>, transaction: &Transaction) -> Option<Range<usize>> {
    let start = transaction.map_offset(range.start, Bias::Left).ok()?;
    let end = transaction.map_offset(range.end, Bias::Right).ok()?;
    (start < end).then_some(start..end)
}

fn unmap_range(range: Range<usize>, transaction: &Transaction) -> Range<usize> {
    let start = unmap_offset(transaction, range.start, Bias::Left);
    let end = unmap_offset(transaction, range.end, Bias::Right);
    start.min(end)..end.max(start)
}

fn unmap_offset(transaction: &Transaction, offset: usize, bias: Bias) -> usize {
    let mut old_cursor = 0_usize;
    let mut new_cursor = 0_usize;
    for edit in transaction.edits() {
        let unchanged = edit.range.start.saturating_sub(old_cursor);
        let unchanged_end = new_cursor.saturating_add(unchanged);
        if offset < unchanged_end {
            return old_cursor.saturating_add(offset.saturating_sub(new_cursor));
        }
        new_cursor = unchanged_end;
        let inserted_end = new_cursor.saturating_add(edit.insert.len());
        if offset < inserted_end {
            return match bias {
                Bias::Left => edit.range.start,
                Bias::Right => edit.range.end,
            };
        }
        if offset == inserted_end {
            return edit.range.end;
        }
        new_cursor = inserted_end;
        old_cursor = edit.range.end;
    }
    old_cursor.saturating_add(offset.saturating_sub(new_cursor))
}

fn decoration_order(left: &DecorationSpan, right: &DecorationSpan) -> std::cmp::Ordering {
    (left.range.start, std::cmp::Reverse(left.range.end)).cmp(&(right.range.start, std::cmp::Reverse(right.range.end)))
}

fn merge_sorted_decorations(left: impl Iterator<Item = DecorationSpan>, right: impl Iterator<Item = DecorationSpan>) -> Vec<DecorationSpan> {
    let mut left = left.peekable();
    let mut right = right.peekable();
    let mut merged = Vec::with_capacity(left.size_hint().0.saturating_add(right.size_hint().0));
    loop {
        let next = match (left.peek(), right.peek()) {
            (Some(left_span), Some(right_span)) => {
                if decoration_order(left_span, right_span).is_le() {
                    left.next()
                } else {
                    right.next()
                }
            }
            (Some(_), None) => left.next(),
            (None, Some(_)) => right.next(),
            (None, None) => break,
        };
        if let Some(span) = next
            && merged.last() != Some(&span)
        {
            merged.push(span);
        }
    }
    merged
}

fn ace_jump_labels(count: usize) -> Vec<String> {
    const ALPHABET: &[u8] = b"asdfghjklqwertyuiopzxcvbnm";
    let mut width = 1_usize;
    let mut capacity = ALPHABET.len();
    while capacity < count {
        width = width.saturating_add(1);
        capacity = capacity.saturating_mul(ALPHABET.len());
    }
    (0..count)
        .map(|mut index| {
            let mut label = vec![ALPHABET[0]; width];
            for byte in label.iter_mut().rev() {
                *byte = ALPHABET[index % ALPHABET.len()];
                index /= ALPHABET.len();
            }
            String::from_utf8(label).unwrap_or_default()
        })
        .collect()
}

#[derive(Debug, Clone)]
struct Substitute {
    needle: String,
    replacement: String,
    ranges: Vec<Range<usize>>,
    flags: SubstituteFlags,
    persist_pattern: bool,
}

#[derive(Debug, Clone)]
struct LastSubstitute {
    needle: String,
    replacement: String,
    flags: SubstituteFlags,
}

#[derive(Debug)]
struct SubstituteConfirmation {
    base_revision: DocumentRevision,
    original_text: String,
    candidates: Vec<Edit>,
    accepted: Vec<Edit>,
    index: usize,
    print: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QuickfixEntry {
    path: PathBuf,
    line: usize,
    column: usize,
    selection_end: Option<(usize, usize)>,
    column_utf16: bool,
    severity: Severity,
    text: String,
}

struct GrepPickerRequest {
    generation: u64,
    query: String,
    root: PathBuf,
}

struct GrepPickerTask {
    generation: u64,
    query: String,
    child: Arc<LocalMutex<Option<std::process::Child>>>,
    cancelled: Arc<AtomicBool>,
    receiver: mpsc::Receiver<std::result::Result<Vec<QuickfixEntry>, String>>,
}

impl GrepPickerTask {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(child) = self.child.lock().as_mut() {
            let _ = child.kill();
        }
    }
}

impl Drop for GrepPickerTask {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl QuickfixEntry {
    fn new(path: impl Into<PathBuf>, line: usize, column: usize, text: impl Into<String>) -> Self {
        Self { path: path.into(), line, column, selection_end: None, column_utf16: false, severity: Severity::None, text: text.into() }
    }

    fn diagnostic(path: impl Into<PathBuf>, line: usize, column: usize, severity: Severity, message: impl Into<String>) -> Self {
        Self { severity, ..Self::new(path, line, column, message) }
    }

    fn utf16(mut self) -> Self {
        self.column_utf16 = true;
        self
    }

    fn with_end(mut self, line: usize, column: usize) -> Self {
        self.selection_end = Some((line, column));
        self
    }

    fn display(&self) -> String {
        let severity = self.severity.label();
        let separator = if severity.is_empty() { "" } else { ": " };
        format!("{}:{}:{}  {severity}{separator}{}", self.path.display(), self.line, self.column, compact(&self.text, 80))
    }

    fn selection_byte_range(&self, text: &str) -> Option<Range<usize>> {
        let (end_line, end_column) = self.selection_end?;
        let line_starts = std::iter::once(0).chain(text.match_indices('\n').map(|(byte, _)| byte.saturating_add(1))).collect::<Vec<_>>();
        let start = quickfix_position_byte(text, &line_starts, self.line, self.column, self.column_utf16)?;
        let end = quickfix_position_byte(text, &line_starts, end_line, end_column, self.column_utf16)?;
        (start < end).then_some(start..end)
    }
}

fn quickfix_position_byte(text: &str, line_starts: &[usize], line: usize, column: usize, utf16: bool) -> Option<usize> {
    let line = line.checked_sub(1)?;
    let line_start = *line_starts.get(line)?;
    let line_end = line_starts.get(line.saturating_add(1)).map_or(text.len(), |next| next.saturating_sub(1));
    let line_text = text.get(line_start..line_end)?;
    let column = column.checked_sub(1)?;
    if !utf16 {
        return (column <= line_text.len()).then_some(line_start + column);
    }
    utf16_column_to_byte(line_text, column).ok().map(|byte| line_start + byte)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    Error,
    Warning,
    Info,
    Hint,
    None,
}

impl Severity {
    const fn label(self) -> &'static str {
        ["error", "warning", "info", "hint", ""][self as usize]
    }
}

fn is_navigation_key(key: TerminalKey) -> bool {
    if key.command_modified() {
        return false;
    }
    matches!(
        key.code,
        TerminalKeyCode::Left
            | TerminalKeyCode::Right
            | TerminalKeyCode::Up
            | TerminalKeyCode::Down
            | TerminalKeyCode::Home
            | TerminalKeyCode::End
            | TerminalKeyCode::PageUp
            | TerminalKeyCode::PageDown
            | TerminalKeyCode::Escape
            | TerminalKeyCode::Char('h' | 'j' | 'k' | 'l' | 'w' | 'b' | 'e' | '0' | '$')
    )
}

fn terminal_key_bytes(key: TerminalKey) -> Option<Vec<u8>> {
    if key.super_key() {
        return None;
    }
    let mut bytes = Vec::new();
    if key.alt() {
        bytes.push(0x1b);
    }
    match key.code {
        TerminalKeyCode::Char(character) if key.control() && character.is_ascii() => {
            let value = u8::try_from(character.to_ascii_uppercase()).ok()?;
            bytes.push(value & 0x1f);
        }
        TerminalKeyCode::Char(character) => {
            let mut encoded = [0_u8; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
        }
        TerminalKeyCode::Escape => bytes.push(0x1b),
        TerminalKeyCode::Enter => bytes.push(b'\r'),
        TerminalKeyCode::Tab => bytes.push(b'\t'),
        TerminalKeyCode::Backspace => bytes.push(0x7f),
        TerminalKeyCode::Delete => bytes.extend_from_slice(b"\x1b[3~"),
        TerminalKeyCode::Insert => bytes.extend_from_slice(b"\x1b[2~"),
        TerminalKeyCode::Home => bytes.extend_from_slice(b"\x1b[H"),
        TerminalKeyCode::End => bytes.extend_from_slice(b"\x1b[F"),
        TerminalKeyCode::PageUp => bytes.extend_from_slice(b"\x1b[5~"),
        TerminalKeyCode::PageDown => bytes.extend_from_slice(b"\x1b[6~"),
        TerminalKeyCode::Left => bytes.extend_from_slice(b"\x1b[D"),
        TerminalKeyCode::Right => bytes.extend_from_slice(b"\x1b[C"),
        TerminalKeyCode::Up => bytes.extend_from_slice(b"\x1b[A"),
        TerminalKeyCode::Down => bytes.extend_from_slice(b"\x1b[B"),
    }
    Some(bytes)
}

fn terminal_mouse_bytes(event: &TerminalInput) -> Vec<u8> {
    let encode = |button: u8, column: usize, row: usize, release: bool| {
        format!("\u{1b}[<{button};{};{}{}", column.saturating_add(1), row.saturating_add(1), if release { 'm' } else { 'M' })
    };
    let TerminalInput::Mouse { action, column, row } = event else {
        return Vec::new();
    };
    match action {
        MouseAction::Click => encode(0, *column, *row, false).into_bytes(),
        MouseAction::Drag => encode(32, *column, *row, false).into_bytes(),
        MouseAction::Release => encode(0, *column, *row, true).into_bytes(),
        MouseAction::Scroll(lines) => {
            let button = if *lines < 0 { 64 } else { 65 };
            let events = lines.unsigned_abs().div_ceil(3).max(1);
            encode(button, *column, *row, false).repeat(events).into_bytes()
        }
    }
}

const fn substitute_case_override(flags: SubstituteFlags) -> CaseOverride {
    match flags.case_sensitive {
        None => CaseOverride::Default,
        Some(false) => CaseOverride::Ignore,
        Some(true) => CaseOverride::Sensitive,
    }
}

fn address_search_pattern(address: &ExAddress) -> Option<(&str, SearchDirection)> {
    match address {
        ExAddress::SearchForward(pattern) => Some((pattern, SearchDirection::Forward)),
        ExAddress::SearchBackward(pattern) => Some((pattern, SearchDirection::Backward)),
        ExAddress::Offset { base, .. } => address_search_pattern(base),
        ExAddress::Current | ExAddress::Last | ExAddress::Line(_) | ExAddress::Mark(_) => None,
    }
}

fn parse_inccommand_substitute(input: &str) -> Option<ExCommand> {
    let is_substitute = |command: &ExCommand| matches!(command, ExCommand::Substitute { .. } | ExCommand::SubstituteRepeat { .. });
    if let Ok(command) = parse_ex(input)
        && is_substitute(&command)
    {
        return Some(command);
    }
    let command_byte = input.char_indices().find_map(|(index, character)| {
        if character != 's' {
            return None;
        }
        let before = &input[..index];
        before.chars().all(|value| value.is_ascii_digit() || matches!(value, '%' | ',' | ';' | '.' | '$')).then_some(index)
    })?;
    let tail = &input[command_byte + 1..];
    let delimiter = tail.chars().next().filter(|value| !value.is_alphanumeric() && !value.is_whitespace())?;
    for missing in 1..=2 {
        let mut candidate = input.to_owned();
        candidate.extend(std::iter::repeat_n(delimiter, missing));
        if let Ok(command) = parse_ex(&candidate)
            && is_substitute(&command)
        {
            return Some(command);
        }
    }
    None
}

fn has_unescaped_tilde(input: &str) -> bool {
    let mut escaped = false;
    for character in input.chars() {
        match (escaped, character) {
            (true, _) => escaped = false,
            (false, '\\') => escaped = true,
            (false, '~') => return true,
            (false, _) => {}
        }
    }
    false
}

fn plan_substitution_edits(
    text: &str,
    pattern: &VimPattern,
    replacement: &VimReplacement,
    ranges: &[Range<usize>],
    global: bool,
    mut checkpoint: impl FnMut() -> Result<(), TaskFailure>,
) -> Result<Vec<Edit>, TaskFailure> {
    let mut ranges = ranges.to_vec();
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut edits = Vec::new();
    let mut processed_bytes = 0_usize;
    for range in ranges {
        let start = range.start.min(text.len());
        let end = range.end.min(text.len()).max(start);
        let slice = &text[start..end];
        let mut scan_cursor = 0_usize;
        let mut line = 0_usize;
        let mut replaced_line = None;
        for captures in pattern.captures_iter(slice) {
            let Some(found) = captures.get(0) else {
                continue;
            };
            line += slice[scan_cursor..found.start()].bytes().filter(|byte| *byte == b'\n').count();
            scan_cursor = found.start();
            if global || replaced_line != Some(line) {
                edits.push(Edit::new(start + found.start()..start + found.end(), replacement.expand(&captures)));
                replaced_line = Some(line);
            }
            let absolute = start + found.end();
            if absolute.saturating_sub(processed_bytes) >= 4_096 {
                checkpoint()?;
                processed_bytes = absolute;
            }
        }
        checkpoint()?;
        processed_bytes = end;
    }
    edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
    edits.dedup_by(|left, right| left.range == right.range && left.insert == right.insert);
    Ok(edits)
}

fn substitution_message(count: usize, print: bool, original: &str, transaction: &Transaction) -> String {
    let summary = format!("{count} substitution(s)");
    if !print {
        return summary;
    }
    let Ok(changed) = transaction.apply_to_string(original) else {
        return summary;
    };
    let Some(last) = transaction.edits().last() else {
        return summary;
    };
    let anchor = transaction.map_offset(last.range.start, Bias::Left).unwrap_or(last.range.start).min(changed.len());
    let start = changed[..anchor].rfind('\n').map_or(0, |byte| byte + 1);
    let end = changed[anchor..].find('\n').map_or(changed.len(), |byte| anchor + byte);
    format!("{summary} │ {}", compact(&changed[start..end], 120))
}

fn ex_normal_keys(input: &str) -> Vec<KeyEvent> {
    let mut keys = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        if let Some((after, code)) = [("<Esc>", KeyCode::Escape), ("<CR>", KeyCode::Enter), ("<Tab>", KeyCode::Tab)]
            .into_iter()
            .find_map(|(prefix, code)| rest.strip_prefix(prefix).map(|after| (after, code)))
        {
            keys.push(KeyEvent::plain(code));
            rest = after;
        } else {
            let Some(character) = rest.chars().next() else { break };
            keys.push(KeyEvent::character(character));
            rest = &rest[character.len_utf8()..];
        }
    }
    keys
}

fn parse_vimgrep_line(line: &str) -> Option<QuickfixEntry> {
    let mut fields = line.splitn(4, ':');
    Some(QuickfixEntry::new(fields.next()?, fields.next()?.parse().ok()?, fields.next()?.parse().ok()?, fields.next().unwrap_or_default()))
}

fn stable_document_id(path: Option<&Path>) -> DocumentId {
    let Some(path) = path else {
        return DocumentId::new(1);
    };
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    DocumentId::new(stable_hash(canonical.to_string_lossy().bytes()).max(2))
}

/// Merges meaningfully independent edits from the disk and local replicas.
/// The base snapshot makes this a semantic three-way operation: a line is
/// selected for its relationship to the common document state, not simply by
/// preferring whichever complete file was read last. Overlapping changes stay
/// explicit in the output so the merge pane never silently drops either side.
fn semantic_three_way_merge(base: &str, ours: &str, theirs: &str) -> ThreeWayMerge {
    let base_lines = base.split_inclusive('\n').collect::<Vec<_>>();
    let ours_lines = ours.split_inclusive('\n').collect::<Vec<_>>();
    let theirs_lines = theirs.split_inclusive('\n').collect::<Vec<_>>();
    // Fast path for a stable line shape. Treat each line as a semantic unit
    // relative to its shared base so independently changed neighbouring lines
    // do not become one coarse textual conflict region.
    if base_lines.len() == ours_lines.len() && base_lines.len() == theirs_lines.len() {
        let mut text = String::with_capacity(base.len().max(ours.len()).max(theirs.len()));
        let mut conflicts = 0;
        for ((base, ours), theirs) in base_lines.iter().zip(&ours_lines).zip(&theirs_lines) {
            match (*ours == *theirs, *ours == *base, *theirs == *base) {
                (true, _, _) | (_, true, false) => text.push_str(theirs),
                (_, false, true) => text.push_str(ours),
                _ => {
                    conflicts += 1;
                    text.push_str("<<<<<<< ours\n");
                    append_merge_lines(&mut text, &[*ours]);
                    text.push_str("=======\n");
                    append_merge_lines(&mut text, &[*theirs]);
                    text.push_str(">>>>>>> theirs\n");
                }
            }
        }
        return ThreeWayMerge { text, conflicts };
    }
    let mut text = String::with_capacity(base.len().max(ours.len()).max(theirs.len()));
    let mut conflicts = 0;
    for group in Merge3::new(&base_lines, &ours_lines, &theirs_lines).merge_groups() {
        match group {
            MergeGroup::Unchanged(lines) | MergeGroup::Same(lines) | MergeGroup::A(lines) | MergeGroup::B(lines) => {
                text.extend(lines.iter().copied());
            }
            MergeGroup::Conflict(_base, ours, theirs) => {
                conflicts += 1;
                text.push_str("<<<<<<< ours\n");
                append_merge_lines(&mut text, ours);
                text.push_str("=======\n");
                append_merge_lines(&mut text, theirs);
                text.push_str(">>>>>>> theirs\n");
            }
        }
    }
    ThreeWayMerge { text, conflicts }
}

fn append_merge_lines(output: &mut String, lines: &[&str]) {
    for line in lines {
        output.push_str(line);
    }
    if !lines.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn complete_path(fragment: &str) -> Option<String> {
    let expanded =
        if let Some(rest) = fragment.strip_prefix("~/") { env::var_os("HOME").map(|home| PathBuf::from(home).join(rest))? } else { PathBuf::from(fragment) };
    let directory = expanded.parent().unwrap_or_else(|| Path::new("."));
    let prefix = expanded.file_name().and_then(std::ffi::OsStr::to_str).unwrap_or_default();
    let mut matches =
        std::fs::read_dir(directory).ok()?.filter_map(Result::ok).filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix)).collect::<Vec<_>>();
    matches.sort_by_key(std::fs::DirEntry::file_name);
    let entry = matches.first()?;
    let path = entry.path();
    let mut completed = if fragment.starts_with("~/") {
        let home = PathBuf::from(env::var_os("HOME")?);
        format!("~/{}", path.strip_prefix(home).ok()?.to_string_lossy())
    } else {
        path.to_string_lossy().into_owned()
    };
    if path.is_dir() {
        completed.push(std::path::MAIN_SEPARATOR);
    }
    Some(completed)
}

fn system_clipboard_text(_register: char) -> Option<String> {
    Command::new("pbpaste").output().ok().filter(|output| output.status.success()).and_then(|output| String::from_utf8(output.stdout).ok())
}

fn save_buffer(buffer: &mut BufferState) -> Result<()> {
    if let Some(wal) = &buffer.wal {
        wal.barrier().context("make recovery WAL durable before save")?;
    }
    let report = buffer.document.save(&buffer.editor.contents())?;
    buffer.editor.mark_clean();
    buffer.base_hash = report.stamp.content_hash;
    save_undo_state(buffer)?;
    if let Some(wal) = &buffer.wal {
        wal.clear().context("compact recovery WAL after save")?;
    }
    Ok(())
}

fn compact(text: &str, limit: usize) -> String {
    let escaped = text.replace('\n', "\\n").replace('\t', "\\t");
    let mut compact: String = escaped.chars().take(limit).collect();
    if escaped.chars().count() > limit {
        compact.push('…');
    }
    compact
}

struct ChannelDisconnected;

fn poll_channel<T>(receiver: &mpsc::Receiver<T>) -> Result<Option<T>, ChannelDisconnected> {
    match receiver.try_recv() {
        Ok(value) => Ok(Some(value)),
        Err(mpsc::TryRecvError::Empty) => Ok(None),
        Err(mpsc::TryRecvError::Disconnected) => Err(ChannelDisconnected),
    }
}

fn grammar_key(key: TerminalKey) -> Option<KeyEvent> {
    let code = match key.code {
        TerminalKeyCode::PageUp | TerminalKeyCode::Up => KeyCode::Up,
        TerminalKeyCode::PageDown | TerminalKeyCode::Down => KeyCode::Down,
        TerminalKeyCode::Insert => return None,
        code => code,
    };
    Some(KeyEvent::modified(code, key.modifiers))
}

fn format_key_event(key: &KeyEvent) -> String {
    let base = match key.code {
        KeyCode::Char(' ') => "Space".to_owned(),
        KeyCode::Char(character) => character.to_string(),
        KeyCode::Escape => "Esc".to_owned(),
        KeyCode::Enter => "CR".to_owned(),
        KeyCode::Tab => "Tab".to_owned(),
        KeyCode::Backspace => "BS".to_owned(),
        KeyCode::Delete => "Del".to_owned(),
        KeyCode::Insert => "Insert".to_owned(),
        KeyCode::Home => "Home".to_owned(),
        KeyCode::End => "End".to_owned(),
        KeyCode::PageUp => "PageUp".to_owned(),
        KeyCode::PageDown => "PageDown".to_owned(),
        KeyCode::Left => "Left".to_owned(),
        KeyCode::Right => "Right".to_owned(),
        KeyCode::Up => "Up".to_owned(),
        KeyCode::Down => "Down".to_owned(),
    };
    if key.modifiers.is_empty() {
        return match key.code {
            KeyCode::Char(character) if character != ' ' && !character.is_control() => base,
            _ => format!("<{base}>"),
        };
    }
    let modifiers = [(Modifiers::CONTROL, "C"), (Modifiers::SHIFT, "S"), (Modifiers::ALT, "A"), (Modifiers::SUPER, "D")]
        .into_iter()
        .filter_map(|(flag, label)| key.modifiers.contains(flag).then_some(label))
        .collect::<Vec<_>>();
    format!("<{}-{base}>", modifiers.join("-"))
}

fn changed_editor_state(
    registers_before: &BTreeMap<char, (Box<str>, bool)>,
    marks_before: &BTreeMap<char, usize>,
    macros_before: &BTreeMap<char, Vec<KeyEvent>>,
    editor: &Editor,
    document_id: DocumentId,
) -> Vec<StateDelta> {
    let mut changed = Vec::new();
    changed.extend(
        editor
            .registers()
            .filter(|(name, value)| {
                registers_before.get(name).map(|(text, linewise)| (text.as_ref(), *linewise)) != Some((value.text.as_ref(), value.linewise))
            })
            .map(|(name, value)| StateDelta::Register { name, text: value.text.clone(), linewise: value.linewise }),
    );
    changed.extend(
        editor.marks().filter(|(name, byte)| name.is_ascii_uppercase() && marks_before.get(name) != Some(byte)).map(|(name, byte)| StateDelta::GlobalMark {
            name,
            document_id,
            anchor: Anchor { byte, bias: Bias::Right },
        }),
    );
    changed.extend(editor.macros().filter(|(name, keys)| macros_before.get(name).is_none_or(|old| old.as_slice() != *keys)).filter_map(|(name, keys)| {
        Some(StateDelta::MacroRecording {
            name,
            raw_keys: serde_json::to_vec(keys).ok()?,
            lowered_ir: serde_json::to_vec(&keys.iter().map(|key| format!("{:?}:{:?}", key.modifiers, key.code)).collect::<Vec<_>>()).ok()?,
        })
    }));
    changed
}

#[cfg(test)]
mod tests;
