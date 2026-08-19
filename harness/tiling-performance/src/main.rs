use std::collections::BTreeMap;
use std::env;
use std::process::Command;

use anyhow::{Context, Result};
use hdrhistogram::Histogram;
use serde_json::{Value, json};
use wren_benchmark_support::{
    ArgumentCursor, CommonArguments, bare_metal_declared, distribution, emit_report, histogram, pin_requested_cpu, require_bare_metal_cpu, ten_percent_cut,
};
use wren_tui::{TilingPerformanceReport, run_tiling_performance_probe};

type Arguments = CommonArguments;

#[derive(Debug, Clone, Copy)]
struct ScenarioGate {
    name: &'static str,
    desired: StageBaseline,
    diff: StageBaseline,
    terminal: StageBaseline,
    full: StageBaseline,
}

#[derive(Debug, Clone, Copy)]
struct StageBaseline {
    p99_nanos: u64,
    maximum_nanos: u64,
    maximum_hard_gate: bool,
}

const fn baseline(p99_nanos: u64, maximum_nanos: u64) -> StageBaseline {
    StageBaseline { p99_nanos, maximum_nanos, maximum_hard_gate: false }
}

const fn full_baseline(p99_nanos: u64, maximum_nanos: u64) -> StageBaseline {
    StageBaseline { p99_nanos, maximum_nanos, maximum_hard_gate: true }
}

// Captured from the unoptimized full-path probe on this fixed workload. Every
// hard gate is a strict additional ten-percent cut, matching the app benchmark
// contract. Every stage's p99 and the full path's worst sample must improve.
const SCENARIO_GATES: [ScenarioGate; 4] = [
    ScenarioGate {
        name: "animated_120x40",
        desired: baseline(1_100_799, 1_100_799),
        diff: baseline(25_167, 25_167),
        terminal: baseline(20_250_623, 20_250_623),
        full: full_baseline(21_135_359, 21_135_359),
    },
    ScenarioGate {
        name: "animated_240x80",
        desired: baseline(1_690_623, 1_690_623),
        diff: baseline(73_343, 73_343),
        terminal: baseline(188_481_535, 188_481_535),
        full: full_baseline(189_923_327, 189_923_327),
    },
    ScenarioGate {
        name: "cold_120x40",
        desired: baseline(630_271, 630_271),
        diff: baseline(2_125, 2_125),
        terminal: baseline(17_252_351, 17_252_351),
        full: full_baseline(17_776_639, 17_776_639),
    },
    ScenarioGate {
        name: "resize_120x40_160x50",
        desired: baseline(917_503, 917_503),
        diff: baseline(1_667, 1_667),
        terminal: baseline(84_475_903, 84_475_903),
        full: full_baseline(85_327_871, 85_327_871),
    },
];

fn arguments() -> Result<Arguments> {
    let mut arguments = Arguments::new(1_000);
    let mut cursor = ArgumentCursor::from_env();
    while let Some(argument) = cursor.next() {
        if !arguments.consume(&argument, &mut cursor)? {
            anyhow::bail!("unknown argument: {argument}");
        }
    }
    arguments.validate()?;
    Ok(arguments)
}

#[derive(Debug)]
struct ScenarioMetrics {
    desired: Histogram<u64>,
    diff: Histogram<u64>,
    terminal: Histogram<u64>,
    full: Histogram<u64>,
    terminal_bytes: Histogram<u64>,
    terminal_patches: Histogram<u64>,
}

impl ScenarioMetrics {
    fn new() -> Result<Self> {
        Ok(Self {
            desired: histogram()?,
            diff: histogram()?,
            terminal: histogram()?,
            full: histogram()?,
            terminal_bytes: histogram()?,
            terminal_patches: histogram()?,
        })
    }
}

