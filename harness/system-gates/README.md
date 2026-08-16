# wren-system-gates

Cross-phase observability harness for provider freshness/queue depth, snapshot
retention, process memory, and remote convergence/persisted-save latency. It
emits HDR distributions and gates provider-freshness p99 at
91,859ns/173,351ns/202ns for normal/large/pathological documents. Real
dual-OpenSSH convergence and fsynced save gate below 3,958,086ns and
16,004,872ns p99 for the checked-in loopback profile. Missing SSH
configuration fails the hard gate. A `--gate` run is accepted only on a
CPU-pinned bare-metal runner with active `tc netem`, a real dual-OpenSSH remote
target, and `WREN_REMOTE_BASELINE_JSON` for that exact network profile.
Ordinary real-SSH runs retain hard loopback results but are explicitly
non-authoritative.

Provider distributions always contain at least 1,000 samples; the remote
iteration cap therefore cannot turn p99 into a single-sample maximum.

The remote baseline report is intentionally separate from the measured output:

```json
{
  "schema": 1,
  "profile_id": "bare-metal-netem-80ms-loss2-reorder5",
  "convergence_baseline_p99_nanos": 123456789,
  "persisted_save_baseline_p99_nanos": 234567890
}
```

The harness computes both hard targets by taking the floor of 90% twice: a
second 10% cut from the prior hard target, approximately 81% of this baseline.
The runner fails if the report is absent or malformed; a loopback baseline is
never presented as an authoritative netem target.

The `architecture-gates` CI job is unconditional and requires a runner carrying
the `self-hosted`, `linux`, `x64`, and `wren-benchmark` labels. Runner-local
paths to the physical and remote baseline reports are supplied through
`WREN_KEY_TO_PHOTON_JSON` and `WREN_REMOTE_BASELINE_JSON`; absent reports fail
instead of disabling the job. `scripts/netem-remote.sh` likewise returns a
nonzero status when Linux, privilege, netem, or remote inputs are unavailable.

On the authoritative runner, capture current performance and then gate the
optimized build with the same profile:

```sh
WREN_REMOTE_PROFILE_ID=bare-metal-netem-80ms-loss2-reorder5 \
  scripts/netem-remote.sh --capture-baseline remote-baseline.json
WREN_REMOTE_BASELINE_JSON=remote-baseline.json scripts/netem-remote.sh
```

`WREN_BENCH_SSH_OPTIONS` accepts comma-separated OpenSSH `-o` values for
isolated benchmark targets (for example, a disposable known-hosts file).
