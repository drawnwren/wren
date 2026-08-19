# ADR 0002: Use Crop as the Phase-0 default text store

- Status: accepted for Phase 0
- Date: 2026-08-14
- Machine: Apple M4, 16 GiB, arm64, macOS 26.5.2
- Toolchain: rustc 1.95.0, release profile

## Context

The text layer must choose a default only after measuring Ropey, Crop, and an
experimental piece-tree track on normal, large, long-line, and Unicode-heavy
documents. The on-demand `normal.rs` has 2,002 lines; `large-100mb.js` and
`oneline-8mb.json` are deterministic generated build artifacts.

## Measurements

The timing table comes from `cargo run -p wren-corpus --release --bin measure`.
It is a local decision run, not a CI threshold. Snapshot and retained-snapshot
figures include destruction and black-boxing; edits are midpoint insertions on
fresh snapshots.

| backend | corpus | bytes | load ms | snapshot ns | edit µs | retained-32 µs | line↔byte ns |
|---|---|---:|---:|---:|---:|---:|---:|
| ropey | normal.rs | 62,189 | 0.800 | 3.6 | 0.552 | 0.117 | 121.9 |
| crop | normal.rs | 62,189 | 0.133 | 3.6 | 0.821 | 0.122 | 33.6 |
| piece-stub | normal.rs | 62,189 | 0.044 | 3.8 | 4.747 | 0.119 | 18,373.2 |
| ropey | unicode.txt | 296,960 | 1.263 | 3.1 | 0.320 | 0.106 | 203.2 |
| crop | unicode.txt | 296,960 | 0.256 | 3.5 | 0.223 | 0.123 | 36.8 |
| piece-stub | unicode.txt | 296,960 | 0.215 | 4.0 | 18.154 | 0.117 | 75,048.0 |
| ropey | large-100mb.js | 104,857,600 | 83.072 | 4.3 | 1.943 | 0.118 | 97.2 |
| crop | large-100mb.js | 104,857,600 | 34.370 | 3.6 | 9.365 | 0.110 | 75.1 |
| piece-stub | large-100mb.js | 104,857,600 | 24.030 | 3.2 | 42,081.255 | 0.117 | 27,759,808.3 |
| ropey | oneline-8mb.json | 8,388,608 | 6.896 | 3.5 | 0.647 | 0.116 | 66.5 |
| crop | oneline-8mb.json | 8,388,608 | 1.585 | 4.2 | 0.782 | 0.111 | 35.7 |
| piece-stub | oneline-8mb.json | 8,388,608 | 1.722 | 3.6 | 2,203.308 | 0.118 | 558,891.7 |

The 100 MB heap run uses the checked-in dhat example with 32 retained
snapshots:

| backend | steady bytes | peak bytes | total allocated bytes |
|---|---:|---:|---:|
| ropey | 119,039,232 | 119,040,024 | 119,041,312 |
| crop | 108,790,312 | 214,156,288 | 214,723,672 |
| piece-stub | 104,858,128 | 209,715,216 | 209,715,728 |

## Decision

`CropText` is exported as `DefaultText`. It wins load time, steady memory, and
line conversion on the measured inputs while retaining low-microsecond edits
and O(1)-class snapshots. The 34 ms 100 MB load is above a 10 ms aspirational
first-frame budget, but this benchmark measures full ingestion rather than a
bounded viewport read. That result keeps the mmap-base piece-tree investigation
justified; it does not justify selecting the current string-backed stub.

## Alternatives rejected

- Ropey: mature and competitive edits, but slower loads and higher steady heap
  in this run.
- Piece-tree stub: fastest raw load and lowest steady heap, but its full-copy
  edits and linear line lookup are non-interactive. It is not the proposed mmap
  implementation and must not be marketed as one.
- Immediate custom mmap piece tree: rejected until these measurements existed;
  it remains follow-up work targeted at first-frame rather than full-load time.

## Reproduction

```console
cargo run -p wren-corpus --bin wren-corpus -- generate
cargo run -p wren-corpus --release --bin measure
cargo bench -p wren-text --bench textstore
cargo run -p wren-text --release --example textstore_memory \
  --features memory-profiling -- crop harness/corpus/generated/large-100mb.js
```
