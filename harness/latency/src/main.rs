use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, Cursor, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hdrhistogram::Histogram;
use serde::Deserialize;
use serde_json::{Value, json};
use wren_benchmark_support::{
    ArgumentCursor, CommonArguments, bare_metal_declared, distribution, elapsed_nanos, emit_report,
    histogram, pin_requested_cpu, require_bare_metal_cpu, ten_percent_cut,
};
use wren_command::{TaskResult, TaskRunner};
use wren_engine::Editor;
use wren_grammar::KeyEvent;
use wren_presenter::{PresentationObserver, Presenter, PresenterStats};
use wren_provider::{CompletionCandidate, CompletionSession};
use wren_term::{TerminaBackend, TerminalBackend, TerminalError};
use wren_text::{DefaultText, TextStore};
use wren_tui::ProductionLatencyReport;
use wren_types::{BufferId, CommandTask, CommandTaskId, DocumentId, Effects};
use wren_view::{ClientViewModel, TerminalPatch, ViewportLayout};

const REALTIME_AGGREGATE_P99_GATE_NANOS: u64 = ten_percent_cut(86_283);
const REALTIME_MAX_GATE_NANOS: u64 = 100_000;
const LARGE_FILE_OPEN_MAX_GATE_NANOS: u64 = 500_000_000;
const LARGE_FILE_FIRST_FRAME_MAX_GATE_NANOS: u64 = 5_000_000;
const LARGE_FILE_OPEN_TO_TERMINAL_MAX_GATE_NANOS: u64 = 600_000_000;
const TASK_YIELD_P99_GATE_NANOS: u64 = ten_percent_cut(69_176);
const TASK_YIELD_MAX_GATE_NANOS: u64 = ten_percent_cut(71_077);
const TERMINAL_WRITE_P99_GATE_NANOS: u64 = ten_percent_cut(115_141);

type Arguments = CommonArguments;

fn arguments() -> Result<Arguments> {
    let mut arguments = Arguments::new(10_000);
    let mut cursor = ArgumentCursor::from_env();
    while let Some(argument) = cursor.next() {
        if !arguments.consume(&argument, &mut cursor)? {
            anyhow::bail!("unknown argument: {argument}");
        }
    }
    arguments.validate()?;
    Ok(arguments)
}

fn validate_gate_environment(arguments: &Arguments, pinned: bool) -> Result<()> {
    require_bare_metal_cpu(arguments.gate, arguments.cpu, pinned, "--gate")
}

fn run_production_probe_child(arguments: &[String]) -> Result<()> {
    let iterations = arguments
        .first()
        .context("production probe child requires an iteration count")?
        .parse::<u64>()
        .context("invalid production probe iteration count")?;
    let report = wren_tui::run_production_latency_probe(iterations)?;
    serde_json::to_writer(io::stdout().lock(), &report)?;
    Ok(())
}

