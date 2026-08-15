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

interface="${WREN_NETEM_INTERFACE:-lo}"
cleanup() {
  tc qdisc del dev "$interface" root 2>/dev/null || true
}
trap cleanup EXIT
tc qdisc replace dev "$interface" root netem delay 80ms 20ms loss 2% reorder 5%
cargo run -p wren-system-gates --release -- --iterations 100 --gate
