# wren-presenter

**Layer:** OS-enabled terminal presentation boundary.

Owns the last fully written grid and publishes only complete `Arc<DesiredGrid>`
values through a capacity-one latest-wins slot. It computes terminal patches
and writes them away from the input-to-desired-frame hot path.
