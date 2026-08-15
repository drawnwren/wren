use std::env;
use std::fs::File;
use std::hint::black_box;

use wren_text::{CropText, PieceTreeStub, RopeyText, TextStore};

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

fn profile<T: TextStore>(path: &str) -> anyhow::Result<()> {
    let store = T::from_reader(File::open(path)?)?;
    let snapshots: Vec<T> = (0..32).map(|_| store.snapshot()).collect();
    black_box((&store, &snapshots));
    let stats = dhat::HeapStats::get();
    println!(
        "steady_bytes={} peak_bytes={} total_bytes={}",
        stats.curr_bytes, stats.max_bytes, stats.total_bytes
    );
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let arguments: Vec<String> = env::args().collect();
    let backend = arguments.get(1).map(String::as_str).unwrap_or("ropey");
    let path = arguments
        .get(2)
        .ok_or_else(|| anyhow::anyhow!("usage: textstore_memory BACKEND CORPUS"))?;
    let _profiler = dhat::Profiler::new_heap();
    match backend {
        "ropey" => profile::<RopeyText>(path),
        "crop" => profile::<CropText>(path),
        "piece-stub" => profile::<PieceTreeStub>(path),
        other => Err(anyhow::anyhow!("unknown backend: {other}")),
    }
}
