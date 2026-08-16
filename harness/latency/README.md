# wren-latency

**Layer:** OS-enabled performance harness. **Phase:** 0 measurement validation.

Measures real editor insert/delete/motion/operator/selection/viewport and
completion-acceptance paths through desired-frame readiness, with optional CPU
pinning and an isolated presenter thread. Desired-frame, task-yield,
fully-written terminal, and physical key-to-photon measurements are hard gates.
Terminal samples include Termina serialization, must complete one-for-one, and
may not drop frames. The physical rig report is mandatory for `--gate`. It may
use OS facilities; production crates must never depend on it.

The rig report is intentionally physical and minimal:

```json
{
  "schema": 1,
  "rig_id": "photodiode-rig-name",
  "samples": 10000,
  "baseline_p99_nanos": 16000000,
  "measured_p99_nanos": 14000000
}
```

The measured p99 must be strictly below 90% of the baseline p99.
