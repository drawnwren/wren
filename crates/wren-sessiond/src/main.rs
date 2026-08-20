use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use wren_remote::{REMOTE_PROTOCOL_MAJOR, TransportLane};
use wren_session::{SessionAuthority, SessionJournal};
use wren_sessiond::SessionServer;
use wren_shmem::SharedDocumentHeadWriter;
use wren_types::SessionId;

use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;

#[derive(Debug)]
struct Arguments {
    socket: Option<PathBuf>,
    state_dir: PathBuf,
    session_id: SessionId,
    head_table: Option<PathBuf>,
    transport: Option<TransportLane>,
    workspace: Option<PathBuf>,
}

fn arguments() -> Result<Arguments> {
    let mut socket = None;
    let mut state_dir = None;
    let mut session_id = SessionId::new(1);
    let mut head_table = None;
    let mut transport = None;
    let mut workspace = None;
    let mut values = env::args().skip(1);
    while let Some(argument) = values.next() {
        match argument.as_str() {
            "--socket" => {
                socket = Some(values.next().context("--socket requires a path")?.into());
            }
            "--state-dir" => {
                state_dir = Some(values.next().context("--state-dir requires a path")?.into());
            }
            "--session-id" => {
                session_id = SessionId::new(values.next().context("--session-id requires an integer")?.parse()?);
            }
            "--head-table" => {
                head_table = Some(values.next().context("--head-table requires a path")?.into());
            }
            "--transport" => {
                transport = Some(match values.next().as_deref() {
                    Some("control") => TransportLane::Control,
                    Some("bulk") => TransportLane::Bulk,
                    _ => bail!("--transport must be control or bulk"),
                });
            }
            "--protocol" => {
                let protocol = values.next().context("--protocol requires MAJOR.MINOR")?;
                let (major, _) = protocol.split_once('.').context("--protocol requires MAJOR.MINOR")?;
                let major: u16 = major.parse()?;
                if major != REMOTE_PROTOCOL_MAJOR {
                    bail!("incompatible remote protocol major {major}; expected {REMOTE_PROTOCOL_MAJOR}");
                }
            }
            "--workspace" => {
                workspace = Some(values.next().context("--workspace requires a path")?.into());
            }
            "-h" | "--help" => {
                println!(
                    "wren-sessiond --socket PATH --state-dir PATH [--session-id INTEGER] [--head-table PATH]\n\
                     wren-sessiond --transport control|bulk --protocol MAJOR.MINOR --workspace PATH --state-dir PATH"
                );
                std::process::exit(0);
            }
            argument => bail!("unknown argument {argument}"),
        }
    }
    Ok(Arguments { socket, state_dir: state_dir.context("--state-dir is required")?, session_id, head_table, transport, workspace })
}

struct StdioConnection {
    input: io::Stdin,
    output: io::Stdout,
}

impl Read for StdioConnection {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.input.read(buffer)
    }
}

impl Write for StdioConnection {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.output.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

fn main() -> Result<()> {
    let arguments = arguments()?;
    if let Some(lane) = arguments.transport {
        let workspace = arguments.workspace.as_ref().context("--workspace is required with --transport")?;
        fs::create_dir_all(&arguments.state_dir).with_context(|| format!("create remote state directory {}", arguments.state_dir.display()))?;
        let journal = SessionJournal::in_directory(arguments.state_dir.join(match lane {
            TransportLane::Control => "session",
            TransportLane::Bulk => "bulk-session",
        }));
        let restarted = journal.path().exists();
        let mut authority = SessionAuthority::open(journal, arguments.session_id).context("open durable remote session authority")?;
        if lane == TransportLane::Control && restarted {
            authority.break_event_continuity().context("advance remote session epoch after daemon restart")?;
        }
        let server = SessionServer::new(authority).with_remote_workspace(workspace, arguments.state_dir.join("blob-cache"), 512 * 1024 * 1024, lane)?;
        let mut connection = StdioConnection { input: io::stdin(), output: io::stdout() };
        server.serve_connection(&mut connection)?;
        return Ok(());
    }
    let socket = arguments.socket.as_ref().context("--socket is required without --transport")?;
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create socket directory {}", parent.display()))?;
    }
    let journal = SessionJournal::in_directory(&arguments.state_dir);
    let restarted = journal.path().exists();
    let mut authority = SessionAuthority::open(journal, arguments.session_id).context("open durable session authority")?;
    if restarted {
        authority.break_event_continuity().context("advance session epoch after daemon restart")?;
    }
    let head_path = arguments.head_table.unwrap_or_else(|| arguments.state_dir.join("document-heads.link"));
    let head_writer = std::sync::Arc::new(
        SharedDocumentHeadWriter::create_or_replace_stale(&head_path, 4_096).with_context(|| format!("create shared head table {}", head_path.display()))?,
    );
    let server = SessionServer::new(authority).with_head_writer(head_writer).context("publish initial document heads")?;
    let listener = UnixListener::bind(socket).with_context(|| format!("bind control socket {}", socket.display()))?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600)).with_context(|| format!("secure control socket {}", socket.display()))?;
    for connection in listener.incoming() {
        let mut connection = connection.context("accept control connection")?;
        let server = server.clone();
        std::thread::spawn(move || {
            if let Err(error) = server.serve_connection(&mut connection) {
                eprintln!("wren-sessiond connection closed: {error}");
            }
        });
    }
    Ok(())
}
