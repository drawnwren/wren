use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use wren_presenter::{PresentationObserver, Presenter};
use wren_term::{TerminaBackend, TerminalBackend, TerminalError, TerminalInput, TerminalKey, TerminalKeyCode};
use wren_types::Modifiers;
use wren_view::{CatppuccinColor, CellColor, DesiredGrid, TerminalUpdate, ViewportLayout, diff_into};

use super::{App, BufferDecorations, QuickfixEntry, SearchDirection, Severity, StartupScreen, desired_frame, poll_app_work};

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
    pub synthetic_semantic_spans: usize,
    pub search_highlight_active: bool,
    pub diagnostic_count: usize,
    pub git_baseline_active: bool,
    pub open_nanos: u64,
    pub first_desired_frame_nanos: u64,
    pub open_to_first_terminal_write_nanos: u64,
    pub setup_presentations: u64,
    pub published_frames: u64,
    pub dropped_frames: u64,
    pub presented_frames: u64,
    pub samples: Vec<ProductionLatencySample>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TilingPerformanceSample {
    pub scenario: Box<str>,
    pub width: usize,
    pub height: usize,
    pub desired_frame_nanos: u64,
    pub diff_nanos: u64,
    pub terminal_write_nanos: u64,
    pub full_render_nanos: u64,
    pub terminal_patches: usize,
    pub terminal_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TilingPerformanceReport {
    pub schema: u64,
    pub workload: Box<str>,
    pub requested_iterations: u64,
    pub setup_presentations: u64,
    pub published_frames: u64,
    pub dropped_frames: u64,
    pub presented_frames: u64,
    pub samples: Vec<TilingPerformanceSample>,
}

#[derive(Default)]
struct CountingWriter {
    bytes: u64,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        std::hint::black_box(buffer);
        self.bytes = self.bytes.saturating_add(u64::try_from(buffer.len()).unwrap_or(u64::MAX));
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
        Ok(Self { terminal: TerminaBackend::new(CountingWriter::default()) })
    }
}

impl TerminalBackend for MeasuringBackend {
    type Error = TerminalError;

    fn submit(&mut self, update: &TerminalUpdate) -> Result<(), Self::Error> {
        self.terminal.submit(update)
    }
}

fn terminal_update_operations(update: &TerminalUpdate) -> usize {
    2 + update.rows.len() + usize::from(update.clear) + usize::from(update.raster_overlay.is_some())
}

#[derive(Clone, Default)]
struct SharedCountingWriter {
    bytes: Arc<AtomicU64>,
}

impl Write for SharedCountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        std::hint::black_box(buffer);
        self.bytes.fetch_add(u64::try_from(buffer.len()).unwrap_or(u64::MAX), Ordering::Relaxed);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct TilingProbe {
    app: App,
    layout: ViewportLayout,
    diagnostic_terminal: TerminaBackend<CountingWriter>,
    presenter: Presenter<TerminaBackend<SharedCountingWriter>>,
    presented: mpsc::Receiver<u64>,
    presenter_bytes: Arc<AtomicU64>,
    previous: Option<DesiredGrid>,
    diagnostic_update: TerminalUpdate,
    setup_presentations: u64,
    samples: Vec<TilingPerformanceSample>,
}

const TILING_BENCHMARK_STARTUP_SEED: u64 = 0x7765_726e_2d74_696c;

impl TilingProbe {
    fn new() -> Result<Self> {
        let mut app = App::open(None, None).context("open empty tiling performance app")?;
        *app.startup_screen.borrow_mut() = StartupScreen::from_seed(TILING_BENCHMARK_STARTUP_SEED);
        anyhow::ensure!(app.shows_startup_screen(), "tiling performance app did not open on the startup screen");
        let mut layout = ViewportLayout::new(WIDTH, HEIGHT);
        layout.configure_dotfile_profile();
        app.resize_terminal(HEIGHT, WIDTH);
        let diagnostic_terminal = TerminaBackend::new(CountingWriter::default());
        let presenter_writer = SharedCountingWriter::default();
        let presenter_bytes = Arc::clone(&presenter_writer.bytes);
        let backend = TerminaBackend::new(presenter_writer);
        let (presented_sender, presented) = mpsc::sync_channel(1);
        let observer: PresentationObserver = Arc::new(move |epoch| {
            let _ = presented_sender.send(epoch);
        });
        let presenter = Presenter::start_observed(backend, Some(observer))?;
        Ok(Self {
            app,
            layout,
            diagnostic_terminal,
            presenter,
            presented,
            presenter_bytes,
            previous: None,
            diagnostic_update: TerminalUpdate::default(),
            setup_presentations: 0,
            samples: Vec::new(),
        })
    }

