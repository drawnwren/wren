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
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use wren_client_state::{ClientViewStateStore, DurableClientState};
use wren_command::{CancellationToken, TaskFailure, TaskRunner};
use wren_config::{CommandRegistry, WorkspaceTrust, executable_hash, parse_and_validate};
use wren_engine::{DurableUndoState, Editor, EngineError, Mode, SearchDirection};
use wren_grammar::{
    BufferAction, ExAddress, ExCommand, ExRange, ExpressionContext, KeyCode, KeyEvent, Modifiers,
    ParseState, TabAction, Value, evaluate_expression, parse_ex,
};
use wren_presenter::Presenter;
#[cfg(test)]
use wren_provider::ProviderActor;
#[cfg(not(test))]
use wren_provider::ProviderSupervisor;
use wren_provider::{
    CompletionCandidate, CompletionSession, HighlightSpan, ProviderRequest, ProviderResponse,
    fuzzy_rank, lexical_highlight_text,
};
use wren_session::{
    DocumentEncoding, LocalDocument, LocalWal, MutationOutbox, MutationSubmission, OpenedDocument,
    RecoveredState, SaveWarning, SessionAuthority, SessionJournal,
};
use wren_term::{
    SystemTerminalBackend, TerminaBackend, TerminalInput, TerminalKey, TerminalKeyCode,
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
    AceJumpOverlay, AceJumpTarget, CatppuccinFlavor, CatppuccinPalette, CellColor, CellStyle,
    ClientViewModel, CompletionOverlay, CompletionOverlayRow, DebugOverlay, DecorationSpan,
    DesiredGrid, LineDecoration, MessageEntry, MessageSeverity, PickerOverlay, PickerOverlayRow,
    RgbColor, SplitAxis, StatusOverlay, StatusSegment, TextPopup, ViewportLayout, WindowDirection,
};
use wren_workflow::{
    DocumentVisibility, LspClient, LspPosition, LspTextEdit, PtySession, SavePolicy,
    TaskSpec as WorkflowTaskSpec, TaskSupervisor, WorkflowError, git_hunks, lower_lsp_text_edits,
    run_formatter_until_cancelled,
};

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
    :s/old/new/g   :%s/old/new/g       literal substitution
    :undo   :redo   :normal KEYS       editing commands
    :registers   :marks                inspect durable state
    :messages / :debuglog               open message history buffer
    :find [QUERY]                       fuzzy file picker
    :terminal [PROGRAM]                 interactive terminal buffer
    :make PROGRAM [ARGS]                cancellable build task
    :format PROGRAM [ARGS]              revision-safe formatter
    :Git [ARGS]  :Gwrite  :Gdiffsplit   Fugitive-style Git commands
    :Codex / :AvanteChat / :AvanteAsk   Codex assistant float

Unsaved edits use a checksummed recovery WAL; undo history and oldfiles persist.
";

const MESSAGES_BUFFER_NAME: &str = "[Messages]";

fn main() -> Result<()> {
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
            match input {
                TerminalInput::Resized { columns, rows } => {
                    layout.resize(columns, rows);
                    app.resize_terminal(rows, columns);
                    presenter
                        .backend()
                        .lock()
                        .map_err(|_| anyhow!("presenter backend lock is poisoned"))?
                        .resize(columns, rows);
                    needs_render = true;
                }
                event => {
                    let should_render = input_requires_render(&event);
                    if should_render {
                        let clipboard_before = app
                            .active
                            .editor
                            .register('+')
                            .map(|value| value.text.clone());
                        let result = match event {
                            TerminalInput::MouseClick { column, row } => {
                                app.handle_mouse_click(&layout, column, row)
                            }
                            event => app.handle_input(event),
                        };
                        if let Err(error) = result {
                            app.show_error(error);
                        }
                        app.capture_debug_output();
                        needs_render = true;
                        let clipboard_after = app
                            .active
                            .editor
                            .register('+')
                            .map(|value| value.text.clone());
                        if clipboard_after != clipboard_before
                            && let Some(text) = clipboard_after
                            && let Err(error) = output
                                .lock()
                                .map_err(|_| anyhow!("presenter backend lock is poisoned"))?
                                .copy_osc52(&text)
                        {
                            app.show_error(format!("clipboard: {error}"));
                        }
                    }
                }
            }
        }
        // Quitting is a local editor action. Once accepted, leave the event
        // loop before polling providers or language servers so their state can
        // never delay or turn a successful quit into an error exit.
        if app.quit {
            break;
        }
        if app.poll_task_results()? {
            needs_render = true;
        }
        if app.poll_provider_results() {
            needs_render = true;
        }
        if app.poll_lsp_start()? {
            needs_render = true;
        }
        if app.poll_lsp_background()? {
            needs_render = true;
        }
        if app.poll_lsp_semantic_due()? {
            needs_render = true;
        }
        if app.poll_terminal()? {
            needs_render = true;
        }
        if app.poll_mapping_timeout()? {
            needs_render = true;
        }
        if app.poll_popup_timeout() {
            needs_render = true;
        }
        app.capture_debug_output();
        presenter.check_failure()?;
        if !app.quit && needs_render {
            app.schedule_provider_refreshes(layout.height);
            presenter.publish(desired_frame(&mut layout, &app))?;
        }
    }
    app.flush_wal()?;
    let _stats = presenter.finish()?;
    Ok(())
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
        let frame = app.terminal_frame();
        let grid = layout.desired_editor_grid(&frame, &app.terminal_status(), None);
        return Arc::new(grid);
    }
    let frame = app.active.editor.frame();
    layout.ensure_cursor_visible(&frame, 1);
    let mut frames = Vec::with_capacity(app.inactive.len() + 1);
    frames.push((app.active.buffer_id, frame));
    frames.extend(
        app.inactive
            .iter()
            .map(|buffer| (buffer.buffer_id, buffer.editor.frame())),
    );
    let prompt = app
        .prompt
        .as_ref()
        .filter(|prompt| !prompt.kind.is_picker())
        .map(|prompt| {
            let input = prompt.display();
            if app.message.is_empty() {
                input
            } else {
                format!("{input}  │  {}", app.message)
            }
        });
    let status = app.status();
    let mut decorations = app
        .decorations
        .iter()
        .filter(|(buffer_id, state)| {
            app.buffer(**buffer_id)
                .is_some_and(|buffer| buffer.editor.revision() == state.revision)
        })
        .map(|(buffer_id, state)| (*buffer_id, state.spans.clone()))
        .collect::<Vec<_>>();
    for (buffer_id, state) in &app.semantic_decorations {
        if app
            .buffer(*buffer_id)
            .is_some_and(|buffer| buffer.editor.revision() == state.revision)
        {
            if let Some((_, spans)) = decorations
                .iter_mut()
                .find(|(candidate, _)| candidate == buffer_id)
            {
                spans.extend(state.spans.iter().cloned());
            } else {
                decorations.push((*buffer_id, state.spans.clone()));
            }
        }
    }
    if app.search_highlight {
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
            if matches.is_empty() {
                continue;
            }
            let decoration_index = decorations
                .iter()
                .position(|(buffer_id, _)| *buffer_id == window.buffer_id)
                .unwrap_or_else(|| {
                    decorations.push((window.buffer_id, Vec::new()));
                    decorations.len() - 1
                });
            decorations[decoration_index]
                .1
                .extend(matches.into_iter().map(|range| DecorationSpan {
                    style: CellStyle {
                        foreground: Some(CellColor::Rgb(app.theme.crust)),
                        background: Some(CellColor::Rgb(
                            if window.buffer_id == app.active.buffer_id
                                && range.start == app.active.editor.primary_cursor()
                            {
                                app.theme.peach
                            } else {
                                app.theme.yellow
                            },
                        )),
                        ..CellStyle::default()
                    },
                    range,
                }));
        }
    }
    let mut line_decorations = Vec::<(BufferId, Vec<LineDecoration>)>::new();
    for buffer in std::iter::once(&app.active).chain(app.inactive.iter()) {
        let Some(path) = buffer.document.presentation_path() else {
            continue;
        };
        let decoration_index = decorations
            .iter()
            .position(|(buffer_id, _)| *buffer_id == buffer.buffer_id)
            .unwrap_or_else(|| {
                decorations.push((buffer.buffer_id, Vec::new()));
                decorations.len() - 1
            });
        let spans = &mut decorations[decoration_index].1;
        let line_decoration_index = line_decorations
            .iter()
            .position(|(buffer_id, _)| *buffer_id == buffer.buffer_id)
            .unwrap_or_else(|| {
                line_decorations.push((buffer.buffer_id, Vec::new()));
                line_decorations.len() - 1
            });
        let gutter_spans = &mut line_decorations[line_decoration_index].1;
        if language_bundle(Some(path)).language_id.as_ref() == "markdown" {
            spans.extend(markdown_decorations(&buffer.editor.contents(), app.theme));
        }
        if let Some(index_text) = &buffer.git_index_text {
            let contents = buffer.editor.contents();
            for hunk in git_hunks(index_text, &contents) {
                let line = if hunk.after.start == hunk.after.end {
                    hunk.after.start.saturating_sub(1)
                } else {
                    hunk.after.start
                } as usize;
                gutter_spans.push(LineDecoration {
                    line,
                    style: CellStyle {
                        bold: true,
                        foreground: Some(CellColor::Rgb(if hunk.before.start == hunk.before.end {
                            app.theme.green
                        } else if hunk.after.start == hunk.after.end {
                            app.theme.red
                        } else {
                            app.theme.yellow
                        })),
                        ..CellStyle::default()
                    },
                });
            }
        }
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
            let severity_color = match diagnostic.severity {
                DiagnosticSeverity::Error => app.theme.red,
                DiagnosticSeverity::Warning => app.theme.yellow,
                DiagnosticSeverity::Information => app.theme.blue,
                DiagnosticSeverity::Hint => app.theme.teal,
            };
            spans.push(DecorationSpan {
                range: start..end,
                style: CellStyle {
                    underline: true,
                    foreground: Some(CellColor::Rgb(severity_color)),
                    ..CellStyle::default()
                },
            });
            gutter_spans.push(LineDecoration {
                line: diagnostic.line.saturating_sub(1),
                style: CellStyle {
                    bold: true,
                    foreground: Some(CellColor::Rgb(severity_color)),
                    ..CellStyle::default()
                },
            });
        }
        if let Some(lines) = app.breakpoints.get(path) {
            for line in lines.keys() {
                gutter_spans.push(LineDecoration {
                    line: line.saturating_sub(1),
                    style: CellStyle {
                        bold: true,
                        foreground: Some(CellColor::Rgb(app.theme.red)),
                        background: Some(CellColor::Rgb(app.theme.surface0)),
                        ..CellStyle::default()
                    },
                });
            }
        }
    }
    let grid = layout.desired_workspace_grid_with_line_decorations(
        &app.views,
        &frames,
        &decorations,
        &line_decorations,
        &status,
        prompt.as_deref(),
    );
    let grid = if prompt.is_none() {
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
    let grid = if let Some(picker) = app.picker_overlay() {
        layout.apply_picker_overlay(grid, &picker)
    } else if let Some(completion) = app.completion_overlay() {
        layout.apply_completion_overlay(grid, &completion)
    } else if let Some(popup) = &app.popup {
        layout.apply_text_popup(grid, popup)
    } else {
        grid
    };
    Arc::new(grid)
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
    git_index_text: Option<String>,
    git_branch: Option<Box<str>>,
    display_name: Option<Box<str>>,
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
            git_index_contents(&root, relative).ok()
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
        let store = DefaultText::from_reader(Cursor::new(text)).context("create text store")?;
        let mut editor = Editor::new(store);
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
    provider_submitted: BTreeMap<DocumentId, ProviderDemandKey>,
    decorations: BTreeMap<BufferId, BufferDecorations>,
    semantic_decorations: BTreeMap<BufferId, BufferDecorations>,
    prompt: Option<Prompt>,
    search_prompt_origin: Option<SearchPromptOrigin>,
    search_highlight: bool,
    message: String,
    tasks: TaskRunner,
    active_task: Option<CancellationToken>,
    next_task_id: u64,
    terminal: Option<PtySession>,
    terminal_focused: bool,
    terminal_escape_pending: bool,
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
    quit: bool,
}

