# wren

Wren is a native Rust modal code editor with a transactional Vim-style engine,
safe file writes, crash recovery, restartable providers, native workflow tools,
remote workspace transport, and sandboxed WASI extension hosts.

## Run it

The pinned environment includes Rust, Neovim (the conformance oracle),
OpenSSH, git, and ripgrep:

```console
nix develop
cargo run --release -p wren-tui --bin wren -- path/to/file.rs
```

Without Nix, install Rust and `rg`, then:

```console
cargo build --release -p wren-tui --bin wren
./target/release/wren path/to/file.rs
```

The essential flow is Vim-like: `i` enters insert mode, `Esc` returns to normal
mode, `:w` saves, and `:q` quits. `wren --help` is the complete built-in quick
reference.

Wren starts with the checked-in equivalent of Wintermute's Neovim profile:
Space is the leader, `Space q` quits, `Space w` writes, `Space ff` fuzzy-finds
files from the current working directory, `Space fr` live-greps the current Git
root, and `Ctrl-h/j/k/l` moves between split windows. The profile also enables
two-space smart indentation, smart-case search, relative line numbers,
scrolloff 3, column 80, Catppuccin styling, OSC 52/system unnamed clipboard,
Sleuth-style indentation detection, format-on-save, and autosave when leaving a
named buffer.

## Editor workflows

- Unicode-aware motions, counts, operators, text objects, visual selections,
  registers, marks, raw-key macros, dot-repeat, and branching undo/redo
- Search, Ex v1 ranges, substitute/global/normal, buffers, splits, tabs,
  native grep/quickfix/cdo, and ranged writes
- Telescope-profile fuzzy files, browser, Git-root grep, buffers, durable
  oldfiles, jumplist, and diagnostic pickers, including picker resume
- Revision-validated word/LSP/snippet completion with `Ctrl-Space`,
  `Ctrl-N`/`Ctrl-P`, explicit `Enter` acceptance, and `Ctrl-E` cancellation
- Tree-sitter syntax decorations, Catppuccin semantic groups, rendered Markdown,
  diagnostics, Git hunk signs, and breakpoint/conditional-breakpoint marks
- Lazy cross-platform GPU compute for parallel provider classification when
  suitable, with transparent CPU fallback when an adapter or workload is unsupported
- Native Git hunk navigation, stage/reset/undo/preview/blame/diff plus
  Fugitive-style `:Git`, `:Gwrite`, and `:Gdiffsplit`
- Rust, TypeScript/JavaScript, Python, Go, Terraform, Nix, Haskell, Lua, shell,
  and C/C++ language-server profiles with navigation, hover/signature help,
  references, rename, code actions/lenses, workspace folders, and formatting
- Dotfile formatter profiles, `gq` at textwidth 79, global/buffer-local
  `:FormatToggle[!]`, and format-on-save
- Debug breakpoints/UI controls and debugger REPL workflows; Hoogle, Haskell
  package/file REPL, and selection evaluation; `:Codex`/Avante command aliases
- `:terminal`, cancellable `:make`, and revision-safe `:format`
- Local clipboard routing for register `+` through bounded OSC 52
- UTF-8/invalid-byte policy, mixed-EOL preservation, symlink-aware atomic saves,
  metadata preservation, external-change fencing, and hard-link warnings
- Checksummed recovery WAL and durable registers/history/marks/repeat state,
  branching undo history, and oldfiles

The workspace also ships a journaled session daemon, dual-lane OpenSSH remote
agent, native LSP/DAP/task/PTY/formatter/structural/AI primitives, and separate
client/workspace WASI extension-host binaries. Their protocol and failure
boundaries are exercised by integration and crash-recovery tests.

## Verify it

```console
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings -D unsafe-code
python3 scripts/layer-check.py
cargo run -p wren-conformance --locked -- --check-determinism
cargo run -p wren-conformance --locked -- --check-wren
cargo run -p wren-latency --locked --release -- --iterations 10000 --gate
cargo run -p wren-startup --locked --release -- --iterations 1000 --gate
cargo run -p wren-system-gates --locked --release -- --iterations 1000 --gate
```

The authoritative design and its performance contracts are in
[wren-architecture.md](wren-architecture.md). Direct implementation evidence is
indexed in [docs/completion-audit.md](docs/completion-audit.md).
