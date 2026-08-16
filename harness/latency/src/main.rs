use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, Cursor, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hdrhistogram::Histogram;
use serde::Deserialize;
use serde_json::{Value, json};
use wren_command::{TaskResult, TaskRunner};
use wren_engine::Editor;
use wren_grammar::KeyEvent;
use wren_presenter::{PresentationObserver, Presenter};
use wren_provider::{CompletionCandidate, CompletionSession};
use wren_term::{TerminaBackend, TerminalBackend, TerminalError};
use wren_text::{DefaultText, TextStore};
use wren_types::{CommandTask, CommandTaskId, DocumentId, Effects, Transaction};
use wren_view::{TerminalPatch, ViewportLayout};

const REALTIME_AGGREGATE_P99_GATE_NANOS: u64 = 86_283;
const TASK_YIELD_P99_GATE_NANOS: u64 = 69_176;
const TASK_YIELD_MAX_GATE_NANOS: u64 = 71_077;
const TERMINAL_WRITE_P99_GATE_NANOS: u64 = 115_141;

#[derive(Debug)]
struct Arguments {
    iterations: u64,
    cpu: Option<usize>,
    output: Option<PathBuf>,
    gate: bool,
}

fn arguments() -> Result<Arguments> {
    let values: Vec<String> = env::args().skip(1).collect();
    let mut iterations = 10_000;
    let mut cpu = None;
    let mut output = None;
    let mut gate = false;
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--iterations" => {
                index += 1;
                iterations = values
                    .get(index)
                    .context("--iterations needs a value")?
                    .parse()?;
            }
            "--cpu" => {
                index += 1;
                cpu = Some(values.get(index).context("--cpu needs a value")?.parse()?);
            }
            "--output" => {
                index += 1;
                output = Some(PathBuf::from(
                    values.get(index).context("--output needs a value")?,
                ));
            }
            "--gate" => gate = true,
            argument => anyhow::bail!("unknown argument: {argument}"),
        }
        index += 1;
    }
    anyhow::ensure!(iterations > 0, "--iterations must be positive");
    Ok(Arguments {
        iterations,
        cpu,
        output,
        gate,
    })
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
            let gate_nanos = measurement.baseline_p99_nanos.saturating_mul(9) / 10;
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

fn histogram() -> Result<Histogram<u64>> {
    Histogram::new_with_bounds(1, 60_000_000_000, 3).map_err(Into::into)
}

fn distribution(histogram: &Histogram<u64>) -> Value {
    json!({
        "samples": histogram.len(),
        "min": histogram.min(),
        "p50": histogram.value_at_quantile(0.50),
        "p90": histogram.value_at_quantile(0.90),
        "p99": histogram.value_at_quantile(0.99),
        "max": histogram.max(),
    })
}

fn elapsed_nanos(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos())
        .unwrap_or(u64::MAX)
        .max(1)
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

const REALTIME_CASES: &[&str] = &[
    "insert_character",
    "delete_character",
    "local_motion",
    "bounded_operator",
    "selection_change",
    "viewport_navigation",
    "completion_acceptance",
];

fn realtime_case_p99_gate_nanos(name: &str) -> Option<u64> {
    match name {
        "bounded_operator" => Some(86_629),
        "completion_acceptance" => Some(86_283),
        "delete_character" => Some(87_263),
        "insert_character" => Some(98_667),
        "local_motion" => Some(87_378),
        "selection_change" => Some(84_440),
        "viewport_navigation" => Some(73_093),
        _ => None,
    }
}

fn fixture_editor() -> Result<Editor<DefaultText>> {
    let source = (0..256)
        .map(|line| format!("fn item_{line}() {{ let value = {line}; }}\n"))
        .collect::<String>();
    let text = DefaultText::from_reader(Cursor::new(source)).context("build realtime fixture")?;
    Ok(Editor::new(text))
}

fn apply_key(
    editor: &mut Editor<DefaultText>,
    setup: &[KeyEvent],
    key: KeyEvent,
) -> Result<Option<Transaction>> {
    for setup_key in setup {
        editor
            .handle_key(*setup_key)
            .context("prepare realtime command")?;
    }
    editor.handle_key(key).context("execute realtime command")
}

