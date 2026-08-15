# wren-tui

Runnable client composition root. It owns terminal input, prompts and Ex
routing, buffers/splits/tabs, a latest-wins presenter, background mutation/WAL
and client-state workers, restartable syntax/completion providers, fuzzy file
picker, OSC 52 clipboard routing, PTY terminal buffers, tasks, and formatters.
Semantic editing and file guarantees remain in their owning libraries.

Run with `cargo run --release -p wren-tui --bin wren -- FILE`.
