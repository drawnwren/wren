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
use wren_benchmark_support::{
    CommonArguments, bare_metal_declared, elapsed_nanos, emit_report, histogram, percentiles, pin_requested_cpu, require_bare_metal_cpu, ten_percent_cut,
};
use wren_client_state::{ClientViewStateStore, PublishedViewport};
use wren_engine::EngineFrame;
use wren_session::{SessionAuthority, SessionJournal};
use wren_shmem::{SharedDocumentHeadReader, SharedDocumentHeadWriter};
use wren_types::{
    ClientId, ConfigGeneration, DocumentHead, DocumentId, DocumentRevision, HeadValidation, PublishedViewportKey, ResumeViewState, SelRange, SelectionSet,
    SessionEpoch, SessionId, ViewId,
};
use wren_view::ViewportLayout;

const SCENARIO_A_P99_GATE_NANOS: u64 = ten_percent_cut(3_098_418);
const SCENARIO_B1_P99_GATE_NANOS: u64 = ten_percent_cut(385_688);
const VIEWPORT_BYTES: usize = 64 * 1024;

#[derive(Debug)]
struct Arguments {
    common: CommonArguments,
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
        Ok(Self { speculative: histogram()?, correct: histogram()?, interactive: histogram()? })
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
            "time_to_speculative_frame": percentiles(&self.speculative),
            "time_to_correct_frame": percentiles(&self.correct),
            "time_to_interactive": percentiles(&self.interactive),
        })
    }
}

