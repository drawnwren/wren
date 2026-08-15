# wren-position

**Layer:** coordinate indexing above `wren-text`. **Phase:** 0.

Maps canonical UTF-8 byte positions to scalar, UTF-16, grapheme, and display
cell coordinates. It must never depend on I/O, Tokio, grammar, engine, view, or
terminal crates.