fn production_probe(iterations: u64) -> Result<ProductionLatencyReport> {
    let isolated = tempfile::tempdir().context("create isolated production probe home")?;
    let executable = env::current_exe().context("locate latency harness executable")?;
    let output = Command::new(executable)
        .arg("--production-probe-child")
        .arg(iterations.to_string())
        .current_dir(isolated.path())
        .env("HOME", isolated.path())
        .env("XDG_STATE_HOME", isolated.path().join("state"))
        .env("XDG_DATA_HOME", isolated.path().join("data"))
        .env("XDG_CONFIG_HOME", isolated.path().join("config"))
        .output()
        .context("run isolated full-production latency probe")?;
    anyhow::ensure!(
        output.status.success(),
        "full-production latency probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: ProductionLatencyReport = serde_json::from_slice(&output.stdout)
        .context("decode full-production latency probe report")?;
    anyhow::ensure!(report.schema == 1, "unsupported production probe schema");
    anyhow::ensure!(
        !report.samples.is_empty(),
        "production probe returned no samples"
    );
    Ok(report)
}

#[derive(Debug, Deserialize)]
struct KeyToPhotonMeasurement {
    schema: u64,
    rig_id: Box<str>,
    samples: u64,
    baseline_p99_nanos: u64,
    measured_p99_nanos: u64,
}

fn key_to_photon_report() -> (Value, bool) {
    key_to_photon_report_from_path(env::var_os("WREN_KEY_TO_PHOTON_JSON").map(PathBuf::from))
}

fn key_to_photon_report_from_path(path: Option<PathBuf>) -> (Value, bool) {
    let Some(path) = path else {
        return (
            json!({
                "available": false,
                "hard_gate": true,
                "passed": false,
                "reason": "hard gate requires WREN_KEY_TO_PHOTON_JSON from the physical rig",
            }),
            false,
        );
    };
    match fs::read_to_string(&path)
        .ok()
        .and_then(|source| serde_json::from_str::<KeyToPhotonMeasurement>(&source).ok())
    {
        Some(measurement)
            if measurement.schema == 1
                && measurement.samples > 0
                && measurement.baseline_p99_nanos > 0 =>
        {
            let gate_nanos = ten_percent_cut(ten_percent_cut(measurement.baseline_p99_nanos));
            let passed = measurement.measured_p99_nanos < gate_nanos;
            (
                json!({
                    "available": true,
                    "hard_gate": true,
                    "passed": passed,
                    "source": path,
                    "rig_id": measurement.rig_id,
                    "samples": measurement.samples,
                    "baseline_p99_nanos": measurement.baseline_p99_nanos,
                    "p99_gate_nanos": gate_nanos,
                    "measured_p99_nanos": measurement.measured_p99_nanos,
                }),
                passed,
            )
        }
        _ => (
            json!({
                "available": false,
                "hard_gate": true,
                "passed": false,
                "source": path,
                "reason": "hardware-rig report must be schema 1 with rig_id, samples, baseline_p99_nanos, and measured_p99_nanos",
            }),
            false,
        ),
    }
}

#[derive(Default)]
struct CountingWriter {
    bytes: u64,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        std::hint::black_box(buffer);
        self.bytes = self
            .bytes
            .saturating_add(u64::try_from(buffer.len()).unwrap_or(u64::MAX));
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct MeasuringBackend {
    terminal: TerminaBackend<CountingWriter>,
    writes: u64,
    patches: u64,
}

impl MeasuringBackend {
    fn new() -> Result<Self> {
        Ok(Self {
            terminal: TerminaBackend::new(CountingWriter::default(), 120, 40)?,
            writes: 0,
            patches: 0,
        })
    }
}

impl TerminalBackend for MeasuringBackend {
    type Error = TerminalError;

    fn submit(&mut self, patches: &[TerminalPatch]) -> Result<(), Self::Error> {
        self.writes = self.writes.saturating_add(1);
        self.patches = self
            .patches
            .saturating_add(u64::try_from(patches.len()).unwrap_or(u64::MAX));
        self.terminal.submit(patches)
    }
}

struct TerminalLatency {
    starts: Mutex<BTreeMap<u64, Instant>>,
    histogram: Mutex<Histogram<u64>>,
}

impl TerminalLatency {
    fn new() -> Result<Self> {
        Ok(Self {
            starts: Mutex::new(BTreeMap::new()),
            histogram: Mutex::new(histogram()?),
        })
    }

    fn begin(&self, epoch: u64, started: Instant) {
        if let Ok(mut starts) = self.starts.lock() {
            starts.insert(epoch, started);
        }
    }

    fn completed(&self, epoch: u64) {
        let started = self
            .starts
            .lock()
            .ok()
            .and_then(|mut starts| starts.remove(&epoch));
        if let Some(started) = started
            && let Ok(mut histogram) = self.histogram.lock()
        {
            let _ = histogram.record(elapsed_nanos(started));
        }
    }

    fn measurements(&self) -> Result<Histogram<u64>> {
        self.histogram
            .lock()
            .map(|histogram| histogram.clone())
            .map_err(|_| anyhow::anyhow!("terminal histogram lock poisoned"))
    }
}

fn wait_result(runner: &TaskRunner) -> Result<TaskResult> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = runner.try_result()? {
            return Ok(result);
        }
        anyhow::ensure!(Instant::now() < deadline, "TaskCommand benchmark timed out");
        thread::yield_now();
    }
}

fn task_metrics(samples: u64) -> Result<(Histogram<u64>, Histogram<u64>)> {
    let runner = TaskRunner::new(1, 2)?;
    let mut yields = histogram()?;
    let mut cancellations = histogram()?;
    for sample in 0..samples.max(1) {
        runner.submit(
            CommandTask {
                task_id: CommandTaskId::new(sample.saturating_mul(2).saturating_add(1)),
                affected_documents: vec![DocumentId::new(1)],
                label: "yield benchmark".into(),
            },
            |context| {
                for _ in 0..1_024 {
                    for value in 0..128_u64 {
                        std::hint::black_box(value.wrapping_mul(17));
                    }
                    context.checkpoint()?;
                }
                Ok(Effects::default())
            },
        )?;
        let result = wait_result(&runner)?;
        anyhow::ensure!(result.outcome.is_ok(), "yield benchmark task failed");
        yields.record(
            u64::try_from(result.max_checkpoint_gap.as_nanos())
                .unwrap_or(u64::MAX)
                .max(1),
        )?;

        let (started_sender, started_receiver) = mpsc::channel();
        let cancellation = runner.submit(
            CommandTask {
                task_id: CommandTaskId::new(sample.saturating_mul(2).saturating_add(2)),
                affected_documents: vec![DocumentId::new(1)],
                label: "cancellation benchmark".into(),
            },
            move |context| {
                started_sender.send(()).map_err(|_| {
                    wren_command::TaskFailure::Failed("start channel closed".into())
                })?;
                loop {
                    for value in 0..128_u64 {
                        std::hint::black_box(value.wrapping_mul(31));
                    }
                    context.checkpoint()?;
                }
            },
        )?;
        started_receiver
            .recv_timeout(Duration::from_secs(2))
            .context("cancellation task did not start")?;
        let cancelled_at = Instant::now();
        cancellation.cancel();
        let result = wait_result(&runner)?;
        anyhow::ensure!(
            matches!(result.outcome, Err(wren_command::TaskFailure::Cancelled)),
            "cancellation benchmark task did not cancel"
        );
        cancellations.record(elapsed_nanos(cancelled_at))?;
    }
    Ok((yields, cancellations))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RealtimeCase {
    InsertCharacter,
    DeleteCharacter,
    LocalMotion,
    BoundedOperator,
    SelectionChange,
    ViewportNavigation,
    CompletionAcceptance,
}

impl RealtimeCase {
    const ALL: [Self; 7] = [
        Self::InsertCharacter,
        Self::DeleteCharacter,
        Self::LocalMotion,
        Self::BoundedOperator,
        Self::SelectionChange,
        Self::ViewportNavigation,
        Self::CompletionAcceptance,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::InsertCharacter => "insert_character",
            Self::DeleteCharacter => "delete_character",
            Self::LocalMotion => "local_motion",
            Self::BoundedOperator => "bounded_operator",
            Self::SelectionChange => "selection_change",
            Self::ViewportNavigation => "viewport_navigation",
            Self::CompletionAcceptance => "completion_acceptance",
        }
    }

    const fn p99_gate_nanos(self) -> u64 {
        match self {
            Self::BoundedOperator => ten_percent_cut(86_629),
            Self::CompletionAcceptance => ten_percent_cut(86_283),
            Self::DeleteCharacter => ten_percent_cut(87_263),
            Self::InsertCharacter => ten_percent_cut(98_667),
            Self::LocalMotion => ten_percent_cut(87_378),
            Self::SelectionChange => ten_percent_cut(84_440),
            Self::ViewportNavigation => ten_percent_cut(73_093),
        }
    }
}

