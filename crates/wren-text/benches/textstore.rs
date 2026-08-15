use std::fs::File;
use std::hint::black_box;
use std::path::{Path, PathBuf};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use wren_text::{CropText, PieceTreeStub, RopeyText, TextStore};
use wren_types::{DocumentRevision, Edit, Transaction};

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/corpus")
}

fn corpus_files() -> Vec<(&'static str, PathBuf)> {
    let root = corpus_root();
    [
        ("normal", root.join("documents/normal.rs")),
        ("unicode", root.join("documents/unicode.txt")),
        ("large-100mb", root.join("generated/large-100mb.js")),
        ("oneline-8mb", root.join("generated/oneline-8mb.json")),
    ]
    .into_iter()
    .filter(|(_, path)| path.exists())
    .collect()
}

fn load<T: TextStore>(path: &Path) -> T {
    let file = File::open(path).expect("generate the corpus before benchmarking");
    T::from_reader(file).expect("corpus is valid UTF-8")
}

fn deterministic_edits(text: &str, count: usize) -> Transaction {
    let boundaries: Vec<usize> = text
        .char_indices()
        .map(|(byte, _)| byte)
        .chain(std::iter::once(text.len()))
        .collect();
    let mut state = 0x9e37_79b9_usize;
    let mut points = Vec::with_capacity(count);
    for _ in 0..count {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        points.push(boundaries[state % boundaries.len()]);
    }
    points.sort_unstable();
    points.dedup();
    Transaction::new(
        DocumentRevision::new(0),
        points
            .into_iter()
            .map(|point| Edit::new(point..point, "x"))
            .collect(),
    )
    .expect("deterministic edit points are valid")
}

fn bench_loads(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("load");
    for (name, path) in corpus_files() {
        group.bench_with_input(BenchmarkId::new("ropey", name), &path, |bencher, path| {
            bencher.iter(|| black_box(load::<RopeyText>(path)));
        });
        group.bench_with_input(BenchmarkId::new("crop", name), &path, |bencher, path| {
            bencher.iter(|| black_box(load::<CropText>(path)));
        });
        group.bench_with_input(
            BenchmarkId::new("piece-stub", name),
            &path,
            |bencher, path| {
                bencher.iter(|| black_box(load::<PieceTreeStub>(path)));
            },
        );
    }
    group.finish();
}

fn bench_operations_for<T: TextStore>(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    backend: &str,
    corpus: &str,
    store: &T,
) {
    group.bench_function(
        BenchmarkId::new(format!("snapshot/{backend}"), corpus),
        |bencher| {
            bencher.iter(|| black_box(store.snapshot()));
        },
    );
    let whole = store.slice(0..store.len_bytes());
    let mut midpoint = store.len_bytes() / 2;
    while midpoint > 0 && !whole.is_char_boundary(midpoint) {
        midpoint -= 1;
    }
    let transaction = deterministic_edits(&whole, 16);
    group.bench_function(
        BenchmarkId::new(format!("edit/{backend}"), corpus),
        |bencher| {
            bencher.iter_batched(
                || store.snapshot(),
                |mut candidate| {
                    candidate.apply(black_box(&transaction));
                    black_box(candidate)
                },
                criterion::BatchSize::SmallInput,
            );
        },
    );
    group.bench_function(
        BenchmarkId::new(format!("edit-retained-32/{backend}"), corpus),
        |bencher| {
            bencher.iter(|| {
                let retained: Vec<T> = (0..32).map(|_| store.snapshot()).collect();
                let mut candidate = store.snapshot();
                candidate.apply(&transaction);
                black_box((candidate, retained))
            });
        },
    );
    group.bench_function(
        BenchmarkId::new(format!("line-byte/{backend}"), corpus),
        |bencher| {
            bencher.iter(|| {
                let line = store.line_of_byte(black_box(midpoint));
                black_box(store.byte_of_line(line))
            });
        },
    );
}

fn bench_operations(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("operations");
    for (name, path) in corpus_files() {
        let ropey = load::<RopeyText>(&path);
        let crop = load::<CropText>(&path);
        let piece = load::<PieceTreeStub>(&path);
        bench_operations_for(&mut group, "ropey", name, &ropey);
        bench_operations_for(&mut group, "crop", name, &crop);
        bench_operations_for(&mut group, "piece-stub", name, &piece);
    }
    group.finish();
}

criterion_group!(benches, bench_loads, bench_operations);
criterion_main!(benches);
