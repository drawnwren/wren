use std::env;
use std::fs::{self, File};
use std::hint::black_box;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use hdrhistogram::Histogram;
use serde_json::{Value, json};
use tempfile::tempdir;
use wren_client_state::{ClientViewStateStore, PublishedViewport};
use wren_engine::EngineFrame;
use wren_session::{SessionAuthority, SessionJournal};
use wren_shmem::{SharedDocumentHeadReader, SharedDocumentHeadWriter};
use wren_types::{
    ClientId, ConfigGeneration, DocumentHead, DocumentId, DocumentRevision, HeadValidation,
    PublishedViewportKey, ResumeViewState, SelRange, SelectionSet, SessionEpoch, SessionId, ViewId,
};
use wren_view::ViewportLayout;

const GATE_NANOS: u64 = 5_000_000;
const VIEWPORT_BYTES: usize = 64 * 1024;

#[derive(Debug)]
struct Arguments {
    iterations: u64,
    cpu: Option<usize>,
    output: Option<PathBuf>,
    gate: bool,
    b3_path: Option<PathBuf>,
    probe_file: Option<PathBuf>,
}

#[derive(Debug)]
struct Timings {
    speculative: Histogram<u64>,
    correct: Histogram<u64>,
    interactive: Histogram<u64>,
}

impl Timings {
    fn new() -> Result<Self> {
        Ok(Self {
            speculative: histogram()?,
            correct: histogram()?,
            interactive: histogram()?,
        })
    }

    fn record(&mut self, speculative: u64, correct: u64, interactive: u64) -> Result<()> {
        self.speculative.record(speculative.max(1))?;
        self.correct.record(correct.max(1))?;
        self.interactive.record(interactive.max(1))?;
        Ok(())
    }

    fn report(&self) -> Value {
        json!({
            "unit": "nanoseconds",
            "samples": self.correct.len(),
            "time_to_speculative_frame": distribution(&self.speculative),
            "time_to_correct_frame": distribution(&self.correct),
            "time_to_interactive": distribution(&self.interactive),
        })
    }
}

fn histogram() -> Result<Histogram<u64>> {
    Histogram::new_with_bounds(1, 60_000_000_000, 3).map_err(Into::into)
}

fn distribution(histogram: &Histogram<u64>) -> Value {
    json!({
        "min": histogram.min(),
        "p50": histogram.value_at_quantile(0.50),
        "p90": histogram.value_at_quantile(0.90),
        "p99": histogram.value_at_quantile(0.99),
        "max": histogram.max(),
    })
}

fn arguments() -> Result<Arguments> {
    let values: Vec<String> = env::args().skip(1).collect();
    let mut arguments = Arguments {
        iterations: 1_000,
        cpu: None,
        output: None,
        gate: false,
        b3_path: None,
        probe_file: None,
    };
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--iterations" => {
                index += 1;
                arguments.iterations = values
                    .get(index)
                    .context("--iterations needs a value")?
                    .parse()?;
            }
            "--cpu" => {
                index += 1;
                arguments.cpu = Some(values.get(index).context("--cpu needs a value")?.parse()?);
            }
            "--output" => {
                index += 1;
                arguments.output = Some(PathBuf::from(
                    values.get(index).context("--output needs a value")?,
                ));
            }
            "--b3-path" => {
                index += 1;
                arguments.b3_path = Some(PathBuf::from(
                    values.get(index).context("--b3-path needs a value")?,
                ));
            }
            "--probe-file" => {
                index += 1;
                arguments.probe_file = Some(PathBuf::from(
                    values.get(index).context("--probe-file needs a value")?,
                ));
            }
            "--gate" => arguments.gate = true,
            argument => anyhow::bail!("unknown argument: {argument}"),
        }
        index += 1;
    }
    anyhow::ensure!(arguments.iterations > 0, "--iterations must be positive");
    Ok(arguments)
}

fn pin_cpu(index: usize) -> bool {
    core_affinity::get_core_ids()
        .and_then(|ids| ids.get(index).copied())
        .is_some_and(core_affinity::set_for_current)
}

fn validate_gate_environment(arguments: &Arguments, pinned: bool) -> Result<()> {
    if !arguments.gate {
        return Ok(());
    }
    anyhow::ensure!(
        env::var("WREN_BARE_METAL").as_deref() == Ok("1"),
        "--gate requires WREN_BARE_METAL=1 on the dedicated benchmark runner"
    );
    anyhow::ensure!(arguments.cpu.is_some(), "--gate requires --cpu");
    anyhow::ensure!(pinned, "requested benchmark CPU could not be pinned");
    Ok(())
}