fn run_realtime_case(
    name: &str,
    editor: &mut Editor<DefaultText>,
    layout: &mut ViewportLayout,
) -> Result<()> {
    match name {
        "insert_character" => {
            apply_key(
                editor,
                &[KeyEvent::character('i')],
                KeyEvent::character('x'),
            )?;
        }
        "delete_character" => {
            apply_key(editor, &[], KeyEvent::character('x'))?;
        }
        "local_motion" => {
            apply_key(editor, &[], KeyEvent::character('l'))?;
        }
        "bounded_operator" => {
            apply_key(
                editor,
                &[KeyEvent::character('d')],
                KeyEvent::character('w'),
            )?;
        }
        "selection_change" => {
            apply_key(
                editor,
                &[KeyEvent::character('v')],
                KeyEvent::character('l'),
            )?;
        }
        "viewport_navigation" => {
            layout.top_line = layout.top_line.saturating_add(1);
        }
        "completion_acceptance" => {
            let session = CompletionSession::merge(
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
            );
            if let Some(transaction) = session
                .accept(editor.revision(), 0)
                .context("accept fixed completion")?
            {
                editor
                    .apply_transaction(transaction)
                    .context("apply completion transaction")?;
            }
        }
        _ => anyhow::bail!("unknown realtime benchmark case {name}"),
    }
    Ok(())
}

