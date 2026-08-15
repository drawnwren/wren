#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "netem remote fault test: skipped (Linux only)"
  exit 0
fi
if [[ "$(id -u)" != "0" ]]; then
  echo "netem remote fault test: requires a privileged bare-metal runner" >&2
  exit 77
fi

for variable in WREN_BENCH_REMOTE_HOST WREN_BENCH_REMOTE_WORKSPACE WREN_BENCH_REMOTE_STATE; do
  if [[ -z "${!variable:-}" ]]; then
    echo "netem remote fault test: $variable is required" >&2
    exit 64
  fi
done

benchmark_cpu="${WREN_BENCH_CPU:-0}"
governor="/sys/devices/system/cpu/cpu${benchmark_cpu}/cpufreq/scaling_governor"
if [[ -r "$governor" ]] && [[ "$(<"$governor")" != "performance" ]]; then
  echo "netem remote fault test: cpu${benchmark_cpu} governor must be performance" >&2
  exit 65
fi

interface="${WREN_NETEM_INTERFACE:-lo}"
cleanup() {
  tc qdisc del dev "$interface" root 2>/dev/null || true
}
trap cleanup EXIT
tc qdisc replace dev "$interface" root netem delay 80ms 20ms loss 2% reorder 5%
export WREN_BARE_METAL=1
export WREN_NETEM_ACTIVE=1
cargo run -p wren-system-gates --release -- --iterations 100 --cpu "$benchmark_cpu" --gate --output system-gates-gate.json