fn isolate_probe(iterations: u64) -> Result<TilingPerformanceReport> {
    let isolated = tempfile::tempdir().context("create isolated tiling benchmark home")?;
    let output = Command::new(env::current_exe().context("locate tiling benchmark executable")?)
        .arg("--probe-child")
        .arg(iterations.to_string())
        .current_dir(isolated.path())
        .env("HOME", isolated.path())
        .env("XDG_STATE_HOME", isolated.path().join("state"))
        .env("XDG_DATA_HOME", isolated.path().join("data"))
        .env("XDG_CONFIG_HOME", isolated.path().join("config"))
        .output()
        .context("run isolated tiling performance probe")?;
    anyhow::ensure!(output.status.success(), "tiling performance probe failed: {}", String::from_utf8_lossy(&output.stderr));
    let report: TilingPerformanceReport = serde_json::from_slice(&output.stdout).context("decode tiling performance probe")?;
    anyhow::ensure!(report.schema == 1, "unsupported tiling probe schema");
    anyhow::ensure!(report.requested_iterations == iterations, "tiling probe iteration count changed");
    anyhow::ensure!(
        u64::try_from(report.samples.len()).unwrap_or(u64::MAX) == iterations.saturating_mul(SCENARIO_GATES.len() as u64),
        "tiling probe returned an incomplete sample set"
    );
    Ok(report)
}

fn metrics(report: &TilingPerformanceReport) -> Result<BTreeMap<Box<str>, ScenarioMetrics>> {
    let mut metrics = BTreeMap::<Box<str>, ScenarioMetrics>::new();
    for sample in &report.samples {
        let scenario = metrics.entry(sample.scenario.clone()).or_insert(ScenarioMetrics::new()?);
        scenario.desired.record(sample.desired_frame_nanos)?;
        scenario.diff.record(sample.diff_nanos)?;
        scenario.terminal.record(sample.terminal_write_nanos)?;
        scenario.full.record(sample.full_render_nanos)?;
        scenario.terminal_bytes.record(sample.terminal_bytes.max(1))?;
        scenario.terminal_patches.record(u64::try_from(sample.terminal_patches).unwrap_or(u64::MAX).max(1))?;
    }
    Ok(metrics)
}

fn stage_report(histogram: &Histogram<u64>, baseline: StageBaseline) -> (Value, bool) {
    let p99_gate_nanos = ten_percent_cut(baseline.p99_nanos);
    let maximum_gate_nanos = baseline.maximum_hard_gate.then(|| ten_percent_cut(baseline.maximum_nanos));
    let observed_p99_nanos = histogram.value_at_quantile(0.99);
    let observed_maximum_nanos = histogram.max();
    let p99_passed = observed_p99_nanos < p99_gate_nanos;
    let maximum_passed = maximum_gate_nanos.map(|maximum_gate_nanos| observed_maximum_nanos < maximum_gate_nanos);
    let passed = p99_passed && maximum_passed.unwrap_or(true);
    (
        json!({
            "hard_gate": true,
            "maximum_hard_gate": baseline.maximum_hard_gate,
            "baseline_p99_nanos": baseline.p99_nanos,
            "baseline_maximum_nanos": baseline.maximum_nanos,
            "p99_gate_nanos": p99_gate_nanos,
            "maximum_gate_nanos": maximum_gate_nanos,
            "p99_passed": p99_passed,
            "maximum_passed": maximum_passed,
            "passed": passed,
            "distribution": distribution(histogram),
        }),
        passed,
    )
}

