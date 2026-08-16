use std::io::{self, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use wren_presenter::{PresentationObserver, Presenter};
use wren_term::{
    TerminaBackend, TerminalBackend, TerminalError, TerminalInput, TerminalKey, TerminalKeyCode,
};
use wren_view::{TerminalPatch, ViewportLayout};

use super::{App, desired_frame, poll_app_work};

const WIDTH: usize = 120;
const HEIGHT: usize = 40;
const LARGE_RUST_LINES: usize = 14_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductionLatencySample {
    pub scenario: Box<str>,
    pub input_nanos: u64,
    pub app_poll_nanos: u64,
    pub provider_schedule_nanos: u64,
    pub desired_frame_nanos: u64,
    pub input_to_desired_frame_nanos: u64,
    pub input_to_terminal_write_nanos: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductionLatencyReport {
    pub schema: u64,
    pub workload: Box<str>,
    pub width: usize,
    pub height: usize,
    pub requested_iterations: u64,
    pub syntax_spans: usize,
    pub open_nanos: u64,
    pub first_desired_frame_nanos: u64,
    pub open_to_first_terminal_write_nanos: u64,
    pub published_frames: u64,
    pub dropped_frames: u64,
    pub presented_frames: u64,
    pub samples: Vec<ProductionLatencySample>,
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
}

impl MeasuringBackend {
    fn new() -> Result<Self> {
        Ok(Self {
            terminal: TerminaBackend::new(CountingWriter::default(), WIDTH, HEIGHT)?,
        })
    }
}

impl TerminalBackend for MeasuringBackend {
    type Error = TerminalError;

    fn submit(&mut self, patches: &[TerminalPatch]) -> Result<(), Self::Error> {
        self.terminal.submit(patches)
    }
}

fn elapsed_nanos(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos())
        .unwrap_or(u64::MAX)
        .max(1)
}

fn terminal_key(code: TerminalKeyCode, control: bool) -> TerminalInput {
    TerminalInput::Key(TerminalKey {
        code,
        shift: false,
        control,
        alt: false,
        super_key: false,
    })
}

fn character(character: char) -> TerminalInput {
    terminal_key(TerminalKeyCode::Char(character), false)
}

struct Probe {
    app: App,
    layout: ViewportLayout,
    presenter: Presenter<MeasuringBackend>,
    presented: mpsc::Receiver<u64>,
    samples: Vec<ProductionLatencySample>,
}

impl Probe {
    fn measure(&mut self, scenario: &str, input: TerminalInput) -> Result<()> {
        let started = Instant::now();
        self.app.handle_input(input)?;
        let input_nanos = elapsed_nanos(started);

        let poll_started = Instant::now();
        let _changed = poll_app_work(&mut self.app)?;
        let app_poll_nanos = elapsed_nanos(poll_started);
        self.app.capture_debug_output();

        let schedule_started = Instant::now();
        self.app.schedule_provider_refreshes(self.layout.height);
        let provider_schedule_nanos = elapsed_nanos(schedule_started);

        let frame_started = Instant::now();
        let frame = desired_frame(&mut self.layout, &self.app);
        let desired_frame_nanos = elapsed_nanos(frame_started);
        let input_to_desired_frame_nanos = elapsed_nanos(started);
        let epoch = frame.epoch;
        self.presenter.publish(frame)?;
        let presented = self
            .presented
            .recv_timeout(Duration::from_secs(5))
            .context("production latency presenter timed out")?;
        anyhow::ensure!(
            presented == epoch,
            "presenter completed an unexpected epoch"
        );
        let input_to_terminal_write_nanos = elapsed_nanos(started);
        self.samples.push(ProductionLatencySample {
            scenario: scenario.into(),
            input_nanos,
            app_poll_nanos,
            provider_schedule_nanos,
            desired_frame_nanos,
            input_to_desired_frame_nanos,
            input_to_terminal_write_nanos,
        });
        Ok(())
    }
}

/// Runs a fixed large-file workload through the same `App`, decoration
/// collection, workspace renderer, overlays, presenter, and Termina serializer
/// as the executable. Callers should isolate HOME/XDG state because the App's
/// durability paths are intentionally part of the measured product behavior.
pub fn run_production_latency_probe(iterations: u64) -> Result<ProductionLatencyReport> {
    let source = (0..LARGE_RUST_LINES)
        .map(|line| {
            format!(
                "pub fn item_{line:05}() -> usize {{ let value_{line:05}: usize = {line}; value_{line:05} }}\n"
            )
        })
        .collect::<String>();
    let mut source_file = tempfile::Builder::new()
        .prefix("wren-production-latency-")
        .suffix(".rs")
        .tempfile_in(std::env::current_dir()?)
        .context("create production latency fixture")?;
    source_file
        .write_all(source.as_bytes())
        .context("write production latency fixture")?;
    source_file
        .flush()
        .context("flush production latency fixture")?;
    let source_path = source_file.path();

    let open_started = Instant::now();
    let mut app = App::open(Some(source_path), None).context("open production latency fixture")?;
    let open_nanos = elapsed_nanos(open_started);
    app.active.git_index_text = Some(Arc::from(source));
    app.active.refresh_git_hunks();
    let syntax_spans = app
        .decorations
        .get(&app.active.buffer_id)
        .map_or(0, |decorations| decorations.spans.len());
    anyhow::ensure!(
        syntax_spans >= 42_000,
        "production probe did not retain full syntax highlighting"
    );

    let mut layout = ViewportLayout::new(WIDTH, HEIGHT);
    layout.configure_dotfile_profile();
    app.resize_terminal(HEIGHT, WIDTH);
    let backend = Arc::new(Mutex::new(MeasuringBackend::new()?));
    let (presented_sender, presented) = mpsc::sync_channel(1);
    let observer: PresentationObserver = Arc::new(move |epoch| {
        let _ = presented_sender.send(epoch);
    });
    let presenter = Presenter::start_observed(backend, Some(observer))?;

    let first_started = Instant::now();
    app.schedule_provider_refreshes(layout.height);
    let first = desired_frame(&mut layout, &app);
    let first_desired_frame_nanos = elapsed_nanos(first_started);
    let first_epoch = first.epoch;
    presenter.publish(first)?;
    let presented_epoch = presented
        .recv_timeout(Duration::from_secs(5))
        .context("first production frame was not presented")?;
    anyhow::ensure!(
        presented_epoch == first_epoch,
        "presenter completed an unexpected first epoch"
    );
    let open_to_first_terminal_write_nanos = elapsed_nanos(open_started);

    let mut probe = Probe {
        app,
        layout,
        presenter,
        presented,
        samples: Vec::new(),
    };
    probe.measure("bottom_navigation", character('G'))?;

    let samples_per_case = iterations.saturating_add(3).saturating_div(4).max(1);
    for sample in 0..samples_per_case {
        probe.measure(
            "local_motion",
            character(if sample % 2 == 0 { 'h' } else { 'l' }),
        )?;
    }
    for sample in 0..samples_per_case {
        probe.measure(
            "viewport_navigation",
            terminal_key(
                TerminalKeyCode::Char(if sample % 2 == 0 { 'u' } else { 'd' }),
                true,
            ),
        )?;
    }

    probe.measure("enter_insert", character('i'))?;
    for sample in 0..samples_per_case {
        probe.measure(
            if sample % 2 == 0 {
                "insert_character"
            } else {
                "delete_character"
            },
            if sample % 2 == 0 {
                character('x')
            } else {
                terminal_key(TerminalKeyCode::Backspace, false)
            },
        )?;
    }
    if samples_per_case % 2 == 1 {
        probe.measure(
            "delete_character",
            terminal_key(TerminalKeyCode::Backspace, false),
        )?;
    }
    probe.measure("leave_insert", terminal_key(TerminalKeyCode::Escape, false))?;
    probe.measure("enter_visual", character('v'))?;
    for sample in 0..samples_per_case {
        probe.measure(
            "selection_change",
            character(if sample % 2 == 0 { 'h' } else { 'l' }),
        )?;
    }

    let samples = std::mem::take(&mut probe.samples);
    let stats = probe.presenter.finish()?;
    Ok(ProductionLatencyReport {
        schema: 1,
        workload: "full_tui_app_large_rust_14000_lines".into(),
        width: WIDTH,
        height: HEIGHT,
        requested_iterations: iterations,
        syntax_spans,
        open_nanos,
        first_desired_frame_nanos,
        open_to_first_terminal_write_nanos,
        published_frames: stats.published_frames,
        dropped_frames: stats.dropped_frames,
        presented_frames: stats.presented_frames,
        samples,
    })
}