    fn configure_size(&mut self, width: usize, height: usize) {
        self.layout.resize(width, height);
        self.layout.configure_dotfile_profile();
        self.app.resize_terminal(height, width);
    }

    fn set_animation_time(&mut self, elapsed: Duration) {
        self.app.started_at = Instant::now().checked_sub(elapsed).unwrap_or_else(Instant::now);
    }

    fn reset_tiling_cache(&mut self) {
        *self.app.startup_screen.borrow_mut() = StartupScreen::from_seed(TILING_BENCHMARK_STARTUP_SEED);
    }

    fn present_setup(&mut self, elapsed: Duration) -> Result<()> {
        self.set_animation_time(elapsed);
        let frame = desired_frame(&mut self.layout, &self.app);
        let epoch = frame.epoch;
        let diagnostic = frame.clone();
        self.presenter.publish(frame)?;
        let presented = self.presented.recv_timeout(Duration::from_secs(5)).context("tiling performance setup presentation timed out")?;
        anyhow::ensure!(presented == epoch, "presenter completed an unexpected epoch");
        diff_into(self.previous.as_ref(), &diagnostic, &mut self.diagnostic_update);
        self.diagnostic_terminal.submit(&self.diagnostic_update)?;
        self.previous = Some(diagnostic);
        self.setup_presentations = self.setup_presentations.saturating_add(1);
        Ok(())
    }

    fn measure(&mut self, scenario: &str, elapsed: Duration) -> Result<()> {
        self.set_animation_time(elapsed);
        let width = self.layout.width;
        let height = self.layout.height;
        let bytes_before = self.presenter_bytes.load(Ordering::Relaxed);
        let started = Instant::now();
        let frame = desired_frame(&mut self.layout, &self.app);
        let desired_frame_nanos = elapsed_nanos(started);
        let epoch = frame.epoch;
        let diagnostic = frame.clone();
        self.presenter.publish(frame)?;
        let presented = self.presented.recv_timeout(Duration::from_secs(5)).context("tiling performance presentation timed out")?;
        anyhow::ensure!(presented == epoch, "presenter completed an unexpected epoch");
        let full_render_nanos = elapsed_nanos(started);
        let terminal_bytes = self.presenter_bytes.load(Ordering::Relaxed).saturating_sub(bytes_before);

        // These component clocks replay the same diff/write against a private
        // backend after the production presenter has completed. They expose
        // regressions by stage without adding duplicate work to the aggregate
        // desired-frame-to-fully-written clock above.
        // Drop the preceding diagnostic patch contents before the component
        // clock, matching the allocation-returning `diff` baseline whose
        // result was dropped after its measured interval.
        self.diagnostic_update.rows.clear();
        self.diagnostic_update.raster_overlay = None;
        let diff_started = Instant::now();
        diff_into(self.previous.as_ref(), &diagnostic, &mut self.diagnostic_update);
        let diff_nanos = elapsed_nanos(diff_started);
        let terminal_started = Instant::now();
        self.diagnostic_terminal.submit(&self.diagnostic_update)?;
        let terminal_write_nanos = elapsed_nanos(terminal_started);
        anyhow::ensure!(diagnostic.raster_overlay.is_some(), "tiling performance sample did not render the startup tiling");
        self.samples.push(TilingPerformanceSample {
            scenario: scenario.into(),
            width,
            height,
            desired_frame_nanos,
            diff_nanos,
            terminal_write_nanos,
            full_render_nanos,
            terminal_patches: terminal_update_operations(&self.diagnostic_update),
            terminal_bytes,
        });
        self.previous = Some(diagnostic);
        Ok(())
    }
}

