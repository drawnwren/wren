use std::collections::VecDeque;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

// Debug unit tests share the process with dozens of parser/provider threads;
// release keeps the product budget while the hard isolated distributions live
// in `wren-latency`.
const fn test_latency_budget(release: Duration, debug: Duration) -> Duration {
    if cfg!(debug_assertions) {
        debug
    } else {
        release
    }
}

fn terminal_character(character: char) -> TerminalKey {
    TerminalKey {
        code: TerminalKeyCode::Char(character),
        shift: false,
        control: false,
        alt: false,
        super_key: false,
    }
}

fn terminal_key(code: TerminalKeyCode) -> TerminalKey {
    TerminalKey {
        code,
        shift: false,
        control: false,
        alt: false,
        super_key: false,
    }
}

fn terminal_control(character: char) -> TerminalKey {
    TerminalKey {
        code: TerminalKeyCode::Char(character),
        shift: false,
        control: true,
        alt: false,
        super_key: false,
    }
}

fn type_prompt(app: &mut App, text: &str) {
    for character in text.chars() {
        app.handle_prompt_key(terminal_character(character))
            .expect("type prompt");
    }
}

fn submit_prompt(app: &mut App) {
    app.handle_prompt_key(terminal_key(TerminalKeyCode::Enter))
        .expect("submit prompt");
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
        result = {"capabilities": {
            "semanticTokensProvider": {"legend": {
                "tokenTypes": ["variable"], "tokenModifiers": []
            }, "full": True},
            "declarationProvider": True,
            "definitionProvider": True,
            "implementationProvider": True,
            "referencesProvider": True
        }}
    elif method == "textDocument/definition":
        time.sleep(0.2)
        result = None
    elif method == "textDocument/hover":
        time.sleep(0.2)
        result = {"contents": "delayed hover details"}
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
            initialization_options: serde_json::Value::Null,
            settings: serde_json::Value::Null,
        },
        log,
    )
}

#[test]
fn parses_cli_file_and_line() {
    let cli = Cli::parse(["+42".to_owned(), "src/main.rs".to_owned()].into_iter()).expect("parse");
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
    worker.append_frame([0; 32], 1, FrameText::from("edit"), 4);
    worker.barrier().expect("barrier");
    assert!(wal.recover_latest().expect("recover").is_some());
    worker.clear().expect("clear");
    assert_eq!(wal.recover_latest().expect("recover"), None);
}

#[test]
fn wal_worker_coalesces_a_burst_without_losing_the_latest_revision() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let wal = LocalWal::in_directory(directory.path(), b"app-coalescing-test");
    let worker = WalWorker::start(wal.clone());
    for revision in 1..=100 {
        worker.append_frame(
            [0; 32],
            revision,
            FrameText::from(format!("revision {revision}")),
            revision as usize,
        );
    }
    worker.barrier().expect("barrier");

    let recovered = wal
        .recover_latest()
        .expect("recover latest")
        .expect("recovery state");
    assert_eq!(recovered.revision, 100);
    assert_eq!(recovered.text, "revision 100");
    assert_eq!(recovered.cursor, 100);
}

#[test]
fn wal_worker_removes_recovery_state_when_edits_return_to_the_saved_base() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let wal = LocalWal::in_directory(directory.path(), b"app-clean-state-test");
    let worker = WalWorker::start(wal.clone());
    let base_hash = *blake3::hash(b"base").as_bytes();
    worker.append_frame(base_hash, 1, FrameText::from("changed"), 7);
    worker.barrier().expect("dirty barrier");
    assert!(wal.recover_latest().expect("dirty recovery").is_some());

    worker.append_frame(base_hash, 2, FrameText::from("base"), 4);
    worker.barrier().expect("clean barrier");
    assert_eq!(wal.recover_latest().expect("clean recovery"), None);
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
fn space_w_uses_the_same_write_path_without_quitting_a_new_file() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("new-file.txt");
    let (document, opened) = LocalDocument::open_or_new(&path).expect("open new file");
    let mut app = App::from_opened(document, opened, None, None).expect("create app");
    app.dispatch_key(KeyEvent::character('i'));
    for character in "new contents".chars() {
        app.dispatch_key(KeyEvent::character(character));
    }
    app.dispatch_key(KeyEvent::plain(KeyCode::Escape));

    app.handle_editor_key(terminal_character(' '))
        .expect("leader");
    app.handle_editor_key(terminal_character('w'))
        .expect("write");

    assert_eq!(
        fs::read_to_string(&path).expect("saved source"),
        "new contents"
    );
    assert!(!app.active.editor.is_dirty());
    assert!(!app.quit);
    assert!(
        app.leader_keys.is_none(),
        "Space-w must execute immediately"
    );
}

#[test]
fn realtime_path_preparation_cannot_mutate_the_live_buffer() {
    let (document, mut opened) = LocalDocument::unnamed();
    opened.text = "fn live() { let untouched = 1; }\n".to_owned();
    let mut app = App::from_opened(document, opened, None, None).expect("app");
    app.active.editor.set_cursor(8);
    app.message = "preserve me".to_owned();
    let contents = app.active.editor.contents();
    let revision = app.active.editor.revision();
    let undo = app.active.editor.durable_undo_state();
    let cursor = app.active.editor.primary_cursor();
    let mode = app.active.editor.mode();

    app.prepare_realtime_paths().expect("prepare paths");

    assert_eq!(app.active.editor.contents(), contents);
    assert_eq!(app.active.editor.revision(), revision);
    assert_eq!(app.active.editor.durable_undo_state(), undo);
    assert_eq!(app.active.editor.primary_cursor(), cursor);
    assert_eq!(app.active.editor.mode(), mode);
    assert!(!app.active.editor.is_dirty());
    assert_eq!(app.message, "preserve me");
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
fn quit_succeeds_after_undo_restores_the_opened_file() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("main.rs");
    fs::write(&path, "fn main() {}\n").expect("write source");
    let mut app = App::open(Some(&path), None).expect("open source");

    app.dispatch_key(KeyEvent::character('i'));
    app.dispatch_key(KeyEvent::character('x'));
    app.dispatch_key(KeyEvent::plain(KeyCode::Escape));
    assert!(app.active.editor.is_dirty());
    app.dispatch_key(KeyEvent::character('u'));

    assert_eq!(app.active.editor.contents(), "fn main() {}\n");
    assert!(!app.active.editor.is_dirty());
    assert!(!app.status().contains("[+]"));
    app.execute_ex("q").expect("quit restored buffer");
    assert!(app.quit);
}

#[test]
fn dotfile_leader_q_and_ff_are_exact_native_sequences() {
    let mut clean = App::open(None, None).expect("clean app");
    clean
        .handle_editor_key(terminal_character(' '))
        .expect("leader");
    assert!(
        clean.popup.as_ref().is_some_and(|popup| {
            popup.title.contains("NORMAL") && popup.text.contains("+find")
        })
    );
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
fn leader_a_is_the_embedded_agent_terminal_binding() {
    let keymap = RuntimeKeymap::defaults();
    let binding = keymap.leader.get("a").expect("leader-a binding");
    assert_eq!(binding.invocation.command.as_ref(), "ai.chat");
    assert_eq!(binding.description.as_ref(), "Oh My Pi pane");
}

#[test]
#[cfg(unix)]
fn focused_agent_sidebar_forwards_input_and_terminal_escape_returns_to_editor() {
    let mut app = App::open(None, None).expect("app");
    app.resize_terminal(12, 80);
    app.open_agent_sidebar_in(
        "sh",
        &[
            "-c",
            "stty -echo; IFS= read -r line; printf 'HARNESS:%s' \"$line\"; sleep 1",
        ],
    )
    .expect("open embedded harness");
    app.handle_input(TerminalInput::Paste("a界b".to_owned()))
        .expect("paste into harness");
    app.handle_input(TerminalInput::Key(terminal_key(TerminalKeyCode::Enter)))
        .expect("submit harness input");
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.poll_agent_terminal().expect("poll harness");
        if app
            .agent_terminal
            .as_ref()
            .is_some_and(|terminal| terminal.surface().contents().contains("HARNESS:a界b"))
        {
            break;
        }
        thread::yield_now();
    }
    assert!(
        app.agent_terminal
            .as_ref()
            .is_some_and(|terminal| terminal.surface().contents().contains("HARNESS:a界b"))
    );
    let mut layout = ViewportLayout::new(80, 12);
    layout.configure_dotfile_profile();
    let rendered = desired_frame(&mut layout, &app)
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().map(|cell| cell.grapheme.as_ref()))
        .collect::<String>();
    assert!(rendered.contains("HARNESS:a界b"));

    app.handle_input(TerminalInput::Key(terminal_control('\\')))
        .expect("terminal escape prefix");
    app.handle_input(TerminalInput::Key(terminal_control('n')))
        .expect("return focus to editor");
    assert!(app.agent_sidebar.visible);
    assert!(!app.agent_sidebar.focused);
}

#[test]
fn embedded_agent_mouse_coordinates_are_relative_to_the_harness_surface() {
    let mut app = App::open(None, None).expect("app");
    app.resize_terminal(20, 100);
    let start = ViewportLayout::terminal_sidebar_column_for_size(100, 20)
        .expect("sidebar")
        .saturating_add(1);
    assert_eq!(
        app.agent_local_mouse_event(&TerminalInput::MouseScroll {
            lines: -3,
            column: start + 7,
            row: 4,
        }),
        Some(TerminalInput::MouseScroll {
            lines: -3,
            column: 7,
            row: 4,
        })
    );
    assert_eq!(
        app.agent_local_mouse_event(&TerminalInput::MouseClick {
            column: start + 2,
            row: 9,
        }),
        Some(TerminalInput::MouseClick { column: 2, row: 9 })
    );
}

#[test]
#[cfg(unix)]
fn embedded_agent_participates_in_ctrl_w_window_navigation() {
    let mut app = App::open(None, None).expect("app");
    app.resize_terminal(20, 100);
    app.open_agent_sidebar_in("sh", &["-c", "sleep 5"])
        .expect("open embedded harness");

    app.handle_input(TerminalInput::Key(terminal_character('q')))
        .expect("ordinary q reaches harness");
    assert!(app.agent_sidebar.visible && app.agent_sidebar.focused);

    app.handle_input(TerminalInput::Key(terminal_control('w')))
        .expect("window prefix in harness");
    app.handle_input(TerminalInput::Key(terminal_character('h')))
        .expect("focus editor");
    assert!(app.agent_sidebar.visible);
    assert!(!app.agent_sidebar.focused);

    app.handle_input(TerminalInput::Key(terminal_control('w')))
        .expect("window prefix in editor");
    app.handle_input(TerminalInput::Key(terminal_character('l')))
        .expect("focus harness");
    assert!(app.agent_sidebar.focused);

    app.handle_input(TerminalInput::Key(terminal_control('w')))
        .expect("window prefix in harness");
    app.handle_input(TerminalInput::Key(terminal_character('q')))
        .expect("hide harness window");
    assert!(!app.agent_sidebar.visible);
    assert!(!app.agent_sidebar.focused);
    assert!(
        app.agent_terminal
            .as_ref()
            .is_some_and(|terminal| terminal.exit_code().is_none())
    );
}

