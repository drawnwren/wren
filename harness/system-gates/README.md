# wren-system-gates

Cross-phase observability harness for provider freshness/queue depth, snapshot
retention, process memory, and remote convergence/persisted-save latency. It
emits HDR distributions and gates provider-freshness p99 at
102,066ns/192,613ns/225ns for normal/large/pathological documents. Real
dual-OpenSSH convergence and fsynced save gate below 4,397,874ns and
17,783,192ns p99 for the checked-in loopback profile. Missing SSH
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

The harness computes both hard targets as the floor of 90% of this baseline.
The runner fails if the report is absent or malformed; a loopback baseline is
never presented as an authoritative netem target.

On the authoritative runner, capture current performance and then gate the
optimized build with the same profile:

```sh
WREN_REMOTE_PROFILE_ID=bare-metal-netem-80ms-loss2-reorder5 \
  scripts/netem-remote.sh --capture-baseline remote-baseline.json
WREN_REMOTE_BASELINE_JSON=remote-baseline.json scripts/netem-remote.sh
```

`WREN_BENCH_SSH_OPTIONS` accepts comma-separated OpenSSH `-o` values for
isolated benchmark targets (for example, a disposable known-hosts file).
