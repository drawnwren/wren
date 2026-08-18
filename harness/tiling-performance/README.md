# wren-tiling-performance

Measures Wren's complete empty-startup Penrose render path: production `App`
composition, exact tiling/raster work, half-block cell construction, desired
grid publication, Presenter diffing, and completed Termina byte serialization.

The fixed scenarios cover warm animation at 120×40 and 240×80, cold tiling
construction at 120×40, and alternating 120×40/160×50 resize frames. Reports
separate desired-frame, diff, terminal-write, and aggregate distributions.
Every stage has a hard p99 target more than 10% below the pre-optimization
measurement, and the aggregate path also gates the worst observed frame. A run
fails if the Presenter drops a frame or does not fully write every publication.

```sh
cargo run -p wren-tiling-performance --release -- \
  --iterations 1000 --output target/tiling-performance.json
```

Authoritative gates use the same isolated, pinned-runner contract as the app
benchmarks:

```sh
WREN_BARE_METAL=1 cargo run -p wren-tiling-performance --release -- \
  --iterations 1000 --cpu 0 --gate --output target/tiling-performance-gate.json
```
