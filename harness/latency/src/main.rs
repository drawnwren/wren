use std::collections::BTreeMap;
use std::convert::Infallible;
use std::env;
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hdrhistogram::Histogram;
use serde_json::{Value, json};
use wren_command::{TaskResult, TaskRunner};
use wren_engine::Editor;
use wren_grammar::KeyEvent;
use wren_presenter::{PresentationObserver, Presenter};
use wren_provider::{CompletionCandidate, CompletionSession};
use wren_term::TerminalBackend;
use wren_text::{DefaultText, TextStore};
use wren_types::{CommandTask, CommandTaskId, DocumentId, Effects, Transaction};
use wren_view::{TerminalPatch, ViewportLayout};

const REALTIME_GATE_NANOS: u64 = 4_000_000;

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

fn key_to_photon_report() -> Value {
    let Some(path) = env::var_os("WREN_KEY_TO_PHOTON_JSON").map(PathBuf::from) else {
        return json!({
            "available": false,
            "hard_gate": false,
            "reason": "set WREN_KEY_TO_PHOTON_JSON to hardware-rig output",
        });
    };
    match fs::read_to_string(&path)
        .ok()
        .and_then(|source| serde_json::from_str::<Value>(&source).ok())
    {
        Some(measurement) => json!({
            "available": true,
            "hard_gate": false,
            "source": path,
            "measurement": measurement,
        }),
        None => json!({
            "available": false,
            "hard_gate": false,
            "source": path,
            "reason": "hardware-rig report was unreadable or invalid JSON",
        }),
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
struct MeasuringBackend {
    writes: u64,
    patches: u64,
}

impl TerminalBackend for MeasuringBackend {
    type Error = Infallible;

    fn submit(&mut self, patches: &[TerminalPatch]) -> Result<(), Self::Error> {
        self.writes = self.writes.saturating_add(1);
        self.patches = self
            .patches
            .saturating_add(u64::try_from(patches.len()).unwrap_or(u64::MAX));
        std::hint::black_box(patches);
        Ok(())
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
    let backend = Arc::new(Mutex::new(MeasuringBackend::default()));
    let terminal_latency = Arc::new(TerminalLatency::new()?);
    let observer_latency = Arc::clone(&terminal_latency);
    let observer: PresentationObserver = Arc::new(move |epoch| {
        observer_latency.completed(epoch);
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
        presenter.publish(desired)?;
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
            let passed = distribution_histogram.value_at_quantile(0.99) < REALTIME_GATE_NANOS;
            (
                (*name).to_owned(),
                json!({
                    "hard_gate": true,
                    "passed": passed,
                    "distribution": distribution(distribution_histogram),
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let realtime_pass = case_histograms.values().all(|distribution_histogram| {
        distribution_histogram.value_at_quantile(0.99) < REALTIME_GATE_NANOS
    });
    let task_yield_pass = task_yields.max() < REALTIME_GATE_NANOS;
    let report = json!({
        "schema": 2,
        "unit": "nanoseconds",
        "iterations": arguments.iterations,
        "cpu_requested": arguments.cpu,
        "cpu_pinned": pinned,
        "hard_gate_nanos": REALTIME_GATE_NANOS,
        "physical_input_to_transaction_commit": distribution(&commit_histogram),
        "physical_input_to_desired_frame_ready": {
            "hard_gate": true,
            "passed": realtime_pass,
            "distribution": distribution(&desired_grid_histogram),
            "realtime_commands": case_report,
        },
        "task_command_yield_to_ui": {
            "hard_gate": true,
            "passed": task_yield_pass,
            "distribution": distribution(&task_yields),
        },
        "task_command_cancellation_latency": distribution(&task_cancellations),
        "input_to_terminal_write_completed": {
            "hard_gate": false,
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
        "hardware_key_to_photon": key_to_photon_report(),
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
        anyhow::ensure!(realtime_pass, "RealtimeCommand p99 exceeded 4ms");
        anyhow::ensure!(task_yield_pass, "TaskCommand checkpoint gap exceeded 4ms");
    }
    Ok(())
}
