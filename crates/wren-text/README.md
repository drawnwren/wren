# wren-text

**Layer:** text storage above `wren-types`. **Phase:** 0 bake-off.

Defines the backend-neutral `TextStore` trait and Ropey, Crop, and experimental
piece-tree implementations. It must never depend on Tokio, OS APIs, editor
grammar/state, view code, or terminal code. Inputs are deterministic.