impl App {
    fn open(path: Option<&Path>, line: Option<usize>) -> Result<Self> {
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

    fn from_opened(
        document: LocalDocument,
        opened: OpenedDocument,
        line: Option<usize>,
        wal: Option<LocalWal>,
    ) -> Result<Self> {
        let buffer_id = BufferId::new(1);
        let document_id = stable_document_id(document.presentation_path());
        let (mut active, mut message) =
            BufferState::from_opened(buffer_id, document_id, document, opened, line, wal)?;
        let (client_state_worker, client_state) = ClientStateWorker::open(ClientId::new(1))?;
        if let Err(error) = restore_client_state(&mut active, &client_state) {
            if !message.is_empty() {
                message.push_str("; ");
            }
            message.push_str(&format!("client state: {error}"));
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
        mutations.register(document_id, active.editor.contents())?;
        let (theme_flavor, theme, theme_message) = load_theme();
        if !theme_message.is_empty() {
            if !message.is_empty() {
                message.push_str("; ");
            }
            message.push_str(&theme_message);
        }
        let (keymap, keymap_message) = load_keymap();
        if !keymap_message.is_empty() {
            if !message.is_empty() {
                message.push_str("; ");
            }
            message.push_str(&keymap_message);
        }
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
            provider_submitted: BTreeMap::new(),
            decorations: BTreeMap::new(),
            semantic_decorations: BTreeMap::new(),
            prompt: None,
            search_prompt_origin: None,
            search_highlight: false,
            message,
            tasks: TaskRunner::new(1, 8)?,
            active_task: None,
            next_task_id: 1,
            terminal: None,
            terminal_focused: false,
            terminal_escape_pending: false,
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
            quit: false,
        };
        app.capture_debug_output();
        app.record_active_file();
        app.prime_active_syntax();
        app.begin_lsp_start();
        Ok(app)
    }

    fn handle_input(&mut self, input: TerminalInput) -> Result<()> {
        if let Some(lsp) = &mut self.lsp
            && lsp.semantic_due.is_some()
        {
            lsp.semantic_due = Some(Instant::now() + Duration::from_millis(750));
        }
        if self.terminal_focused {
            return self.handle_terminal_input(input);
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
            TerminalInput::MouseClick { .. } => Ok(()),
            TerminalInput::Ignored | TerminalInput::Resized { .. } => Ok(()),
        }
    }

    fn handle_mouse_click(
        &mut self,
        layout: &ViewportLayout,
        column: usize,
        row: usize,
    ) -> Result<()> {
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
        self.ace_jump = None;
        self.normal_prefix = None;
        self.leader_keys = None;
        self.message.clear();
        Ok(())
    }

    fn handle_prompt_key(&mut self, key: TerminalKey) -> Result<()> {
        match key.code {
            TerminalKeyCode::Escape => {
                if self.search_prompt_origin.is_some() {
                    self.cancel_search_prompt();
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

    fn prompt_kind_is_picker(&self) -> bool {
        self.prompt
            .as_ref()
            .is_some_and(|prompt| prompt.kind.is_picker())
    }

    fn begin_search_prompt(&mut self, kind: PromptKind) {
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

    fn cancel_search_prompt(&mut self) {
        if let Some(origin) = self.search_prompt_origin.take() {
            self.active.editor.set_cursor(origin.cursor);
            if let Some((pattern, direction)) = origin.previous_search {
                self.active.editor.restore_search(pattern, direction);
            } else {
                self.active.editor.clear_search();
            }
            self.search_highlight = origin.previous_highlight;
        }
        self.prompt = None;
        self.message.clear();
    }

    fn update_incremental_search(&mut self) {
        let Some(prompt) = self.prompt.as_ref().filter(|prompt| {
            matches!(
                prompt.kind,
                PromptKind::SearchForward | PromptKind::SearchBackward
            )
        }) else {
            return;
        };
        let Some(origin) = self.search_prompt_origin.as_ref() else {
            return;
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
                    .restore_search(pattern.clone(), *direction);
            } else {
                self.active.editor.clear_search();
            }
            self.search_highlight = origin.previous_highlight;
            self.message.clear();
            return;
        }
        let found = match self.active.editor.preview_search(&query, direction, cursor) {
            Ok(found) => found,
            Err(error) => {
                self.active.editor.set_cursor(cursor);
                self.active.editor.restore_search(query, direction);
                self.search_highlight = false;
                self.message = error.to_string();
                return;
            }
        };
        self.active.editor.restore_search(query.clone(), direction);
        self.search_highlight = true;
        if let Some(byte) = found {
            self.active.editor.set_cursor(byte);
            self.message = format!("{}{}", prompt.prefix(), query);
        } else {
            self.active.editor.set_cursor(cursor);
            self.message = format!("pattern not found: {query}");
        }
    }

    fn complete_prompt(&mut self) {
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

    fn execute_prompt(&mut self, prompt: Prompt) -> Result<()> {
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
                                self.active.editor.restore_search(pattern, direction);
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
                self.search_highlight = true;
                if !prompt.buffer.is_empty() {
                    self.after_effect(None, vec![StateDelta::SearchPattern(pattern.into())]);
                }
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

    fn handle_editor_key(&mut self, key: TerminalKey) -> Result<()> {
        if self.ace_jump.is_some() {
            return self.handle_ace_jump_key(key);
        }
        let dismisses_popup = key.code == TerminalKeyCode::Escape
            || (key.code == TerminalKeyCode::Char('K')
                && !key.control
                && !key.alt
                && !key.super_key);
        if dismisses_popup && self.popup.take().is_some() {
            self.popup_deadline = None;
            self.leader_keys = None;
            self.leader_deadline = None;
            return Ok(());
        }
        if self.active.editor.mode() == Mode::Normal && key.code == TerminalKeyCode::Escape {
            self.active.editor.cancel_pending();
            self.normal_prefix = None;
            self.leader_keys = None;
            self.leader_deadline = None;
            self.message.clear();
            return Ok(());
        }
        if self.active.editor.mode() == Mode::Insert
            && key.code == TerminalKeyCode::Tab
            && !self.snippet_stops.is_empty()
        {
            self.move_snippet_stop(if key.shift { -1 } else { 1 });
            return Ok(());
        }
        if self.active.editor.mode() == Mode::Insert && key.control {
            match key.code {
                TerminalKeyCode::Char('n' | 'N' | 'p' | 'P') => {
                    if self.completion.is_some() {
                        self.move_completion(
                            if matches!(key.code, TerminalKeyCode::Char('p' | 'P')) {
                                -1
                            } else {
                                1
                            },
                        );
                    } else {
                        self.request_completion();
                    }
                    return Ok(());
                }
                TerminalKeyCode::Char(' ') => {
                    self.request_completion();
                    return Ok(());
                }
                TerminalKeyCode::Char('e' | 'E') => {
                    self.completion = None;
                    self.completion_selected = false;
                    self.completion_documentation_scroll = 0;
                    self.message = "completion cancelled".to_owned();
                    return Ok(());
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
                    return Ok(());
                }
                _ => {}
            }
        }
        if self.active.editor.mode() == Mode::Insert
            && key.code == TerminalKeyCode::Enter
            && self.completion.is_some()
            && self.completion_selected
        {
            return self.accept_completion();
        }
        self.completion = None;
        self.completion_selected = false;
        self.completion_documentation_scroll = 0;
        if self.leader_keys.is_some() {
            if key.code == TerminalKeyCode::Escape {
                self.leader_keys = None;
                self.leader_deadline = None;
                self.message.clear();
                return Ok(());
            }
            if !key.control
                && !key.alt
                && !key.super_key
                && let TerminalKeyCode::Char(character) = key.code
            {
                return self.handle_leader_character(character);
            }
            self.leader_keys = None;
            self.leader_deadline = None;
        }
        if self.active.editor.mode() == Mode::Normal
            && let Some(prefix) = self.normal_prefix.take()
        {
            if prefix == '\u{17}' {
                return self.handle_window_prefix(key);
            }
            if !key.control
                && !key.alt
                && !key.super_key
                && let TerminalKeyCode::Char(character) = key.code
            {
                match (prefix, character) {
                    ('[', 'd') => return self.move_diagnostic(-1),
                    (']', 'd') => return self.move_diagnostic(1),
                    ('[', 'c') => return self.move_git_hunk(-1),
                    (']', 'c') => return self.move_git_hunk(1),
                    ('g', 'D') => return self.lsp_location("textDocument/declaration"),
                    ('g', 'd') => return self.lsp_location("textDocument/definition"),
                    ('g', 'i') => return self.lsp_location("textDocument/implementation"),
                    ('g', 'r') => return self.lsp_references(),
                    ('g', 'q') => return self.format_text_width(),
                    ('g', ';') => {
                        let count = self.take_normal_count().unwrap_or(1);
                        if !self.navigate_change_count(true, count) {
                            self.message = "at oldest change".to_owned();
                        }
                        return Ok(());
                    }
                    ('g', ',') => {
                        let count = self.take_normal_count().unwrap_or(1);
                        if !self.navigate_change_count(false, count) {
                            self.message = "at newest change".to_owned();
                        }
                        return Ok(());
                    }
                    ('z', 'z') => {
                        self.apply_z_count();
                        self.center_cursor_line(ViewPosition::Middle);
                        return Ok(());
                    }
                    ('z', 't') => {
                        self.apply_z_count();
                        self.center_cursor_line(ViewPosition::Top);
                        return Ok(());
                    }
                    ('z', 'b') => {
                        self.apply_z_count();
                        self.center_cursor_line(ViewPosition::Bottom);
                        return Ok(());
                    }
                    ('Z', 'Z') => return self.execute_ex("wq"),
                    ('Z', 'Q') => return self.execute_ex("q!"),
                    _ => {
                        self.dispatch_key(KeyEvent::character(prefix));
                        if let Some(event) = grammar_key(key) {
                            self.dispatch_key(event);
                        }
                        return Ok(());
                    }
                }
            }
            self.dispatch_key(KeyEvent::character(prefix));
        }
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
            return Ok(());
        }
        if self.active.editor.mode() == Mode::Normal
            && key.control
            && let TerminalKeyCode::Char(character) = key.code
            && matches!(character.to_ascii_lowercase(), 'h' | 'j' | 'k' | 'l')
        {
            let _ = self.take_normal_count();
            if character.eq_ignore_ascii_case(&'k') && self.active_language_server().is_some() {
                return self.lsp_hover("textDocument/signatureHelp");
            }
            let direction = match character.to_ascii_lowercase() {
                'h' => WindowDirection::Left,
                'j' => WindowDirection::Down,
                'k' => WindowDirection::Up,
                _ => WindowDirection::Right,
            };
            self.views.focus_window(direction)?;
            self.activate_view_buffer()?;
            return Ok(());
        }
        if self.active.editor.mode() == Mode::Normal
            && key.control
            && let TerminalKeyCode::Char(character) = key.code
            && matches!(character.to_ascii_lowercase(), 'd' | 'u' | 'f' | 'b')
        {
            let full_page = matches!(character.to_ascii_lowercase(), 'f' | 'b');
            let direction = if matches!(character.to_ascii_lowercase(), 'u' | 'b') {
                -1
            } else {
                1
            };
            let count = self.take_normal_count();
            self.scroll_page(direction, full_page, count);
            return Ok(());
        }
        if self.active.editor.mode() == Mode::Normal && key.control {
            match key.code {
                TerminalKeyCode::Char('w' | 'W') => {
                    self.normal_prefix = Some('\u{17}');
                    self.message =
                        "window: h/j/k/l focus · s/v split · c close · o only · w next".to_owned();
                    return Ok(());
                }
                TerminalKeyCode::Char('o' | 'O') => {
                    let count = self.take_normal_count().unwrap_or(1);
                    if !self.navigate_jump_count(true, count)? {
                        self.message = "at oldest jump".to_owned();
                    }
                    return Ok(());
                }
                TerminalKeyCode::Char('e' | 'E') => {
                    let count = self.take_normal_count().unwrap_or(1);
                    self.scroll_view_line(1, count);
                    return Ok(());
                }
                TerminalKeyCode::Char('y' | 'Y') => {
                    let count = self.take_normal_count().unwrap_or(1);
                    self.scroll_view_line(-1, count);
                    return Ok(());
                }
                TerminalKeyCode::Char('a' | 'A' | 'x' | 'X') => {
                    let count = self.take_normal_count().unwrap_or(1);
                    let direction = if matches!(key.code, TerminalKeyCode::Char('a' | 'A')) {
                        1_i64
                    } else {
                        -1_i64
                    };
                    let delta = direction.saturating_mul(i64::try_from(count).unwrap_or(i64::MAX));
                    let transaction = self.active.editor.adjust_number(delta)?;
                    self.after_transaction(transaction);
                    return Ok(());
                }
                TerminalKeyCode::Char('g' | 'G') => {
                    let _ = self.take_normal_count();
                    self.show_file_info();
                    return Ok(());
                }
                _ => {}
            }
        }
        if self.active.editor.mode() == Mode::Normal && key.code == TerminalKeyCode::Tab {
            let count = self.take_normal_count().unwrap_or(1);
            if !self.navigate_jump_count(false, count)? {
                self.message = "at newest jump".to_owned();
            }
            return Ok(());
        }
        if self.active.editor.mode() == Mode::Normal
            && matches!(
                key.code,
                TerminalKeyCode::PageUp | TerminalKeyCode::PageDown
            )
        {
            let count = self.take_normal_count();
            self.scroll_page(
                if key.code == TerminalKeyCode::PageUp {
                    -1
                } else {
                    1
                },
                true,
                count,
            );
            return Ok(());
        }
        if self.tasks.is_document_blocked(self.active.document_id) && !is_navigation_key(key) {
            self.message = "document is waiting for a TaskCommand; Ctrl-C cancels".to_owned();
            return Ok(());
        }
        if key.control && matches!(key.code, TerminalKeyCode::Char('s' | 'S')) {
            return self.save(None);
        }
        if self.active.editor.mode() == Mode::Normal
            && !key.control
            && !key.alt
            && !key.super_key
            && key.code == TerminalKeyCode::Char(' ')
        {
            self.active.editor.cancel_pending();
            self.leader_keys = Some(String::new());
            self.leader_deadline = Some(Instant::now() + Duration::from_millis(500));
            self.message.clear();
            self.show_which_key("");
            return Ok(());
        }
        if self.active.editor.mode() == Mode::Normal && !key.control && !key.alt && !key.super_key {
            if matches!(key.code, TerminalKeyCode::Char('p' | 'P'))
                && let Some(clipboard) = system_clipboard_text()
            {
                self.active.editor.set_register('+', clipboard, false);
            }
            if key.code == TerminalKeyCode::Char('=')
                && matches!(
                    self.active.editor.pending_parse_state(),
                    Some(ParseState::Register { .. })
                )
            {
                self.active.editor.cancel_pending();
                self.prompt = Some(Prompt::new(PromptKind::Expression));
                return Ok(());
            }
            match key.code {
                TerminalKeyCode::Char('g' | '[' | ']' | 'z' | 'Z') => {
                    if let TerminalKeyCode::Char(prefix) = key.code {
                        self.normal_prefix = Some(prefix);
                    }
                    return Ok(());
                }
                TerminalKeyCode::Char('K') => {
                    let _ = self.take_normal_count();
                    return self.lsp_hover("textDocument/hover");
                }
                TerminalKeyCode::Char('H') => {
                    let count = self.take_normal_count().unwrap_or(1);
                    self.move_cursor_to_view(ViewPosition::Top, count);
                    return Ok(());
                }
                TerminalKeyCode::Char('M') => {
                    let _ = self.take_normal_count();
                    self.move_cursor_to_view(ViewPosition::Middle, 1);
                    return Ok(());
                }
                TerminalKeyCode::Char('L') => {
                    let count = self.take_normal_count().unwrap_or(1);
                    self.move_cursor_to_view(ViewPosition::Bottom, count);
                    return Ok(());
                }
                TerminalKeyCode::Char('*' | '#') => {
                    let backward = key.code == TerminalKeyCode::Char('#');
                    let count = self.take_normal_count().unwrap_or(1);
                    self.search_word_under_cursor(backward, count);
                    return Ok(());
                }
                TerminalKeyCode::Char(';' | ',') => {
                    let reverse = key.code == TerminalKeyCode::Char(',');
                    let count = self.take_normal_count().unwrap_or(1);
                    if !self
                        .active
                        .editor
                        .repeat_find(reverse, u32::try_from(count).unwrap_or(u32::MAX))
                    {
                        self.message = "no previous character search".to_owned();
                    }
                    return Ok(());
                }
                TerminalKeyCode::Char(':') => {
                    self.prompt = Some(Prompt::new(PromptKind::Command));
                    self.message.clear();
                    return Ok(());
                }
                TerminalKeyCode::Char('/') => {
                    self.begin_search_prompt(PromptKind::SearchForward);
                    return Ok(());
                }
                TerminalKeyCode::Char('?') => {
                    self.begin_search_prompt(PromptKind::SearchBackward);
                    return Ok(());
                }
                _ => {}
            }
        }
        if let Some(event) = grammar_key(key) {
            self.dispatch_key(event);
        }
        Ok(())
    }

    fn handle_leader_character(&mut self, character: char) -> Result<()> {
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

    fn binding_enabled(&self, binding: &RuntimeBinding) -> bool {
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

    fn execute_runtime_command(&mut self, invocation: &CommandInvocation) -> Result<()> {
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

    fn start_ace_jump(&mut self) {
        self.ace_jump = Some(AceJumpState::AwaitTarget);
        self.message = "jump to character: ".to_owned();
    }

    fn handle_ace_jump_key(&mut self, key: TerminalKey) -> Result<()> {
        let Some(state) = self.ace_jump.take() else {
            return Ok(());
        };
        if key.code == TerminalKeyCode::Escape {
            self.message.clear();
            return Ok(());
        }
        match state {
            AceJumpState::AwaitTarget => {
                let TerminalKeyCode::Char(target) = key.code else {
                    self.message = "jump cancelled".to_owned();
                    return Ok(());
                };
                self.populate_ace_jump(target);
            }
            AceJumpState::AwaitLabel {
                target,
                mut prefix,
                targets,
            } => {
                if key.code == TerminalKeyCode::Backspace {
                    prefix.pop();
                } else if let TerminalKeyCode::Char(label) = key.code {
                    prefix.push(label.to_ascii_lowercase());
                } else {
                    self.message = "jump cancelled".to_owned();
                    return Ok(());
                }
                let matching = targets
                    .iter()
                    .filter(|candidate| candidate.label.starts_with(&prefix))
                    .collect::<Vec<_>>();
                if matching.len() == 1 {
                    let byte = matching[0].byte;
                    self.finish_ace_jump(byte);
                } else if matching.is_empty() {
                    self.message = format!("no {target:?} jump labeled {prefix}");
                } else {
                    self.message = format!("jump {target:?}: {prefix}");
                    self.ace_jump = Some(AceJumpState::AwaitLabel {
                        target,
                        prefix,
                        targets,
                    });
                }
            }
        }
        Ok(())
    }

    fn populate_ace_jump(&mut self, target: char) {
        let text = self.active.editor.contents();
        let top_line = self.views.active_window().top_line;
        let visible_lines = self.viewport_rows.saturating_sub(1).max(1);
        let start = self.active.editor.text().byte_of_line(top_line);
        let end = self
            .active
            .editor
            .text()
            .byte_of_line(top_line.saturating_add(visible_lines))
            .max(start)
            .min(text.len());
        let cursor = self.active.editor.primary_cursor();
        let bytes = text[start..end]
            .char_indices()
            .filter(|(_, character)| *character == target)
            .map(|(relative, _)| start + relative)
            .filter(|byte| *byte != cursor)
            .collect::<Vec<_>>();
        if bytes.is_empty() {
            self.message = format!("no {target:?} in view");
            return;
        }
        if bytes.len() == 1 {
            self.finish_ace_jump(bytes[0]);
            return;
        }
        let labels = ace_jump_labels(bytes.len());
        let targets = bytes
            .into_iter()
            .zip(labels)
            .map(|(byte, label)| AceJumpTarget {
                byte,
                label: label.into_boxed_str(),
            })
            .collect::<Vec<_>>();
        self.message = format!("jump {target:?}: type label");
        self.ace_jump = Some(AceJumpState::AwaitLabel {
            target,
            prefix: String::new(),
            targets,
        });
    }

    fn finish_ace_jump(&mut self, byte: usize) {
        let origin = self.current_jump_location();
        self.active.editor.set_cursor(byte);
        if let (Some(origin), Some(target)) = (origin, self.current_jump_location()) {
            self.record_navigation(origin, target);
        }
        self.ace_jump = None;
        self.message.clear();
    }

    fn ace_jump_overlay(&self) -> Option<AceJumpOverlay> {
        let AceJumpState::AwaitLabel {
            prefix, targets, ..
        } = self.ace_jump.as_ref()?
        else {
            return None;
        };
        Some(AceJumpOverlay {
            targets: targets
                .iter()
                .filter_map(|target| {
                    target
                        .label
                        .strip_prefix(prefix)
                        .map(|suffix| AceJumpTarget {
                            byte: target.byte,
                            label: suffix.into(),
                        })
                })
                .collect(),
        })
    }

    fn show_which_key(&mut self, prefix: &str) {
        let mut entries = BTreeMap::<String, String>::new();
        if let Some(binding) = self
            .keymap
            .leader
            .get(prefix)
            .filter(|binding| self.binding_enabled(binding))
        {
            entries.insert("(wait)".to_owned(), binding.description.to_string());
        }
        let mut next_keys = BTreeSet::new();
        for (sequence, binding) in &self.keymap.leader {
            if self.binding_enabled(binding)
                && let Some(rest) = sequence.strip_prefix(prefix)
                && !rest.is_empty()
                && let Some(next) = rest.chars().next()
            {
                next_keys.insert(next);
            }
        }
        for next in next_keys {
            let candidate = format!("{prefix}{next}");
            let exact = self
                .keymap
                .leader
                .get(candidate.as_str())
                .filter(|binding| self.binding_enabled(binding));
            let longer = self.keymap.leader.iter().any(|(sequence, binding)| {
                sequence.len() > candidate.len()
                    && sequence.starts_with(candidate.as_str())
                    && self.binding_enabled(binding)
            });
            let group = self
                .keymap
                .groups
                .get(candidate.as_str())
                .or_else(|| self.keymap.groups.get(next.to_string().as_str()))
                .map_or("group", Box::as_ref);
            let description = match (exact, longer) {
                (Some(binding), true) => format!("+{group} / {}", binding.description),
                (Some(binding), false) => binding.description.to_string(),
                (None, true) => format!("+{group}"),
                (None, false) => continue,
            };
            entries.insert(
                if prefix.is_empty() && next == ' ' {
                    "Space".to_owned()
                } else {
                    next.to_string()
                },
                description,
            );
        }
        let title = if prefix.is_empty() {
            " NORMAL ".to_owned()
        } else {
            format!(
                " {} ",
                self.keymap.groups.get(prefix).map_or(prefix, Box::as_ref)
            )
        };
        let width = entries
            .keys()
            .map(|key| key.chars().count())
            .max()
            .unwrap_or(1);
        let text = entries
            .iter()
            .map(|(key, description)| format!("{key:>width$}  {description}"))
            .collect::<Vec<_>>()
            .join("\n");
        self.popup = Some(TextPopup {
            title: title.into(),
            text: text.into(),
            scroll: 0,
            decorations: Vec::new(),
        });
        self.popup_deadline = None;
    }

    fn poll_mapping_timeout(&mut self) -> Result<bool> {
        if self
            .leader_deadline
            .is_none_or(|deadline| Instant::now() < deadline)
        {
            return Ok(false);
        }
        self.leader_deadline = None;
        let Some(sequence) = self.leader_keys.take() else {
            return Ok(false);
        };
        self.popup = None;
        self.popup_deadline = None;
        let binding = self
            .keymap
            .leader
            .get(sequence.as_str())
            .filter(|binding| self.binding_enabled(binding))
            .cloned();
        if let Some(binding) = binding {
            self.execute_runtime_command(&binding.invocation)?;
        } else {
            self.message = format!("incomplete mapping <Space>{sequence}");
        }
        Ok(true)
    }

    fn show_info(&mut self, information: impl std::fmt::Display) {
        self.show_message(MessageSeverity::Info, information);
    }

    fn show_error(&mut self, error: impl std::fmt::Display) {
        self.show_message(MessageSeverity::Error, error);
    }

    fn show_message(&mut self, severity: MessageSeverity, message: impl std::fmt::Display) {
        let message = message.to_string();
        self.record_debug_output(severity, &message);
        self.message = message.clone();
        if severity == MessageSeverity::Error {
            self.popup = Some(TextPopup {
                title: "Error".into(),
                text: message.into(),
                scroll: 0,
                decorations: Vec::new(),
            });
            self.popup_deadline = Some(Instant::now() + Duration::from_secs(8));
        }
    }

    fn capture_debug_output(&mut self) {
        if self.message.is_empty() {
            return;
        }
        let message = self.message.clone();
        self.record_debug_output(MessageSeverity::Info, &message);
    }

    fn record_debug_output(&mut self, severity: MessageSeverity, text: &str) {
        const MAX_ENTRIES: usize = 512;
        if text.trim().is_empty()
            || self
                .views
                .messages
                .entries
                .last()
                .is_some_and(|entry| entry.text.as_ref() == text)
        {
            return;
        }
        let sequence = self
            .views
            .messages
            .entries
            .last()
            .map_or(1, |entry| entry.sequence.saturating_add(1));
        self.views.messages.entries.push(MessageEntry {
            sequence,
            severity,
            text: text.into(),
        });
        let overflow = self
            .views
            .messages
            .entries
            .len()
            .saturating_sub(MAX_ENTRIES);
        if overflow > 0 {
            self.views.messages.entries.drain(..overflow);
        }
    }

    fn show_debug_output(&mut self) -> Result<()> {
        self.capture_debug_output();
        let text = if self.views.messages.entries.is_empty() {
            "No debug output has been recorded.".to_owned()
        } else {
            self.views
                .messages
                .entries
                .iter()
                .map(|entry| {
                    let severity = match entry.severity {
                        MessageSeverity::Info => "INFO",
                        MessageSeverity::Warning => "WARN",
                        MessageSeverity::Error => "ERROR",
                    };
                    format!("{:04} [{severity}] {}", entry.sequence, entry.text)
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        self.open_messages_buffer(text)?;
        self.popup = None;
        self.popup_deadline = None;
        self.message.clear();
        Ok(())
    }

    fn open_messages_buffer(&mut self, text: String) -> Result<()> {
        let document_id = virtual_document_id(MESSAGES_BUFFER_NAME, &text);
        let active_is_messages = self.active.has_display_name(MESSAGES_BUFFER_NAME);
        let inactive_index = self
            .inactive
            .iter()
            .position(|buffer| buffer.has_display_name(MESSAGES_BUFFER_NAME));
        let is_new = !active_is_messages && inactive_index.is_none();
        let buffer_id = if active_is_messages {
            self.active.buffer_id
        } else if let Some(index) = inactive_index {
            self.inactive[index].buffer_id
        } else {
            self.views.add_buffer(document_id, MESSAGES_BUFFER_NAME)
        };
        let mut messages =
            match BufferState::virtual_buffer(buffer_id, document_id, MESSAGES_BUFFER_NAME, text) {
                Ok(messages) => messages,
                Err(error) => {
                    if is_new {
                        self.views.buffers.retain(|buffer| buffer.id != buffer_id);
                    }
                    return Err(error);
                }
            };
        restore_client_state(&mut messages, &self.client_state)?;
        if let Err(error) = self
            .mutations
            .register(document_id, messages.editor.contents())
        {
            if is_new {
                self.views.buffers.retain(|buffer| buffer.id != buffer_id);
            }
            return Err(error);
        }

        if active_is_messages {
            self.active = messages;
        } else {
            self.autosave_active_if_named()?;
            let previous = std::mem::replace(&mut self.active, messages);
            if let Some(index) = inactive_index {
                self.inactive[index] = previous;
            } else {
                self.inactive.push(previous);
            }
            self.views.set_active_buffer(buffer_id)?;
        }
        let view = self
            .views
            .buffers
            .iter_mut()
            .find(|buffer| buffer.id == buffer_id)
            .ok_or_else(|| anyhow!("messages buffer view disappeared"))?;
        view.document_id = document_id;
        view.name = MESSAGES_BUFFER_NAME.into();
        self.decorations.remove(&buffer_id);
        self.semantic_decorations.remove(&buffer_id);
        self.prime_active_syntax();
        self.begin_lsp_start();
        Ok(())
    }

    fn poll_popup_timeout(&mut self) -> bool {
        if self
            .popup_deadline
            .is_none_or(|deadline| Instant::now() < deadline)
        {
            return false;
        }
        self.popup_deadline = None;
        self.popup.take().is_some()
    }

    fn dispatch_key(&mut self, key: KeyEvent) {
        let registers_before = register_snapshot(&self.active.editor);
        let marks_before = mark_snapshot(&self.active.editor);
        let macros_before = macro_snapshot(&self.active.editor);
        let repeat_before = self.active.editor.durable_repeat_data();
        match self.active.editor.handle_key(key) {
            Ok(transaction) => {
                self.message.clear();
                let mut state_deltas = changed_registers(&registers_before, &self.active.editor);
                state_deltas.extend(changed_global_marks(
                    &marks_before,
                    &self.active.editor,
                    self.active.document_id,
                ));
                state_deltas.extend(changed_macros(&macros_before, &self.active.editor));
                let repeat_after = self.active.editor.durable_repeat_data();
                if repeat_after != repeat_before
                    && let Some(repeat) = repeat_after
                {
                    state_deltas.push(StateDelta::RepeatData(repeat));
                }
                self.after_effect(transaction, state_deltas);
            }
            Err(error) => self.engine_error(error),
        }
    }

    fn engine_error(&mut self, error: EngineError) {
        match error {
            EngineError::InvalidGrammar { sequence, reason } => {
                self.active.editor.cancel_pending();
                self.show_info(format!(
                    "grammar rejected sequence {:?}: {reason}",
                    format_key_sequence(&sequence)
                ));
            }
            error => self.show_error(error),
        }
    }

    fn after_transaction(&mut self, transaction: Option<wren_types::Transaction>) {
        self.after_effect(transaction, Vec::new());
    }

    fn after_effect(&mut self, transaction: Option<Transaction>, state_deltas: Vec<StateDelta>) {
        for delta in &state_deltas {
            self.client_state.apply(delta);
        }
        if !state_deltas.is_empty() {
            if let Err(error) =
                sync_client_state(&mut self.active, &mut self.inactive, &self.client_state)
            {
                self.show_error(format!("client state: {error}"));
            }
            self.client_state_worker.try_save(self.client_state.clone());
        }
        if let Err(error) =
            self.mutations
                .append(self.active.document_id, transaction.clone(), state_deltas)
        {
            self.show_error(format!("mutation outbox: {error}"));
        }
        let Some(transaction) = transaction else {
            return;
        };
        if let Some(syntax) = self.decorations.get_mut(&self.active.buffer_id)
            && syntax.revision == transaction.base_revision
        {
            syntax.spans = syntax
                .spans
                .iter()
                .filter_map(|span| {
                    let start = transaction.map_offset(span.range.start, Bias::Left).ok()?;
                    let end = transaction.map_offset(span.range.end, Bias::Right).ok()?;
                    (start < end).then_some(DecorationSpan {
                        range: start..end,
                        style: span.style,
                    })
                })
                .collect();
            syntax.revision = self.active.editor.revision();
        }
        if let Some(semantic) = self.semantic_decorations.get_mut(&self.active.buffer_id)
            && semantic.revision == transaction.base_revision
        {
            semantic.spans = semantic
                .spans
                .iter()
                .filter_map(|span| {
                    let start = transaction.map_offset(span.range.start, Bias::Left).ok()?;
                    let end = transaction.map_offset(span.range.end, Bias::Right).ok()?;
                    (start < end).then_some(DecorationSpan {
                        range: start..end,
                        style: span.style,
                    })
                })
                .collect();
            semantic.revision = self.active.editor.revision();
        }
        self.refresh_changed_syntax(&transaction);
        if let Some(lsp) = &mut self.lsp {
            if lsp.semantic_legend.is_some() {
                lsp.semantic_due = Some(Instant::now() + Duration::from_millis(750));
            }
        } else if self.lsp_start.is_some() || self.lsp_background.is_some() {
            self.lsp_semantic_dirty = true;
        }
        if let Some(wal) = &self.active.wal {
            wal.append(RecoveredState {
                base_hash: self.active.base_hash,
                revision: self.active.editor.revision().get(),
                text: self.active.editor.contents(),
                cursor: self.active.editor.primary_cursor(),
            });
        }
    }

    fn execute_ex(&mut self, input: &str) -> Result<()> {
        let input = input.trim();
        if input.is_empty() {
            return Ok(());
        }
        if input == "FormatToggle" {
            self.format_on_save = !self.format_on_save;
            self.message = format!(
                "format-on-save globally {}",
                if self.format_on_save {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            return Ok(());
        }
        if input == "FormatToggle!" {
            let document_id = self.active.document_id;
            if !self.format_disabled.remove(&document_id) {
                self.format_disabled.insert(document_id);
            }
            self.message = format!(
                "format-on-save for buffer {}",
                if self.format_disabled.contains(&document_id) {
                    "disabled"
                } else {
                    "enabled"
                }
            );
            return Ok(());
        }
        let mut words = input.split_whitespace();
        match words.next() {
            Some("colorscheme" | "Catppuccin") => {
                let requested = words.next().unwrap_or("catppuccin");
                let flavor = CatppuccinFlavor::parse(requested)
                    .ok_or_else(|| anyhow!("unknown Catppuccin flavor {requested:?}"))?;
                self.theme_flavor = flavor;
                self.theme = CatppuccinPalette::for_flavor(flavor);
                self.message = format!("colorscheme catppuccin-{}", flavor.name());
                return Ok(());
            }
            Some("setcolor") => {
                let name = words
                    .next()
                    .ok_or_else(|| anyhow!("usage: setcolor SLOT #RRGGBB"))?;
                let value = words
                    .next()
                    .ok_or_else(|| anyhow!("usage: setcolor SLOT #RRGGBB"))?;
                let color = RgbColor::from_hex(value)
                    .ok_or_else(|| anyhow!("invalid RGB color {value:?}; expected #RRGGBB"))?;
                if !self.theme.set(name, color) {
                    bail!("unknown Catppuccin color slot {name:?}");
                }
                self.message = format!(
                    "theme {name}=#{:02x}{:02x}{:02x}",
                    color.red, color.green, color.blue
                );
                return Ok(());
            }
            Some("Git") => {
                let arguments = words.map(Box::<str>::from).collect::<Vec<_>>();
                let arguments = if arguments.is_empty() {
                    vec!["status".into()]
                } else {
                    arguments
                };
                return self.open_terminal(Some("git"), &arguments);
            }
            Some("Gwrite") => return self.git_stage_buffer(),
            Some("Gdiffsplit") => return self.git_diff_index(),
            Some("AvanteToggle") => {
                if self
                    .popup
                    .as_ref()
                    .is_some_and(|popup| popup.title.as_ref() == "Avante · Codex")
                {
                    self.popup = None;
                    self.popup_deadline = None;
                } else if self.ai_transcript.is_empty() {
                    self.prompt = Some(Prompt::new(PromptKind::Ai));
                } else {
                    self.show_ai_transcript();
                }
                return Ok(());
            }
            Some("Codex" | "AvanteChat" | "AvanteAsk") => {
                let prompt = words.collect::<Vec<_>>().join(" ");
                if prompt.is_empty() {
                    self.prompt = Some(Prompt::new(PromptKind::Ai));
                    return Ok(());
                } else {
                    return self.start_ai_task(&prompt);
                }
            }
            Some("RustLsp") => {
                return match words.next() {
                    Some("testables" | "test") => {
                        self.open_terminal(Some("cargo"), &["test".into()])
                    }
                    Some("debuggables" | "debug") => self.open_debug_repl(),
                    _ => self.open_terminal(Some("cargo"), &["run".into()]),
                };
            }
            _ => {}
        }
        self.execute_ex_command(parse_ex(input)?)
    }

    fn execute_ex_command(&mut self, command: ExCommand) -> Result<()> {
        match command {
            ExCommand::Goto { address } => {
                let line = self.resolve_address(&address)?;
                self.active
                    .editor
                    .set_cursor(self.active.editor.text().byte_of_line(line));
                Ok(())
            }
            ExCommand::Substitute {
                range,
                pattern,
                replacement,
                flags,
            } => {
                let range = self.resolve_byte_range(range.as_ref())?;
                self.start_substitution_task(Substitute {
                    needle: pattern.into(),
                    replacement: replacement.into(),
                    ranges: vec![range],
                    global: flags.global,
                    ignore_case: flags.ignore_case,
                })
            }
            ExCommand::Global {
                range,
                invert,
                pattern,
                command,
            } => self.execute_global(range.as_ref(), invert, &pattern, *command),
            ExCommand::Normal { range, keys, .. } => self.execute_normal(range.as_ref(), &keys),
            ExCommand::Write {
                range, all, path, ..
            } => {
                if let Some(range) = range.as_ref() {
                    let path = path
                        .as_deref()
                        .map(Path::new)
                        .ok_or_else(|| anyhow!("ranged :write requires a destination path"))?;
                    return self.write_range(range, path);
                }
                if all {
                    self.save_all()
                } else {
                    self.save(path.as_deref().map(Path::new))
                }
            }
            ExCommand::WriteQuit { path, .. } => {
                self.save(path.as_deref().map(Path::new))?;
                self.quit = true;
                Ok(())
            }
            ExCommand::Quit { all, bang } => {
                let dirty = self.active.editor.is_dirty()
                    || (all && self.inactive.iter().any(|buffer| buffer.editor.is_dirty()));
                if dirty && !bang {
                    self.show_error("E37: unsaved changes; use :q!");
                    return Ok(());
                }
                self.quit = true;
                Ok(())
            }
            ExCommand::Edit { bang, path } => {
                let Some(path) = path else {
                    self.show_error("usage: :e[!] FILE");
                    return Ok(());
                };
                if self.active.editor.is_dirty() && !bang {
                    self.show_error("E37: unsaved changes; use :e!");
                    return Ok(());
                }
                self.open_buffer(Path::new(path.as_ref()))
            }
            ExCommand::Buffer {
                action,
                bang,
                target,
            } => self.buffer_command(action, bang, target.as_deref()),
            ExCommand::Split { vertical, path } => {
                if let Some(path) = path {
                    self.open_buffer(Path::new(path.as_ref()))?;
                }
                self.views.split_active(if vertical {
                    SplitAxis::Vertical
                } else {
                    SplitAxis::Horizontal
                })?;
                self.message = if vertical {
                    "vertical split created".to_owned()
                } else {
                    "horizontal split created".to_owned()
                };
                Ok(())
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
                self.message = value.to_editor_text();
                Ok(())
            }
            ExCommand::Registers { names } => {
                let entries: Vec<_> = self
                    .active
                    .editor
                    .registers()
                    .filter(|(name, _)| names.is_empty() || names.contains(*name))
                    .map(|(name, value)| format!("\"{name} {}", compact(value.text.as_ref(), 24)))
                    .collect();
                self.message = if entries.is_empty() {
                    "no registers".to_owned()
                } else {
                    entries.join("  ")
                };
                Ok(())
            }
            ExCommand::Marks { names } => {
                let entries: Vec<_> = self
                    .active
                    .editor
                    .marks()
                    .filter(|(name, _)| names.is_empty() || names.contains(*name))
                    .map(|(name, byte)| format!("'{name}={byte}"))
                    .collect();
                self.message = if entries.is_empty() {
                    "no marks".to_owned()
                } else {
                    entries.join("  ")
                };
                Ok(())
            }
            ExCommand::NoHighlight => {
                self.search_highlight = false;
                self.message.clear();
                Ok(())
            }
            ExCommand::Help { topic } => {
                self.message = topic.map_or_else(
                    || "run `wren --help` for the command reference".to_owned(),
                    |topic| format!("help for {topic}: run `wren --help`"),
                );
                Ok(())
            }
            ExCommand::Messages => self.show_debug_output(),
            ExCommand::Grep { pattern, paths } => self.grep(&pattern, &paths),
            ExCommand::Cdo { command } => self.execute_cdo(*command),
            ExCommand::ConvertUtf8 => {
                if self.active.document.encoding() == DocumentEncoding::Utf8 {
                    self.message = "document is already UTF-8".to_owned();
                    return Ok(());
                }
                let converted = self.active.document.convert_to_utf8()?;
                self.active.editor.set_read_only(false);
                let transaction = Transaction::new(
                    self.active.editor.revision(),
                    vec![Edit::new(
                        0..self.active.editor.text().len_bytes(),
                        converted,
                    )],
                )?;
                self.active.editor.apply_transaction(transaction.clone())?;
                self.after_transaction(Some(transaction));
                self.message = "converted invalid bytes to explicit UTF-8 \\xNN escapes".to_owned();
                Ok(())
            }
            ExCommand::Terminal { program, arguments } => {
                self.open_terminal(program.as_deref(), &arguments)
            }
            ExCommand::Make { program, arguments } => self.start_make_task(&program, &arguments),
            ExCommand::Format { program, arguments } => {
                self.start_format_task(&program, &arguments)
            }
            ExCommand::Find { query } => self.start_file_picker(&query),
        }
    }

    fn resolve_address(&self, address: &ExAddress) -> Result<usize> {
        let text = self.active.editor.contents();
        let current = self.active.editor.cursor_line_column().0;
        let last = self.active.editor.text().line_of_byte(text.len());
        let line = match address {
            ExAddress::Current => current,
            ExAddress::Last => last,
            ExAddress::Line(line) => line.saturating_sub(1).min(last),
            ExAddress::Mark(name) => self
                .active
                .editor
                .mark(*name)
                .map(|byte| self.active.editor.text().line_of_byte(byte))
                .ok_or_else(|| anyhow!("mark '{name} is not set"))?,
            ExAddress::SearchForward(pattern) => {
                let cursor = self.active.editor.primary_cursor().min(text.len());
                text[cursor..]
                    .find(pattern.as_ref())
                    .map(|offset| self.active.editor.text().line_of_byte(cursor + offset))
                    .or_else(|| {
                        text[..cursor]
                            .find(pattern.as_ref())
                            .map(|offset| self.active.editor.text().line_of_byte(offset))
                    })
                    .ok_or_else(|| anyhow!("pattern not found: {pattern}"))?
            }
            ExAddress::SearchBackward(pattern) => {
                let cursor = self.active.editor.primary_cursor().min(text.len());
                text[..cursor]
                    .rfind(pattern.as_ref())
                    .map(|offset| self.active.editor.text().line_of_byte(offset))
                    .or_else(|| {
                        text[cursor..]
                            .rfind(pattern.as_ref())
                            .map(|offset| self.active.editor.text().line_of_byte(cursor + offset))
                    })
                    .ok_or_else(|| anyhow!("pattern not found: {pattern}"))?
            }
            ExAddress::Offset { base, delta } => {
                let base = self.resolve_address(base)?;
                base.saturating_add_signed(*delta as isize).min(last)
            }
        };
        Ok(line)
    }

    fn resolve_line_range(&self, range: Option<&ExRange>) -> Result<Range<usize>> {
        let current = self.active.editor.cursor_line_column().0;
        let last = self
            .active
            .editor
            .text()
            .line_of_byte(self.active.editor.text().len_bytes());
        let (start, end) = if let Some(range) = range {
            let start = self.resolve_address(&range.start)?;
            let end = range
                .end
                .as_ref()
                .map_or(Ok(start), |address| self.resolve_address(address))?;
            (start.min(end), start.max(end))
        } else {
            (current, current)
        };
        Ok(start.min(last)..end.min(last).saturating_add(1))
    }

    fn resolve_byte_range(&self, range: Option<&ExRange>) -> Result<Range<usize>> {
        let lines = self.resolve_line_range(range)?;
        Ok(self.active.editor.text().byte_of_line(lines.start)
            ..self.active.editor.text().byte_of_line(lines.end))
    }

    fn execute_normal(&mut self, range: Option<&ExRange>, keys: &str) -> Result<()> {
        let lines: Vec<_> = self.resolve_line_range(range)?.collect();
        for line in lines.into_iter().rev() {
            self.active
                .editor
                .set_cursor(self.active.editor.text().byte_of_line(line));
            for key in ex_normal_keys(keys) {
                self.dispatch_key(key);
            }
        }
        Ok(())
    }

    fn execute_global(
        &mut self,
        range: Option<&ExRange>,
        invert: bool,
        pattern: &str,
        command: ExCommand,
    ) -> Result<()> {
        let lines = if range.is_some() {
            self.resolve_line_range(range)?
        } else {
            0..self
                .active
                .editor
                .text()
                .line_of_byte(self.active.editor.text().len_bytes())
                .saturating_add(1)
        };
        let text = self.active.editor.contents();
        let mut selected = Vec::new();
        for line in lines {
            let start = self.active.editor.text().byte_of_line(line);
            let end = self
                .active
                .editor
                .text()
                .byte_of_line(line.saturating_add(1));
            if text[start..end].contains(pattern) != invert {
                selected.push((line, start..end));
            }
        }
        if let ExCommand::Substitute {
            pattern,
            replacement,
            flags,
            ..
        } = command
        {
            return self.start_substitution_task(Substitute {
                needle: pattern.into(),
                replacement: replacement.into(),
                ranges: selected.into_iter().map(|(_, range)| range).collect(),
                global: flags.global,
                ignore_case: flags.ignore_case,
            });
        }
        for (line, _) in selected.into_iter().rev() {
            self.active
                .editor
                .set_cursor(self.active.editor.text().byte_of_line(line));
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

    fn open_buffer(&mut self, path: &Path) -> Result<()> {
        let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if self
            .active
            .document
            .presentation_path()
            .is_some_and(|open| same_path(open, &resolved))
        {
            self.record_file(&resolved);
            return Ok(());
        }
        if let Some(index) = self.inactive.iter().position(|buffer| {
            buffer
                .document
                .presentation_path()
                .is_some_and(|open| same_path(open, &resolved))
        }) {
            self.switch_to_inactive(index)?;
            self.record_file(&resolved);
            return Ok(());
        }
        let document_id = stable_document_id(Some(&resolved));
        let buffer_id = self
            .views
            .add_buffer(document_id, resolved.display().to_string());
        let (mut buffer, message) =
            BufferState::open(buffer_id, document_id, Some(&resolved), None)?;
        restore_client_state(&mut buffer, &self.client_state)?;
        self.mutations
            .register(document_id, buffer.editor.contents())?;
        self.autosave_active_if_named()?;
        let previous = std::mem::replace(&mut self.active, buffer);
        self.inactive.push(previous);
        self.views.set_active_buffer(buffer_id)?;
        self.message = message;
        self.record_active_file();
        self.prime_active_syntax();
        self.begin_lsp_start();
        Ok(())
    }

    fn current_jump_location(&self) -> Option<JumpLocation> {
        self.active
            .document
            .presentation_path()
            .map(|path| JumpLocation {
                document_id: self.active.document_id,
                path: std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
                byte: self.active.editor.primary_cursor(),
            })
    }

    fn entry_cursor_byte(&self, entry: &QuickfixEntry) -> usize {
        let line = entry.line.saturating_sub(1);
        let start = self.active.editor.text().byte_of_line(line);
        let end = self
            .active
            .editor
            .text()
            .byte_of_line(line.saturating_add(1));
        let text = self.active.editor.contents();
        let line_text = text.get(start..end).unwrap_or_default();
        let column = entry.column.saturating_sub(1);
        if !entry.column_utf16 {
            return start.saturating_add(column.min(line_text.len()));
        }
        let mut utf16 = 0;
        for (byte, character) in line_text.char_indices() {
            if utf16 >= column {
                return start.saturating_add(byte);
            }
            utf16 = utf16.saturating_add(character.len_utf16());
        }
        end
    }

    fn record_navigation(&mut self, origin: JumpLocation, target: JumpLocation) {
        if origin == target {
            return;
        }
        let retained = self
            .jump_index
            .map_or(self.jump_history.len(), |index| index.saturating_add(1));
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

    fn navigate_to_entry(&mut self, entry: &QuickfixEntry) -> Result<()> {
        let origin = self.current_jump_location();
        self.open_buffer(&entry.path)?;
        let byte = self.entry_cursor_byte(entry);
        self.active.editor.set_cursor(byte);
        if let (Some(origin), Some(target)) = (origin, self.current_jump_location()) {
            self.record_navigation(origin, target);
        }
        Ok(())
    }

    fn navigate_global_jump(&mut self, backward: bool) -> Result<bool> {
        let Some(index) = self.jump_index else {
            return Ok(false);
        };
        let next = if backward {
            index.checked_sub(1)
        } else {
            index
                .checked_add(1)
                .filter(|next| *next < self.jump_history.len())
        };
        let Some(next) = next else {
            return Ok(false);
        };
        let target = self.jump_history[next].clone();
        self.open_buffer(&target.path)?;
        self.active.editor.set_cursor(target.byte);
        self.jump_index = Some(next);
        self.persist_jump_list();
        self.message = format!("jump {} of {}", next + 1, self.jump_history.len());
        Ok(true)
    }

    fn persist_jump_list(&mut self) {
        let entries = self
            .jump_history
            .iter()
            .map(|location| DurableJumpEntry {
                document_id: location.document_id,
                anchor: Anchor {
                    byte: location.byte,
                    bias: Bias::Right,
                },
                path_hint: Some(
                    location
                        .path
                        .to_string_lossy()
                        .into_owned()
                        .into_boxed_str(),
                ),
            })
            .collect();
        self.after_effect(
            None,
            vec![StateDelta::JumpList {
                entries,
                current: self.jump_index,
            }],
        );
    }

    fn record_active_file(&mut self) {
        if let Some(path) = self
            .active
            .document
            .presentation_path()
            .map(Path::to_path_buf)
        {
            self.record_file(&path);
        }
    }

    fn record_file(&mut self, path: &Path) {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.recent_files.retain(|recent| !same_path(recent, &path));
        self.recent_files.insert(0, path);
        self.recent_files.truncate(100);
        if let Err(error) = save_recent_files(&self.recent_files) {
            self.show_error(format!("oldfiles: {error}"));
        }
    }

    fn switch_to_inactive(&mut self, index: usize) -> Result<()> {
        self.autosave_active_if_named()?;
        let buffer = self
            .inactive
            .get_mut(index)
            .ok_or_else(|| anyhow!("buffer index disappeared"))?;
        std::mem::swap(&mut self.active, buffer);
        self.views.set_active_buffer(self.active.buffer_id)?;
        self.message = self.active.name();
        self.prime_active_syntax();
        self.begin_lsp_start();
        Ok(())
    }

    fn activate_view_buffer(&mut self) -> Result<()> {
        let wanted = self.views.active_buffer();
        if wanted == self.active.buffer_id {
            return Ok(());
        }
        let index = self
            .inactive
            .iter()
            .position(|buffer| buffer.buffer_id == wanted)
            .ok_or_else(|| anyhow!("view references a missing buffer"))?;
        self.switch_to_inactive(index)
    }

    fn buffer_command(
        &mut self,
        action: BufferAction,
        bang: bool,
        target: Option<&str>,
    ) -> Result<()> {
        if action == BufferAction::Delete {
            if self.active.editor.is_dirty() && !bang {
                self.show_error("E89: unsaved changes; use :bdelete!");
                return Ok(());
            }
            let Some(mut replacement) = self.inactive.pop() else {
                self.message = "cannot delete the final buffer".to_owned();
                return Ok(());
            };
            let deleted = self.active.buffer_id;
            std::mem::swap(&mut self.active, &mut replacement);
            self.views.remove_buffer(deleted, self.active.buffer_id)?;
            self.views.set_active_buffer(self.active.buffer_id)?;
            self.message = "buffer deleted".to_owned();
            return Ok(());
        }

        let mut ids: Vec<_> = self
            .inactive
            .iter()
            .map(|buffer| buffer.buffer_id)
            .chain(std::iter::once(self.active.buffer_id))
            .collect();
        ids.sort_by_key(|id| id.get());
        let current = ids
            .iter()
            .position(|id| *id == self.active.buffer_id)
            .unwrap_or(0);
        let wanted = match action {
            BufferAction::Next => ids[(current + 1) % ids.len()],
            BufferAction::Previous => ids[(current + ids.len() - 1) % ids.len()],
            BufferAction::First => ids[0],
            BufferAction::Last => ids[ids.len() - 1],
            BufferAction::Select => {
                let Some(target) = target else {
                    self.message = format!(
                        "buffer {}: {}",
                        self.active.buffer_id.get(),
                        self.active.name()
                    );
                    return Ok(());
                };
                let numeric = target.parse::<u64>().ok();
                self.inactive
                    .iter()
                    .chain(std::iter::once(&self.active))
                    .find(|buffer| {
                        numeric == Some(buffer.buffer_id.get()) || buffer.name().contains(target)
                    })
                    .map(|buffer| buffer.buffer_id)
                    .ok_or_else(|| anyhow!("no matching buffer: {target}"))?
            }
            BufferAction::Delete => unreachable!(),
        };
        if wanted != self.active.buffer_id {
            let index = self
                .inactive
                .iter()
                .position(|buffer| buffer.buffer_id == wanted)
                .ok_or_else(|| anyhow!("buffer disappeared"))?;
            self.switch_to_inactive(index)?;
        }
        Ok(())
    }

    fn tab_command(&mut self, action: TabAction, path: Option<&str>) -> Result<()> {
        match action {
            TabAction::New => {
                if let Some(path) = path {
                    self.open_buffer(Path::new(path))?;
                }
                self.views.new_tab(self.active.buffer_id)?;
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

    fn save_all(&mut self) -> Result<()> {
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
        self.message = format!("{saved} buffer(s) written");
        Ok(())
    }

    fn autosave_active_if_named(&mut self) -> Result<()> {
        if self.active.editor.is_dirty()
            && !self.active.editor.is_read_only()
            && self.active.document.presentation_path().is_some()
        {
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

    fn write_range(&mut self, range: &ExRange, path: &Path) -> Result<()> {
        let range = self.resolve_byte_range(Some(range))?;
        let contents = self.active.editor.contents();
        let selected = contents
            .get(range)
            .ok_or_else(|| anyhow!("resolved write range is not on UTF-8 boundaries"))?;
        let (mut target, opened) = LocalDocument::open_or_new(path)
            .with_context(|| format!("open range destination {}", path.display()))?;
        if opened.read_only {
            bail!("range destination {} is not editable UTF-8", path.display());
        }
        let report = target.save(selected)?;
        self.message = format!(
            "{} bytes written to {}",
            report.bytes_written,
            path.display()
        );
        Ok(())
    }

    fn grep(&mut self, pattern: &str, paths: &[Box<str>]) -> Result<()> {
        let root = self.workspace_root();
        self.populate_grep_results(pattern, paths, &root)?;
        self.message = format!("{} grep result(s)", self.quickfix.len());
        Ok(())
    }

    fn populate_grep_results(
        &mut self,
        pattern: &str,
        paths: &[Box<str>],
        root: &Path,
    ) -> Result<()> {
        if pattern.is_empty() {
            self.quickfix.clear();
            return Ok(());
        }
        let mut command = Command::new("rg");
        command
            .current_dir(root)
            .arg("--vimgrep")
            .arg("--")
            .arg(pattern);
        if paths.is_empty() {
            command.arg(".");
        } else {
            command.args(paths.iter().map(AsRef::<str>::as_ref));
        }
        let output = command.output().context("run native rg search")?;
        if !output.status.success() && output.status.code() != Some(1) {
            bail!(
                "rg failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
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

    fn workspace_root(&self) -> PathBuf {
        let start = self
            .active
            .document
            .presentation_path()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let root = Command::new("git")
            .arg("-C")
            .arg(&start)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|root| PathBuf::from(root.trim()))
            .filter(|root| !root.as_os_str().is_empty())
            .unwrap_or(start);
        std::fs::canonicalize(&root).unwrap_or(root)
    }

    fn lsp_root(&self) -> PathBuf {
        self.root_workspace.clone()
    }

    fn refresh_diagnostics(&mut self) -> Result<()> {
        let Some(path) = self
            .active
            .document
            .presentation_path()
            .map(Path::to_path_buf)
        else {
            self.diagnostics.clear();
            return Ok(());
        };
        let Some(invocation) = diagnostic_invocation(&path, &self.workspace_root()) else {
            self.diagnostics.clear();
            return Ok(());
        };
        if !executable_exists(&invocation.program) {
            self.diagnostics.clear();
            self.message = format!("diagnostic tool {} is not installed", invocation.program);
            return Ok(());
        }
        let output = Command::new(&invocation.program)
            .args(&invocation.arguments)
            .current_dir(&invocation.directory)
            .output()
            .with_context(|| format!("run diagnostics with {}", invocation.program))?;
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        self.diagnostics = combined
            .lines()
            .filter_map(|line| parse_diagnostic_line(line, &invocation.directory))
            .collect();
        if self.diagnostics.is_empty() && !output.status.success() {
            let detail = combined
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("diagnostic command failed")
                .trim();
            self.diagnostics.push(DiagnosticEntry {
                path,
                line: 1,
                column: 1,
                severity: DiagnosticSeverity::Error,
                message: detail.to_owned(),
            });
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

    fn show_cursor_diagnostic(&mut self) -> Result<()> {
        self.refresh_diagnostics()?;
        let path = self.active.document.presentation_path();
        let cursor_line = self.active.editor.cursor_line_column().0 + 1;
        let diagnostic = self
            .diagnostics
            .iter()
            .filter(|entry| path.is_some_and(|path| same_path(path, &entry.path)))
            .min_by_key(|entry| entry.line.abs_diff(cursor_line))
            .map_or_else(
                || "no diagnostics".to_owned(),
                |entry| {
                    format!(
                        "{} {}:{}: {}",
                        entry.severity.label(),
                        entry.line,
                        entry.column,
                        entry.message
                    )
                },
            );
        if diagnostic == "no diagnostics" {
            self.popup = None;
            self.popup_deadline = None;
            self.message = diagnostic;
        } else {
            self.popup = Some(TextPopup {
                title: "diagnostic".into(),
                text: diagnostic.into(),
                scroll: 0,
                decorations: Vec::new(),
            });
            self.popup_deadline = None;
            self.message.clear();
        }
        Ok(())
    }

    fn move_diagnostic(&mut self, direction: isize) -> Result<()> {
        self.refresh_diagnostics()?;
        let Some(path) = self
            .active
            .document
            .presentation_path()
            .map(Path::to_path_buf)
        else {
            self.message = "no diagnostics".to_owned();
            return Ok(());
        };
        let current = self.active.editor.cursor_line_column().0 + 1;
        let entries = self
            .diagnostics
            .iter()
            .filter(|entry| same_path(&entry.path, &path))
            .collect::<Vec<_>>();
        let selected = if direction < 0 {
            entries
                .iter()
                .rev()
                .find(|entry| entry.line < current)
                .or_else(|| entries.last())
        } else {
            entries
                .iter()
                .find(|entry| entry.line > current)
                .or_else(|| entries.first())
        };
        let Some(entry) = selected.copied() else {
            self.message = "no diagnostics".to_owned();
            return Ok(());
        };
        let line_start = self
            .active
            .editor
            .text()
            .byte_of_line(entry.line.saturating_sub(1));
        self.active
            .editor
            .set_cursor(line_start.saturating_add(entry.column.saturating_sub(1)));
        self.message = format!("{}: {}", entry.severity.label(), entry.message);
        Ok(())
    }

    fn active_git_file(&self) -> Result<(PathBuf, PathBuf, PathBuf)> {
        let path = self
            .active
            .document
            .presentation_path()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("Git action needs a named buffer"))?;
        let root = git_root_for(&path)?;
        let relative = path
            .strip_prefix(&root)
            .with_context(|| format!("{} is outside {}", path.display(), root.display()))?
            .to_path_buf();
        Ok((root, relative, path))
    }

    fn active_git_patch(&self) -> Result<(PathBuf, Vec<u8>)> {
        let (root, relative, _) = self.active_git_file()?;
        if !git_path_tracked(&root, &relative)? {
            let output = Command::new("git")
                .current_dir(&root)
                .args(["add", "--intent-to-add", "--"])
                .arg(&relative)
                .output()
                .context("mark untracked file as intent-to-add")?;
            if !output.status.success() {
                bail!(
                    "git add --intent-to-add: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        let before = git_index_contents(&root, &relative)?;
        let after = self.active.editor.contents();
        let patch = make_git_patch(&root, &relative, &before, &after)?;
        Ok((root, patch))
    }

    fn selected_git_patch(&self) -> Result<(PathBuf, Vec<u8>)> {
        let (root, patch) = self.active_git_patch()?;
        let (cursor_line, _) = self.active.editor.cursor_line_column();
        let line_range =
            matches!(self.active.editor.mode(), Mode::Visual | Mode::VisualLine).then(|| {
                let region = self.active.editor.selection_byte_range();
                let start = self.active.editor.text().line_of_byte(region.start) + 1;
                let end = self
                    .active
                    .editor
                    .text()
                    .line_of_byte(region.end.saturating_sub(1))
                    + 1;
                start..end.saturating_add(1)
            });
        let selected = select_git_hunk(&patch, cursor_line + 1, line_range.as_ref())?;
        Ok((root, selected))
    }

    fn git_stage_hunk(&mut self) -> Result<()> {
        let (root, patch) = self.selected_git_patch()?;
        git_apply_patch(&root, &patch, true, false)?;
        self.last_staged_patch = Some(patch);
        self.refresh_active_git_baseline();
        self.message = "staged hunk".to_owned();
        Ok(())
    }

    fn git_reset_hunk(&mut self) -> Result<()> {
        let (root, relative, _) = self.active_git_file()?;
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
        let after_range =
            byte_range_of_lines(&after, hunk.after.start as usize..hunk.after.end as usize);
        let before_range = byte_range_of_lines(
            &before,
            hunk.before.start as usize..hunk.before.end as usize,
        );
        let replacement = before.get(before_range).unwrap_or_default().to_owned();
        let transaction = Transaction::new(
            self.active.editor.revision(),
            vec![Edit::new(after_range, replacement)],
        )?;
        self.active.editor.apply_transaction(transaction.clone())?;
        self.after_transaction(Some(transaction));
        self.message = "reset hunk".to_owned();
        Ok(())
    }

    fn git_stage_buffer(&mut self) -> Result<()> {
        let (root, patch) = self.active_git_patch()?;
        if patch.is_empty() {
            self.message = "buffer has no changes".to_owned();
            return Ok(());
        }
        git_apply_patch(&root, &patch, true, false)?;
        self.last_staged_patch = Some(patch);
        self.refresh_active_git_baseline();
        self.message = "staged buffer".to_owned();
        Ok(())
    }

    fn git_undo_stage_hunk(&mut self) -> Result<()> {
        let Some(patch) = self.last_staged_patch.take() else {
            self.message = "no staged hunk to undo".to_owned();
            return Ok(());
        };
        let (root, _, _) = self.active_git_file()?;
        git_apply_patch(&root, &patch, true, true)?;
        self.refresh_active_git_baseline();
        self.message = "undid staged hunk".to_owned();
        Ok(())
    }

    fn git_preview_hunk(&mut self) -> Result<()> {
        let (_, patch) = self.selected_git_patch()?;
        self.popup = Some(TextPopup {
            title: "Git hunk".into(),
            text: String::from_utf8_lossy(&patch).into_owned().into(),
            scroll: 0,
            decorations: Vec::new(),
        });
        self.popup_deadline = None;
        self.message.clear();
        Ok(())
    }

    fn git_blame_line(&mut self) -> Result<()> {
        let (root, relative, _) = self.active_git_file()?;
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
            bail!(
                "git blame: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        self.popup = Some(TextPopup {
            title: "Git blame".into(),
            text: String::from_utf8_lossy(&output.stdout).trim().into(),
            scroll: 0,
            decorations: Vec::new(),
        });
        self.popup_deadline = None;
        self.message.clear();
        Ok(())
    }

    fn git_diff_index(&mut self) -> Result<()> {
        let (root, relative, _) = self.active_git_file()?;
        let root = root.to_string_lossy().into_owned().into_boxed_str();
        let relative = relative.to_string_lossy().into_owned().into_boxed_str();
        self.open_terminal(
            Some("git"),
            &[
                "-C".into(),
                root,
                "--no-pager".into(),
                "diff".into(),
                "--".into(),
                relative,
            ],
        )
    }

    fn move_git_hunk(&mut self, direction: isize) -> Result<()> {
        let (root, relative, _) = self.active_git_file()?;
        let before = git_index_contents(&root, &relative)?;
        let after = self.active.editor.contents();
        let hunks = git_hunks(&before, &after);
        let current = u32::try_from(self.active.editor.cursor_line_column().0).unwrap_or(u32::MAX);
        let selected = if direction < 0 {
            hunks
                .iter()
                .rev()
                .find(|hunk| hunk.after.start < current)
                .or_else(|| hunks.last())
        } else {
            hunks
                .iter()
                .find(|hunk| hunk.after.start > current)
                .or_else(|| hunks.first())
        };
        let Some(hunk) = selected else {
            self.message = "no Git hunks".to_owned();
            return Ok(());
        };
        self.active.editor.set_cursor(
            self.active
                .editor
                .text()
                .byte_of_line(hunk.after.start as usize),
        );
        self.message = format!(
            "hunk -{},{} +{},{}",
            hunk.before.start + 1,
            hunk.before.end.saturating_sub(hunk.before.start),
            hunk.after.start + 1,
            hunk.after.end.saturating_sub(hunk.after.start)
        );
        Ok(())
    }

    fn refresh_active_git_baseline(&mut self) {
        self.active.git_index_text = self
            .active_git_file()
            .ok()
            .and_then(|(root, relative, _)| git_index_contents(&root, &relative).ok());
    }

    fn toggle_breakpoint(&mut self, condition: Option<String>) {
        let Some(path) = self
            .active
            .document
            .presentation_path()
            .map(Path::to_path_buf)
        else {
            self.message = "breakpoints need a named buffer".to_owned();
            return;
        };
        let line = self.active.editor.cursor_line_column().0 + 1;
        let file = self.breakpoints.entry(path.clone()).or_default();
        if file.remove(&line).is_some() {
            self.message = format!("removed breakpoint at {}:{line}", path.display());
        } else {
            let label = condition
                .as_deref()
                .filter(|value| !value.is_empty())
                .map_or_else(String::new, |value| format!(" if {value}"));
            file.insert(line, condition.filter(|value| !value.is_empty()));
            self.message = format!("breakpoint at {}:{line}{label}", path.display());
        }
    }

    fn debug_overlay(&self) -> DebugOverlay {
        let breakpoints = self
            .breakpoints
            .iter()
            .flat_map(|(path, lines)| {
                lines.iter().map(move |(line, condition)| {
                    format!(
                        "● {}:{line}{}",
                        path.display(),
                        condition
                            .as_deref()
                            .map_or_else(String::new, |condition| format!(" if {condition}"))
                    )
                })
            })
            .collect::<Vec<_>>()
            .join("\n");
        let (line, column) = self.active.editor.cursor_line_column();
        let stacks = format!(
            "▾ current thread\n  {}:{}:{}",
            self.active.name(),
            line + 1,
            column + 1
        );
        let repl = self
            .terminal
            .as_ref()
            .map(|_| self.terminal_frame().text.into_string())
            .unwrap_or_else(|| "Press <Space>dl to start the debugger REPL".to_owned());
        DebugOverlay {
            scopes: "▸ Locals\n▸ Arguments\n▸ Registers".into(),
            breakpoints: if breakpoints.is_empty() {
                "No breakpoints".into()
            } else {
                breakpoints.into()
            },
            stacks: stacks.into(),
            watches: "Add watches from the REPL".into(),
            repl: repl.into(),
            console: if self.message.is_empty() {
                "Debugger console".into()
            } else {
                self.message.clone().into()
            },
        }
    }

    fn open_debug_repl(&mut self) -> Result<()> {
        let path = self
            .active
            .document
            .presentation_path()
            .map(|path| path.to_string_lossy().into_owned());
        let language = language_bundle(self.active.document.presentation_path()).language_id;
        let (program, arguments): (&str, Vec<Box<str>>) = match language.as_ref() {
            "python" if executable_exists("python3") => (
                "python3",
                path.map_or_else(
                    || vec!["-m".into(), "pdb".into()],
                    |path| vec!["-m".into(), "pdb".into(), path.into_boxed_str()],
                ),
            ),
            "go" if executable_exists("dlv") => ("dlv", vec!["debug".into()]),
            "rust" | "c" | "cpp" if executable_exists("lldb") => (
                "lldb",
                path.map_or_else(Vec::new, |path| vec![path.into_boxed_str()]),
            ),
            _ => {
                self.message = format!("no installed debugger for {language}");
                return Ok(());
            }
        };
        self.open_terminal(Some(program), &arguments)
    }

    fn run_debug_action(&mut self, action: &str) -> Result<()> {
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
            self.terminal_focused = true;
            self.message = format!("debug {action}");
        }
        Ok(())
    }

    fn open_hoogle(&mut self) -> Result<()> {
        let query = self.word_under_cursor().unwrap_or_default();
        if query.is_empty() {
            self.message = "no Haskell identifier under cursor".to_owned();
            return Ok(());
        }
        let url = format!("https://hoogle.haskell.org/?hoogle={}", url_encode(&query));
        let program = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        Command::new(program)
            .arg(&url)
            .spawn()
            .with_context(|| format!("open {url}"))?;
        self.message = format!("Hoogle: {query}");
        Ok(())
    }

    fn hoogle_signature(&mut self) -> Result<()> {
        let query = self.word_under_cursor().unwrap_or_default();
        if query.is_empty() {
            self.message = "no Haskell identifier under cursor".to_owned();
            return Ok(());
        }
        if !executable_exists("hoogle") {
            self.message = "hoogle is not installed".to_owned();
            return Ok(());
        }
        let output = Command::new("hoogle")
            .args(["--count=1", "--color=false", "--"])
            .arg(&query)
            .output()
            .context("run Hoogle")?;
        self.message = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("no Hoogle result")
            .to_owned();
        Ok(())
    }

    fn open_haskell_repl(&mut self, package: bool) -> Result<()> {
        if package && executable_exists("cabal") {
            return self.open_terminal(Some("cabal"), &["repl".into()]);
        }
        let arguments = if package {
            Vec::new()
        } else {
            self.active
                .document
                .presentation_path()
                .map(|path| vec![path.to_string_lossy().into_owned().into_boxed_str()])
                .unwrap_or_default()
        };
        self.open_terminal(Some("ghci"), &arguments)
    }

    fn quit_repl(&mut self) -> Result<()> {
        if let Some(terminal) = &mut self.terminal {
            terminal.send_input(b":quit\n")?;
            self.message = "REPL quit requested".to_owned();
        } else {
            self.message = "no active REPL".to_owned();
        }
        Ok(())
    }

    fn evaluate_in_repl(&mut self) -> Result<()> {
        let text = self.active.editor.contents();
        let expression = if matches!(self.active.editor.mode(), Mode::Visual | Mode::VisualLine) {
            text.get(self.active.editor.selection_byte_range())
                .unwrap_or_default()
                .to_owned()
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
            self.terminal_focused = true;
        }
        Ok(())
    }

    fn active_language_server(&self) -> Option<LanguageServerInvocation> {
        language_server_invocation(self.active.document.presentation_path())
            .filter(|server| executable_exists(&server.program))
    }

    fn begin_lsp_start(&mut self) {
        // A startup owns a live child as soon as its worker is spawned. Buffer
        // navigation must wait for it and attach the new document afterward,
        // never drop the receiver and implicitly kill/restart that child.
        if self.lsp_background.is_some() || self.lsp_start.is_some() {
            return;
        }
        match self.reuse_lsp_for_active() {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                // Protocol failure is the one case where the selected client
                // is no longer safe to retain and should be restarted.
                self.lsp = None;
                self.show_error(format!("language server: {error}"));
            }
        }
        // Visiting a help, generated, or otherwise unsupported buffer must
        // not tear down the root workspace's already-running server.
        if self.active_language_server().is_none() {
            if let Some(lsp) = &mut self.lsp {
                lsp.semantic_due = None;
            }
            return;
        }
        if let Some(lsp) = self.lsp.take() {
            self.parked_lsps.push(lsp);
        }
        self.pending_lsp_hover = None;
        #[cfg(test)]
        return;
        #[cfg(not(test))]
        {
            let Some(server) = self.active_language_server() else {
                return;
            };
            let Some(path) = self
                .active
                .document
                .presentation_path()
                .map(Path::to_path_buf)
            else {
                return;
            };
            let root = self.lsp_root();
            let document_id = self.active.document_id;
            let revision = self.active.editor.revision();
            let text = self.active.editor.contents();
            let server_state = server.clone();
            let root_state = root.clone();
            let environment = env::vars()
                .map(|(name, value)| (name.into_boxed_str(), value.into_boxed_str()))
                .collect::<BTreeMap<_, _>>();
            let (sender, receiver) = mpsc::channel();
            match thread::Builder::new()
                .name("wren-lsp-start".to_owned())
                .spawn(move || {
                    let result =
                        spawn_lsp_client(&server, &path, &root, revision, &text, environment)
                            .map(|(client, uri, semantic_legend)| {
                                let open_documents = BTreeMap::from([(
                                    document_id,
                                    LspOpenDocument {
                                        uri: uri.clone(),
                                        revision,
                                    },
                                )]);
                                PersistentLsp {
                                    document_id,
                                    revision,
                                    uri,
                                    client,
                                    server: server_state,
                                    root: root_state,
                                    open_documents,
                                    semantic_due: semantic_legend
                                        .as_ref()
                                        .map(|_| Instant::now() + Duration::from_millis(750)),
                                    semantic_legend,
                                }
                            })
                            .map_err(|error| error.to_string());
                    let _ = sender.send(result);
                }) {
                Ok(_) => self.lsp_start = Some(receiver),
                Err(error) => self.show_error(format!("start language server: {error}")),
            }
        }
    }

    fn poll_lsp_start(&mut self) -> Result<bool> {
        let Some(receiver) = &self.lsp_start else {
            return Ok(false);
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return Ok(false),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.lsp_start = None;
                self.show_error("language server startup worker disconnected");
                return Ok(true);
            }
        };
        self.lsp_start = None;
        let ready_for_active = match result {
            Ok(lsp) => {
                self.lsp = Some(lsp);
                match self.reuse_lsp_for_active() {
                    Ok(true) => true,
                    Ok(false) => {
                        if language_server_invocation(self.active.document.presentation_path())
                            .is_none()
                        {
                            // The completed root server remains owned by the
                            // workspace while a non-LSP buffer is active.
                            if let Some(lsp) = &mut self.lsp {
                                lsp.semantic_due = None;
                            }
                            self.lsp_semantic_dirty = false;
                            false
                        } else {
                            self.begin_lsp_start();
                            self.lsp_ready_for_active()
                        }
                    }
                    Err(error) => {
                        self.lsp = None;
                        self.show_error(format!("language server: {error}"));
                        self.begin_lsp_start();
                        false
                    }
                }
            }
            Err(error) => {
                self.show_error(format!("language server: {error}"));
                return Ok(true);
            }
        };
        self.lsp_semantic_dirty = false;
        if !ready_for_active {
            return Ok(true);
        }
        if let Some(method) = self.pending_lsp_location.take() {
            if let Err(error) = self.start_lsp_location_request(&method) {
                self.show_error(format!("{method}: {error}"));
            }
        } else if let Some(method) = self.pending_lsp_hover.take()
            && let Err(error) = self.lsp_hover_ready(&method)
        {
            self.show_error(format!("{method}: {error}"));
        }
        Ok(true)
    }

    fn lsp_ready_for_active(&self) -> bool {
        self.lsp
            .as_ref()
            .is_some_and(|lsp| lsp.document_id == self.active.document_id)
    }

    fn reuse_lsp_for_active(&mut self) -> Result<bool> {
        let Some(server) = language_server_invocation(self.active.document.presentation_path())
        else {
            return Ok(false);
        };
        let root = self.lsp_root();
        let current_matches = self
            .lsp
            .as_ref()
            .is_some_and(|lsp| lsp.server == server && lsp.root == root);
        if !current_matches {
            let Some(index) = self
                .parked_lsps
                .iter()
                .position(|lsp| lsp.server == server && lsp.root == root)
            else {
                return Ok(false);
            };
            let replacement = self.parked_lsps.swap_remove(index);
            if let Some(current) = self.lsp.replace(replacement) {
                self.parked_lsps.push(current);
            }
        }
        let Some(lsp) = &mut self.lsp else {
            return Ok(false);
        };
        let document_id = self.active.document_id;
        let revision = self.active.editor.revision();
        let uri = file_uri(
            self.active
                .document
                .presentation_path()
                .ok_or_else(|| anyhow!("LSP action needs a named buffer"))?,
        );
        if let Some(open) = lsp.open_documents.get(&document_id) {
            lsp.document_id = document_id;
            lsp.revision = open.revision;
            lsp.uri.clone_from(&open.uri);
        } else {
            lsp.client.did_open(
                &uri,
                &server.language_id,
                i64::try_from(revision.get()).unwrap_or(i64::MAX),
                &self.active.editor.contents(),
            )?;
            lsp.document_id = document_id;
            lsp.revision = revision;
            lsp.uri.clone_from(&uri);
            lsp.open_documents
                .insert(document_id, LspOpenDocument { uri, revision });
        }
        lsp.semantic_due = lsp
            .semantic_legend
            .as_ref()
            .map(|_| Instant::now() + Duration::from_millis(750));
        Ok(true)
    }

    fn start_lsp(&self) -> Result<(LspClient, String)> {
        let server = self.active_language_server().ok_or_else(|| {
            let language = language_bundle(self.active.document.presentation_path()).language_id;
            anyhow!("no installed language server for {language}")
        })?;
        let path = self
            .active
            .document
            .presentation_path()
            .ok_or_else(|| anyhow!("LSP action needs a named buffer"))?;
        let root = self.lsp_root();
        let environment = env::vars()
            .map(|(name, value)| (name.into_boxed_str(), value.into_boxed_str()))
            .collect();
        spawn_lsp_client(
            &server,
            path,
            &root,
            self.active.editor.revision(),
            &self.active.editor.contents(),
            environment,
        )
        .map(|(client, uri, _)| (client, uri))
    }

    fn lsp_position(&self) -> LspPosition {
        let text = self.active.editor.contents();
        let cursor = self.active.editor.primary_cursor().min(text.len());
        let line = self.active.editor.text().line_of_byte(cursor);
        let start = self.active.editor.text().byte_of_line(line);
        let character = text[start..cursor].encode_utf16().count();
        LspPosition {
            line: u32::try_from(line).unwrap_or(u32::MAX),
            character: u32::try_from(character).unwrap_or(u32::MAX),
        }
    }

    fn lsp_request_at_cursor(
        &mut self,
        method: &str,
        extra: serde_json::Value,
    ) -> Result<serde_json::Value> {
        if self.lsp_background.is_some() {
            bail!("language server is completing another request");
        }
        let document_id = self.active.document_id;
        let revision = self.active.editor.revision();
        let text = self.active.editor.contents();
        let position = self.lsp_position();
        let persistent = self
            .lsp
            .as_ref()
            .is_some_and(|lsp| lsp.document_id == document_id);
        if persistent {
            let lsp = self.lsp.as_mut().expect("persistent LSP was checked");
            if lsp.revision != revision {
                lsp.client.did_change_full(
                    &lsp.uri,
                    i64::try_from(revision.get()).unwrap_or(i64::MAX),
                    &text,
                )?;
                lsp.revision = revision;
                if let Some(open) = lsp.open_documents.get_mut(&document_id) {
                    open.revision = revision;
                }
            }
            let mut parameters = serde_json::json!({
                "textDocument": {"uri": lsp.uri},
                "position": position,
            });
            if let (Some(target), Some(extra)) = (parameters.as_object_mut(), extra.as_object()) {
                target.extend(extra.clone());
            }
            return lsp.client.request(method, parameters).map_err(Into::into);
        }
        let (mut client, uri) = self.start_lsp()?;
        let mut parameters = serde_json::json!({
            "textDocument": {"uri": uri},
            "position": position,
        });
        if let (Some(target), Some(extra)) = (parameters.as_object_mut(), extra.as_object()) {
            target.extend(extra.clone());
        }
        client.request(method, parameters).map_err(Into::into)
    }

    fn request_lsp_completion(&mut self) -> Result<Option<LspCompletion>> {
        if self.active_language_server().is_none() {
            return Ok(None);
        }
        let result = self.lsp_request_at_cursor(
            "textDocument/completion",
            serde_json::json!({"context": {"triggerKind": 1}}),
        )?;
        let items = result
            .as_array()
            .or_else(|| result.get("items").and_then(serde_json::Value::as_array));
        let Some(items) = items else {
            return Ok(None);
        };
        let candidates = items
            .iter()
            .filter_map(|item| {
                let label = item.get("label")?.as_str()?;
                let raw_insert = item
                    .pointer("/textEdit/newText")
                    .or_else(|| item.get("insertText"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(label);
                let snippet = item
                    .get("insertTextFormat")
                    .and_then(serde_json::Value::as_u64)
                    == Some(2);
                Some(CompletionCandidate {
                    label: label.into(),
                    insert: if snippet {
                        expand_lsp_snippet(raw_insert).into_boxed_str()
                    } else {
                        raw_insert.into()
                    },
                    source: "lsp".into(),
                    detail: item
                        .get("detail")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("LSP")
                        .into(),
                    documentation: item
                        .get("documentation")
                        .map(render_lsp_text)
                        .unwrap_or_default()
                        .into_boxed_str(),
                    replace: None,
                    snippet: snippet.then(|| raw_insert.into()),
                })
            })
            .take(256)
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(None);
        }
        let text = self.active.editor.contents();
        let cursor = self.active.editor.primary_cursor().min(text.len());
        let mut start = cursor;
        while start > 0 {
            let previous = text[..start]
                .char_indices()
                .next_back()
                .map_or(0, |(byte, _)| byte);
            let character = text[previous..start].chars().next().unwrap_or(' ');
            if !character.is_alphanumeric() && character != '_' {
                break;
            }
            start = previous;
        }
        Ok(Some(LspCompletion {
            revision: self.active.editor.revision(),
            replace: start..cursor,
            candidates,
        }))
    }

    fn lsp_location(&mut self, method: &str) -> Result<()> {
        if self.lsp_background.is_some() {
            self.pending_lsp_location = Some(method.to_owned());
            self.message = "language server busy; definition queued".to_owned();
            return Ok(());
        }
        if self
            .lsp
            .as_ref()
            .is_none_or(|lsp| lsp.document_id != self.active.document_id)
        {
            if self.lsp_start.is_none() {
                self.begin_lsp_start();
            }
            if self.lsp_start.is_some() {
                self.pending_lsp_location = Some(method.to_owned());
                self.message = "language server starting; definition queued".to_owned();
                return Ok(());
            }
        }
        self.start_lsp_location_request(method)
    }

    fn start_lsp_location_request(&mut self, method: &str) -> Result<()> {
        let Some(mut lsp) = self.lsp.take() else {
            let language = language_bundle(self.active.document.presentation_path()).language_id;
            bail!("no ready language server for {language}");
        };
        let document_id = self.active.document_id;
        let revision = self.active.editor.revision();
        let text = self.active.editor.contents().into_boxed_str();
        let position = self.lsp_position();
        let method = method.to_owned();
        let operation_method = method.clone();
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("wren-lsp-definition".to_owned())
            .spawn(move || {
                let outcome = (|| -> Result<serde_json::Value, String> {
                    if lsp.revision != revision {
                        lsp.client
                            .did_change_full(
                                &lsp.uri,
                                i64::try_from(revision.get()).unwrap_or(i64::MAX),
                                &text,
                            )
                            .map_err(|error| error.to_string())?;
                        lsp.revision = revision;
                        if let Some(open) = lsp.open_documents.get_mut(&document_id) {
                            open.revision = revision;
                        }
                    }
                    lsp.client
                        .request(
                            &method,
                            serde_json::json!({
                                "textDocument": {"uri": lsp.uri},
                                "position": position,
                            }),
                        )
                        .map_err(|error| error.to_string())
                })();
                let _ = sender.send(LspBackgroundResult {
                    lsp,
                    operation: LspBackgroundOperation::Location {
                        method: operation_method,
                    },
                    outcome,
                });
            })
            .context("spawn asynchronous definition request")?;
        self.lsp_background = Some(receiver);
        self.message = "finding definition…".to_owned();
        Ok(())
    }

    fn finish_lsp_location(&mut self, method: &str, result: &serde_json::Value) -> Result<()> {
        let locations = parse_lsp_locations(result)?;
        if locations.is_empty() {
            self.message = format!("{method}: no location");
            return Ok(());
        }
        self.quickfix = locations;
        if self.quickfix.len() == 1 {
            let entry = self.quickfix[0].clone();
            self.navigate_to_entry(&entry)?;
            self.message = entry.display();
        } else {
            self.start_location_picker(PickerSource::Jumps, "")?;
        }
        Ok(())
    }

    fn poll_lsp_background(&mut self) -> Result<bool> {
        let Some(receiver) = &self.lsp_background else {
            return Ok(false);
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return Ok(false),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.lsp_background = None;
                self.show_error("language server request worker disconnected");
                self.begin_lsp_start();
                return Ok(true);
            }
        };
        self.lsp_background = None;
        self.lsp = Some(result.lsp);
        if self.lsp_semantic_dirty {
            if let Some(lsp) = &mut self.lsp
                && lsp.semantic_legend.is_some()
            {
                lsp.semantic_due = Some(Instant::now() + Duration::from_millis(750));
            }
            self.lsp_semantic_dirty = false;
        }
        match (result.operation, result.outcome) {
            (LspBackgroundOperation::Location { method }, Ok(value)) => {
                if let Err(error) = self.finish_lsp_location(&method, &value) {
                    self.show_error(format!("{method}: {error}"));
                }
            }
            (
                LspBackgroundOperation::Semantic {
                    buffer_id,
                    revision,
                    text,
                    legend,
                },
                Ok(value),
            ) => {
                if self
                    .buffer(buffer_id)
                    .is_some_and(|buffer| buffer.editor.revision() == revision)
                {
                    let spans = parse_semantic_tokens(&text, &value, &legend)
                        .into_iter()
                        .map(|span| provider_decoration(span, self.theme))
                        .collect();
                    self.semantic_decorations
                        .insert(buffer_id, BufferDecorations { revision, spans });
                }
            }
            (operation, Err(error)) => {
                let label = match operation {
                    LspBackgroundOperation::Location { method } => method,
                    LspBackgroundOperation::Semantic { .. } => {
                        "textDocument/semanticTokens/full".to_owned()
                    }
                };
                self.show_error(format!("{label}: {error}"));
            }
        }
        self.begin_lsp_start();
        if !self.lsp_ready_for_active() {
            return Ok(true);
        }
        if let Some(method) = self.pending_lsp_location.take() {
            if let Err(error) = self.start_lsp_location_request(&method) {
                self.show_error(format!("{method}: {error}"));
            }
        } else if let Some(method) = self.pending_lsp_hover.take()
            && let Err(error) = self.lsp_hover_ready(&method)
        {
            self.show_error(format!("{method}: {error}"));
        }
        Ok(true)
    }

    fn poll_lsp_semantic_due(&mut self) -> Result<bool> {
        if self.lsp_background.is_some()
            || self.lsp_start.is_some()
            || self.pending_lsp_location.is_some()
            || self
                .lsp
                .as_ref()
                .is_none_or(|lsp| lsp.document_id != self.active.document_id)
        {
            return Ok(false);
        }
        let due = self.lsp.as_ref().and_then(|lsp| lsp.semantic_due);
        if due.is_none_or(|due| Instant::now() < due) {
            return Ok(false);
        }
        let Some(mut lsp) = self.lsp.take() else {
            return Ok(false);
        };
        let Some(legend) = lsp.semantic_legend.clone() else {
            lsp.semantic_due = None;
            self.lsp = Some(lsp);
            return Ok(false);
        };
        lsp.semantic_due = None;
        let document_id = self.active.document_id;
        let buffer_id = self.active.buffer_id;
        let revision = self.active.editor.revision();
        let text = self.active.editor.contents().into_boxed_str();
        let request_text = text.clone();
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("wren-lsp-semantic".to_owned())
            .spawn(move || {
                let outcome = (|| -> Result<serde_json::Value, String> {
                    if lsp.revision != revision {
                        lsp.client
                            .did_change_full(
                                &lsp.uri,
                                i64::try_from(revision.get()).unwrap_or(i64::MAX),
                                &request_text,
                            )
                            .map_err(|error| error.to_string())?;
                        lsp.revision = revision;
                        if let Some(open) = lsp.open_documents.get_mut(&document_id) {
                            open.revision = revision;
                        }
                    }
                    lsp.client
                        .request(
                            "textDocument/semanticTokens/full",
                            serde_json::json!({"textDocument": {"uri": lsp.uri}}),
                        )
                        .map_err(|error| error.to_string())
                })();
                let _ = sender.send(LspBackgroundResult {
                    lsp,
                    operation: LspBackgroundOperation::Semantic {
                        buffer_id,
                        revision,
                        text,
                        legend,
                    },
                    outcome,
                });
            })
            .context("spawn semantic-token request")?;
        self.lsp_background = Some(receiver);
        Ok(false)
    }

    fn lsp_references(&mut self) -> Result<()> {
        let result = self.lsp_request_at_cursor(
            "textDocument/references",
            serde_json::json!({"context": {"includeDeclaration": true}}),
        )?;
        self.quickfix = parse_lsp_locations(&result)?;
        self.start_location_picker(PickerSource::Jumps, "")
    }

    fn lsp_hover(&mut self, method: &str) -> Result<()> {
        if self.lsp_background.is_some() {
            self.pending_lsp_hover = Some(method.to_owned());
            self.message = "language server busy; hover queued".to_owned();
            return Ok(());
        }
        if self
            .lsp
            .as_ref()
            .is_none_or(|lsp| lsp.document_id != self.active.document_id)
        {
            if self.lsp_start.is_none() {
                self.begin_lsp_start();
            }
            if self.lsp_start.is_some() {
                self.pending_lsp_hover = Some(method.to_owned());
                self.popup = None;
                self.popup_deadline = None;
                self.message.clear();
                return Ok(());
            }
        }
        self.lsp_hover_ready(method)
    }

    fn lsp_hover_ready(&mut self, method: &str) -> Result<()> {
        let result = self.lsp_request_at_cursor(method, serde_json::json!({}))?;
        let rendered = render_lsp_text(&result);
        if rendered.is_empty() {
            self.popup = None;
            self.popup_deadline = None;
            self.message = format!("{method}: no information");
        } else {
            let (text, decorations) = lsp_popup_markdown(&rendered, self.theme);
            self.popup = Some(TextPopup {
                title: "".into(),
                text: text.into(),
                scroll: 0,
                decorations,
            });
            self.popup_deadline = Some(Instant::now() + Duration::from_secs(6));
            self.message.clear();
        }
        Ok(())
    }

    fn rename_symbol(&mut self, new_name: &str) -> Result<()> {
        if new_name.trim().is_empty() {
            self.message = "rename cancelled".to_owned();
            return Ok(());
        }
        let edit = self.lsp_request_at_cursor(
            "textDocument/rename",
            serde_json::json!({"newName": new_name}),
        )?;
        self.apply_lsp_workspace_edit(&edit)?;
        self.message = format!("renamed symbol to {new_name}");
        Ok(())
    }

    fn apply_lsp_workspace_edit(&mut self, workspace_edit: &serde_json::Value) -> Result<()> {
        let mut edits_by_uri: BTreeMap<String, Vec<LspTextEdit>> = BTreeMap::new();
        if let Some(changes) = workspace_edit
            .get("changes")
            .and_then(serde_json::Value::as_object)
        {
            for (uri, edits) in changes {
                edits_by_uri.insert(uri.clone(), serde_json::from_value(edits.clone())?);
            }
        }
        if let Some(changes) = workspace_edit
            .get("documentChanges")
            .and_then(serde_json::Value::as_array)
        {
            for change in changes {
                let Some(uri) = change
                    .pointer("/textDocument/uri")
                    .and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                let edits: Vec<LspTextEdit> = serde_json::from_value(
                    change
                        .get("edits")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                )?;
                edits_by_uri
                    .entry(uri.to_owned())
                    .or_default()
                    .extend(edits);
            }
        }
        for (uri, edits) in edits_by_uri {
            let path = path_from_file_uri(&uri)?;
            self.open_buffer(&path)?;
            let revision = self.active.editor.revision();
            let text = self.active.editor.contents();
            let lowered =
                lower_lsp_text_edits(self.active.document_id, revision, revision, &text, edits)?;
            if lowered.edits.is_empty() {
                continue;
            }
            let transaction = Transaction::new(revision, lowered.edits)?;
            self.active.editor.apply_transaction(transaction.clone())?;
            self.after_transaction(Some(transaction));
        }
        Ok(())
    }

    fn lsp_code_action(&mut self) -> Result<()> {
        let position = self.lsp_position();
        let (mut client, uri) = self.start_lsp()?;
        let result = client.request(
            "textDocument/codeAction",
            serde_json::json!({
                "textDocument": {"uri": uri},
                "range": {"start": position, "end": position},
                "context": {"diagnostics": []}
            }),
        )?;
        let Some(actions) = result.as_array() else {
            self.message = "no code actions".to_owned();
            return Ok(());
        };
        let action = actions
            .iter()
            .find(|action| {
                action
                    .get("isPreferred")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
            })
            .or_else(|| actions.first());
        let Some(action) = action else {
            self.message = "no code actions".to_owned();
            return Ok(());
        };
        if let Some(edit) = action.get("edit") {
            self.apply_lsp_workspace_edit(edit)?;
        }
        if let Some(command) = action.get("command") {
            let (identifier, arguments) = if let Some(identifier) = command.as_str() {
                (
                    Some(identifier),
                    action
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                )
            } else {
                (
                    command.get("command").and_then(serde_json::Value::as_str),
                    command
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                )
            };
            if let Some(identifier) = identifier {
                let _ = client.request(
                    "workspace/executeCommand",
                    serde_json::json!({"command": identifier, "arguments": arguments}),
                )?;
            }
        }
        self.message = action
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || "code action applied".to_owned(),
                |title| title.to_owned(),
            );
        Ok(())
    }

    fn lsp_code_lens(&mut self) -> Result<()> {
        let (mut client, uri) = self.start_lsp()?;
        let result = client.request(
            "textDocument/codeLens",
            serde_json::json!({"textDocument": {"uri": uri}}),
        )?;
        let Some(lens) = result.as_array().and_then(|lenses| lenses.first()) else {
            self.message = "no code lens at buffer".to_owned();
            return Ok(());
        };
        let command = if lens.get("command").is_some() {
            lens.get("command").cloned().unwrap_or_default()
        } else {
            client
                .request("codeLens/resolve", lens.clone())?
                .get("command")
                .cloned()
                .unwrap_or_default()
        };
        let title = command
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("code lens")
            .to_owned();
        if let Some(identifier) = command.get("command").and_then(serde_json::Value::as_str) {
            let _ = client.request(
                "workspace/executeCommand",
                serde_json::json!({
                    "command": identifier,
                    "arguments": command.get("arguments").cloned().unwrap_or_else(|| serde_json::json!([]))
                }),
            )?;
        }
        self.message = title;
        Ok(())
    }

    fn lsp_workspace_folder(&mut self, _method: &str, add: bool) -> Result<()> {
        let folder = self.workspace_root();
        if add {
            if !self
                .workspace_folders
                .iter()
                .any(|path| same_path(path, &folder))
            {
                self.workspace_folders.push(folder.clone());
            }
            self.message = format!("workspace folder added: {}", folder.display());
        } else {
            self.workspace_folders
                .retain(|path| !same_path(path, &folder));
            self.message = format!("workspace folder removed: {}", folder.display());
        }
        Ok(())
    }

    fn list_workspace_folders(&mut self) {
        self.message = if self.workspace_folders.is_empty() {
            "no workspace folders".to_owned()
        } else {
            self.workspace_folders
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(" · ")
        };
    }

    fn execute_cdo(&mut self, command: ExCommand) -> Result<()> {
        let entries = self.quickfix.clone();
        if entries.is_empty() {
            self.message = "quickfix list is empty".to_owned();
            return Ok(());
        }
        for entry in entries {
            self.open_buffer(&entry.path)?;
            self.active.editor.set_cursor(
                self.active
                    .editor
                    .text()
                    .byte_of_line(entry.line.saturating_sub(1)),
            );
            self.execute_ex_command(command.clone())?;
        }
        self.message = "cdo complete".to_owned();
        Ok(())
    }

    fn save(&mut self, path: Option<&Path>) -> Result<()> {
        if self.tasks.is_document_blocked(self.active.document_id) {
            bail!("document has a pending TaskCommand; wait or cancel before saving");
        }
        if self.format_on_save
            && !self.format_disabled.contains(&self.active.document_id)
            && path.is_none()
            && let Err(error) = self.format_active_sync(false)
        {
            self.show_error(format!("format-on-save: {error}"));
        }
        if let Some(wal) = &self.active.wal {
            wal.barrier()
                .context("make recovery WAL durable before save")?;
        }
        let report = match path {
            Some(path) => self
                .active
                .document
                .save_as(path, &self.active.editor.contents()),
            None => self.active.document.save(&self.active.editor.contents()),
        }?;
        self.active.editor.mark_clean();
        self.active.base_hash = report.stamp.content_hash;
        save_undo_state(&mut self.active)?;
        if path.is_some() {
            if let Some(wal) = &self.active.wal {
                wal.clear()
                    .context("compact old recovery WAL after save-as")?;
            }
            self.active.wal = self
                .active
                .document
                .presentation_path()
                .map(LocalWal::for_document)
                .transpose()?
                .map(WalWorker::start);
        }
        if let Some(wal) = &self.active.wal {
            wal.clear().context("compact recovery WAL after save")?;
        }
        self.message = match report.warning {
            Some(SaveWarning::HardLinkReplaced { links }) => format!(
                "{} bytes written; warning: replaced one of {links} hard links",
                report.bytes_written
            ),
            None => format!("{} bytes written", report.bytes_written),
        };
        Ok(())
    }

    fn status(&self) -> String {
        let mode = match self.active.editor.mode() {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Replace => "REPLACE",
            Mode::Visual => "VISUAL",
            Mode::VisualLine => "V-LINE",
        };
        let path = self.active.name();
        let changed = if self.active.editor.is_dirty() {
            " [+]"
        } else {
            ""
        };
        let readonly = if self.active.editor.is_read_only() {
            " [RO]"
        } else {
            ""
        };
        let eol = if self.active.mixed_line_endings {
            " [mixed EOL]"
        } else {
            ""
        };
        let (line, column) = self.active.editor.cursor_line_column();
        let class = match self.active.class {
            DocumentClass::Normal => "",
            DocumentClass::Large => " [large]",
            DocumentClass::Pathological => " [pathological]",
        };
        let detail = if self.message.is_empty() {
            String::new()
        } else {
            format!(" | {}", self.message)
        };
        let tab = self
            .views
            .tabs
            .iter()
            .position(|tab| tab.id == self.views.active_tab)
            .map_or(1, |index| index + 1);
        format!(
            " {mode} | {path}{changed}{readonly}{eol}{class} | {}:{} | b{} t{}/{}{detail}",
            line + 1,
            column + 1,
            self.active.buffer_id.get(),
            tab,
            self.views.tabs.len(),
        )
    }

    fn status_overlay(&self) -> StatusOverlay {
        let mode = match self.active.editor.mode() {
            Mode::Normal => ("NORMAL", self.theme.blue),
            Mode::Insert => ("INSERT", self.theme.green),
            Mode::Replace => ("REPLACE", self.theme.red),
            Mode::Visual => ("VISUAL", self.theme.mauve),
            Mode::VisualLine => ("V-LINE", self.theme.mauve),
        };
        let section_a = CellStyle {
            bold: true,
            foreground: Some(CellColor::Rgb(self.theme.base)),
            background: Some(CellColor::Rgb(mode.1)),
            ..CellStyle::default()
        };
        let section_b = CellStyle {
            bold: true,
            foreground: Some(CellColor::Rgb(self.theme.text)),
            background: Some(CellColor::Rgb(self.theme.surface1)),
            ..CellStyle::default()
        };
        let section_c = CellStyle {
            foreground: Some(CellColor::Rgb(self.theme.text)),
            background: Some(CellColor::Rgb(self.theme.mantle)),
            ..CellStyle::default()
        };
        let path = self.active.name();
        let flags = format!(
            "{}{}{}",
            if self.active.editor.is_dirty() {
                " [+]"
            } else {
                ""
            },
            if self.active.editor.is_read_only() {
                " [RO]"
            } else {
                ""
            },
            if self.active.mixed_line_endings {
                " [mixed EOL]"
            } else {
                ""
            }
        );
        let mut left = vec![StatusSegment {
            text: format!(" {} ", mode.0).into(),
            style: section_a,
        }];
        if let Some(branch) = &self.active.git_branch {
            left.push(StatusSegment {
                text: format!("  {branch} ").into(),
                style: section_b,
            });
        }
        let diagnostic_count = self.active.document.presentation_path().map_or(0, |path| {
            self.diagnostics
                .iter()
                .filter(|diagnostic| same_path(&diagnostic.path, path))
                .count()
        });
        if diagnostic_count > 0 {
            left.push(StatusSegment {
                text: format!("  {diagnostic_count} ").into(),
                style: CellStyle {
                    foreground: Some(CellColor::Rgb(self.theme.yellow)),
                    ..section_b
                },
            });
        }
        left.push(StatusSegment {
            text: format!(" {path}{flags} ").into(),
            style: section_c,
        });
        if !self.message.is_empty() {
            left.push(StatusSegment {
                text: format!(" {} ", self.message).into(),
                style: CellStyle {
                    foreground: Some(CellColor::Rgb(self.theme.subtext0)),
                    ..section_c
                },
            });
        }

        let (line, column) = self.active.editor.cursor_line_column();
        let line_count = self
            .active
            .editor
            .contents()
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1;
        let progress = (line + 1)
            .saturating_mul(100)
            .checked_div(line_count)
            .unwrap_or(100);
        let language = language_bundle(self.active.document.presentation_path()).language_id;
        let right = vec![
            StatusSegment {
                text: format!(" utf-8  unix  {language} ").into(),
                style: section_c,
            },
            StatusSegment {
                text: format!(" {progress}% ").into(),
                style: section_b,
            },
            StatusSegment {
                text: format!(" {}:{} ", line + 1, column + 1).into(),
                style: section_a,
            },
        ];
        StatusOverlay { left, right }
    }

    fn expression_context(&self) -> ExpressionContext {
        let (line, column) = self.active.editor.cursor_line_column();
        let class = match self.active.class {
            DocumentClass::Normal => "normal",
            DocumentClass::Large => "large",
            DocumentClass::Pathological => "pathological",
        };
        ExpressionContext::new()
            .with("cursor.line", Value::Number((line + 1) as f64))
            .with("cursor.column", Value::Number((column + 1) as f64))
            .with("selection.nonempty", Value::Bool(false))
            .with("document.class", Value::String(class.to_owned()))
            .with("remote", Value::Bool(false))
            .with("workspace.trusted", Value::Bool(false))
            .with("os", Value::String(env::consts::OS.to_owned()))
    }

    fn flush_wal(&self) -> Result<()> {
        if let Some(wal) = &self.active.wal {
            wal.barrier().context("flush recovery WAL")?;
        }
        for buffer in &self.inactive {
            if let Some(wal) = &buffer.wal {
                wal.barrier().context("flush recovery WAL")?;
            }
        }
        self.mutations.barrier()?;
        self.client_state_worker
            .barrier(self.client_state.clone())?;
        save_recent_files(&self.recent_files)?;
        Ok(())
    }

    fn start_substitution_task(&mut self, substitute: Substitute) -> Result<()> {
        if self.active_task.is_some() {
            bail!("a TaskCommand is already running");
        }
        let reused_search = substitute.needle.is_empty();
        let needle = if reused_search {
            self.active
                .editor
                .last_search()
                .map(|(pattern, _)| pattern.to_owned())
                .ok_or_else(|| anyhow!("no previous search pattern"))?
        } else {
            substitute.needle
        };
        let regex = RegexBuilder::new(&needle)
            .case_insensitive(substitute.ignore_case)
            .build()
            .with_context(|| format!("invalid substitution pattern {needle:?}"))?;
        let direction = self
            .active
            .editor
            .last_search()
            .map_or(SearchDirection::Forward, |(_, direction)| direction);
        self.active.editor.restore_search(needle.clone(), direction);
        self.search_highlight = true;
        if !reused_search {
            self.after_effect(None, vec![StateDelta::SearchPattern(needle.clone().into())]);
        }
        let task_id = CommandTaskId::new(self.next_task_id);
        self.next_task_id = self
            .next_task_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("task ID overflow"))?;
        let text = self.active.editor.contents();
        let base_revision = self.active.editor.revision();
        let replacement = vim_regex_replacement(&substitute.replacement);
        let ranges = substitute.ranges;
        let global = substitute.global;
        let document_id = self.active.document_id;
        let cancellation = self.tasks.submit(
            CommandTask {
                task_id,
                affected_documents: vec![document_id],
                label: "range substitution".into(),
            },
            move |context| {
                let mut edits = Vec::new();
                let mut bytes_since_checkpoint = 0;
                for range in ranges {
                    let start = range.start.min(text.len());
                    let end = range.end.min(text.len()).max(start);
                    let mut line_start = start;
                    for line in text[start..end].split_inclusive('\n') {
                        for captures in regex.captures_iter(line) {
                            let Some(found) = captures.get(0) else {
                                continue;
                            };
                            let mut insert = String::new();
                            captures.expand(&replacement, &mut insert);
                            edits.push(Edit::new(
                                line_start + found.start()..line_start + found.end(),
                                insert,
                            ));
                            if !global {
                                break;
                            }
                        }
                        line_start += line.len();
                        bytes_since_checkpoint += line.len();
                        if bytes_since_checkpoint >= 4096 {
                            context.checkpoint()?;
                            bytes_since_checkpoint = 0;
                        }
                    }
                }
                context.checkpoint()?;
                let count = edits.len();
                let mut effects = Effects {
                    messages: vec![format!("{count} substitution(s)").into_boxed_str()],
                    ..Effects::default()
                };
                if count > 0 {
                    effects.edit_proposals.push(EditProposal {
                        document_id,
                        base_revision,
                        transactions: vec![
                            Transaction::new(base_revision, edits)
                                .map_err(|error| TaskFailure::Failed(error.to_string().into()))?,
                        ],
                        label: "regex substitution".into(),
                    });
                }
                Ok(effects)
            },
        )?;
        self.active_task = Some(cancellation);
        self.message = format!("task {} running", task_id.get());
        Ok(())
    }

    fn poll_task_results(&mut self) -> Result<bool> {
        let mut changed = false;
        while let Some(result) = self.tasks.try_result()? {
            changed = true;
            self.active_task = None;
            let ai_result = self.active_ai_task == Some(result.task.task_id);
            if ai_result {
                self.active_ai_task = None;
            }
            match result.outcome {
                Ok(effects) => {
                    if ai_result {
                        self.ai_transcript = effects
                            .messages
                            .iter()
                            .map(AsRef::as_ref)
                            .collect::<Vec<&str>>()
                            .join("\n");
                        if self.ai_transcript.trim().is_empty() {
                            self.ai_transcript = "Codex returned no text.".to_owned();
                        }
                        self.show_ai_transcript();
                        self.message =
                            format!("Codex finished in {:.1}s", result.elapsed.as_secs_f32());
                        continue;
                    }
                    for proposal in effects.edit_proposals {
                        if proposal.document_id != self.active.document_id {
                            continue;
                        }
                        if proposal.base_revision != self.active.editor.revision() {
                            self.message = format!(
                                "task {} is stale at revision {}",
                                result.task.task_id.get(),
                                self.active.editor.revision().get()
                            );
                            continue;
                        }
                        for transaction in proposal.transactions {
                            self.active.editor.apply_transaction(transaction.clone())?;
                            self.after_transaction(Some(transaction));
                        }
                    }
                    if let Some(message) = effects.messages.last() {
                        self.message = message.to_string();
                    }
                }
                Err(TaskFailure::Cancelled) => {
                    self.message = if ai_result {
                        "Codex cancelled".to_owned()
                    } else {
                        "task cancelled".to_owned()
                    };
                }
                Err(error) => {
                    if ai_result {
                        self.ai_transcript = error.to_string();
                        self.show_ai_transcript();
                        self.message = error.to_string();
                    } else {
                        self.show_error(error);
                    }
                }
            }
        }
        Ok(changed)
    }

    fn open_terminal(&mut self, program: Option<&str>, arguments: &[Box<str>]) -> Result<()> {
        if self
            .terminal
            .as_ref()
            .is_some_and(|terminal| terminal.exit_code().is_none())
        {
            self.terminal_focused = true;
            self.message.clear();
            return Ok(());
        }
        let program = program
            .map(str::to_owned)
            .or_else(|| env::var("SHELL").ok())
            .unwrap_or_else(|| "sh".to_owned());
        let arguments = arguments
            .iter()
            .map(AsRef::<str>::as_ref)
            .collect::<Vec<_>>();
        self.terminal = Some(PtySession::spawn(&program, &arguments, 24, 80)?);
        self.terminal_focused = true;
        self.terminal_escape_pending = false;
        self.message = format!("terminal: {program}");
        Ok(())
    }

    fn handle_terminal_input(&mut self, input: TerminalInput) -> Result<()> {
        match input {
            TerminalInput::Resized { columns, rows } => self.resize_terminal(rows, columns),
            TerminalInput::Paste(text) => {
                if let Some(terminal) = &mut self.terminal {
                    terminal.send_input(text.as_bytes())?;
                }
            }
            TerminalInput::Key(key) => {
                if self.terminal_escape_pending {
                    self.terminal_escape_pending = false;
                    if key.control && matches!(key.code, TerminalKeyCode::Char('n' | 'N')) {
                        self.terminal_focused = false;
                        self.message = "terminal hidden; :terminal returns".to_owned();
                        return Ok(());
                    }
                    if let Some(terminal) = &mut self.terminal {
                        terminal.send_input(&[0x1c])?;
                    }
                }
                if key.control && key.code == TerminalKeyCode::Char('\\') {
                    self.terminal_escape_pending = true;
                    return Ok(());
                }
                if let Some(bytes) = terminal_key_bytes(key)
                    && let Some(terminal) = &mut self.terminal
                {
                    terminal.send_input(&bytes)?;
                }
            }
            TerminalInput::MouseScroll { .. } => {}
            TerminalInput::MouseClick { .. } => {}
            TerminalInput::Ignored => {}
        }
        Ok(())
    }

    fn resize_terminal(&mut self, rows: usize, columns: usize) {
        self.viewport_rows = rows.max(1);
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        let rows = u16::try_from(rows.saturating_sub(1))
            .unwrap_or(u16::MAX)
            .max(1);
        let columns = u16::try_from(columns).unwrap_or(u16::MAX).max(1);
        if let Err(error) = terminal.resize(rows, columns) {
            self.show_error(format!("terminal resize: {error}"));
        }
    }

    fn take_normal_count(&mut self) -> Option<usize> {
        let count = match self.active.editor.pending_parse_state() {
            Some(ParseState::Count { value }) => usize::try_from(value.get()).ok(),
            _ => None,
        };
        if count.is_some() {
            self.active.editor.cancel_pending();
        }
        count
    }

    fn apply_z_count(&mut self) {
        let Some(count) = self.take_normal_count() else {
            return;
        };
        let line = count.saturating_sub(1);
        let start = self.active.editor.text().byte_of_line(line);
        self.active.editor.set_cursor(start);
        self.dispatch_key(KeyEvent::character('^'));
    }

    fn navigate_jump_count(&mut self, backward: bool, count: usize) -> Result<bool> {
        let mut moved = false;
        for _ in 0..count {
            if self.navigate_global_jump(backward)? {
                moved = true;
                continue;
            }
            if !self.active.editor.navigate_jump(backward) {
                break;
            }
            moved = true;
        }
        Ok(moved)
    }

    fn navigate_change_count(&mut self, backward: bool, count: usize) -> bool {
        let mut moved = false;
        for _ in 0..count {
            if !self.active.editor.navigate_change(backward) {
                break;
            }
            moved = true;
        }
        moved
    }

    fn handle_window_prefix(&mut self, key: TerminalKey) -> Result<()> {
        let count = self.take_normal_count().unwrap_or(1);
        let direction = match key.code {
            TerminalKeyCode::Char('h' | 'H') | TerminalKeyCode::Left => Some(WindowDirection::Left),
            TerminalKeyCode::Char('j' | 'J') | TerminalKeyCode::Down => Some(WindowDirection::Down),
            TerminalKeyCode::Char('k' | 'K') | TerminalKeyCode::Up => Some(WindowDirection::Up),
            TerminalKeyCode::Char('l' | 'L') | TerminalKeyCode::Right => {
                Some(WindowDirection::Right)
            }
            _ => None,
        };
        if let Some(direction) = direction {
            self.views.focus_window(direction)?;
            self.activate_view_buffer()?;
            self.message.clear();
            return Ok(());
        }
        match key.code {
            TerminalKeyCode::Char('s' | 'S') => {
                self.views.split_active(SplitAxis::Horizontal)?;
                self.message.clear();
            }
            TerminalKeyCode::Char('v' | 'V') => {
                self.views.split_active(SplitAxis::Vertical)?;
                self.message.clear();
            }
            TerminalKeyCode::Char('c' | 'C' | 'q' | 'Q') => {
                if let Err(error) = self.views.close_active_window() {
                    self.show_error(error);
                } else {
                    self.activate_view_buffer()?;
                    self.message.clear();
                }
            }
            TerminalKeyCode::Char('o' | 'O') => {
                self.views.only_active_window()?;
                self.message.clear();
            }
            TerminalKeyCode::Char('w' | 'W') => {
                self.views.cycle_window(
                    if key.shift { -1 } else { 1_isize }
                        .saturating_mul(isize::try_from(count).unwrap_or(isize::MAX)),
                )?;
                self.activate_view_buffer()?;
                self.message.clear();
            }
            TerminalKeyCode::Char('=') => {
                self.views.equalize_windows()?;
                self.message.clear();
            }
            TerminalKeyCode::Escape => self.message.clear(),
            _ => self.message = "unknown Ctrl-W command".to_owned(),
        }
        Ok(())
    }

    fn scroll_page(&mut self, direction: isize, full_page: bool, count: Option<usize>) {
        let content_rows = self.viewport_rows.saturating_sub(1).max(1);
        let amount = if full_page {
            content_rows
                .saturating_sub(2)
                .max(1)
                .saturating_mul(count.unwrap_or(1))
        } else if let Some(count) = count {
            count
        } else {
            content_rows.checked_div(2).unwrap_or(1).max(1)
        };
        let text = self.active.editor.contents();
        let line_count = text.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let last_line = line_count.saturating_sub(1);
        let current_line = self.active.editor.cursor_line_column().0;
        let current_top = self.views.active_window().top_line;
        let (next_line, next_top) = if direction < 0 {
            (
                current_line.saturating_sub(amount),
                current_top.saturating_sub(amount),
            )
        } else {
            (
                current_line.saturating_add(amount).min(last_line),
                current_top
                    .saturating_add(amount)
                    .min(last_line.saturating_sub(content_rows.saturating_sub(1))),
            )
        };
        let column = self.active.editor.cursor_line_column().1;
        let start = self.active.editor.text().byte_of_line(next_line);
        let end = self
            .active
            .editor
            .text()
            .byte_of_line(next_line + 1)
            .saturating_sub(usize::from(next_line < last_line));
        let relative = text[start..end]
            .char_indices()
            .nth(column)
            .map_or(end.saturating_sub(start), |(byte, _)| byte);
        self.active
            .editor
            .set_cursor(start.saturating_add(relative));
        self.views.active_window_mut().top_line = next_top;
        self.message.clear();
    }

    fn scroll_view_line(&mut self, direction: isize, count: usize) {
        let content_rows = self.viewport_rows.saturating_sub(1).max(1);
        let text = self.active.editor.contents();
        let last_line = text.bytes().filter(|byte| *byte == b'\n').count();
        let max_top = last_line.saturating_sub(content_rows.saturating_sub(1));
        let current_top = self.views.active_window().top_line;
        let next_top = if direction < 0 {
            current_top.saturating_sub(count)
        } else {
            current_top.saturating_add(count).min(max_top)
        };
        self.views.active_window_mut().top_line = next_top;
        let cursor_line = self.active.editor.cursor_line_column().0;
        if cursor_line < next_top {
            self.set_cursor_line(next_top);
        } else if cursor_line >= next_top.saturating_add(content_rows) {
            self.set_cursor_line(next_top.saturating_add(content_rows - 1).min(last_line));
        }
        self.message.clear();
    }

    fn move_cursor_to_view(&mut self, position: ViewPosition, count: usize) {
        let top = self.views.active_window().top_line;
        let content_rows = self.viewport_rows.saturating_sub(1).max(1);
        let last_line = self
            .active
            .editor
            .contents()
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        let offset = match position {
            ViewPosition::Top => count.saturating_sub(1),
            ViewPosition::Middle => content_rows / 2,
            ViewPosition::Bottom => content_rows.saturating_sub(count),
        };
        self.set_cursor_line(top.saturating_add(offset).min(last_line));
        self.message.clear();
    }

    fn center_cursor_line(&mut self, position: ViewPosition) {
        let line = self.active.editor.cursor_line_column().0;
        let content_rows = self.viewport_rows.saturating_sub(1).max(1);
        let top = match position {
            ViewPosition::Top => line,
            ViewPosition::Middle => line.saturating_sub(content_rows / 2),
            ViewPosition::Bottom => line.saturating_sub(content_rows.saturating_sub(1)),
        };
        self.views.active_window_mut().top_line = top;
        self.message.clear();
    }

    fn set_cursor_line(&mut self, line: usize) {
        let column = self.active.editor.cursor_line_column().1;
        let text = self.active.editor.contents();
        let start = self.active.editor.text().byte_of_line(line);
        let raw_end = self.active.editor.text().byte_of_line(line + 1);
        let end = raw_end.saturating_sub(usize::from(
            raw_end > start && text.as_bytes().get(raw_end - 1) == Some(&b'\n'),
        ));
        let relative = text[start..end]
            .char_indices()
            .nth(column)
            .map_or(end.saturating_sub(start), |(byte, _)| byte);
        self.active
            .editor
            .set_cursor(start.saturating_add(relative));
    }

    fn search_word_under_cursor(&mut self, backward: bool, count: usize) {
        let Some(word) = self.word_under_cursor() else {
            self.message = "no word under cursor".to_owned();
            return;
        };
        let direction = if backward {
            SearchDirection::Backward
        } else {
            SearchDirection::Forward
        };
        let found = match self.active.editor.search(&word, direction) {
            Ok(found) => found,
            Err(error) => {
                self.show_error(error);
                return;
            }
        };
        self.message = if found {
            for _ in 1..count {
                if !self.active.editor.search_next(false) {
                    break;
                }
            }
            format!("{}{}", if backward { '#' } else { '*' }, word)
        } else {
            format!("pattern not found: {word}")
        };
        self.after_effect(None, vec![StateDelta::SearchPattern(word.into())]);
    }

    fn show_file_info(&mut self) {
        let text = self.active.editor.contents();
        let line_count = text.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let (line, column) = self.active.editor.cursor_line_column();
        let percent = (line + 1)
            .saturating_mul(100)
            .checked_div(line_count)
            .unwrap_or(100);
        self.message = format!(
            "\"{}\"{} {} line(s) --{}%-- {}:{}",
            self.active.name(),
            if self.active.editor.is_dirty() {
                " [Modified]"
            } else {
                ""
            },
            line_count,
            percent,
            line + 1,
            column + 1
        );
    }

    fn poll_terminal(&mut self) -> Result<bool> {
        let Some(terminal) = &mut self.terminal else {
            return Ok(false);
        };
        let changed = terminal.poll()?;
        if changed && let Some(code) = terminal.exit_code() {
            self.terminal_focused = false;
            self.message = format!("terminal exited with status {code}");
        }
        Ok(changed)
    }

    fn terminal_frame(&self) -> wren_engine::EngineFrame {
        let Some(terminal) = &self.terminal else {
            return wren_engine::EngineFrame {
                text: "terminal unavailable".into(),
                cursor_byte: 0,
            };
        };
        let text = terminal.surface().contents();
        let (row, column) = terminal.surface().cursor_position();
        let row_start = text
            .match_indices('\n')
            .nth(usize::from(row).saturating_sub(1))
            .map_or(0, |(byte, _)| byte + 1);
        let row_end = text[row_start..]
            .find('\n')
            .map_or(text.len(), |offset| row_start + offset);
        let cursor_byte = text[row_start..row_end]
            .char_indices()
            .nth(usize::from(column))
            .map_or(row_end, |(offset, _)| row_start + offset);
        wren_engine::EngineFrame {
            text: text.into(),
            cursor_byte,
        }
    }

    fn terminal_status(&self) -> String {
        self.terminal.as_ref().map_or_else(
            || " TERMINAL | unavailable".to_owned(),
            |terminal| {
                let state = terminal
                    .exit_code()
                    .map_or_else(|| "running".to_owned(), |code| format!("exit {code}"));
                format!(
                    " TERMINAL | {state} | {} bytes | Ctrl-\\ Ctrl-N returns",
                    terminal.bytes_read()
                )
            },
        )
    }

    fn show_ai_transcript(&mut self) {
        let (text, decorations) = lsp_popup_markdown(&self.ai_transcript, self.theme);
        self.popup = Some(TextPopup {
            title: "Avante · Codex".into(),
            text: text.into(),
            scroll: 0,
            decorations,
        });
        self.popup_deadline = None;
    }

    fn start_ai_task(&mut self, prompt: &str) -> Result<()> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            self.prompt = Some(Prompt::new(PromptKind::Ai));
            return Ok(());
        }
        if self.active_task.is_some() {
            bail!("a TaskCommand is already running");
        }
        let task_id = CommandTaskId::new(self.next_task_id);
        self.next_task_id = self
            .next_task_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("task ID overflow"))?;
        let root = self
            .active
            .document
            .presentation_path()
            .and_then(|path| git_root_for(path).ok())
            .or_else(|| self.workspace_folders.first().cloned())
            .unwrap_or_else(|| PathBuf::from("."));
        let (line, column) = self.active.editor.cursor_line_column();
        let context = self.active.document.presentation_path().map_or_else(
            || prompt.to_owned(),
            |path| {
                format!(
                    "The active editor file is {} at line {}, column {}.\n\n{}",
                    path.display(),
                    line + 1,
                    column + 1,
                    prompt
                )
            },
        );
        let mut environment = BTreeMap::new();
        if let Ok(path) = env::var("PATH") {
            environment.insert("PATH".into(), path.into());
        }
        let arguments = vec![
            "exec".into(),
            "--sandbox".into(),
            "read-only".into(),
            "--color".into(),
            "never".into(),
            "--skip-git-repo-check".into(),
            "-C".into(),
            root.to_string_lossy().into_owned().into_boxed_str(),
            context.into_boxed_str(),
        ];
        let spec = WorkflowTaskSpec {
            program: "codex".into(),
            arguments,
            environment,
            visibility: DocumentVisibility::Persisted,
            save: SavePolicy::Never,
            max_output_bytes: 4 * 1024 * 1024,
        };
        let cancellation = self.tasks.submit(
            CommandTask {
                task_id,
                affected_documents: Vec::new(),
                label: "Codex assistant".into(),
            },
            move |context| {
                context.checkpoint()?;
                let token = context.cancellation_token();
                let output = TaskSupervisor::new(true)
                    .run_until_cancelled(&spec, || token.is_cancelled())
                    .map_err(|error| TaskFailure::Failed(error.to_string().into()))?;
                context.checkpoint()?;
                if output.cancelled {
                    return Err(TaskFailure::Cancelled);
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if output.status != Some(0) {
                    let detail = if stderr.trim().is_empty() {
                        stdout.trim()
                    } else {
                        stderr.trim()
                    };
                    return Err(TaskFailure::Failed(
                        format!("Codex failed: {detail}").into(),
                    ));
                }
                Ok(Effects {
                    messages: vec![stdout.trim().to_owned().into_boxed_str()],
                    ..Effects::default()
                })
            },
        )?;
        self.active_task = Some(cancellation);
        self.active_ai_task = Some(task_id);
        self.popup = None;
        self.popup_deadline = None;
        self.message = "Codex is thinking…".to_owned();
        Ok(())
    }

    fn start_make_task(&mut self, program: &str, arguments: &[Box<str>]) -> Result<()> {
        if self.active_task.is_some() {
            bail!("a TaskCommand is already running");
        }
        let task_id = CommandTaskId::new(self.next_task_id);
        self.next_task_id = self
            .next_task_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("task ID overflow"))?;
        let mut environment = BTreeMap::new();
        if let Ok(path) = env::var("PATH") {
            environment.insert("PATH".into(), path.into());
        }
        let spec = WorkflowTaskSpec {
            program: program.into(),
            arguments: arguments.to_vec(),
            environment,
            visibility: DocumentVisibility::Persisted,
            save: SavePolicy::Never,
            max_output_bytes: 1024 * 1024,
        };
        let cancellation = self.tasks.submit(
            CommandTask {
                task_id,
                affected_documents: Vec::new(),
                label: format!("make {program}").into(),
            },
            move |context| {
                context.checkpoint()?;
                let token = context.cancellation_token();
                let output = TaskSupervisor::new(true)
                    .run_until_cancelled(&spec, || token.is_cancelled())
                    .map_err(|error| TaskFailure::Failed(error.to_string().into()))?;
                context.checkpoint()?;
                if output.cancelled {
                    return Err(TaskFailure::Cancelled);
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let detail = if !stderr.trim().is_empty() {
                    stderr.trim()
                } else {
                    stdout.trim()
                };
                let status = output
                    .status
                    .map_or_else(|| "signal".to_owned(), |status| status.to_string());
                Ok(Effects {
                    messages: vec![
                        format!(
                            "task {task_id:?} status {status}: {}",
                            if detail.is_empty() {
                                "no output"
                            } else {
                                detail
                            }
                        )
                        .into(),
                    ],
                    ..Effects::default()
                })
            },
        )?;
        self.active_task = Some(cancellation);
        self.message = format!("task {} running", task_id.get());
        Ok(())
    }

    fn start_format_task(&mut self, program: &str, arguments: &[Box<str>]) -> Result<()> {
        if self.active_task.is_some() {
            bail!("a TaskCommand is already running");
        }
        if self.active.editor.is_read_only() {
            bail!("document is read-only");
        }
        let task_id = CommandTaskId::new(self.next_task_id);
        self.next_task_id = self
            .next_task_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("task ID overflow"))?;
        let document_id = self.active.document_id;
        let base_revision = self.active.editor.revision();
        let input = self.active.editor.contents();
        let mut environment = BTreeMap::new();
        if let Ok(path) = env::var("PATH") {
            environment.insert("PATH".into(), path.into());
        }
        let spec = WorkflowTaskSpec {
            program: program.into(),
            arguments: arguments.to_vec(),
            environment,
            visibility: DocumentVisibility::Persisted,
            save: SavePolicy::Never,
            max_output_bytes: input.len().saturating_mul(4).max(1024 * 1024),
        };
        let cancellation = self.tasks.submit(
            CommandTask {
                task_id,
                affected_documents: vec![document_id],
                label: format!("format {program}").into(),
            },
            move |context| {
                context.checkpoint()?;
                let token = context.cancellation_token();
                let formatted = run_formatter_until_cancelled(
                    &spec,
                    true,
                    document_id,
                    base_revision,
                    base_revision,
                    &input,
                    || token.is_cancelled(),
                )
                .map_err(|error| match error {
                    WorkflowError::Cancelled => TaskFailure::Cancelled,
                    error => TaskFailure::Failed(error.to_string().into()),
                })?;
                context.checkpoint()?;
                let edit_proposals = if formatted.edits.is_empty() {
                    Vec::new()
                } else {
                    vec![EditProposal {
                        document_id,
                        base_revision,
                        transactions: vec![
                            Transaction::new(base_revision, formatted.edits)
                                .map_err(|error| TaskFailure::Failed(error.to_string().into()))?,
                        ],
                        label: "formatter".into(),
                    }]
                };
                Ok(Effects {
                    edit_proposals,
                    messages: vec!["format complete".into()],
                    ..Effects::default()
                })
            },
        )?;
        self.active_task = Some(cancellation);
        self.message = format!("formatter task {} running", task_id.get());
        Ok(())
    }

    fn format_active_language(&mut self) -> Result<()> {
        if self.format_active_sync(true)? {
            self.message = "format complete".to_owned();
        }
        Ok(())
    }

    fn format_text_width(&mut self) -> Result<()> {
        let text = self.active.editor.contents();
        let (range, was_visual) =
            if matches!(self.active.editor.mode(), Mode::Visual | Mode::VisualLine) {
                (self.active.editor.selection_byte_range(), true)
            } else {
                let current = self.active.editor.cursor_line_column().0;
                let mut first = current;
                while first > 0 {
                    let start = self.active.editor.text().byte_of_line(first - 1);
                    let end = self.active.editor.text().byte_of_line(first);
                    if text[start..end].trim().is_empty() {
                        break;
                    }
                    first -= 1;
                }
                let mut last = current + 1;
                let line_count = self.active.editor.text().line_of_byte(text.len()) + 1;
                while last < line_count {
                    let start = self.active.editor.text().byte_of_line(last);
                    let end = self.active.editor.text().byte_of_line(last + 1);
                    if text[start..end].trim().is_empty() {
                        break;
                    }
                    last += 1;
                }
                (
                    self.active.editor.text().byte_of_line(first)
                        ..self.active.editor.text().byte_of_line(last),
                    false,
                )
            };
        let source = text.get(range.clone()).unwrap_or_default();
        let formatted = wrap_editor_text(source, 79);
        if formatted == source {
            self.message = "text already fits textwidth=79".to_owned();
            return Ok(());
        }
        if was_visual {
            self.dispatch_key(KeyEvent::plain(KeyCode::Escape));
        }
        let transaction = Transaction::new(
            self.active.editor.revision(),
            vec![Edit::new(range, formatted)],
        )?;
        self.active.editor.apply_transaction(transaction.clone())?;
        self.after_transaction(Some(transaction));
        self.message = "formatted to textwidth=79".to_owned();
        Ok(())
    }

    fn format_active_sync(&mut self, explicit: bool) -> Result<bool> {
        if self.active.editor.is_read_only() {
            if explicit {
                self.message = "document is read-only".to_owned();
            }
            return Ok(false);
        }
        let Some(path) = self.active.document.presentation_path() else {
            if explicit {
                self.message = "formatter needs a named buffer".to_owned();
            }
            return Ok(false);
        };
        let Some(invocation) = formatter_invocation(path) else {
            return self.lsp_format_sync(explicit);
        };
        if !executable_exists(&invocation.program) {
            if explicit {
                self.message = format!("formatter {} is not installed", invocation.program);
            }
            return Ok(false);
        }
        let input = self.active.editor.contents();
        let mut child = Command::new(&invocation.program)
            .args(&invocation.arguments)
            .current_dir(self.workspace_root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("start formatter {}", invocation.program))?;
        child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("formatter stdin is unavailable"))?
            .write_all(input.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!(
                "{} failed: {}",
                invocation.program,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let formatted = String::from_utf8(output.stdout)
            .with_context(|| format!("{} returned non-UTF-8", invocation.program))?;
        if formatted == input {
            return Ok(true);
        }
        let transaction = Transaction::new(
            self.active.editor.revision(),
            vec![Edit::new(0..input.len(), formatted)],
        )?;
        self.active.editor.apply_transaction(transaction.clone())?;
        self.after_transaction(Some(transaction));
        Ok(true)
    }

    fn lsp_format_sync(&mut self, explicit: bool) -> Result<bool> {
        if self.active_language_server().is_none() {
            if explicit {
                self.message = format!(
                    "no formatter configured for {}",
                    language_bundle(self.active.document.presentation_path()).language_id
                );
            }
            return Ok(false);
        }
        let (mut client, uri) = self.start_lsp()?;
        let result = client.request(
            "textDocument/formatting",
            serde_json::json!({
                "textDocument": {"uri": uri},
                "options": {"tabSize": 2, "insertSpaces": true}
            }),
        )?;
        let edits: Vec<LspTextEdit> = serde_json::from_value(result)?;
        let revision = self.active.editor.revision();
        let text = self.active.editor.contents();
        let lowered =
            lower_lsp_text_edits(self.active.document_id, revision, revision, &text, edits)?;
        if !lowered.edits.is_empty() {
            let transaction = Transaction::new(revision, lowered.edits)?;
            self.active.editor.apply_transaction(transaction.clone())?;
            self.after_transaction(Some(transaction));
        }
        Ok(true)
    }

    /// Install the local Tree-sitter baseline before the first frame for a
    /// normal document. LSP semantic tokens remain fully asynchronous and
    /// refine these spans when the language server responds.
    fn prime_active_syntax(&mut self) {
        let revision = self.active.editor.revision();
        if self
            .decorations
            .get(&self.active.buffer_id)
            .is_some_and(|state| state.revision == revision)
        {
            return;
        }
        if !self.active.class.policy().whole_document_syntax {
            return;
        }
        let bundle = language_bundle(self.active.document.presentation_path());
        let text = self.active.editor.contents();
        let spans = self
            .provider
            .highlight_now(
                self.active.document_id,
                revision,
                text.clone().into_boxed_str(),
                bundle,
            )
            .unwrap_or_else(|_| lexical_highlight_text(&text))
            .into_iter()
            .map(|span| provider_decoration(span, self.theme))
            .collect::<Vec<_>>();
        self.decorations
            .insert(self.active.buffer_id, BufferDecorations { revision, spans });
    }

    /// Reparse a bounded context around changed lines before the next frame.
    /// Existing full-buffer spans have already been transaction-mapped, so
    /// newly typed syntax appears immediately without a remote LSP round trip
    /// or a whole-file parse on every keypress.
    fn refresh_changed_syntax(&mut self, transaction: &Transaction) {
        if transaction.edits.is_empty() {
            return;
        }
        let text_store = self.active.editor.text();
        let text_len = text_store.len_bytes();
        let mut targets = transaction
            .edits
            .iter()
            .filter_map(|edit| {
                let start = transaction.map_offset(edit.range.start, Bias::Left).ok()?;
                let end = transaction.map_offset(edit.range.end, Bias::Right).ok()?;
                let start_line = text_store.line_of_byte(start.min(text_len));
                let end_line = text_store.line_of_byte(end.min(text_len));
                let target_start_line = start_line.saturating_sub(1);
                let target_end_line = end_line.saturating_add(2);
                let target = text_store.byte_of_line(target_start_line)
                    ..text_store
                        .byte_of_line(target_end_line)
                        .max(start)
                        .min(text_len);
                let context = text_store.byte_of_line(target_start_line.saturating_sub(32))
                    ..text_store
                        .byte_of_line(target_end_line.saturating_add(32))
                        .max(target.end)
                        .min(text_len);
                Some((target, context))
            })
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return;
        }
        targets.sort_by_key(|(target, _)| target.start);
        let text = self.active.editor.contents();
        let mut replacement = Vec::new();
        for (target, context) in &targets {
            let Some(source) = text.get(context.clone()) else {
                continue;
            };
            replacement.extend(
                lexical_highlight_text(source)
                    .into_iter()
                    .map(|mut span| {
                        span.range.start = span.range.start.saturating_add(context.start);
                        span.range.end = span.range.end.saturating_add(context.start);
                        span
                    })
                    .filter(|span| span.range.start < target.end && target.start < span.range.end)
                    .map(|span| provider_decoration(span, self.theme)),
            );
        }
        let revision = self.active.editor.revision();
        let state = self
            .decorations
            .entry(self.active.buffer_id)
            .or_insert_with(|| BufferDecorations {
                revision,
                spans: Vec::new(),
            });
        state.spans.retain(|span| {
            !targets
                .iter()
                .any(|(target, _)| span.range.start < target.end && target.start < span.range.end)
        });
        state.spans.extend(replacement);
        state
            .spans
            .sort_by_key(|span| (span.range.start, std::cmp::Reverse(span.range.end)));
        state.spans.dedup();
        state.revision = revision;
    }

    fn schedule_provider_refreshes(&mut self, viewport_height: usize) {
        let viewport_height = viewport_height.max(1);
        let mut line_ranges: BTreeMap<BufferId, (usize, usize)> = BTreeMap::new();
        let windows = self
            .views
            .windows
            .iter()
            .enumerate()
            .map(|(index, window)| (index, window.buffer_id, window.top_line))
            .collect::<Vec<_>>();
        for (window_index, buffer_id, top_line) in windows {
            let Some(buffer) = self.buffer(buffer_id) else {
                continue;
            };
            let cursor_line = buffer.editor.cursor_line_column().0;
            let margin = 3.min(viewport_height.saturating_sub(1) / 2);
            let effective_top = if cursor_line < top_line.saturating_add(margin) {
                cursor_line.saturating_sub(margin)
            } else if cursor_line.saturating_add(margin) >= top_line.saturating_add(viewport_height)
            {
                cursor_line
                    .saturating_add(margin)
                    .saturating_add(1)
                    .saturating_sub(viewport_height)
            } else {
                top_line
            };
            if let Some(window) = self.views.windows.get_mut(window_index) {
                window.top_line = effective_top;
            }
            let range = line_ranges
                .entry(buffer_id)
                .or_insert((effective_top, effective_top + viewport_height));
            range.0 = range.0.min(effective_top);
            range.1 = range.1.max(effective_top + viewport_height);
        }
        let refreshes = line_ranges
            .into_iter()
            .filter_map(|(buffer_id, (top_line, bottom_line))| {
                let buffer = self.buffer(buffer_id)?;
                let text_store = buffer.editor.text();
                let visible_start = text_store.byte_of_line(top_line);
                let visible_end = text_store.byte_of_line(bottom_line).max(visible_start);
                let near_start = text_store.byte_of_line(top_line.saturating_sub(viewport_height));
                let near_end = text_store
                    .byte_of_line(bottom_line.saturating_add(viewport_height))
                    .max(visible_end);
                Some(ProviderRefresh {
                    buffer_id,
                    document_id: buffer.document_id,
                    revision: buffer.editor.revision(),
                    text: buffer.editor.contents().into(),
                    bundle: language_bundle(buffer.document.presentation_path()),
                    visible: visible_start..visible_end,
                    near_viewport: near_start..near_end,
                })
            })
            .collect::<Vec<_>>();
        for refresh in refreshes {
            let key = ProviderDemandKey::from(&refresh);
            if self.provider_submitted.get(&refresh.document_id) == Some(&key) {
                continue;
            }
            if self.provider.try_refresh(refresh.clone()) {
                self.provider_submitted.insert(refresh.document_id, key);
            }
        }
    }

    fn poll_provider_results(&mut self) -> bool {
        let mut changed = false;
        while let Some(result) = self.provider.try_result() {
            match result {
                ProviderWorkerResult::Decorations {
                    buffer_id,
                    document_id,
                    revision,
                    spans,
                    ranges,
                } => {
                    let current_revision = self
                        .buffer(buffer_id)
                        .map(|buffer| buffer.editor.revision());
                    if current_revision != Some(revision) {
                        continue;
                    }
                    let spans = spans
                        .into_iter()
                        .map(|span| provider_decoration(span, self.theme))
                        .collect::<Vec<_>>();
                    let mut merged = self
                        .decorations
                        .get(&buffer_id)
                        .filter(|state| state.revision == revision)
                        .map_or_else(Vec::new, |state| state.spans.clone());
                    merged.retain(|span| {
                        !ranges.iter().any(|range| {
                            span.range.start < range.end && range.start < span.range.end
                        })
                    });
                    merged.extend(spans);
                    // Paint broader parent captures first so narrower semantic
                    // captures at the same start offset win deterministically.
                    merged
                        .sort_by_key(|span| (span.range.start, std::cmp::Reverse(span.range.end)));
                    merged.dedup();
                    let next = BufferDecorations {
                        revision,
                        spans: merged,
                    };
                    if self.decorations.get(&buffer_id) != Some(&next) {
                        self.decorations.insert(buffer_id, next);
                        changed = true;
                    }
                    self.provider_submitted
                        .entry(document_id)
                        .or_insert(ProviderDemandKey {
                            revision,
                            visible_start: 0,
                            visible_end: 0,
                            near_start: 0,
                            near_end: 0,
                        });
                }
                ProviderWorkerResult::Failed {
                    document_id,
                    message,
                } => {
                    self.provider_submitted.remove(&document_id);
                    self.show_error(format!("provider: {message}"));
                    changed = true;
                }
                ProviderWorkerResult::Completion {
                    document_id,
                    mut session,
                } => {
                    if document_id == self.active.document_id
                        && session.revision == self.active.editor.revision()
                    {
                        if let Some(lsp) = self.lsp_completion.take()
                            && lsp.revision == session.revision
                        {
                            session = CompletionSession::merge(
                                session.revision,
                                session.replace,
                                session.candidates,
                                lsp.candidates,
                            );
                        }
                        self.completion_index = 0;
                        self.completion_selected = false;
                        self.completion_documentation_scroll = 0;
                        self.completion = Some(session);
                        self.update_completion_message();
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    fn request_completion(&mut self) {
        let (replace, local_candidates) = self.local_completion_candidates();
        self.lsp_completion = self.request_lsp_completion().ok().flatten();
        if !local_candidates.is_empty() {
            if let Some(completion) = &mut self.lsp_completion {
                completion.candidates.extend(local_candidates);
            } else {
                self.lsp_completion = Some(LspCompletion {
                    revision: self.active.editor.revision(),
                    replace,
                    candidates: local_candidates,
                });
            }
        }
        if let Some(lsp) = &self.lsp_completion {
            self.completion = Some(CompletionSession::merge(
                lsp.revision,
                lsp.replace.clone(),
                Vec::new(),
                lsp.candidates.clone(),
            ));
            self.completion_selected = false;
            self.completion_index = 0;
            self.completion_documentation_scroll = 0;
        }
        let completion = ProviderCompletion {
            document_id: self.active.document_id,
            revision: self.active.editor.revision(),
            text: self.active.editor.contents().into_boxed_str(),
            bundle: language_bundle(self.active.document.presentation_path()),
            byte: self.active.editor.primary_cursor(),
        };
        if self.provider.try_complete(completion) {
            self.message = "completion…".to_owned();
        } else {
            self.message = "completion queue is busy".to_owned();
        }
    }

    fn local_completion_candidates(&self) -> (Range<usize>, Vec<CompletionCandidate>) {
        let text = self.active.editor.contents();
        let cursor = self.active.editor.primary_cursor().min(text.len());
        let word_start = text[..cursor]
            .char_indices()
            .rev()
            .take_while(|(_, character)| character.is_alphanumeric() || *character == '_')
            .last()
            .map_or(cursor, |(byte, _)| byte);
        let mut candidates = self.path_completion_candidates(&text, cursor);
        candidates.extend(
            self.vsnip_completion_candidates(&text[word_start..cursor], word_start..cursor),
        );
        (word_start..cursor, candidates)
    }

    fn path_completion_candidates(&self, text: &str, cursor: usize) -> Vec<CompletionCandidate> {
        let token_start = text[..cursor]
            .char_indices()
            .rev()
            .find(|(_, character)| {
                character.is_whitespace()
                    || matches!(
                        character,
                        '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
                    )
            })
            .map_or(0, |(byte, character)| byte + character.len_utf8());
        let token = &text[token_start..cursor];
        if token.len() > 512 || token.contains("::") {
            return Vec::new();
        }
        let (typed_directory, name_prefix) = token
            .rsplit_once('/')
            .map_or(("", token), |(directory, name)| {
                (&token[..directory.len() + 1], name)
            });
        let expanded_directory = if let Some(relative) = typed_directory.strip_prefix("~/") {
            env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(relative)
        } else if Path::new(typed_directory).is_absolute() {
            PathBuf::from(typed_directory)
        } else {
            env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(typed_directory)
        };
        let Ok(entries) = std::fs::read_dir(&expanded_directory) else {
            return Vec::new();
        };
        let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
        entries.sort_by_key(|entry| (!entry.path().is_dir(), entry.file_name()));
        entries
            .into_iter()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name_prefix.is_empty()
                    && !name
                        .to_ascii_lowercase()
                        .starts_with(&name_prefix.to_ascii_lowercase())
                {
                    return None;
                }
                let directory = entry.path().is_dir();
                let label = format!("{name}{}", if directory { "/" } else { "" });
                Some(CompletionCandidate {
                    label: label.clone().into(),
                    insert: format!("{typed_directory}{label}").into(),
                    source: "path".into(),
                    detail: if directory { "Directory" } else { "File" }.into(),
                    documentation: entry.path().display().to_string().into(),
                    replace: Some(token_start..cursor),
                    snippet: None,
                })
            })
            .take(64)
            .collect()
    }

    fn vsnip_completion_candidates(
        &self,
        prefix: &str,
        replace: Range<usize>,
    ) -> Vec<CompletionCandidate> {
        let language = language_bundle(self.active.document.presentation_path()).language_id;
        let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
            return Vec::new();
        };
        let paths = [
            home.join(".vsnip").join(format!("{language}.json")),
            home.join(".config/nvim/snippets")
                .join(format!("{language}.json")),
        ];
        let mut candidates = Vec::new();
        for path in paths.into_iter().filter(|path| path.exists()) {
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(snippets) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&source)
            else {
                continue;
            };
            for (name, snippet) in snippets {
                let prefixes = snippet.get("prefix").map_or_else(
                    || vec![name.as_str()],
                    |value| {
                        value.as_array().map_or_else(
                            || value.as_str().into_iter().collect(),
                            |values| {
                                values
                                    .iter()
                                    .filter_map(serde_json::Value::as_str)
                                    .collect()
                            },
                        )
                    },
                );
                let body = snippet.get("body").map_or_else(String::new, |body| {
                    body.as_array().map_or_else(
                        || body.as_str().unwrap_or_default().to_owned(),
                        |lines| {
                            lines
                                .iter()
                                .filter_map(serde_json::Value::as_str)
                                .collect::<Vec<_>>()
                                .join("\n")
                        },
                    )
                });
                if body.is_empty() {
                    continue;
                }
                let description = snippet
                    .get("description")
                    .map(render_lsp_text)
                    .unwrap_or_else(|| name.clone());
                for snippet_prefix in prefixes {
                    if !prefix.is_empty()
                        && !snippet_prefix
                            .to_ascii_lowercase()
                            .starts_with(&prefix.to_ascii_lowercase())
                    {
                        continue;
                    }
                    candidates.push(CompletionCandidate {
                        label: snippet_prefix.into(),
                        insert: expand_lsp_snippet(&body).into(),
                        source: "vsnip".into(),
                        detail: name.as_str().into(),
                        documentation: description.as_str().into(),
                        replace: Some(replace.clone()),
                        snippet: Some(body.as_str().into()),
                    });
                }
            }
        }
        candidates
    }

    fn completion_overlay(&self) -> Option<CompletionOverlay> {
        let session = self.completion.as_ref()?;
        if session.candidates.is_empty() {
            return None;
        }
        let selected = self
            .completion_selected
            .then_some(self.completion_index.min(session.candidates.len() - 1));
        let documentation = selected
            .and_then(|index| session.candidates.get(index))
            .map_or("", |candidate| candidate.documentation.as_ref());
        Some(CompletionOverlay {
            rows: session
                .candidates
                .iter()
                .map(|candidate| CompletionOverlayRow {
                    label: candidate.label.clone(),
                    detail: candidate.detail.clone(),
                    source: candidate.source.clone(),
                })
                .collect(),
            selected,
            documentation: documentation.into(),
            documentation_scroll: self.completion_documentation_scroll,
        })
    }

    fn completion_documentation_lines(&self) -> usize {
        self.completion
            .as_ref()
            .and_then(|session| session.candidates.get(self.completion_index))
            .map_or(0, |candidate| candidate.documentation.lines().count())
    }

    fn move_completion(&mut self, direction: isize) {
        let Some(session) = &self.completion else {
            return;
        };
        if session.candidates.is_empty() {
            self.completion_index = 0;
        } else if direction < 0 {
            self.completion_index = self
                .completion_index
                .checked_sub(1)
                .unwrap_or(session.candidates.len() - 1);
        } else {
            self.completion_index = (self.completion_index + 1) % session.candidates.len();
        }
        self.completion_selected = !session.candidates.is_empty();
        self.completion_documentation_scroll = 0;
        self.update_completion_message();
    }

    fn update_completion_message(&mut self) {
        self.message = self.completion.as_ref().map_or_else(
            || "no completion".to_owned(),
            |session| {
                session.candidates.get(self.completion_index).map_or_else(
                    || "no completion candidates".to_owned(),
                    |candidate| {
                        format!(
                            "completion [{}/{}] {} · {} · Enter accepts, Ctrl-N/P cycles",
                            self.completion_index + 1,
                            session.candidates.len(),
                            candidate.label,
                            candidate.source
                        )
                    },
                )
            },
        );
    }

    fn accept_completion(&mut self) -> Result<()> {
        let Some(session) = self.completion.take() else {
            return Ok(());
        };
        let candidate = session.candidates.get(self.completion_index).cloned();
        let replace_start = candidate
            .as_ref()
            .map_or(session.replace.start, |candidate| {
                candidate
                    .replace
                    .as_ref()
                    .map_or(session.replace.start, |range| range.start)
            });
        let transaction = session.accept(self.active.editor.revision(), self.completion_index)?;
        if let Some(transaction) = transaction {
            self.active.editor.apply_transaction(transaction.clone())?;
            self.after_transaction(Some(transaction));
            if let Some(snippet) = candidate.and_then(|candidate| candidate.snippet) {
                let (_, stops) = expand_lsp_snippet_with_stops(&snippet);
                self.snippet_stops = stops
                    .into_iter()
                    .map(|range| replace_start + range.start..replace_start + range.end)
                    .collect();
                self.snippet_stop_index = 0;
                if let Some(range) = self.snippet_stops.first().cloned() {
                    self.active.editor.set_selection_range(range);
                }
            }
            self.message = "completion accepted".to_owned();
        }
        Ok(())
    }

    fn move_snippet_stop(&mut self, direction: isize) {
        if self.snippet_stops.is_empty() {
            return;
        }
        if direction < 0 {
            self.snippet_stop_index = self.snippet_stop_index.saturating_sub(1);
        } else if self.snippet_stop_index + 1 >= self.snippet_stops.len() {
            if let Some(range) = self.snippet_stops.last() {
                self.active.editor.set_cursor(range.end);
            }
            self.snippet_stops.clear();
            self.snippet_stop_index = 0;
            return;
        } else {
            self.snippet_stop_index += 1;
        }
        if let Some(range) = self.snippet_stops.get(self.snippet_stop_index).cloned() {
            self.active.editor.set_selection_range(range);
        }
    }

    fn start_file_picker(&mut self, query: &str) -> Result<()> {
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

    fn picker_overlay(&self) -> Option<PickerOverlay> {
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

    fn selected_picker_target(&self) -> Option<(PathBuf, Option<usize>)> {
        let prompt = self.prompt.as_ref()?;
        match prompt.kind {
            PromptKind::FilePicker | PromptKind::FileBrowser => self
                .picker_matches
                .get(self.picker_index)
                .cloned()
                .map(|path| (path, None)),
            PromptKind::Grep => self
                .quickfix
                .get(self.picker_index)
                .map(|entry| (entry.path.clone(), Some(entry.line))),
            PromptKind::Location => self
                .filtered_locations(&prompt.buffer)
                .get(self.picker_index)
                .map(|entry| (entry.path.clone(), Some(entry.line))),
            _ => None,
        }
    }

    fn refresh_picker_preview(&mut self) {
        self.picker_preview_scroll = 0;
        self.picker_preview_highlight_line = None;
        self.picker_preview_decorations.clear();
        let Some((path, line)) = self.selected_picker_target() else {
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
                self.picker_preview.clone().into_boxed_str(),
                bundle,
            )
            .unwrap_or_else(|_| lexical_highlight_text(&self.picker_preview));
        self.picker_preview_decorations = spans
            .into_iter()
            .map(|span| provider_decoration(span, self.theme))
            .collect();
        if let Some(line) = line {
            let line = line.saturating_sub(1);
            self.picker_preview_highlight_line = Some(line);
            self.picker_preview_scroll = line.saturating_sub(5);
        }
    }

    fn start_file_browser(&mut self) -> Result<()> {
        let directory = env::current_dir().context("locate current directory")?;
        self.start_file_browser_at(&directory)
    }

    fn start_file_browser_at(&mut self, directory: &Path) -> Result<()> {
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

    fn browse_parent(&mut self) -> Result<()> {
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

    fn start_buffer_picker(&mut self) -> Result<()> {
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

    fn start_recent_picker(&mut self) -> Result<()> {
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

    fn start_jumplist_picker(&mut self) -> Result<()> {
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
                        column_utf16: false,
                        text: format!("jump {}", index + 1),
                    })
                })
                .collect()
        };
        self.start_location_picker(PickerSource::Jumps, "")
    }

    fn start_diagnostic_picker(&mut self) -> Result<()> {
        self.refresh_diagnostics()?;
        self.quickfix = self
            .diagnostics
            .iter()
            .map(DiagnosticEntry::quickfix)
            .collect();
        self.start_location_picker(PickerSource::Diagnostics, "")
    }

    fn start_location_picker(&mut self, source: PickerSource, query: &str) -> Result<()> {
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

    fn begin_path_picker(&mut self, query: &str) {
        self.picker_directory = None;
        self.prompt = Some(Prompt {
            kind: PromptKind::FilePicker,
            buffer: query.to_owned(),
            history_index: None,
        });
        self.update_file_picker();
    }

    fn start_grep_picker(&mut self, query: &str) -> Result<()> {
        self.last_picker_source = Some(PickerSource::Grep);
        self.prompt = Some(Prompt {
            kind: PromptKind::Grep,
            buffer: query.to_owned(),
            history_index: None,
        });
        self.picker_index = 0;
        self.update_grep_picker()
    }

    fn update_prompt_picker(&mut self) -> Result<()> {
        match self.prompt.as_ref().map(|prompt| prompt.kind) {
            Some(PromptKind::FilePicker | PromptKind::FileBrowser) => {
                self.update_file_picker();
                Ok(())
            }
            Some(PromptKind::Grep) => self.update_grep_picker(),
            Some(PromptKind::Location) => {
                self.update_location_picker();
                Ok(())
            }
            Some(PromptKind::Command) => {
                self.update_inccommand_preview();
                Ok(())
            }
            Some(PromptKind::SearchForward | PromptKind::SearchBackward) => {
                self.update_incremental_search();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn update_inccommand_preview(&mut self) {
        let Some(command) = self
            .prompt
            .as_ref()
            .filter(|prompt| prompt.kind == PromptKind::Command)
            .map(|prompt| prompt.buffer.clone())
        else {
            return;
        };
        let command = command
            .strip_prefix('%')
            .unwrap_or(&command)
            .strip_prefix('s');
        let Some(command) = command else {
            self.message.clear();
            return;
        };
        let Some(delimiter) = command.chars().next() else {
            self.message.clear();
            return;
        };
        let fields = command[delimiter.len_utf8()..]
            .split(delimiter)
            .collect::<Vec<_>>();
        let Some(pattern) = fields.first().filter(|pattern| !pattern.is_empty()) else {
            self.message.clear();
            return;
        };
        let replacement = fields.get(1).copied().unwrap_or_default();
        let text = self.active.editor.contents();
        let count = text.match_indices(pattern).count();
        let preview = text
            .lines()
            .find(|line| line.contains(pattern))
            .map(|line| line.replacen(pattern, replacement, 1));
        self.message = preview.map_or_else(
            || format!("inccommand: {count} match(es)"),
            |preview| format!("inccommand: {count} match(es) │ {}", compact(&preview, 80)),
        );
    }

    fn update_file_picker(&mut self) {
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

    fn update_grep_picker(&mut self) -> Result<()> {
        let Some(query) = self
            .prompt
            .as_ref()
            .filter(|prompt| prompt.kind == PromptKind::Grep)
            .map(|prompt| prompt.buffer.clone())
        else {
            return Ok(());
        };
        self.last_picker_query.clone_from(&query);
        let root = self.workspace_root();
        self.populate_grep_results(&query, &[], &root)?;
        self.picker_index = 0;
        self.update_grep_picker_message();
        self.refresh_picker_preview();
        Ok(())
    }

    fn open_selected_grep_result(&mut self, query: &str) -> Result<()> {
        let entry = self
            .quickfix
            .get(self.picker_index)
            .cloned()
            .ok_or_else(|| anyhow!("no grep matches for {query:?}"))?;
        self.navigate_to_entry(&entry)?;
        self.message = format!("{}:{}: {}", entry.line, entry.column, entry.text);
        Ok(())
    }

    fn open_selected_location(&mut self, query: &str) -> Result<()> {
        let entry = self
            .filtered_locations(query)
            .get(self.picker_index)
            .cloned()
            .ok_or_else(|| anyhow!("no matching location"))?;
        self.navigate_to_entry(&entry)?;
        self.message = format!("{}:{}: {}", entry.line, entry.column, entry.text);
        Ok(())
    }

    fn filtered_locations(&self, query: &str) -> Vec<QuickfixEntry> {
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

    fn update_location_picker(&mut self) {
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

    fn resume_picker(&mut self) -> Result<()> {
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

    fn word_under_cursor(&self) -> Option<String> {
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

    fn move_picker(&mut self, direction: isize) {
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

    fn update_grep_picker_message(&mut self) {
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

    fn update_file_picker_message(&mut self) {
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

    fn buffer(&self, buffer_id: BufferId) -> Option<&BufferState> {
        if self.active.buffer_id == buffer_id {
            Some(&self.active)
        } else {
            self.inactive
                .iter()
                .find(|buffer| buffer.buffer_id == buffer_id)
        }
    }

    fn move_prompt_history(&mut self, direction: isize) {
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
            | PromptKind::ConditionalBreakpoint
            | PromptKind::Ai => return,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct BufferDecorations {
    revision: DocumentRevision,
    spans: Vec<DecorationSpan>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderDemandKey {
    revision: DocumentRevision,
    visible_start: usize,
    visible_end: usize,
    near_start: usize,
    near_end: usize,
}

impl From<&ProviderRefresh> for ProviderDemandKey {
    fn from(refresh: &ProviderRefresh) -> Self {
        Self {
            revision: refresh.revision,
            visible_start: refresh.visible.start,
            visible_end: refresh.visible.end,
            near_start: refresh.near_viewport.start,
            near_end: refresh.near_viewport.end,
        }
    }
}

#[derive(Debug, Clone)]
struct ProviderRefresh {
    buffer_id: BufferId,
    document_id: DocumentId,
    revision: DocumentRevision,
    text: Box<str>,
    bundle: LanguageBundle,
    visible: Range<usize>,
    near_viewport: Range<usize>,
}

#[derive(Debug, Clone)]
struct ProviderCompletion {
    document_id: DocumentId,
    revision: DocumentRevision,
    text: Box<str>,
    bundle: LanguageBundle,
    byte: usize,
}

#[derive(Debug, Clone)]
struct LspCompletion {
    revision: DocumentRevision,
    replace: Range<usize>,
    candidates: Vec<CompletionCandidate>,
}

struct PersistentLsp {
    document_id: DocumentId,
    revision: DocumentRevision,
    uri: String,
    client: LspClient,
    server: LanguageServerInvocation,
    root: PathBuf,
    open_documents: BTreeMap<DocumentId, LspOpenDocument>,
    semantic_legend: Option<SemanticTokenLegend>,
    semantic_due: Option<Instant>,
}

#[derive(Debug, Clone)]
struct LspOpenDocument {
    uri: String,
    revision: DocumentRevision,
}

enum LspBackgroundOperation {
    Location {
        method: String,
    },
    Semantic {
        buffer_id: BufferId,
        revision: DocumentRevision,
        text: Box<str>,
        legend: SemanticTokenLegend,
    },
}

struct LspBackgroundResult {
    lsp: PersistentLsp,
    operation: LspBackgroundOperation,
    outcome: Result<serde_json::Value, String>,
}

enum ProviderWorkerMessage {
    Refresh(Box<ProviderRefresh>),
    Complete(Box<ProviderCompletion>),
    HighlightNow(Box<ImmediateHighlight>),
    Wake,
    Stop,
}

struct ImmediateHighlight {
    document_id: DocumentId,
    revision: DocumentRevision,
    text: Box<str>,
    bundle: LanguageBundle,
    reply: mpsc::Sender<Result<Vec<HighlightSpan>, String>>,
}

enum ProviderWorkerResult {
    Decorations {
        buffer_id: BufferId,
        document_id: DocumentId,
        revision: DocumentRevision,
        spans: Vec<HighlightSpan>,
        ranges: Vec<Range<usize>>,
    },
    Completion {
        document_id: DocumentId,
        session: CompletionSession,
    },
    Failed {
        document_id: DocumentId,
        message: String,
    },
}

struct ProviderWorker {
    sender: mpsc::SyncSender<ProviderWorkerMessage>,
    immediate_sender: mpsc::Sender<ProviderWorkerMessage>,
    results: mpsc::Receiver<ProviderWorkerResult>,
    join: Option<JoinHandle<()>>,
}

fn join_worker_thread(join: &mut Option<JoinHandle<()>>) {
    if let Some(join) = join.take() {
        let _ = join.join();
    }
}

impl ProviderWorker {
    fn start() -> Result<Self> {
        let (sender, requests) = mpsc::sync_channel(8);
        let (immediate_sender, immediate_requests) = mpsc::channel();
        let (results, receiver) = mpsc::channel();
        #[cfg(not(test))]
        let executable = env::current_exe().context("locate provider executable")?;
        let join = thread::Builder::new()
            .name("wren-provider-supervisor".to_owned())
            .spawn(move || {
                #[cfg(test)]
                provider_actor_loop(requests, immediate_requests, results);
                #[cfg(not(test))]
                provider_process_loop(executable, requests, immediate_requests, results);
            })
            .context("spawn provider supervisor")?;
        Ok(Self {
            sender,
            immediate_sender,
            results: receiver,
            join: Some(join),
        })
    }

    fn try_refresh(&self, refresh: ProviderRefresh) -> bool {
        self.sender
            .try_send(ProviderWorkerMessage::Refresh(Box::new(refresh)))
            .is_ok()
    }

    fn try_complete(&self, completion: ProviderCompletion) -> bool {
        self.sender
            .try_send(ProviderWorkerMessage::Complete(Box::new(completion)))
            .is_ok()
    }

    fn try_result(&self) -> Option<ProviderWorkerResult> {
        self.results.try_recv().ok()
    }

    fn highlight_now(
        &self,
        document_id: DocumentId,
        revision: DocumentRevision,
        text: Box<str>,
        bundle: LanguageBundle,
    ) -> Result<Vec<HighlightSpan>> {
        let (reply, response) = mpsc::channel();
        self.immediate_sender
            .send(ProviderWorkerMessage::HighlightNow(Box::new(
                ImmediateHighlight {
                    document_id,
                    revision,
                    text,
                    bundle,
                    reply,
                },
            )))
            .map_err(|_| anyhow!("provider process stopped"))?;
        // Wake an idle provider without putting the synchronous request behind
        // already queued viewport or completion work. A full background queue
        // already guarantees that the worker is awake.
        if matches!(
            self.sender.try_send(ProviderWorkerMessage::Wake),
            Err(mpsc::TrySendError::Disconnected(_))
        ) {
            return Err(anyhow!("provider process stopped"));
        }
        response
            .recv_timeout(Duration::from_millis(200))
            .map_err(|_| anyhow!("provider first-frame highlight timed out"))?
            .map_err(anyhow::Error::msg)
    }
}

impl Drop for ProviderWorker {
    fn drop(&mut self) {
        let _ = self.sender.send(ProviderWorkerMessage::Stop);
        join_worker_thread(&mut self.join);
    }
}

#[cfg(not(test))]
fn provider_process_loop(
    executable: PathBuf,
    requests: mpsc::Receiver<ProviderWorkerMessage>,
    immediate_requests: mpsc::Receiver<ProviderWorkerMessage>,
    results: mpsc::Sender<ProviderWorkerResult>,
) {
    let supervisor = ProviderSupervisor::spawn_with_args(&executable, ["--internal-provider-host"]);
    let mut supervisor = match supervisor {
        Ok(supervisor) => supervisor,
        Err(error) => {
            provider_failures_until_stop(
                &requests,
                &immediate_requests,
                &results,
                error.to_string(),
            );
            return;
        }
    };
    if let Err(error) = supervisor.request(&ProviderRequest::Hello { protocol: 1 }) {
        provider_failures_until_stop(&requests, &immediate_requests, &results, error.to_string());
        return;
    }
    provider_loop(requests, immediate_requests, results, |request| {
        supervisor.request(request)
    });
}

#[cfg(test)]
fn provider_actor_loop(
    requests: mpsc::Receiver<ProviderWorkerMessage>,
    immediate_requests: mpsc::Receiver<ProviderWorkerMessage>,
    results: mpsc::Sender<ProviderWorkerResult>,
) {
    let mut actor = ProviderActor::default();
    provider_loop(requests, immediate_requests, results, |request| {
        actor.handle(request.clone())
    });
}

fn provider_loop(
    requests: mpsc::Receiver<ProviderWorkerMessage>,
    immediate_requests: mpsc::Receiver<ProviderWorkerMessage>,
    results: mpsc::Sender<ProviderWorkerResult>,
    mut request: impl FnMut(&ProviderRequest) -> Result<ProviderResponse, wren_provider::ProviderError>,
) {
    // A viewport demand is not a document update. Keeping these identities
    // separate avoids serializing and reparsing the entire buffer on every
    // scroll while still replacing the provider snapshot on each revision.
    let mut uploaded =
        BTreeMap::<DocumentId, (DocumentRevision, wren_types::ProviderGeneration)>::new();
    loop {
        let message = match immediate_requests.try_recv() {
            Ok(message) => message,
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {
                let Ok(message) = requests.recv() else {
                    return;
                };
                message
            }
        };
        match message {
            ProviderWorkerMessage::Refresh(refresh) => {
                let refresh = *refresh;
                let identity = (refresh.revision, refresh.bundle.provider_generation());
                let demand = ProviderRequest::Demand {
                    document_id: refresh.document_id,
                    demand: ProviderDemand {
                        revision: refresh.revision,
                        visible: vec![refresh.visible],
                        near_viewport: vec![refresh.near_viewport],
                        priority: Priority::Visible,
                    },
                };
                let outcome = if uploaded.get(&refresh.document_id) == Some(&identity) {
                    request(&demand)
                } else {
                    let update = request(&ProviderRequest::UpdateDocument {
                        document_id: refresh.document_id,
                        revision: refresh.revision,
                        text: refresh.text,
                        bundle: refresh.bundle,
                    });
                    match update {
                        Ok(ProviderResponse::Updated { .. }) => {
                            uploaded.insert(refresh.document_id, identity);
                            request(&demand)
                        }
                        Ok(response) => Err(wren_provider::ProviderError::Json(
                            serde_json::Error::io(std::io::Error::other(format!(
                                "unexpected update response {response:?}"
                            ))),
                        )),
                        Err(error) => Err(error),
                    }
                };
                let result = match outcome {
                    Ok(ProviderResponse::Highlight(highlight))
                        if highlight.freshness == Freshness::Fresh
                            && matches!(
                                highlight.key,
                                FreshnessKey::Document { document_revision, .. }
                                    if document_revision == refresh.revision
                            ) =>
                    {
                        ProviderWorkerResult::Decorations {
                            buffer_id: refresh.buffer_id,
                            document_id: refresh.document_id,
                            revision: refresh.revision,
                            spans: highlight.spans,
                            ranges: highlight.requested_ranges,
                        }
                    }
                    Ok(response) => ProviderWorkerResult::Failed {
                        document_id: refresh.document_id,
                        message: format!("stale or unexpected response {response:?}"),
                    },
                    Err(error) => ProviderWorkerResult::Failed {
                        document_id: refresh.document_id,
                        message: error.to_string(),
                    },
                };
                if matches!(&result, ProviderWorkerResult::Failed { .. }) {
                    uploaded.remove(&refresh.document_id);
                }
                if results.send(result).is_err() {
                    return;
                }
            }
            ProviderWorkerMessage::Complete(completion) => {
                let completion = *completion;
                let identity = (completion.revision, completion.bundle.provider_generation());
                let complete = ProviderRequest::Complete {
                    document_id: completion.document_id,
                    revision: completion.revision,
                    byte: completion.byte,
                };
                let outcome = if uploaded.get(&completion.document_id) == Some(&identity) {
                    request(&complete)
                } else {
                    let update = request(&ProviderRequest::UpdateDocument {
                        document_id: completion.document_id,
                        revision: completion.revision,
                        text: completion.text,
                        bundle: completion.bundle,
                    });
                    match update {
                        Ok(ProviderResponse::Updated { .. }) => {
                            uploaded.insert(completion.document_id, identity);
                            request(&complete)
                        }
                        Ok(response) => Err(wren_provider::ProviderError::Json(
                            serde_json::Error::io(std::io::Error::other(format!(
                                "unexpected completion update response {response:?}"
                            ))),
                        )),
                        Err(error) => Err(error),
                    }
                };
                let result = match outcome {
                    Ok(ProviderResponse::Completion(result))
                        if result.freshness == Freshness::Fresh =>
                    {
                        ProviderWorkerResult::Completion {
                            document_id: completion.document_id,
                            session: CompletionSession {
                                revision: completion.revision,
                                replace: result.replace,
                                candidates: result.candidates,
                            },
                        }
                    }
                    Ok(response) => ProviderWorkerResult::Failed {
                        document_id: completion.document_id,
                        message: format!("stale or unexpected completion {response:?}"),
                    },
                    Err(error) => ProviderWorkerResult::Failed {
                        document_id: completion.document_id,
                        message: error.to_string(),
                    },
                };
                if matches!(&result, ProviderWorkerResult::Failed { .. }) {
                    uploaded.remove(&completion.document_id);
                }
                if results.send(result).is_err() {
                    return;
                }
            }
            ProviderWorkerMessage::HighlightNow(highlight) => {
                let ImmediateHighlight {
                    document_id,
                    revision,
                    text,
                    bundle,
                    reply,
                } = *highlight;
                let identity = (revision, bundle.provider_generation());
                let text_len = text.len();
                let update = request(&ProviderRequest::UpdateDocument {
                    document_id,
                    revision,
                    text,
                    bundle,
                });
                let outcome = match update {
                    Ok(ProviderResponse::Updated { .. }) => {
                        uploaded.insert(document_id, identity);
                        request(&ProviderRequest::Demand {
                            document_id,
                            demand: ProviderDemand {
                                revision,
                                visible: std::iter::once(0..text_len).collect(),
                                near_viewport: Vec::new(),
                                priority: Priority::Visible,
                            },
                        })
                    }
                    Ok(response) => Err(wren_provider::ProviderError::Json(serde_json::Error::io(
                        std::io::Error::other(format!(
                            "unexpected immediate highlight response {response:?}"
                        )),
                    ))),
                    Err(error) => Err(error),
                };
                let result = match outcome {
                    Ok(ProviderResponse::Highlight(highlight))
                        if highlight.freshness == Freshness::Fresh =>
                    {
                        Ok(highlight.spans)
                    }
                    Ok(response) => Err(format!("stale or unexpected highlight {response:?}")),
                    Err(error) => Err(error.to_string()),
                };
                if result.is_err() {
                    uploaded.remove(&document_id);
                }
                let _ = reply.send(result);
            }
            ProviderWorkerMessage::Wake => {}
            ProviderWorkerMessage::Stop => return,
        }
    }
}

#[cfg(not(test))]
fn provider_failures_until_stop(
    requests: &mpsc::Receiver<ProviderWorkerMessage>,
    immediate_requests: &mpsc::Receiver<ProviderWorkerMessage>,
    results: &mpsc::Sender<ProviderWorkerResult>,
    message: String,
) {
    loop {
        let request = match immediate_requests.try_recv() {
            Ok(request) => request,
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {
                let Ok(request) = requests.recv() else {
                    return;
                };
                request
            }
        };
        match request {
            ProviderWorkerMessage::Refresh(refresh) => {
                let _ = results.send(ProviderWorkerResult::Failed {
                    document_id: refresh.document_id,
                    message: message.clone(),
                });
            }
            ProviderWorkerMessage::Complete(completion) => {
                let _ = results.send(ProviderWorkerResult::Failed {
                    document_id: completion.document_id,
                    message: message.clone(),
                });
            }
            ProviderWorkerMessage::HighlightNow(highlight) => {
                let ImmediateHighlight {
                    document_id, reply, ..
                } = *highlight;
                let _ = reply.send(Err(format!("document {document_id:?}: {message}")));
            }
            ProviderWorkerMessage::Wake => {}
            ProviderWorkerMessage::Stop => break,
        }
    }
}

fn provider_decoration(span: HighlightSpan, theme: CatppuccinPalette) -> DecorationSpan {
    let style = match span.kind.as_ref() {
        "keyword" | "conditional" | "repeat" | "exception" | "type.qualifier"
        | "type.definition" | "storage" => CellStyle {
            bold: true,
            foreground: Some(CellColor::Rgb(theme.mauve)),
            ..CellStyle::default()
        },
        "comment" => CellStyle {
            italic: true,
            foreground: Some(CellColor::Rgb(theme.overlay2)),
            ..CellStyle::default()
        },
        "preproc" | "attribute" | "include" | "constant.macro" | "function.macro" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.pink)),
            ..CellStyle::default()
        },
        "operator" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.sky)),
            ..CellStyle::default()
        },
        kind if kind.starts_with("punctuation") => CellStyle {
            foreground: Some(CellColor::Rgb(theme.overlay2)),
            ..CellStyle::default()
        },
        "string" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.green)),
            ..CellStyle::default()
        },
        "escape" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.pink)),
            ..CellStyle::default()
        },
        "character" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.teal)),
            ..CellStyle::default()
        },
        "boolean" | "number" | "constant" | "constant.builtin" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.peach)),
            ..CellStyle::default()
        },
        "type" | "type.builtin" | "constructor" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.yellow)),
            ..CellStyle::default()
        },
        "function" | "function.builtin" | "method" | "tag" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.blue)),
            ..CellStyle::default()
        },
        "parameter" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.maroon)),
            ..CellStyle::default()
        },
        "property" | "field" | "tag.attribute" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.sapphire)),
            ..CellStyle::default()
        },
        "variable.builtin" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.red)),
            ..CellStyle::default()
        },
        "namespace" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.pink)),
            ..CellStyle::default()
        },
        "variable" => CellStyle {
            foreground: Some(CellColor::Rgb(theme.text)),
            ..CellStyle::default()
        },
        _ => CellStyle {
            foreground: Some(CellColor::Rgb(theme.text)),
            ..CellStyle::default()
        },
    };
    DecorationSpan {
        range: span.range,
        style,
    }
}