fn fixture_editor() -> Result<Editor<DefaultText>> {
    let source = (0..256)
        .map(|line| format!("fn item_{line}() {{ let value = {line}; }}\n"))
        .collect::<String>();
    let text = DefaultText::from_reader(Cursor::new(source)).context("build realtime fixture")?;
    Ok(Editor::new(text))
}

struct PreparedRealtimeCase {
    editor: Editor<DefaultText>,
    layout: ViewportLayout,
    model: ClientViewModel,
    buffer_id: BufferId,
    completion: Option<CompletionSession>,
    baseline: Arc<wren_view::DesiredGrid>,
}

fn prepare_realtime_case(case: RealtimeCase) -> Result<PreparedRealtimeCase> {
    let mut editor = fixture_editor()?;
    let completion = match case {
        RealtimeCase::InsertCharacter => {
            editor
                .handle_key(KeyEvent::character('i'))
                .context("prepare insert mode")?;
            None
        }
        RealtimeCase::BoundedOperator => {
            editor
                .handle_key(KeyEvent::character('d'))
                .context("prepare operator-pending mode")?;
            None
        }
        RealtimeCase::SelectionChange => {
            editor
                .handle_key(KeyEvent::character('v'))
                .context("prepare visual mode")?;
            None
        }
        RealtimeCase::CompletionAcceptance => Some(CompletionSession::merge(
            editor.revision(),
            0..2,
            vec![CompletionCandidate {
                label: "function".into(),
                insert: "function".into(),
                source: "benchmark".into(),
                detail: "fixed completion".into(),
                documentation: "".into(),
                replace: None,
                snippet: None,
            }],
            Vec::new(),
        )),
        RealtimeCase::DeleteCharacter
        | RealtimeCase::LocalMotion
        | RealtimeCase::ViewportNavigation => None,
    };
    let mut layout = ViewportLayout::new(120, 40);
    layout.configure_dotfile_profile();
    let model = ClientViewModel::new(DocumentId::new(1), "benchmark.rs");
    let buffer_id = model.active_buffer();
    // Physical input begins from an already presented editor. Prime retained
    // production workspace state before the clock starts instead of
    // benchmarking a cold first frame for every key.
    let frames = [(buffer_id, editor.frame())];
    let baseline = Arc::new(layout.desired_workspace_grid(&model, &frames, "NORMAL", None));
    Ok(PreparedRealtimeCase {
        editor,
        layout,
        model,
        buffer_id,
        completion,
        baseline,
    })
}

fn run_realtime_case(case: RealtimeCase, prepared: &mut PreparedRealtimeCase) -> Result<()> {
    let editor = &mut prepared.editor;
    match case {
        RealtimeCase::InsertCharacter => {
            editor
                .handle_key(KeyEvent::character('x'))
                .context("insert character")?;
        }
        RealtimeCase::DeleteCharacter => {
            editor
                .handle_key(KeyEvent::character('x'))
                .context("delete character")?;
        }
        RealtimeCase::LocalMotion => {
            editor
                .handle_key(KeyEvent::character('l'))
                .context("local motion")?;
        }
        RealtimeCase::BoundedOperator => {
            editor
                .handle_key(KeyEvent::character('w'))
                .context("complete bounded operator")?;
        }
        RealtimeCase::SelectionChange => {
            editor
                .handle_key(KeyEvent::character('l'))
                .context("change visual selection")?;
        }
        RealtimeCase::ViewportNavigation => {
            let top_line = prepared.model.active_window().top_line;
            prepared.model.active_window_mut().top_line = top_line.saturating_add(1);
        }
        RealtimeCase::CompletionAcceptance => {
            let session = prepared
                .completion
                .as_ref()
                .context("completion scenario was not prepared")?;
            if let Some(transaction) = session
                .accept(editor.revision(), 0)
                .context("accept fixed completion")?
            {
                editor
                    .apply_transaction(transaction)
                    .context("apply completion transaction")?;
            }
        }
    }
    Ok(())
}