fn main() -> Result<()> {
    let arguments = arguments()?;
    let pinned = arguments.cpu.is_some_and(pin_cpu);
    validate_gate_environment(&arguments, pinned)?;
    let backend = Arc::new(Mutex::new(MeasuringBackend::new()?));
    let terminal_latency = Arc::new(TerminalLatency::new()?);
    let observer_latency = Arc::clone(&terminal_latency);
    let (presented_sender, presented_receiver) = mpsc::sync_channel(1);
    let observer: PresentationObserver = Arc::new(move |epoch| {
        observer_latency.completed(epoch);
        let _ = presented_sender.send(epoch);
    });
    let presenter = Presenter::start_observed(Arc::clone(&backend), Some(observer))?;

    let mut commit_histogram = histogram()?;
    let mut desired_grid_histogram = histogram()?;
    let mut case_histograms = REALTIME_CASES
        .iter()
        .map(|name| Ok((*name, histogram()?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    for iteration in 0..arguments.iterations {
        let name = REALTIME_CASES[(iteration as usize) % REALTIME_CASES.len()];
        let mut editor = fixture_editor()?;
        let mut layout = ViewportLayout::new(120, 40);
        let started = Instant::now();
        run_realtime_case(name, &mut editor, &mut layout)?;
        commit_histogram.record(elapsed_nanos(started))?;
        let desired = Arc::new(layout.desired_grid(&editor.frame()));
        let desired_elapsed = elapsed_nanos(started);
        desired_grid_histogram.record(desired_elapsed)?;
        case_histograms
            .get_mut(name)
            .context("realtime case histogram missing")?
            .record(desired_elapsed)?;
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
    let presenter_stats = presenter.finish()?;
    let (writes, patches) = backend
        .lock()
        .map(|backend| (backend.writes, backend.patches))
        .map_err(|_| anyhow::anyhow!("measurement backend lock poisoned"))?;
    let terminal_histogram = terminal_latency
        .histogram
        .lock()
        .map_err(|_| anyhow::anyhow!("terminal histogram lock poisoned"))?;

    let task_samples = arguments.iterations.clamp(1, 100);
    let (task_yields, task_cancellations) = task_metrics(task_samples)?;
    let case_report = case_histograms
        .iter()
        .map(|(name, distribution_histogram)| {
            let gate_nanos =
                realtime_case_p99_gate_nanos(name).context("realtime case p99 gate missing")?;
            let passed = distribution_histogram.value_at_quantile(0.99) < gate_nanos;
            Ok((
                (*name).to_owned(),
                json!({
                    "hard_gate": true,
                    "p99_gate_nanos": gate_nanos,
                    "passed": passed,
                    "distribution": distribution(distribution_histogram),
                }),
            ))
        })
        .collect::<Result<serde_json::Map<_, _>>>()?;
    let aggregate_realtime_pass =
        desired_grid_histogram.value_at_quantile(0.99) < REALTIME_AGGREGATE_P99_GATE_NANOS;
    let realtime_pass = aggregate_realtime_pass
        && case_histograms
            .iter()
            .all(|(name, distribution_histogram)| {
                realtime_case_p99_gate_nanos(name).is_some_and(|gate_nanos| {
                    distribution_histogram.value_at_quantile(0.99) < gate_nanos
                })
            });
    let task_yield_p99_pass = task_yields.value_at_quantile(0.99) < TASK_YIELD_P99_GATE_NANOS;
    let task_yield_max_pass = task_yields.max() < TASK_YIELD_MAX_GATE_NANOS;
    let task_yield_pass = task_yield_p99_pass && task_yield_max_pass;
    let terminal_write_pass = terminal_histogram.value_at_quantile(0.99)
        < TERMINAL_WRITE_P99_GATE_NANOS
        && presenter_stats.dropped_frames == 0
        && terminal_histogram.len() == arguments.iterations;
    let (hardware_key_to_photon, hardware_key_to_photon_pass) = key_to_photon_report();
    let passed =
        realtime_pass && task_yield_pass && terminal_write_pass && hardware_key_to_photon_pass;
    let report = json!({
        "schema": 3,
        "unit": "nanoseconds",
        "iterations": arguments.iterations,
        "cpu_requested": arguments.cpu,
        "cpu_pinned": pinned,
        "passed": passed,
        "physical_input_to_transaction_commit": distribution(&commit_histogram),
        "physical_input_to_desired_frame_ready": {
            "hard_gate": true,
            "p99_gate_nanos": REALTIME_AGGREGATE_P99_GATE_NANOS,
            "aggregate_passed": aggregate_realtime_pass,
            "passed": realtime_pass,
            "distribution": distribution(&desired_grid_histogram),
            "realtime_commands": case_report,
        },
        "task_command_yield_to_ui": {
            "hard_gate": true,
            "passed": task_yield_pass,
            "p99_gate_nanos": TASK_YIELD_P99_GATE_NANOS,
            "maximum_gate_nanos": TASK_YIELD_MAX_GATE_NANOS,
            "p99_passed": task_yield_p99_pass,
            "maximum_passed": task_yield_max_pass,
            "distribution": distribution(&task_yields),
        },
        "task_command_cancellation_latency": distribution(&task_cancellations),
        "input_to_terminal_write_completed": {
            "hard_gate": true,
            "p99_gate_nanos": TERMINAL_WRITE_P99_GATE_NANOS,
            "passed": terminal_write_pass,
            "distribution": distribution(&terminal_histogram),
        },
        "terminal_backpressure": {
            "published_frames": presenter_stats.published_frames,
            "dropped_frames": presenter_stats.dropped_frames,
            "presented_frames": presenter_stats.presented_frames,
            "backend_writes": writes,
            "terminal_patches": patches,
        },
        "physical_input_only": true,
        "macro_replay_keys_included": false,
        "hardware_key_to_photon": hardware_key_to_photon,
        "runner_contract": {
            "bare_metal_declared": env::var("WREN_BARE_METAL").as_deref() == Ok("1"),
            "fixed_workloads": true,
            "gate_authoritative": arguments.gate,
        },
    });
    drop(terminal_histogram);
    let rendered = serde_json::to_string_pretty(&report)?;
    if let Some(path) = arguments.output {
        fs::write(&path, format!("{rendered}\n"))
            .with_context(|| format!("write {}", path.display()))?;
    }
    println!("{rendered}");
    if arguments.gate {
        anyhow::ensure!(
            realtime_pass,
            "RealtimeCommand p99 exceeded a 90%-of-baseline gate"
        );
        anyhow::ensure!(
            task_yield_p99_pass,
            "TaskCommand checkpoint-gap p99 exceeded its 90%-of-baseline gate"
        );
        anyhow::ensure!(
            task_yield_max_pass,
            "TaskCommand checkpoint gap exceeded its 90%-of-baseline maximum gate"
        );
        anyhow::ensure!(
            terminal_write_pass,
            "terminal-write-completed p99 exceeded its 90%-of-baseline gate"
        );
        anyhow::ensure!(
            hardware_key_to_photon_pass,
            "hardware key-to-photon hard gate failed or was unavailable"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_rig_gate_requires_a_strict_ten_percent_improvement() {
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

        write_report(899);
        let (passing, passed) = key_to_photon_report_from_path(Some(report_path.clone()));
        assert!(passed);
        assert_eq!(passing["p99_gate_nanos"], 900);
        assert_eq!(passing["hard_gate"], true);

        write_report(900);
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
