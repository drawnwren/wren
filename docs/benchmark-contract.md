# Benchmark contract

`wren-latency` records physical-input-to-transaction, physical-input-to-grid,
TaskCommand checkpoint/cancellation, and terminal completion distributions.
Realtime desired grids now gate at p99 <1ms. Task yield gaps gate at p99 <1ms
while retaining the architecture's <4ms maximum safety ceiling. Terminal write
and dropped-frame backpressure are reported separately.

`wren-startup` records speculative, correct, and interactive timestamps for A,
B1, B2, C, and D. A loads an actual persistent `PublishedViewport` and validates
it through the daemon-style shared-memory head table. B1 uses a known deep byte
range and page-cache-hot `pread`. A/B1 gate at 5ms. B3 is emitted only when a
caller supplies a mounted network/FUSE file; the harness does not mislabel a
local path. B2/D disclose that portable user-space code does not control kernel
cache eviction; the bare-metal runner controls that environment.

Both harnesses emit HDR histogram JSON and accept CPU affinity. Macro-replayed
keys are excluded by construction.

`--gate` is authoritative only with `WREN_BARE_METAL=1`, an explicit `--cpu`,
and successful affinity. Hosted CI runs publish non-gating distributions. The
dedicated architecture runner additionally verifies its CPU governor; hardware
key-to-photon JSON is included when supplied and is never a gate.

`wren-system-gates` covers the remaining recorded families: per-DocumentClass
provider freshness and bounded latest-wins queue depth, snapshot retention
(`oldest_live_revision` and retained bytes), process peak/steady RSS, and
remote materialization convergence plus fsynced persisted-save latency. The
crash-recovery matrix is executed by the `wren-session` and `wren-sessiond`
fault-injection suites in the same CI run.
Provider-freshness p99 gates are 2ms for normal documents, 1ms for large
documents, and 0.25ms for pathological documents.
Its remote gate additionally requires active `tc netem` and a real dual-lane
OpenSSH target; the local materializer/cache probe is labeled as a non-
authoritative algorithm smoke test.