fn arguments() -> Result<Arguments> {
    let (mut b3_path, mut probe_file) = (None, None);
    let common = CommonArguments::parse_with(1_000, |argument, cursor| {
        match argument {
            "--b3-path" => b3_path = Some(cursor.path(argument)?),
            "--probe-file" => probe_file = Some(cursor.path(argument)?),
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    Ok(Arguments { common, b3_path, probe_file })
}

fn validate_gate_environment(arguments: &Arguments, pinned: bool) -> Result<()> {
    require_bare_metal_cpu(arguments.common.gate, arguments.common.cpu, pinned, "--gate")
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
        let output = Command::new("stat").args(["-f", "%T"]).arg(canonical).output().ok()?;
        return output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    #[allow(unreachable_code)]
    None
}

fn is_network_or_fuse_path(path: &Path) -> bool {
    mounted_filesystem_type(path).is_some_and(|filesystem| {
        let filesystem = filesystem.to_ascii_lowercase();
        filesystem.starts_with("fuse") || matches!(filesystem.as_str(), "nfs" | "nfs4" | "cifs" | "smbfs" | "sshfs" | "9p" | "afs" | "davfs")
    })
}

fn resume_state() -> ResumeViewState {
    ResumeViewState {
        client_id: ClientId::new(1),
        view_id: ViewId::new(1),
        document_id: DocumentId::new(1),
        document_revision: DocumentRevision::new(7),
        selections: SelectionSet { primary: 0, ranges: vec![SelRange { anchor: 32, head: 32 }] },
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

fn scenario_a(iterations: u64, directory: &Path, text: &str, heads: &SharedDocumentHeadReader) -> Result<Timings> {
    let state = resume_state();
    let frame = EngineFrame::new(text, state.selections.ranges[0].head);
    let mut layout = ViewportLayout::new(state.columns, state.rows);
    layout.top_line = state.top_line;
    let published =
        PublishedViewport { session_epoch: SessionEpoch::new(1), document_id: DocumentId::new(1), key: published_key(), grid: layout.desired_grid(&frame) };
    let store = ClientViewStateStore::new(directory.join("startup-client-state"));
    store.save_resume(&state)?;
    store.save_viewport(&published)?;
    let mut timings = Timings::new()?;
    for _ in 0..iterations {
        let started = Instant::now();
        let cached = store.load_viewport(ClientId::new(1), ViewId::new(1), DocumentId::new(1))?.context("scenario A published viewport")?;
        anyhow::ensure!(cached.key == published_key(), "scenario A cache key drifted");
        let speculative_grid = Arc::new(cached.grid);
        black_box(&speculative_grid);
        let speculative = elapsed_nanos(started);
        anyhow::ensure!(heads.validate(SessionEpoch::new(1), &state)? == HeadValidation::Correct, "scenario A published viewport was not head-valid");
        let correct = elapsed_nanos(started);
        black_box((speculative_grid.cursor, state.selections.primary));
        timings.record(speculative, correct, elapsed_nanos(started))?;
    }
    Ok(timings)
}

fn scenario_b1(iterations: u64, path: &Path, known_offset: u64, heads: &SharedDocumentHeadReader) -> Result<Timings> {
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
        let grid = layout.desired_grid(&EngineFrame::new(text, 0));
        let speculative = elapsed_nanos(started);
        anyhow::ensure!(heads.validate(SessionEpoch::new(1), &state)? == HeadValidation::Correct, "scenario B1 frontier was not head-valid");
        black_box(grid);
        let correct = elapsed_nanos(started);
        timings.record(speculative, correct, elapsed_nanos(started))?;
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
        let grid = layout.desired_grid(&EngineFrame::new(text, 0));
        let speculative = elapsed_nanos(started);
        black_box(grid);
        let correct = elapsed_nanos(started);
        timings.record(speculative, correct, elapsed_nanos(started))?;
    }
    Ok(timings)
}

fn scenario_c(iterations: u64, directory: &Path, text: &str) -> Result<Timings> {
    let session_id = SessionId::new(41);
    {
        let mut authority = SessionAuthority::open(SessionJournal::in_directory(directory), session_id)?;
        authority.register_document(DocumentId::new(1), text, ClientId::new(1))?;
    }
    let mut timings = Timings::new()?;
    for _ in 0..iterations {
        let started = Instant::now();
        let authority = SessionAuthority::open(SessionJournal::in_directory(directory), session_id)?;
        let document = authority.document(DocumentId::new(1)).context("recovered startup document")?;
        let mut layout = ViewportLayout::new(120, 40);
        let grid = layout.desired_grid(&EngineFrame::new(document.text(), 0));
        let speculative = elapsed_nanos(started);
        black_box(grid);
        let correct = elapsed_nanos(started);
        timings.record(speculative, correct, elapsed_nanos(started))?;
    }
    Ok(timings)
}

fn scenario_d(iterations: u64, path: &Path) -> Result<Timings> {
    let executable = env::current_exe().context("locate startup harness executable")?;
    let mut timings = Timings::new()?;
    for _ in 0..iterations.min(10) {
        let started = Instant::now();
        let status = Command::new(&executable).arg("--probe-file").arg(path).status().context("spawn cold-process probe")?;
        anyhow::ensure!(status.success(), "cold-process probe failed");
        let elapsed = elapsed_nanos(started);
        timings.record(elapsed, elapsed, elapsed)?;
    }
    Ok(timings)
}

fn probe(path: &Path) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("probe read {}", path.display()))?;
    let text = String::from_utf8(bytes).context("probe file must be UTF-8")?;
    let mut layout = ViewportLayout::new(120, 40);
    black_box(layout.desired_grid(&EngineFrame::new(text, 0)));
    Ok(())
}

fn read_at(file: &File, offset: u64, length: usize) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut bytes = vec![0; length];
    let count = file.read_at(&mut bytes, offset)?;
    bytes.truncate(count);
    Ok(bytes)
}

struct StartupMetrics {
    scenario_a: Timings,
    scenario_b1: Timings,
    scenario_b2: Timings,
    scenario_b3: Option<Timings>,
    scenario_c: Timings,
    scenario_d: Timings,
}

fn startup_metrics(arguments: &Arguments) -> Result<StartupMetrics> {
    if let Some(path) = &arguments.b3_path {
        anyhow::ensure!(is_network_or_fuse_path(path), "--b3-path must resolve to a mounted network or FUSE filesystem");
    }
    let fixture = fixture_text();
    let directory = tempdir().context("startup fixture directory")?;
    let fixture_path = directory.path().join("fixture.rs");
    fs::write(&fixture_path, &fixture)?;
    let known_offset = u64::try_from("fn measured_startup() { let β = \"viewport\"; }\n".len())?.saturating_mul(12_000);

    let (_head_writer, head_reader) = shared_heads(directory.path())?;

    let scenario_a = scenario_a(arguments.common.iterations, directory.path(), &fixture, &head_reader)?;
    let scenario_b1 = scenario_b1(arguments.common.iterations, &fixture_path, known_offset, &head_reader)?;
    let b2_iterations = arguments.common.iterations.min(25);
    let scenario_b2 = scenario_full_read(b2_iterations, &fixture_path)?;
    let scenario_c = scenario_c(arguments.common.iterations.min(100), &directory.path().join("session"), &fixture)?;
    let scenario_d = scenario_d(arguments.common.iterations, &fixture_path)?;
    let scenario_b3 = arguments.b3_path.as_deref().map(|path| scenario_full_read(b2_iterations, path)).transpose()?;
    Ok(StartupMetrics { scenario_a, scenario_b1, scenario_b2, scenario_b3, scenario_c, scenario_d })
}

#[derive(Debug, Clone, Copy)]
struct StartupGates {
    scenario_a: bool,
    scenario_b1: bool,
}

impl StartupGates {
    fn evaluate(metrics: &StartupMetrics) -> Self {
        Self {
            scenario_a: metrics.scenario_a.correct.value_at_quantile(0.99) < SCENARIO_A_P99_GATE_NANOS,
            scenario_b1: metrics.scenario_b1.correct.value_at_quantile(0.99) < SCENARIO_B1_P99_GATE_NANOS,
        }
    }

    fn enforce(self) -> Result<()> {
        anyhow::ensure!(self.scenario_a, "scenario A p99 exceeded its tightened gate");
        anyhow::ensure!(self.scenario_b1, "scenario B1 p99 exceeded its tightened gate");
        Ok(())
    }
}

fn report(arguments: &Arguments, cpu_pinned: bool, metrics: &StartupMetrics, gates: StartupGates) -> Value {
    json!({
        "schema": 2,
        "cpu_requested": arguments.common.cpu,
        "cpu_pinned": cpu_pinned,
        "runner_contract": {
            "bare_metal_declared": bare_metal_declared(),
            "gate_authoritative": arguments.common.gate,
            "kernel_cache_state_runner_controlled": true,
        },
        "scenario_a": {
            "contract": "warm session + warm published viewport + local head validation",
            "gated": true,
            "p99_gate_nanos": SCENARIO_A_P99_GATE_NANOS,
            "passed": gates.scenario_a,
            "metrics": metrics.scenario_a.report(),
        },
        "scenario_b1": {
            "contract": "unopened page-cache-hot local file + known byte range + head validation",
            "gated": true,
            "p99_gate_nanos": SCENARIO_B1_P99_GATE_NANOS,
            "passed": gates.scenario_b1,
            "metrics": metrics.scenario_b1.report(),
        },
        "scenario_b2": {
            "contract": "unopened local file, cache state uncontrolled by portable harness",
            "gated": false,
            "metrics": metrics.scenario_b2.report(),
        },
        "scenario_b3": match &metrics.scenario_b3 {
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
            "metrics": metrics.scenario_c.report(),
        },
        "scenario_d": {
            "contract": "fresh process + full file read; binary/fs cache eviction is runner-controlled",
            "gated": false,
            "metrics": metrics.scenario_d.report(),
        },
    })
}

fn main() -> Result<()> {
    let arguments = arguments()?;
    if let Some(path) = &arguments.probe_file {
        return probe(path);
    }
    let cpu_pinned = pin_requested_cpu(arguments.common.cpu);
    validate_gate_environment(&arguments, cpu_pinned)?;
    let metrics = startup_metrics(&arguments)?;
    let gates = StartupGates::evaluate(&metrics);
    emit_report(&report(&arguments, cpu_pinned, &metrics, gates), arguments.common.output.as_deref())?;
    if arguments.common.gate {
        gates.enforce()?;
    }
    Ok(())
}