#[test]
fn normal_prefixes_open_which_key_hints_without_consuming_the_next_key() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("motions.txt");
    fs::write(&path, "first\nsecond\n").expect("write source");
    let mut app = App::open(Some(&path), Some(2)).expect("open source");

    app.handle_editor_key(terminal_character('g'))
        .expect("open goto hints");

    assert_eq!(app.normal_prefix, Some('g'));
    assert!(app.popup.as_ref().is_some_and(|popup| {
        popup.title.as_ref() == " goto "
            && popup.text.contains("g  first line / [count] line")
            && popup.text.contains("q  format to text width")
            && !popup.text.contains("definition")
            && !popup.text.contains("references")
    }));

    app.handle_editor_key(terminal_character('g'))
        .expect("execute gg through hint");

    assert!(app.popup.is_none());
    assert_eq!(app.normal_prefix, None);
    assert_eq!(app.active.editor.primary_cursor(), 0);

    app.handle_editor_key(terminal_character('['))
        .expect("open previous hints");
    assert!(app.popup.as_ref().is_some_and(|popup| {
        popup.title.as_ref() == " previous "
            && popup.text.contains("previous diagnostic")
            && popup.text.contains("previous Git hunk")
    }));
}

#[test]
fn goto_hints_include_only_advertised_lsp_navigation() {
    let entries = app_interaction::normal_prefix_hint_entries(
        'g',
        Some(LspNavigationCapabilities {
            declaration: false,
            definition: true,
            implementation: false,
            references: true,
        }),
    );

    assert_eq!(entries.get("d").map(String::as_str), Some("definition"));
    assert_eq!(entries.get("r").map(String::as_str), Some("references"));
    assert!(!entries.contains_key("D"));
    assert!(!entries.contains_key("i"));
    assert!(entries.contains_key("g"));
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
    app.handle_editor_key(terminal_control('d'))
        .expect("half page down");
    assert_eq!(app.active.editor.cursor_line_column().0, 4);
    assert_eq!(app.views.active_window().top_line, 4);
    assert!(!app.message.contains("grammar"));
    app.handle_editor_key(terminal_control('u'))
        .expect("half page up");
    assert_eq!(app.active.editor.cursor_line_column().0, 0);
    assert_eq!(app.views.active_window().top_line, 0);
    app.handle_editor_key(terminal_control('f'))
        .expect("page down");
    assert_eq!(app.active.editor.cursor_line_column().0, 7);
    app.handle_editor_key(terminal_control('b'))
        .expect("page up");
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
    assert_counted_view_navigation(&mut app);
    assert_window_commands(&mut app);
    assert_number_and_replace_commands(&mut app);
}

fn assert_counted_view_navigation(app: &mut App) {
    app.handle_editor_key(terminal_character('2'))
        .expect("count");
    app.handle_editor_key(terminal_control('d'))
        .expect("counted half page");
    assert_eq!(app.active.editor.cursor_line_column().0, 2);
    assert_eq!(app.views.active_window().top_line, 2);
    app.handle_editor_key(terminal_character('3'))
        .expect("count");
    app.handle_editor_key(terminal_control('e'))
        .expect("counted line scroll");
    assert_eq!(app.views.active_window().top_line, 5);
    app.handle_editor_key(terminal_character('2'))
        .expect("count");
    app.handle_editor_key(terminal_character('H'))
        .expect("second from top");
    assert_eq!(app.active.editor.cursor_line_column().0, 6);
}

fn assert_window_commands(app: &mut App) {
    app.handle_editor_key(terminal_control('w'))
        .expect("window prefix");
    app.handle_editor_key(terminal_character('v'))
        .expect("vertical split");
    assert_eq!(app.views.windows.len(), 2);
    app.handle_editor_key(terminal_control('w'))
        .expect("window prefix");
    app.handle_editor_key(terminal_character('h'))
        .expect("focus left");
    assert_eq!(app.views.windows.len(), 2);
    assert!(!app.message.contains("grammar"));
}