fn markdown_decorations(text: &str, theme: CatppuccinPalette) -> Vec<DecorationSpan> {
    let mut spans = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let indentation = line.len().saturating_sub(trimmed.len());
        if trimmed.starts_with('#') {
            spans.push(DecorationSpan {
                range: offset + indentation..offset + line.trim_end().len(),
                style: CellStyle {
                    bold: true,
                    foreground: Some(CellColor::Rgb(theme.mauve)),
                    ..CellStyle::default()
                },
            });
        } else if trimmed.starts_with("> ") {
            spans.push(DecorationSpan {
                range: offset + indentation..offset + line.trim_end().len(),
                style: CellStyle {
                    italic: true,
                    foreground: Some(CellColor::Rgb(theme.overlay2)),
                    ..CellStyle::default()
                },
            });
        }
        for (delimiter, mut style) in [
            (
                "**",
                CellStyle {
                    bold: true,
                    ..CellStyle::default()
                },
            ),
            (
                "~~",
                CellStyle {
                    strikethrough: true,
                    ..CellStyle::default()
                },
            ),
            (
                "`",
                CellStyle {
                    foreground: Some(CellColor::Rgb(theme.green)),
                    background: Some(CellColor::Rgb(theme.surface0)),
                    ..CellStyle::default()
                },
            ),
        ] {
            let mut search = 0;
            while let Some(start) = line[search..].find(delimiter) {
                let start = search + start;
                let content_start = start + delimiter.len();
                let Some(end) = line[content_start..].find(delimiter) else {
                    break;
                };
                let end = content_start + end + delimiter.len();
                if delimiter == "`" {
                    style.italic = false;
                }
                spans.push(DecorationSpan {
                    range: offset + start..offset + end,
                    style,
                });
                search = end;
            }
        }
        offset += line.len();
    }
    spans
}

