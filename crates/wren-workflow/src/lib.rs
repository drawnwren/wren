#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use imara_diff::{Algorithm, Diff, InternedInput};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use wren_types::{
    Bias, DocumentId, DocumentMutation, DocumentRevision, Edit, ExpectedTarget, FileIdentity,
    LeaseEpoch, ResourceOp, SemanticGroupId, SemanticGroupKind, Transaction, WorkspaceTransaction,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentVisibility {
    Persisted,
    RemoteAcked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavePolicy {
    Never,
    Prompt,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    pub program: PathBuf,
    pub arguments: Vec<Box<str>>,
    pub environment: BTreeMap<Box<str>, Box<str>>,
    pub visibility: DocumentVisibility,
    pub save: SavePolicy,
    pub max_output_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskOutput {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
    pub cancelled: bool,
    pub elapsed: Duration,
}

#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("workflow I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("PTY operation failed: {0}")]
    Pty(String),
    #[error("DAP frame is malformed: {0}")]
    Dap(Box<str>),
    #[error("JSON protocol failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("provider result targets revision {actual:?}, expected {expected:?}")]
    StaleRevision {
        expected: DocumentRevision,
        actual: DocumentRevision,
    },
    #[error("structural or AI edit could not be mapped through intervening changes")]
    Unmappable,
    #[error("task program is not trusted or executable")]
    UntrustedTask,
    #[error("invalid semantic transaction: {0}")]
    Transaction(Box<str>),
    #[error("structural search failed: {0}")]
    Structural(Box<str>),
    #[error("workflow process exited unsuccessfully: {0}")]
    ProcessFailed(Box<str>),
    #[error("workflow process was cancelled")]
    Cancelled,
    #[error("LSP position {line}:{character} is outside the document")]
    InvalidLspPosition { line: u32, character: u32 },
}

pub struct TaskSupervisor {
    trusted: bool,
}

impl TaskSupervisor {
    #[must_use]
    pub const fn new(trusted: bool) -> Self {
        Self { trusted }
    }

    pub fn run(&self, spec: &TaskSpec) -> Result<TaskOutput, WorkflowError> {
        self.run_until_cancelled(spec, || false)
    }

    pub fn run_until_cancelled(
        &self,
        spec: &TaskSpec,
        is_cancelled: impl Fn() -> bool,
    ) -> Result<TaskOutput, WorkflowError> {
        if !self.trusted {
            return Err(WorkflowError::UntrustedTask);
        }
        let started = Instant::now();
        let mut command = Command::new(&spec.program);
        command
            .args(spec.arguments.iter().map(AsRef::<str>::as_ref))
            .env_clear()
            .envs(
                spec.environment
                    .iter()
                    .map(|(name, value)| (name.as_ref(), value.as_ref())),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn()?;
        let limit = spec.max_output_bytes.max(1);
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("task stdout pipe missing"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("task stderr pipe missing"))?;
        let stdout_reader = thread::spawn(move || read_bounded(stdout, limit));
        let stderr_reader = thread::spawn(move || read_bounded(stderr, limit));
        let mut cancelled = false;
        let mut terminated_at = None;
        let status = loop {
            if is_cancelled() && !cancelled {
                cancelled = true;
                signal_process_group(&mut child, false)?;
                terminated_at = Some(Instant::now());
            }
            if terminated_at.is_some_and(|at| at.elapsed() >= Duration::from_millis(250)) {
                signal_process_group(&mut child, true)?;
                terminated_at = None;
            }
            if let Some(status) = child.try_wait()? {
                break status;
            }
            thread::sleep(Duration::from_millis(1));
        };
        let (stdout, stdout_truncated) = stdout_reader
            .join()
            .map_err(|_| io::Error::other("task stdout reader panicked"))??;
        let (stderr, stderr_truncated) = stderr_reader
            .join()
            .map_err(|_| io::Error::other("task stderr reader panicked"))??;
        Ok(TaskOutput {
            status: status.code(),
            stdout,
            stderr,
            truncated: stdout_truncated || stderr_truncated,
            cancelled,
            elapsed: started.elapsed(),
        })
    }
}

fn read_bounded(mut reader: impl Read, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(kept.len());
        let take = read.min(remaining);
        kept.extend_from_slice(&buffer[..take]);
        truncated |= take < read;
    }
    Ok((kept, truncated))
}

fn signal_process_group(child: &mut std::process::Child, force: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        let process_group = i32::try_from(child.id())
            .map(Pid::from_raw)
            .map_err(|_| io::Error::other("task process ID exceeds platform range"))?;
        let signal = if force {
            Signal::SIGKILL
        } else {
            Signal::SIGTERM
        };
        match killpg(process_group, signal) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(error) => Err(io::Error::other(error)),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = force;
        child.kill()
    }
}

pub struct TerminalSurface {
    parser: vt100::Parser,
}

impl TerminalSurface {
    #[must_use]
    pub fn new(rows: u16, columns: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, columns, 0),
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub fn resize(&mut self, rows: u16, columns: u16) {
        self.parser.screen_mut().set_size(rows, columns);
    }

    #[must_use]
    pub fn contents(&self) -> String {
        self.parser.screen().contents()
    }

    #[must_use]
    pub fn cursor_position(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }
}

/// Live PTY session. Output is drained continuously on a background thread,
/// then applied to the vt100 surface only when the client polls it.
pub struct PtySession {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: mpsc::Receiver<Vec<u8>>,
    surface: TerminalSurface,
    bytes_read: usize,
    exit_code: Option<u32>,
}

