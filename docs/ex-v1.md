# Published Ex v1 scope

In scope and compiled to the command/effect system: ranges and addresses,
substitution, `:global`/`:vglobal`, `:normal`, `:write`/`:wall`, `:edit`, buffer
cycling, horizontal/vertical splits and close, tabs, marks/register inspection,
native grep, and cdo-style multi-buffer application. Work exceeding the
realtime budget becomes a cancellable `TaskCommand` with document barriers.

Explicitly deferred: shell filters, Vimscript-defined commands, and any command
whose meaning requires embedded Vimscript. The `=` register and config `when`
clauses share the closed expression evaluator; they have no definitions, I/O,
mutation, or extension calls. Intentional Neovim differences must be exact
entries in `harness/conformance/intentional-divergences.toml`.
