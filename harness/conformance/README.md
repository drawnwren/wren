# wren-conformance

**Layer:** OS-enabled differential test harness. **Phase:** 0/1 oracle traces.

Spawns the pinned `nvim --embed --headless` oracle, produces deterministic
golden traces, and generates grammar-aware key sequences. It may use process
I/O; production crates must never depend on it.
