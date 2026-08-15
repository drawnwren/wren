# wren-system-gates

Cross-phase observability harness for provider freshness/queue depth, snapshot
retention, process memory, and remote convergence/persisted-save latency. It
emits HDR distributions. A `--gate` run is accepted only on a CPU-pinned
bare-metal runner with active `tc netem` and a real dual-OpenSSH remote target;
ordinary hosted runs are explicitly non-authoritative reports.