struct RealtimeMetrics {
    commit: Histogram<u64>,
    frame_snapshot: Histogram<u64>,
    frame_snapshot_max: u64,
    grid_build: Histogram<u64>,
    grid_build_max: u64,
    desired_grid: Histogram<u64>,
    desired_grid_max: u64,
    cases: BTreeMap<RealtimeCase, Histogram<u64>>,
    case_maxima: BTreeMap<RealtimeCase, u64>,
    terminal: Histogram<u64>,
    presenter: PresenterStats,
    backend_writes: u64,
    terminal_patches: u64,
}

struct ProductionMetrics {
    report: ProductionLatencyReport,
    input: Histogram<u64>,
    app_poll: Histogram<u64>,
    provider_schedule: Histogram<u64>,
    frame: Histogram<u64>,
    desired: Histogram<u64>,
    desired_max: u64,
    terminal: Histogram<u64>,
    cases: BTreeMap<Box<str>, Histogram<u64>>,
    case_maxima: BTreeMap<Box<str>, u64>,
}

fn production_metrics(iterations: u64) -> Result<ProductionMetrics> {
    let report = production_probe(iterations)?;
    let mut input = histogram()?;
    let mut app_poll = histogram()?;
    let mut provider_schedule = histogram()?;
    let mut frame = histogram()?;
    let mut desired = histogram()?;
    let mut desired_max = 0;
    let mut terminal = histogram()?;
    let mut cases = BTreeMap::<Box<str>, Histogram<u64>>::new();
    let mut case_maxima = BTreeMap::<Box<str>, u64>::new();
    for sample in &report.samples {
        input.record(sample.input_nanos)?;
        app_poll.record(sample.app_poll_nanos)?;
        provider_schedule.record(sample.provider_schedule_nanos)?;
        frame.record(sample.desired_frame_nanos)?;
        desired.record(sample.input_to_desired_frame_nanos)?;
        desired_max = desired_max.max(sample.input_to_desired_frame_nanos);
        terminal.record(sample.input_to_terminal_write_nanos)?;
        cases
            .entry(sample.scenario.clone())
            .or_insert(histogram()?)
            .record(sample.input_to_desired_frame_nanos)?;
        case_maxima
            .entry(sample.scenario.clone())
            .and_modify(|maximum| *maximum = (*maximum).max(sample.input_to_desired_frame_nanos))
            .or_insert(sample.input_to_desired_frame_nanos);
    }
    Ok(ProductionMetrics {
        report,
        input,
        app_poll,
        provider_schedule,
        frame,
        desired,
        desired_max,
        terminal,
        cases,
        case_maxima,
    })
}

