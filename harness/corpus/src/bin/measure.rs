use std::env;
use std::fs::File;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
use wren_text::{CropText, PieceTreeStub, RopeyText, TextStore};
use wren_types::{DocumentRevision, Edit, Transaction};

#[derive(Debug, Serialize)]
struct Measurement {
    backend: &'static str,
    corpus: String,
    bytes: usize,
    load_ms: f64,
    snapshot_ns: f64,
    edit_us: f64,
    retained_32_us: f64,
    line_byte_ns: f64,
}

fn elapsed_per(duration: Duration, iterations: u32, scale: f64) -> f64 {
    duration.as_secs_f64() * scale / f64::from(iterations)
}

fn measure<T: TextStore>(backend: &'static str, path: &Path) -> Result<Measurement> {
    let started = Instant::now();
    let store = T::from_reader(File::open(path)?)?;
    let load = started.elapsed();
    let snapshot_iterations = 10_000;
    let started = Instant::now();
    for _ in 0..snapshot_iterations {
        black_box(store.snapshot());
    }
    let snapshot = started.elapsed();
    let whole = store.slice(0..store.len_bytes());
    let mut midpoint = store.len_bytes() / 2;
    while midpoint > 0 && !whole.is_char_boundary(midpoint) {
        midpoint -= 1;
    }
    let transaction = Transaction::new(DocumentRevision::new(0), vec![Edit::new(midpoint..midpoint, "x")])?;
    let edit_iterations = if store.len_bytes() > 16 * 1024 * 1024 { 8 } else { 100 };
    let started = Instant::now();
    for _ in 0..edit_iterations {
        let mut candidate = store.snapshot();
        candidate.apply(&transaction);
        black_box(candidate);
    }
    let edit = started.elapsed();
    let retain_iterations = 1_000;
    let started = Instant::now();
    for _ in 0..retain_iterations {
        let retained: Vec<T> = (0..32).map(|_| store.snapshot()).collect();
        black_box(retained);
    }
    let retained = started.elapsed();
    let conversion_iterations = if backend == "piece-stub" { if store.len_bytes() > 16 * 1024 * 1024 { 10 } else { 1_000 } } else { 100_000 };
    let started = Instant::now();
    for _ in 0..conversion_iterations {
        let line = store.line_of_byte(black_box(midpoint));
        black_box(store.byte_of_line(line));
    }
    let conversion = started.elapsed();
    Ok(Measurement {
        backend,
        corpus: path.file_name().and_then(|name| name.to_str()).unwrap_or("unknown").to_owned(),
        bytes: store.len_bytes(),
        load_ms: load.as_secs_f64() * 1_000.0,
        snapshot_ns: elapsed_per(snapshot, snapshot_iterations, 1_000_000_000.0),
        edit_us: elapsed_per(edit, edit_iterations, 1_000_000.0),
        retained_32_us: elapsed_per(retained, retain_iterations, 1_000_000.0),
        line_byte_ns: elapsed_per(conversion, conversion_iterations, 1_000_000_000.0),
    })
}

fn paths() -> Result<Vec<PathBuf>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    Ok([
        wren_benchmark_support::normal_rust_corpus()?,
        root.join("documents/unicode.txt"),
        root.join("generated/large-100mb.js"),
        root.join("generated/oneline-8mb.json"),
    ]
    .into_iter()
    .filter(|path| path.exists())
    .collect())
}

fn main() -> Result<()> {
    let mut measurements = Vec::new();
    for path in paths()? {
        measurements.push(measure::<RopeyText>("ropey", &path).with_context(|| path.display().to_string())?);
        measurements.push(measure::<CropText>("crop", &path).with_context(|| path.display().to_string())?);
        measurements.push(measure::<PieceTreeStub>("piece-stub", &path).with_context(|| path.display().to_string())?);
    }
    if env::args().any(|argument| argument == "--json") {
        println!("{}", serde_json::to_string_pretty(&measurements)?);
    } else {
        println!("| backend | corpus | bytes | load ms | snapshot ns | edit µs | retained-32 µs | line↔byte ns |");
        println!("|---|---|---:|---:|---:|---:|---:|---:|");
        for item in measurements {
            println!(
                "| {} | {} | {} | {:.3} | {:.1} | {:.3} | {:.3} | {:.1} |",
                item.backend, item.corpus, item.bytes, item.load_ms, item.snapshot_ns, item.edit_us, item.retained_32_us, item.line_byte_ns,
            );
        }
    }
    Ok(())
}