fn assert_number_and_replace_commands(app: &mut App) {
    app.active.editor.set_cursor(0);
    app.handle_editor_key(terminal_control('a'))
        .expect("increment");
    assert!(app.active.editor.contents().starts_with("line 001"));
    app.handle_editor_key(terminal_control('x'))
        .expect("decrement");
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
        opened_at.elapsed()
            < test_latency_budget(Duration::from_millis(250), Duration::from_millis(500)),
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
    let changed_spans = changed.spans_in(0..app.active.editor.text().len_bytes());
    assert!(
        changed_spans
            .iter()
            .any(|span| span.range.start == 0 && span.range.end >= 3),
        "newly inserted keyword must be highlighted before provider polling"
    );
    let shifted_last = last_function + "pub ".len();
    assert!(
        changed_spans
            .iter()
            .any(|span| span.range.contains(&shifted_last))
    );

    let second_opened_at = Instant::now();
    app.open_buffer(&second).expect("open second");
    assert!(
        second_opened_at.elapsed()
            < test_latency_budget(Duration::from_millis(250), Duration::from_millis(500)),
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

fn assert_prepared_edit_decorations(app: &App, edit_range: Range<usize>) {
    for (label, state) in [
        (
            "syntax",
            app.decorations
                .get(&app.active.buffer_id)
                .expect("syntax decorations"),
        ),
        (
            "semantic",
            app.semantic_decorations
                .get(&app.active.buffer_id)
                .expect("semantic decorations"),
        ),
    ] {
        let cache = state.visible_cache.borrow();
        assert!(
            cache.iter().any(|cached| {
                cached.range == edit_range
                    && decoration_transforms_equal(&cached.transforms, &state.transforms)
                    && cached.overrides == state.overrides
                    && cached.invalidated == state.invalidated
            }),
            "{label} prepared cache missing: wanted={edit_range:?} transforms={} overrides={} invalidated={:?}, cached={:?}",
            state.transforms.len(),
            state.overrides.len(),
            state.invalidated,
            cache
                .iter()
                .map(|cached| (
                    &cached.range,
                    cached.transforms.len(),
                    cached.overrides.len(),
                    &cached.invalidated
                ))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn large_rust_open_navigation_and_edit_stay_within_frame_budgets() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let source = directory.path().join("large.rs");
    let text = (0..14_000)
            .map(|line| {
                format!(
                    "pub fn item_{line:05}() -> usize {{ let value_{line:05}: usize = {line}; value_{line:05} }}\n"
                )
            })
            .collect::<String>();
    fs::write(&source, &text).expect("large Rust source");

    let opened_at = Instant::now();
    let mut app = App::open(Some(&source), None).expect("open large Rust source");
    let open_elapsed = opened_at.elapsed();
    app.active.git_index_text = Some(Arc::from(text));
    app.active.refresh_git_hunks();
    let span_count = app
        .decorations
        .get(&app.active.buffer_id)
        .map_or(0, |state| state.spans.len());
    let semantic = app
        .decorations
        .get(&app.active.buffer_id)
        .map(|decorations| {
            decorations
                .spans
                .iter()
                .step_by(6)
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    app.semantic_decorations.insert(
        app.active.buffer_id,
        BufferDecorations::new(app.active.editor.revision(), semantic),
    );
    app.active
        .editor
        .search("value_", SearchDirection::Forward)
        .expect("search highlight");
    app.search_highlight = true;
    app.diagnostics.push(DiagnosticEntry {
        path: source.clone(),
        line: 1,
        column: 1,
        severity: DiagnosticSeverity::Warning,
        message: "benchmark diagnostic".to_owned(),
    });
    let mut layout = ViewportLayout::new(120, 40);
    layout.configure_dotfile_profile();
    app.resize_terminal(40, 120);
    app.schedule_provider_refreshes(layout.height);
    let (_frame, first_frame) = desired_frame_profiled(&mut layout, &app);
    let last_line = app.active.editor.text().byte_of_line(13_999);
    let navigation_input_at = Instant::now();
    app.active.editor.set_cursor(last_line);
    let navigation_input_elapsed = navigation_input_at.elapsed();
    let navigation_schedule_at = Instant::now();
    app.schedule_provider_refreshes(layout.height);
    let navigation_schedule_elapsed = navigation_schedule_at.elapsed();
    let (_frame, navigation_frame) = desired_frame_profiled(&mut layout, &app);
    app.dispatch_key(KeyEvent::character('h'));
    let (_frame, local_motion_frame) = desired_frame_profiled(&mut layout, &app);
    app.handle_editor_key(TerminalKey {
        code: TerminalKeyCode::Char('u'),
        shift: false,
        control: true,
        alt: false,
        super_key: false,
    })
    .expect("half-page viewport motion");
    let (_frame, viewport_frame) = desired_frame_profiled(&mut layout, &app);
    app.handle_editor_key(TerminalKey {
        code: TerminalKeyCode::Char('d'),
        shift: false,
        control: true,
        alt: false,
        super_key: false,
    })
    .expect("return to the document-end viewport");
    let before_edit = desired_frame(&mut layout, &app);
    app.dispatch_key(KeyEvent::character('i'));
    let edit_input_at = Instant::now();
    app.dispatch_key(KeyEvent::character('x'));
    let edit_input_elapsed = edit_input_at.elapsed();
    let edit_schedule_at = Instant::now();
    app.schedule_provider_refreshes(layout.height);
    let edit_schedule_elapsed = edit_schedule_at.elapsed();
    let edit_range = visible_byte_range(&app, app.active.buffer_id).expect("edit viewport");
    assert_prepared_edit_decorations(&app, edit_range);
    let (edited_grid, edit_frame) = desired_frame_profiled(&mut layout, &app);
    let retained_edit_rows = before_edit
        .rows
        .iter()
        .zip(&edited_grid.rows)
        .filter(|(before, after)| Arc::ptr_eq(before, after))
        .count();
    assert!(
        retained_edit_rows >= layout.height.saturating_sub(3),
        "same-line edit rebuilt too many rows: retained {retained_edit_rows} of {}",
        layout.height
    );
    app.dispatch_key(KeyEvent::plain(KeyCode::Escape));
    app.dispatch_key(KeyEvent::character('v'));
    app.dispatch_key(KeyEvent::character('h'));
    let (_frame, selection_frame) = desired_frame_profiled(&mut layout, &app);
    eprintln!(
        "large Rust gate: open={open_elapsed:?} first_frame=[{first_frame}] navigation_input={navigation_input_elapsed:?} navigation_schedule={navigation_schedule_elapsed:?} navigation_frame=[{navigation_frame}] local_motion_frame=[{local_motion_frame}] viewport_frame=[{viewport_frame}] edit_input={edit_input_elapsed:?} edit_schedule={edit_schedule_elapsed:?} edit_frame=[{edit_frame}] retained_edit_rows={retained_edit_rows} selection_frame=[{selection_frame}] spans={span_count}"
    );
    assert!(
        span_count >= 42_000,
        "large-file gate must retain full syntax"
    );
    assert!(
        open_elapsed < test_latency_budget(Duration::from_secs(1), Duration::from_secs(2)),
        "large-file open took {open_elapsed:?}"
    );
    assert!(
        first_frame.total < Duration::from_millis(50),
        "large-file first frame took {first_frame}"
    );
    assert!(
        navigation_input_elapsed < Duration::from_millis(5),
        "large-file bottom navigation input took {navigation_input_elapsed:?}"
    );
    assert!(
        navigation_schedule_elapsed < Duration::from_millis(10),
        "large-file bottom provider scheduling took {navigation_schedule_elapsed:?}"
    );
    assert!(
        navigation_frame.total < Duration::from_millis(20),
        "large-file bottom frame took {navigation_frame}"
    );
    assert!(
        edit_input_elapsed < Duration::from_millis(30),
        "large-file edit input took {edit_input_elapsed:?}"
    );
    assert!(
        edit_schedule_elapsed < Duration::from_millis(10),
        "large-file edit provider scheduling took {edit_schedule_elapsed:?}"
    );
    assert!(
        edit_frame.total < Duration::from_millis(40),
        "large-file edited frame took {edit_frame}"
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
                text: FrameText::from(text.as_str()),
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
    let completed_demands = receiver
        .try_iter()
        .filter(|result| matches!(result, ProviderWorkerResult::Decorations { .. }))
        .count();
    assert!((1..=2).contains(&completed_demands));
}

#[test]
fn background_provider_coalesces_queued_stale_document_revisions() {
    let (sender, requests) = mpsc::channel();
    let (_immediate_sender, immediate_requests) = mpsc::channel();
    let (results, _receiver) = mpsc::channel();
    let (started_sender, started_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let updates = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&updates);
    let worker = thread::spawn(move || {
        let mut actor = ProviderActor::default();
        let mut first = true;
        provider_loop(requests, immediate_requests, results, |request| {
            if let ProviderRequest::UpdateDocument { revision, .. } = request {
                observed.lock().expect("updates").push(*revision);
                if first {
                    first = false;
                    started_sender.send(()).expect("started");
                    release_receiver.recv().expect("release");
                }
            }
            actor.handle(request.clone())
        });
    });
    let make_refresh = |revision| {
        ProviderWorkerMessage::Refresh(Box::new(ProviderRefresh {
            buffer_id: BufferId::new(1),
            document_id: DocumentId::new(1),
            revision: DocumentRevision::new(revision),
            text: FrameText::from(format!("fn revision_{revision}() {{}}\n")),
            bundle: language_bundle(Some(Path::new("coalesced.rs"))),
            visible: 0..24,
            near_viewport: 0..24,
        }))
    };
    sender.send(make_refresh(1)).expect("first refresh");
    started_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("first refresh started");
    for revision in 2..=9 {
        sender
            .send(make_refresh(revision))
            .expect("queue stale refresh");
    }
    release_sender.send(()).expect("release first refresh");
    sender
        .send(ProviderWorkerMessage::Stop)
        .expect("stop provider");
    worker.join().expect("provider worker");
    assert_eq!(
        *updates.lock().expect("updates"),
        vec![DocumentRevision::new(1), DocumentRevision::new(9)]
    );
}

#[test]
fn unchanged_provider_text_advances_revision_without_reupload_or_reparse() {
    let (sender, requests) = mpsc::channel();
    let (_immediate_sender, immediate_requests) = mpsc::channel();
    let (results, receiver) = mpsc::channel();
    let updates = Arc::new(AtomicUsize::new(0));
    let advances = Arc::new(AtomicUsize::new(0));
    let observed_updates = Arc::clone(&updates);
    let observed_advances = Arc::clone(&advances);
    let worker = thread::spawn(move || {
        let mut actor = ProviderActor::default();
        provider_loop(requests, immediate_requests, results, |request| {
            match request {
                ProviderRequest::UpdateDocument { .. } => {
                    observed_updates.fetch_add(1, Ordering::Relaxed);
                }
                ProviderRequest::AdvanceDocumentRevision { .. } => {
                    observed_advances.fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            }
            actor.handle(request.clone())
        });
    });
    let text = FrameText::from("fn unchanged() {}\n");
    let refresh = |revision| {
        ProviderWorkerMessage::Refresh(Box::new(ProviderRefresh {
            buffer_id: BufferId::new(1),
            document_id: DocumentId::new(1),
            revision: DocumentRevision::new(revision),
            text: text.clone(),
            bundle: language_bundle(Some(Path::new("unchanged.rs"))),
            visible: 0..text.len(),
            near_viewport: 0..text.len(),
        }))
    };
    sender.send(refresh(1)).expect("initial refresh");
    receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("initial result");
    sender.send(refresh(2)).expect("revision advance");
    receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("advanced result");
    sender
        .send(ProviderWorkerMessage::Stop)
        .expect("stop provider");
    worker.join().expect("provider worker");
    assert_eq!(updates.load(Ordering::Relaxed), 1);
    assert_eq!(advances.load(Ordering::Relaxed), 1);
}

#[test]
fn changed_provider_revision_waits_for_the_typing_quiet_period() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let source = directory.path().join("debounce.rs");
    let text = (0..200)
        .map(|line| format!("fn debounce_{line}() {{}}\n"))
        .collect::<String>();
    fs::write(&source, &text).expect("source");
    let mut app = App::open(Some(&source), None).expect("app");
    app.dispatch_key(KeyEvent::character('i'));
    app.dispatch_key(KeyEvent::character('x'));
    let revision = app.active.editor.revision();
    app.schedule_provider_refreshes(20);
    assert_ne!(
        app.provider_submitted
            .get(&app.active.document_id)
            .map(|key| key.revision),
        Some(revision)
    );
    app.provider_refresh_due
        .insert(app.active.document_id, Instant::now());
    app.schedule_provider_refreshes(20);
    assert_eq!(
        app.provider_submitted
            .get(&app.active.document_id)
            .map(|key| key.revision),
        Some(revision)
    );
    let submitted = app
        .provider_submitted
        .get(&app.active.document_id)
        .expect("submitted changed-line refresh");
    assert_eq!(submitted.visible_start, submitted.near_start);
    assert_eq!(submitted.visible_end, submitted.near_end);
    assert!(
        submitted.visible_end < text.len() / 4,
        "a local edit must not replace a whole viewport or document of syntax spans"
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
        cursor: None,
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
        cursor: None,
        decorations: Vec::new(),
    });
    app.popup_deadline = Instant::now().checked_sub(Duration::from_millis(1));
    assert!(app.poll_popup_timeout());
    assert!(app.popup.is_none());
    assert!(app.popup_deadline.is_none());
    assert!(!app.poll_popup_timeout());
}

#[test]
fn k_focuses_an_open_popup_instead_of_requesting_another_hover() {
    let mut app = App::open(None, None).expect("app");
    app.popup = Some(TextPopup {
        title: "Documentation".into(),
        text: "first line\nsecond line\nthird line".into(),
        scroll: 0,
        cursor: None,
        decorations: Vec::new(),
    });
    app.popup_deadline = Some(Instant::now() + Duration::from_secs(6));

    app.handle_editor_key(terminal_character('K'))
        .expect("focus popup");

    assert_eq!(
        app.popup.as_ref().and_then(|popup| popup.cursor),
        Some((0, 0))
    );
    assert!(app.popup_deadline.is_none());
    assert!(app.pending_lsp_hover.is_none());

    app.handle_editor_key(terminal_character('j'))
        .expect("navigate popup");
    assert_eq!(
        app.popup.as_ref().and_then(|popup| popup.cursor),
        Some((1, 0))
    );

    app.handle_editor_key(terminal_character('q'))
        .expect("close focused popup");
    assert!(app.popup.is_none());
}

#[test]
fn editor_movement_closes_an_unfocused_popup_and_still_moves_the_cursor() {
    let mut source = tempfile::NamedTempFile::new().expect("source");
    source.write_all(b"abc\n").expect("write source");
    let mut app = App::open(Some(source.path()), None).expect("app");
    app.popup = Some(TextPopup {
        title: "Documentation".into(),
        text: "hover details".into(),
        scroll: 0,
        cursor: None,
        decorations: Vec::new(),
    });
    app.popup_deadline = Some(Instant::now() + Duration::from_secs(6));

    app.handle_editor_key(terminal_character('l'))
        .expect("move through popup dismissal");

    assert!(app.popup.is_none());
    assert!(app.popup_deadline.is_none());
    assert_eq!(app.active.editor.primary_cursor(), 1);
}

#[test]
fn mouse_wheel_navigates_only_a_focused_popup() {
    let mut app = App::open(None, None).expect("app");
    app.popup = Some(TextPopup {
        title: "Documentation".into(),
        text: (0..20)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n")
            .into(),
        scroll: 0,
        cursor: None,
        decorations: Vec::new(),
    });

    app.handle_editor_key(terminal_character('K'))
        .expect("focus popup");
    app.handle_input(TerminalInput::MouseScroll {
        lines: 4,
        column: 10,
        row: 10,
    })
    .expect("scroll focused popup");
    assert_eq!(
        app.popup.as_ref().and_then(|popup| popup.cursor),
        Some((4, 0))
    );

    app.popup.as_mut().expect("popup").cursor = None;
    app.handle_input(TerminalInput::MouseScroll {
        lines: 1,
        column: 10,
        row: 10,
    })
    .expect("scroll enclosing buffer");
    assert!(app.popup.is_none());
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

    assert_messages_buffer(&app);
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

fn assert_messages_buffer(app: &App) {
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
    let revision = attach_fake_root_lsp(&mut app, directory.path(), &source, &workspace_root);
    exercise_lsp_prefix_hints(&mut app);
    exercise_async_lsp_requests(&mut app);
    let (outside_workspace, second) = open_outside_rust(&mut app, &workspace_root);
    exercise_cross_workspace_jumps(&mut app, &source, &second, &workspace_root);
    exercise_non_lsp_jump(&mut app, outside_workspace.path(), &second, &workspace_root);
    exercise_language_server_parking(&mut app, &second, &workspace_root, revision);
    exercise_malformed_definition_recovery(&mut app);
}

#[cfg(unix)]
fn exercise_lsp_prefix_hints(app: &mut App) {
    app.handle_editor_key(terminal_character('g'))
        .expect("open LSP goto hints");
    assert!(app.popup.as_ref().is_some_and(|popup| {
        popup.text.contains("d  definition")
            && popup.text.contains("D  declaration")
            && popup.text.contains("i  implementation")
            && popup.text.contains("r  references")
    }));
    app.handle_editor_key(TerminalKey {
        code: TerminalKeyCode::Escape,
        shift: false,
        control: false,
        alt: false,
        super_key: false,
    })
    .expect("close goto hints");
    assert!(app.popup.is_none());
    assert!(app.normal_prefix.is_none());
}

#[cfg(unix)]
fn attach_fake_root_lsp(
    app: &mut App,
    directory: &Path,
    source: &Path,
    workspace_root: &Path,
) -> DocumentRevision {
    let (server, log) = fake_lsp_server(directory);
    let environment = env::vars()
        .map(|(name, value)| (name.into_boxed_str(), value.into_boxed_str()))
        .collect();
    let revision = DocumentRevision::new(1);
    let started = Instant::now();
    let (client, uri, capabilities) = spawn_lsp_client(
        &server,
        source,
        workspace_root,
        revision,
        "fn main() { target(); }\n",
        environment,
    )
    .expect("start fake LSP");
    assert_elapsed_below(
        started,
        test_latency_budget(Duration::from_millis(100), Duration::from_millis(500)),
        "client readiness waited on post-initialize work",
    );
    let startup_log = wait_for_log(&log, "textDocument/didOpen");
    assert!(startup_log.contains("initialize"));
    assert!(startup_log.contains(&file_uri(workspace_root)));
    assert!(startup_log.contains("textDocument/didOpen"));
    assert!(!startup_log.contains("semanticTokens/full"));

    let document_id = app.active.document_id;
    app.lsp = Some(PersistentLsp {
        document_id,
        revision,
        uri: uri.clone(),
        client,
        server: language_server_invocation(Some(source)).expect("Rust profile"),
        root: workspace_root.to_path_buf(),
        open_documents: BTreeMap::from([(document_id, LspOpenDocument { uri, revision })]),
        capabilities,
        semantic_due: None,
    });
    let lsp = app.lsp.as_ref().expect("attached LSP");
    app.lsp_navigation_capabilities.insert(
        lsp.server.language_id.clone().into_boxed_str(),
        lsp.capabilities.navigation,
    );
    revision
}

#[cfg(unix)]
fn wait_for_log(path: &Path, expected: &str) -> String {
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        let current = fs::read_to_string(path).unwrap_or_default();
        if current.contains(expected) {
            return current;
        }
        assert!(Instant::now() < deadline, "{expected} was not observed");
        thread::yield_now();
    }
}

#[cfg(unix)]
fn assert_elapsed_below(started: Instant, budget: Duration, operation: &str) {
    assert!(
        started.elapsed() < budget,
        "{operation}: {:?}",
        started.elapsed()
    );
}

#[cfg(unix)]
fn poll_lsp_until_complete(app: &mut App, operation: &str) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while app.lsp_background.is_some() {
        assert!(Instant::now() < deadline, "fake {operation} timed out");
        let _ = app.poll_lsp_background().expect("poll LSP operation");
        thread::yield_now();
    }
}

#[cfg(unix)]
fn exercise_async_lsp_requests(app: &mut App) {
    let semantic_due = Instant::now() + Duration::from_secs(10);
    app.lsp.as_mut().expect("fake LSP").semantic_due = Some(semantic_due);
    app.handle_input(TerminalInput::Key(terminal_character('l')))
        .expect("normal-mode movement");
    assert_eq!(
        app.lsp.as_ref().and_then(|lsp| lsp.semantic_due),
        Some(semantic_due),
        "cursor movement must not postpone semantic highlighting"
    );
    let dispatched = Instant::now();
    app.lsp_location("textDocument/definition")
        .expect("dispatch gd");
    assert_elapsed_below(
        dispatched,
        Duration::from_millis(20),
        "gd blocked the input loop",
    );
    assert!(app.lsp_background.is_some());
    app.handle_editor_key(terminal_character('g'))
        .expect("show goto hints while LSP request runs");
    assert!(app.popup.as_ref().is_some_and(|popup| {
        popup.text.contains("d  definition") && popup.text.contains("r  references")
    }));
    app.handle_editor_key(terminal_key(TerminalKeyCode::Escape))
        .expect("close busy LSP hints");
    poll_lsp_until_complete(app, "definition");
    assert!(app.message.contains("no location"));

    let dispatched = Instant::now();
    app.handle_editor_key(terminal_character('K'))
        .expect("dispatch hover");
    assert_elapsed_below(
        dispatched,
        Duration::from_millis(20),
        "K blocked the input loop",
    );
    assert!(app.lsp_background.is_some());
    let motion = Instant::now();
    app.handle_editor_key(terminal_character('l'))
        .expect("move while hover is pending");
    assert_elapsed_below(
        motion,
        Duration::from_millis(20),
        "pending hover blocked ordinary input",
    );
    poll_lsp_until_complete(app, "hover");
    assert!(
        app.popup
            .as_ref()
            .is_some_and(|popup| { popup.text.contains("delayed hover details") })
    );
    app.popup = None;
}

#[cfg(unix)]
fn open_outside_rust(app: &mut App, workspace_root: &Path) -> (tempfile::TempDir, PathBuf) {
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
        std::fs::canonicalize(workspace_root).ok().as_deref()
    );
    app.navigate_to_entry(&QuickfixEntry {
        path: second.clone(),
        line: 1,
        column: 1,
        selection_end: None,
        column_utf16: true,
        text: "outside definition".to_owned(),
    })
    .expect("cross-workspace navigation open");
    assert!(app.lsp_start.is_none());
    let reused = app.lsp.as_ref().expect("LSP was discarded");
    assert_eq!(reused.document_id, app.active.document_id);
    assert_eq!(reused.open_documents.len(), 2);
    assert_eq!(reused.root, workspace_root);
    (outside_workspace, second)
}

#[cfg(unix)]
fn exercise_cross_workspace_jumps(
    app: &mut App,
    source: &Path,
    second: &Path,
    workspace_root: &Path,
) {
    assert!(app.navigate_jump_count(true, 1).expect("Ctrl-O to root"));
    assert!(
        app.active
            .document
            .presentation_path()
            .is_some_and(|path| { same_path(path, source) })
    );
    assert!(app.lsp_start.is_none());
    assert_eq!(
        app.lsp
            .as_ref()
            .map(|lsp| (lsp.root.as_path(), lsp.open_documents.len())),
        Some((workspace_root, 2))
    );
    assert!(app.navigate_jump_count(false, 1).expect("Ctrl-I outside"));
    assert!(
        app.active
            .document
            .presentation_path()
            .is_some_and(|path| { same_path(path, second) })
    );
    assert!(app.lsp_start.is_none());
    assert_eq!(
        app.lsp
            .as_ref()
            .map(|lsp| (lsp.root.as_path(), lsp.open_documents.len())),
        Some((workspace_root, 2))
    );
}

#[cfg(unix)]
fn exercise_non_lsp_jump(app: &mut App, outside: &Path, second: &Path, workspace_root: &Path) {
    let notes = outside.join("notes.txt");
    fs::write(&notes, "not an LSP buffer\n").expect("notes");
    if let Some(lsp) = &mut app.lsp {
        lsp.semantic_due = Some(Instant::now());
    }
    app.navigate_to_entry(&QuickfixEntry {
        path: notes.clone(),
        line: 1,
        column: 1,
        selection_end: None,
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
            .is_some_and(|path| { same_path(path, second) })
    );
    assert!(app.lsp_start.is_none());
    assert!(app.lsp.as_ref().and_then(|lsp| lsp.semantic_due).is_some());
    assert_eq!(
        app.lsp
            .as_ref()
            .map(|lsp| (lsp.root.as_path(), lsp.open_documents.len())),
        Some((workspace_root, 2))
    );
}

#[cfg(unix)]
fn exercise_language_server_parking(
    app: &mut App,
    second: &Path,
    workspace_root: &Path,
    revision: DocumentRevision,
) {
    let python_workspace = tempfile::tempdir().expect("Python workspace");
    let python_source = python_workspace.path().join("tool.py");
    fs::write(&python_source, "def tool():\n  pass\n").expect("Python source");
    let (python_fake, _) = fake_lsp_server(python_workspace.path());
    let (python_client, python_uri, python_capabilities) = spawn_lsp_client(
        &python_fake,
        &python_source,
        workspace_root,
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
        root: workspace_root.to_path_buf(),
        open_documents: BTreeMap::new(),
        capabilities: python_capabilities,
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
    app.open_buffer(second).expect("return to Rust");
    assert_eq!(
        app.lsp.as_ref().map(|lsp| lsp.server.language_id.as_str()),
        Some("rust")
    );
    assert_eq!(app.parked_lsps.len(), 1);
    app.lsp_request_at_cursor("textDocument/hover", serde_json::json!({}))
        .expect("parked Rust client remains live");
}

#[cfg(unix)]
fn exercise_malformed_definition_recovery(app: &mut App) {
    let lsp = app.lsp.take().expect("reused LSP");
    let (sender, receiver) = mpsc::channel();
    sender
        .send(LspBackgroundResult {
            lsp,
            operation: LspBackgroundOperation::Location {
                method: "textDocument/definition".to_owned(),
            },
            outcome: Ok(LspBackgroundPayload::Response(
                serde_json::json!({"range": {"start": {"line": 0}}}),
            )),
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
fn automatic_lsp_failures_are_logged_without_error_popups() {
    let mut app = App::open(None, None).expect("app");
    app.apply_lsp_background_outcome(
        LspBackgroundOperation::Semantic {
            buffer_id: app.active.buffer_id,
            revision: app.active.editor.revision(),
        },
        Err("compile_commands.json is incomplete".to_owned()),
    );
    assert!(app.popup.is_none());
    assert!(app.message.contains("semanticTokens/full unavailable"));

    let (sender, receiver) = mpsc::channel();
    sender
        .send(Err(
            "clangd could not initialize this partial project".to_owned()
        ))
        .expect("startup result");
    app.lsp_start = Some(receiver);
    assert!(app.poll_lsp_start().expect("poll startup"));
    assert!(app.popup.is_none());
    assert!(app.message.contains("language server unavailable"));
}

#[test]
fn first_hover_queues_behind_workspace_startup_without_starting_another_server() {
    let mut app = App::open(None, None).expect("app");
    let (_sender, receiver) = mpsc::channel::<Result<PersistentLsp, String>>();
    app.lsp_start = Some(receiver);

    let dispatched = Instant::now();
    app.lsp_hover("textDocument/hover")
        .expect("queue first hover");

    assert!(dispatched.elapsed() < Duration::from_millis(10));
    assert_eq!(app.pending_lsp_hover.as_deref(), Some("textDocument/hover"));
    assert!(app.lsp_start.is_some());
    assert!(app.lsp_background.is_none());
    assert!(app.message.is_empty());
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
fn keyboard_visual_mode_is_painted_without_erasing_syntax_foreground() {
    let (document, mut opened) = LocalDocument::unnamed();
    opened.text = "fn main() {}\n".to_owned();
    let mut app = App::from_opened(document, opened, None, None).expect("app");
    let mut layout = ViewportLayout::new(30, 5);
    layout.configure_dotfile_profile();
    app.decorations.insert(
        app.active.buffer_id,
        BufferDecorations::new(
            app.active.editor.revision(),
            vec![DecorationSpan {
                range: 0..2,
                priority: u32::MAX,
                style: CellStyle {
                    foreground: Some(CellColor::Rgb(app.theme.blue)),
                    background: Some(CellColor::Rgb(app.theme.surface0)),
                    ..CellStyle::default()
                },
            }],
        ),
    );
    let normal = desired_frame(&mut layout, &app);
    let normal_foregrounds = normal.rows[0]
        .cells
        .iter()
        .filter(|cell| matches!(cell.grapheme.as_ref(), "f" | "n"))
        .take(2)
        .map(|cell| cell.style.foreground)
        .collect::<Vec<_>>();
    app.handle_editor_key(terminal_character('v'))
        .expect("Visual mode");
    app.handle_editor_key(terminal_character('l'))
        .expect("extend");
    let grid = desired_frame(&mut layout, &app);
    let selected = grid
        .rows
        .iter()
        .flat_map(|row| row.cells.iter())
        .filter(|cell| cell.style.background == Some(CellColor::Rgb(app.theme.surface2)))
        .collect::<Vec<_>>();
    assert_eq!(
        selected
            .iter()
            .map(|cell| cell.grapheme.as_ref())
            .collect::<String>(),
        "fn"
    );
    assert_eq!(
        selected
            .iter()
            .map(|cell| cell.style.foreground)
            .collect::<Vec<_>>(),
        normal_foregrounds,
        "Visual background must compose with the maximum-priority highlight foreground"
    );
}

#[test]
fn markdown_plain_words_do_not_gain_code_keyword_highlights_after_editing() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("notes.md");
    fs::write(&path, "").expect("Markdown file");
    let mut app = App::open(Some(&path), None).expect("app");

    app.handle_editor_key(terminal_character('i'))
        .expect("insert");
    for character in "use for".chars() {
        app.handle_editor_key(terminal_character(character))
            .expect("type Markdown");
    }
    app.handle_editor_key(TerminalKey {
        code: TerminalKeyCode::Escape,
        shift: false,
        control: false,
        alt: false,
        super_key: false,
    })
    .expect("normal mode");

    let spans = app
        .decorations
        .get(&app.active.buffer_id)
        .expect("syntax state")
        .spans_in(0..app.active.editor.text().len_bytes());
    assert!(
        spans.iter().all(|span| {
            span.style.foreground != Some(CellColor::Rgb(app.theme.mauve)) || !span.style.bold
        }),
        "plain Markdown was classified as a code keyword: {spans:?}"
    );
}

#[test]
fn entering_visual_mode_does_not_move_a_markdown_viewport() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("viewport.md");
    let text = (0..40)
        .map(|line| {
            format!(
                "paragraph {line}: this is a deliberately long Markdown line that wraps in a narrow terminal\n"
            )
        })
        .collect::<String>();
    fs::write(&path, &text).expect("Markdown file");
    let mut app = App::open(Some(&path), None).expect("app");
    let cursor = text.find("paragraph 25").expect("target paragraph");
    app.active.editor.set_cursor(cursor);
    let mut layout = ViewportLayout::new(42, 10);
    layout.configure_dotfile_profile();
    let normal = desired_frame(&mut layout, &app);
    let normal_rows = normal.rows[..normal.height - 1]
        .iter()
        .map(|row| {
            row.cells
                .iter()
                .map(|cell| cell.grapheme.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    app.handle_editor_key(terminal_character('v'))
        .expect("Visual mode");
    let visual = desired_frame(&mut layout, &app);
    let visual_rows = visual.rows[..visual.height - 1]
        .iter()
        .map(|row| {
            row.cells
                .iter()
                .map(|cell| cell.grapheme.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    assert_eq!(visual.cursor, normal.cursor);
    assert_eq!(visual_rows, normal_rows);
}

#[test]
fn inverse_edits_do_not_accumulate_decoration_coordinate_transforms() {
    let mut revision = DocumentRevision::new(1);
    let expected = DecorationSpan {
        range: 10..13,
        style: CellStyle::default(),
        priority: 1,
    };
    let mut decorations = BufferDecorations::new(revision, vec![expected.clone()]);

    for _ in 0..1_000 {
        let insert = Transaction::new(revision, vec![Edit::new(0..0, "x")]).expect("insert");
        revision = revision.next().expect("insert revision");
        decorations.map_through(&insert, revision);

        let delete = Transaction::new(revision, vec![Edit::new(0..1, "")]).expect("delete");
        revision = revision.next().expect("delete revision");
        decorations.map_through(&delete, revision);
    }

    assert!(decorations.transforms.is_empty());
    assert_eq!(decorations.spans_in(0..32), vec![expected]);
}

#[test]
fn visible_decoration_slices_are_shared_until_the_state_changes() {
    let revision = DocumentRevision::new(1);
    let mut decorations = BufferDecorations::new(
        revision,
        vec![DecorationSpan {
            range: 10..13,
            style: CellStyle::default(),
            priority: 1,
        }],
    );
    let first = decorations.spans_in_shared(0..32);
    let second = decorations.spans_in_shared(0..32);
    assert!(Arc::ptr_eq(&first, &second));

    let insert = Transaction::new(revision, vec![Edit::new(0..0, "x")]).expect("insert");
    decorations.map_through(&insert, revision.next().expect("next revision"));
    let changed = decorations.spans_in_shared(0..32);
    assert!(!Arc::ptr_eq(&first, &changed));
    assert_eq!(changed[0].range, 11..14);
}

#[test]
fn equivalent_recent_decoration_mapping_states_reuse_visible_slices() {
    let mut revision = DocumentRevision::new(1);
    let mut decorations = BufferDecorations::new(
        revision,
        vec![DecorationSpan {
            range: 10..13,
            style: CellStyle::default(),
            priority: 1,
        }],
    );

    let insert = Transaction::new(revision, vec![Edit::new(0..0, "x")]).expect("first insert");
    revision = revision.next().expect("insert revision");
    decorations.map_through(&insert, revision);
    let first_insert = decorations.spans_in_shared(0..33);
    let delete = Transaction::new(revision, vec![Edit::new(0..1, "")]).expect("first delete");
    revision = revision.next().expect("delete revision");
    decorations.map_through(&delete, revision);
    let first_delete = decorations.spans_in_shared(0..32);

    let insert = Transaction::new(revision, vec![Edit::new(0..0, "x")]).expect("second insert");
    revision = revision.next().expect("insert revision");
    decorations.map_through(&insert, revision);
    let second_insert = decorations.spans_in_shared(0..33);
    let delete = Transaction::new(revision, vec![Edit::new(0..1, "")]).expect("second delete");
    revision = revision.next().expect("delete revision");
    decorations.map_through(&delete, revision);
    let second_delete = decorations.spans_in_shared(0..32);

    assert!(Arc::ptr_eq(&first_insert, &second_insert));
    assert!(Arc::ptr_eq(&first_delete, &second_delete));
}

#[test]
fn prepared_replacement_state_is_reused_by_the_first_real_edit() {
    let revision = DocumentRevision::new(1);
    let mut decorations = BufferDecorations::new(
        revision,
        vec![DecorationSpan {
            range: 10..13,
            style: CellStyle::default(),
            priority: 1,
        }],
    );
    let insert = Transaction::new(revision, vec![Edit::new(0..0, "x")]).expect("insert");
    let changed = 0..2;
    let replacement = vec![DecorationSpan {
        range: 0..1,
        style: CellStyle::default(),
        priority: 1,
    }];

    decorations.prepare_replaced_visible(
        &insert,
        std::slice::from_ref(&changed),
        replacement.clone(),
        0..33,
    );
    let prepared = Arc::clone(
        &decorations
            .visible_cache
            .borrow()
            .last()
            .expect("prepared visible state")
            .spans,
    );
    decorations.replace_after_transaction(
        &insert,
        revision.next().expect("next revision"),
        &[changed],
        replacement,
    );
    let actual = decorations.spans_in_shared(0..33);

    assert!(Arc::ptr_eq(&prepared, &actual));
    assert_eq!(actual[0].range, 0..1);
    assert_eq!(actual[1].range, 11..14);
}

#[test]
fn first_document_end_insert_reuses_the_prepared_visible_syntax() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("prepared.rs");
    let text = (0..200)
        .map(|line| format!("pub fn item_{line}() {{ let value = {line}; }}\n"))
        .collect::<String>();
    fs::write(&path, text).expect("fixture");
    let mut app = App::open(Some(&path), None).expect("app");
    let semantic = app
        .decorations
        .get(&app.active.buffer_id)
        .expect("syntax decorations")
        .spans
        .iter()
        .step_by(6)
        .cloned()
        .collect();
    app.semantic_decorations.insert(
        app.active.buffer_id,
        BufferDecorations::new(app.active.editor.revision(), semantic),
    );
    app.resize_terminal(40, 120);
    let mut layout = ViewportLayout::new(120, 40);
    layout.configure_dotfile_profile();
    app.schedule_provider_refreshes(layout.height);
    let _ = desired_frame(&mut layout, &app);

    app.dispatch_key(KeyEvent::character('G'));
    app.schedule_provider_refreshes(layout.height);
    let _ = desired_frame(&mut layout, &app);
    for code in ['u', 'd'] {
        app.handle_editor_key(TerminalKey {
            code: TerminalKeyCode::Char(code),
            shift: false,
            control: true,
            alt: false,
            super_key: false,
        })
        .expect("half-page viewport motion");
        let _ = desired_frame(&mut layout, &app);
    }
    app.dispatch_key(KeyEvent::character('i'));
    app.dispatch_key(KeyEvent::character('x'));
    app.schedule_provider_refreshes(layout.height);

    let range = visible_byte_range(&app, app.active.buffer_id).expect("visible range");
    for state in [
        app.decorations
            .get(&app.active.buffer_id)
            .expect("syntax decorations"),
        app.semantic_decorations
            .get(&app.active.buffer_id)
            .expect("semantic decorations"),
    ] {
        let prepared = state
            .visible_cache
            .borrow()
            .iter()
            .find(|cached| {
                cached.range == range
                    && decoration_transforms_equal(&cached.transforms, &state.transforms)
                    && cached.overrides == state.overrides
                    && cached.invalidated == state.invalidated
            })
            .map(|cached| Arc::clone(&cached.spans))
            .expect("prepared document-end decoration state");
        let actual = state.spans_in_shared(range.clone());
        assert!(Arc::ptr_eq(&prepared, &actual));
    }
}

#[test]
fn click_drag_enters_visual_mode_and_selects_rendered_cells() {
    let (document, mut opened) = LocalDocument::unnamed();
    opened.text = "abcdef\n".to_owned();
    let mut app = App::from_opened(document, opened, None, None).expect("app");
    let mut layout = ViewportLayout::new(30, 5);
    layout.configure_dotfile_profile();
    let frames = [(app.active.buffer_id, app.active.editor.frame())];
    let coordinate_for = |wanted| {
        (0..5)
            .flat_map(|row| (0..30).map(move |column| (column, row)))
            .find(|(column, row)| {
                layout
                    .hit_test_workspace(&app.views, &frames, *column, *row, 1)
                    .is_some_and(|hit| hit.byte == wanted)
            })
            .expect("rendered byte coordinate")
    };
    let start = coordinate_for(1);
    let end = coordinate_for(4);
    app.handle_mouse_click(&layout, start.0, start.1)
        .expect("mouse down");
    app.handle_mouse_drag(&layout, end.0, end.1)
        .expect("mouse drag");
    app.handle_mouse_release(&layout, end.0, end.1)
        .expect("mouse release");
    assert_eq!(app.active.editor.mode(), Mode::Visual);
    assert_eq!(app.active.editor.selection_byte_range(), 1..5);

    let grid = desired_frame(&mut layout, &app);
    let selected = grid
        .rows
        .iter()
        .flat_map(|row| row.cells.iter())
        .filter(|cell| cell.style.background == Some(CellColor::Rgb(app.theme.surface2)))
        .map(|cell| cell.grapheme.as_ref())
        .collect::<String>();
    assert_eq!(selected, "bcde");
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

    let scheduled = Instant::now();
    app.start_grep_picker("needle").expect("grep picker");
    assert!(scheduled.elapsed() < Duration::from_millis(100));
    assert!(app.quickfix.is_empty());
    assert!(app.grep_pending.is_some());
    app.grep_due = Some(Instant::now());
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.quickfix.is_empty() && Instant::now() < deadline {
        app.poll_grep_picker();
        thread::sleep(Duration::from_millis(2));
    }
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
fn terminal_clipboard_paste_targets_unnamedplus_and_explicit_star() {
    let mut app = App::open(None, None).expect("unnamed editor");
    let paste = TerminalInput::Key(terminal_character('p'));
    assert_eq!(app.clipboard_register_for_paste(&paste), Some('+'));

    app.dispatch_key(KeyEvent::character('"'));
    app.dispatch_key(KeyEvent::character('*'));
    assert_eq!(app.clipboard_register_for_paste(&paste), Some('*'));
    app.restore_clipboard_register('*', "primary".to_owned());
    assert!(app.take_clipboard_writes().is_empty());
    app.dispatch_key(KeyEvent::character('p'));
    assert_eq!(app.active.editor.contents(), "primary");

    app.dispatch_key(KeyEvent::character('"'));
    app.dispatch_key(KeyEvent::character('a'));
    assert_eq!(app.clipboard_register_for_paste(&paste), None);
}

#[test]
fn search_prompt_consumes_p_before_clipboard_paste_preflight() {
    let (document, mut opened) = LocalDocument::unnamed();
    opened.text = "alpha apple\n".to_owned();
    let mut app = App::from_opened(document, opened, None, None).expect("app");
    app.handle_input(TerminalInput::Key(terminal_character('/')))
        .expect("open search prompt");
    app.handle_input(TerminalInput::Key(terminal_character('a')))
        .expect("type first search character");

    let paste_key = TerminalInput::Key(terminal_character('p'));
    assert_eq!(app.clipboard_register_for_paste(&paste_key), None);
    app.handle_input(paste_key)
        .expect("type p into search prompt");

    assert_eq!(
        app.prompt.as_ref().map(|prompt| prompt.buffer.as_str()),
        Some("ap")
    );
    assert_eq!(app.active.editor.contents(), "alpha apple\n");
}

#[test]
fn slash_search_is_incremental_highlighted_repeatable_and_cancelable() {
    let (document, mut opened) = LocalDocument::unnamed();
    opened.text = "zero hit one hit two hit\n".to_owned();
    let mut app = App::from_opened(document, opened, None, None).expect("app");

    assert_forward_search(&mut app);
    assert_backward_search(&mut app);
    assert_search_cancel_restores_origin(&mut app);
    app.execute_ex("nohlsearch").expect("clear highlights");
    assert!(!app.search_highlight);
    app.handle_editor_key(terminal_character('n'))
        .expect("search remains repeatable after nohlsearch");
    assert_eq!(app.active.editor.primary_cursor(), 5);
}

fn assert_forward_search(app: &mut App) {
    app.handle_editor_key(terminal_character('/'))
        .expect("open forward search");
    type_prompt(app, "h.t");
    assert_eq!(app.active.editor.primary_cursor(), 5, "incremental match");
    assert!(app.search_highlight);
    submit_prompt(app);
    assert_eq!(app.active.editor.primary_cursor(), 5);
    assert_search_highlight_rendered(app);

    app.handle_editor_key(terminal_character('n'))
        .expect("next match");
    assert_eq!(app.active.editor.primary_cursor(), 13);
    app.handle_editor_key(terminal_character('N'))
        .expect("previous match");
    assert_eq!(app.active.editor.primary_cursor(), 5);
}

fn assert_backward_search(app: &mut App) {
    app.active
        .editor
        .set_cursor(app.active.editor.text().len_bytes());
    app.handle_editor_key(terminal_character('?'))
        .expect("open backward search");
    type_prompt(app, "hit");
    submit_prompt(app);
    assert_eq!(app.active.editor.primary_cursor(), 21);
    app.handle_editor_key(terminal_character('n'))
        .expect("repeat backward search");
    assert_eq!(app.active.editor.primary_cursor(), 13);
}

fn assert_search_cancel_restores_origin(app: &mut App) {
    app.handle_editor_key(terminal_character('/'))
        .expect("start cancelable search");
    type_prompt(app, "zero");
    assert_eq!(app.active.editor.primary_cursor(), 0);
    app.handle_prompt_key(terminal_key(TerminalKeyCode::Escape))
        .expect("cancel search");
    assert_eq!(app.active.editor.primary_cursor(), 13);
    assert_eq!(
        app.active.editor.last_search(),
        Some(("hit", SearchDirection::Backward))
    );
}

fn assert_search_highlight_rendered(app: &App) {
    let mut layout = ViewportLayout::new(80, 10);
    layout.configure_dotfile_profile();
    let frame = desired_frame(&mut layout, app);
    assert!(frame.rows.iter().any(|row| row.cells.iter().any(|cell| {
        cell.grapheme.as_ref() == "h"
            && matches!(
                cell.style.background,
                Some(CellColor::Rgb(color))
                    if color == app.theme.yellow || color == app.theme.peach
            )
    })));
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

#[test]
fn substitute_case_confirm_and_print_flags_have_real_editor_behavior() {
    let (document, mut opened) = LocalDocument::unnamed();
    opened.text = "Foo foo FOO\nfoo foo\n".to_owned();
    let mut app = App::from_opened(document, opened, None, None).expect("app");

    app.execute_ex("%s/foo/x/g")
        .expect("default smart-case substitution");
    wait_for_task(&mut app);
    assert_eq!(app.active.editor.contents(), "x x x\nx x\n");
    app.active.editor.undo().expect("undo default case");

    app.execute_ex("%s/Foo/x/gI")
        .expect("forced case-sensitive substitution");
    wait_for_task(&mut app);
    assert_eq!(app.active.editor.contents(), "x foo FOO\nfoo foo\n");
    app.active.editor.undo().expect("undo sensitive case");

    app.execute_ex("%s/foo/X/gc").expect("begin confirmation");
    assert!(app.substitute_confirmation.is_some());
    app.handle_input(TerminalInput::Key(terminal_character('n')))
        .expect("skip first");
    app.handle_input(TerminalInput::Key(terminal_character('y')))
        .expect("accept second");
    app.handle_input(TerminalInput::Key(terminal_character('a')))
        .expect("accept remaining");
    assert!(app.substitute_confirmation.is_none());
    assert_eq!(app.active.editor.contents(), "Foo X X\nX X\n");
    app.active.editor.undo().expect("confirmation is one undo");
    assert_eq!(app.active.editor.contents(), "Foo foo FOO\nfoo foo\n");

    app.execute_ex("%s/foo/Z/gp").expect("printed substitution");
    wait_for_task(&mut app);
    assert!(app.message.contains("5 substitution(s)"));
    assert!(app.message.contains("Z Z"), "message was {}", app.message);
}

#[test]
fn vim_patterns_replacements_repeats_and_multiline_edits_are_transactional() {
    let (document, mut opened) = LocalDocument::unnamed();
    opened.text = "cat cat\nfoo\nbar\ncat\n".to_owned();
    let mut app = App::from_opened(document, opened, None, None).expect("app");

    app.execute_ex(r"%s/\<cat\>/\U&\E/g")
        .expect("Vim word and case atoms");
    wait_for_task(&mut app);
    assert_eq!(app.active.editor.contents(), "CAT CAT\nfoo\nbar\nCAT\n");

    app.execute_ex(r"%s/foo\nbar/joined/")
        .expect("multiline pattern");
    wait_for_task(&mut app);
    assert_eq!(app.active.editor.contents(), "CAT CAT\njoined\nCAT\n");
    app.execute_ex(r"%s/joined/one\rTWO/")
        .expect("newline replacement");
    wait_for_task(&mut app);
    assert_eq!(app.active.editor.contents(), "CAT CAT\none\nTWO\nCAT\n");

    let (document, mut opened) = LocalDocument::unnamed();
    opened.text = "cat\ncat\ncat\n".to_owned();
    let mut repeat = App::from_opened(document, opened, None, None).expect("repeat app");
    repeat.execute_ex("1s/cat/DOG/").expect("first substitute");
    wait_for_task(&mut repeat);
    repeat.execute_ex("2s").expect("bare substitute repeat");
    wait_for_task(&mut repeat);
    repeat.execute_ex("3&").expect("ampersand repeat");
    wait_for_task(&mut repeat);
    assert_eq!(repeat.active.editor.contents(), "DOG\nDOG\nDOG\n");

    repeat.active.editor.set_cursor(0);
    repeat
        .active
        .editor
        .search("DOG", SearchDirection::Forward)
        .expect("search DOG");
    repeat
        .synchronize_search("DOG", SearchDirection::Forward, true)
        .expect("share search");
    repeat.execute_ex("1~").expect("tilde repeat");
    wait_for_task(&mut repeat);
    assert_eq!(repeat.active.editor.contents(), "DOG\nDOG\nDOG\n");

    repeat.execute_ex("2s/DOG/cat/").expect("new replacement");
    wait_for_task(&mut repeat);
    repeat.execute_ex("3s/DOG/~/").expect("replacement tilde");
    wait_for_task(&mut repeat);
    assert_eq!(repeat.active.editor.contents(), "DOG\ncat\ncat\n");
}

#[test]
fn ex_addresses_global_and_inccommand_share_the_vim_regex_engine() {
    let (document, mut opened) = LocalDocument::unnamed();
    opened.text = "foo1\nfooX\nbar\n".to_owned();
    let mut app = App::from_opened(document, opened, None, None).expect("app");

    app.execute_ex(r"/foo./").expect("regex Ex address");
    assert_eq!(app.active.editor.cursor_line_column().0, 1);
    assert_eq!(
        app.active.editor.last_search(),
        Some(("foo.", SearchDirection::Forward))
    );
    app.execute_ex(r"g/foo\d/normal A!").expect("regex global");
    assert_eq!(app.active.editor.contents(), "foo1!\nfooX\nbar\n");

    app.prompt = Some(Prompt {
        kind: PromptKind::Command,
        buffer: r"%s/\(foo\)\d/\U\1".to_owned(),
        history_index: None,
    });
    app.update_inccommand_preview();
    assert!(app.message.contains("1 substitution(s)"));
    assert!(app.message.contains("FOO!"), "preview was {}", app.message);
}

#[test]
fn search_direction_is_shared_across_buffers_and_star_uses_word_boundaries() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let first = directory.path().join("first.txt");
    let second = directory.path().join("second.txt");
    fs::write(&first, "hit one hit\n").expect("first");
    fs::write(&second, "hit one hit\n").expect("second");
    let mut app = App::open(Some(&first), None).expect("app");
    app.active
        .editor
        .set_cursor(app.active.editor.text().len_bytes());
    app.execute_prompt(Prompt {
        kind: PromptKind::SearchBackward,
        buffer: "hit".to_owned(),
        history_index: None,
    })
    .expect("backward search");
    assert!(app.client_state.search_backward);
    app.open_buffer(&second).expect("open second");
    assert_eq!(
        app.active.editor.last_search(),
        Some(("hit", SearchDirection::Backward))
    );
    app.handle_editor_key(terminal_character('n'))
        .expect("repeat shared backward search");
    assert_eq!(app.active.editor.primary_cursor(), 8);

    let (document, mut opened) = LocalDocument::unnamed();
    opened.text = "cat catalog cat\n".to_owned();
    let mut words = App::from_opened(document, opened, None, None).expect("word app");
    words.search_word_under_cursor(false, 1);
    assert_eq!(words.active.editor.primary_cursor(), 12);
    assert_eq!(
        words.active.editor.last_search(),
        Some((r"\<cat\>", SearchDirection::Forward))
    );
    assert!(words.search_highlight);
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

#[cfg(unix)]
#[test]
fn terminal_grid_preserves_ansi_diff_colors_without_editor_gutters() {
    let mut app = App::open(None, None).expect("app");
    app.resize_terminal(12, 60);
    app.open_terminal(
        Some("sh"),
        &["-c".into(), "printf '\\033[1;31;44mREMOVED\\033[0m'".into()],
    )
    .expect("styled terminal");
    let deadline = Instant::now() + Duration::from_secs(2);
    while app
        .terminal
        .as_ref()
        .is_some_and(|terminal| terminal.exit_code().is_none())
        && Instant::now() < deadline
    {
        app.terminal
            .as_mut()
            .expect("terminal")
            .poll()
            .expect("poll terminal");
        thread::yield_now();
    }
    let terminal = app.terminal.as_ref().expect("terminal session");
    assert_eq!(terminal.surface().size(), (11, 60));

    let mut layout = ViewportLayout::new(60, 12);
    layout.configure_dotfile_profile();
    let grid = desired_frame(&mut layout, &app);
    let first = &grid.rows[0].cells[0];
    assert_eq!(first.grapheme.as_ref(), "R");
    assert_eq!(first.style.foreground, Some(CellColor::Palette(1)));
    assert_eq!(first.style.background, Some(CellColor::Palette(4)));
    assert!(first.style.bold);
    assert_ne!(first.grapheme.as_ref(), "1");

    assert_eq!(
        terminal_mouse_bytes(&TerminalInput::MouseClick { column: 2, row: 3 }),
        b"\x1b[<0;3;4M"
    );
    assert_eq!(
        terminal_mouse_bytes(&TerminalInput::MouseScroll {
            lines: -6,
            column: 2,
            row: 3,
        }),
        b"\x1b[<64;3;4M\x1b[<64;3;4M"
    );
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
    assert_bundled_language_profiles();
    assert_special_language_server_settings();
    assert_language_server_profiles();
}

fn assert_bundled_language_profiles() {
    let bundled = [
        ("script.sh", "bash"),
        ("source.c", "c"),
        ("source.cpp", "cpp"),
        ("source.cs", "csharp"),
        ("style.css", "css"),
        ("main.dart", "dart"),
        ("demo.exs", "elixir"),
        ("main.go", "go"),
        ("Main.hs", "haskell"),
        ("main.hcl", "hcl"),
        ("main.tf", "hcl"),
        ("index.html", "html"),
        ("Main.java", "java"),
        ("main.jsx", "javascript"),
        ("main.json", "json"),
        ("Main.kt", "kotlin"),
        ("main.lua", "lua"),
        ("README.md", "markdown"),
        ("flake.nix", "nix"),
        ("index.php", "php"),
        ("main.py", "python"),
        ("main.rb", "ruby"),
        ("main.rs", "rust"),
        ("Main.scala", "scala"),
        ("main.sol", "solidity"),
        ("main.swift", "swift"),
        ("main.tsx", "tsx"),
        ("main.ts", "typescript"),
        ("main.yaml", "yaml"),
    ];
    for (path, expected) in bundled {
        assert_eq!(
            language_bundle(Some(Path::new(path))).language_id.as_ref(),
            expected,
            "wrong bundled grammar for {path}"
        );
    }
}

fn assert_special_language_server_settings() {
    let c = language_server_invocation(Some(Path::new("main.c"))).expect("C LSP");
    assert_eq!(
        (c.program.as_str(), c.language_id.as_str()),
        ("clangd", "c")
    );
    let rust = language_server_invocation(Some(Path::new("main.rs"))).expect("Rust LSP");
    assert_eq!(rust.settings["rust-analyzer"]["check"]["command"], "clippy");
    let typescript =
        language_server_invocation(Some(Path::new("main.tsx"))).expect("TypeScript LSP");
    assert_eq!(typescript.initialization_options["jsx"]["enabled"], true);
    assert_eq!(
        typescript.settings["typescript"]["suggestionActions"]["enabled"],
        true
    );
    let haskell = language_server_invocation(Some(Path::new("Main.hs"))).expect("Haskell LSP");
    assert_eq!(
        haskell.settings["haskell"]["plugin"]["fourmolu"]["config"]["external"],
        true
    );
    let nix = language_server_invocation(Some(Path::new("flake.nix"))).expect("Nix LSP");
    assert_eq!(nix.program, "nixd");
    assert!(nix.settings["nixd"]["nixpkgs"]["expr"].is_string());
    assert_eq!(
        nixd_nixpkgs_expression(None, "nixpkgs"),
        "import <nixpkgs> { }"
    );
}

fn assert_language_server_profiles() {
    let lsp_profiles = [
        ("main.rs", "rust-analyzer", "rust"),
        ("main.js", "pnpm", "javascript"),
        ("main.jsx", "pnpm", "javascriptreact"),
        ("main.ts", "pnpm", "typescript"),
        ("main.tsx", "pnpm", "typescriptreact"),
        ("main.py", "basedpyright-langserver", "python"),
        ("main.go", "gopls", "go"),
        ("main.tf", "terraform-ls", "terraform"),
        ("flake.nix", "nixd", "nix"),
        ("Main.hs", "haskell-language-server-wrapper", "haskell"),
        ("main.lua", "lua-language-server", "lua"),
        ("main.sh", "bash-language-server", "shellscript"),
        ("main.c", "clangd", "c"),
        ("main.cpp", "clangd", "cpp"),
    ];
    for (path, program, language_id) in lsp_profiles {
        let invocation = language_server_invocation(Some(Path::new(path)))
            .unwrap_or_else(|| panic!("missing LSP profile for {path}"));
        assert_eq!(invocation.program, program, "wrong LSP program for {path}");
        assert_eq!(
            invocation.language_id, language_id,
            "wrong LSP language ID for {path}"
        );
    }
}

#[test]
fn nix_file_has_tree_sitter_decorations_before_its_first_frame() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("flake.nix");
    fs::write(
        &path,
        "{ lib, ... }: let greeting = \"hello\"; in { enabled = lib.mkDefault true; } # note\n",
    )
    .expect("Nix source");
    let app = App::open(Some(&path), None).expect("open Nix file");
    let decorations = app
        .decorations
        .get(&app.active.buffer_id)
        .expect("first-frame syntax decorations");
    assert_eq!(decorations.revision, app.active.editor.revision());
    let text = app.active.editor.contents();
    for needle in ["\"hello\"", "true", "# note"] {
        let start = text.find(needle).expect("Nix token");
        assert!(
            decorations
                .spans
                .iter()
                .any(|span| span.range == (start..start + needle.len())),
            "missing first-frame Nix decoration for {needle:?}"
        );
    }
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
fn bare_git_ex_opens_lazygit_and_subcommands_stay_direct() {
    assert_eq!(git_ex_program(&[]), "lazygit");
    assert_eq!(git_ex_program(&["status".into()]), "git");

    let directory = tempfile::tempdir().expect("temporary Git repository");
    assert!(
        Command::new("git")
            .current_dir(directory.path())
            .arg("init")
            .status()
            .expect("git init")
            .success()
    );
    assert_eq!(
        git_root_for(directory.path()).expect("Git root from directory"),
        directory.path().canonicalize().expect("canonical Git root")
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
    assert_eq!(
        locations[0].selection_end,
        Some(QuickfixPosition { line: 3, column: 9 })
    );
    assert!(locations[0].column_utf16);

    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("unicode.rs");
    fs::write(&path, "😀target\n").expect("write source");
    let app = App::open(Some(&path), None).expect("open app");
    let entry = QuickfixEntry {
        path,
        line: 1,
        column: 3,
        selection_end: Some(QuickfixPosition { line: 1, column: 9 }),
        column_utf16: true,
        text: String::new(),
    };
    assert_eq!(app.entry_cursor_byte(&entry), "😀".len());
    assert_eq!(entry.selection_byte_range("😀target\n"), Some(4..10));
}

#[test]
fn lsp_navigation_capabilities_follow_initialize_provider_advertisements() {
    let capabilities = lsp_capabilities(&serde_json::json!({
        "capabilities": {
            "declarationProvider": false,
            "definitionProvider": true,
            "implementationProvider": {"workDoneProgress": true},
            "referencesProvider": null,
            "semanticTokensProvider": {
                "legend": {"tokenTypes": ["function"], "tokenModifiers": []}
            }
        }
    }));

    assert_eq!(
        capabilities.navigation,
        LspNavigationCapabilities {
            declaration: false,
            definition: true,
            implementation: true,
            references: false,
        }
    );
    assert!(capabilities.semantic_legend.is_some());
}

#[test]
fn reference_picker_preview_decorates_only_the_exact_utf16_range() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("references.rs");
    let second_path = directory.path().join("second.rs");
    let source = "// 😀 target and other\n";
    let second_source = "fn other_reference() {}\n";
    fs::write(&path, source).expect("write source");
    fs::write(&second_path, second_source).expect("write second source");
    let mut app = App::open(Some(&path), None).expect("open app");
    app.quickfix = vec![
        QuickfixEntry {
            path,
            line: 1,
            column: 7,
            selection_end: Some(QuickfixPosition {
                line: 1,
                column: 13,
            }),
            column_utf16: true,
            text: "language-server location".to_owned(),
        },
        QuickfixEntry {
            path: second_path,
            line: 1,
            column: 4,
            selection_end: Some(QuickfixPosition {
                line: 1,
                column: 19,
            }),
            column_utf16: true,
            text: "language-server location".to_owned(),
        },
    ];

    app.start_location_picker(PickerSource::Jumps, "")
        .expect("open reference picker");

    assert_eq!(app.picker_preview, source);
    assert_eq!(app.picker_preview_highlight_line, None);
    assert!(app.picker_preview_decorations.iter().any(|decoration| {
        decoration.range == (8..14)
            && decoration.style.foreground.is_none()
            && decoration.style.background == Some(CellColor::Rgb(app.theme.surface0))
            && decoration.priority == u32::MAX
    }));

    app.move_picker(1);

    assert_eq!(app.picker_preview, second_source);
    assert_eq!(app.picker_preview_highlight_line, None);
    assert!(app.picker_preview_decorations.iter().any(|decoration| {
        decoration.range == (3..18)
            && decoration.style.background == Some(CellColor::Rgb(app.theme.surface0))
    }));
    assert!(
        !app.picker_preview_decorations
            .iter()
            .any(|decoration| decoration.range == (8..14) && decoration.priority == u32::MAX)
    );
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
                priority: u32::MAX,
            },
            HighlightSpan {
                range: call..call + 4,
                kind: "method".into(),
                priority: u32::MAX,
            },
        ]
    );

    let (document, mut opened) = LocalDocument::unnamed();
    opened.text = text.to_owned();
    let mut app = App::from_opened(document, opened, None, None).expect("app");
    let buffer_id = app.active.buffer_id;
    let revision = app.active.editor.revision();
    app.decorations.insert(
        buffer_id,
        BufferDecorations::new(
            revision,
            vec![provider_decoration(
                HighlightSpan {
                    range: value..value + 5,
                    kind: "function".into(),
                    priority: 1_000_000,
                },
                app.theme,
            )],
        ),
    );
    let mut layout = ViewportLayout::new(80, 8);
    layout.configure_dotfile_profile();
    let tree_sitter_frame = desired_frame(&mut layout, &app);
    let tree_sitter_value = tree_sitter_frame
        .rows
        .iter()
        .flat_map(|row| &row.cells)
        .find(|cell| cell.grapheme.as_ref() == "v")
        .expect("Tree-sitter value cell");
    assert_eq!(
        tree_sitter_value.style.foreground,
        Some(CellColor::Rgb(app.theme.blue))
    );

    app.semantic_decorations.insert(
        buffer_id,
        BufferDecorations::new(
            revision,
            spans
                .into_iter()
                .map(|span| provider_decoration(span, app.theme))
                .collect(),
        ),
    );
    let semantic_frame = desired_frame(&mut layout, &app);
    let semantic_value = semantic_frame
        .rows
        .iter()
        .flat_map(|row| &row.cells)
        .find(|cell| cell.grapheme.as_ref() == "v")
        .expect("semantic value cell");
    assert_eq!(
        semantic_value.style.foreground,
        Some(CellColor::Rgb(app.theme.peach)),
        "readonly semantic token must override the overlapping Tree-sitter function capture"
    );
}

#[test]
fn full_buffer_semantic_token_decoding_is_linear_and_bounded() {
    const LINES: usize = 14_000;
    let text = "let value = 1;\n".repeat(LINES);
    let mut data = Vec::with_capacity(LINES * 5);
    for line in 0..LINES {
        data.extend([
            serde_json::json!(usize::from(line != 0)),
            serde_json::json!(4),
            serde_json::json!(5),
            serde_json::json!(0),
            serde_json::json!(0),
        ]);
    }
    let response = serde_json::json!({"data": data});
    let legend = SemanticTokenLegend {
        token_types: vec!["variable".to_owned()],
        token_modifiers: Vec::new(),
    };

    let started = Instant::now();
    let spans = parse_semantic_tokens(&text, &response, &legend);
    let elapsed = started.elapsed();

    assert_eq!(spans.len(), LINES);
    assert!(
        elapsed < Duration::from_millis(500),
        "full-buffer semantic decoding regressed to {elapsed:?}; line lookup must remain indexed"
    );
}

#[test]
fn rust_semantic_tokens_preserve_transparent_tree_sitter_colors() {
    let text = "self.registers.call Register\n";
    let legend = SemanticTokenLegend {
        token_types: vec![
            "selfKeyword".to_owned(),
            "property".to_owned(),
            "generic".to_owned(),
            "enumMember".to_owned(),
        ],
        token_modifiers: Vec::new(),
    };
    let semantic = parse_semantic_tokens(
        text,
        &serde_json::json!({
            "data": [
                0, 0, 4, 0, 0,
                0, 5, 9, 1, 0,
                0, 10, 4, 2, 0,
                0, 5, 8, 3, 0
            ]
        }),
        &legend,
    );
    assert_eq!(
        semantic,
        vec![
            HighlightSpan {
                range: 5..14,
                kind: "property".into(),
                priority: u32::MAX,
            },
            HighlightSpan {
                range: 20..28,
                kind: "semantic.enum-member".into(),
                priority: u32::MAX,
            },
        ],
        "semantic groups without an effective Neovim color must leave Tree-sitter visible"
    );

    let (document, mut opened) = LocalDocument::unnamed();
    opened.text = text.to_owned();
    let mut app = App::from_opened(document, opened, None, None).expect("app");
    let buffer_id = app.active.buffer_id;
    let revision = app.active.editor.revision();
    app.decorations.insert(
        buffer_id,
        BufferDecorations::new(
            revision,
            [
                (0..4, "variable.builtin"),
                (5..14, "variable.member"),
                (15..19, "function.call"),
                (20..28, "constant"),
            ]
            .into_iter()
            .map(|(range, kind)| {
                provider_decoration(
                    HighlightSpan {
                        range,
                        kind: kind.into(),
                        priority: 1_000_000,
                    },
                    app.theme,
                )
            })
            .collect(),
        ),
    );
    app.semantic_decorations.insert(
        buffer_id,
        BufferDecorations::new(
            revision,
            semantic
                .into_iter()
                .map(|span| provider_decoration(span, app.theme))
                .collect(),
        ),
    );

    let mut layout = ViewportLayout::new(60, 5);
    layout.configure_dotfile_profile();
    let grid = desired_frame(&mut layout, &app);
    let color_of = |wanted: &str| {
        grid.rows[0]
            .cells
            .iter()
            .find(|cell| cell.grapheme.as_ref() == wanted)
            .and_then(|cell| cell.style.foreground)
            .expect("colored source cell")
    };
    assert_eq!(color_of("s"), CellColor::Rgb(app.theme.red));
    assert_eq!(color_of("r"), CellColor::Rgb(app.theme.lavender));
    assert_eq!(color_of("c"), CellColor::Rgb(app.theme.blue));
    assert_eq!(color_of("R"), CellColor::Rgb(app.theme.teal));
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
        selection_end: None,
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
    let keys: Vec<KeyEvent> = serde_json::from_slice(&recording.raw_keys).expect("raw macro keys");
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