fn realtime_metrics(iterations: u64) -> Result<RealtimeMetrics> {
    let backend = Arc::new(Mutex::new(MeasuringBackend::new()?));
    let terminal_latency = Arc::new(TerminalLatency::new()?);
    let observer_latency = Arc::clone(&terminal_latency);
    let (presented_sender, presented_receiver) = mpsc::sync_channel(1);
    let observer: PresentationObserver = Arc::new(move |epoch| {
        observer_latency.completed(epoch);
        let _ = presented_sender.send(epoch);
    });
    let presenter = Presenter::start_observed(Arc::clone(&backend), Some(observer))?;

    let mut commit = histogram()?;
    let mut frame_snapshot = histogram()?;
    let mut frame_snapshot_max = 0;
    let mut grid_build = histogram()?;
    let mut grid_build_max = 0;
    let mut desired_grid = histogram()?;
    let mut desired_grid_max = 0;
    let mut cases = RealtimeCase::ALL
        .iter()
        .map(|case| Ok((*case, histogram()?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut case_maxima = RealtimeCase::ALL
        .iter()
        .map(|case| (*case, 0))
        .collect::<BTreeMap<_, _>>();
    for iteration in 0..iterations {
        let case = RealtimeCase::ALL[(iteration as usize) % RealtimeCase::ALL.len()];
        let mut prepared = prepare_realtime_case(case)?;
        let baseline_epoch = prepared.baseline.epoch;
        presenter.publish(Arc::clone(&prepared.baseline))?;
        let presented_epoch = presented_receiver
            .recv_timeout(Duration::from_secs(1))
            .context("presenter did not complete the scenario baseline")?;
        anyhow::ensure!(
            presented_epoch == baseline_epoch,
            "presenter completed an unexpected scenario baseline"
        );
        let started = Instant::now();
        run_realtime_case(case, &mut prepared)?;
        let committed = Instant::now();
        let frame = prepared.editor.frame();
        let frame_ready = Instant::now();
        let frames = [(prepared.buffer_id, frame)];
        let desired = Arc::new(prepared.layout.desired_workspace_grid(
            &prepared.model,
            &frames,
            "NORMAL",
            None,
        ));
        let desired_ready = Instant::now();
        let commit_elapsed = duration_nanos(committed.duration_since(started));
        let frame_elapsed = duration_nanos(frame_ready.duration_since(committed));
        let grid_elapsed = duration_nanos(desired_ready.duration_since(frame_ready));
        let desired_elapsed = duration_nanos(desired_ready.duration_since(started));
        commit.record(commit_elapsed)?;
        frame_snapshot.record(frame_elapsed)?;
        frame_snapshot_max = frame_snapshot_max.max(frame_elapsed);
        grid_build.record(grid_elapsed)?;
        grid_build_max = grid_build_max.max(grid_elapsed);
        desired_grid.record(desired_elapsed)?;
        desired_grid_max = desired_grid_max.max(desired_elapsed);
        cases
            .get_mut(&case)
            .context("realtime case histogram missing")?
            .record(desired_elapsed)?;
        case_maxima
            .entry(case)
            .and_modify(|maximum| *maximum = (*maximum).max(desired_elapsed));
        terminal_latency.begin(desired.epoch, started);
        let desired_epoch = desired.epoch;
        presenter.publish(desired)?;
        let presented_epoch = presented_receiver
            .recv_timeout(Duration::from_secs(1))
            .context("presenter did not complete the physical-input frame")?;
        anyhow::ensure!(
            presented_epoch == desired_epoch,
            "presenter completed an unexpected frame epoch"
        );
    }
    let presenter = presenter.finish()?;
    let (backend_writes, terminal_patches) = backend
        .lock()
        .map(|backend| (backend.writes, backend.patches))
        .map_err(|_| anyhow::anyhow!("measurement backend lock poisoned"))?;
    let terminal = terminal_latency.measurements()?;
    Ok(RealtimeMetrics {
        commit,
        frame_snapshot,
        frame_snapshot_max,
        grid_build,
        grid_build_max,
        desired_grid,
        desired_grid_max,
        cases,
        case_maxima,
        terminal,
        presenter,
        backend_writes,
        terminal_patches,
    })
}

fn duration_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos())
        .unwrap_or(u64::MAX)
        .max(1)
}

struct GateResults {
    large_file_open: bool,
    large_file_first_frame: bool,
    large_file_open_to_terminal: bool,
    full_production_desired_frame: bool,
    production_desired_p99: bool,
    production_desired_maximum: bool,
    production_cases_p99: bool,
    production_cases_maximum: bool,
    production_terminal_write: bool,
    aggregate_realtime_p99: bool,
    aggregate_realtime_maximum: bool,
    realtime_cases_p99: bool,
    realtime_cases_maximum: bool,
    task_yield_p99: bool,
    task_yield_maximum: bool,
    terminal_write: bool,
    hardware_key_to_photon: bool,
}

const fn desired_frame_maximum_passes(maximum: u64) -> bool {
    maximum <= REALTIME_MAX_GATE_NANOS
}

impl GateResults {
    fn evaluate(
        production: &ProductionMetrics,
        realtime: &RealtimeMetrics,
        task_yields: &Histogram<u64>,
        iterations: u64,
        hardware_key_to_photon: bool,
    ) -> Self {
        let aggregate_realtime_p99 =
            realtime.desired_grid.value_at_quantile(0.99) < REALTIME_AGGREGATE_P99_GATE_NANOS;
        let aggregate_realtime_maximum = desired_frame_maximum_passes(realtime.desired_grid_max);
        let realtime_cases_p99 = realtime
            .cases
            .iter()
            .all(|(case, histogram)| histogram.value_at_quantile(0.99) < case.p99_gate_nanos());
        let realtime_cases_maximum = realtime
            .case_maxima
            .values()
            .copied()
            .all(desired_frame_maximum_passes);
        let production_desired_p99 =
            production.desired.value_at_quantile(0.99) < REALTIME_AGGREGATE_P99_GATE_NANOS;
        let production_desired_maximum = desired_frame_maximum_passes(production.desired_max);
        let production_cases_p99 = production.cases.iter().all(|(case, histogram)| {
            histogram.value_at_quantile(0.99) < production_case_p99_gate(case)
        });
        let production_cases_maximum = production
            .case_maxima
            .values()
            .copied()
            .all(desired_frame_maximum_passes);
        let production_terminal_write = production.terminal.value_at_quantile(0.99)
            < TERMINAL_WRITE_P99_GATE_NANOS
            && production.report.dropped_frames == 0
            && production.report.presented_frames == production.report.published_frames;
        Self {
            large_file_open: production.report.open_nanos < LARGE_FILE_OPEN_MAX_GATE_NANOS,
            large_file_first_frame: production.report.first_desired_frame_nanos
                < LARGE_FILE_FIRST_FRAME_MAX_GATE_NANOS,
            large_file_open_to_terminal: production.report.open_to_first_terminal_write_nanos
                < LARGE_FILE_OPEN_TO_TERMINAL_MAX_GATE_NANOS,
            full_production_desired_frame: production_desired_p99
                && production_desired_maximum
                && production_cases_p99
                && production_cases_maximum,
            production_desired_p99,
            production_desired_maximum,
            production_cases_p99,
            production_cases_maximum,
            production_terminal_write,
            aggregate_realtime_p99,
            aggregate_realtime_maximum,
            realtime_cases_p99,
            realtime_cases_maximum,
            task_yield_p99: task_yields.value_at_quantile(0.99) < TASK_YIELD_P99_GATE_NANOS,
            task_yield_maximum: task_yields.max() < TASK_YIELD_MAX_GATE_NANOS,
            terminal_write: realtime.terminal.value_at_quantile(0.99)
                < TERMINAL_WRITE_P99_GATE_NANOS
                && realtime.presenter.dropped_frames == 0
                && realtime.terminal.len() == iterations,
            hardware_key_to_photon,
        }
    }

    const fn task_yield(&self) -> bool {
        self.task_yield_p99 && self.task_yield_maximum
    }

    const fn aggregate_realtime(&self) -> bool {
        self.aggregate_realtime_p99 && self.aggregate_realtime_maximum
    }

    const fn realtime(&self) -> bool {
        self.aggregate_realtime() && self.realtime_cases_p99 && self.realtime_cases_maximum
    }

    const fn passed(&self) -> bool {
        self.large_file_open
            && self.large_file_first_frame
            && self.large_file_open_to_terminal
            && self.full_production_desired_frame
            && self.realtime()
            && self.task_yield()
            && self.terminal_write
            && self.production_terminal_write
            && self.hardware_key_to_photon
    }

    fn enforce(&self) -> Result<()> {
        anyhow::ensure!(
            self.large_file_open,
            "14,000-line Rust file open exceeded its 500 millisecond maximum gate"
        );
        anyhow::ensure!(
            self.large_file_first_frame,
            "14,000-line Rust first desired frame exceeded its 5 millisecond maximum gate"
        );
        anyhow::ensure!(
            self.large_file_open_to_terminal,
            "14,000-line Rust open-to-terminal-write exceeded its 600 millisecond maximum gate"
        );
        anyhow::ensure!(
            self.full_production_desired_frame,
            "full TUI App desired-frame latency exceeded its p99 or 100 microsecond maximum gate"
        );
        anyhow::ensure!(
            self.aggregate_realtime_p99 && self.realtime_cases_p99,
            "RealtimeCommand p99 exceeded its additionally tightened gate"
        );
        anyhow::ensure!(
            self.aggregate_realtime_maximum && self.realtime_cases_maximum,
            "desired-frame worst observed latency exceeded 100 microseconds"
        );
        anyhow::ensure!(
            self.task_yield_p99,
            "TaskCommand checkpoint-gap p99 exceeded its additionally tightened gate"
        );
        anyhow::ensure!(
            self.task_yield_maximum,
            "TaskCommand checkpoint gap exceeded its additionally tightened maximum gate"
        );
        anyhow::ensure!(
            self.terminal_write,
            "workspace-component terminal-write p99 exceeded its tightened gate"
        );
        anyhow::ensure!(
            self.production_terminal_write,
            "full TUI App terminal-write-completed p99 exceeded its tightened gate"
        );
        anyhow::ensure!(
            self.hardware_key_to_photon,
            "hardware key-to-photon hard gate failed or was unavailable"
        );
        Ok(())
    }
}

fn production_case_p99_gate(case: &str) -> u64 {
    match case {
        "insert_character" | "enter_insert" => RealtimeCase::InsertCharacter.p99_gate_nanos(),
        "delete_character" => RealtimeCase::DeleteCharacter.p99_gate_nanos(),
        "local_motion" | "bottom_navigation" | "leave_insert" => {
            RealtimeCase::LocalMotion.p99_gate_nanos()
        }
        "selection_change" | "enter_visual" => RealtimeCase::SelectionChange.p99_gate_nanos(),
        "viewport_navigation" => RealtimeCase::ViewportNavigation.p99_gate_nanos(),
        _ => REALTIME_AGGREGATE_P99_GATE_NANOS,
    }
}

fn realtime_case_report(metrics: &RealtimeMetrics) -> serde_json::Map<String, Value> {
    metrics
        .cases
        .iter()
        .map(|(case, histogram)| {
            let gate_nanos = case.p99_gate_nanos();
            let p99_passed = histogram.value_at_quantile(0.99) < gate_nanos;
            let observed_maximum = metrics.case_maxima.get(case).copied().unwrap_or(u64::MAX);
            let maximum_passed = desired_frame_maximum_passes(observed_maximum);
            (
                case.name().to_owned(),
                json!({
                    "hard_gate": true,
                    "p99_gate_nanos": gate_nanos,
                    "maximum_gate_nanos": REALTIME_MAX_GATE_NANOS,
                    "p99_passed": p99_passed,
                    "maximum_passed": maximum_passed,
                    "observed_maximum_nanos": observed_maximum,
                    "passed": p99_passed && maximum_passed,
                    "distribution": distribution(histogram),
                }),
            )
        })
        .collect()
}

fn production_case_report(metrics: &ProductionMetrics) -> serde_json::Map<String, Value> {
    metrics
        .cases
        .iter()
        .map(|(case, histogram)| {
            let gate_nanos = production_case_p99_gate(case);
            let p99_passed = histogram.value_at_quantile(0.99) < gate_nanos;
            let observed_maximum = metrics.case_maxima.get(case).copied().unwrap_or(u64::MAX);
            let maximum_passed = desired_frame_maximum_passes(observed_maximum);
            (
                case.to_string(),
                json!({
                    "hard_gate": true,
                    "p99_gate_nanos": gate_nanos,
                    "maximum_gate_nanos": REALTIME_MAX_GATE_NANOS,
                    "p99_passed": p99_passed,
                    "maximum_passed": maximum_passed,
                    "observed_maximum_nanos": observed_maximum,
                    "passed": p99_passed && maximum_passed,
                    "distribution": distribution(histogram),
                }),
            )
        })
        .collect()
}

struct ReportInputs<'a> {
    arguments: &'a Arguments,
    pinned: bool,
    production: &'a ProductionMetrics,
    realtime: &'a RealtimeMetrics,
    task_yields: &'a Histogram<u64>,
    task_cancellations: &'a Histogram<u64>,
    hardware_key_to_photon: Value,
    gates: &'a GateResults,
}

