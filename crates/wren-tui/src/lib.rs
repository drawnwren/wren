use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{Cursor, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use wren_client_state::{ClientViewStateStore, DurableClientState};
use wren_command::{CancellationToken, TaskFailure, TaskRunner};
use wren_config::{CommandRegistry, WorkspaceTrust, executable_hash, parse_and_validate};
use wren_engine::{
    CaseOverride, DurableUndoState, Editor, EngineError, FrameText, Mode, SearchDirection,
    VimPattern, VimReplacement, resolve_previous_replacement,
};
use wren_grammar::{
    BufferAction, ExAddress, ExCommand, ExRange, ExpressionContext, KeyCode, KeyEvent, Modifiers,
    ParseState, SubstituteFlags, TabAction, Value, evaluate_expression, parse_ex,
};
use wren_presenter::Presenter;
#[cfg(test)]
use wren_provider::ProviderActor;
#[cfg(not(test))]
use wren_provider::ProviderSupervisor;
use wren_provider::{
    CompletionCandidate, CompletionSession, HighlightSpan, ProviderRequest, ProviderResponse,
    bundled_language_id, fuzzy_rank, highlight_text, lexical_highlight_text,
};
use wren_session::{
    DocumentEncoding, LocalDocument, LocalWal, MutationOutbox, MutationSubmission, OpenedDocument,
    RecoveredState, SaveWarning, SessionAuthority, SessionJournal,
};
use wren_term::{
    ClipboardSelection, SystemTerminalBackend, TerminaBackend, TerminalInput, TerminalKey,
    TerminalKeyCode,
};
use wren_text::{DefaultText, TextStore};
use wren_types::{
    Anchor, Bias, BufferId, ClientId, ClientMutation, ClientSequence, CommandClass,
    CommandInvocation, CommandSchema, CommandTask, CommandTaskId, DocumentClass, DocumentId,
    DocumentMutation, DocumentRevision, DurableJumpEntry, Edit, EditProposal, Effects, Freshness,
    FreshnessKey, LanguageBundle, MutationId, Priority, ProviderDemand, SemanticGroupId,
    SemanticGroupKind, SessionId, StateDelta, Transaction,
};
use wren_view::{
    AceJumpOverlay, AceJumpTarget, CatppuccinFlavor, CatppuccinPalette, Cell as ViewCell,
    CellColor, CellRow, CellStyle, ClientViewModel, CompletionOverlay, CompletionOverlayRow,
    DebugOverlay, DecorationSpan, DesiredGrid, LineDecoration, MessageEntry, MessageSeverity,
    PickerOverlay, PickerOverlayRow, RgbColor, SplitAxis, StatusOverlay, StatusSegment, TextPopup,
    ViewportLayout, WindowDirection,
};
use wren_workflow::{
    DocumentVisibility, GitHunk, LspClient, LspPosition, LspTextEdit, PtySession, SavePolicy,
    TaskSpec as WorkflowTaskSpec, TaskSupervisor, TerminalColor, WorkflowError, git_hunks,
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

mod latency;
pub use latency::{ProductionLatencyReport, ProductionLatencySample, run_production_latency_probe};

const HELP: &str = "\
wren — a small, reliable modal code editor

USAGE:
    wren [FILE] [+LINE]

NORMAL MODE:
    h j k l / arrows     move          w b e       words
    0 ^ $ gg G           line/file     f{char}     find on line
    i a I A o O          insert/open   Esc         normal mode
    d c y + motion       operator      dd cc yy    whole line
    x X D C J r{char}    edit          p P         paste
    u / Ctrl-R           undo/redo     .           repeat change
    / ? then Enter       search        n N         next/previous
    m{letter} / `{mark}  marks         q{reg}...q  record macro
    @{reg} / @@           replay macro
    \"=EXPR then p          evaluate closed expression register

DOTFILE KEYS:
    Space q / Space w     quit/write    Space Space  select line
    Space ff              fuzzy files   Space fb     file browser
    Space fr / Space fw   grep repo     Space b      buffers
    Space fo/fj/fd        oldfiles/jumps/diagnostics
    Space jj              Ace jump to a visible character
    Space e               diagnostic    [d / ]d      diagnostic nav
    [c / ]c               Git hunk nav  Space g…     Git hunk actions
    gd/gD/gi/gr/K         LSP navigation, refs, hover
    Space rn/ca/D         rename/action/type definition
    Space f               format        :FormatToggle[!] on-save toggle
    Space d…              debug/REPL     Space h…/r…  Haskell/Hoogle/REPL
    Ctrl-h/j/k/l          focus split window

INSERT COMPLETION:
    Ctrl-Space            complete       Ctrl-N/P     select candidate
    Enter                 accept selected Ctrl-E      abort completion
    Ctrl-B/F              scroll completion documentation

EX COMMANDS:
    :w [FILE]   :q[!]   :wq   :x       save and quit
    :e[!] FILE                         edit another file
    :s/old/new/[gciIp]  :%s/old/new/g  Vim-regex substitution
    :s   :&   :~                         repeat previous replacement
    :undo   :redo   :normal KEYS       editing commands
    :registers   :marks                inspect durable state
    :messages / :debuglog               open message history buffer
    :find [QUERY]                       fuzzy file picker
    :terminal [PROGRAM]                 interactive terminal buffer
    :make PROGRAM [ARGS]                cancellable build task
    :format PROGRAM [ARGS]              revision-safe formatter
    :Git                                 lazygit diff/status interface
    :Git ARGS    :Gwrite  :Gdiffsplit   direct and native Git commands
    :Codex / :AvanteChat / :AvanteAsk   Codex assistant float

Unsaved edits use a checksummed recovery WAL; undo history and oldfiles persist.
";

const MESSAGES_BUFFER_NAME: &str = "[Messages]";
const LSP_START_IDLE_PERIOD: Duration = Duration::from_millis(750);
const LSP_SEMANTIC_IDLE_PERIOD: Duration = Duration::from_millis(750);
type PresenterBackend = TerminaBackend<std::io::Stdout>;
type SharedPresenterBackend = Arc<Mutex<PresenterBackend>>;

pub fn main_entry() -> Result<()> {
    if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--internal-provider-host")) {
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
    let (columns, rows) = terminal.size().context("query terminal size")?;
    let output = Arc::new(Mutex::new(
        TerminaBackend::new(std::io::stdout(), columns, rows).context("open terminal presenter")?,
    ));
    let presenter = Presenter::start(Arc::clone(&output))?;
    let mut layout = ViewportLayout::new(columns, rows);
    layout.configure_dotfile_profile();
    app.resize_terminal(rows, columns);
    app.schedule_provider_refreshes(layout.height);
    presenter.publish(desired_frame(&mut layout, &app))?;
    let mut pending_input = None;

    while !app.quit {
        let mut needs_render = false;
        let input = match pending_input.take() {
            Some(input) => Some(input),
            None => terminal
                .poll_input(Some(Duration::from_millis(4)))
                .context("read terminal input")?,
        };
        if let Some(input) = input {
            let (input, next) = coalesce_mouse_scroll_input(input, |timeout| {
                terminal
                    .poll_input(Some(timeout))
                    .context("drain terminal input")
            })?;
            pending_input = next;
            needs_render |=
                handle_terminal_event(input, &mut app, &mut terminal, &output, &mut layout)?;
        }
        // Quitting is a local editor action. Once accepted, leave the event
        // loop before polling providers or language servers so their state can
        // never delay or turn a successful quit into an error exit.
        if app.quit {
            break;
        }
        needs_render |= poll_app_work(&mut app)?;
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
    let _stats = presenter.finish()?;
    Ok(())
}

const fn clipboard_selection(register: char) -> ClipboardSelection {
    if register == '*' {
        ClipboardSelection::Primary
    } else {
        ClipboardSelection::Clipboard
    }
}

fn restore_clipboard_for_input(
    app: &mut App,
    terminal: &mut SystemTerminalBackend,
    event: &TerminalInput,
) {
    let Some(register) = app.clipboard_register_for_paste(event) else {
        return;
    };
    let clipboard =
        match terminal.paste_osc52(clipboard_selection(register), Duration::from_secs(1)) {
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
        app.restore_clipboard_register(register, text);
    }
}

fn dispatch_terminal_event(app: &mut App, layout: &ViewportLayout, event: TerminalInput) {
    let result = match event {
        TerminalInput::MouseClick { column, row } => app.handle_mouse_click(layout, column, row),
        TerminalInput::MouseDrag { column, row } => app.handle_mouse_drag(layout, column, row),
        TerminalInput::MouseRelease { column, row } => {
            app.handle_mouse_release(layout, column, row)
        }
        event => app.handle_input(event),
    };
    if let Err(error) = result {
        app.show_error(error);
    }
}

fn flush_clipboard_writes(app: &mut App, output: &SharedPresenterBackend) -> Result<()> {
    for (register, text) in app.take_clipboard_writes() {
        if let Err(error) = output
            .lock()
            .map_err(|_| anyhow!("presenter backend lock is poisoned"))?
            .copy_osc52(clipboard_selection(register), &text)
        {
            app.show_error(format!("clipboard: {error}"));
        }
    }
    Ok(())
}

fn handle_terminal_event(
    event: TerminalInput,
    app: &mut App,
    terminal: &mut SystemTerminalBackend,
    output: &SharedPresenterBackend,
    layout: &mut ViewportLayout,
) -> Result<bool> {
    if let TerminalInput::Resized { columns, rows } = event {
        layout.resize(columns, rows);
        app.resize_terminal(rows, columns);
        output
            .lock()
            .map_err(|_| anyhow!("presenter backend lock is poisoned"))?
            .resize(columns, rows);
        return Ok(true);
    }
    if !input_requires_render(&event) {
        return Ok(false);
    }
    restore_clipboard_for_input(app, terminal, &event);
    dispatch_terminal_event(app, layout, event);
    app.capture_debug_output();
    flush_clipboard_writes(app, output)?;
    Ok(true)
}

fn poll_app_work(app: &mut App) -> Result<bool> {
    let mut changed = app.poll_task_results()?;
    changed |= app.poll_provider_results();
    changed |= app.poll_git_hunk_results();
    app.poll_lsp_start_due();
    changed |= app.poll_lsp_start()?;
    changed |= app.poll_lsp_background()?;
    changed |= app.poll_lsp_semantic_due()?;
    changed |= app.poll_terminal()?;
    changed |= app.poll_mapping_timeout()?;
    changed |= app.poll_popup_timeout();
    Ok(changed)
}

fn coalesce_mouse_scroll_input(
    first: TerminalInput,
    mut poll: impl FnMut(Duration) -> Result<Option<TerminalInput>>,
) -> Result<(TerminalInput, Option<TerminalInput>)> {
    let TerminalInput::MouseScroll {
        mut lines,
        mut column,
        mut row,
    } = first
    else {
        return Ok((first, None));
    };
    // Ghostty can emit wheel events faster than a frame can be presented, and
    // Termina can expose bytes from one burst over several decoder reads. Give
    // that decoder a tiny scroll-only grace period, then drain a bounded burst
    // into one viewport transaction so old events never queue behind renders.
    for _ in 0..255 {
        match poll(Duration::from_millis(2))? {
            Some(TerminalInput::MouseScroll {
                lines: next,
                column: next_column,
                row: next_row,
            }) => {
                lines = lines.saturating_add(next);
                column = next_column;
                row = next_row;
            }
            Some(TerminalInput::Ignored) => {}
            Some(input) => {
                return Ok((
                    TerminalInput::MouseScroll { lines, column, row },
                    Some(input),
                ));
            }
            None => break,
        }
    }
    Ok((TerminalInput::MouseScroll { lines, column, row }, None))
}

fn input_requires_render(input: &TerminalInput) -> bool {
    !matches!(input, TerminalInput::Ignored)
}

fn desired_frame(layout: &mut ViewportLayout, app: &App) -> Arc<DesiredGrid> {
    layout.set_theme(app.theme);
    if app.terminal_focused {
        return Arc::new(app.desired_terminal_grid(layout));
    }
    let frame = app.active.editor.frame();
    layout.ensure_cursor_visible(&frame, 1);
    let frames = buffer_frames(app, frame);
    let prompt = prompt_text(app);
    let mut decorations = syntax_decorations(app);
    add_search_decorations(app, &mut decorations);
    let line_decorations = add_buffer_decorations(app, &mut decorations);
    add_selection_decoration(app, &mut decorations);
    let grid = layout.desired_workspace_grid_with_line_decorations(
        &app.views,
        &frames,
        &decorations,
        &line_decorations,
        " ",
        prompt.as_deref(),
    );
    prefetch_document_end(layout, app, &frames, &line_decorations);
    Arc::new(apply_editor_overlays(layout, app, grid, prompt.is_none()))
}

fn prefetch_document_end(
    layout: &mut ViewportLayout,
    app: &App,
    frames: &[(BufferId, wren_engine::EngineFrame)],
    line_decorations: &[(BufferId, Vec<LineDecoration>)],
) {
    if app.active.editor.revision().get() != 0
        || app.active.editor.mode() != Mode::Normal
        || app.views.windows.len() != 1
        || !app.active.class.policy().whole_document_syntax
        || language_bundle(app.active.document.presentation_path())
            .language_id
            .as_ref()
            == "markdown"
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
    let top_line = last_line
        .saturating_add(margin)
        .saturating_add(1)
        .saturating_sub(rows);
    let window = app.views.active_window();
    if window.top_line != 0 {
        return;
    }
    let start = text.byte_of_line(top_line);
    let syntax_end = text
        .byte_of_line(top_line.saturating_add(rows).saturating_add(1))
        .max(start);
    let range = start..syntax_end;
    let mut spans = app
        .decorations
        .get(&app.active.buffer_id)
        .filter(|state| state.revision == app.active.editor.revision())
        .map_or_else(Vec::new, |state| state.spans_in(range.clone()));
    if let Some(state) = app
        .semantic_decorations
        .get(&app.active.buffer_id)
        .filter(|state| state.revision == app.active.editor.revision())
    {
        spans.extend(state.spans_in(range));
    }
    if app.search_highlight {
        let search_end = text.byte_of_line(top_line.saturating_add(rows));
        spans.extend(
            app.active
                .editor
                .search_match_ranges(start..search_end, 4_096)
                .into_iter()
                .map(|range| DecorationSpan {
                    range,
                    priority: 3_000_000,
                    style: CellStyle {
                        foreground: Some(CellColor::Rgb(app.theme.crust)),
                        background: Some(CellColor::Rgb(app.theme.yellow)),
                        ..CellStyle::default()
                    },
                }),
        );
    }
    if let Some(path) = app.active.document.presentation_path() {
        let mut ignored_lines = Vec::new();
        add_diagnostic_decorations(app, &app.active, path, &mut spans, &mut ignored_lines);
    }
    let Some(frame) = frames
        .iter()
        .find_map(|(buffer_id, frame)| (*buffer_id == app.active.buffer_id).then_some(frame))
    else {
        return;
    };
    let frame = wren_engine::EngineFrame {
        text: frame.text.clone(),
        cursor_byte: app.active.editor.document_end_byte(),
    };
    let lines = line_decorations
        .iter()
        .find_map(|(buffer_id, lines)| {
            (*buffer_id == app.active.buffer_id).then_some(lines.as_slice())
        })
        .unwrap_or_default();
    layout.prefetch_workspace_viewport(window.id, &frame, top_line, &spans, lines);
}

#[cfg(test)]
#[derive(Default)]
struct DesiredFrameTimings {
    inputs: Duration,
    syntax: Duration,
    search: Duration,
    buffer_decorations: Duration,
    selection: Duration,
    render: Duration,
    overlays: Duration,
    total: Duration,
}

#[cfg(test)]
impl std::fmt::Display for DesiredFrameTimings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "total={:?} inputs={:?} syntax={:?} search={:?} buffer_decorations={:?} selection={:?} render={:?} overlays={:?}",
            self.total,
            self.inputs,
            self.syntax,
            self.search,
            self.buffer_decorations,
            self.selection,
            self.render,
            self.overlays,
        )
    }
}

#[cfg(test)]
fn desired_frame_profiled(
    layout: &mut ViewportLayout,
    app: &App,
) -> (Arc<DesiredGrid>, DesiredFrameTimings) {
    let total_at = Instant::now();
    let inputs_at = Instant::now();
    layout.set_theme(app.theme);
    let frame = app.active.editor.frame();
    layout.ensure_cursor_visible(&frame, 1);
    let frames = buffer_frames(app, frame);
    let prompt = prompt_text(app);
    let inputs = inputs_at.elapsed();

    let syntax_at = Instant::now();
    let mut decorations = syntax_decorations(app);
    let syntax = syntax_at.elapsed();
    let search_at = Instant::now();
    add_search_decorations(app, &mut decorations);
    let search = search_at.elapsed();
    let buffer_decorations_at = Instant::now();
    let line_decorations = add_buffer_decorations(app, &mut decorations);
    let buffer_decorations = buffer_decorations_at.elapsed();
    let selection_at = Instant::now();
    add_selection_decoration(app, &mut decorations);
    let selection = selection_at.elapsed();

    let render_at = Instant::now();
    let grid = layout.desired_workspace_grid_with_line_decorations(
        &app.views,
        &frames,
        &decorations,
        &line_decorations,
        " ",
        prompt.as_deref(),
    );
    let render = render_at.elapsed();
    let overlays_at = Instant::now();
    let grid = Arc::new(apply_editor_overlays(layout, app, grid, prompt.is_none()));
    let overlays = overlays_at.elapsed();
    (
        grid,
        DesiredFrameTimings {
            inputs,
            syntax,
            search,
            buffer_decorations,
            selection,
            render,
            overlays,
            total: total_at.elapsed(),
        },
    )
}

fn buffer_frames(
    app: &App,
    active: wren_engine::EngineFrame,
) -> Vec<(BufferId, wren_engine::EngineFrame)> {
    std::iter::once((app.active.buffer_id, active))
        .chain(
            app.inactive
                .iter()
                .map(|buffer| (buffer.buffer_id, buffer.editor.frame())),
        )
        .collect()
}

fn prompt_text(app: &App) -> Option<String> {
    app.prompt
        .as_ref()
        .filter(|prompt| !prompt.kind.is_picker())
        .map(|prompt| {
            let input = prompt.display();
            if app.message.is_empty() {
                input
            } else {
                format!("{input}  │  {}", app.message)
            }
        })
}

fn decoration_bucket<T>(
    decorations: &mut Vec<(BufferId, Vec<T>)>,
    buffer_id: BufferId,
) -> &mut Vec<T> {
    let index = decorations
        .iter()
        .position(|(candidate, _)| *candidate == buffer_id)
        .unwrap_or_else(|| {
            decorations.push((buffer_id, Vec::new()));
            decorations.len() - 1
        });
    &mut decorations[index].1
}

fn syntax_decorations(app: &App) -> Vec<(BufferId, Vec<DecorationSpan>)> {
    let mut decorations = app
        .decorations
        .iter()
        .filter_map(|(buffer_id, state)| {
            let buffer = app.buffer(*buffer_id)?;
            (buffer.editor.revision() == state.revision)
                .then(|| visible_byte_range(app, *buffer_id))
                .flatten()
                .map(|range| (*buffer_id, state.spans_in(range)))
        })
        .collect::<Vec<_>>();
    for (buffer_id, state) in &app.semantic_decorations {
        let Some(buffer) = app.buffer(*buffer_id) else {
            continue;
        };
        let Some(range) = (buffer.editor.revision() == state.revision)
            .then(|| visible_byte_range(app, *buffer_id))
            .flatten()
        else {
            continue;
        };
        decoration_bucket(&mut decorations, *buffer_id).extend(state.spans_in(range));
    }
    decorations
}

fn visible_byte_range(app: &App, buffer_id: BufferId) -> Option<Range<usize>> {
    let buffer = app.buffer(buffer_id)?;
    let viewport_rows = app.viewport_rows.max(1);
    app.views
        .windows
        .iter()
        .filter(|window| window.buffer_id == buffer_id)
        .map(|window| {
            let start = buffer.editor.text().byte_of_line(window.top_line);
            let end = buffer
                .editor
                .text()
                .byte_of_line(
                    window
                        .top_line
                        .saturating_add(viewport_rows)
                        .saturating_add(1),
                )
                .max(start);
            start..end
        })
        .reduce(|left, right| left.start.min(right.start)..left.end.max(right.end))
}

fn add_search_decorations(app: &App, decorations: &mut Vec<(BufferId, Vec<DecorationSpan>)>) {
    if !app.search_highlight {
        return;
    }
    let previewing = app.prompt.as_ref().is_some_and(|prompt| {
        matches!(
            prompt.kind,
            PromptKind::SearchForward | PromptKind::SearchBackward
        )
    });
    for window in &app.views.windows {
        if previewing && window.buffer_id != app.active.buffer_id {
            continue;
        }
        let Some(buffer) = app.buffer(window.buffer_id) else {
            continue;
        };
        let visible_start = buffer.editor.text().byte_of_line(window.top_line);
        let visible_end = buffer
            .editor
            .text()
            .byte_of_line(window.top_line.saturating_add(app.viewport_rows.max(1)));
        let matches = buffer
            .editor
            .search_match_ranges(visible_start..visible_end, 4_096);
        decoration_bucket(decorations, window.buffer_id).extend(matches.into_iter().map(|range| {
            let current = window.buffer_id == app.active.buffer_id
                && range.start == app.active.editor.primary_cursor();
            DecorationSpan {
                priority: 3_000_000,
                style: CellStyle {
                    foreground: Some(CellColor::Rgb(app.theme.crust)),
                    background: Some(CellColor::Rgb(if current {
                        app.theme.peach
                    } else {
                        app.theme.yellow
                    })),
                    ..CellStyle::default()
                },
                range,
            }
        }));
    }
}

fn add_git_decorations(
    buffer: &BufferState,
    theme: CatppuccinPalette,
    lines: &mut Vec<LineDecoration>,
) {
    lines.extend(buffer.git_hunks.iter().map(|hunk| {
        let line = if hunk.after.start == hunk.after.end {
            hunk.after.start.saturating_sub(1)
        } else {
            hunk.after.start
        } as usize;
        let color = if hunk.before.start == hunk.before.end {
            theme.green
        } else if hunk.after.start == hunk.after.end {
            theme.red
        } else {
            theme.yellow
        };
        LineDecoration {
            line,
            style: CellStyle {
                bold: true,
                foreground: Some(CellColor::Rgb(color)),
                ..CellStyle::default()
            },
        }
    }));
}

fn add_diagnostic_decorations(
    app: &App,
    buffer: &BufferState,
    path: &Path,
    spans: &mut Vec<DecorationSpan>,
    lines: &mut Vec<LineDecoration>,
) {
    for diagnostic in app
        .diagnostics
        .iter()
        .filter(|entry| same_path(&entry.path, path))
    {
        let start = buffer
            .editor
            .text()
            .byte_of_line(diagnostic.line.saturating_sub(1));
        let end = buffer
            .editor
            .text()
            .byte_of_line(diagnostic.line)
            .saturating_sub(1)
            .max(start.saturating_add(1))
            .min(buffer.editor.text().len_bytes());
        let color = match diagnostic.severity {
            DiagnosticSeverity::Error => app.theme.red,
            DiagnosticSeverity::Warning => app.theme.yellow,
            DiagnosticSeverity::Information => app.theme.blue,
            DiagnosticSeverity::Hint => app.theme.teal,
        };
        spans.push(DecorationSpan {
            range: start..end,
            priority: 2_000_000,
            style: CellStyle {
                underline: true,
                foreground: Some(CellColor::Rgb(color)),
                ..CellStyle::default()
            },
        });
        lines.push(LineDecoration {
            line: diagnostic.line.saturating_sub(1),
            style: CellStyle {
                bold: true,
                foreground: Some(CellColor::Rgb(color)),
                ..CellStyle::default()
            },
        });
    }
}

fn add_breakpoint_decorations(app: &App, path: &Path, lines: &mut Vec<LineDecoration>) {
    let Some(breakpoints) = app.breakpoints.get(path) else {
        return;
    };
    lines.extend(breakpoints.keys().map(|line| LineDecoration {
        line: line.saturating_sub(1),
        style: CellStyle {
            bold: true,
            foreground: Some(CellColor::Rgb(app.theme.red)),
            background: Some(CellColor::Rgb(app.theme.surface0)),
            ..CellStyle::default()
        },
    }));
}

fn add_buffer_decorations(
    app: &App,
    decorations: &mut Vec<(BufferId, Vec<DecorationSpan>)>,
) -> Vec<(BufferId, Vec<LineDecoration>)> {
    let mut line_decorations = Vec::new();
    for buffer in std::iter::once(&app.active).chain(app.inactive.iter()) {
        let Some(path) = buffer.document.presentation_path() else {
            continue;
        };
        let spans = decoration_bucket(decorations, buffer.buffer_id);
        let lines = decoration_bucket(&mut line_decorations, buffer.buffer_id);
        if language_bundle(Some(path)).language_id.as_ref() == "markdown" {
            spans.extend(markdown_decorations(&buffer.editor.contents(), app.theme));
        }
        add_git_decorations(buffer, app.theme, lines);
        add_diagnostic_decorations(app, buffer, path, spans, lines);
        add_breakpoint_decorations(app, path, lines);
    }
    line_decorations
}

fn add_selection_decoration(app: &App, decorations: &mut Vec<(BufferId, Vec<DecorationSpan>)>) {
    if !matches!(app.active.editor.mode(), Mode::Visual | Mode::VisualLine) {
        return;
    }
    let selection = app.active.editor.selection_byte_range();
    if selection.is_empty() {
        return;
    }
    decoration_bucket(decorations, app.active.buffer_id).push(DecorationSpan {
        range: selection,
        // Selection is appended after syntax and semantic decorations, so
        // tying their maximum priority lets it own the background while
        // retaining the highlighter's foreground and text attributes.
        priority: u32::MAX,
        style: CellStyle {
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            reverse: false,
            foreground: None,
            background: Some(CellColor::Rgb(app.theme.surface1)),
        },
    });
}

fn apply_editor_overlays(
    layout: &mut ViewportLayout,
    app: &App,
    grid: DesiredGrid,
    show_status: bool,
) -> DesiredGrid {
    let grid = if show_status {
        layout.apply_status_overlay(grid, &app.status_overlay())
    } else {
        grid
    };
    let grid = if let Some(overlay) = app.ace_jump_overlay() {
        layout.apply_ace_jump_overlay(grid, &app.views, &app.active.editor.frame(), &overlay)
    } else {
        grid
    };
    let grid = if app.debug_ui_visible {
        layout.apply_debug_overlay(grid, &app.debug_overlay())
    } else {
        grid
    };
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
                    cli.line = Some(
                        value[1..]
                            .parse::<usize>()
                            .with_context(|| format!("invalid line argument {value}"))?,
                    );
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
    SearchForward,
    SearchBackward,
    Expression,
    FilePicker,
    FileBrowser,
    Grep,
    Location,
    Rename,
    ConditionalBreakpoint,
    Ai,
}

impl PromptKind {
    const fn is_picker(self) -> bool {
        matches!(
            self,
            Self::FilePicker | Self::FileBrowser | Self::Grep | Self::Location
        )
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
        Self {
            kind,
            buffer: String::new(),
            history_index: None,
        }
    }

    fn display(&self) -> String {
        format!("{}{}", self.prefix(), self.buffer)
    }

    const fn prefix(&self) -> &'static str {
        match self.kind {
            PromptKind::Command => ":",
            PromptKind::SearchForward => "/",
            PromptKind::SearchBackward => "?",
            PromptKind::Expression => "=",
            PromptKind::FilePicker => "find> ",
            PromptKind::FileBrowser => "browse> ",
            PromptKind::Grep => "grep> ",
            PromptKind::Location => "jump> ",
            PromptKind::Rename => "rename> ",
            PromptKind::ConditionalBreakpoint => "break if> ",
            PromptKind::Ai => "ask Codex> ",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewPosition {
    Top,
    Middle,
    Bottom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AceJumpState {
    AwaitTarget,
    AwaitLabel {
        target: char,
        prefix: String,
        targets: Vec<AceJumpTarget>,
    },
}

#[cfg(not(test))]
#[derive(Debug, Default, Deserialize)]
struct ThemeConfig {
    flavor: Option<String>,
    #[serde(default)]
    colors: BTreeMap<String, String>,
}

#[cfg(test)]
fn load_theme() -> (CatppuccinFlavor, CatppuccinPalette, String) {
    (
        CatppuccinFlavor::Mocha,
        CatppuccinPalette::MOCHA,
        String::new(),
    )
}

#[cfg(not(test))]
fn load_theme() -> (CatppuccinFlavor, CatppuccinPalette, String) {
    let path = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|directory| directory.join("wren/theme.toml"));
    let mut message = String::new();
    let config = path
        .as_deref()
        .filter(|path| path.exists())
        .and_then(|path| match std::fs::read_to_string(path) {
            Ok(source) => match toml::from_str::<ThemeConfig>(&source) {
                Ok(config) => Some(config),
                Err(error) => {
                    message = format!("theme config {}: {error}", path.display());
                    None
                }
            },
            Err(error) => {
                message = format!("theme config {}: {error}", path.display());
                None
            }
        })
        .unwrap_or_default();
    let requested = env::var("WREN_CATPPUCCIN_FLAVOR")
        .ok()
        .or(config.flavor)
        .unwrap_or_else(|| "mocha".to_owned());
    let flavor = CatppuccinFlavor::parse(&requested).unwrap_or_else(|| {
        message = format!("unknown Catppuccin flavor {requested:?}; using mocha");
        CatppuccinFlavor::Mocha
    });
    let mut palette = CatppuccinPalette::for_flavor(flavor);
    for (name, value) in config.colors {
        let Some(color) = RgbColor::from_hex(&value) else {
            message = format!("invalid theme color {name}={value:?}");
            continue;
        };
        if !palette.set(&name, color) {
            message = format!("unknown theme color slot {name:?}");
        }
    }
    (flavor, palette, message)
}

#[derive(Debug, Clone)]
struct RuntimeBinding {
    invocation: CommandInvocation,
    when: Option<Box<str>>,
    description: Box<str>,
}

#[derive(Debug, Clone)]
struct RuntimeKeymap {
    leader: BTreeMap<Box<str>, RuntimeBinding>,
    groups: BTreeMap<Box<str>, Box<str>>,
}

impl RuntimeKeymap {
    fn defaults() -> Self {
        let mut leader = BTreeMap::new();
        for (sequence, command, description) in DEFAULT_LEADER_BINDINGS {
            leader.insert(
                Box::<str>::from(*sequence),
                RuntimeBinding {
                    invocation: CommandInvocation {
                        command: Box::<str>::from(*command),
                        arguments: BTreeMap::new(),
                    },
                    when: None,
                    description: Box::<str>::from(*description),
                },
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
        let parsed: wren_config::Config =
            toml::from_str(source).context("parse user configuration")?;
        let config = parse_and_validate(
            source,
            &registry,
            WorkspaceTrust::Trusted {
                executable_hash: executable_hash(&parsed),
            },
        )
        .map_err(|error| anyhow!(error))?;
        let descriptions: BTreeMap<&str, &str> = schemas
            .iter()
            .map(|schema| (schema.name.as_ref(), schema.description.as_ref()))
            .collect();
        if let Some(bindings) = config.keys.get("normal") {
            for (keys, binding) in bindings {
                let Some(sequence) = normalize_leader_sequence(keys) else {
                    continue;
                };
                let invocation = registry
                    .validate(&binding.command, &binding.args)
                    .map_err(|error| anyhow!(error))?;
                let description = descriptions
                    .get(binding.command.as_str())
                    .copied()
                    .unwrap_or(binding.command.as_str());
                self.leader.insert(
                    sequence.into_boxed_str(),
                    RuntimeBinding {
                        invocation,
                        when: binding.when.as_deref().map(Box::<str>::from),
                        description: description.into(),
                    },
                );
            }
        }
        Ok(())
    }
}

const DEFAULT_LEADER_BINDINGS: &[(&str, &str, &str)] = &[
    (" ", "selection.line", "select line"),
    ("q", "editor.quit", "quit"),
    ("w", "file.write", "write"),
    ("x", "search.clear", "clear search"),
    ("b", "picker.buffers", "buffers"),
    ("jj", "jump.ace", "visible character"),
    ("f", "format.document", "format"),
    ("ff", "picker.files", "files"),
    ("fb", "picker.browser", "file browser"),
    ("f.", "picker.resume", "resume picker"),
    ("fo", "picker.recent", "recent files"),
    ("fr", "picker.grep", "grep Git root"),
    ("fw", "picker.grep_word", "grep word"),
    ("fj", "picker.jumplist", "jumplist"),
    ("fd", "picker.diagnostics", "diagnostics"),
    ("e", "diagnostic.show", "diagnostic float"),
    ("ea", "repl.evaluate", "evaluate selection"),
    ("dt", "debug.toggle", "toggle UI"),
    ("db", "debug.breakpoint", "breakpoint"),
    (
        "dB",
        "debug.conditional_breakpoint",
        "conditional breakpoint",
    ),
    ("dl", "debug.repl", "REPL"),
    ("dc", "debug.continue", "continue"),
    ("ds", "debug.step_into", "step into"),
    ("dn", "debug.step_over", "step over"),
    ("do", "debug.step_out", "step out"),
    ("dr", "debug.restart", "restart"),
    ("gs", "git.stage_hunk", "stage hunk"),
    ("gr", "git.reset_hunk", "reset hunk"),
    ("gS", "git.stage_buffer", "stage buffer"),
    ("gu", "git.undo_stage", "undo stage"),
    ("gp", "git.preview_hunk", "preview hunk"),
    ("gb", "git.blame_line", "blame line"),
    ("gd", "git.diff_index", "diff index"),
    ("rn", "lsp.rename", "rename"),
    ("ca", "lsp.code_action", "code action"),
    ("D", "lsp.type_definition", "type definition"),
    ("wa", "workspace.add_folder", "add workspace folder"),
    ("wr", "workspace.remove_folder", "remove workspace folder"),
    ("wl", "workspace.list_folders", "list workspace folders"),
    ("hw", "haskell.hoogle", "Hoogle"),
    ("hs", "haskell.signature", "signature"),
    ("hl", "haskell.code_lens", "code lens"),
    ("rr", "haskell.repl_package", "package GHCi"),
    ("rf", "haskell.repl_file", "file GHCi"),
    ("rq", "haskell.repl_quit", "quit GHCi"),
];

fn native_command_schemas() -> Vec<CommandSchema> {
    DEFAULT_LEADER_BINDINGS
        .iter()
        .map(|(_, command, description)| CommandSchema {
            name: Box::<str>::from(*command),
            description: Box::<str>::from(*description),
            class: if command.starts_with("picker.")
                || command.starts_with("git.")
                || command.starts_with("lsp.")
                || command.starts_with("debug.")
                || command.starts_with("haskell.")
                || matches!(*command, "format.document" | "file.write" | "repl.evaluate")
            {
                CommandClass::Task
            } else {
                CommandClass::Realtime
            },
            arguments: Vec::new(),
        })
        .collect()
}

fn normalize_leader_sequence(keys: &str) -> Option<String> {
    let keys = keys.trim();
    let tail = keys
        .strip_prefix("space ")
        .or_else(|| keys.strip_prefix("<space>"))?;
    let sequence: String = tail
        .split_whitespace()
        .map(|token| if token == "space" { " " } else { token })
        .collect();
    (!sequence.is_empty()).then_some(sequence)
}

#[cfg(test)]
fn load_keymap() -> (RuntimeKeymap, String) {
    (RuntimeKeymap::defaults(), String::new())
}

#[cfg(not(test))]
fn load_keymap() -> (RuntimeKeymap, String) {
    let mut keymap = RuntimeKeymap::defaults();
    let path = env::var_os("WREN_CONFIG").map(PathBuf::from).or_else(|| {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|directory| directory.join("wren/config.toml"))
    });
    let Some(path) = path.filter(|path| path.exists()) else {
        return (keymap, String::new());
    };
    match std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))
        .and_then(|source| keymap.overlay_user_config(&source))
    {
        Ok(()) => (keymap, String::new()),
        Err(error) => (
            RuntimeKeymap::defaults(),
            format!("config {}: {error:#}", path.display()),
        ),
    }
}

struct BufferState {
    buffer_id: BufferId,
    document_id: DocumentId,
    editor: Editor<DefaultText>,
    document: LocalDocument,
    class: DocumentClass,
    mixed_line_endings: bool,
    wal: Option<WalWorker>,
    base_hash: [u8; 32],
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
    fn open(
        buffer_id: BufferId,
        document_id: DocumentId,
        path: Option<&Path>,
        line: Option<usize>,
    ) -> Result<(Self, String)> {
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
        let git_index_text = document.presentation_path().and_then(|path| {
            let root = git_root_for(path).ok()?;
            let relative = path.strip_prefix(&root).ok()?;
            git_index_contents(&root, relative).ok().map(Arc::from)
        });
        let git_branch = document
            .presentation_path()
            .and_then(git_branch_for)
            .map(String::into_boxed_str);
        let recovered = wal
            .as_ref()
            .map(LocalWal::recover_latest)
            .transpose()
            .context("read recovery WAL")?
            .flatten()
            .filter(|state| state.base_hash == base_hash && !opened.read_only);
        let (text, recovered_cursor, recovered_revision) = recovered
            .map(|state| (state.text, Some(state.cursor), Some(state.revision)))
            .unwrap_or((opened.text, None, None));
        let indent = detect_indent_style(&text);
        let initial_git_hunks = git_index_text
            .as_ref()
            .map_or_else(Vec::new, |index_text| git_hunks(index_text, &text));
        let store =
            DefaultText::from_reader(Cursor::new(text.as_bytes())).context("create text store")?;
        let mut editor = Editor::with_contents(store, text);
        editor.set_search_options(true, true);
        editor.set_clipboard_unnamed(true);
        editor.set_indent_options(indent.expand_tabs, indent.width, indent.width, true);
        editor.set_expand_region_keys(true);
        editor.set_read_only(opened.read_only);
        if let Some(cursor) = recovered_cursor {
            editor.set_cursor(cursor);
            editor.mark_dirty();
        } else if let Some(line) = line {
            editor.set_cursor(editor.text().byte_of_line(line.saturating_sub(1)));
        }
        if recovered_revision.is_none()
            && let Some(path) = document.presentation_path()
            && let Some(state) = load_undo_state(path, base_hash)?
        {
            editor.restore_undo_state(state)?;
        }
        let message = if let Some(revision) = recovered_revision {
            format!("recovered unsaved revision {revision}")
        } else if opened.read_only {
            format!("read-only {:?} byte view", opened.encoding)
        } else {
            String::new()
        };
        Ok((
            Self {
                buffer_id,
                document_id,
                editor,
                document,
                class: opened.class,
                mixed_line_endings: opened.mixed_line_endings,
                wal: wal.map(WalWorker::start),
                base_hash,
                git_index_text,
                git_hunks: initial_git_hunks,
                git_branch,
                display_name: None,
            },
            message,
        ))
    }

    fn virtual_buffer(
        buffer_id: BufferId,
        document_id: DocumentId,
        name: &str,
        text: String,
    ) -> Result<Self> {
        let (document, mut opened) = LocalDocument::unnamed();
        opened.text = text;
        opened.read_only = true;
        let (mut buffer, _) =
            Self::from_opened(buffer_id, document_id, document, opened, None, None)?;
        buffer.display_name = Some(name.into());
        Ok(buffer)
    }

    fn name(&self) -> String {
        if let Some(name) = &self.display_name {
            return name.to_string();
        }
        self.document
            .presentation_path()
            .map_or_else(|| "[No Name]".to_owned(), |path| path.display().to_string())
    }

    fn has_display_name(&self, name: &str) -> bool {
        self.display_name.as_deref() == Some(name)
    }

    fn refresh_git_hunks(&mut self) {
        self.git_hunks = self.git_index_text.as_ref().map_or_else(Vec::new, |index| {
            git_hunks(index, self.editor.frame().text.as_ref())
        });
    }
}

struct App {
    active: BufferState,
    inactive: Vec<BufferState>,
    views: ClientViewModel,
    quickfix: Vec<QuickfixEntry>,
    jump_history: Vec<JumpLocation>,
    jump_index: Option<usize>,
    mutations: MutationWorker,
    client_state: DurableClientState,
    client_state_worker: ClientStateWorker,
    provider: ProviderWorker,
    git_worker: GitHunkWorker,
    provider_submitted: BTreeMap<DocumentId, ProviderDemandKey>,
    provider_refresh_due: BTreeMap<DocumentId, Instant>,
    provider_refresh_ranges: BTreeMap<DocumentId, Vec<Range<usize>>>,
    decorations: BTreeMap<BufferId, BufferDecorations>,
    semantic_decorations: BTreeMap<BufferId, BufferDecorations>,
    prompt: Option<Prompt>,
    search_prompt_origin: Option<SearchPromptOrigin>,
    last_search_direction: SearchDirection,
    search_highlight: bool,
    last_substitute: Option<LastSubstitute>,
    substitute_confirmation: Option<SubstituteConfirmation>,
    message: String,
    tasks: TaskRunner,
    active_task: Option<CancellationToken>,
    next_task_id: u64,
    terminal: Option<PtySession>,
    terminal_focused: bool,
    terminal_escape_pending: bool,
    mouse_selection: Option<MouseSelection>,
    picker_files: Vec<String>,
    picker_matches: Vec<PathBuf>,
    picker_index: usize,
    picker_directory: Option<PathBuf>,
    picker_preview_title: String,
    picker_preview: String,
    picker_preview_scroll: usize,
    picker_preview_highlight_line: Option<usize>,
    picker_preview_decorations: Vec<DecorationSpan>,
    popup: Option<TextPopup>,
    popup_deadline: Option<Instant>,
    ace_jump: Option<AceJumpState>,
    completion: Option<CompletionSession>,
    completion_index: usize,
    completion_selected: bool,
    completion_documentation_scroll: usize,
    snippet_stops: Vec<Range<usize>>,
    snippet_stop_index: usize,
    lsp_completion: Option<LspCompletion>,
    lsp: Option<PersistentLsp>,
    parked_lsps: Vec<PersistentLsp>,
    lsp_start_due: Option<Instant>,
    lsp_start: Option<mpsc::Receiver<Result<PersistentLsp, String>>>,
    lsp_background: Option<mpsc::Receiver<LspBackgroundResult>>,
    lsp_semantic_dirty: bool,
    pending_lsp_hover: Option<String>,
    pending_lsp_location: Option<String>,
    leader_keys: Option<String>,
    leader_deadline: Option<Instant>,
    keymap: RuntimeKeymap,
    normal_prefix: Option<char>,
    last_picker_query: String,
    last_picker_source: Option<PickerSource>,
    recent_files: Vec<PathBuf>,
    diagnostics: Vec<DiagnosticEntry>,
    format_on_save: bool,
    format_disabled: BTreeSet<DocumentId>,
    breakpoints: BTreeMap<PathBuf, BTreeMap<usize, Option<String>>>,
    root_workspace: PathBuf,
    workspace_folders: Vec<PathBuf>,
    debug_ui_visible: bool,
    ai_transcript: String,
    active_ai_task: Option<CommandTaskId>,
    last_staged_patch: Option<Vec<u8>>,
    theme_flavor: CatppuccinFlavor,
    theme: CatppuccinPalette,
    viewport_rows: usize,
    viewport_columns: usize,
    quit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BufferDecorations {
    revision: DocumentRevision,
    /// Provider-owned baseline spans in the coordinate space before
    /// `transforms`. Ordinary edits update the small visible override set,
    /// rather than rewriting every span in a large file.
    spans: Vec<DecorationSpan>,
    prefix_max_end: Vec<usize>,
    transforms: Vec<Transaction>,
    overrides: Vec<DecorationSpan>,
    invalidated: Vec<Range<usize>>,
}

impl BufferDecorations {
    fn new(revision: DocumentRevision, mut spans: Vec<DecorationSpan>) -> Self {
        spans.sort_by_key(|span| (span.range.start, std::cmp::Reverse(span.range.end)));
        spans.dedup();
        let mut decorations = Self {
            revision,
            spans,
            prefix_max_end: Vec::new(),
            transforms: Vec::new(),
            overrides: Vec::new(),
            invalidated: Vec::new(),
        };
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
        self.advance_current_coordinates(transaction);
        self.invalidated
            .extend(transaction_current_edit_ranges(transaction));
        merge_ranges(&mut self.invalidated);
        self.revision = revision;
    }

    fn replace_after_transaction(
        &mut self,
        transaction: &Transaction,
        revision: DocumentRevision,
        ranges: &[Range<usize>],
        mut replacement: Vec<DecorationSpan>,
    ) {
        self.advance_current_coordinates(transaction);
        self.overrides.retain(|span| {
            !ranges
                .iter()
                .any(|range| ranges_overlap(&span.range, range))
        });
        replacement.sort_by(decoration_order);
        replacement.dedup();
        self.overrides = merge_sorted_decorations(
            std::mem::take(&mut self.overrides).into_iter(),
            replacement.into_iter(),
        );
        self.invalidated.extend(ranges.iter().cloned());
        merge_ranges(&mut self.invalidated);
        self.revision = revision;
    }

    fn advance_current_coordinates(&mut self, transaction: &Transaction) {
        self.overrides = std::mem::take(&mut self.overrides)
            .into_iter()
            .filter_map(|span| map_decoration_span(span, transaction))
            .collect();
        self.invalidated = std::mem::take(&mut self.invalidated)
            .into_iter()
            .filter_map(|range| map_range(range, transaction))
            .collect();
        let cancels_previous = self
            .transforms
            .last()
            .and_then(|previous| previous.then(transaction).ok())
            .is_some_and(|composed| composed.edits.is_empty());
        if cancels_previous {
            self.transforms.pop();
        } else {
            self.transforms.push(transaction.clone());
        }
    }

    fn replace_current_ranges(
        &mut self,
        ranges: &[Range<usize>],
        mut replacement: Vec<DecorationSpan>,
    ) {
        self.overrides.retain(|span| {
            !ranges
                .iter()
                .any(|range| ranges_overlap(&span.range, range))
        });
        replacement.sort_by(decoration_order);
        replacement.dedup();
        self.overrides = merge_sorted_decorations(
            std::mem::take(&mut self.overrides).into_iter(),
            replacement.into_iter(),
        );
        self.invalidated.extend(ranges.iter().cloned());
        merge_ranges(&mut self.invalidated);
    }

    fn spans_in(&self, range: Range<usize>) -> Vec<DecorationSpan> {
        let base_range = self
            .transforms
            .iter()
            .rev()
            .fold(range.clone(), unmap_range);
        let first = self
            .prefix_max_end
            .partition_point(|maximum_end| *maximum_end <= base_range.start);
        let last = self
            .spans
            .partition_point(|span| span.range.start < base_range.end);
        let mut spans = self.spans[first..last]
            .iter()
            .cloned()
            .filter_map(|span| {
                let span = self.transforms.iter().try_fold(span, |span, transaction| {
                    map_decoration_span(span, transaction)
                })?;
                (ranges_overlap(&span.range, &range)
                    && !self
                        .invalidated
                        .iter()
                        .any(|invalidated| ranges_overlap(&span.range, invalidated)))
                .then_some(span)
            })
            .collect::<Vec<_>>();
        spans.extend(
            self.overrides
                .iter()
                .filter(|span| ranges_overlap(&span.range, &range))
                .cloned(),
        );
        spans.sort_by(decoration_order);
        spans.dedup();
        spans
    }
}

fn map_decoration_span(span: DecorationSpan, transaction: &Transaction) -> Option<DecorationSpan> {
    let start = transaction.map_offset(span.range.start, Bias::Left).ok()?;
    let end = transaction.map_offset(span.range.end, Bias::Right).ok()?;
    (start < end).then_some(DecorationSpan {
        range: start..end,
        style: span.style,
        priority: span.priority,
    })
}

fn map_range(range: Range<usize>, transaction: &Transaction) -> Option<Range<usize>> {
    let start = transaction.map_offset(range.start, Bias::Left).ok()?;
    let end = transaction.map_offset(range.end, Bias::Right).ok()?;
    (start < end).then_some(start..end)
}

fn transaction_current_edit_ranges(transaction: &Transaction) -> Vec<Range<usize>> {
    transaction
        .edits
        .iter()
        .filter_map(|edit| {
            let start = transaction.map_offset(edit.range.start, Bias::Left).ok()?;
            let end = transaction.map_offset(edit.range.end, Bias::Right).ok()?;
            (start < end).then_some(start..end)
        })
        .collect()
}

fn unmap_range(range: Range<usize>, transaction: &Transaction) -> Range<usize> {
    let start = unmap_offset(transaction, range.start, Bias::Left);
    let end = unmap_offset(transaction, range.end, Bias::Right);
    start.min(end)..end.max(start)
}

fn unmap_offset(transaction: &Transaction, offset: usize, bias: Bias) -> usize {
    let mut old_cursor = 0_usize;
    let mut new_cursor = 0_usize;
    for edit in &transaction.edits {
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

fn merge_ranges(ranges: &mut Vec<Range<usize>>) {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged = Vec::<Range<usize>>::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if let Some(previous) = merged.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    *ranges = merged;
}

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn decoration_order(left: &DecorationSpan, right: &DecorationSpan) -> std::cmp::Ordering {
    (left.range.start, std::cmp::Reverse(left.range.end))
        .cmp(&(right.range.start, std::cmp::Reverse(right.range.end)))
}

fn merge_sorted_decorations(
    left: impl Iterator<Item = DecorationSpan>,
    right: impl Iterator<Item = DecorationSpan>,
) -> Vec<DecorationSpan> {
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
    global: bool,
    case_override: CaseOverride,
    confirm: bool,
    print: bool,
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
    column_utf16: bool,
    text: String,
}

impl QuickfixEntry {
    fn display(&self) -> String {
        format!(
            "{}:{}:{}  {}",
            self.path.display(),
            self.line,
            self.column,
            compact(&self.text, 80)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiagnosticEntry {
    path: PathBuf,
    line: usize,
    column: usize,
    severity: DiagnosticSeverity,
    message: String,
}

impl DiagnosticEntry {
    fn quickfix(&self) -> QuickfixEntry {
        QuickfixEntry {
            path: self.path.clone(),
            line: self.line,
            column: self.column,
            column_utf16: false,
            text: format!("{}: {}", self.severity.label(), self.message),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

impl DiagnosticSeverity {
    const fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "info",
            Self::Hint => "hint",
        }
    }
}

fn is_navigation_key(key: TerminalKey) -> bool {
    if key.control || key.alt || key.super_key {
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
    if key.super_key {
        return None;
    }
    let mut bytes = Vec::new();
    if key.alt {
        bytes.push(0x1b);
    }
    match key.code {
        TerminalKeyCode::Char(character) if key.control && character.is_ascii() => {
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
        format!(
            "\u{1b}[<{button};{};{}{}",
            column.saturating_add(1),
            row.saturating_add(1),
            if release { 'm' } else { 'M' }
        )
    };
    match event {
        TerminalInput::MouseClick { column, row } => encode(0, *column, *row, false).into_bytes(),
        TerminalInput::MouseDrag { column, row } => encode(32, *column, *row, false).into_bytes(),
        TerminalInput::MouseRelease { column, row } => encode(0, *column, *row, true).into_bytes(),
        TerminalInput::MouseScroll { lines, column, row } => {
            let button = if *lines < 0 { 64 } else { 65 };
            let events = lines.unsigned_abs().div_ceil(3).max(1);
            encode(button, *column, *row, false)
                .repeat(events)
                .into_bytes()
        }
        _ => Vec::new(),
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
    let is_substitute = |command: &ExCommand| {
        matches!(
            command,
            ExCommand::Substitute { .. } | ExCommand::SubstituteRepeat { .. }
        )
    };
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
        before
            .chars()
            .all(|value| value.is_ascii_digit() || matches!(value, '%' | ',' | ';' | '.' | '$'))
            .then_some(index)
    })?;
    let tail = &input[command_byte + 1..];
    let delimiter = tail
        .chars()
        .next()
        .filter(|value| !value.is_alphanumeric() && !value.is_whitespace())?;
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
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '~' {
            return true;
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
            line += slice[scan_cursor..found.start()]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count();
            scan_cursor = found.start();
            if global || replaced_line != Some(line) {
                edits.push(Edit::new(
                    start + found.start()..start + found.end(),
                    replacement.expand(&captures),
                ));
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

fn substitution_message(
    count: usize,
    print: bool,
    original: &str,
    transaction: &Transaction,
) -> String {
    let summary = format!("{count} substitution(s)");
    if !print {
        return summary;
    }
    let Ok(changed) = transaction.apply_to_string(original) else {
        return summary;
    };
    let Some(last) = transaction.edits.last() else {
        return summary;
    };
    let anchor = transaction
        .map_offset(last.range.start, Bias::Left)
        .unwrap_or(last.range.start)
        .min(changed.len());
    let start = changed[..anchor].rfind('\n').map_or(0, |byte| byte + 1);
    let end = changed[anchor..]
        .find('\n')
        .map_or(changed.len(), |byte| anchor + byte);
    format!("{summary} │ {}", compact(&changed[start..end], 120))
}

fn ex_normal_keys(input: &str) -> Vec<KeyEvent> {
    let mut keys = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("<Esc>") {
            keys.push(KeyEvent::plain(KeyCode::Escape));
            rest = after;
        } else if let Some(after) = rest.strip_prefix("<CR>") {
            keys.push(KeyEvent::plain(KeyCode::Enter));
            rest = after;
        } else if let Some(after) = rest.strip_prefix("<Tab>") {
            keys.push(KeyEvent::plain(KeyCode::Tab));
            rest = after;
        } else if let Some(character) = rest.chars().next() {
            keys.push(KeyEvent::character(character));
            rest = &rest[character.len_utf8()..];
        }
    }
    keys
}

fn parse_vimgrep_line(line: &str) -> Option<QuickfixEntry> {
    let mut fields = line.splitn(4, ':');
    Some(QuickfixEntry {
        path: fields.next()?.into(),
        line: fields.next()?.parse().ok()?,
        column: fields.next()?.parse().ok()?,
        column_utf16: false,
        text: fields.next().unwrap_or_default().to_owned(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JumpLocation {
    document_id: DocumentId,
    path: PathBuf,
    byte: usize,
}

fn stable_document_id(path: Option<&Path>) -> DocumentId {
    let Some(path) = path else {
        return DocumentId::new(1);
    };
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in canonical.to_string_lossy().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    DocumentId::new(hash.max(2))
}

fn virtual_document_id(name: &str, text: &str) -> DocumentId {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in b"wren:virtual-buffer\0"
        .iter()
        .copied()
        .chain(name.bytes())
        .chain(std::iter::once(0))
        .chain(text.bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    DocumentId::new(hash.max(2))
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
    let expanded = if let Some(rest) = fragment.strip_prefix("~/") {
        env::var_os("HOME").map(|home| PathBuf::from(home).join(rest))?
    } else {
        PathBuf::from(fragment)
    };
    let directory = expanded.parent().unwrap_or_else(|| Path::new("."));
    let prefix = expanded
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    let mut matches = std::fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
        .collect::<Vec<_>>();
    matches.sort_by_key(|entry| entry.file_name());
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

fn system_clipboard_text(register: char) -> Option<String> {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbpaste", &[])]
    } else if register == '*' {
        &[
            ("wl-paste", &["--primary", "--no-newline"]),
            ("xclip", &["-selection", "primary", "-o"]),
        ]
    } else {
        &[
            ("wl-paste", &["--no-newline"]),
            ("xclip", &["-selection", "clipboard", "-o"]),
        ]
    };
    candidates.iter().find_map(|(program, arguments)| {
        if !executable_exists(program) {
            return None;
        }
        Command::new(program)
            .args(*arguments)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
    })
}

fn save_buffer(buffer: &mut BufferState) -> Result<()> {
    if let Some(wal) = &buffer.wal {
        wal.barrier()
            .context("make recovery WAL durable before save")?;
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

fn grammar_key(key: TerminalKey) -> Option<KeyEvent> {
    let code = match key.code {
        TerminalKeyCode::Char(character) => KeyCode::Char(character),
        TerminalKeyCode::Escape => KeyCode::Escape,
        TerminalKeyCode::Enter => KeyCode::Enter,
        TerminalKeyCode::Tab => KeyCode::Tab,
        TerminalKeyCode::Backspace => KeyCode::Backspace,
        TerminalKeyCode::Delete => KeyCode::Delete,
        TerminalKeyCode::Home => KeyCode::Home,
        TerminalKeyCode::End => KeyCode::End,
        TerminalKeyCode::PageUp | TerminalKeyCode::Up => KeyCode::Up,
        TerminalKeyCode::PageDown | TerminalKeyCode::Down => KeyCode::Down,
        TerminalKeyCode::Left => KeyCode::Left,
        TerminalKeyCode::Right => KeyCode::Right,
        TerminalKeyCode::Insert => return None,
    };
    let mut modifiers = Modifiers::empty();
    if key.shift {
        modifiers |= Modifiers::SHIFT;
    }
    if key.control {
        modifiers |= Modifiers::CONTROL;
    }
    if key.alt {
        modifiers |= Modifiers::ALT;
    }
    if key.super_key {
        modifiers |= Modifiers::SUPER;
    }
    Some(KeyEvent { code, modifiers })
}

fn format_key_sequence(sequence: &[KeyEvent]) -> String {
    sequence.iter().map(format_key_event).collect()
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
    let mut modifiers = Vec::with_capacity(4);
    if key.modifiers.contains(Modifiers::CONTROL) {
        modifiers.push("C");
    }
    if key.modifiers.contains(Modifiers::SHIFT) {
        modifiers.push("S");
    }
    if key.modifiers.contains(Modifiers::ALT) {
        modifiers.push("A");
    }
    if key.modifiers.contains(Modifiers::SUPER) {
        modifiers.push("D");
    }
    format!("<{}-{base}>", modifiers.join("-"))
}

fn register_snapshot(editor: &Editor<DefaultText>) -> BTreeMap<char, (Box<str>, bool)> {
    editor
        .registers()
        .map(|(name, value)| (name, (value.text.clone(), value.linewise)))
        .collect()
}

fn mark_snapshot(editor: &Editor<DefaultText>) -> BTreeMap<char, usize> {
    editor.marks().collect()
}

fn macro_snapshot(editor: &Editor<DefaultText>) -> BTreeMap<char, Vec<KeyEvent>> {
    editor
        .macros()
        .map(|(name, keys)| (name, keys.to_vec()))
        .collect()
}

fn changed_macros(
    before: &BTreeMap<char, Vec<KeyEvent>>,
    editor: &Editor<DefaultText>,
) -> Vec<StateDelta> {
    editor
        .macros()
        .filter(|(name, keys)| before.get(name).is_none_or(|old| old.as_slice() != *keys))
        .filter_map(|(name, keys)| {
            let raw_keys = serde_json::to_vec(keys).ok()?;
            let lowered_ir = serde_json::to_vec(
                &keys
                    .iter()
                    .map(|key| format!("{:?}:{:?}", key.modifiers, key.code))
                    .collect::<Vec<_>>(),
            )
            .ok()?;
            Some(StateDelta::MacroRecording {
                name,
                raw_keys,
                lowered_ir,
            })
        })
        .collect()
}

fn changed_registers(
    before: &BTreeMap<char, (Box<str>, bool)>,
    editor: &Editor<DefaultText>,
) -> Vec<StateDelta> {
    editor
        .registers()
        .filter(|(name, value)| before.get(name) != Some(&(value.text.clone(), value.linewise)))
        .map(|(name, value)| StateDelta::Register {
            name,
            text: value.text.clone(),
            linewise: value.linewise,
        })
        .collect()
}

fn changed_global_marks(
    before: &BTreeMap<char, usize>,
    editor: &Editor<DefaultText>,
    document_id: DocumentId,
) -> Vec<StateDelta> {
    editor
        .marks()
        .filter(|(name, byte)| name.is_ascii_uppercase() && before.get(name) != Some(byte))
        .map(|(name, byte)| StateDelta::GlobalMark {
            name,
            document_id,
            anchor: Anchor {
                byte,
                bias: Bias::Right,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests;