/// Measures the complete no-buffer tiling path through the production `App`,
/// desired grid construction, Presenter diffing, and completed Termina byte
/// serialization. Component diff/write clocks replay the already-presented
/// frame against a private backend and are excluded from the aggregate clock.
/// Geometry/cache resets and viewport resizes happen before their timed
/// `desired_frame` call only where the named scenario requires them.
pub fn run_tiling_performance_probe(iterations: u64) -> Result<TilingPerformanceReport> {
    let iterations = iterations.max(1);
    let scenario_span_millis = iterations.saturating_add(1).saturating_mul(83);
    let mut probe = TilingProbe::new()?;
    probe.present_setup(Duration::ZERO)?;

    for sample in 0..iterations {
        let elapsed = Duration::from_millis(sample.saturating_add(1).saturating_mul(83));
        probe.measure("animated_120x40", elapsed)?;
    }

    probe.configure_size(240, 80);
    probe.previous = None;
    probe.present_setup(Duration::from_millis(scenario_span_millis))?;
    for sample in 0..iterations {
        let elapsed = Duration::from_millis(scenario_span_millis.saturating_add(sample.saturating_add(1).saturating_mul(83)));
        probe.measure("animated_240x80", elapsed)?;
    }

    probe.configure_size(WIDTH, HEIGHT);
    for sample in 0..iterations {
        probe.reset_tiling_cache();
        probe.previous = None;
        let elapsed = Duration::from_millis(scenario_span_millis.saturating_mul(2).saturating_add(sample.saturating_add(1).saturating_mul(83)));
        probe.measure("cold_120x40", elapsed)?;
    }

    for sample in 0..iterations {
        let (width, height) = if sample % 2 == 0 { (160, 50) } else { (120, 40) };
        probe.configure_size(width, height);
        let elapsed = Duration::from_millis(scenario_span_millis.saturating_mul(3).saturating_add(sample.saturating_add(1).saturating_mul(83)));
        probe.measure("resize_120x40_160x50", elapsed)?;
    }

    let samples = std::mem::take(&mut probe.samples);
    let setup_presentations = probe.setup_presentations;
    let stats = probe.presenter.finish()?;
    Ok(TilingPerformanceReport {
        schema: 1,
        workload: "full_tui_empty_startup_tiling_desired_grid_presenter_diff_termina".into(),
        requested_iterations: iterations,
        setup_presentations,
        published_frames: stats.published_frames,
        dropped_frames: stats.dropped_frames,
        presented_frames: stats.presented_frames,
        samples,
    })
}

fn elapsed_nanos(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX).max(1)
}