fn report(inputs: ReportInputs<'_>) -> Value {
    let ReportInputs {
        arguments,
        pinned,
        production,
        realtime,
        task_yields,
        task_cancellations,
        hardware_key_to_photon,
        gates,
    } = inputs;
    json!({
        "schema": 4,
        "unit": "nanoseconds",
        "iterations": arguments.iterations,
        "cpu_requested": arguments.cpu,
        "cpu_pinned": pinned,
        "passed": gates.passed(),
        "physical_input_to_transaction_commit": distribution(&realtime.commit),
        "frame_snapshot_materialization": {
            "observed_maximum_nanos": realtime.frame_snapshot_max,
            "distribution": distribution(&realtime.frame_snapshot),
        },
        "desired_grid_construction": {
            "observed_maximum_nanos": realtime.grid_build_max,
            "distribution": distribution(&realtime.grid_build),
        },
        "component_physical_input_to_workspace_grid_ready": {
            "coverage": "engine_and_workspace_composer_component_only",
            "omitted_production_stages": [
                "TUI App transaction side effects",
                "syntax and semantic decoration selection",
                "search, Git, diagnostic, and selection decorations",
                "editor overlays"
            ],
            "hard_gate": true,
            "p99_gate_nanos": REALTIME_AGGREGATE_P99_GATE_NANOS,
            "maximum_gate_nanos": REALTIME_MAX_GATE_NANOS,
            "p99_passed": gates.aggregate_realtime_p99,
            "maximum_passed": gates.aggregate_realtime_maximum,
            "observed_maximum_nanos": realtime.desired_grid_max,
            "aggregate_passed": gates.aggregate_realtime(),
            "passed": gates.realtime(),
            "distribution": distribution(&realtime.desired_grid),
            "realtime_commands": realtime_case_report(realtime),
        },
        "physical_input_to_desired_frame_ready": {
            "coverage": "full_tui_app_decorations_overlays_and_workspace_renderer",
            "hard_gate": true,
            "p99_gate_nanos": REALTIME_AGGREGATE_P99_GATE_NANOS,
            "maximum_gate_nanos": REALTIME_MAX_GATE_NANOS,
            "p99_passed": gates.production_desired_p99,
            "maximum_passed": gates.production_desired_maximum,
            "case_p99_passed": gates.production_cases_p99,
            "case_maximum_passed": gates.production_cases_maximum,
            "observed_maximum_nanos": production.desired_max,
            "passed": gates.full_production_desired_frame,
            "distribution": distribution(&production.desired),
            "realtime_commands": production_case_report(production),
            "stage_distributions": {
                "input_dispatch": distribution(&production.input),
                "app_background_poll": distribution(&production.app_poll),
                "provider_scheduling": distribution(&production.provider_schedule),
                "desired_frame_construction": distribution(&production.frame),
            },
            "workload": {
                "name": production.report.workload,
                "width": production.report.width,
                "height": production.report.height,
                "syntax_spans": production.report.syntax_spans,
                "large_file_open": {
                    "hard_gate": true,
                    "maximum_gate_nanos": LARGE_FILE_OPEN_MAX_GATE_NANOS,
                    "observed_nanos": production.report.open_nanos,
                    "passed": gates.large_file_open,
                },
                "large_file_first_desired_frame": {
                    "hard_gate": true,
                    "maximum_gate_nanos": LARGE_FILE_FIRST_FRAME_MAX_GATE_NANOS,
                    "observed_nanos": production.report.first_desired_frame_nanos,
                    "passed": gates.large_file_first_frame,
                },
                "large_file_open_to_first_terminal_write": {
                    "hard_gate": true,
                    "maximum_gate_nanos": LARGE_FILE_OPEN_TO_TERMINAL_MAX_GATE_NANOS,
                    "observed_nanos": production.report.open_to_first_terminal_write_nanos,
                    "passed": gates.large_file_open_to_terminal,
                },
            },
        },
        "task_command_yield_to_ui": {
            "hard_gate": true,
            "passed": gates.task_yield(),
            "p99_gate_nanos": TASK_YIELD_P99_GATE_NANOS,
            "maximum_gate_nanos": TASK_YIELD_MAX_GATE_NANOS,
            "p99_passed": gates.task_yield_p99,
            "maximum_passed": gates.task_yield_maximum,
            "distribution": distribution(task_yields),
        },
        "task_command_cancellation_latency": distribution(task_cancellations),
        "component_input_to_terminal_write_completed": {
            "hard_gate": true,
            "p99_gate_nanos": TERMINAL_WRITE_P99_GATE_NANOS,
            "passed": gates.terminal_write,
            "distribution": distribution(&realtime.terminal),
        },
        "input_to_terminal_write_completed": {
            "coverage": "full_tui_app_and_termina",
            "hard_gate": true,
            "p99_gate_nanos": TERMINAL_WRITE_P99_GATE_NANOS,
            "passed": gates.production_terminal_write,
            "distribution": distribution(&production.terminal),
        },
        "component_terminal_backpressure": {
            "published_frames": realtime.presenter.published_frames,
            "scenario_baseline_presentations": arguments.iterations,
            "measured_presentations": arguments.iterations,
            "dropped_frames": realtime.presenter.dropped_frames,
            "presented_frames": realtime.presenter.presented_frames,
            "backend_writes": realtime.backend_writes,
            "terminal_patches": realtime.terminal_patches,
        },
        "terminal_backpressure": {
            "published_frames": production.report.published_frames,
            "dropped_frames": production.report.dropped_frames,
            "presented_frames": production.report.presented_frames,
        },
        "physical_input_only": true,
        "macro_replay_keys_included": false,
        "hardware_key_to_photon": hardware_key_to_photon,
        "runner_contract": {
            "bare_metal_declared": bare_metal_declared(),
            "fixed_workloads": true,
            "render_path": "full_tui_app_dotfile_profile_plus_component_breakdown",
            "full_production_desired_frame_covered": true,
            "gate_authoritative": arguments.gate,
        },
    })
}

