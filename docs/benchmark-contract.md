# Benchmark contract

`wren-latency` records physical-input-to-transaction,
physical-input-to-workspace-grid, TaskCommand checkpoint/cancellation, and
terminal completion distributions. It also runs a production probe through the
real TUI `App`, including transaction side effects, syntax, semantic, search,
Git, diagnostic and selection decorations, overlays, workspace composition,
the presenter, and Termina serialization. The fixed production workload is a
14,000-line Rust file with at least 42,000 retained syntax spans. Each scenario's
baseline is fully presented before its physical-input clock begins, and setup
versus measured presentations are disclosed independently.

The semantic layer in this deterministic local workload is synthesized from
retained syntax spans and reported as `synthetic_semantic_spans`; it exercises
the same composition and rendering path without claiming a live LSP response.

Both the component and full-App desired-frame paths retain the architecture's
77,654ns p99 target and 100,000ns worst-observed maximum. The harness reports
and gates them separately; a passing component result cannot be cited as an
achieved product-frame result. The large-file probe additionally has hard
maximum gates of 500ms for open, 5ms for first desired-frame construction, and
600ms from open through the first completed terminal write. Task yield gates
below 62,258ns p99 and 63,969ns maximum. Termina serialization through the
fully-written presenter epoch gates below 103,626ns p99, requires one
completion per physical-input sample, and permits no dropped frames.

`wren-startup` records speculative, correct, and interactive timestamps for A,
B1, B2, C, and D. A loads an actual persistent `PublishedViewport` and validates
it through the daemon-style shared-memory head table. B1 uses a known deep byte
range and page-cache-hot `pread`. Scenario A gates below 2,788,576ns p99 and B1
below 347,119ns p99. B3 is emitted only when a caller supplies a mounted
network/FUSE file; the harness does not mislabel a local path. B2/D disclose
that portable user-space code does not control kernel cache eviction; the
bare-metal runner controls that environment.

The latency and startup harnesses emit HDR histogram JSON and accept CPU
affinity. Macro-replayed keys are excluded by construction.

`wren-tiling-performance` measures the complete empty-startup animation through
the production `App`: exact Penrose construction/vector composition, Presenter
publication/diffing, high-resolution quad rasterization, zlib/Kitty graphics
encoding, and completed Termina byte serialization. The isolated child
identifies as Ghostty so the full graphics path is measured. Its fixed
workloads are warm animation at 120×40 and 240×80, cold construction at
120×40, and alternating 120×40/160×50 resizes.
Desired-frame, diff, terminal-write, and complete-path p99 values each have hard
gates at the floor of 90% of their pre-optimization baselines. The complete path
also gates its worst observed sample. Its full-path limits are respectively
19,021,823ns, 170,930,994ns, 15,998,975ns, and 76,795,083ns; a faster component
cannot hide a regression in another stage or in the aggregate. Every published
sample must be fully written with no dropped frames.

`--gate` is authoritative only with `WREN_BARE_METAL=1`, an explicit `--cpu`,
and successful affinity. Hosted CI runs publish non-gating distributions. The
dedicated architecture runner additionally verifies its CPU governor; hardware
key-to-photon is a mandatory hard gate. `WREN_KEY_TO_PHOTON_JSON` must name a
physical-rig report with `schema`, `rig_id`, `samples`, `baseline_p99_nanos`,
and `measured_p99_nanos`; the harness computes a strict target at 81% of the
rig's recorded baseline, a second 10% cut from the prior target. A missing or
malformed report fails the gate.

`wren-system-gates` covers the remaining recorded families: per-DocumentClass
provider freshness and bounded latest-wins queue depth, snapshot retention
(`oldest_live_revision` and retained bytes), process peak/steady RSS, and
remote materialization convergence plus fsynced persisted-save latency. The
crash-recovery matrix is executed by the `wren-session` and `wren-sessiond`
fault-injection suites in the same CI run.
Provider-freshness p99 gates are 91,859ns for normal documents, 173,351ns for
large documents, and 202ns for pathological documents. Real dual-OpenSSH
convergence gates below 3,958,086ns p99 and fsynced persisted save below
16,004,872ns p99 for the checked-in loopback profile. Missing OpenSSH
configuration is a hard failure. Its authoritative remote gate additionally
requires active `tc netem`, a real dual-lane OpenSSH target, and a
profile-specific `WREN_REMOTE_BASELINE_JSON`; the harness derives strict 81%
targets from that profile rather than mislabeling loopback numbers. The local
materializer/cache probe is a non-authoritative algorithm smoke test.

Each numeric gate is the floor of 90% of the previous hard requirement.
Profile-derived gates are therefore the nested floor of 90% twice, or
approximately 81% of their original baseline measurement. Local profiles are
checked in; physical-rig and netem profiles travel with their
authoritative runner because they cannot be reproduced by a hosted process.
The performance changes that beat a gate are part of the product; harnesses may
not omit work, downgrade durability, discard terminal samples, or relabel a
software timestamp as a physical photon measurement.

The authoritative `architecture-gates` workflow job is unconditional. If its
labeled bare-metal runner or runner-local hardware/netem reports are absent,
the workflow remains incomplete or fails; absence can never become a successful
skip.

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
