# ADR 0001: Enforce a deterministic Phase-0 core

- Status: accepted
- Date: 2026-08-14

## Context

The editor hot path needs deterministic tests and must remain embeddable. The
bootstrap also reserves future protocol and session package names without
freezing unstable surfaces.

## Decision

The dependency checker enforces these internal edges:

```text
wren-types <- wren-text <- wren-position
wren-types <- wren-grammar <- wren-engine <- wren-view
wren-view <- wren-term <- wren-tui
```

`wren-engine` may consume the text and position branches but cannot depend on
terminal, OS, async, or harness packages. Tokio is forbidden in types, text,
position, grammar, engine, and view. OS-facing dependencies are restricted to
term, binaries, and harness packages. Library builds deny `unwrap` and `expect`
outside tests; binaries remain responsible for top-level error reporting.

`wren-proto`, `wren-session`, and `wren-sessiond` remain compileable stubs. The
unstable mutation envelope is hidden and is not serializable.

## Alternatives rejected

- A single editor crate: simpler initially, but makes I/O leakage and circular
  ownership difficult to detect.
- An async engine API: rejected because it would compromise deterministic
  replay and put runtime scheduling on the input path.
- Filling Phase-2 crates speculatively: rejected because it would freeze an
  explicitly unstable mutation boundary.

