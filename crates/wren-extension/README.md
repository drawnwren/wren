# wren-extension

Published Wren extension API v1 and host runtime. Static TOML manifests declare
commands, settings, languages, queries, snippets, themes, keybindings, provider
placement, and requested grants. UI contributions use a closed semantic model.

`wren-client-extension-host` and `wren-workspace-extension-host` are distinct
host processes. Each extension owns one fuel- and memory-limited Wasmtime
store. Completion output uses a bounded one-chunk channel with cooperative
cancellation; traps discard and reinstantiate the store. Raw filesystem and
network access are denied unless a manifest receives an explicit high-trust
grant; document/workspace/task/client operations are mediated by the v1 WIT.
