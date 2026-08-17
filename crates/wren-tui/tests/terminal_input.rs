use std::io::Write as _;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use tempfile::{NamedTempFile, TempDir};
use wren_workflow::PtySession;

fn editor_session(contents: &str) -> (TempDir, NamedTempFile, PtySession) {
    let home = tempfile::tempdir().expect("isolated Wren home");
    let mut file = NamedTempFile::new().expect("temporary editor document");
    file.write_all(contents.as_bytes())
        .expect("write editor document");
    file.flush().expect("flush editor document");
    let path = file.path().to_string_lossy().into_owned();
    let home_variable = format!("HOME={}", home.path().display());
    let state_variable = format!("XDG_STATE_HOME={}/state", home.path().display());
    let data_variable = format!("XDG_DATA_HOME={}/data", home.path().display());
    let config_variable = format!("XDG_CONFIG_HOME={}/config", home.path().display());
    let session = PtySession::spawn(
        "env",
        &[
            home_variable.as_str(),
            state_variable.as_str(),
            data_variable.as_str(),
            config_variable.as_str(),
            env!("CARGO_BIN_EXE_wren"),
            path.as_str(),
        ],
        24,
        80,
    )
    .expect("spawn Wren in a real PTY");
    (home, file, session)
}

fn spawn_editor(home: &Path, path: &Path, rows: u16, columns: u16) -> PtySession {
    let path = path.to_string_lossy().into_owned();
    let home_variable = format!("HOME={}", home.display());
    let state_variable = format!("XDG_STATE_HOME={}/state", home.display());
    let data_variable = format!("XDG_DATA_HOME={}/data", home.display());
    let config_variable = format!("XDG_CONFIG_HOME={}/config", home.display());
    PtySession::spawn(
        "env",
        &[
            home_variable.as_str(),
            state_variable.as_str(),
            data_variable.as_str(),
            config_variable.as_str(),
            env!("CARGO_BIN_EXE_wren"),
            path.as_str(),
        ],
        rows,
        columns,
    )
    .expect("spawn Wren in a real PTY")
}