fn scenario_report(metrics: &BTreeMap<Box<str>, ScenarioMetrics>) -> Result<(Value, bool)> {
    anyhow::ensure!(metrics.len() == SCENARIO_GATES.len(), "tiling probe returned {} scenarios, expected {}", metrics.len(), SCENARIO_GATES.len());
    let mut report = serde_json::Map::new();
    let mut all_passed = true;
    for gate in SCENARIO_GATES {
        let metrics = metrics.get(gate.name).with_context(|| format!("tiling probe omitted scenario {}", gate.name))?;
        let (desired, desired_passed) = stage_report(&metrics.desired, gate.desired);
        let (diff, diff_passed) = stage_report(&metrics.diff, gate.diff);
        let (terminal, terminal_passed) = stage_report(&metrics.terminal, gate.terminal);
        let (full, full_passed) = stage_report(&metrics.full, gate.full);
        let passed = desired_passed && diff_passed && terminal_passed && full_passed;
        all_passed &= passed;
        report.insert(
            gate.name.to_owned(),
            json!({
                "passed": passed,
                "desired_frame": desired,
                "grid_diff": diff,
                "termina_write": terminal,
                "full_render_path": full,
                "terminal_bytes": distribution(&metrics.terminal_bytes),
                "terminal_patches": distribution(&metrics.terminal_patches),
            }),
        );
    }
    Ok((Value::Object(report), all_passed))
}

fn main() -> Result<()> {
    let process_arguments = env::args().skip(1).collect::<Vec<_>>();
    if process_arguments.first().map(String::as_str) == Some("--probe-child") {
        let iterations =
            process_arguments.get(1).context("probe child requires an iteration count")?.parse::<u64>().context("invalid probe child iteration count")?;
        serde_json::to_writer(std::io::stdout().lock(), &run_tiling_performance_probe(iterations)?)?;
        return Ok(());
    }

    let arguments = arguments()?;
    let pinned = pin_requested_cpu(arguments.cpu);
    require_bare_metal_cpu(arguments.gate, arguments.cpu, pinned, "--gate")?;
    let probe = isolate_probe(arguments.iterations)?;
    let metrics = metrics(&probe)?;
    anyhow::ensure!(
        metrics.values().all(|scenario| scenario.full.len() == arguments.iterations),
        "tiling probe did not return the requested samples for every scenario"
    );
    let (scenarios, scenarios_passed) = scenario_report(&metrics)?;
    let presenter_passed = probe.dropped_frames == 0
        && probe.presented_frames == probe.published_frames
        && probe.published_frames == probe.setup_presentations.saturating_add(arguments.iterations.saturating_mul(4));
    let passed = scenarios_passed && presenter_passed;
    let report = json!({
        "schema": 1,
        "workload": probe.workload,
        "requested_iterations": arguments.iterations,
        "unit": "nanoseconds",
        "passed": passed,
        "scenarios": scenarios,
        "baseline_contract": {
            "kind": "fixed_pre_optimization_full_path",
            "samples_per_scenario": 50,
            "required_improvement": "strictly_more_than_ten_percent_for_every_stage_p99_and_full_path_maximum",
        },
        "presenter": {
            "hard_gate": true,
            "setup_presentations": probe.setup_presentations,
            "published_frames": probe.published_frames,
            "dropped_frames": probe.dropped_frames,
            "presented_frames": probe.presented_frames,
            "passed": presenter_passed,
        },
        "runner_contract": {
            "bare_metal_declared": bare_metal_declared(),
            "cpu_pinned": pinned,
            "fixed_workloads": true,
            "render_path": "full_tui_app_penrose_grid_presenter_diff_termina_completion",
            "gate_authoritative": arguments.gate,
        },
    });
    emit_report(&report, arguments.output.as_deref())?;
    if arguments.gate {
        anyhow::ensure!(
            passed,
            "tiling desired-frame, diff, terminal-write, presenter completion, or full-render p99/full-render maximum exceeded its strict pre-optimization gate"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_hard_gate_is_strictly_below_its_pre_optimization_baseline() {
        for scenario in SCENARIO_GATES {
            for stage in [scenario.desired, scenario.diff, scenario.terminal, scenario.full] {
                assert_eq!(ten_percent_cut(stage.p99_nanos), stage.p99_nanos * 9 / 10);
                assert!(ten_percent_cut(stage.p99_nanos) < stage.p99_nanos);
                if stage.maximum_hard_gate {
                    assert!(ten_percent_cut(stage.maximum_nanos) < stage.maximum_nanos);
                }
            }
            assert!(scenario.full.maximum_hard_gate);
        }
    }
}