impl PtySession {
    pub fn spawn(
        program: &str,
        arguments: &[&str],
        rows: u16,
        columns: u16,
    ) -> Result<Self, WorkflowError> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| WorkflowError::Pty(format!("open failed: {error}")))?;
        let mut command = CommandBuilder::new(resolve_pty_program(program));
        command.args(arguments);
        if let Ok(directory) = std::env::current_dir() {
            command.cwd(directory);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| WorkflowError::Pty(format!("spawn failed: {error}")))?;
        drop(pair.slave);
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| WorkflowError::Pty(error.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| WorkflowError::Pty(error.to_string()))?;
        let (sender, output) = mpsc::sync_channel(32);
        thread::Builder::new()
            .name("wren-pty-output".to_owned())
            .spawn(move || {
                let mut buffer = [0_u8; 8 * 1024];
                while let Ok(read) = reader.read(&mut buffer) {
                    if read == 0 || sender.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
            })
            .map_err(WorkflowError::Io)?;
        Ok(Self {
            master: pair.master,
            writer,
            child,
            output,
            surface: TerminalSurface::new(rows, columns),
            bytes_read: 0,
            exit_code: None,
        })
    }

    pub fn send_input(&mut self, bytes: &[u8]) -> Result<(), WorkflowError> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn resize(&mut self, rows: u16, columns: u16) -> Result<(), WorkflowError> {
        self.master
            .resize(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| WorkflowError::Pty(error.to_string()))?;
        self.surface.resize(rows, columns);
        Ok(())
    }

    pub fn poll(&mut self) -> Result<bool, WorkflowError> {
        let mut changed = false;
        while let Ok(bytes) = self.output.try_recv() {
            self.bytes_read = self.bytes_read.saturating_add(bytes.len());
            self.surface.process(&bytes);
            changed = true;
        }
        if self.exit_code.is_none()
            && let Some(status) = self.child.try_wait()?
        {
            self.exit_code = Some(status.exit_code());
            changed = true;
        }
        Ok(changed)
    }

    #[must_use]
    pub fn surface(&self) -> &TerminalSurface {
        &self.surface
    }

    #[must_use]
    pub const fn bytes_read(&self) -> usize {
        self.bytes_read
    }

    #[must_use]
    pub const fn exit_code(&self) -> Option<u32> {
        self.exit_code
    }

    pub fn terminate(&mut self) -> Result<(), WorkflowError> {
        self.child.kill()?;
        Ok(())
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if self.exit_code.is_none() {
            let _ = self.child.kill();
        }
    }
}

pub struct PtyResult {
    pub surface: TerminalSurface,
    pub exit_success: bool,
    pub bytes_read: usize,
}

pub fn run_pty(
    program: &str,
    arguments: &[&str],
    rows: u16,
    columns: u16,
) -> Result<PtyResult, WorkflowError> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols: columns,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| WorkflowError::Pty(format!("open failed: {error}")))?;
    let mut command = CommandBuilder::new(resolve_pty_program(program));
    command.args(arguments);
    if let Ok(directory) = std::env::current_dir() {
        command.cwd(directory);
    }
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| WorkflowError::Pty(format!("spawn failed: {error}")))?;
    drop(pair.slave);
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| WorkflowError::Pty(error.to_string()))?;
    let mut output = Vec::new();
    reader.read_to_end(&mut output)?;
    let status = child
        .wait()
        .map_err(|error| WorkflowError::Pty(error.to_string()))?;
    let mut surface = TerminalSurface::new(rows, columns);
    surface.process(&output);
    Ok(PtyResult {
        surface,
        exit_success: status.success(),
        bytes_read: output.len(),
    })
}

fn resolve_pty_program(program: &str) -> PathBuf {
    let direct = PathBuf::from(program);
    if Path::new(program).is_absolute() || program.contains(std::path::MAIN_SEPARATOR) {
        return direct;
    }
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join(program))
                .find(|candidate| is_executable_file(candidate))
        })
        .unwrap_or(direct)
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DapMessage {
    pub seq: u64,
    #[serde(flatten)]
    pub body: serde_json::Value,
}

pub fn encode_dap(message: &DapMessage) -> Result<Vec<u8>, WorkflowError> {
    let body = serde_json::to_vec(message)?;
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend(body);
    Ok(frame)
}

pub fn decode_dap(frame: &[u8]) -> Result<DapMessage, WorkflowError> {
    let separator = b"\r\n\r\n";
    let header_end = frame
        .windows(separator.len())
        .position(|window| window == separator)
        .ok_or_else(|| WorkflowError::Dap("missing header terminator".into()))?;
    let header = std::str::from_utf8(&frame[..header_end])
        .map_err(|_| WorkflowError::Dap("header is not UTF-8".into()))?;
    let length = header
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .ok_or_else(|| WorkflowError::Dap("missing Content-Length".into()))?;
    let body = &frame[header_end + separator.len()..];
    if body.len() != length {
        return Err(WorkflowError::Dap("Content-Length mismatch".into()));
    }
    Ok(serde_json::from_slice(body)?)
}

pub struct DapClient {
    child: Child,
    input: BufWriter<ChildStdin>,
    output: BufReader<ChildStdout>,
    max_frame_bytes: usize,
}