fn wait_for_screen(session: &mut PtySession, description: &str, predicate: impl Fn(&str) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        session.poll().expect("poll Wren PTY");
        let screen = session.surface().contents();
        if predicate(&screen) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}; screen was:\n{screen}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn quit_without_saving(session: &mut PtySession) {
    session.send_input(b"\x1b[27u:q!\r").expect("quit Wren");
    let deadline = Instant::now() + Duration::from_secs(5);
    while session.exit_code().is_none() && Instant::now() < deadline {
        session.poll().expect("poll Wren exit");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(session.exit_code(), Some(0));
}

#[cfg(unix)]
#[test]
fn workspace_lsp_starts_on_open_even_while_input_remains_busy() {
    let home = tempfile::tempdir().expect("isolated Wren home");
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).expect("create fake executable directory");
    let server = bin.join("rust-analyzer");
    std::fs::write(
        &server,
        r#"#!/usr/bin/env python3
import json
import os
import sys

log_path = os.environ["WREN_TEST_LSP_LOG"]
while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            raise SystemExit(0)
        if line == b"\r\n":
            break
        name, value = line.decode().split(":", 1)
        headers[name.lower()] = value.strip()
    message = json.loads(sys.stdin.buffer.read(int(headers["content-length"])))
    method = message.get("method", "")
    with open(log_path, "a", encoding="utf-8") as output:
        root = " " + message.get("params", {}).get("rootUri", "") if method == "initialize" else ""
        output.write(method + root + "\n")
        output.flush()
    if "id" not in message:
        continue
    result = {"capabilities": {"hoverProvider": True}} if method == "initialize" else None
    response = json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(response)}\r\n\r\n".encode() + response)
    sys.stdout.buffer.flush()
"#,
    )
    .expect("write fake rust-analyzer");
    let mut permissions = std::fs::metadata(&server)
        .expect("fake server metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&server, permissions).expect("make fake server executable");

    let source = home.path().join("main.rs");
    std::fs::write(&source, "fn main() {}\n").expect("write Rust source");
    let log = home.path().join("lsp.log");
    let path_variable = format!(
        "PATH={}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let log_variable = format!("WREN_TEST_LSP_LOG={}", log.display());
    let home_variable = format!("HOME={}", home.path().display());
    let state_variable = format!("XDG_STATE_HOME={}/state", home.path().display());
    let data_variable = format!("XDG_DATA_HOME={}/data", home.path().display());
    let config_variable = format!("XDG_CONFIG_HOME={}/config", home.path().display());
    let source_argument = source.to_string_lossy().into_owned();
    let mut session = PtySession::spawn(
        "env",
        &[
            home_variable.as_str(),
            state_variable.as_str(),
            data_variable.as_str(),
            config_variable.as_str(),
            path_variable.as_str(),
            log_variable.as_str(),
            env!("CARGO_BIN_EXE_wren"),
            source_argument.as_str(),
        ],
        24,
        80,
    )
    .expect("spawn Wren with fake workspace LSP");

    // Keep the input loop active beyond the former 750 ms idle debounce. A
    // workspace server must initialize anyway, before any hover is requested.
    let deadline = Instant::now() + Duration::from_secs(3);
    let startup_log = loop {
        session.send_input(b"hl").expect("send continuous input");
        session.poll().expect("poll Wren PTY");
        let current = std::fs::read_to_string(&log).unwrap_or_default();
        if current.contains("textDocument/didOpen") {
            break current;
        }
        assert!(
            Instant::now() < deadline,
            "workspace LSP did not start while input remained active; log: {current}"
        );
        thread::sleep(Duration::from_millis(10));
    };

    assert!(startup_log.contains("initialize file://"));
    let workspace = std::fs::canonicalize(std::env::current_dir().expect("current workspace"))
        .expect("canonical workspace");
    assert!(
        startup_log.contains(workspace.to_string_lossy().as_ref()),
        "LSP was not rooted at the launch workspace: {startup_log}"
    );
    assert!(!startup_log.contains("textDocument/hover"));
    wait_for_screen(
        &mut session,
        "editor readiness after workspace LSP startup",
        |screen| screen.contains("NORMAL") && screen.contains("fn main"),
    );
    quit_without_saving(&mut session);
}

#[test]
fn real_terminal_keyboard_and_mouse_enter_visual_mode() {
    let (_home, _file, mut session) = editor_session("abcdef\nsecond line\n");
    wait_for_screen(&mut session, "initial Normal mode", |screen| {
        screen.contains("NORMAL") && screen.contains("abcdef")
    });

    session
        .send_input(b"vl")
        .expect("enter keyboard Visual mode");
    wait_for_screen(&mut session, "keyboard Visual mode", |screen| {
        screen.contains("VISUAL")
    });
    session
        .send_input(b"d")
        .expect("delete the keyboard selection");
    wait_for_screen(
        &mut session,
        "keyboard Visual selection deletion",
        |screen| screen.contains("NORMAL") && screen.contains("cdef") && !screen.contains("abcdef"),
    );
    session.send_input(b"u").expect("restore keyboard deletion");
    wait_for_screen(&mut session, "restored document", |screen| {
        screen.contains("abcdef")
    });

    // SGR mouse coordinates are one-based. Columns 5 through 8 are rendered
    // document cells on the first row after Wren's three-column number gutter.
    session
        .send_input(b"\x1b[<0;5;1M\x1b[<32;8;1M\x1b[<0;8;1m")
        .expect("drag across editor cells");
    wait_for_screen(&mut session, "mouse Visual mode", |screen| {
        screen.contains("VISUAL")
    });
    session
        .send_input(b"d")
        .expect("delete the mouse selection");
    wait_for_screen(&mut session, "mouse Visual selection deletion", |screen| {
        screen.contains("NORMAL") && screen.contains("af") && !screen.contains("abcdef")
    });

    quit_without_saving(&mut session);
}

#[test]
fn real_terminal_unnamedplus_paste_reads_osc52() {
    let (_home, _file, mut session) = editor_session("alpha\n");
    wait_for_screen(&mut session, "initial document", |screen| {
        screen.contains("NORMAL") && screen.contains("alpha")
    });

    session.send_input(b"p").expect("request unnamedplus paste");
    // The application is now waiting on its bounded OSC 52 query. Reply as a
    // terminal would, using the clipboard selection and "clip" in base64.
    thread::sleep(Duration::from_millis(20));
    session
        .send_input(b"\x1b]52;c;Y2xpcA==\x1b\\")
        .expect("reply to clipboard query");
    wait_for_screen(&mut session, "OSC 52 clipboard paste", |screen| {
        screen.contains("acliplpha")
    });

    quit_without_saving(&mut session);
}

#[test]
fn real_terminal_large_rust_file_opens_navigates_and_edits_responsively() {
    let home = tempfile::tempdir().expect("isolated Wren home");
    let source = home.path().join("large.rs");
    let text = (0..14_000)
        .map(|line| {
            format!(
                "pub fn item_{line:05}() -> usize {{ let value_{line:05}: usize = {line}; value_{line:05} }}\n"
            )
        })
        .collect::<String>();
    std::fs::write(&source, text).expect("write large Rust source");

    let open_at = Instant::now();
    let mut session = spawn_editor(home.path(), &source, 40, 120);
    wait_for_screen(&mut session, "large Rust first frame", |screen| {
        screen.contains("NORMAL") && screen.contains("item_00000")
    });
    let open_elapsed = open_at.elapsed();
    assert!(
        open_elapsed < Duration::from_secs(2),
        "large Rust executable open took {open_elapsed:?}"
    );

    let navigation_at = Instant::now();
    session.send_input(b"G").expect("navigate to file end");
    wait_for_screen(&mut session, "large Rust final line", |screen| {
        screen.contains("item_13999")
    });
    let navigation_elapsed = navigation_at.elapsed();
    assert!(
        navigation_elapsed < Duration::from_millis(250),
        "large Rust executable navigation took {navigation_elapsed:?}"
    );

    let edit_at = Instant::now();
    session
        .send_input(b"iX\x1b")
        .expect("edit the large Rust final line");
    wait_for_screen(&mut session, "large Rust edited final line", |screen| {
        screen.contains("[+]") && screen.lines().any(|line| line.contains("140X"))
    });
    let edit_elapsed = edit_at.elapsed();
    assert!(
        edit_elapsed < Duration::from_millis(250),
        "large Rust executable edit took {edit_elapsed:?}"
    );

    quit_without_saving(&mut session);
}
