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
| Cross-cutting gates | **Incomplete:** `wren-latency` now measures and separately gates both the component path and the full TUI `App` path, including transaction side effects, viewport-bounded syntax plus synthetic semantic spans, search/Git/diagnostic/selection decorations, overlays, presentation, and Termina serialization. The full-product 77,654 ns p99 / 100,000 ns maximum and 103,626 ns terminal-completion gates remain failures until an authoritative run actually beats them; coverage is no longer the missing part. Other gates remain A/B1 p99 <2,788,576/347,119 ns; task yield p99/max <62,258/63,969 ns; physical key-to-photon <81% of rig baseline; provider p99 <91,859/173,351/202 ns; real-OpenSSH loopback convergence/save p99 <3,958,086/16,004,872 ns; profile-specific netem convergence/save <81% baseline; crash matrix; fuzz/loom jobs. Missing hardware, SSH, authoritative-profile measurements, or any failed distribution remains a hard failure. | `wren-startup`, `wren-latency`, `wren-system-gates`, conformance, fuzz targets, loom test, `scripts/netem-remote.sh` |

The Neovim differential records mode/operator state, buffer and cursor,
selections, register types and values, marks, jump/change lists, search,
message-log state, undo topology, and semantics-affecting options. Goldens are
versioned under `harness/conformance/goldens/` and regenerated twice for the
determinism check.
