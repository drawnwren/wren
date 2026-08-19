#![deny(unsafe_code)]

use std::env;
use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Instant;

use anyhow::{Context, Result};
use hdrhistogram::Histogram;
use serde_json::{Value, json};

#[derive(Debug)]
pub struct CommonArguments {
    pub iterations: u64,
    pub cpu: Option<usize>,
    pub output: Option<PathBuf>,
    pub gate: bool,
}

impl CommonArguments {
    #[must_use]
    pub const fn new(iterations: u64) -> Self {
        Self { iterations, cpu: None, output: None, gate: false }
    }

    pub fn consume(&mut self, argument: &str, cursor: &mut ArgumentCursor) -> Result<bool> {
        match argument {
            "--iterations" => self.iterations = cursor.value(argument)?,
            "--cpu" => self.cpu = Some(cursor.value(argument)?),
            "--output" => self.output = Some(cursor.path(argument)?),
            "--gate" => self.gate = true,
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.iterations > 0, "--iterations must be positive");
        Ok(())
    }
}

pub struct ArgumentCursor {
    values: std::vec::IntoIter<String>,
}

impl ArgumentCursor {
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(env::args().skip(1))
    }

    #[must_use]
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        Self { values: values.into_iter().collect::<Vec<_>>().into_iter() }
    }

    pub fn value<T>(&mut self, option: &str) -> Result<T>
    where
        T: FromStr,
        T::Err: std::error::Error + Send + Sync + 'static,
    {
        self.next().with_context(|| format!("{option} needs a value"))?.parse().with_context(|| format!("invalid value for {option}"))
    }

    pub fn path(&mut self, option: &str) -> Result<PathBuf> {
        self.next().map(PathBuf::from).with_context(|| format!("{option} needs a value"))
    }
}

impl Iterator for ArgumentCursor {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        self.values.next()
    }
}

#[must_use]
pub const fn ten_percent_cut(prior_gate_nanos: u64) -> u64 {
    prior_gate_nanos.saturating_mul(9) / 10
}

pub fn histogram() -> Result<Histogram<u64>> {
    Histogram::new_with_bounds(1, 60_000_000_000, 3).map_err(Into::into)
}

/// A latency distribution and its exact worst observation. HDR histograms
/// quantize bucket maxima, so hard real-time gates must retain the original
/// maximum beside the distribution rather than reconstructing it later.
#[derive(Clone)]
pub struct SampleSeries {
    histogram: Histogram<u64>,
    maximum: u64,
}

impl SampleSeries {
    pub fn new() -> Result<Self> {
        Ok(Self { histogram: histogram()?, maximum: 0 })
    }

    pub fn record(&mut self, value: u64) -> Result<(), hdrhistogram::RecordError> {
        self.histogram.record(value)?;
        self.maximum = self.maximum.max(value);
        Ok(())
    }

    #[must_use]
    pub const fn maximum(&self) -> u64 {
        self.maximum
    }
}

impl Deref for SampleSeries {
    type Target = Histogram<u64>;

    fn deref(&self) -> &Self::Target {
        &self.histogram
    }
}

#[must_use]
pub fn percentiles(histogram: &Histogram<u64>) -> Value {
    json!({
        "min": histogram.min(),
        "p50": histogram.value_at_quantile(0.50),
        "p90": histogram.value_at_quantile(0.90),
        "p99": histogram.value_at_quantile(0.99),
        "max": histogram.max(),
    })
}

#[must_use]
pub fn distribution(histogram: &Histogram<u64>) -> Value {
    let Value::Object(mut report) = percentiles(histogram) else {
        unreachable!("percentile report is always an object");
    };
    report.insert("samples".to_owned(), histogram.len().into());
    Value::Object(report)
}

#[must_use]
pub fn elapsed_nanos(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX).max(1)
}

#[must_use]
pub fn pin_requested_cpu(cpu: Option<usize>) -> bool {
    cpu.and_then(|index| core_affinity::get_core_ids().and_then(|ids| ids.get(index).copied())).is_some_and(core_affinity::set_for_current)
}

#[must_use]
pub fn bare_metal_declared() -> bool {
    env::var("WREN_BARE_METAL").as_deref() == Ok("1")
}

pub fn require_bare_metal_cpu(required: bool, cpu: Option<usize>, pinned: bool, action: &str) -> Result<()> {
    if !required {
        return Ok(());
    }
    anyhow::ensure!(bare_metal_declared(), "{action} requires WREN_BARE_METAL=1 on the dedicated benchmark runner");
    anyhow::ensure!(cpu.is_some(), "{action} requires --cpu");
    anyhow::ensure!(pinned, "requested benchmark CPU could not be pinned");
    Ok(())
}

pub fn emit_report(report: &Value, output: Option<&Path>) -> Result<()> {
    let rendered = serde_json::to_string_pretty(report)?;
    if let Some(path) = output {
        fs::write(path, format!("{rendered}\n")).with_context(|| format!("write {}", path.display()))?;
    }
    println!("{rendered}");
    Ok(())
}

/// Materializes the deterministic, real-looking Rust document used by text
/// and editor benchmarks. Generated corpus data belongs beside other temporary
/// benchmark artifacts rather than in the Rust source tree.
pub fn normal_rust_corpus() -> Result<PathBuf> {
    let directory = env::temp_dir().join("wren-benchmark-corpus-v1");
    fs::create_dir_all(&directory).context("create benchmark corpus directory")?;
    let path = directory.join("normal.rs");
    let temporary = directory.join(format!("normal-{}.tmp", std::process::id()));
    let mut output = String::from("// Deterministic, real-looking Rust benchmark corpus.\n\n");
    for module in 0..250 {
        output.push_str(&format!(
            "pub fn transform_{module}(input: &[u64]) -> u64 {{\n    input.iter().copied()\n        .map(|value| value.rotate_left({}))\n        .filter(|value| value & 1 == 0)\n        .fold({module}, |sum, value| sum.wrapping_add(value))\n}}\n\nconst CHECK_{module}: u64 = 0x{module:08x};\n",
            module % 63 + 1
        ));
    }
    fs::write(&temporary, output).context("write benchmark Rust corpus")?;
    fs::rename(&temporary, &path).context("install benchmark Rust corpus")?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_arguments_and_custom_options_share_one_cursor() {
        let mut cursor = ArgumentCursor::new(["--iterations", "12", "--custom", "value", "--gate"].into_iter().map(str::to_owned));
        let mut common = CommonArguments::new(1);
        let mut custom = None;
        while let Some(argument) = cursor.next() {
            if common.consume(&argument, &mut cursor).expect("common option") {
                continue;
            }
            match argument.as_str() {
                "--custom" => custom = Some(cursor.value::<String>(&argument).expect("value")),
                _ => panic!("unexpected option"),
            }
        }
        assert_eq!(common.iterations, 12);
        assert!(common.gate);
        assert_eq!(custom.as_deref(), Some("value"));
    }

    #[test]
    fn generated_rust_corpus_preserves_the_benchmark_fixture() {
        let path = normal_rust_corpus().expect("generate normal Rust corpus");
        let source = fs::read_to_string(path).expect("read normal Rust corpus");
        assert_eq!(source.len(), 62_189);
        assert_eq!(source.lines().count(), 2_002);
        assert!(source.contains("pub fn transform_249"));
    }
}