fn terminal_key(code: TerminalKeyCode, control: bool) -> TerminalInput {
    let modifiers = if control { Modifiers::CONTROL } else { Modifiers::empty() };
    TerminalInput::Key(TerminalKey::modified(code, modifiers))
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

struct PreparedApp {
    app: App,
    syntax_spans: usize,
    semantic_spans: usize,
}

struct StartedProbe {
    probe: Probe,
    first_desired_frame_nanos: u64,
    open_to_first_terminal_write_nanos: u64,
}

impl Probe {
    fn present_setup(&mut self) -> Result<()> {
        let frame = desired_frame(&mut self.layout, &self.app);
        let epoch = frame.epoch;
        self.presenter.publish(frame)?;
        let presented = self.presented.recv_timeout(Duration::from_secs(5)).context("production latency setup presentation timed out")?;
        anyhow::ensure!(presented == epoch, "presenter completed an unexpected setup epoch");
        Ok(())
    }

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
        let presented = self.presented.recv_timeout(Duration::from_secs(5)).context("production latency presenter timed out")?;
        anyhow::ensure!(presented == epoch, "presenter completed an unexpected epoch");
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
    let (source_file, source) = production_fixture()?;
    let source_path = source_file.path();
    let open_started = Instant::now();
    let app = App::open(Some(source_path), None).context("open production latency fixture")?;
    let open_nanos = elapsed_nanos(open_started);
    let prepared = prepare_app(app, source_path, source)?;
    let StartedProbe { mut probe, first_desired_frame_nanos, open_to_first_terminal_write_nanos } = start_probe(prepared.app, open_started)?;
    let samples_per_case = run_probe_workload(&mut probe, iterations)?;
    let samples = std::mem::take(&mut probe.samples);
    let stats = probe.presenter.finish()?;
    Ok(ProductionLatencyReport {
        schema: 2,
        workload: "full_tui_app_large_rust_14000_lines_active_syntax_semantic_search_git_diagnostics".into(),
        width: WIDTH,
        height: HEIGHT,
        requested_iterations: iterations,
        syntax_spans: prepared.syntax_spans,
        synthetic_semantic_spans: prepared.semantic_spans,
        search_highlight_active: true,
        diagnostic_count: 2,
        git_baseline_active: true,
        open_nanos,
        first_desired_frame_nanos,
        open_to_first_terminal_write_nanos,
        setup_presentations: samples_per_case,
        published_frames: stats.published_frames,
        dropped_frames: stats.dropped_frames,
        presented_frames: stats.presented_frames,
        samples,
    })
}

fn production_fixture() -> Result<(tempfile::NamedTempFile, String)> {
    let source = (0..LARGE_RUST_LINES)
        .map(|line| format!("pub fn item_{line:05}() -> usize {{ let value_{line:05}: usize = {line}; value_{line:05} }}\n"))
        .collect::<String>();
    let mut source_file = tempfile::Builder::new().prefix("wren-production-latency-").suffix(".rs").tempfile().context("create production latency fixture")?;
    source_file.write_all(source.as_bytes()).context("write production latency fixture")?;
    source_file.flush().context("flush production latency fixture")?;
    Ok((source_file, source))
}

fn prepare_app(mut app: App, source_path: &Path, source: String) -> Result<PreparedApp> {
    app.active.git_index_text = Some(Arc::from(source));
    app.active.refresh_git_hunks();
    let syntax_spans = app.decorations.get(&app.active.buffer_id).map_or(0, |decorations| decorations.spans.len());
    anyhow::ensure!(syntax_spans >= 42_000, "production probe did not retain full syntax highlighting");
    let revision = app.active.editor.revision();
    let semantic_spans = app
        .decorations
        .get(&app.active.buffer_id)
        .map(|decorations| {
            decorations
                .spans
                .iter()
                .step_by(6)
                .cloned()
                .map(|mut span| {
                    span.priority = 2_000_000;
                    span.style = span.style.with_foreground(CellColor::Theme(CatppuccinColor::Lavender));
                    span
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let semantic_spans_count = semantic_spans.len();
    app.semantic_decorations.insert(app.active.buffer_id, BufferDecorations::new(revision, semantic_spans));
    app.active.editor.search("value_", SearchDirection::Forward).context("prepare production search highlight")?;
    app.search_highlight = true;
    app.diagnostics.extend([
        QuickfixEntry::diagnostic(source_path, 1, 1, Severity::Warning, "deterministic benchmark warning"),
        QuickfixEntry::diagnostic(source_path, LARGE_RUST_LINES, 1, Severity::Error, "deterministic benchmark error"),
    ]);
    Ok(PreparedApp { app, syntax_spans, semantic_spans: semantic_spans_count })
}

fn start_probe(mut app: App, open_started: Instant) -> Result<StartedProbe> {
    let mut layout = ViewportLayout::new(WIDTH, HEIGHT);
    layout.configure_dotfile_profile();
    app.resize_terminal(HEIGHT, WIDTH);
    let backend = MeasuringBackend::new()?;
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
    let presented_epoch = presented.recv_timeout(Duration::from_secs(5)).context("first production frame was not presented")?;
    anyhow::ensure!(presented_epoch == first_epoch, "presenter completed an unexpected first epoch");
    let open_to_first_terminal_write_nanos = elapsed_nanos(open_started);
    Ok(StartedProbe { probe: Probe { app, layout, presenter, presented, samples: Vec::new() }, first_desired_frame_nanos, open_to_first_terminal_write_nanos })
}

fn run_probe_workload(probe: &mut Probe, iterations: u64) -> Result<u64> {
    probe.measure("cold_bottom_navigation", character('G'))?;

    let samples_per_case = iterations.saturating_add(4).saturating_div(5).max(1);
    for _ in 0..samples_per_case {
        probe.app.active.editor.set_cursor(0);
        probe.app.views.active_window_mut().top_line = 0;
        probe.app.schedule_provider_refreshes(probe.layout.height);
        probe.present_setup()?;
        probe.measure("bottom_navigation", character('G'))?;
    }
    for sample in 0..samples_per_case {
        probe.measure("local_motion", character(if sample % 2 == 0 { 'h' } else { 'l' }))?;
    }
    for sample in 0..samples_per_case {
        probe.measure("viewport_navigation", terminal_key(TerminalKeyCode::Char(if sample % 2 == 0 { 'u' } else { 'd' }), true))?;
    }

    probe.measure("enter_insert", character('i'))?;
    for sample in 0..samples_per_case {
        probe.measure(
            if sample % 2 == 0 { "insert_character" } else { "delete_character" },
            if sample % 2 == 0 { character('x') } else { terminal_key(TerminalKeyCode::Backspace, false) },
        )?;
    }
    if samples_per_case % 2 == 1 {
        probe.measure("delete_character", terminal_key(TerminalKeyCode::Backspace, false))?;
    }
    probe.measure("leave_insert", terminal_key(TerminalKeyCode::Escape, false))?;
    probe.measure("enter_visual", character('v'))?;
    for sample in 0..samples_per_case {
        probe.measure("selection_change", character(if sample % 2 == 0 { 'h' } else { 'l' }))?;
    }
    Ok(samples_per_case)
}
