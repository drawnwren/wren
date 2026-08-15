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
  termwiz terminal lifecycle, race-checked atomic saves, and a checksummed
  background recovery WAL.

Later distributed/provider/remote/WASI phases remain behind the protocol
boundaries described by the architecture and are not silently simulated by
the local editor.
