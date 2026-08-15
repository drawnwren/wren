# wren-system-gates

Cross-phase observability harness for provider freshness/queue depth, snapshot
retention, process memory, and remote convergence/persisted-save latency. It
emits HDR distributions and gates provider-freshness p99 at 2ms/1ms/0.25ms for
normal/large/pathological documents. A `--gate` run is accepted only on a
CPU-pinned bare-metal runner with active `tc netem` and a real dual-OpenSSH
remote target; ordinary hosted runs are explicitly non-authoritative reports.
