# wren-term

**Layer:** OS/terminal adapter above `wren-view`. **Phase:** 1 terminal backend.

Defines `TerminalBackend` and owns Termina input, capabilities,
raw/alternate-screen lifecycle, and rendering. Core types, grammar, engine, and
view never depend on this crate or Termina. Client-local register `+` copies use
a bounded OSC 52 operation here, outside workspace/provider authority.
