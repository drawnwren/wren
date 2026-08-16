# Benchmark contract

`wren-latency` records physical-input-to-transaction, physical-input-to-grid,
TaskCommand checkpoint/cancellation, and terminal completion distributions.
Every recorded latency family is hard. Aggregate desired-grid p99 gates below
86,283ns, with workload-specific p99 gates embedded in the report. Task yield
gates below 69,176ns p99 and 71,077ns maximum. Termina serialization through
the fully-written presenter epoch gates below 115,141ns p99, requires one
completion per physical-input sample, and permits no dropped frames.

`wren-startup` records speculative, correct, and interactive timestamps for A,
B1, B2, C, and D. A loads an actual persistent `PublishedViewport` and validates
it through the daemon-style shared-memory head table. B1 uses a known deep byte
range and page-cache-hot `pread`. Scenario A gates below 3,098,418ns p99 and B1
below 385,688ns p99. B3 is emitted only when a caller supplies a mounted
network/FUSE file; the harness does not mislabel a local path. B2/D disclose
that portable user-space code does not control kernel cache eviction; the
bare-metal runner controls that environment.

Both harnesses emit HDR histogram JSON and accept CPU affinity. Macro-replayed
keys are excluded by construction.

`--gate` is authoritative only with `WREN_BARE_METAL=1`, an explicit `--cpu`,
and successful affinity. Hosted CI runs publish non-gating distributions. The
dedicated architecture runner additionally verifies its CPU governor; hardware
key-to-photon is a mandatory hard gate. `WREN_KEY_TO_PHOTON_JSON` must name a
physical-rig report with `schema`, `rig_id`, `samples`, `baseline_p99_nanos`,
and `measured_p99_nanos`; the harness computes a strict target at 90% of the
rig's recorded baseline. A missing or malformed report fails the gate.

`wren-system-gates` covers the remaining recorded families: per-DocumentClass
provider freshness and bounded latest-wins queue depth, snapshot retention
(`oldest_live_revision` and retained bytes), process peak/steady RSS, and
remote materialization convergence plus fsynced persisted-save latency. The
crash-recovery matrix is executed by the `wren-session` and `wren-sessiond`
fault-injection suites in the same CI run.
Provider-freshness p99 gates are 102,066ns for normal documents, 192,613ns for
large documents, and 225ns for pathological documents. Real dual-OpenSSH
convergence gates below 4,397,874ns p99 and fsynced persisted save below
17,783,192ns p99 for the checked-in loopback profile. Missing OpenSSH
configuration is a hard failure. Its authoritative remote gate additionally
requires active `tc netem`, a real dual-lane OpenSSH target, and a
profile-specific `WREN_REMOTE_BASELINE_JSON`; the harness derives strict 90%
targets from that profile rather than mislabeling loopback numbers. The local
materializer/cache probe is a non-authoritative algorithm smoke test.

Each numeric gate is the floor of 90% of its baseline measurement. Local
profiles are checked in; physical-rig and netem profiles travel with their
authoritative runner because they cannot be reproduced by a hosted process.
The performance changes that beat a gate are part of the product; harnesses may
not omit work, downgrade durability, discard terminal samples, or relabel a
software timestamp as a physical photon measurement.

`wren-provider` also owns a comparative Criterion benchmark for GPU lexical
classification. It times identical 4 MiB, 8 MiB, and 32 MiB
generated-provider demands through `ProviderActor`, validates byte-identical
CPU/GPU responses before each comparison, excludes lazy adapter initialization
from the timed region, and reports bytes per second. Software adapters are
rejected; machines without a hardware GPU record only the CPU baselines. This
comparison documents the 4 MiB routing crossover and is smoke-tested in CI,
but it is not a portable hard architecture gate because hosted runner GPU
availability and models vary.
The macOS hardware-GPU job does enforce a same-run relative guard: each GPU
median must be at least 1.5 times faster than its matching CPU median, and the
Criterion report is retained as a CI artifact.