fn lsp_popup_markdown(markdown: &str, theme: CatppuccinPalette) -> (String, Vec<DecorationSpan>) {
    let mut text = String::new();
    let mut code_block = None::<(usize, String)>;
    let mut code_spans = Vec::new();
    for line in markdown.split_inclusive('\n') {
        let trimmed = line.trim();
        if let Some(fence) = trimmed.strip_prefix("```") {
            if let Some((start, language)) = code_block.take() {
                let _language = normalized_fence_language(&language);
                code_spans.extend(lexical_highlight_text(&text[start..]).into_iter().map(
                    |mut span| {
                        span.range = start + span.range.start..start + span.range.end;
                        provider_decoration(span, theme)
                    },
                ));
            } else {
                code_block = Some((text.len(), fence.trim().to_owned()));
            }
            continue;
        }
        text.push_str(line);
    }
    if let Some((start, language)) = code_block {
        let _language = normalized_fence_language(&language);
        code_spans.extend(
            lexical_highlight_text(&text[start..])
                .into_iter()
                .map(|mut span| {
                    span.range = start + span.range.start..start + span.range.end;
                    provider_decoration(span, theme)
                }),
        );
    }
    while text.ends_with('\n') {
        text.pop();
    }
    let mut decorations = markdown_decorations(&text, theme);
    decorations.extend(code_spans);
    (text, decorations)
}

