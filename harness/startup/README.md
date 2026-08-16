# wren-startup

Measures the architecture's startup scenarios and records speculative-frame,
head-validated correct-frame, and interactive times separately. Scenarios A
and B1 gate below 2,788,576ns and 347,119ns p99 respectively; B2–D are
explicitly reported with cache-control limitations in the JSON metadata.

```sh
cargo run -p wren-startup --release -- --iterations 1000 --gate --output target/startup.json
```

Use `--b3-path` for a mounted network/FUSE file. The harness never labels an
ordinary local file as a network-filesystem measurement.
