# Architecture completion audit

This audit maps the requirements in
[`../wren-architecture.md`](../wren-architecture.md) to executable evidence.
The commands in the last column are also run by CI where the required platform
capability is available.

| Scope | Implemented evidence | Verification |
|---|---|---|
| Phase 0 — contracts | Versioned semantic and Prost DTOs; atomic mutation/state deltas; durability frontiers; resumable views and validated published grids; document classes; file semantics; command classes; closed expression evaluator; WIT stream/trap/restart spike; text-store bake-off | `wren-types`, `wren-proto`, `wren-session`, `wren-client-state`, `wren-command`, `wren-extension`, `wren-text`; latency/startup harnesses |
| Phase 1 — native editor | Runnable TUI; transactional modal engine; durable registers/marks/macros/raw-key IR/jumplist/repeat; search; published Ex v1; buffers/splits/tabs; latest-wins presenter; safe open/save; background recovery WAL; Termina lifecycle and terminal clipboard routing | `cargo test -p wren-engine -p wren-view -p wren-presenter -p wren-tui`; real-file and PTY tests |
| Phase 2 — replicated session and providers | Checksummed workspace-keyed session/outbox journals, startup outbox replay, durable dedup, Received/Durable ordering, leases, replay/snapshot resume, bounded event retention, safe POSIX shared-memory heads, Unix daemon, restartable provider process, language-bundle generations, completion/picker/decorations, live typed TOML keymaps, trust and derived-state query shapes | `cargo test -p wren-session -p wren-sessiond -p wren-shmem -p wren-provider -p wren-config -p wren-derived -p wren-tui` |
| Phase 3 — workflow providers | Process-group tasks with TERM/KILL cancellation, live PTY/vt100 surfaces, native DAP and LSP clients, UTF-16 lowering, revision-fenced formatter, ast-grep structural work, excerpts, workspace transactions/persist batches, git hunks, speculative AI review | `cargo test -p wren-workflow -p wren-session`; TUI terminal/make/format integration tests |
| Phase 4 — remote | Shell-safe dual OpenSSH control/bulk launch, negotiated stdio agent, SSH and application heartbeats, dual-lane replacement plus application `Resume`, journaled mutations and persisted saves, Merkle namespace, BLAKE3/FastCDC cache, cached/progressive open states, dirty overlays, tiered coherence/hash budget, search, and three-way reconciliation | `cargo test -p wren-remote -p wren-sessiond --test remote_stdio`; real-OpenSSH system convergence/save histogram |
| Phase 5 — extensions | Frozen v1 WIT, typed manifests/contributions, distinct placement-faithful hosts, mediated grants, fuel/memory/request/result limits, bounded completion streaming, cancellation, trap restart, declarative UI, install/restart/remove lifecycle | `cargo test -p wren-extension` including both host binaries |
| Cross-cutting gates | A/B1 ≤5 ms; per-real-command input-to-desired-frame p99 <1 ms; task-yield p99 <1 ms with a <4 ms maximum; provider freshness p99 ≤2/1/0.25 ms by document class; task cancellation and terminal-write distributions; snapshot retention; memory; real-OpenSSH remote convergence/save; crash matrix; fuzz/loom/netem jobs. `--gate` refuses non-bare-metal, unpinned, or (for system gates) non-netem/simulated execution. | `wren-startup`, `wren-latency`, `wren-system-gates`, conformance, fuzz targets, loom test, `scripts/netem-remote.sh` |

The Neovim differential records mode/operator state, buffer and cursor,
selections, register types and values, marks, jump/change lists, search,
message-log state, undo topology, and semantics-affecting options. Goldens are
versioned under `harness/conformance/goldens/` and regenerated twice for the
determinism check.