fn fixture_text() -> String {
    let line = "fn measured_startup() { let β = \"viewport\"; }\n";
    line.repeat(24_000)
}

fn mounted_filesystem_type(path: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(path).ok()?;
    #[cfg(target_os = "linux")]
    {
        let mountinfo = fs::read_to_string("/proc/self/mountinfo").ok()?;
        return mountinfo
            .lines()
            .filter_map(|line| {
                let (mount, filesystem) = line.split_once(" - ")?;
                let mountpoint = PathBuf::from(mount.split_whitespace().nth(4)?);
                canonical
                    .starts_with(&mountpoint)
                    .then(|| filesystem.split_whitespace().next().map(str::to_owned))
                    .flatten()
                    .map(|filesystem| (mountpoint.components().count(), filesystem))
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, filesystem)| filesystem);
    }
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("stat")
            .args(["-f", "%T"])
            .arg(canonical)
            .output()
            .ok()?;
        return output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    #[allow(unreachable_code)]
    None
}

fn is_network_or_fuse_path(path: &Path) -> bool {
    mounted_filesystem_type(path).is_some_and(|filesystem| {
        let filesystem = filesystem.to_ascii_lowercase();
        filesystem.starts_with("fuse")
            || matches!(
                filesystem.as_str(),
                "nfs" | "nfs4" | "cifs" | "smbfs" | "sshfs" | "9p" | "afs" | "davfs"
            )
    })
}

fn resume_state() -> ResumeViewState {
    ResumeViewState {
        client_id: ClientId::new(1),
        view_id: ViewId::new(1),
        document_id: DocumentId::new(1),
        document_revision: DocumentRevision::new(7),
        selections: SelectionSet {
            primary: 0,
            ranges: vec![SelRange {
                anchor: 32,
                head: 32,
            }],
        },
        top_line: 400,
        rows: 40,
        columns: 120,
        config_generation: ConfigGeneration::new(1),
    }
}

fn published_key() -> PublishedViewportKey {
    PublishedViewportKey {
        client_id: ClientId::new(1),
        view_id: ViewId::new(1),
        document_revision: DocumentRevision::new(7),
        rows: 40,
        columns: 120,
        theme_hash: [9; 32],
        config_generation: ConfigGeneration::new(1),
        renderer_version: 1,
    }
}

fn shared_heads(directory: &Path) -> Result<(SharedDocumentHeadWriter, SharedDocumentHeadReader)> {
    let path = directory.join("startup-heads.link");
    let writer = SharedDocumentHeadWriter::create(&path, 4)?;
    writer.publish(&[DocumentHead {
        session_epoch: SessionEpoch::new(1),
        document_id: DocumentId::new(1),
        authoritative_revision: DocumentRevision::new(7),
    }])?;
    let reader = SharedDocumentHeadReader::open(path)?;
    Ok((writer, reader))
}

fn scenario_a(
    iterations: u64,
    directory: &Path,
    text: &str,
    heads: &SharedDocumentHeadReader,
) -> Result<Timings> {
    let state = resume_state();
    let frame = EngineFrame {
        text: text.into(),
        cursor_byte: state.selections.ranges[0].head,
    };
    let mut layout = ViewportLayout::new(state.columns, state.rows);
    layout.top_line = state.top_line;
    let published = PublishedViewport {
        session_epoch: SessionEpoch::new(1),
        document_id: DocumentId::new(1),
        key: published_key(),
        grid: layout.desired_grid(&frame),
    };
    let store = ClientViewStateStore::new(directory.join("startup-client-state"));
    store.save_resume(&state)?;
    store.save_viewport(&published)?;
    let mut timings = Timings::new()?;
    for _ in 0..iterations {
        let started = Instant::now();
        let cached = store
            .load_viewport(ClientId::new(1), ViewId::new(1), DocumentId::new(1))?
            .context("scenario A published viewport")?;
        anyhow::ensure!(
            cached.key == published_key(),
            "scenario A cache key drifted"
        );
        let speculative_grid = Arc::new(cached.grid);
        black_box(&speculative_grid);
        let speculative = nanos(started);
        anyhow::ensure!(
            heads.validate(SessionEpoch::new(1), &state)? == HeadValidation::Correct,
            "scenario A published viewport was not head-valid"
        );
        let correct = nanos(started);
        black_box((speculative_grid.cursor, state.selections.primary));
        timings.record(speculative, correct, nanos(started))?;
    }
    Ok(timings)
}