impl DapClient {
    pub fn spawn(
        spec: &TaskSpec,
        trusted: bool,
        max_frame_bytes: usize,
    ) -> Result<Self, WorkflowError> {
        if !trusted {
            return Err(WorkflowError::UntrustedTask);
        }
        let mut command = Command::new(&spec.program);
        command
            .args(spec.arguments.iter().map(AsRef::<str>::as_ref))
            .env_clear()
            .envs(
                spec.environment
                    .iter()
                    .map(|(name, value)| (name.as_ref(), value.as_ref())),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn()?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("DAP stdin pipe missing"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("DAP stdout pipe missing"))?;
        if let Some(stderr) = child.stderr.take() {
            thread::Builder::new()
                .name("wren-dap-stderr".to_owned())
                .spawn(move || {
                    let _ = read_bounded(stderr, 1024 * 1024);
                })?;
        }
        Ok(Self {
            child,
            input: BufWriter::new(input),
            output: BufReader::new(output),
            max_frame_bytes: max_frame_bytes.max(1),
        })
    }

    pub fn send(&mut self, message: &DapMessage) -> Result<(), WorkflowError> {
        let frame = encode_dap(message)?;
        if frame.len() > self.max_frame_bytes {
            return Err(WorkflowError::Dap("outbound frame exceeds limit".into()));
        }
        self.input.write_all(&frame)?;
        self.input.flush()?;
        Ok(())
    }

    pub fn receive(&mut self) -> Result<DapMessage, WorkflowError> {
        let mut content_length = None;
        let mut header_bytes = 0_usize;
        loop {
            let mut line = String::new();
            if self.output.read_line(&mut line)? == 0 {
                return Err(WorkflowError::Dap("adapter closed its output".into()));
            }
            header_bytes = header_bytes.saturating_add(line.len());
            if header_bytes > 16 * 1024 {
                return Err(WorkflowError::Dap("header exceeds limit".into()));
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some(value) = line
                .trim_end_matches(['\r', '\n'])
                .strip_prefix("Content-Length:")
            {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
        let content_length =
            content_length.ok_or_else(|| WorkflowError::Dap("missing Content-Length".into()))?;
        if content_length > self.max_frame_bytes {
            return Err(WorkflowError::Dap("inbound frame exceeds limit".into()));
        }
        let mut body = vec![0_u8; content_length];
        self.output.read_exact(&mut body)?;
        Ok(serde_json::from_slice(&body)?)
    }

    pub fn request(&mut self, message: &DapMessage) -> Result<DapMessage, WorkflowError> {
        self.send(message)?;
        self.receive()
    }

    pub fn terminate(&mut self) -> Result<(), WorkflowError> {
        signal_process_group(&mut self.child, true)?;
        let _ = self.child.wait()?;
        Ok(())
    }
}

impl Drop for DapClient {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = signal_process_group(&mut self.child, true);
            let _ = self.child.wait();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspTextEdit {
    pub range: LspRange,
    #[serde(rename = "newText")]
    pub new_text: Box<str>,
}

pub fn lower_lsp_text_edits(
    document_id: DocumentId,
    base_revision: DocumentRevision,
    current_revision: DocumentRevision,
    text: &str,
    edits: Vec<LspTextEdit>,
) -> Result<RevisionedEdits, WorkflowError> {
    if base_revision != current_revision {
        return Err(WorkflowError::StaleRevision {
            expected: base_revision,
            actual: current_revision,
        });
    }
    let edits = edits
        .into_iter()
        .map(|edit| {
            let start = lsp_position_to_byte(text, edit.range.start)?;
            let end = lsp_position_to_byte(text, edit.range.end)?;
            if start > end {
                return Err(WorkflowError::InvalidLspPosition {
                    line: edit.range.end.line,
                    character: edit.range.end.character,
                });
            }
            Ok(Edit::new(start..end, edit.new_text))
        })
        .collect::<Result<Vec<_>, WorkflowError>>()?;
    Transaction::new(base_revision, edits.clone())
        .map_err(|error| WorkflowError::Transaction(error.to_string().into()))?;
    Ok(RevisionedEdits {
        document_id,
        base_revision,
        edits,
    })
}

fn lsp_position_to_byte(text: &str, position: LspPosition) -> Result<usize, WorkflowError> {
    let line = usize::try_from(position.line).map_err(|_| WorkflowError::InvalidLspPosition {
        line: position.line,
        character: position.character,
    })?;
    let start = if line == 0 {
        0
    } else {
        text.match_indices('\n')
            .nth(line - 1)
            .map(|(byte, _)| byte + 1)
            .ok_or(WorkflowError::InvalidLspPosition {
                line: position.line,
                character: position.character,
            })?
    };
    let end = text[start..]
        .find('\n')
        .map_or(text.len(), |offset| start + offset);
    let wanted =
        usize::try_from(position.character).map_err(|_| WorkflowError::InvalidLspPosition {
            line: position.line,
            character: position.character,
        })?;
    let mut utf16 = 0_usize;
    for (offset, character) in text[start..end].char_indices() {
        if utf16 == wanted {
            return Ok(start + offset);
        }
        utf16 = utf16.saturating_add(character.len_utf16());
        if utf16 > wanted {
            return Err(WorkflowError::InvalidLspPosition {
                line: position.line,
                character: position.character,
            });
        }
    }
    if utf16 == wanted {
        Ok(end)
    } else {
        Err(WorkflowError::InvalidLspPosition {
            line: position.line,
            character: position.character,
        })
    }
}

pub struct LspClient {
    child: Child,
    input: BufWriter<ChildStdin>,
    output: BufReader<ChildStdout>,
    max_frame_bytes: usize,
    next_request_id: u64,
}

impl LspClient {
    pub fn spawn(
        spec: &TaskSpec,
        trusted: bool,
        max_frame_bytes: usize,
    ) -> Result<Self, WorkflowError> {
        if !trusted {
            return Err(WorkflowError::UntrustedTask);
        }
        let mut command = Command::new(&spec.program);
        command
            .args(spec.arguments.iter().map(AsRef::<str>::as_ref))
            .env_clear()
            .envs(
                spec.environment
                    .iter()
                    .map(|(name, value)| (name.as_ref(), value.as_ref())),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn()?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("LSP stdin pipe missing"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("LSP stdout pipe missing"))?;
        if let Some(stderr) = child.stderr.take() {
            thread::Builder::new()
                .name("wren-lsp-stderr".to_owned())
                .spawn(move || {
                    let _ = read_bounded(stderr, 1024 * 1024);
                })?;
        }
        Ok(Self {
            child,
            input: BufWriter::new(input),
            output: BufReader::new(output),
            max_frame_bytes: max_frame_bytes.max(1),
            next_request_id: 1,
        })
    }

    pub fn notify(
        &mut self,
        method: &str,
        parameters: serde_json::Value,
    ) -> Result<(), WorkflowError> {
        self.write_message(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": parameters,
        }))
    }

    pub fn request(
        &mut self,
        method: &str,
        parameters: serde_json::Value,
    ) -> Result<serde_json::Value, WorkflowError> {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        self.write_message(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": parameters,
        }))?;
        loop {
            let message = self.read_message()?;
            if message.get("method").is_some() && message.get("id").is_some() {
                self.answer_server_request(&message)?;
                continue;
            }
            if message.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(WorkflowError::ProcessFailed(error.to_string().into()));
            }
            return Ok(message
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null));
        }
    }

