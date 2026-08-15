# Architecture

The reviewed architecture is versioned at
[`../wren-architecture.md`](../wren-architecture.md) and is authoritative for
this workspace. This file is the stable documentation entry point so internal
links do not depend on where the source document was originally delivered.

Implemented milestones:

- Phase 0 contracts: layered deterministic core, semantic mutation and resume
  types, durability frontiers, state ownership, conformance oracle, text-store
  bake-off, rendering/latency harnesses, and file-semantics policy.
- Phase 1 local editor: native modal engine, registers, marks, raw-key macros,
  dot repeat, grouped undo/redo, search, Ex core, viewport/cell renderer,
  Termina terminal lifecycle, race-checked atomic saves, and a checksummed
  background recovery WAL.
- Phase 2 replication/providers: durable authority and outbox journals,
  Received/Durable ordering, resume/epoch handling, safe shared-memory heads,
  a restartable provider process, freshness-keyed bounded queues, typed config,
  trust fencing, and dependency-shaped derived state.
- Phase 3 workflows: LSP/DAP, tasks, PTY terminal buffers, formatting, native
  git and structural operations, excerpts, workspace transactions, and
  speculative review branches.
- Phase 4 remote: capability-negotiated dual OpenSSH lanes, application
  heartbeat/resumption, Merkle/cache materialization, remote search and dirty
  overlays, reconciliation, fenced mutation and persisted-save frontiers.
- Phase 5 extensions: frozen WIT, placement-specific hosts, mediated
  capabilities, resource/fuel limits, bounded streaming, cancellation, trap
  restart, and declarative UI models.

Executable evidence for each item is indexed by
[`completion-audit.md`](completion-audit.md); simulated algorithm probes are
reported as non-authoritative and never satisfy architecture gates.
