# wren-latency

**Layer:** OS-enabled performance harness. **Phase:** 0 measurement validation.

Measures real editor insert/delete/motion/operator/selection/viewport and
completion-acceptance paths through desired-frame readiness, with optional CPU
pinning and an isolated presenter thread. Realtime-command p99 and task-yield
p99 gate below 1ms; task yield also retains a 4ms maximum safety ceiling. It
may use OS facilities; production crates must never depend on it.