fn scenario_b1(
    iterations: u64,
    path: &Path,
    known_offset: u64,
    heads: &SharedDocumentHeadReader,
) -> Result<Timings> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut warm = Vec::new();
    File::open(path)?.read_to_end(&mut warm)?;
    black_box(warm);
    let state = resume_state();
    let mut timings = Timings::new()?;
    for _ in 0..iterations {
        let started = Instant::now();
        let bytes = read_at(&file, known_offset, VIEWPORT_BYTES)?;
        let text = std::str::from_utf8(&bytes).context("fixture viewport is UTF-8")?;
        let mut layout = ViewportLayout::new(state.columns, state.rows);
        let grid = layout.desired_grid(&EngineFrame {
            text: text.into(),
            cursor_byte: 0,
        });
        let speculative = nanos(started);
        anyhow::ensure!(
            heads.validate(SessionEpoch::new(1), &state)? == HeadValidation::Correct,
            "scenario B1 frontier was not head-valid"
        );
        black_box(grid);
        let correct = nanos(started);
        timings.record(speculative, correct, nanos(started))?;
    }
    Ok(timings)
}

fn scenario_full_read(iterations: u64, path: &Path) -> Result<Timings> {
    let mut timings = Timings::new()?;
    for _ in 0..iterations {
        let started = Instant::now();
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let text = String::from_utf8(bytes).context("startup fixture must be UTF-8")?;
        let mut layout = ViewportLayout::new(120, 40);
        let grid = layout.desired_grid(&EngineFrame {
            text: text.into_boxed_str(),
            cursor_byte: 0,
        });
        let speculative = nanos(started);
        black_box(grid);
        let correct = nanos(started);
        timings.record(speculative, correct, nanos(started))?;
    }
    Ok(timings)
}

fn scenario_c(iterations: u64, directory: &Path, text: &str) -> Result<Timings> {
    let session_id = SessionId::new(41);
    {
        let mut authority =
            SessionAuthority::open(SessionJournal::in_directory(directory), session_id)?;
        authority.register_document(DocumentId::new(1), text, ClientId::new(1))?;
    }
    let mut timings = Timings::new()?;
    for _ in 0..iterations {
        let started = Instant::now();
        let authority =
            SessionAuthority::open(SessionJournal::in_directory(directory), session_id)?;
        let document = authority
            .document(DocumentId::new(1))
            .context("recovered startup document")?;
        let mut layout = ViewportLayout::new(120, 40);
        let grid = layout.desired_grid(&EngineFrame {
            text: document.text.clone().into_boxed_str(),
            cursor_byte: 0,
        });
        let speculative = nanos(started);
        black_box(grid);
        let correct = nanos(started);
        timings.record(speculative, correct, nanos(started))?;
    }
    Ok(timings)
}

fn scenario_d(iterations: u64, path: &Path) -> Result<Timings> {
    let executable = env::current_exe().context("locate startup harness executable")?;
    let mut timings = Timings::new()?;
    for _ in 0..iterations.min(10) {
        let started = Instant::now();
        let status = Command::new(&executable)
            .arg("--probe-file")
            .arg(path)
            .status()
            .context("spawn cold-process probe")?;
        anyhow::ensure!(status.success(), "cold-process probe failed");
        let elapsed = nanos(started);
        timings.record(elapsed, elapsed, elapsed)?;
    }
    Ok(timings)
}

fn probe(path: &Path) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("probe read {}", path.display()))?;
    let text = String::from_utf8(bytes).context("probe file must be UTF-8")?;
    let mut layout = ViewportLayout::new(120, 40);
    black_box(layout.desired_grid(&EngineFrame {
        text: text.into_boxed_str(),
        cursor_byte: 0,
    }));
    Ok(())
}

fn nanos(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos())
        .unwrap_or(u64::MAX)
        .max(1)
}

#[cfg(unix)]
fn read_at(file: &File, offset: u64, length: usize) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut bytes = vec![0; length];
    let count = file.read_at(&mut bytes, offset)?;
    bytes.truncate(count);
    Ok(bytes)
}

