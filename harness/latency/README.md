# wren-latency

**Layer:** OS-enabled performance harness. **Phase:** 0 measurement validation.

Measures editor insert/delete/motion/operator/selection/viewport and
completion-acceptance paths in two scopes. The component scope drives the
engine and production `ClientViewModel` workspace composer. The full-product
scope drives the real TUI `App` over a 14,000-line Rust file, including syntax,
semantic/search/Git/diagnostic/selection decorations, overlays, the presenter,
and the Termina writer. The report keeps both scopes explicit so a fast
component result cannot be mistaken for a full-product result.

Both desired-frame scopes retain the architecture's 77,654ns p99 and 100
microsecond worst-observed targets. The production workload also hard-gates
large-file open below 500ms, first desired-frame construction below 5ms, and
open through the first terminal write below 600ms. Desired grids use the same
dotfile rendering profile as the runnable TUI.
Before each measured key, the harness fully presents that scenario's baseline;
setup presentations are disclosed separately and excluded from input latency.
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

The measured p99 must be strictly below the nested floor of 90% twice: a
second 10% cut from the prior hard target, approximately 81% of baseline p99.