fn normalized_fence_language(language: &str) -> &str {
    match language.trim().to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "js" | "jsx" | "ts" | "tsx" => "javascript",
        "py" => "python",
        "sh" | "shell" | "zsh" => "bash",
        "hs" => "haskell",
        _ => language.trim(),
    }
}

fn language_bundle(path: Option<&Path>) -> LanguageBundle {
    let language_id = path
        .and_then(Path::extension)
        .and_then(std::ffi::OsStr::to_str)
        .map_or("text", |extension| match extension {
            "rs" => "rust",
            "py" => "python",
            "js" | "mjs" | "cjs" => "javascript",
            "ts" | "tsx" => "typescript",
            "go" => "go",
            "c" | "h" => "c",
            "cc" | "cpp" | "cxx" | "hpp" | "msg" => "cpp",
            "java" => "java",
            "rb" => "ruby",
            "sh" | "bash" | "zsh" => "bash",
            "json" => "json",
            "hs" | "lhs" => "haskell",
            "lua" => "lua",
            "nix" => "nix",
            "tf" | "tfvars" => "terraform",
            "md" | "markdown" => "markdown",
            _ => "text",
        });
    let mut identity = [0_u8; 32];
    for (index, byte) in language_id.bytes().enumerate() {
        identity[index % identity.len()] ^= byte;
    }
    LanguageBundle {
        language_id: language_id.into(),
        grammar_hash: identity,
        grammar_abi: 15,
        grammar_semver: "bundled".into(),
        highlight_query_hash: identity,
        object_query_hash: identity,
        outline_query_hash: identity,
        injection_query_hash: identity,
        config_schema_version: 1,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FormatterInvocation {
    program: String,
    arguments: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IndentStyle {
    expand_tabs: bool,
    width: usize,
}

fn detect_indent_style(text: &str) -> IndentStyle {
    let mut tab_lines = 0;
    let mut widths = Vec::new();
    for line in text.lines().take(2_000) {
        if line.starts_with('\t') {
            tab_lines += 1;
            continue;
        }
        let width = line.bytes().take_while(|byte| *byte == b' ').count();
        if width > 0 && !line.trim().is_empty() {
            widths.push(width);
        }
    }
    if tab_lines > widths.len() {
        return IndentStyle {
            expand_tabs: false,
            width: 2,
        };
    }
    let width = widths
        .into_iter()
        .filter(|width| *width <= 8)
        .reduce(greatest_common_divisor)
        .filter(|width| *width > 1)
        .unwrap_or(2)
        .clamp(2, 8);
    IndentStyle {
        expand_tabs: true,
        width,
    }
}

fn greatest_common_divisor(mut left: usize, mut right: usize) -> usize {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

fn wrap_editor_text(source: &str, width: usize) -> String {
    let first = source.lines().next().unwrap_or_default();
    let indentation = &first[..first.len().saturating_sub(first.trim_start().len())];
    let trimmed = first.trim_start();
    let marker = ["// ", "# ", "-- ", "* "]
        .into_iter()
        .find(|marker| trimmed.starts_with(marker))
        .unwrap_or("");
    let prefix = format!("{indentation}{marker}");
    let words = source
        .lines()
        .flat_map(|line| {
            let line = line.trim_start();
            line.strip_prefix(marker).unwrap_or(line).split_whitespace()
        })
        .collect::<Vec<_>>();
    if words.is_empty() {
        return source.to_owned();
    }
    let mut output = prefix.clone();
    let mut column = prefix.chars().count();
    for word in words {
        let separator = usize::from(column > prefix.chars().count());
        if column + separator + word.chars().count() > width && column > prefix.chars().count() {
            output.push('\n');
            output.push_str(&prefix);
            output.push_str(word);
            column = prefix.chars().count() + word.chars().count();
        } else {
            if separator == 1 {
                output.push(' ');
                column += 1;
            }
            output.push_str(word);
            column += word.chars().count();
        }
    }
    if source.ends_with('\n') {
        output.push('\n');
    }
    output
}

fn formatter_invocation(path: &Path) -> Option<FormatterInvocation> {
    let extension = path.extension()?.to_str()?;
    let filename = path.to_string_lossy().into_owned();
    let (program, arguments) = match extension {
        "rs" => ("rustfmt", vec!["--emit=stdout".to_owned()]),
        "py" => (
            "ruff",
            vec![
                "format".to_owned(),
                "--stdin-filename".to_owned(),
                filename,
                "-".to_owned(),
            ],
        ),
        "go" => ("gofmt", Vec::new()),
        "tf" | "tfvars" => ("terraform", vec!["fmt".to_owned(), "-".to_owned()]),
        "nix" => ("nixfmt", Vec::new()),
        "hs" | "lhs" => ("fourmolu", vec!["--stdin-input-file".to_owned(), filename]),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "msg" => {
            let mut arguments = vec!["--mode=c".to_owned(), "--suffix=none".to_owned()];
            if let Some(options) = find_upward(path, ".astylerc") {
                arguments.push(format!("--options={}", options.display()));
            }
            ("astyle", arguments)
        }
        _ => return None,
    };
    Some(FormatterInvocation {
        program: program.to_owned(),
        arguments,
    })
}

fn find_upward(path: &Path, name: &str) -> Option<PathBuf> {
    let mut directory = path.parent();
    while let Some(current) = directory {
        let candidate = current.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        directory = current.parent();
    }
    None
}

fn executable_exists(program: &str) -> bool {
    if program.contains(std::path::MAIN_SEPARATOR) {
        return Path::new(program).is_file();
    }
    env::var_os("PATH").is_some_and(|path| {
        env::split_paths(&path).any(|directory| directory.join(program).is_file())
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiagnosticInvocation {
    program: String,
    arguments: Vec<String>,
    directory: PathBuf,
}

fn diagnostic_invocation(path: &Path, workspace_root: &Path) -> Option<DiagnosticInvocation> {
    let extension = path.extension()?.to_str()?;
    let file = path.to_string_lossy().into_owned();
    let (program, arguments, directory) = match extension {
        "rs" => (
            "cargo",
            vec![
                "clippy".to_owned(),
                "--quiet".to_owned(),
                "--message-format=short".to_owned(),
            ],
            workspace_root.to_path_buf(),
        ),
        "py" => (
            "ruff",
            vec![
                "check".to_owned(),
                "--output-format=concise".to_owned(),
                file,
            ],
            workspace_root.to_path_buf(),
        ),
        "js" | "jsx" | "ts" | "tsx" => (
            "pnpm",
            vec![
                "exec".to_owned(),
                "tsc".to_owned(),
                "--noEmit".to_owned(),
                "--pretty".to_owned(),
                "false".to_owned(),
            ],
            workspace_root.to_path_buf(),
        ),
        "go" => (
            "go",
            vec!["vet".to_owned(), "./...".to_owned()],
            workspace_root.to_path_buf(),
        ),
        "tf" | "tfvars" => (
            "terraform",
            vec!["validate".to_owned(), "-no-color".to_owned()],
            path.parent().unwrap_or(workspace_root).to_path_buf(),
        ),
        "nix" => (
            "nix-instantiate",
            vec!["--parse".to_owned(), file],
            workspace_root.to_path_buf(),
        ),
        "hs" | "lhs" => (
            "ghc",
            vec!["-fno-code".to_owned(), file],
            workspace_root.to_path_buf(),
        ),
        "lua" => (
            "luac",
            vec!["-p".to_owned(), file],
            workspace_root.to_path_buf(),
        ),
        "sh" | "bash" | "zsh" => (
            "bash",
            vec!["-n".to_owned(), file],
            workspace_root.to_path_buf(),
        ),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "msg" => (
            "clang++",
            vec!["-fsyntax-only".to_owned(), file],
            workspace_root.to_path_buf(),
        ),
        _ => return None,
    };
    Some(DiagnosticInvocation {
        program: program.to_owned(),
        arguments,
        directory,
    })
}

fn parse_diagnostic_line(line: &str, directory: &Path) -> Option<DiagnosticEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let mut parts = line.splitn(4, ':');
    let raw_path = parts.next()?.trim();
    let line_number = parts.next()?.trim().parse::<usize>().ok()?;
    let column = parts
        .next()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(1);
    let message = parts.next().unwrap_or_default().trim();
    let path = PathBuf::from(raw_path);
    let path = if path.is_absolute() {
        path
    } else {
        directory.join(path)
    };
    let lowercase = message.to_ascii_lowercase();
    let severity = if lowercase.contains("error") {
        DiagnosticSeverity::Error
    } else if lowercase.contains("warning") || lowercase.contains("warn") {
        DiagnosticSeverity::Warning
    } else if lowercase.contains("hint") {
        DiagnosticSeverity::Hint
    } else {
        DiagnosticSeverity::Information
    };
    Some(DiagnosticEntry {
        path,
        line: line_number.max(1),
        column: column.max(1),
        severity,
        message: message.to_owned(),
    })
}

fn git_root_for(path: &Path) -> Result<PathBuf> {
    let directory = path.parent().unwrap_or(path);
    let output = Command::new("git")
        .current_dir(directory)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("locate Git root")?;
    if !output.status.success() {
        bail!("not inside a Git repository");
    }
    let root = String::from_utf8(output.stdout).context("Git root is not UTF-8")?;
    Ok(PathBuf::from(root.trim()))
}

fn git_branch_for(path: &Path) -> Option<String> {
    let root = git_root_for(path).ok()?;
    let output = Command::new("git")
        .current_dir(root)
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return Some("HEAD".to_owned());
    }
    let branch = String::from_utf8(output.stdout).ok()?;
    Some(branch.trim().to_owned())
}

fn git_index_contents(root: &Path, relative: &Path) -> Result<String> {
    let output = Command::new("git")
        .current_dir(root)
        .arg("show")
        .arg(format!(":{}", relative.to_string_lossy()))
        .output()
        .context("read file from Git index")?;
    if output.status.success() {
        return String::from_utf8(output.stdout).context("Git index contents are not UTF-8");
    }
    if git_path_tracked(root, relative)? {
        bail!(
            "read Git index: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::new())
}

fn git_path_tracked(root: &Path, relative: &Path) -> Result<bool> {
    Ok(Command::new("git")
        .current_dir(root)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(relative)
        .output()
        .context("check whether Git tracks file")?
        .status
        .success())
}

fn make_git_patch(root: &Path, relative: &Path, before: &str, after: &str) -> Result<Vec<u8>> {
    if before == after {
        return Ok(Vec::new());
    }
    let mut old = tempfile::NamedTempFile::new().context("create old Git hunk input")?;
    let mut new = tempfile::NamedTempFile::new().context("create new Git hunk input")?;
    old.write_all(before.as_bytes())?;
    new.write_all(after.as_bytes())?;
    let output = Command::new("git")
        .current_dir(root)
        .args(["diff", "--no-index", "--no-color", "--unified=0", "--"])
        .arg(old.path())
        .arg(new.path())
        .output()
        .context("compute Git-compatible buffer patch")?;
    if !output.status.success() && output.status.code() != Some(1) {
        bail!(
            "git diff: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let relative = relative.to_string_lossy();
    let diff = String::from_utf8(output.stdout).context("git diff returned non-UTF-8")?;
    let mut rewritten = String::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            rewritten.push_str(&format!("diff --git a/{relative} b/{relative}\n"));
        } else if line.starts_with("--- ") {
            rewritten.push_str(&format!("--- a/{relative}\n"));
        } else if line.starts_with("+++ ") {
            rewritten.push_str(&format!("+++ b/{relative}\n"));
        } else {
            rewritten.push_str(line);
            rewritten.push('\n');
        }
    }
    Ok(rewritten.into_bytes())
}

fn select_git_hunk(
    patch: &[u8],
    cursor_line: usize,
    selected_lines: Option<&Range<usize>>,
) -> Result<Vec<u8>> {
    let patch = std::str::from_utf8(patch).context("Git patch is not UTF-8")?;
    if patch.is_empty() {
        bail!("buffer has no Git changes");
    }
    let lines = patch.lines().collect::<Vec<_>>();
    let header_end = lines
        .iter()
        .position(|line| line.starts_with("@@"))
        .ok_or_else(|| anyhow!("Git patch contains no hunks"))?;
    let mut selected = None;
    let mut index = header_end;
    while index < lines.len() {
        let end = lines[index + 1..]
            .iter()
            .position(|line| line.starts_with("@@"))
            .map_or(lines.len(), |offset| index + 1 + offset);
        let range = parse_git_after_range(lines[index])?;
        let matches = selected_lines.map_or_else(
            || {
                let effective_end = range.end.max(range.start.saturating_add(1));
                range.start <= cursor_line && cursor_line < effective_end
            },
            |selection| {
                let effective_end = range.end.max(range.start.saturating_add(1));
                range.start < selection.end && selection.start < effective_end
            },
        );
        if matches {
            selected = Some(index..end);
            break;
        }
        index = end;
    }
    let selected = selected.ok_or_else(|| anyhow!("cursor is not in a changed Git hunk"))?;
    let mut output = String::new();
    for line in lines[..header_end].iter().chain(lines[selected].iter()) {
        output.push_str(line);
        output.push('\n');
    }
    Ok(output.into_bytes())
}

fn parse_git_after_range(header: &str) -> Result<Range<usize>> {
    let after = header
        .split_whitespace()
        .find(|field| field.starts_with('+'))
        .ok_or_else(|| anyhow!("invalid Git hunk header {header:?}"))?
        .trim_start_matches('+');
    let mut values = after.split(',');
    let start = values
        .next()
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| anyhow!("invalid Git hunk range {after:?}"))?;
    let count = values
        .next()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1);
    Ok(start..start.saturating_add(count))
}

fn byte_range_of_lines(text: &str, lines: Range<usize>) -> Range<usize> {
    fn line_byte(text: &str, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        text.match_indices('\n')
            .nth(line - 1)
            .map_or(text.len(), |(byte, _)| byte + 1)
    }
    line_byte(text, lines.start)..line_byte(text, lines.end)
}

fn git_apply_patch(root: &Path, patch: &[u8], cached: bool, reverse: bool) -> Result<()> {
    if patch.is_empty() {
        bail!("Git patch is empty");
    }
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args(["apply", "--unidiff-zero", "--whitespace=nowarn"]);
    if cached {
        command.arg("--cached");
    }
    if reverse {
        command.arg("--reverse");
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start git apply")?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("git apply stdin unavailable"))?
        .write_all(patch)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "git apply: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn url_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[derive(Debug, Clone, PartialEq)]
struct LanguageServerInvocation {
    program: String,
    arguments: Vec<String>,
    language_id: String,
    settings: serde_json::Value,
}

fn spawn_lsp_client(
    server: &LanguageServerInvocation,
    path: &Path,
    root: &Path,
    revision: DocumentRevision,
    text: &str,
    environment: BTreeMap<Box<str>, Box<str>>,
) -> Result<(LspClient, String, Option<SemanticTokenLegend>)> {
    let spec = WorkflowTaskSpec {
        program: server.program.clone().into(),
        arguments: server
            .arguments
            .iter()
            .cloned()
            .map(String::into_boxed_str)
            .collect(),
        environment,
        visibility: DocumentVisibility::Persisted,
        save: SavePolicy::Never,
        max_output_bytes: 16 * 1024 * 1024,
    };
    let mut client = LspClient::spawn(&spec, true, 16 * 1024 * 1024)?;
    let initialize = client.initialize(
        &file_uri(root),
        serde_json::json!({
            "workspace": {"workspaceFolders": true},
            "textDocument": {
                "hover": {"contentFormat": ["markdown", "plaintext"]},
                "signatureHelp": {"signatureInformation": {"documentationFormat": ["markdown", "plaintext"]}},
                "completion": {"completionItem": {"snippetSupport": true, "documentationFormat": ["markdown", "plaintext"]}},
                "publishDiagnostics": {"relatedInformation": true},
                "codeAction": {"dynamicRegistration": true},
                "rename": {"prepareSupport": true},
                "semanticTokens": {
                    "dynamicRegistration": true,
                    "requests": {"range": false, "full": true},
                    "tokenTypes": [
                        "namespace", "type", "class", "enum", "interface", "struct",
                        "typeParameter", "parameter", "variable", "property", "enumMember",
                        "event", "function", "method", "macro", "keyword", "modifier",
                        "comment", "string", "number", "regexp", "operator", "decorator"
                    ],
                    "tokenModifiers": [
                        "declaration", "definition", "readonly", "static", "deprecated",
                        "abstract", "async", "modification", "documentation", "defaultLibrary"
                    ],
                    "formats": ["relative"],
                    "overlappingTokenSupport": false,
                    "multilineTokenSupport": false
                }
            }
        }),
    )?;
    if !server.settings.is_null() {
        client.notify(
            "workspace/didChangeConfiguration",
            serde_json::json!({"settings": server.settings}),
        )?;
    }
    let uri = file_uri(path);
    client.did_open(
        &uri,
        &server.language_id,
        i64::try_from(revision.get()).unwrap_or(i64::MAX),
        text,
    )?;
    Ok((client, uri, semantic_token_legend(&initialize)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SemanticTokenLegend {
    token_types: Vec<String>,
    token_modifiers: Vec<String>,
}

fn semantic_token_legend(initialize: &serde_json::Value) -> Option<SemanticTokenLegend> {
    let legend = initialize.pointer("/capabilities/semanticTokensProvider/legend")?;
    Some(SemanticTokenLegend {
        token_types: legend
            .get("tokenTypes")?
            .as_array()?
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect(),
        token_modifiers: legend
            .get("tokenModifiers")?
            .as_array()?
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect(),
    })
}

fn parse_semantic_tokens(
    text: &str,
    response: &serde_json::Value,
    legend: &SemanticTokenLegend,
) -> Vec<HighlightSpan> {
    let Some(data) = response.get("data").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let mut spans = Vec::with_capacity(data.len() / 5);
    let mut line = 0_u32;
    let mut character = 0_u32;
    for token in data.chunks_exact(5) {
        let Some(delta_line) = token[0]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
        else {
            continue;
        };
        let Some(delta_start) = token[1]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
        else {
            continue;
        };
        let Some(length) = token[2]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
        else {
            continue;
        };
        let Some(token_type) = token[3]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
        else {
            continue;
        };
        let modifiers = token[4].as_u64().unwrap_or(0);
        line = line.saturating_add(delta_line);
        character = if delta_line == 0 {
            character.saturating_add(delta_start)
        } else {
            delta_start
        };
        let Some(start) = lsp_position_byte(text, line, character) else {
            continue;
        };
        let Some(end) = lsp_position_byte(text, line, character.saturating_add(length)) else {
            continue;
        };
        if start >= end {
            continue;
        }
        let token_type = legend
            .token_types
            .get(token_type)
            .map(String::as_str)
            .unwrap_or("variable");
        let has_modifier = |name: &str| {
            legend
                .token_modifiers
                .iter()
                .position(|modifier| modifier == name)
                .and_then(|index| u32::try_from(index).ok())
                .is_some_and(|index| modifiers & 1_u64.checked_shl(index).unwrap_or(0) != 0)
        };
        let kind = match token_type {
            "namespace" => "namespace",
            "type" | "class" | "enum" | "interface" | "struct" | "typeParameter" => "type",
            "parameter" => "parameter",
            "variable" if has_modifier("defaultLibrary") => "variable.builtin",
            "variable" if has_modifier("readonly") => "constant",
            "variable" => "variable",
            "property" | "event" => "property",
            "enumMember" => "constant",
            "function" if has_modifier("defaultLibrary") => "function.builtin",
            "function" => "function",
            "method" => "method",
            "macro" => "function.macro",
            "keyword" => "keyword",
            "modifier" => "type.qualifier",
            "comment" => "comment",
            "string" | "regexp" => "string",
            "number" => "number",
            "operator" => "operator",
            "decorator" => "attribute",
            _ => "variable",
        };
        spans.push(HighlightSpan {
            range: start..end,
            kind: kind.into(),
        });
    }
    spans
}

fn lsp_position_byte(text: &str, line: u32, character: u32) -> Option<usize> {
    let line = usize::try_from(line).ok()?;
    let start = if line == 0 {
        0
    } else {
        text.match_indices('\n').nth(line - 1)?.0.saturating_add(1)
    };
    let end = text[start..]
        .find('\n')
        .map_or(text.len(), |offset| start + offset);
    let wanted = usize::try_from(character).ok()?;
    let mut utf16 = 0;
    for (offset, current) in text[start..end].char_indices() {
        if utf16 == wanted {
            return Some(start + offset);
        }
        utf16 = utf16.saturating_add(current.len_utf16());
        if utf16 > wanted {
            return None;
        }
    }
    (utf16 == wanted).then_some(end)
}

fn language_server_invocation(path: Option<&Path>) -> Option<LanguageServerInvocation> {
    let path = path?;
    let extension = path.extension()?.to_str()?;
    let (program, arguments, language_id, settings) = match extension {
        "rs" => (
            "rust-analyzer",
            Vec::new(),
            "rust",
            serde_json::json!({"rust-analyzer": {"check": {"command": "clippy"}}}),
        ),
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" => (
            "pnpm",
            vec![
                "exec".to_owned(),
                "typescript-language-server".to_owned(),
                "--stdio".to_owned(),
            ],
            match extension {
                "ts" => "typescript",
                "tsx" => "typescriptreact",
                "jsx" => "javascriptreact",
                _ => "javascript",
            },
            serde_json::json!({
                "typescript": {
                    "suggest": {"completeFunctionCalls": true},
                    "updateImportsOnFileMove": {"enabled": "always"}
                },
                "javascript": {
                    "suggest": {"completeFunctionCalls": true},
                    "updateImportsOnFileMove": {"enabled": "always"}
                }
            }),
        ),
        "py" => {
            let interpreter = python_interpreter(path);
            (
                "basedpyright-langserver",
                vec!["--stdio".to_owned()],
                "python",
                serde_json::json!({
                    "basedpyright": {
                        "pythonPath": interpreter,
                        "analysis": {
                            "diagnosticMode": "workspace",
                            "inlayHints": {
                                "variableTypes": true,
                                "callArgumentNames": true,
                                "functionReturnTypes": true,
                                "genericTypes": true
                            }
                        },
                        "disableOrganizeImports": true
                    }
                }),
            )
        }
        "go" => ("gopls", Vec::new(), "go", serde_json::json!({})),
        "tf" | "tfvars" => (
            "terraform-ls",
            vec!["serve".to_owned()],
            "terraform",
            serde_json::json!({}),
        ),
        "nix" => {
            let input = env::var("NIXD_NIXPKGS_INPUT").unwrap_or_else(|_| "nixpkgs".to_owned());
            let expression = nixd_expression_path().map(|config| {
                format!(
                    "let ctx = import {} {{ self = \"dummy\"; }}; in if ctx.local != null then ctx.local.inputs.{input} else import <nixpkgs> {{ }}",
                    config.display()
                )
            });
            (
                "nixd",
                Vec::new(),
                "nix",
                serde_json::json!({
                    "nixd": {
                        "nixpkgs": {"expr": expression},
                        "formatting": {"command": ["nixfmt"]}
                    }
                }),
            )
        }
        "hs" | "lhs" => (
            "haskell-language-server-wrapper",
            vec!["--lsp".to_owned()],
            "haskell",
            serde_json::json!({
                "haskell": {
                    "plugin": {"hlint": {"diagnosticsOn": true, "codeActionsOn": true}},
                    "formattingProvider": "fourmolu"
                }
            }),
        ),
        "lua" => (
            "lua-language-server",
            Vec::new(),
            "lua",
            serde_json::json!({
                "Lua": {
                    "runtime": {"version": "LuaJIT"},
                    "workspace": {"checkThirdParty": false}
                }
            }),
        ),
        "sh" | "bash" | "zsh" => (
            "bash-language-server",
            vec!["start".to_owned()],
            "shellscript",
            serde_json::json!({}),
        ),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "msg" => {
            ("clangd", Vec::new(), "cpp", serde_json::json!({}))
        }
        _ => return None,
    };
    Some(LanguageServerInvocation {
        program: program.to_owned(),
        arguments,
        language_id: language_id.to_owned(),
        settings,
    })
}

fn python_interpreter(path: &Path) -> Option<String> {
    if let Some(virtual_environment) = env::var_os("VIRTUAL_ENV") {
        let candidate = PathBuf::from(virtual_environment).join("bin/python");
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    let mut directory = path.parent();
    while let Some(current) = directory {
        for name in [".venv", "venv"] {
            let candidate = current.join(name).join("bin/python");
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
        directory = current.parent();
    }
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join("python3"))
            .find(|candidate| candidate.is_file())
            .map(|candidate| candidate.to_string_lossy().into_owned())
    })
}

fn nixd_expression_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(config) = env::var_os("XDG_CONFIG_HOME") {
        candidates.push(PathBuf::from(config).join("nvim/nixd/_nixd-expr.nix"));
    }
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        candidates.push(home.join(".config/nvim/nixd/_nixd-expr.nix"));
        candidates.push(home.join("nixfiles/config/nvim/nixd/_nixd-expr.nix"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn file_uri(path: &Path) -> String {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut encoded = String::new();
    for byte in path.to_string_lossy().bytes() {
        if byte == b'/' || byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
        {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("file://{encoded}")
}

fn path_from_file_uri(uri: &str) -> Result<PathBuf> {
    let encoded = uri
        .strip_prefix("file://")
        .ok_or_else(|| anyhow!("unsupported non-file LSP URI {uri}"))?;
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let value = std::str::from_utf8(&bytes[index + 1..index + 3])?;
            decoded.push(u8::from_str_radix(value, 16)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Ok(PathBuf::from(String::from_utf8(decoded)?))
}

fn parse_lsp_locations(value: &serde_json::Value) -> Result<Vec<QuickfixEntry>> {
    let values = value
        .as_array()
        .map_or_else(|| vec![value], |values| values.iter().collect());
    values
        .into_iter()
        .filter(|value| !value.is_null())
        .map(|location| {
            let uri = location
                .get("targetUri")
                .or_else(|| location.get("uri"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow!("LSP location omitted URI"))?;
            let range = location
                .get("targetSelectionRange")
                .or_else(|| location.get("range"))
                .or_else(|| location.get("targetRange"))
                .ok_or_else(|| anyhow!("LSP location omitted range"))?;
            let line = range
                .pointer("/start/line")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(0)
                + 1;
            let column = range
                .pointer("/start/character")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(0)
                + 1;
            Ok(QuickfixEntry {
                path: path_from_file_uri(uri)?,
                line,
                column,
                column_utf16: true,
                text: "language-server location".to_owned(),
            })
        })
        .collect()
}

fn render_lsp_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Array(values) => values
            .iter()
            .map(render_lsp_text)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(" · "),
        serde_json::Value::Object(values) => {
            for key in ["value", "label", "contents", "signatures", "documentation"] {
                if let Some(value) = values.get(key) {
                    let rendered = render_lsp_text(value);
                    if !rendered.is_empty() {
                        return rendered;
                    }
                }
            }
            String::new()
        }
        _ => value.to_string(),
    }
}

fn expand_lsp_snippet(snippet: &str) -> String {
    expand_lsp_snippet_with_stops(snippet).0
}

fn expand_lsp_snippet_with_stops(snippet: &str) -> (String, Vec<Range<usize>>) {
    let bytes = snippet.as_bytes();
    let mut output = String::new();
    let mut stops = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && bytes
                .get(index + 1)
                .is_some_and(|next| matches!(next, b'$' | b'}' | b'\\'))
        {
            output.push(char::from(bytes[index + 1]));
            index += 2;
            continue;
        }
        if bytes[index] != b'$' {
            let character = snippet[index..].chars().next().unwrap_or_default();
            output.push(character);
            index += character.len_utf8();
            continue;
        }
        if bytes.get(index + 1) == Some(&b'{') {
            let Some(relative_end) = snippet[index + 2..].find('}') else {
                output.push('$');
                index += 1;
                continue;
            };
            let end = index + 2 + relative_end;
            let placeholder = &snippet[index + 2..end];
            let digits = placeholder.bytes().take_while(u8::is_ascii_digit).count();
            let stop = placeholder[..digits].parse::<usize>().ok();
            let start = output.len();
            if let Some((_, default)) = placeholder.split_once(':') {
                output.push_str(default);
            } else if let Some((_, choices)) = placeholder.split_once('|') {
                output.push_str(
                    choices
                        .trim_end_matches('|')
                        .split(',')
                        .next()
                        .unwrap_or_default(),
                );
            }
            if let Some(stop) = stop {
                stops.push((stop, start..output.len()));
            }
            index = end + 1;
            continue;
        }
        index += 1;
        let digit_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if digit_start < index
            && let Ok(stop) = snippet[digit_start..index].parse::<usize>()
        {
            stops.push((stop, output.len()..output.len()));
        }
    }
    stops.sort_by_key(|(stop, _)| (*stop == 0, *stop));
    stops.dedup_by_key(|(stop, _)| *stop);
    (output, stops.into_iter().map(|(_, range)| range).collect())
}

#[derive(Debug)]
struct Substitute {
    needle: String,
    replacement: String,
    ranges: Vec<Range<usize>>,
    global: bool,
    ignore_case: bool,
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

fn vim_regex_replacement(input: &str) -> String {
    let mut output = String::new();
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '&' => output.push_str("$0"),
            '\\' if characters.peek().is_some_and(char::is_ascii_digit) => {
                if let Some(group) = characters.next() {
                    output.push_str("${");
                    output.push(group);
                    output.push('}');
                }
            }
            '\\' => {
                if let Some(next) = characters.next() {
                    output.push(next);
                } else {
                    output.push('\\');
                }
            }
            '$' => output.push_str("$$"),
            _ => output.push(character),
        }
    }
    output
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

fn system_clipboard_text() -> Option<String> {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbpaste", &[])]
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
        TerminalKeyCode::PageUp => KeyCode::Up,
        TerminalKeyCode::PageDown => KeyCode::Down,
        TerminalKeyCode::Left => KeyCode::Left,
        TerminalKeyCode::Right => KeyCode::Right,
        TerminalKeyCode::Up => KeyCode::Up,
        TerminalKeyCode::Down => KeyCode::Down,
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

fn restore_client_state(buffer: &mut BufferState, state: &DurableClientState) -> Result<()> {
    for (name, register) in &state.registers {
        buffer
            .editor
            .restore_register(*name, register.text.clone(), register.linewise);
    }
    if let Some(pattern) = state.search_history.last() {
        buffer.editor.restore_search_pattern(pattern.clone());
    }
    for (name, mark) in &state.global_marks {
        if mark.document_id == buffer.document_id {
            buffer.editor.restore_mark(*name, mark.anchor.byte);
        }
    }
    if let Some(repeat) = &state.repeat_data {
        buffer.editor.restore_repeat_data(repeat)?;
    }
    for (name, recording) in &state.macro_recordings {
        let keys: Vec<KeyEvent> = serde_json::from_slice(&recording.raw_keys)
            .with_context(|| format!("restore macro {name}"))?;
        buffer.editor.restore_macro(*name, keys);
    }
    Ok(())
}

fn sync_client_state(
    active: &mut BufferState,
    inactive: &mut [BufferState],
    state: &DurableClientState,
) -> Result<()> {
    restore_client_state(active, state)?;
    for buffer in inactive {
        restore_client_state(buffer, state)?;
    }
    Ok(())
}

enum ClientStateMessage {
    Save(Box<DurableClientState>),
    Barrier {
        state: Box<DurableClientState>,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Stop,
}

struct ClientStateWorker {
    sender: mpsc::SyncSender<ClientStateMessage>,
    join: Option<JoinHandle<()>>,
    _temporary: Option<tempfile::TempDir>,
}

impl ClientStateWorker {
    fn open(client_id: ClientId) -> Result<(Self, DurableClientState)> {
        #[cfg(test)]
        let (directory, temporary) = {
            let temporary = tempfile::tempdir().context("create test client state")?;
            (temporary.path().to_path_buf(), Some(temporary))
        };
        #[cfg(not(test))]
        let (directory, temporary) = (client_state_directory()?, None);
        let store = ClientViewStateStore::new(directory);
        let state = store
            .load_durable(client_id)
            .context("load durable client state")?
            .unwrap_or_else(|| DurableClientState::new(client_id));
        let (sender, receiver) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("wren-client-state".to_owned())
            .spawn(move || client_state_loop(store, receiver))
            .context("spawn client state writer")?;
        Ok((
            Self {
                sender,
                join: Some(join),
                _temporary: temporary,
            },
            state,
        ))
    }

    fn try_save(&self, state: DurableClientState) {
        let _ = self
            .sender
            .try_send(ClientStateMessage::Save(Box::new(state)));
    }

    fn barrier(&self, state: DurableClientState) -> Result<()> {
        let (reply, response) = mpsc::channel();
        self.sender
            .send(ClientStateMessage::Barrier {
                state: Box::new(state),
                reply,
            })
            .map_err(|_| anyhow!("client state writer stopped"))?;
        response
            .recv()
            .map_err(|_| anyhow!("client state writer did not acknowledge barrier"))?
            .map_err(anyhow::Error::msg)
    }
}

impl Drop for ClientStateWorker {
    fn drop(&mut self) {
        let _ = self.sender.send(ClientStateMessage::Stop);
        join_worker_thread(&mut self.join);
    }
}

fn client_state_loop(store: ClientViewStateStore, receiver: mpsc::Receiver<ClientStateMessage>) {
    let mut error: Option<String> = None;
    for message in receiver {
        match message {
            ClientStateMessage::Save(state) => {
                if error.is_none()
                    && let Err(current) = store.save_durable(&state)
                {
                    error = Some(current.to_string());
                }
            }
            ClientStateMessage::Barrier { state, reply } => {
                if let Err(current) = store.save_durable(&state) {
                    error = Some(current.to_string());
                }
                let _ = reply.send(error.clone().map_or(Ok(()), Err));
            }
            ClientStateMessage::Stop => break,
        }
    }
}

#[cfg(not(test))]
fn client_state_directory() -> Result<PathBuf> {
    if let Some(directory) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(directory).join("wren"));
    }
    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".local/state/wren"));
    }
    Ok(env::current_dir()
        .context("locate current directory for client state")?
        .join(".wren-state"))
}

#[cfg(not(test))]
fn load_recent_files() -> Vec<PathBuf> {
    let Ok(path) = client_state_directory().map(|directory| directory.join("oldfiles")) else {
        return Vec::new();
    };
    std::fs::read(path).map_or_else(
        |_| Vec::new(),
        |contents| {
            contents
                .split(|byte| *byte == 0)
                .filter(|entry| !entry.is_empty())
                .map(|entry| PathBuf::from(String::from_utf8_lossy(entry).into_owned()))
                .filter(|path| path.is_absolute())
                .take(100)
                .collect()
        },
    )
}

#[cfg(test)]
fn load_recent_files() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(not(test))]
fn save_recent_files(paths: &[PathBuf]) -> Result<()> {
    let directory = client_state_directory()?;
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create oldfiles directory {}", directory.display()))?;
    let path = directory.join("oldfiles");
    let temporary = directory.join("oldfiles.tmp");
    let mut contents = Vec::new();
    for path in paths.iter().take(100) {
        contents.extend_from_slice(path.to_string_lossy().as_bytes());
        contents.push(0);
    }
    std::fs::write(&temporary, contents)
        .with_context(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
fn save_recent_files(_paths: &[PathBuf]) -> Result<()> {
    Ok(())
}

#[cfg_attr(test, allow(dead_code))]
#[derive(Debug, Serialize, Deserialize)]
struct UndoStateFile {
    base_hash: [u8; 32],
    state: DurableUndoState,
}

#[cfg(not(test))]
fn undo_state_path(document: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(document).unwrap_or_else(|_| document.to_path_buf());
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in canonical.to_string_lossy().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Ok(client_state_directory()?
        .join("undo")
        .join(format!("{hash:016x}.json")))
}

#[cfg(not(test))]
fn load_undo_state(document: &Path, base_hash: [u8; 32]) -> Result<Option<DurableUndoState>> {
    let path = undo_state_path(document)?;
    let contents = match std::fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let stored: UndoStateFile =
        serde_json::from_slice(&contents).with_context(|| format!("decode {}", path.display()))?;
    Ok((stored.base_hash == base_hash).then_some(stored.state))
}

#[cfg(test)]
fn load_undo_state(_document: &Path, _base_hash: [u8; 32]) -> Result<Option<DurableUndoState>> {
    Ok(None)
}

#[cfg(not(test))]
fn save_undo_state(buffer: &mut BufferState) -> Result<()> {
    let Some(document) = buffer.document.presentation_path() else {
        return Ok(());
    };
    let path = undo_state_path(document)?;
    let directory = path
        .parent()
        .ok_or_else(|| anyhow!("undo state path has no parent"))?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("create {}", directory.display()))?;
    let temporary = path.with_extension("json.tmp");
    let contents = serde_json::to_vec(&UndoStateFile {
        base_hash: buffer.base_hash,
        state: buffer.editor.durable_undo_state(),
    })?;
    std::fs::write(&temporary, contents)
        .with_context(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
fn save_undo_state(_buffer: &mut BufferState) -> Result<()> {
    Ok(())
}

enum MutationMessage {
    Register {
        document_id: DocumentId,
        text: String,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Append {
        document_id: DocumentId,
        transaction: Option<Transaction>,
        state_deltas: Vec<StateDelta>,
    },
    Barrier(mpsc::Sender<Result<(), String>>),
    Stop(mpsc::Sender<Result<(), String>>),
}

struct MutationWorker {
    sender: mpsc::Sender<MutationMessage>,
    join: Option<JoinHandle<()>>,
    _temporary: Option<tempfile::TempDir>,
}

impl MutationWorker {
    fn start(_workspace: &Path) -> Result<Self> {
        #[cfg(test)]
        let (session_directory, outbox_directory, temporary) = {
            let temporary = tempfile::tempdir().context("create test session state")?;
            (
                temporary.path().join("session"),
                temporary.path().join("outbox"),
                Some(temporary),
            )
        };
        #[cfg(not(test))]
        let (session_directory, outbox_directory, temporary) = {
            let workspace_key = format!("{:016x}", stable_document_id(Some(_workspace)).get());
            let root = client_state_directory()?;
            (
                root.join("sessions").join(&workspace_key),
                root.join("outbox").join(workspace_key),
                None,
            )
        };
        std::fs::create_dir_all(&session_directory).with_context(|| {
            format!(
                "create durable session directory {}",
                session_directory.display()
            )
        })?;
        std::fs::create_dir_all(&outbox_directory).with_context(|| {
            format!(
                "create durable outbox directory {}",
                outbox_directory.display()
            )
        })?;
        let authority = SessionAuthority::open(
            SessionJournal::in_directory(session_directory),
            SessionId::new(1),
        )?;
        let outbox = MutationOutbox::in_directory(outbox_directory);
        let (sender, receiver) = mpsc::channel();
        let join = thread::Builder::new()
            .name("wren-in-process-session".to_owned())
            .spawn(move || mutation_loop(authority, outbox, receiver))
            .context("spawn in-process mutation session")?;
        Ok(Self {
            sender,
            join: Some(join),
            _temporary: temporary,
        })
    }

    fn register(&self, document_id: DocumentId, text: String) -> Result<()> {
        let (reply, response) = mpsc::channel();
        self.sender
            .send(MutationMessage::Register {
                document_id,
                text,
                reply,
            })
            .map_err(|_| anyhow!("in-process session stopped"))?;
        response
            .recv()
            .map_err(|_| anyhow!("in-process session did not register document"))?
            .map_err(anyhow::Error::msg)
    }

    fn append(
        &self,
        document_id: DocumentId,
        transaction: Option<Transaction>,
        state_deltas: Vec<StateDelta>,
    ) -> Result<()> {
        if transaction.is_none() && state_deltas.is_empty() {
            return Ok(());
        }
        self.sender
            .send(MutationMessage::Append {
                document_id,
                transaction,
                state_deltas,
            })
            .map_err(|_| anyhow!("in-process session stopped"))
    }

    fn barrier(&self) -> Result<()> {
        let (reply, response) = mpsc::channel();
        self.sender
            .send(MutationMessage::Barrier(reply))
            .map_err(|_| anyhow!("in-process session stopped"))?;
        response
            .recv()
            .map_err(|_| anyhow!("in-process session did not acknowledge barrier"))?
            .map_err(anyhow::Error::msg)
    }
}

impl Drop for MutationWorker {
    fn drop(&mut self) {
        let (reply, response) = mpsc::channel();
        let _ = self.sender.send(MutationMessage::Stop(reply));
        let _ = response.recv();
        join_worker_thread(&mut self.join);
    }
}

fn mutation_loop(
    mut authority: SessionAuthority,
    outbox: MutationOutbox,
    receiver: mpsc::Receiver<MutationMessage>,
) {
    let client_id = ClientId::new(1);
    let mut error = replay_outstanding_mutations(&mut authority, &outbox)
        .err()
        .map(|current| current.to_string());
    let mut next_sequence = authority
        .highest_client_sequence(client_id)
        .get()
        .saturating_add(1);
    for message in receiver {
        match message {
            MutationMessage::Register {
                document_id,
                text,
                reply,
            } => {
                let result = if let Some(document) = authority.document(document_id) {
                    if document.text == text {
                        Ok(())
                    } else {
                        Err(format!(
                            "durable session text for {document_id:?} differs from local recovery; explicit reconciliation is required"
                        ))
                    }
                } else {
                    authority
                        .register_document(document_id, text, client_id)
                        .map(|_| ())
                        .map_err(|current| current.to_string())
                };
                if let Err(current) = &result {
                    error = Some(current.clone());
                }
                let _ = reply.send(result);
            }
            MutationMessage::Append {
                document_id,
                transaction,
                state_deltas,
            } => {
                if error.is_some() {
                    continue;
                }
                let result = submit_local_mutation(
                    &mut authority,
                    &outbox,
                    client_id,
                    next_sequence,
                    document_id,
                    transaction,
                    state_deltas,
                );
                if let Err(current) = result {
                    error = Some(current.to_string());
                } else {
                    next_sequence = next_sequence.saturating_add(1);
                }
            }
            MutationMessage::Barrier(reply) => {
                let _ = reply.send(error.clone().map_or(Ok(()), Err));
            }
            MutationMessage::Stop(reply) => {
                let _ = reply.send(error.clone().map_or(Ok(()), Err));
                break;
            }
        }
    }
}

fn replay_outstanding_mutations(
    authority: &mut SessionAuthority,
    outbox: &MutationOutbox,
) -> Result<()> {
    for mutation in outbox.outstanding()? {
        match authority.submit(mutation)? {
            MutationSubmission::Accepted { durable, .. } => {
                if !outbox.observe_result(&durable)? {
                    bail!("replayed durable mutation was missing from the client outbox");
                }
            }
            MutationSubmission::Rejected(result) => {
                bail!("outstanding mutation requires reconciliation: {result:?}");
            }
        }
    }
    Ok(())
}

fn submit_local_mutation(
    authority: &mut SessionAuthority,
    outbox: &MutationOutbox,
    client_id: ClientId,
    sequence: u64,
    document_id: DocumentId,
    transaction: Option<Transaction>,
    state_deltas: Vec<StateDelta>,
) -> Result<()> {
    let document = authority
        .document(document_id)
        .ok_or_else(|| anyhow!("document is not registered"))?;
    let mut documents = Vec::new();
    if let Some(mut transaction) = transaction {
        transaction.base_revision = document.revision;
        documents.push(DocumentMutation {
            document_id,
            lease_epoch: document.lease.lease_epoch,
            base_revision: document.revision,
            semantic_group_id: SemanticGroupId::new(sequence),
            semantic_group_kind: SemanticGroupKind::Operator,
            undo_parent: None,
            transactions: vec![transaction],
        });
    }
    let mutation = ClientMutation {
        mutation_id: MutationId::new(sequence),
        client_id,
        client_sequence: ClientSequence::new(sequence),
        state_deltas,
        documents,
    };
    outbox.append(&mutation)?;
    match authority.submit(mutation)? {
        MutationSubmission::Accepted { durable, .. } => {
            if !outbox.observe_result(&durable)? {
                bail!("durable mutation was missing from the client outbox");
            }
            Ok(())
        }
        MutationSubmission::Rejected(result) => bail!("in-process mutation rejected: {result:?}"),
    }
}

enum WalMessage {
    Append(RecoveredState),
    Clear(mpsc::Sender<Result<(), String>>),
    Barrier(mpsc::Sender<Result<(), String>>),
    Stop(mpsc::Sender<Result<(), String>>),
}

struct WalWorker {
    sender: mpsc::Sender<WalMessage>,
    join: Option<JoinHandle<()>>,
}

impl WalWorker {
    fn start(wal: LocalWal) -> Self {
        let (sender, receiver) = mpsc::channel();
        let join = thread::Builder::new()
            .name("wren-wal".to_owned())
            .spawn(move || wal_loop(&wal, receiver))
            .ok();
        Self { sender, join }
    }

    fn append(&self, state: RecoveredState) {
        let _ = self.sender.send(WalMessage::Append(state));
    }

    fn barrier(&self) -> Result<()> {
        self.request(WalMessage::Barrier)
    }

    fn clear(&self) -> Result<()> {
        self.request(WalMessage::Clear)
    }

    fn request(
        &self,
        make: impl FnOnce(mpsc::Sender<Result<(), String>>) -> WalMessage,
    ) -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        self.sender
            .send(make(sender))
            .map_err(|_| anyhow!("recovery WAL worker stopped"))?;
        receiver
            .recv()
            .map_err(|_| anyhow!("recovery WAL worker did not acknowledge"))?
            .map_err(|error| anyhow!(error))
    }
}

impl Drop for WalWorker {
    fn drop(&mut self) {
        let (sender, receiver) = mpsc::channel();
        let _ = self.sender.send(WalMessage::Stop(sender));
        let _ = receiver.recv();
        join_worker_thread(&mut self.join);
    }
}

fn wal_loop(wal: &LocalWal, receiver: mpsc::Receiver<WalMessage>) {
    let mut error: Option<String> = None;
    for message in receiver {
        match message {
            WalMessage::Append(state) => {
                if let Err(current) = wal.append(&state) {
                    error = Some(current.to_string());
                }
            }
            WalMessage::Clear(reply) => {
                if error.is_none()
                    && let Err(current) = wal.clear()
                {
                    error = Some(current.to_string());
                }
                let _ = reply.send(error.clone().map_or(Ok(()), Err));
            }
            WalMessage::Barrier(reply) => {
                let _ = reply.send(error.clone().map_or(Ok(()), Err));
            }
            WalMessage::Stop(reply) => {
                let _ = reply.send(error.clone().map_or(Ok(()), Err));
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn terminal_character(character: char) -> TerminalKey {
        TerminalKey {
            code: TerminalKeyCode::Char(character),
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        }
    }

    #[cfg(unix)]
    fn fake_lsp_server(directory: &Path) -> (LanguageServerInvocation, PathBuf) {
        let script = directory.join("fake_lsp.py");
        let log = directory.join("lsp.log");
        fs::write(
            &script,
            r#"import json
import sys
import time

log_path = sys.argv[1]
while True:
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b"\r\n", b"\n"):
            break
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":", 1)[1].strip())
    message = json.loads(sys.stdin.buffer.read(length))
    method = message.get("method", "")
    with open(log_path, "a", encoding="utf-8") as output:
        detail = " " + message.get("params", {}).get("rootUri", "") if method == "initialize" else ""
        output.write(method + detail + "\n")
    if "id" not in message:
        continue
    if method == "initialize":
        result = {"capabilities": {"semanticTokensProvider": {"legend": {
            "tokenTypes": ["variable"], "tokenModifiers": []
        }, "full": True}}}
    elif method == "textDocument/definition":
        time.sleep(0.2)
        result = None
    elif method == "textDocument/semanticTokens/full":
        result = {"data": []}
    else:
        result = None
    response = json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(response)}\r\n\r\n".encode() + response)
    sys.stdout.buffer.flush()
"#,
        )
        .expect("fake LSP script");
        (
            LanguageServerInvocation {
                program: "python3".to_owned(),
                arguments: vec![
                    script.to_string_lossy().into_owned(),
                    log.to_string_lossy().into_owned(),
                ],
                language_id: "rust".to_owned(),
                settings: serde_json::Value::Null,
            },
            log,
        )
    }

    #[test]
    fn parses_cli_file_and_line() {
        let cli =
            Cli::parse(["+42".to_owned(), "src/main.rs".to_owned()].into_iter()).expect("parse");
        assert_eq!(cli.line, Some(42));
        assert_eq!(cli.path, Some(PathBuf::from("src/main.rs")));
    }

    #[test]
    fn parses_escaped_literal_substitutions() {
        let ExCommand::Substitute {
            range,
            pattern,
            replacement,
            flags,
        } = parse_ex("%s/a\\/b/c\\/d/g").expect("substitute")
        else {
            panic!("expected substitution");
        };
        assert!(range.is_some());
        assert!(flags.global);
        assert_eq!(pattern.as_ref(), "a/b");
        assert_eq!(replacement.as_ref(), "c/d");
    }

    #[test]
    fn wal_worker_observes_barriers_and_clear() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let wal = LocalWal::in_directory(directory.path(), b"app-test");
        let worker = WalWorker::start(wal.clone());
        worker.append(RecoveredState {
            base_hash: [0; 32],
            revision: 1,
            text: "edit".to_owned(),
            cursor: 4,
        });
        worker.barrier().expect("barrier");
        assert!(wal.recover_latest().expect("recover").is_some());
        worker.clear().expect("clear");
        assert_eq!(wal.recover_latest().expect("recover"), None);
    }

    #[test]
    fn app_opens_edits_and_safely_saves_a_real_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("main.rs");
        let (document, opened) = LocalDocument::open_or_new(&path).expect("open new file");
        let wal = LocalWal::in_directory(directory.path().join("state"), b"app-save-test");
        let mut app = App::from_opened(document, opened, None, Some(wal)).expect("create app");
        app.dispatch_key(KeyEvent::character('i'));
        for character in "fn main() {}\n".chars() {
            app.dispatch_key(if character == '\n' {
                KeyEvent::plain(KeyCode::Enter)
            } else {
                KeyEvent::character(character)
            });
        }
        app.dispatch_key(KeyEvent::plain(KeyCode::Escape));
        assert!(app.active.editor.is_dirty());
        app.execute_ex("w").expect("save");
        assert!(!app.active.editor.is_dirty());
        assert_eq!(
            fs::read_to_string(&path).expect("saved source"),
            "fn main() {}\n"
        );
    }

    #[test]
    fn quit_requires_force_when_changes_are_unsaved() {
        let mut app = App::open(None, None).expect("unnamed editor");
        app.dispatch_key(KeyEvent::character('i'));
        app.dispatch_key(KeyEvent::character('x'));
        app.dispatch_key(KeyEvent::plain(KeyCode::Escape));
        app.execute_ex("q").expect("quit check");
        assert!(!app.quit);
        assert!(app.message.contains("unsaved"));
        app.execute_ex("q!").expect("forced quit");
        assert!(app.quit);
    }

    #[test]
    fn dotfile_leader_q_and_ff_are_exact_native_sequences() {
        let mut clean = App::open(None, None).expect("clean app");
        clean
            .handle_editor_key(terminal_character(' '))
            .expect("leader");
        assert!(clean.popup.as_ref().is_some_and(|popup| {
            popup.title.contains("NORMAL") && popup.text.contains("+find")
        }));
        clean
            .handle_editor_key(terminal_character('q'))
            .expect("leader quit");
        assert!(clean.quit);

        let mut dirty = App::open(None, None).expect("dirty app");
        dirty.dispatch_key(KeyEvent::character('i'));
        dirty.dispatch_key(KeyEvent::character('x'));
        dirty.dispatch_key(KeyEvent::plain(KeyCode::Escape));
        dirty
            .handle_editor_key(terminal_character(' '))
            .expect("leader");
        dirty
            .handle_editor_key(terminal_character('q'))
            .expect("refuse dirty quit");
        assert!(!dirty.quit);
        assert!(dirty.message.contains("unsaved"));
        assert!(dirty.popup.as_ref().is_some_and(|popup| {
            popup.title.as_ref() == "Error" && popup.text.contains("unsaved")
        }));

        dirty
            .handle_editor_key(terminal_character(' '))
            .expect("leader");
        dirty
            .handle_editor_key(terminal_character('f'))
            .expect("find group");
        assert!(dirty.popup.as_ref().is_some_and(|popup| {
            popup.title.contains("find") && popup.text.contains("file browser")
        }));
        dirty
            .handle_editor_key(terminal_character('f'))
            .expect("find files");
        assert!(
            dirty
                .prompt
                .as_ref()
                .is_some_and(|prompt| prompt.kind == PromptKind::FilePicker)
        );
    }

    #[test]
    fn file_picker_is_a_telescope_surface_with_results_and_preview() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("main.rs");
        fs::write(&source, "fn main() {\n    println!(\"preview\");\n}\n").expect("source");
        let mut app = App::open(None, None).expect("app");
        app.last_picker_source = Some(PickerSource::Files);
        app.prompt = Some(Prompt {
            kind: PromptKind::FilePicker,
            buffer: "main".to_owned(),
            history_index: None,
        });
        app.picker_matches = vec![source];
        app.refresh_picker_preview();
        let mut layout = ViewportLayout::new(100, 30);
        layout.configure_dotfile_profile();
        let frame = desired_frame(&mut layout, &app);
        let rendered = frame
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|cell| cell.grapheme.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Find Files (1)"));
        assert!(rendered.contains("main.rs"), "{rendered}");
        assert!(rendered.contains("println!(\"preview\")"));
        assert!(rendered.contains("❯ main"));
        assert!(!rendered.contains("find>"));
        assert!(frame.rows.iter().any(|row| {
            row.cells.iter().any(|cell| {
                cell.grapheme.as_ref() == "f"
                    && cell.style.foreground == Some(CellColor::Rgb(app.theme.mauve))
            })
        }));
    }

    #[test]
    fn colorscheme_and_runtime_color_override_are_customizable() {
        let mut app = App::open(None, None).expect("app");
        app.execute_ex("colorscheme catppuccin-latte")
            .expect("latte");
        assert_eq!(app.theme_flavor, CatppuccinFlavor::Latte);
        assert_eq!(app.theme, CatppuccinPalette::LATTE);
        app.execute_ex("setcolor mauve #010203").expect("override");
        assert_eq!(app.theme.mauve, RgbColor::new(1, 2, 3));
        assert!(app.execute_ex("setcolor missing #ffffff").is_err());
    }

    #[test]
    fn ctrl_d_u_f_b_are_native_vim_viewport_commands() {
        let (document, mut opened) = LocalDocument::unnamed();
        opened.text = (0..40)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let mut app = App::from_opened(document, opened, None, None).expect("app");
        app.viewport_rows = 10;
        let control = |character| TerminalKey {
            code: TerminalKeyCode::Char(character),
            shift: false,
            control: true,
            alt: false,
            super_key: false,
        };
        app.handle_editor_key(control('d')).expect("half page down");
        assert_eq!(app.active.editor.cursor_line_column().0, 4);
        assert_eq!(app.views.active_window().top_line, 4);
        assert!(!app.message.contains("grammar"));
        app.handle_editor_key(control('u')).expect("half page up");
        assert_eq!(app.active.editor.cursor_line_column().0, 0);
        assert_eq!(app.views.active_window().top_line, 0);
        app.handle_editor_key(control('f')).expect("page down");
        assert_eq!(app.active.editor.cursor_line_column().0, 7);
        app.handle_editor_key(control('b')).expect("page up");
        assert_eq!(app.active.editor.cursor_line_column().0, 0);
    }

    #[test]
    fn counted_vim_view_commands_replace_numbers_and_ctrl_w_are_native() {
        let (document, mut opened) = LocalDocument::unnamed();
        opened.text = (0..40)
            .map(|line| format!("line {line:03}\n"))
            .collect::<String>();
        let mut app = App::from_opened(document, opened, None, None).expect("app");
        app.viewport_rows = 10;
        let control = |character| TerminalKey {
            code: TerminalKeyCode::Char(character),
            shift: false,
            control: true,
            alt: false,
            super_key: false,
        };

        app.handle_editor_key(terminal_character('2'))
            .expect("count");
        app.handle_editor_key(control('d'))
            .expect("counted half page");
        assert_eq!(app.active.editor.cursor_line_column().0, 2);
        assert_eq!(app.views.active_window().top_line, 2);
        app.handle_editor_key(terminal_character('3'))
            .expect("count");
        app.handle_editor_key(control('e'))
            .expect("counted line scroll");
        assert_eq!(app.views.active_window().top_line, 5);
        app.handle_editor_key(terminal_character('2'))
            .expect("count");
        app.handle_editor_key(terminal_character('H'))
            .expect("second from top");
        assert_eq!(app.active.editor.cursor_line_column().0, 6);

        app.handle_editor_key(control('w')).expect("window prefix");
        app.handle_editor_key(terminal_character('v'))
            .expect("vertical split");
        assert_eq!(app.views.windows.len(), 2);
        app.handle_editor_key(control('w')).expect("window prefix");
        app.handle_editor_key(terminal_character('h'))
            .expect("focus left");
        assert_eq!(app.views.windows.len(), 2);
        assert!(!app.message.contains("grammar"));

        app.active.editor.set_cursor(0);
        app.handle_editor_key(control('a')).expect("increment");
        assert!(app.active.editor.contents().starts_with("line 001"));
        app.handle_editor_key(control('x')).expect("decrement");
        assert!(app.active.editor.contents().starts_with("line 000"));
        app.handle_editor_key(terminal_character('R'))
            .expect("replace mode");
        app.handle_editor_key(terminal_character('X'))
            .expect("replace character");
        app.handle_editor_key(TerminalKey {
            code: TerminalKeyCode::Escape,
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        })
        .expect("leave replace mode");
        assert_eq!(app.active.editor.mode(), Mode::Normal);
        assert!(app.active.editor.contents().starts_with("line X00"));
        assert!(!app.message.contains("grammar"));
    }

    #[test]
    fn syntax_demand_follows_scrolling_and_preserves_highlighted_viewports() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("scroll.rs");
        let text = (0..120)
            .map(|line| format!("fn item_{line}() {{ let value: i32 = {line}; }}\n"))
            .collect::<String>();
        fs::write(&source, &text).expect("source");
        let mut app = App::open(Some(&source), None).expect("app");
        app.schedule_provider_refreshes(10);
        let first_spans = app
            .decorations
            .get(&app.active.buffer_id)
            .expect("first-frame decorations")
            .spans
            .clone();
        let late_start = app.active.editor.text().byte_of_line(90);
        app.active.editor.set_cursor(late_start);
        app.schedule_provider_refreshes(10);
        assert!(app.views.active_window().top_line >= 80);
        assert!(
            app.decorations
                .get(&app.active.buffer_id)
                .is_some_and(|state| state
                    .spans
                    .iter()
                    .any(|span| span.range.start >= late_start))
        );
        let all_spans = &app
            .decorations
            .get(&app.active.buffer_id)
            .expect("merged decorations")
            .spans;
        assert!(first_spans.iter().all(|span| all_spans.contains(span)));
    }

    #[test]
    fn full_syntax_is_ready_on_file_open_viewport_change_and_buffer_change() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let first = directory.path().join("latency_first.rs");
        let second = directory.path().join("latency_second.rs");
        let first_text = (0..240)
            .map(|line| format!("fn item_{line}() {{ let value: i32 = {line}; }}\n"))
            .collect::<String>();
        let second_text = (0..160)
            .map(|line| format!("pub fn other_{line}() -> usize {{ {line} }}\n"))
            .collect::<String>();
        fs::write(&first, &first_text).expect("first source");
        fs::write(&second, &second_text).expect("second source");

        let opened_at = Instant::now();
        let mut app = App::open(Some(&first), None).expect("open first");
        app.format_on_save = false;
        assert!(
            opened_at.elapsed() < Duration::from_millis(250),
            "initial full syntax exceeded first-frame latency budget: {:?}",
            opened_at.elapsed()
        );
        let first_revision = app.active.editor.revision();
        let first_spans = &app
            .decorations
            .get(&app.active.buffer_id)
            .expect("first-frame syntax");
        assert_eq!(first_spans.revision, first_revision);
        let last_function = first_text.rfind("item_239").expect("last function");
        assert!(
            first_spans
                .spans
                .iter()
                .any(|span| span.range.contains(&last_function)),
            "file open must synchronously cover syntax beyond the first viewport"
        );

        let viewport_at = Instant::now();
        app.active.editor.set_cursor(last_function);
        app.schedule_provider_refreshes(12);
        assert!(
            viewport_at.elapsed() < Duration::from_millis(10),
            "viewport syntax lookup unexpectedly blocked: {:?}",
            viewport_at.elapsed()
        );
        assert!(
            app.decorations
                .get(&app.active.buffer_id)
                .is_some_and(|state| state.revision == first_revision
                    && state
                        .spans
                        .iter()
                        .any(|span| span.range.contains(&last_function)))
        );

        let change = Transaction::new(first_revision, vec![Edit::new(0..0, "pub ")])
            .expect("insert transaction");
        let changed_at = Instant::now();
        app.active
            .editor
            .apply_transaction(change.clone())
            .expect("apply insert");
        app.after_transaction(Some(change));
        assert!(
            changed_at.elapsed() < Duration::from_millis(20),
            "changed-line syntax exceeded next-frame latency budget: {:?}",
            changed_at.elapsed()
        );
        let changed = app
            .decorations
            .get(&app.active.buffer_id)
            .expect("changed syntax");
        assert_eq!(changed.revision, app.active.editor.revision());
        assert!(
            changed
                .spans
                .iter()
                .any(|span| span.range.start == 0 && span.range.end >= 3),
            "newly inserted keyword must be highlighted before provider polling"
        );
        let shifted_last = last_function + "pub ".len();
        assert!(
            changed
                .spans
                .iter()
                .any(|span| span.range.contains(&shifted_last))
        );

        let second_opened_at = Instant::now();
        app.open_buffer(&second).expect("open second");
        assert!(
            second_opened_at.elapsed() < Duration::from_millis(250),
            "new-buffer syntax exceeded first-frame latency budget: {:?}",
            second_opened_at.elapsed()
        );
        let second_last = second_text
            .rfind("other_159")
            .expect("last second function");
        assert!(
            app.decorations
                .get(&app.active.buffer_id)
                .is_some_and(|state| state.revision == app.active.editor.revision()
                    && state
                        .spans
                        .iter()
                        .any(|span| span.range.contains(&second_last)))
        );
    }

    #[test]
    fn immediate_highlight_overtakes_queued_background_provider_work() {
        let (sender, requests) = mpsc::sync_channel(8);
        let (immediate_sender, immediate_requests) = mpsc::channel();
        let (results, _receiver) = mpsc::channel();
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let update_order = Arc::new(Mutex::new(Vec::new()));
        let observed_order = Arc::clone(&update_order);
        let worker = thread::spawn(move || {
            let mut actor = ProviderActor::default();
            let mut first_update = true;
            provider_loop(requests, immediate_requests, results, |request| {
                if let ProviderRequest::UpdateDocument { document_id, .. } = request {
                    observed_order
                        .lock()
                        .expect("update order")
                        .push(*document_id);
                    if first_update {
                        first_update = false;
                        started_sender.send(()).expect("signal first update");
                        release_receiver.recv().expect("release first update");
                    }
                }
                actor.handle(request.clone())
            });
        });
        let revision = DocumentRevision::new(1);
        let bundle = language_bundle(Some(Path::new("priority.rs")));
        let refresh = |document_id: DocumentId| {
            ProviderWorkerMessage::Refresh(Box::new(ProviderRefresh {
                buffer_id: BufferId::new(document_id.get()),
                document_id,
                revision,
                text: "fn background() {}\n".into(),
                bundle: bundle.clone(),
                visible: 0..19,
                near_viewport: 0..19,
            }))
        };
        let first = DocumentId::new(1);
        let second = DocumentId::new(2);
        let immediate = DocumentId::new(3);
        sender.send(refresh(first)).expect("queue active refresh");
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("first refresh started");
        sender.send(refresh(second)).expect("queue waiting refresh");
        let (reply, response) = mpsc::channel();
        immediate_sender
            .send(ProviderWorkerMessage::HighlightNow(Box::new(
                ImmediateHighlight {
                    document_id: immediate,
                    revision,
                    text: "fn immediate() {}\n".into(),
                    bundle,
                    reply,
                },
            )))
            .expect("queue immediate highlight");
        sender
            .try_send(ProviderWorkerMessage::Wake)
            .expect("wake provider");
        release_sender.send(()).expect("release provider");
        response
            .recv_timeout(Duration::from_secs(1))
            .expect("immediate response")
            .expect("fresh immediate highlight");
        assert_eq!(
            &update_order.lock().expect("update order")[..2],
            &[first, immediate],
            "first-frame syntax must not wait behind queued background work"
        );
        sender
            .send(ProviderWorkerMessage::Stop)
            .expect("stop provider");
        worker.join().expect("provider worker");
    }

    #[test]
    fn viewport_demands_do_not_reupload_or_reparse_an_unchanged_document() {
        let (sender, requests) = mpsc::channel();
        let (_immediate_sender, immediate_requests) = mpsc::channel();
        let (results, receiver) = mpsc::channel();
        let updates = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&updates);
        let worker = thread::spawn(move || {
            let mut actor = ProviderActor::default();
            provider_loop(requests, immediate_requests, results, |request| {
                if matches!(request, ProviderRequest::UpdateDocument { .. }) {
                    observed.fetch_add(1, Ordering::Relaxed);
                }
                actor.handle(request.clone())
            });
        });
        let text = (0..80)
            .map(|line| format!("fn item_{line}() {{}}\n"))
            .collect::<String>();
        let revision = DocumentRevision::new(7);
        for visible in [0..80, text.len().saturating_sub(80)..text.len()] {
            sender
                .send(ProviderWorkerMessage::Refresh(Box::new(ProviderRefresh {
                    buffer_id: BufferId::new(1),
                    document_id: DocumentId::new(1),
                    revision,
                    text: text.clone().into_boxed_str(),
                    bundle: language_bundle(Some(Path::new("latency.rs"))),
                    visible: visible.clone(),
                    near_viewport: visible,
                })))
                .expect("queue viewport");
        }
        sender
            .send(ProviderWorkerMessage::Stop)
            .expect("stop provider");
        worker.join().expect("provider worker");
        assert_eq!(updates.load(Ordering::Relaxed), 1);
        assert_eq!(
            receiver
                .try_iter()
                .filter(|result| matches!(result, ProviderWorkerResult::Decorations { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn hover_text_renders_as_a_rounded_float_not_status_text() {
        let mut app = App::open(None, None).expect("app");
        let (text, decorations) = lsp_popup_markdown("```rust\nfn hover() -> i32\n```", app.theme);
        app.popup = Some(TextPopup {
            title: "".into(),
            text: text.into(),
            scroll: 0,
            decorations,
        });
        let mut layout = ViewportLayout::new(100, 30);
        let frame = desired_frame(&mut layout, &app);
        let rendered = frame
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|cell| cell.grapheme.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("╭"));
        assert!(rendered.contains("fn hover() -> i32"));
        assert!(!rendered.contains("```"));
        assert!(!app.status().contains("fn hover"));
        assert!(frame.rows.iter().any(|row| {
            row.cells.iter().any(|cell| {
                cell.grapheme.as_ref() == "f"
                    && cell.style.foreground == Some(CellColor::Rgb(app.theme.mauve))
            })
        }));
    }

    #[test]
    fn hover_popup_expires_after_its_deadline() {
        let mut app = App::open(None, None).expect("app");
        app.popup = Some(TextPopup {
            title: "".into(),
            text: "hover".into(),
            scroll: 0,
            decorations: Vec::new(),
        });
        app.popup_deadline = Instant::now().checked_sub(Duration::from_millis(1));
        assert!(app.poll_popup_timeout());
        assert!(app.popup.is_none());
        assert!(app.popup_deadline.is_none());
        assert!(!app.poll_popup_timeout());
    }

    #[test]
    fn k_closes_an_open_popup_instead_of_requesting_another_hover() {
        let mut app = App::open(None, None).expect("app");
        app.popup = Some(TextPopup {
            title: "Documentation".into(),
            text: "hover details".into(),
            scroll: 0,
            decorations: Vec::new(),
        });
        app.popup_deadline = Some(Instant::now() + Duration::from_secs(6));

        app.handle_editor_key(terminal_character('K'))
            .expect("dismiss popup");

        assert!(app.popup.is_none());
        assert!(app.popup_deadline.is_none());
        assert!(app.pending_lsp_hover.is_none());
    }

    #[test]
    fn recoverable_errors_render_as_timed_help_style_floats() {
        let mut app = App::open(None, None).expect("app");
        app.show_error("definition response omitted its URI");
        assert_eq!(app.message, "definition response omitted its URI");
        assert!(app.popup_deadline.is_some());
        assert!(app.popup.as_ref().is_some_and(|popup| {
            popup.title.as_ref() == "Error" && popup.text.contains("omitted its URI")
        }));

        let mut layout = ViewportLayout::new(80, 24);
        let rendered = desired_frame(&mut layout, &app)
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|cell| cell.grapheme.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("╭"));
        assert!(rendered.contains("Error"));
        assert!(rendered.contains("definition response omitted its URI"));
    }

    #[test]
    fn messages_command_opens_bounded_severity_tagged_history_buffer() {
        let mut app = App::open(None, None).expect("app");
        app.message = "language server starting".to_owned();
        app.capture_debug_output();
        app.show_error("provider worker disconnected");
        assert!(app.popup.as_ref().is_some_and(|popup| {
            popup.title.as_ref() == "Error" && popup.text.as_ref() == "provider worker disconnected"
        }));

        app.execute_ex("messages").expect("show messages");

        assert!(app.popup.is_none());
        assert!(app.popup_deadline.is_none());
        assert!(app.message.is_empty());
        assert_eq!(app.active.name(), MESSAGES_BUFFER_NAME);
        assert!(app.active.editor.is_read_only());
        assert!(
            app.active
                .editor
                .contents()
                .contains("[INFO] language server starting")
        );
        assert!(
            app.active
                .editor
                .contents()
                .contains("[ERROR] provider worker disconnected")
        );
        assert!(app.status().contains("[Messages] [RO]"));

        let messages_buffer_id = app.active.buffer_id;
        app.execute_ex("bprevious")
            .expect("return to source buffer");
        assert_ne!(app.active.buffer_id, messages_buffer_id);
        app.show_info("formatter complete");
        app.execute_ex("debuglog").expect("refresh messages");
        assert_eq!(app.views.buffers.len(), 2);
        assert_eq!(app.inactive.len(), 1);
        assert_eq!(app.active.buffer_id, messages_buffer_id);
        assert!(app.active.editor.contents().contains("formatter complete"));
    }

    #[test]
    fn rejected_grammar_sequence_is_info_and_does_not_open_an_error_popup() {
        let mut app = App::open(None, None).expect("app");

        app.dispatch_key(KeyEvent::character('d'));
        app.dispatch_key(KeyEvent::character('Q'));

        assert!(app.popup.is_none());
        assert!(app.popup_deadline.is_none());
        assert!(app.message.contains("grammar rejected sequence \"dQ\""));
        assert!(app.views.messages.entries.last().is_some_and(|entry| {
            entry.severity == MessageSeverity::Info && entry.text.contains("\"dQ\"")
        }));

        app.show_error("provider crashed");
        assert!(app.popup.as_ref().is_some_and(|popup| {
            popup.title.as_ref() == "Error" && popup.text.as_ref() == "provider crashed"
        }));
        assert!(app.views.messages.entries.last().is_some_and(|entry| {
            entry.severity == MessageSeverity::Error && entry.text.as_ref() == "provider crashed"
        }));
    }

    #[cfg(unix)]
    #[test]
    fn launch_workspace_lsp_survives_ctrl_o_across_roots_and_gd_stays_async() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("main.rs");
        fs::write(&source, "fn main() { target(); }\n").expect("source");
        let mut app = App::open(Some(&source), None).expect("app");
        let workspace_root = app.lsp_root();
        let (server, log) = fake_lsp_server(directory.path());
        let environment = env::vars()
            .map(|(name, value)| (name.into_boxed_str(), value.into_boxed_str()))
            .collect();
        let revision = DocumentRevision::new(1);
        let started = Instant::now();
        let (client, uri, legend) = spawn_lsp_client(
            &server,
            &source,
            &workspace_root,
            revision,
            "fn main() { target(); }\n",
            environment,
        )
        .expect("start fake LSP");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "client readiness waited on post-initialize work: {:?}",
            started.elapsed()
        );
        let log_deadline = Instant::now() + Duration::from_millis(250);
        let startup_log = loop {
            let current = fs::read_to_string(&log).unwrap_or_default();
            if current.contains("textDocument/didOpen") {
                break current;
            }
            assert!(Instant::now() < log_deadline, "didOpen was not observed");
            thread::yield_now();
        };
        assert!(startup_log.contains("initialize"));
        assert!(startup_log.contains(&file_uri(&workspace_root)));
        assert!(startup_log.contains("textDocument/didOpen"));
        assert!(!startup_log.contains("semanticTokens/full"));

        let document_id = app.active.document_id;
        app.lsp = Some(PersistentLsp {
            document_id,
            revision,
            uri: uri.clone(),
            client,
            server: language_server_invocation(Some(&source)).expect("Rust profile"),
            root: workspace_root.clone(),
            open_documents: BTreeMap::from([(document_id, LspOpenDocument { uri, revision })]),
            semantic_legend: legend,
            semantic_due: None,
        });
        let dispatched = Instant::now();
        app.lsp_location("textDocument/definition")
            .expect("dispatch gd");
        assert!(
            dispatched.elapsed() < Duration::from_millis(20),
            "gd blocked the input loop: {:?}",
            dispatched.elapsed()
        );
        assert!(app.lsp_background.is_some());
        let deadline = Instant::now() + Duration::from_secs(1);
        while app.lsp_background.is_some() {
            assert!(Instant::now() < deadline, "fake definition timed out");
            let _ = app.poll_lsp_background().expect("poll gd");
            thread::yield_now();
        }
        assert!(app.message.contains("no location"));

        let outside_workspace = tempfile::tempdir().expect("outside workspace");
        let second = outside_workspace.path().join("other.rs");
        fs::write(&second, "pub fn target() {}\n").expect("second source");
        assert!(app.lsp.is_some(), "definition worker did not restore LSP");
        assert_eq!(
            app.lsp.as_ref().map(|lsp| &lsp.server),
            language_server_invocation(Some(&second)).as_ref()
        );
        assert_eq!(
            app.lsp.as_ref().map(|lsp| lsp.root.as_path()),
            std::fs::canonicalize(&workspace_root).ok().as_deref()
        );
        app.navigate_to_entry(&QuickfixEntry {
            path: second.clone(),
            line: 1,
            column: 1,
            column_utf16: true,
            text: "outside definition".to_owned(),
        })
        .expect("cross-workspace navigation open");
        assert!(app.lsp_start.is_none());
        let reused = app.lsp.as_ref().expect("LSP was discarded");
        assert_eq!(reused.document_id, app.active.document_id);
        assert_eq!(reused.open_documents.len(), 2);
        assert_eq!(reused.root, workspace_root);

        assert!(app.navigate_jump_count(true, 1).expect("Ctrl-O to root"));
        assert!(
            app.active
                .document
                .presentation_path()
                .is_some_and(|path| { same_path(path, &source) })
        );
        assert!(app.lsp_start.is_none());
        assert_eq!(
            app.lsp
                .as_ref()
                .map(|lsp| (&lsp.root, lsp.open_documents.len())),
            Some((&workspace_root, 2))
        );
        assert!(app.navigate_jump_count(false, 1).expect("Ctrl-I outside"));
        assert!(
            app.active
                .document
                .presentation_path()
                .is_some_and(|path| { same_path(path, &second) })
        );
        assert!(app.lsp_start.is_none());
        assert_eq!(
            app.lsp
                .as_ref()
                .map(|lsp| (&lsp.root, lsp.open_documents.len())),
            Some((&workspace_root, 2))
        );

        let notes = outside_workspace.path().join("notes.txt");
        fs::write(&notes, "not an LSP buffer\n").expect("notes");
        if let Some(lsp) = &mut app.lsp {
            lsp.semantic_due = Some(Instant::now());
        }
        app.navigate_to_entry(&QuickfixEntry {
            path: notes.clone(),
            line: 1,
            column: 1,
            column_utf16: true,
            text: "notes".to_owned(),
        })
        .expect("visit non-LSP buffer");
        assert!(app.lsp.is_some(), "non-LSP buffer killed the root server");
        assert_eq!(app.lsp.as_ref().and_then(|lsp| lsp.semantic_due), None);
        assert!(app.navigate_jump_count(true, 1).expect("Ctrl-O from notes"));
        assert!(
            app.active
                .document
                .presentation_path()
                .is_some_and(|path| { same_path(path, &second) })
        );
        assert!(app.lsp_start.is_none());
        assert!(app.lsp.as_ref().and_then(|lsp| lsp.semantic_due).is_some());
        assert_eq!(
            app.lsp
                .as_ref()
                .map(|lsp| (&lsp.root, lsp.open_documents.len())),
            Some((&workspace_root, 2))
        );

        let python_workspace = tempfile::tempdir().expect("Python workspace");
        let python_source = python_workspace.path().join("tool.py");
        fs::write(&python_source, "def tool():\n  pass\n").expect("Python source");
        let (python_fake, _) = fake_lsp_server(python_workspace.path());
        let (python_client, python_uri, python_legend) = spawn_lsp_client(
            &python_fake,
            &python_source,
            &workspace_root,
            revision,
            "def tool():\n  pass\n",
            env::vars()
                .map(|(name, value)| (name.into_boxed_str(), value.into_boxed_str()))
                .collect(),
        )
        .expect("Python client");
        app.parked_lsps.push(PersistentLsp {
            document_id: DocumentId::new(999),
            revision,
            uri: python_uri,
            client: python_client,
            server: language_server_invocation(Some(&python_source)).expect("Python profile"),
            root: workspace_root.clone(),
            open_documents: BTreeMap::new(),
            semantic_legend: python_legend,
            semantic_due: None,
        });
        app.open_buffer(&python_source).expect("activate Python");
        assert_eq!(
            app.lsp.as_ref().map(|lsp| lsp.server.language_id.as_str()),
            Some("python")
        );
        assert_eq!(app.parked_lsps.len(), 1);
        app.lsp_request_at_cursor("textDocument/hover", serde_json::json!({}))
            .expect("parked Python client remains live");
        app.open_buffer(&second).expect("return to Rust");
        assert_eq!(
            app.lsp.as_ref().map(|lsp| lsp.server.language_id.as_str()),
            Some("rust")
        );
        assert_eq!(app.parked_lsps.len(), 1);
        app.lsp_request_at_cursor("textDocument/hover", serde_json::json!({}))
            .expect("parked Rust client remains live");

        let lsp = app.lsp.take().expect("reused LSP");
        let (sender, receiver) = mpsc::channel();
        sender
            .send(LspBackgroundResult {
                lsp,
                operation: LspBackgroundOperation::Location {
                    method: "textDocument/definition".to_owned(),
                },
                outcome: Ok(serde_json::json!({"range": {"start": {"line": 0}}})),
            })
            .expect("queue malformed definition result");
        app.lsp_background = Some(receiver);
        app.handle_editor_key(terminal_character(' '))
            .expect("leader while gd is finishing");
        assert!(
            app.poll_lsp_background()
                .expect("malformed gd must remain recoverable")
        );
        assert!(app.lsp.is_some(), "gd failure lost the reusable LSP client");
        assert!(app.popup.as_ref().is_some_and(|popup| {
            popup.title.as_ref() == "Error" && popup.text.contains("omitted URI")
        }));

        app.handle_editor_key(terminal_character('q'))
            .expect("quit after failed gd");
        assert!(app.quit, "Space-q was coupled to failed LSP state");
    }

    #[test]
    fn gd_queues_behind_in_progress_startup_instead_of_starting_a_second_server() {
        let mut app = App::open(None, None).expect("app");
        let (_sender, receiver) = mpsc::channel::<Result<PersistentLsp, String>>();
        app.lsp_start = Some(receiver);
        let dispatched = Instant::now();
        app.lsp_location("textDocument/definition")
            .expect("queue gd");
        assert!(dispatched.elapsed() < Duration::from_millis(10));
        assert_eq!(
            app.pending_lsp_location.as_deref(),
            Some("textDocument/definition")
        );
        assert!(app.message.contains("queued"));
    }

    #[test]
    fn mouse_wheel_bursts_wait_for_the_decoder_and_coalesce_without_losing_the_next_key() {
        assert!(input_requires_render(&TerminalInput::MouseScroll {
            lines: 3,
            column: 8,
            row: 12,
        }));
        assert!(
            (0..10_000)
                .map(|_| TerminalInput::Ignored)
                .all(|input| !input_requires_render(&input)),
            "ignored mouse motion must never publish terminal frames"
        );
        let first = TerminalInput::MouseScroll {
            lines: 3,
            column: 8,
            row: 12,
        };
        let mut queued = (0..63)
            .map(|_| TerminalInput::MouseScroll {
                lines: 3,
                column: 8,
                row: 12,
            })
            .chain(std::iter::once(TerminalInput::Key(terminal_character('j'))))
            .collect::<VecDeque<_>>();
        let mut drain_timeouts = Vec::new();
        let (scroll, pending) = coalesce_mouse_scroll_input(first, |timeout| {
            drain_timeouts.push(timeout);
            Ok(queued.pop_front())
        })
        .expect("coalesce");
        assert_eq!(
            scroll,
            TerminalInput::MouseScroll {
                lines: 192,
                column: 8,
                row: 12,
            }
        );
        assert_eq!(pending, Some(TerminalInput::Key(terminal_character('j'))));
        assert!(queued.is_empty());
        assert!(
            drain_timeouts
                .iter()
                .all(|timeout| *timeout >= Duration::from_millis(2)),
            "the terminal decoder needs a non-zero grace period to expose every event in a burst"
        );

        let (document, mut opened) = LocalDocument::unnamed();
        opened.text = (0..400)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let mut app = App::from_opened(document, opened, None, None).expect("app");
        app.viewport_rows = 20;
        app.handle_input(scroll).expect("apply coalesced scroll");
        assert_eq!(app.views.active_window().top_line, 192);
    }

    #[test]
    fn left_click_moves_the_editor_cursor_through_rendered_cell_geometry() {
        let (document, mut opened) = LocalDocument::unnamed();
        opened.text = "zero\n\twide界 tail\nthird".to_owned();
        let mut app = App::from_opened(document, opened, None, None).expect("app");
        let mut layout = ViewportLayout::new(30, 6);
        layout.configure_dotfile_profile();

        app.handle_mouse_click(&layout, 10, 1)
            .expect("click wide character");
        assert_eq!(
            app.active.editor.primary_cursor(),
            app.active
                .editor
                .contents()
                .find('界')
                .expect("wide character")
        );
        let cursor = app.active.editor.primary_cursor();
        app.handle_mouse_click(&layout, 10, 5)
            .expect("ignore status line");
        assert_eq!(app.active.editor.primary_cursor(), cursor);
    }

    #[test]
    fn space_jj_labels_every_visible_match_and_jumps_by_label() {
        let (document, mut opened) = LocalDocument::unnamed();
        opened.text = "x first\nx second\nx third\n".to_owned();
        let mut app = App::from_opened(document, opened, None, None).expect("app");
        app.viewport_rows = 8;
        app.handle_editor_key(terminal_character(' '))
            .expect("leader");
        app.handle_editor_key(terminal_character('j'))
            .expect("jump prefix");
        app.handle_editor_key(terminal_character('j'))
            .expect("ace jump");
        app.handle_editor_key(terminal_character('x'))
            .expect("target character");
        let overlay = app.ace_jump_overlay().expect("jump labels");
        assert_eq!(overlay.targets.len(), 2);
        assert_eq!(overlay.targets[0].label.as_ref(), "a");
        assert_eq!(overlay.targets[1].label.as_ref(), "s");
        let mut layout = ViewportLayout::new(60, 8);
        layout.configure_dotfile_profile();
        let frame = desired_frame(&mut layout, &app);
        assert!(frame.rows.iter().any(|row| row.cells.iter().any(|cell| {
            cell.grapheme.as_ref() == "a"
                && cell.style.background == Some(CellColor::Rgb(app.theme.peach))
        })));
        app.handle_editor_key(terminal_character('s'))
            .expect("select label");
        assert_eq!(
            app.active.editor.primary_cursor(),
            app.active.editor.contents().rfind('x').expect("last x")
        );
        assert!(app.ace_jump.is_none());
    }

    #[test]
    fn dotfile_profile_expands_tabs_and_autosaves_on_buffer_leave() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let first = directory.path().join("first.rs");
        let second = directory.path().join("second.rs");
        fs::write(&first, "one\n").expect("first source");
        fs::write(&second, "two\n").expect("second source");
        let mut app = App::open(Some(&first), None).expect("app");
        app.dispatch_key(KeyEvent::character('i'));
        app.handle_editor_key(TerminalKey {
            code: TerminalKeyCode::Tab,
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        })
        .expect("expanded tab");
        app.dispatch_key(KeyEvent::character('x'));
        app.dispatch_key(KeyEvent::plain(KeyCode::Escape));
        app.open_buffer(&second).expect("leave first buffer");
        assert_eq!(fs::read_to_string(&first).expect("autosaved"), "  xone\n");
    }

    #[test]
    fn dotfile_live_grep_picker_searches_the_buffer_workspace_and_opens_result() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let first = directory.path().join("first.rs");
        let second = directory.path().join("second.rs");
        fs::write(&first, "fn first() {}\n").expect("first source");
        fs::write(&second, "fn needle() {}\n").expect("second source");
        let mut app = App::open(Some(&first), None).expect("app");

        app.start_grep_picker("needle").expect("grep picker");
        assert_eq!(app.quickfix.len(), 1);
        app.handle_prompt_key(TerminalKey {
            code: TerminalKeyCode::Enter,
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        })
        .expect("open grep match");

        assert!(
            app.active
                .document
                .presentation_path()
                .is_some_and(|path| same_path(path, &second))
        );
        assert_eq!(app.active.editor.cursor_line_column(), (0, 3));
    }

    #[test]
    fn closed_expression_register_evaluates_and_pastes_without_io() {
        let mut app = App::open(None, None).expect("unnamed editor");
        app.execute_prompt(Prompt {
            kind: PromptKind::Expression,
            buffer: "upper('wr' + 'en')".to_owned(),
            history_index: None,
        })
        .expect("evaluate expression");
        app.dispatch_key(KeyEvent::character('p'));
        assert_eq!(app.active.editor.contents(), "WREN");
        assert!(evaluate_expression("read_file('x')", &app.expression_context()).is_err());
    }

    #[test]
    fn slash_search_is_incremental_highlighted_repeatable_and_cancelable() {
        let (document, mut opened) = LocalDocument::unnamed();
        opened.text = "zero hit one hit two hit\n".to_owned();
        let mut app = App::from_opened(document, opened, None, None).expect("app");

        app.handle_editor_key(terminal_character('/'))
            .expect("open forward search");
        for character in "h.t".chars() {
            app.handle_prompt_key(terminal_character(character))
                .expect("type search");
        }
        assert_eq!(app.active.editor.primary_cursor(), 5, "incremental match");
        assert!(app.search_highlight);
        app.handle_prompt_key(TerminalKey {
            code: TerminalKeyCode::Enter,
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        })
        .expect("commit search");
        assert_eq!(app.active.editor.primary_cursor(), 5);

        let mut layout = ViewportLayout::new(80, 10);
        layout.configure_dotfile_profile();
        let frame = desired_frame(&mut layout, &app);
        assert!(frame.rows.iter().any(|row| row.cells.iter().any(|cell| {
            cell.grapheme.as_ref() == "h"
                && matches!(
                    cell.style.background,
                    Some(CellColor::Rgb(color))
                        if color == app.theme.yellow || color == app.theme.peach
                )
        })));

        app.handle_editor_key(terminal_character('n'))
            .expect("next match");
        assert_eq!(app.active.editor.primary_cursor(), 13);
        app.handle_editor_key(terminal_character('N'))
            .expect("previous match");
        assert_eq!(app.active.editor.primary_cursor(), 5);

        app.active
            .editor
            .set_cursor(app.active.editor.text().len_bytes());
        app.handle_editor_key(terminal_character('?'))
            .expect("open backward search");
        for character in "hit".chars() {
            app.handle_prompt_key(terminal_character(character))
                .expect("type backward search");
        }
        app.handle_prompt_key(TerminalKey {
            code: TerminalKeyCode::Enter,
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        })
        .expect("commit backward search");
        assert_eq!(app.active.editor.primary_cursor(), 21);
        app.handle_editor_key(terminal_character('n'))
            .expect("repeat backward search");
        assert_eq!(app.active.editor.primary_cursor(), 13);

        app.handle_editor_key(terminal_character('/'))
            .expect("start cancelable search");
        for character in "zero".chars() {
            app.handle_prompt_key(terminal_character(character))
                .expect("type cancelable search");
        }
        assert_eq!(app.active.editor.primary_cursor(), 0);
        app.handle_prompt_key(TerminalKey {
            code: TerminalKeyCode::Escape,
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        })
        .expect("cancel search");
        assert_eq!(app.active.editor.primary_cursor(), 13);
        assert_eq!(
            app.active.editor.last_search(),
            Some(("hit", SearchDirection::Backward))
        );

        app.execute_ex("nohlsearch").expect("clear highlights");
        assert!(!app.search_highlight);
        app.handle_editor_key(terminal_character('n'))
            .expect("search remains repeatable after nohlsearch");
        assert_eq!(app.active.editor.primary_cursor(), 5);
    }

    #[test]
    fn command_prompt_substitution_reuses_the_last_slash_pattern() {
        let (document, mut opened) = LocalDocument::unnamed();
        opened.text = "one one\nother one\n".to_owned();
        let mut app = App::from_opened(document, opened, None, None).expect("app");

        app.handle_editor_key(terminal_character('/'))
            .expect("open search");
        for character in "one".chars() {
            app.handle_prompt_key(terminal_character(character))
                .expect("type search");
        }
        app.handle_prompt_key(TerminalKey {
            code: TerminalKeyCode::Enter,
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        })
        .expect("commit search");

        app.handle_editor_key(terminal_character(':'))
            .expect("open command prompt");
        for character in "%s//TWO/g".chars() {
            app.handle_prompt_key(terminal_character(character))
                .expect("type substitution");
        }
        app.handle_prompt_key(TerminalKey {
            code: TerminalKeyCode::Enter,
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        })
        .expect("run substitution");
        wait_for_task(&mut app);
        assert_eq!(app.active.editor.contents(), "TWO TWO\nother TWO\n");
        app.active.editor.undo().expect("undo substitution");
        assert_eq!(app.active.editor.contents(), "one one\nother one\n");
    }

    #[test]
    fn whole_document_substitution_runs_as_a_task_and_commits_one_transaction() {
        let (document, opened) = LocalDocument::unnamed();
        let mut app = App::from_opened(document, opened, None, None).expect("app");
        app.dispatch_key(KeyEvent::character('i'));
        for character in "one one\ntwo one".chars() {
            app.dispatch_key(if character == '\n' {
                KeyEvent::plain(KeyCode::Enter)
            } else {
                KeyEvent::character(character)
            });
        }
        app.dispatch_key(KeyEvent::plain(KeyCode::Escape));
        let revision_before = app.active.editor.revision();
        app.execute_ex("%s/one/ONE/g").expect("start task");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !app.poll_task_results().expect("poll") {
            assert!(std::time::Instant::now() < deadline, "task timed out");
            thread::yield_now();
        }
        assert_eq!(app.active.editor.contents(), "ONE ONE\ntwo ONE");
        assert_eq!(
            app.active.editor.revision(),
            revision_before.next().expect("revision")
        );
        app.active.editor.undo().expect("undo");
        assert_eq!(app.active.editor.contents(), "one one\ntwo one");
    }

    #[cfg(unix)]
    #[test]
    fn terminal_and_make_are_live_editor_workflows() {
        let mut app = App::open(None, None).expect("app");
        app.execute_ex_command(ExCommand::Terminal {
            program: Some("sh".into()),
            arguments: vec!["-c".into(), "printf terminal-ready".into()],
        })
        .expect("terminal");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while app
            .terminal
            .as_ref()
            .is_some_and(|terminal| terminal.exit_code().is_none())
            && std::time::Instant::now() < deadline
        {
            app.poll_terminal().expect("poll terminal");
            thread::yield_now();
        }
        app.poll_terminal().expect("final terminal poll");
        assert!(
            app.terminal
                .as_ref()
                .expect("terminal session")
                .surface()
                .contents()
                .contains("terminal-ready")
        );

        app.execute_ex_command(ExCommand::Make {
            program: "echo".into(),
            arguments: vec!["task-ready".into()],
        })
        .expect("make");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while app.active_task.is_some() && std::time::Instant::now() < deadline {
            app.poll_task_results().expect("poll task");
            thread::yield_now();
        }
        assert!(app.message.contains("task-ready"));

        app.dispatch_key(KeyEvent::character('i'));
        for character in "let value".chars() {
            app.dispatch_key(KeyEvent::character(character));
        }
        app.dispatch_key(KeyEvent::plain(KeyCode::Escape));
        app.execute_ex_command(ExCommand::Format {
            program: "tr".into(),
            arguments: vec!["a-z".into(), "A-Z".into()],
        })
        .expect("format");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while app.active_task.is_some() && std::time::Instant::now() < deadline {
            app.poll_task_results().expect("poll formatter");
            thread::yield_now();
        }
        assert_eq!(app.active.editor.contents(), "LET VALUE");
    }

    #[test]
    fn ex_ranges_global_buffers_splits_and_tabs_execute_in_the_app() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let first = directory.path().join("first.txt");
        let second = directory.path().join("second.txt");
        fs::write(&first, "keep one\ndrop one\nkeep one\n").expect("first file");
        fs::write(&second, "second\n").expect("second file");
        let mut app = App::open(Some(&first), None).expect("open first");

        app.execute_ex("2s/one/TWO/g").expect("ranged substitute");
        wait_for_task(&mut app);
        assert_eq!(
            app.active.editor.contents(),
            "keep one\ndrop TWO\nkeep one\n"
        );

        app.execute_ex("g/drop/normal dd").expect("global delete");
        assert_eq!(app.active.editor.contents(), "keep one\nkeep one\n");
        let ranged = directory.path().join("ranged.txt");
        app.execute_ex(&format!("1,1write {}", ranged.display()))
            .expect("range write");
        assert_eq!(
            fs::read_to_string(&ranged).expect("ranged output"),
            "keep one\n"
        );

        app.execute_ex(&format!("edit! {}", second.display()))
            .expect("open second");
        assert_eq!(app.inactive.len(), 1);
        app.execute_ex("bprevious").expect("previous buffer");
        let canonical_first = fs::canonicalize(&first).expect("canonical first path");
        assert_eq!(
            app.active.document.presentation_path(),
            Some(canonical_first.as_path())
        );
        app.execute_ex("vsplit").expect("split");
        assert_eq!(app.views.windows.len(), 2);
        app.execute_ex("close!").expect("close split");
        assert_eq!(app.views.windows.len(), 1);
        app.execute_ex("tabnew").expect("new tab");
        assert_eq!(app.views.tabs.len(), 2);
        app.execute_ex("tabclose").expect("close tab");
        assert_eq!(app.views.tabs.len(), 1);
    }

    #[test]
    fn provider_completion_is_revision_checked_and_accepted_atomically() {
        let (document, opened) = LocalDocument::unnamed();
        let mut app = App::from_opened(document, opened, None, None).expect("app");
        app.dispatch_key(KeyEvent::character('i'));
        for character in "alphabet alp".chars() {
            app.dispatch_key(KeyEvent::character(character));
        }
        app.request_completion();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while app.completion.is_none() {
            app.poll_provider_results();
            assert!(std::time::Instant::now() < deadline, "completion timed out");
            thread::yield_now();
        }
        app.accept_completion().expect("accept completion");
        assert_eq!(app.active.editor.contents(), "alphabet alphabet");
    }

    #[test]
    fn dotfile_language_profiles_cover_declared_filetypes_and_tools() {
        assert_eq!(
            language_bundle(Some(Path::new("message.msg")))
                .language_id
                .as_ref(),
            "cpp"
        );
        assert_eq!(
            language_server_invocation(Some(Path::new("component.tsx")))
                .expect("TypeScript server")
                .language_id,
            "typescriptreact"
        );
        assert_eq!(
            formatter_invocation(Path::new("module.nix"))
                .expect("Nix formatter")
                .program,
            "nixfmt"
        );
        assert_eq!(
            formatter_invocation(Path::new("Main.hs"))
                .expect("Haskell formatter")
                .program,
            "fourmolu"
        );
    }

    #[test]
    fn dotfile_sleuth_textwidth_snippets_and_file_uris_round_trip() {
        assert_eq!(
            detect_indent_style("fn main() {\n    value();\n}\n"),
            IndentStyle {
                expand_tabs: true,
                width: 4
            }
        );
        assert_eq!(
            wrap_editor_text("// one two three four five\n", 15),
            "// one two\n// three four\n// five\n"
        );
        assert_eq!(
            expand_lsp_snippet("call(${1:value}, ${2|yes,no|})$0"),
            "call(value, yes)"
        );
        let path = Path::new("/tmp/wren uri/naïve.rs");
        assert_eq!(path_from_file_uri(&file_uri(path)).expect("file URI"), path);
    }

    #[test]
    fn dotfile_git_hunks_stage_the_in_memory_buffer_without_saving_it() {
        let directory = tempfile::tempdir().expect("temporary Git repository");
        let root = directory.path();
        assert!(
            Command::new("git")
                .current_dir(root)
                .arg("init")
                .status()
                .expect("git init")
                .success()
        );
        let relative = Path::new("sample.txt");
        fs::write(root.join(relative), "one\ntwo\n").expect("initial source");
        assert!(
            Command::new("git")
                .current_dir(root)
                .args(["add", "--"])
                .arg(relative)
                .status()
                .expect("git add")
                .success()
        );
        let patch =
            make_git_patch(root, relative, "one\ntwo\n", "one\nchanged\n").expect("buffer patch");
        let hunk = select_git_hunk(&patch, 2, None).expect("selected hunk");
        git_apply_patch(root, &hunk, true, false).expect("stage selected hunk");
        assert_eq!(
            git_index_contents(root, relative).expect("index"),
            "one\nchanged\n"
        );
        assert_eq!(
            fs::read_to_string(root.join(relative)).expect("worktree"),
            "one\ntwo\n"
        );
    }

    #[test]
    fn lsp_location_links_use_target_selection_and_utf16_columns() {
        let locations = parse_lsp_locations(&serde_json::json!({
            "targetUri": "file:///tmp/wren-target.rs",
            "targetRange": {
                "start": {"line": 8, "character": 30},
                "end": {"line": 8, "character": 40}
            },
            "targetSelectionRange": {
                "start": {"line": 2, "character": 2},
                "end": {"line": 2, "character": 8}
            },
            "originSelectionRange": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 4}
            }
        }))
        .expect("LocationLink");
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].path, PathBuf::from("/tmp/wren-target.rs"));
        assert_eq!((locations[0].line, locations[0].column), (3, 3));
        assert!(locations[0].column_utf16);

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("unicode.rs");
        fs::write(&path, "😀target\n").expect("write source");
        let app = App::open(Some(&path), None).expect("open app");
        let entry = QuickfixEntry {
            path,
            line: 1,
            column: 3,
            column_utf16: true,
            text: String::new(),
        };
        assert_eq!(app.entry_cursor_byte(&entry), "😀".len());
    }

    #[test]
    fn lsp_semantic_tokens_override_tree_sitter_with_dotfile_groups() {
        let text = "😀 value.call()\n";
        let legend = SemanticTokenLegend {
            token_types: vec!["variable".to_owned(), "method".to_owned()],
            token_modifiers: vec!["readonly".to_owned()],
        };
        let spans = parse_semantic_tokens(
            text,
            &serde_json::json!({
                "data": [
                    0, 3, 5, 0, 1,
                    0, 6, 4, 1, 0
                ]
            }),
            &legend,
        );
        let value = text.find("value").expect("value");
        let call = text.find("call").expect("call");
        assert_eq!(
            spans,
            vec![
                HighlightSpan {
                    range: value..value + 5,
                    kind: "constant".into(),
                },
                HighlightSpan {
                    range: call..call + 4,
                    kind: "method".into(),
                },
            ]
        );
    }

    #[test]
    fn lsp_navigation_mouse_scroll_and_cross_buffer_jumplist_round_trip() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let first = directory.path().join("first.rs");
        let second = directory.path().join("second.rs");
        fs::write(
            &first,
            (0..40)
                .map(|line| format!("fn first_{line}() {{}}\n"))
                .collect::<String>(),
        )
        .expect("write first");
        fs::write(&second, "fn target() {}\n").expect("write second");
        let mut app = App::open(Some(&first), None).expect("open app");
        app.viewport_rows = 10;
        app.handle_input(TerminalInput::MouseScroll {
            lines: 3,
            column: 2,
            row: 4,
        })
        .expect("wheel scroll");
        assert_eq!(app.views.active_window().top_line, 3);

        let origin = app.active.editor.primary_cursor();
        let target = QuickfixEntry {
            path: second.clone(),
            line: 1,
            column: 4,
            column_utf16: true,
            text: "definition".to_owned(),
        };
        app.navigate_to_entry(&target).expect("go to definition");
        assert_eq!(app.client_state.jump_list.len(), 2);
        assert_eq!(app.client_state.jump_index, Some(1));
        assert!(
            app.active
                .document
                .presentation_path()
                .is_some_and(|path| same_path(path, &second))
        );
        assert_eq!(app.active.editor.primary_cursor(), 3);
        assert!(app.navigate_jump_count(true, 1).expect("Ctrl-O"));
        assert!(
            app.active
                .document
                .presentation_path()
                .is_some_and(|path| same_path(path, &first))
        );
        assert_eq!(app.active.editor.primary_cursor(), origin);
        assert!(app.navigate_jump_count(false, 1).expect("Ctrl-I"));
        assert!(
            app.active
                .document
                .presentation_path()
                .is_some_and(|path| same_path(path, &second))
        );
    }

    #[test]
    fn typed_user_keymap_overrides_are_executed_by_the_runtime_registry() {
        let mut keymap = RuntimeKeymap::defaults();
        keymap
            .overlay_user_config(
                r#"
[keys.normal."space z"]
command = "editor.quit"
when = "language == 'text' && !remote"
"#,
            )
            .expect("typed keymap");
        assert_eq!(
            normalize_leader_sequence("space f f").as_deref(),
            Some("ff")
        );
        assert_eq!(
            normalize_leader_sequence("space space").as_deref(),
            Some(" ")
        );
        assert!(
            keymap
                .overlay_user_config(
                    r#"
[keys.normal."space z"]
command = "editor.quit"
[keys.normal."space z".args]
unknown = true
"#,
                )
                .is_err()
        );

        let mut app = App::open(None, None).expect("app");
        app.keymap = keymap;
        app.handle_editor_key(terminal_character(' '))
            .expect("leader");
        app.handle_editor_key(terminal_character('z'))
            .expect("configured quit");
        assert!(app.quit);
    }

    #[test]
    fn macro_raw_keys_and_introspection_ir_are_durable_client_state() {
        let mut app = App::open(None, None).expect("app");
        for key in [
            KeyEvent::character('q'),
            KeyEvent::character('a'),
            KeyEvent::character('i'),
            KeyEvent::character('x'),
            KeyEvent::plain(KeyCode::Escape),
            KeyEvent::character('q'),
        ] {
            app.dispatch_key(key);
        }
        let recording = app
            .client_state
            .macro_recordings
            .get(&'a')
            .expect("durable macro");
        let keys: Vec<KeyEvent> =
            serde_json::from_slice(&recording.raw_keys).expect("raw macro keys");
        let ir: Vec<String> =
            serde_json::from_slice(&recording.lowered_ir).expect("macro introspection IR");
        assert_eq!(keys.len(), 3);
        assert_eq!(ir.len(), keys.len());
    }

    fn wait_for_task(app: &mut App) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !app.poll_task_results().expect("poll") {
            assert!(std::time::Instant::now() < deadline, "task timed out");
            thread::yield_now();
        }
    }
}