    fn answer_server_request(&mut self, request: &serde_json::Value) -> Result<(), WorkflowError> {
        let Some(id) = request.get("id").cloned() else {
            return Ok(());
        };
        let result = match request
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
        {
            "workspace/configuration" => request
                .pointer("/params/items")
                .and_then(serde_json::Value::as_array)
                .map_or_else(
                    || serde_json::json!([]),
                    |items| serde_json::Value::Array(vec![serde_json::Value::Null; items.len()]),
                ),
            "workspace/workspaceFolders" => serde_json::json!([]),
            "workspace/applyEdit" => serde_json::json!({
                "applied": false,
                "failureReason": "workspace edits are applied through Wren's revision bridge"
            }),
            _ => serde_json::Value::Null,
        };
        self.write_message(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
    }

    pub fn initialize(
        &mut self,
        root_uri: &str,
        capabilities: serde_json::Value,
    ) -> Result<serde_json::Value, WorkflowError> {
        self.initialize_with_options(root_uri, capabilities, serde_json::Value::Null)
    }

    pub fn initialize_with_options(
        &mut self,
        root_uri: &str,
        capabilities: serde_json::Value,
        initialization_options: serde_json::Value,
    ) -> Result<serde_json::Value, WorkflowError> {
        let result = self.request(
            "initialize",
            serde_json::json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": capabilities,
                "initializationOptions": initialization_options,
            }),
        )?;
        self.notify("initialized", serde_json::json!({}))?;
        Ok(result)
    }

    pub fn did_open(
        &mut self,
        uri: &str,
        language_id: &str,
        version: i64,
        text: &str,
    ) -> Result<(), WorkflowError> {
        self.notify(
            "textDocument/didOpen",
            serde_json::json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": version,
                    "text": text,
                }
            }),
        )
    }

    pub fn did_change_full(
        &mut self,
        uri: &str,
        version: i64,
        text: &str,
    ) -> Result<(), WorkflowError> {
        self.notify(
            "textDocument/didChange",
            serde_json::json!({
                "textDocument": {"uri": uri, "version": version},
                "contentChanges": [{"text": text}],
            }),
        )
    }

    fn write_message(&mut self, message: &serde_json::Value) -> Result<(), WorkflowError> {
        let body = serde_json::to_vec(message)?;
        if body.len() > self.max_frame_bytes {
            return Err(WorkflowError::ProcessFailed(
                "outbound LSP frame exceeds limit".into(),
            ));
        }
        write!(self.input, "Content-Length: {}\r\n\r\n", body.len())?;
        self.input.write_all(&body)?;
        self.input.flush()?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<serde_json::Value, WorkflowError> {
        let mut content_length = None;
        let mut header_bytes = 0_usize;
        loop {
            let mut line = String::new();
            if self.output.read_line(&mut line)? == 0 {
                return Err(WorkflowError::ProcessFailed(
                    "language server closed its output".into(),
                ));
            }
            header_bytes = header_bytes.saturating_add(line.len());
            if header_bytes > 16 * 1024 {
                return Err(WorkflowError::ProcessFailed(
                    "LSP header exceeds limit".into(),
                ));
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some(value) = line
                .trim_end_matches(['\r', '\n'])
                .strip_prefix("Content-Length:")
            {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
        let content_length = content_length.ok_or_else(|| {
            WorkflowError::ProcessFailed("LSP response omitted Content-Length".into())
        })?;
        if content_length > self.max_frame_bytes {
            return Err(WorkflowError::ProcessFailed(
                "inbound LSP frame exceeds limit".into(),
            ));
        }
        let mut body = vec![0_u8; content_length];
        self.output.read_exact(&mut body)?;
        Ok(serde_json::from_slice(&body)?)
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = signal_process_group(&mut self.child, true);
            let _ = self.child.wait();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionedEdits {
    pub document_id: DocumentId,
    pub base_revision: DocumentRevision,
    pub edits: Vec<Edit>,
}

pub fn formatter_edits(
    document_id: DocumentId,
    base_revision: DocumentRevision,
    current_revision: DocumentRevision,
    before: &str,
    formatted: &str,
) -> Result<RevisionedEdits, WorkflowError> {
    if current_revision != base_revision {
        return Err(WorkflowError::StaleRevision {
            expected: base_revision,
            actual: current_revision,
        });
    }
    let edits = if before == formatted {
        Vec::new()
    } else {
        vec![Edit::new(0..before.len(), formatted)]
    };
    Ok(RevisionedEdits {
        document_id,
        base_revision,
        edits,
    })
}

pub fn run_formatter_process(
    spec: &TaskSpec,
    trusted: bool,
    document_id: DocumentId,
    base_revision: DocumentRevision,
    current_revision: DocumentRevision,
    input: &str,
) -> Result<RevisionedEdits, WorkflowError> {
    run_formatter_until_cancelled(
        spec,
        trusted,
        document_id,
        base_revision,
        current_revision,
        input,
        || false,
    )
}

pub fn run_formatter_until_cancelled(
    spec: &TaskSpec,
    trusted: bool,
    document_id: DocumentId,
    base_revision: DocumentRevision,
    current_revision: DocumentRevision,
    input: &str,
    is_cancelled: impl Fn() -> bool,
) -> Result<RevisionedEdits, WorkflowError> {
    if !trusted {
        return Err(WorkflowError::UntrustedTask);
    }
    if current_revision != base_revision {
        return Err(WorkflowError::StaleRevision {
            expected: base_revision,
            actual: current_revision,
        });
    }
    let mut command = Command::new(&spec.program);
    command
        .args(spec.arguments.iter().map(AsRef::<str>::as_ref))
        .env_clear()
        .envs(
            spec.environment
                .iter()
                .map(|(name, value)| (name.as_ref(), value.as_ref())),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("formatter stdin pipe missing"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("formatter stdout pipe missing"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("formatter stderr pipe missing"))?;
    let input_bytes = input.as_bytes().to_vec();
    let input_writer = thread::spawn(move || -> io::Result<()> {
        stdin.write_all(&input_bytes)?;
        stdin.flush()
    });
    let limit = spec.max_output_bytes.max(1);
    let stdout_reader = thread::spawn(move || read_bounded(stdout, limit));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, limit));
    let mut cancelled = false;
    let mut terminated_at = None;
    let status = loop {
        if is_cancelled() && !cancelled {
            cancelled = true;
            signal_process_group(&mut child, false)?;
            terminated_at = Some(Instant::now());
        }
        if terminated_at.is_some_and(|at| at.elapsed() >= Duration::from_millis(250)) {
            signal_process_group(&mut child, true)?;
            terminated_at = None;
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        thread::sleep(Duration::from_millis(1));
    };
    let input_result = input_writer
        .join()
        .map_err(|_| io::Error::other("formatter input writer panicked"))?;
    let (stdout, stdout_truncated) = stdout_reader
        .join()
        .map_err(|_| io::Error::other("formatter stdout reader panicked"))??;
    let (stderr, stderr_truncated) = stderr_reader
        .join()
        .map_err(|_| io::Error::other("formatter stderr reader panicked"))??;
    if cancelled {
        return Err(WorkflowError::Cancelled);
    }
    input_result?;
    if !status.success() {
        return Err(WorkflowError::ProcessFailed(
            String::from_utf8_lossy(&stderr).trim().to_owned().into(),
        ));
    }
    if stdout_truncated || stderr_truncated {
        return Err(WorkflowError::ProcessFailed(
            "formatter output exceeded its declared bound".into(),
        ));
    }
    let formatted = String::from_utf8(stdout)
        .map_err(|error| WorkflowError::ProcessFailed(error.to_string().into()))?;
    formatter_edits(
        document_id,
        base_revision,
        current_revision,
        input,
        &formatted,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDocumentEdit {
    pub document_id: DocumentId,
    pub lease_epoch: LeaseEpoch,
    pub base_revision: DocumentRevision,
    pub edits: Vec<Edit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspResourceEdit {
    Create {
        path: PathBuf,
        expected_absent: bool,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
        expected_source_identity: FileIdentity,
        expected_target: Option<FileIdentity>,
    },
    Delete {
        path: PathBuf,
        expected_identity: FileIdentity,
    },
}

pub fn lower_lsp_workspace_edit(
    group_id: SemanticGroupId,
    documents: Vec<LspDocumentEdit>,
    resources: Vec<LspResourceEdit>,
) -> Result<WorkspaceTransaction, WorkflowError> {
    let document_edits = documents
        .into_iter()
        .map(|document| {
            let transaction = Transaction::new(document.base_revision, document.edits)
                .map_err(|error| WorkflowError::Transaction(error.to_string().into()))?;
            Ok(DocumentMutation {
                document_id: document.document_id,
                lease_epoch: document.lease_epoch,
                base_revision: document.base_revision,
                semantic_group_id: group_id,
                semantic_group_kind: SemanticGroupKind::WorkspaceRefactor,
                undo_parent: None,
                transactions: vec![transaction],
            })
        })
        .collect::<Result<Vec<_>, WorkflowError>>()?;
    let resource_ops = resources
        .into_iter()
        .map(|resource| match resource {
            LspResourceEdit::Create {
                path,
                expected_absent,
            } => ResourceOp::Create {
                path: path.to_string_lossy().into_owned().into_boxed_str(),
                expected_absent,
            },
            LspResourceEdit::Rename {
                from,
                to,
                expected_source_identity,
                expected_target,
            } => ResourceOp::Rename {
                from: from.to_string_lossy().into_owned().into_boxed_str(),
                to: to.to_string_lossy().into_owned().into_boxed_str(),
                expected_source_identity,
                expected_target: expected_target
                    .map_or(ExpectedTarget::Absent, ExpectedTarget::Identity),
            },
            LspResourceEdit::Delete {
                path,
                expected_identity,
            } => ResourceOp::Delete {
                path: path.to_string_lossy().into_owned().into_boxed_str(),
                expected_identity,
            },
        })
        .collect();
    Ok(WorkspaceTransaction {
        document_edits,
        resource_ops,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excerpt {
    pub document_id: DocumentId,
    pub source_range: Range<usize>,
    pub text: Box<str>,
    pub base_revision: DocumentRevision,
    pub lease_epoch: LeaseEpoch,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExcerptBuffer {
    pub excerpts: Vec<Excerpt>,
}

impl ExcerptBuffer {
    pub fn commit(
        &self,
        group_id: SemanticGroupId,
        replacements: &BTreeMap<DocumentId, Vec<Edit>>,
    ) -> Result<WorkspaceTransaction, WorkflowError> {
        let mut grouped: BTreeMap<DocumentId, (LeaseEpoch, DocumentRevision)> = BTreeMap::new();
        for excerpt in &self.excerpts {
            if !replacements.contains_key(&excerpt.document_id) {
                continue;
            }
            let entry = grouped
                .entry(excerpt.document_id)
                .or_insert((excerpt.lease_epoch, excerpt.base_revision));
            if entry.0 != excerpt.lease_epoch || entry.1 != excerpt.base_revision {
                return Err(WorkflowError::StaleRevision {
                    expected: entry.1,
                    actual: excerpt.base_revision,
                });
            }
        }
        let mut document_edits = Vec::with_capacity(grouped.len());
        for (document_id, (lease_epoch, base_revision)) in grouped {
            let edits = replacements.get(&document_id).cloned().unwrap_or_default();
            let transaction = Transaction::new(base_revision, edits)
                .map_err(|error| WorkflowError::Transaction(error.to_string().into()))?;
            document_edits.push(DocumentMutation {
                document_id,
                lease_epoch,
                base_revision,
                semantic_group_id: group_id,
                semantic_group_kind: SemanticGroupKind::WorkspaceRefactor,
                undo_parent: None,
                transactions: vec![transaction],
            });
        }
        Ok(WorkspaceTransaction {
            document_edits,
            resource_ops: Vec::new(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHunk {
    pub before: Range<u32>,
    pub after: Range<u32>,
}

#[must_use]
pub fn git_hunks(before: &str, after: &str) -> Vec<GitHunk> {
    let input = InternedInput::new(before, after);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    diff.hunks()
        .map(|hunk| GitHunk {
            before: hunk.before,
            after: hunk.after,
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralMatch {
    pub range: Range<usize>,
    pub metavariables: BTreeMap<Box<str>, Range<usize>>,
}

/// The compiled backend identity is exposed so manifests can report exactly
/// which structural engine produced a result. A language-specific tree-sitter
/// document is supplied by the workspace provider in production.
#[must_use]
pub fn structural_backend() -> &'static str {
    std::any::type_name::<ast_grep_core::MatchStrictness>()
}

#[must_use]
pub fn structural_literal_search(text: &str, pattern: &str) -> Vec<StructuralMatch> {
    text.match_indices(pattern)
        .map(|(start, value)| StructuralMatch {
            range: start..start + value.len(),
            metavariables: BTreeMap::new(),
        })
        .collect()
}

pub fn structural_search(
    language: &str,
    text: &str,
    pattern: &str,
    max_matches: usize,
) -> Result<Vec<StructuralMatch>, WorkflowError> {
    use ast_grep_core::Pattern;
    use ast_grep_core::meta_var::MetaVariable;
    use ast_grep_language::{LanguageExt, SupportLang};

    let language = language
        .parse::<SupportLang>()
        .map_err(|error| WorkflowError::Structural(error.to_string().into()))?;
    let pattern = Pattern::try_new(pattern, language)
        .map_err(|error| WorkflowError::Structural(error.to_string().into()))?;
    let document = language.ast_grep(text);
    Ok(document
        .root()
        .find_all(pattern)
        .take(max_matches)
        .map(|matched| {
            let metavariables = matched
                .get_env()
                .get_matched_variables()
                .filter_map(|variable| match variable {
                    MetaVariable::Capture(name, _) => matched
                        .get_env()
                        .get_match(&name)
                        .map(|node| (name.into_boxed_str(), node.range())),
                    MetaVariable::MultiCapture(name) => {
                        let nodes = matched.get_env().get_multiple_matches(&name);
                        nodes.first().zip(nodes.last()).map(|(first, last)| {
                            (name.into_boxed_str(), first.range().start..last.range().end)
                        })
                    }
                    MetaVariable::Dropped(_) | MetaVariable::Multiple => None,
                })
                .collect();
            StructuralMatch {
                range: matched.range(),
                metavariables,
            }
        })
        .collect())
}

pub fn structural_rewrite(
    language: &str,
    text: &str,
    pattern: &str,
    replacement: &str,
    max_edits: usize,
) -> Result<Vec<Edit>, WorkflowError> {
    use ast_grep_core::Pattern;
    use ast_grep_language::{LanguageExt, SupportLang};

    let language = language
        .parse::<SupportLang>()
        .map_err(|error| WorkflowError::Structural(error.to_string().into()))?;
    let pattern = Pattern::try_new(pattern, language)
        .map_err(|error| WorkflowError::Structural(error.to_string().into()))?;
    let document = language.ast_grep(text);
    let mut edits = document
        .root()
        .find_all(&pattern)
        .take(max_edits)
        .map(|matched| {
            let edit = matched.replace_by(replacement);
            let insert = String::from_utf8(edit.inserted_text)
                .map_err(|error| WorkflowError::Structural(error.to_string().into()))?;
            Ok(Edit::new(
                edit.position..edit.position.saturating_add(edit.deleted_length),
                insert,
            ))
        })
        .collect::<Result<Vec<_>, WorkflowError>>()?;
    edits.sort_by_key(|edit| edit.range.start);
    Ok(edits)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiReviewBranch {
    pub document_id: DocumentId,
    pub base_revision: DocumentRevision,
    pub proposed: Vec<Edit>,
}

impl AiReviewBranch {
    pub fn accept(
        &self,
        selected: &[usize],
        current_revision: DocumentRevision,
        intervening: &[Transaction],
    ) -> Result<Transaction, WorkflowError> {
        let mut expected_revision = self.base_revision;
        for transaction in intervening {
            if transaction.base_revision != expected_revision {
                return Err(WorkflowError::Unmappable);
            }
            expected_revision = expected_revision.next().ok_or(WorkflowError::Unmappable)?;
        }
        if expected_revision != current_revision {
            return Err(WorkflowError::Unmappable);
        }
        let mut edits = Vec::new();
        for index in selected {
            let mut edit = self
                .proposed
                .get(*index)
                .cloned()
                .ok_or(WorkflowError::Unmappable)?;
            for transaction in intervening {
                edit.range.start = transaction
                    .map_offset(edit.range.start, Bias::Left)
                    .map_err(|_| WorkflowError::Unmappable)?;
                edit.range.end = transaction
                    .map_offset(edit.range.end, Bias::Right)
                    .map_err(|_| WorkflowError::Unmappable)?;
            }
            edits.push(edit);
        }
        edits.sort_by_key(|edit| edit.range.start);
        Transaction::new(current_revision, edits)
            .map_err(|error| WorkflowError::Transaction(error.to_string().into()))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn inherited_path_environment() -> BTreeMap<Box<str>, Box<str>> {
        let mut environment = BTreeMap::new();
        if let Ok(path) = std::env::var("PATH") {
            environment.insert("PATH".into(), path.into());
        }
        environment
    }

    #[test]
    fn task_visibility_is_explicit_and_output_is_bounded() {
        let supervisor = TaskSupervisor::new(true);
        let output = supervisor
            .run(&TaskSpec {
                program: resolve_pty_program("sh"),
                arguments: vec!["-c".into(), "printf 123456".into()],
                environment: BTreeMap::new(),
                visibility: DocumentVisibility::Persisted,
                save: SavePolicy::Prompt,
                max_output_bytes: 4,
            })
            .expect("task");
        assert_eq!(output.stdout, b"1234");
        assert!(output.truncated);
        assert!(
            TaskSupervisor::new(false)
                .run(&TaskSpec {
                    program: resolve_pty_program("true"),
                    arguments: Vec::new(),
                    environment: inherited_path_environment(),
                    visibility: DocumentVisibility::Persisted,
                    save: SavePolicy::Never,
                    max_output_bytes: 1,
                })
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn task_cancellation_terminates_the_whole_process_group() {
        let (cancel, cancelled) = mpsc::channel();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            let _ = cancel.send(());
        });
        let started = Instant::now();
        let output = TaskSupervisor::new(true)
            .run_until_cancelled(
                &TaskSpec {
                    program: resolve_pty_program("sh"),
                    arguments: vec!["-c".into(), "sleep 5 & wait".into()],
                    environment: inherited_path_environment(),
                    visibility: DocumentVisibility::Persisted,
                    save: SavePolicy::Never,
                    max_output_bytes: 16,
                },
                || cancelled.try_recv().is_ok(),
            )
            .expect("cancel task");
        canceller.join().expect("canceller");
        assert!(output.cancelled);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn task_cancellation_escalates_when_sigterm_is_ignored() {
        let (cancel, cancelled) = mpsc::channel();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            let _ = cancel.send(());
        });
        let started = Instant::now();
        let output = TaskSupervisor::new(true)
            .run_until_cancelled(
                &TaskSpec {
                    program: resolve_pty_program("sh"),
                    arguments: vec![
                        "-c".into(),
                        "trap '' TERM; while :; do sleep 1; done".into(),
                    ],
                    environment: inherited_path_environment(),
                    visibility: DocumentVisibility::Persisted,
                    save: SavePolicy::Never,
                    max_output_bytes: 16,
                },
                || cancelled.try_recv().is_ok(),
            )
            .expect("force cancel task");
        canceller.join().expect("canceller");
        assert!(output.cancelled);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn pty_output_is_emulated_into_a_terminal_surface() {
        let output = run_pty("sh", &["-c", "printf '\\033[31mRED\\033[0m'"], 4, 20).expect("pty");
        assert!(output.exit_success);
        assert_eq!(output.surface.contents(), "RED");
    }

    #[cfg(unix)]
    #[test]
    fn live_pty_accepts_input_and_streams_a_resizable_surface() {
        let mut session = PtySession::spawn(
            "sh",
            &["-c", "IFS= read -r line; printf 'got:%s' \"$line\""],
            4,
            20,
        )
        .expect("spawn live pty");
        session.resize(5, 30).expect("resize");
        session.send_input(b"hello\n").expect("input");
        let deadline = Instant::now() + Duration::from_secs(2);
        while session.exit_code().is_none() && Instant::now() < deadline {
            session.poll().expect("poll");
            thread::sleep(Duration::from_millis(1));
        }
        session.poll().expect("final poll");
        assert_eq!(session.exit_code(), Some(0));
        assert!(session.surface().contents().contains("got:hello"));
        assert!(session.bytes_read() > 0);
    }

    #[test]
    fn dap_framing_round_trips_and_rejects_truncation() {
        let message = DapMessage {
            seq: 4,
            body: json!({"type":"request","command":"continue"}),
        };
        let frame = encode_dap(&message).expect("encode");
        assert_eq!(decode_dap(&frame).expect("decode"), message);
        assert!(decode_dap(&frame[..frame.len() - 1]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn native_dap_client_spawns_adapter_and_exchanges_framed_messages() {
        let response = r#"{"seq":5,"type":"response","request_seq":4,"success":true}"#;
        let script = format!(
            "while IFS= read -r line; do [ \"$line\" = \"$(printf '\\r')\" ] && break; done; printf 'Content-Length: {}\\r\\n\\r\\n%s' '{}'",
            response.len(),
            response
        );
        let mut client = DapClient::spawn(
            &TaskSpec {
                program: resolve_pty_program("sh"),
                arguments: vec!["-c".into(), script.into()],
                environment: BTreeMap::new(),
                visibility: DocumentVisibility::Persisted,
                save: SavePolicy::Never,
                max_output_bytes: 1024,
            },
            true,
            1024,
        )
        .expect("spawn adapter");
        let request = DapMessage {
            seq: 4,
            body: json!({"type":"request","command":"continue"}),
        };
        let received = client.request(&request).expect("DAP exchange");
        assert_eq!(received.seq, 5);
        assert_eq!(received.body["success"], true);
    }

    #[test]
    fn lsp_utf16_edits_lower_to_validated_utf8_byte_transactions() {
        let lowered = lower_lsp_text_edits(
            DocumentId::new(2),
            DocumentRevision::new(3),
            DocumentRevision::new(3),
            "a😀b\n",
            vec![LspTextEdit {
                range: LspRange {
                    start: LspPosition {
                        line: 0,
                        character: 1,
                    },
                    end: LspPosition {
                        line: 0,
                        character: 3,
                    },
                },
                new_text: "X".into(),
            }],
        )
        .expect("lower UTF-16 edit");
        assert_eq!(lowered.edits[0].range, 1..5);
        assert!(matches!(
            lower_lsp_text_edits(
                DocumentId::new(2),
                DocumentRevision::new(3),
                DocumentRevision::new(3),
                "😀",
                vec![LspTextEdit {
                    range: LspRange {
                        start: LspPosition {
                            line: 0,
                            character: 1,
                        },
                        end: LspPosition {
                            line: 0,
                            character: 2,
                        },
                    },
                    new_text: "x".into(),
                }],
            ),
            Err(WorkflowError::InvalidLspPosition { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn native_lsp_client_spawns_and_correlates_json_rpc_responses() {
        let response = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        let script = format!(
            "while IFS= read -r line; do [ \"$line\" = \"$(printf '\\r')\" ] && break; done; printf 'Content-Length: {}\\r\\n\\r\\n%s' '{}'",
            response.len(),
            response
        );
        let mut client = LspClient::spawn(
            &TaskSpec {
                program: resolve_pty_program("sh"),
                arguments: vec!["-c".into(), script.into()],
                environment: BTreeMap::new(),
                visibility: DocumentVisibility::Persisted,
                save: SavePolicy::Never,
                max_output_bytes: 1024,
            },
            true,
            1024,
        )
        .expect("spawn language server");
        let result = client
            .request("initialize", serde_json::json!({"capabilities": {}}))
            .expect("LSP request");
        assert!(result.get("capabilities").is_some());
    }

    #[test]
    fn formatter_lsp_excerpt_and_ai_edits_stay_revisioned() {
        assert!(matches!(
            formatter_edits(
                DocumentId::new(1),
                DocumentRevision::new(1),
                DocumentRevision::new(2),
                "a",
                "b"
            ),
            Err(WorkflowError::StaleRevision { .. })
        ));
        let transaction = lower_lsp_workspace_edit(
            SemanticGroupId::new(1),
            vec![LspDocumentEdit {
                document_id: DocumentId::new(1),
                lease_epoch: LeaseEpoch::new(1),
                base_revision: DocumentRevision::new(1),
                edits: vec![Edit::new(0..1, "b")],
            }],
            Vec::new(),
        )
        .expect("LSP edit");
        assert_eq!(transaction.document_edits.len(), 1);

        let branch = AiReviewBranch {
            document_id: DocumentId::new(1),
            base_revision: DocumentRevision::new(1),
            proposed: vec![Edit::new(2..3, "AI")],
        };
        let mapped = branch
            .accept(
                &[0],
                DocumentRevision::new(2),
                &[
                    Transaction::new(DocumentRevision::new(1), vec![Edit::new(0..0, "xx")])
                        .expect("intervening"),
                ],
            )
            .expect("partial accept");
        assert_eq!(mapped.edits[0].range, 4..5);
    }

    #[cfg(unix)]
    #[test]
    fn formatter_process_is_bounded_and_revision_validated() {
        let formatted = run_formatter_process(
            &TaskSpec {
                program: resolve_pty_program("tr"),
                arguments: vec!["a-z".into(), "A-Z".into()],
                environment: BTreeMap::new(),
                visibility: DocumentVisibility::Persisted,
                save: SavePolicy::Never,
                max_output_bytes: 1024,
            },
            true,
            DocumentId::new(1),
            DocumentRevision::new(2),
            DocumentRevision::new(2),
            "let value\n",
        )
        .expect("format");
        assert_eq!(formatted.edits[0].insert.as_ref(), "LET VALUE\n");
        assert!(matches!(
            run_formatter_process(
                &TaskSpec {
                    program: resolve_pty_program("tr"),
                    arguments: vec!["a-z".into(), "A-Z".into()],
                    environment: BTreeMap::new(),
                    visibility: DocumentVisibility::Persisted,
                    save: SavePolicy::Never,
                    max_output_bytes: 1024,
                },
                true,
                DocumentId::new(1),
                DocumentRevision::new(2),
                DocumentRevision::new(3),
                "x",
            ),
            Err(WorkflowError::StaleRevision { .. })
        ));
    }

    #[test]
    fn git_and_structural_results_are_native_and_bounded() {
        let hunks = git_hunks("one\ntwo\n", "one\nchanged\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(
            structural_literal_search("foo(bar); foo(baz)", "foo").len(),
            2
        );
        assert!(structural_backend().contains("ast_grep_core"));
        let matches = structural_search("rust", "fn one() { Some(1); Some(2); }", "Some($A)", 1)
            .expect("ast-grep search");
        assert_eq!(matches.len(), 1);
        assert!(matches[0].metavariables.contains_key("A"));
        let edits = structural_rewrite("rust", "fn one() { Some(1); }", "Some($A)", "Ok($A)", 8)
            .expect("ast-grep rewrite");
        let rewritten = Transaction::new(DocumentRevision::new(1), edits)
            .expect("rewrite transaction")
            .apply_to_string("fn one() { Some(1); }")
            .expect("apply rewrite");
        assert_eq!(rewritten, "fn one() { Ok(1); }");
    }
}
