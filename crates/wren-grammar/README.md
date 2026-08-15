# wren-grammar

**Layer:** pure key grammar above `wren-types`. **Phase:** 1 native grammar.

Lowers key events into typed command IR while exposing incomplete parser state,
and owns the closed side-effect-free expression evaluator. It never depends on
I/O, Tokio, text storage, editor state, view code, or a Neovim runtime.
