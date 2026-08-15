# wren-engine

**Layer:** deterministic modal editor core above grammar/text/position. **Phase:**
1 local editor.

Owns transactional Unicode editing, motions/operators/text objects, registers,
mapped marks, raw-key macros, semantic dot-repeat, search, and grouped
undo/redo. It never performs I/O, uses Tokio, depends on terminal types, or
invokes providers.
