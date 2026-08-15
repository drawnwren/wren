# wren-view

**Layer:** owned presentation model above `wren-engine`. **Phase:** 1 renderer.

Defines viewport scrolling, safe control-byte cell escaping, prompt/status
rows, desired grids, and terminal patches without leaking terminal-library
types. It never performs I/O, uses Tokio, or depends on `wren-term`/Termina.