#[cfg(windows)]
fn read_at(file: &File, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
    use std::io::{Seek, SeekFrom};

    use std::os::windows::fs::FileExt;
    let mut bytes = vec![0; length];
    let count = file.seek_read(&mut bytes, offset)?;
    bytes.truncate(count);
    Ok(bytes)
}

#[cfg(not(any(unix, windows)))]
fn read_at(file: &File, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; length];
    let count = file.read(&mut bytes)?;
    bytes.truncate(count);
    Ok(bytes)
}

fn main() -> Result<()> {
    let arguments = arguments()?;
    if let Some(path) = arguments.probe_file {
        return probe(&path);
    }
    let cpu_pinned = arguments.cpu.is_some_and(pin_cpu);
    validate_gate_environment(&arguments, cpu_pinned)?;
    if let Some(path) = &arguments.b3_path {
        anyhow::ensure!(
            is_network_or_fuse_path(path),
            "--b3-path must resolve to a mounted network or FUSE filesystem"
        );
    }
    let fixture = fixture_text();
    let directory = tempdir().context("startup fixture directory")?;
    let fixture_path = directory.path().join("fixture.rs");
    fs::write(&fixture_path, &fixture)?;
    let known_offset = u64::try_from("fn measured_startup() { let β = \"viewport\"; }\n".len())?
        .saturating_mul(12_000);

    let (_head_writer, head_reader) = shared_heads(directory.path())?;

    let scenario_a = scenario_a(
        arguments.iterations,
        directory.path(),
        &fixture,
        &head_reader,
    )?;
    let scenario_b1 = scenario_b1(
        arguments.iterations,
        &fixture_path,
        known_offset,
        &head_reader,
    )?;
    let b2_iterations = arguments.iterations.min(25);
    let scenario_b2 = scenario_full_read(b2_iterations, &fixture_path)?;
    let scenario_c = scenario_c(
        arguments.iterations.min(100),
        &directory.path().join("session"),
        &fixture,
    )?;
    let scenario_d = scenario_d(arguments.iterations, &fixture_path)?;
    let scenario_b3 = arguments
        .b3_path
        .as_deref()
        .map(|path| scenario_full_read(b2_iterations, path))
        .transpose()?;

    let a_pass = scenario_a.correct.value_at_quantile(0.99) <= GATE_NANOS;
    let b1_pass = scenario_b1.correct.value_at_quantile(0.99) <= GATE_NANOS;
    let report = json!({
        "schema": 1,
        "gate_nanos": GATE_NANOS,
        "cpu_requested": arguments.cpu,
        "cpu_pinned": cpu_pinned,
        "runner_contract": {
            "bare_metal_declared": env::var("WREN_BARE_METAL").as_deref() == Ok("1"),
            "gate_authoritative": arguments.gate,
            "kernel_cache_state_runner_controlled": true,
        },
        "scenario_a": {
            "contract": "warm session + warm published viewport + local head validation",
            "gated": true,
            "passed": a_pass,
            "metrics": scenario_a.report(),
        },
        "scenario_b1": {
            "contract": "unopened page-cache-hot local file + known byte range + head validation",
            "gated": true,
            "passed": b1_pass,
            "metrics": scenario_b1.report(),
        },
        "scenario_b2": {
            "contract": "unopened local file, cache state uncontrolled by portable harness",
            "gated": false,
            "metrics": scenario_b2.report(),
        },
        "scenario_b3": match scenario_b3 {
            Some(metrics) => json!({
                "contract": "caller-supplied network/FUSE path",
                "gated": false,
                "available": true,
                "metrics": metrics.report(),
            }),
            None => json!({
                "contract": "network/FUSE path requires --b3-path",
                "gated": false,
                "available": false,
            }),
        },
        "scenario_c": {
            "contract": "cold session authority + warm filesystem",
            "gated": false,
            "metrics": scenario_c.report(),
        },
        "scenario_d": {
            "contract": "fresh process + full file read; binary/fs cache eviction is runner-controlled",
            "gated": false,
            "metrics": scenario_d.report(),
        },
    });
    let rendered = serde_json::to_string_pretty(&report)?;
    if let Some(path) = arguments.output {
        fs::write(&path, format!("{rendered}\n"))
            .with_context(|| format!("write {}", path.display()))?;
    }
    println!("{rendered}");
    if arguments.gate {
        anyhow::ensure!(a_pass, "scenario A p99 exceeded 5ms");
        anyhow::ensure!(b1_pass, "scenario B1 p99 exceeded 5ms");
    }
    Ok(())
}