fn main() -> Result<()> {
    let process_arguments = env::args().skip(1).collect::<Vec<_>>();
    if process_arguments.first().map(String::as_str) == Some("--internal-provider-host") {
        wren_provider::serve(io::stdin().lock(), io::stdout().lock())?;
        return Ok(());
    }
    if process_arguments.first().map(String::as_str) == Some("--production-probe-child") {
        return run_production_probe_child(&process_arguments[1..]);
    }
    let arguments = arguments()?;
    let pinned = pin_requested_cpu(arguments.cpu);
    validate_gate_environment(&arguments, pinned)?;
    let production = production_metrics(arguments.iterations)?;
    let realtime = realtime_metrics(arguments.iterations)?;

    let task_samples = arguments.iterations.clamp(1, 100);
    let (task_yields, task_cancellations) = task_metrics(task_samples)?;
    let (hardware_key_to_photon, hardware_key_to_photon_pass) = key_to_photon_report();
    let gates = GateResults::evaluate(
        &production,
        &realtime,
        &task_yields,
        arguments.iterations,
        hardware_key_to_photon_pass,
    );
    let report = report(ReportInputs {
        arguments: &arguments,
        pinned,
        production: &production,
        realtime: &realtime,
        task_yields: &task_yields,
        task_cancellations: &task_cancellations,
        hardware_key_to_photon,
        gates: &gates,
    });
    emit_report(&report, arguments.output.as_deref())?;
    if arguments.gate {
        gates.enforce()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desired_frame_worst_observed_boundary_is_one_hundred_microseconds() {
        assert!(desired_frame_maximum_passes(100_000));
        assert!(!desired_frame_maximum_passes(100_001));
    }

    #[test]
    fn physical_rig_gate_requires_a_second_strict_ten_percent_improvement() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let report_path = directory.path().join("photon.json");
        let write_report = |measured_p99_nanos| {
            fs::write(
                &report_path,
                serde_json::to_vec(&json!({
                    "schema": 1,
                    "rig_id": "test-photodiode",
                    "samples": 10_000,
                    "baseline_p99_nanos": 1_000,
                    "measured_p99_nanos": measured_p99_nanos,
                }))
                .expect("serialize rig report"),
            )
            .expect("write rig report");
        };

        write_report(809);
        let (passing, passed) = key_to_photon_report_from_path(Some(report_path.clone()));
        assert!(passed);
        assert_eq!(passing["p99_gate_nanos"], 810);
        assert_eq!(passing["hard_gate"], true);

        write_report(810);
        let (failing, passed) = key_to_photon_report_from_path(Some(report_path));
        assert!(!passed);
        assert_eq!(failing["passed"], false);
    }

    #[test]
    fn missing_physical_rig_report_is_a_hard_failure() {
        let (report, passed) = key_to_photon_report_from_path(None);
        assert!(!passed);
        assert_eq!(report["hard_gate"], true);
        assert_eq!(report["passed"], false);
    }
}
