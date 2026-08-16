# wren — architecture

Placement rule: **computation runs on the side of the latency boundary that
holds the data it needs.** Consistency is organized into four domains — the
architecture's distinguishing claim is being able to say exactly which edits
and editor semantics survive which failures:

```
REALTIME CLIENT STATE        CLIENT DURABILITY STATE
  mode / selection / replica    mutation WAL (outbox)
  layout / DesiredGrid          ResumeViewState
  immediate semantic caches     registers / histories / repeat state
                                dirty overlays

WORKSPACE AUTHORITY          DERIVED / EXTENSION STATE
  durable mutation journal      revisioned caches
  canonical document revisions  provider outputs
  filesystem + resource ops     WASI components
  leases · project services     decorations / semantic indexes
```

## Requirements

1. **Rust.** All first-party runtime/editor components. Exempt: external tools
   (OpenSSH, git, rg, LSP/DAP servers), tree-sitter grammars, the pinned
   `nvim --embed` test oracle, and sandboxed third-party WASI components. No
   general-purpose embedded scripting language; the only evaluator is the
   closed, side-effect-free expression language serving the `=` register and
   config conditionals.
2. **Startup.** Warm first *correct* frame, by scenario:

   | scenario | session | document | target |
   |---|---|---|---|
   | A  | warm | warm (snapshot published, head-validated) | p99 < 2,788,576ns, gated |
   | B1 | warm | unopened, page-cache-hot local fs, initial viewport at known byte range | p99 < 347,119ns, gated |
   | B2 | warm | unopened, uncached local fs | reported |
   | B3 | warm | network/FUSE filesystem | reported |
   | C  | cold session | warm fs | reported |
   | D  | cold binary | cold fs | reported |

   "Correct" = text revision validated against the authoritative head + cursor +
   unsaved state; highlighting may arrive progressively. Restoring a position
   deep in a never-indexed file is B2, not B1. `time_to_speculative_frame`,
   `time_to_correct_frame`, `time_to_interactive` reported separately.
3. **Latency.** All `RealtimeCommand`s (see Command execution) satisfy
   `physical-input-read → desired-frame-ready` aggregate p99 < 77,654ns on a
   pinned bare-metal runner with fixed workloads, with tighter per-command
   gates recorded by the harness. Long-running native commands gate at
   62,258ns p99 and 63,969ns maximum yield gaps and execute as cancellable
   tasks. `input → terminal-write-completed` is a hard p99 < 103,626ns gate
   with zero dropped or missing samples. Freshness SLOs gate alongside latency
   per `DocumentClass`. The hardware key-to-photon rig is a mandatory hard
   gate at 81% of its recorded baseline; absence fails rather than degrading
   to a software proxy.
4. **Vim.** Full native modal grammar (operators × counts × registers × text
   objects × marks × macros; operator-pending as real parser state) plus a
   **published Ex v1 scope**, validated by a model-based differential suite
   against a pinned Neovim oracle with an intentional-divergence allowlist.
   The synchronous grammar is closed at runtime.
5. **Remote.** Zero-RTT typing on materialized documents; instant open of valid
   cached revisions (head-validated); progressive *viewport paint* of uncached
   files, with editing and whole-document operations enabled only once the
   authoritative revision is fully materialized locally — a sparse text store
   is an explicit v1 non-goal. Zero-RTT filename+symbol search with
   dirty-buffer overlay; content search streams from remote rg (opt-in local
   mirror+index).
6. **Extensibility.** Declarative TOML config with typed context expressions and
   typed command arguments. Native providers cover the priority workflows.
   Third-party surface is a WASI component API: manifest contributions +
   runtime provider capabilities, instantiated in client- or workspace-side
   hosts per declared placement. **Non-goals:** Lua, Neovim plugin
   compatibility, general-purpose embedded scripting, extension-defined
   synchronous operators/motions, extension access to cells/windows/terminal
   coordinates.

## Process model

```
wren-tui ─────────────────────────────────┐
  wren-engine: modal state, selections,   │ ClientMutation / MutationResult
    buffer replicas, undo grouping,       │ SessionEvent stream · Resume
    viewport layout, presenter            ├──► wren-session (per workspace):
  local outbox WAL · ResumeViewState      │      durable journal, authority
      │                                   │      revisions, leases, workspace
      ├──► wren-client-providers          │      providers, supervision
      │    (separate process: tree-sitter,│        ├──► LSP / DAP / task / PTY
      │     structural, dirty overlays)   │        │      worker processes
      └──► client extension host (WASI)   │        └──► workspace extension
                                          │             host (WASI)
            remote workspace: same protocol; wren-session runs on the
            workspace host over SSH (control + bulk transports)
```

- **`wren-client-providers` is a failure boundary, not a module**: tree-sitter
  grammars are externally supplied generated code; a parser fault must not take
  down the latency-critical client. Restartable; IPC cost acceptable because
  these providers are off the keystroke path. Wasm parsers for dynamically
  installed/untrusted grammars; bundled hot grammars may be trusted native.
- Provider placement classes:

```
ClientProvider     syntax/highlight for open docs, structural motions/objects,
                   word/path/snippet completion, picker matching,
                   dirty-buffer git diff, dirty-buffer symbol overlay
WorkspaceProvider  LSP, DAP, tasks/builds, formatter/linter executables,
                   git/index baseline, rg, project indexing
EitherProvider     extension-declared; instantiated independently on either
                   side with placement-appropriate WIT capabilities (no live
                   component migration)
```

- Per-workspace sessions under a per-user supervisor. Sessions survive client
  death; view state is client-owned.
- **Hot path** (input thread): decode → modal machine → transaction on local
  replica → layout invalidation → `DesiredGrid`. Nothing else.
- **Presenter invariant:** input side publishes `Arc<DesiredGrid { epoch, rows:
  Vec<Arc<CellRow>> }>` into a `bounded(1)` latest-wins queue — never deltas.
  Presenter owns `last_fully_written_grid`, diffs, writes, updates. Dropping
  intermediate grids is safe by construction; dropping patches would not be.

## Command execution

Not every native Vim command can be constant-time; the classes make the
per-command p99 gates honest and keep unbounded work off the input thread:

```
RealtimeCommand   DesiredGrid within its checked-in p99 gate: insert/delete, local motions,
                  operators on bounded ranges, selection change, viewport
                  navigation, completion acceptance
BoundedCommand    explicit CPU budget: syntax-aware navigation, moderate search
TaskCommand       cancellable async task + command barrier on affected
                  documents; UI stays responsive: whole-buffer search/replace,
                  gg=G, gqG, large macro runs, structural rewrite, formatting
```

- **Macros/counts render at checkpoints, not per replayed key.** Execution
  separates `PhysicalInputCycle` / `CommandExecutionCycle` / `RenderYield`:
  `100@a` executes with exact intermediate semantics but publishes frames only
  at UI-dependent operations, cancellation/yield checkpoints, and completion.
  Latency metrics key on physical input events only.
- **One effect model for built-ins and extensions.** Semantic commands (rename,
  format, code action, structural rewrite, AI acceptance, workspace
  search/replace) return effects through the same path extensions use:

```rust
enum CommandOutcome { Immediate(Effects), Pending(CommandTask) }
// Effects → EditProposal / WorkspaceTransaction / ui_effects / messages
```

  Low-level cursor/edit primitives stay direct for speed. This makes preview,
  undo, tracing, macro introspection, and replay uniform.
- **Typed command arguments.** Every command publishes a schema driving TOML
  validation, palette forms, completion, docs, and WIT parameters:

```toml
[keys.normal."space f"]
command = "picker.files"
[keys.normal."space f".args]
root = "workspace"
hidden = false
```

## State ownership

| state | class |
|---|---|
| mode, pending operator, count, active register | EphemeralViewState |
| cursor, selections, scroll, cmdline, prompt | EphemeralViewState |
| window-local options, fold view state | EphemeralViewState |
| named/numbered registers, macro recordings (keys + IR) | DurableClientState |
| search/command history, last pattern, dot-repeat factory | DurableClientState |
| jumplist, global marks (document anchors), undo branch head | DurableClientState |
| text, undo-group history, changelist, local marks | DocumentState |
| buffer-local options, extmarks/decoration namespaces | DocumentState |
| keymaps (lowered IR), global options, provider config | WorkspaceState (client mirror) |
| tasks, leases, manifest, symbol index | WorkspaceState |

Undo splits deliberately: immutable undo-group *history* is DocumentState;
the branch *head/cursor* is per-client DurableClientState. `DurableClientState`
is client-mirrored for zero-RTT use, checkpointed asynchronously keyed by
persistent client identity. Clipboard (`+`/`*`) routes through `ClientServices`.

View state:

```
LiveViewState       ephemeral, in-memory
ResumeViewState     client-side durable checkpoint: cursor/selections/layout
PublishedViewport   disposable startup cache, keyed by {client_id, view_id,
                    document_revision, rows, cols, theme_hash,
                    config_generation, renderer_version}
```

## Mutation protocol

**A `ClientMutation` is one semantic atomic unit** — its state deltas and
document mutations commit together or not at all. Logically independent
operations are separate mutations; bulk background sync uses a distinct
batching envelope that does not alter semantic atomicity.

```
ClientMutation {
  mutation_id,                 // idempotency key; persisted in the session
  client_id, client_sequence,  //   journal so dedup survives session restart
  state_deltas: [StateDelta],
  documents: [DocumentMutation {
      document_id, lease_epoch, base_revision,
      semantic_group_id, semantic_group_kind,   // InsertRun | Operator |
      undo_parent,                              //   MacroInvocation | Formatter |
      transactions,                             //   WorkspaceRefactor |
  }],                                           //   UndoOf(g) | RedoOf(g)
}

MutationResult =
    Received { mutation_id }                    // informational, fast
  | Durable  { mutation_id, client_sequence, session_sequence,
               documents: [{ document_id, accepted_revision,
                             canonical_transaction_hash }] }
  | RebaseRequired { mutation_id, document_id, authoritative_revision,
                     delta_since_base }         // client rebases its own
  | LeaseLost { document_id, current_lease_epoch }   // optimistic suffix — it
  | Conflict  { document_id, reason }                // owns the cursor state

SessionEvent { session_sequence, origin,
               payload: DocumentDelta | StateDelta | LeaseChange | ExternalChange }

Save  { document_id, required_frontier, expected_file_identity, expected_content_hash }
Saved { persisted_frontier, new_file_identity, new_content_hash }

StateCheckpoint { client_id, through_client_sequence, state }
LeaseGrant { document_id, lease_epoch, holder_id, offline_policy }
```

**Durability semantics (the load-bearing rule):** `Durable` is sent only after
the mutation is committed to the session's crash-recoverable journal
(group-commit allowed). **Client WAL compaction happens on `Durable`, never on
`Received`** — otherwise a session crash between in-memory apply and journal
write loses an edit both sides thought was safe. The server never silently
rebases normal edits; `RebaseRequired` returns the delta and the client rebases
its outstanding optimistic suffix.

**Resumption:**

```
Resume { session_id, session_epoch, last_session_sequence,
         document_frontiers[], outstanding_mutation_ids[] }
ResumeResult =
    Replay { events[] }
  | SnapshotRequired { new_session_epoch, workspace_generation, document_heads[] }
```

`session_epoch` changes whenever event continuity cannot be guaranteed (daemon
restart, journal compaction past the client's sequence). Journal/event
retention is bounded and published. Crash-test matrix: lost ack, duplicate
mutation, client crash, session crash, both crash, event-log truncation,
reconnect with stale WAL.

**Fencing:** every `DocumentMutation` carries its document's `lease_epoch`.
Expired-offline holders continue only onto a local branch; on reconnect:
deterministic rebase or three-way merge (`imara-diff`), surfaced conflicts,
never auto-regain.

**Durability frontiers:**

```
applied         visible in local replica
local-durable   complete ClientMutation in client outbox WAL
                (group-commit; barrier on save/suspend)  ← text AND state atomic
remote          Durable ack: in the session's crash-recoverable journal
persisted       atomically on workspace disk             ← :w completes here
```

WAL: `~/.local/state/wren/outbox/<workspace>/` — append-only whole mutations
with checksums, compacted on `Durable`, replayed on reconnect. `"adw` never
survives as the deletion without the register write. Statusline:
`OFFLINE · N ops durable`.

**Workspace transactions:**

```
WorkspaceTransaction {
  document_edits: [DocumentMutation],
  resource_ops: [ Create { path, expected_absent },
                  Rename { from, to, expected_source_identity, expected_target },
                  Delete { path, expected_identity } ],
}
```

Memory-level: all preconditions validated, then all-or-nothing. `PersistBatch`
executes edits and resource ops best-effort-transactionally: partial failure
keeps memory state, marks the batch, offers retry; optional git-worktree
staging for large refactors. `DocumentId` is pathname-independent.

## Text and file semantics

- `TextStore` trait before rope commitment; benchmark ropey / crop / piece-tree
  on: normal files, 100MB file, multi-MB single line, non-ASCII, multi-cursor,
  retained snapshots. Interactive 100MB cold-open likely needs mmap-immutable
  base + append buffer + piece index (counted as novel work).
- **`DocumentClass`:** `Normal | Large | Pathological` by byte length,
  longest-line estimate, parse-rate sampling, generated-file detection. Large:
  fixed tree-sitter CPU budget, viewport/lexical highlight fallback, no bounded
  LSP-result freshness, native completion always on, bounded incremental git
  diff. Pathological: no whole-document syntax, capped wrap/display work,
  approximate scrollbar/display-column permitted until regions are indexed.
- **Snapshots are handles:** providers receive `SnapshotHandle` from a manager
  enforcing per-provider byte/revision quotas, held-too-long diagnostics, no
  public escape to `'static` owners. CI tracks `oldest_live_revision`,
  `retained_snapshot_bytes`. Inactive buffers downgrade replica → cached chunks.
- **Coordinates, lazy at every level:**

```
LineIndex           byte offsets of line starts (always)
LinePositionIndex   lazy per-line checkpoints: scalar/UTF-16
DisplayIndex        viewport-driven, with sparse lazy DisplayCheckpoint
                    { byte_offset, grapheme_boundary, display_column,
                      config_generation } — a horizontal jump to byte 7M of an
                    8MB line must not rescan from byte 0
```

  Checkpoints invalidate on edit and on config_generation change. Negotiate
  UTF-8 LSP positions, fall back UTF-16.
- **Encoding policy (v1):**

```
valid UTF-8        normal editing
invalid UTF-8      read-only byte-preserving escaped view;
                   explicit convert-to-UTF-8 command before textual edits
binary/NUL-heavy   binary-view mode; no Vim text operations
```

  Never silently substitute replacement characters and write them back. Line
  endings: preserved per line; mixed-EOL files show a status indication.
- **Link policy (v1):** opening a symlink stores presentation path + resolved
  target identity; normal save updates the *target*, preserving the link;
  workspace `Rename` operates on the path entry. `nlink > 1` warns before
  replace-by-rename (atomic replacement and hard-link preservation genuinely
  conflict — the tradeoff is surfaced, not hidden). Preserved metadata: mode,
  timestamps where sensible, xattrs/ACLs where supported.
- **Save guarantee, stated honestly:** wren performs race-resistant
  precondition checks and never *knowingly* overwrites an externally changed
  file — open-and-retain identity, verify hash, write+fsync temp, revalidate
  immediately before replace, strongest platform atomic op, optional advisory
  locks, surfaced races. Absolute exclusion against a concurrent uncooperative
  writer is not claimed; POSIX has no compare-and-rename. Same caveat applies
  to `Rename`/`Delete` preconditions.

## Rendering

- Owned types only — `Cell`, `CellRow`, `DesiredGrid`, `TerminalPatch`
  (presenter-internal) — behind `trait TerminalBackend`; termwiz inside the
  backend impl for input parsing, capabilities, escape emission.
- **Document bytes never reach the terminal as strings.** Text becomes typed
  cells first; control bytes are *escaped into visible cells* (`^[`, `^A`,
  `^@`), never stripped — the frame stays faithful to the file while only the
  `TerminalBackend` can emit escape sequences.
- Incremental `ViewportLayout`: line→row map, cached grapheme/width runs,
  wrap/fold summaries, overlay placement, forward invalidation to row-geometry
  stability. Synchronized-update sequences when supported.
- **Startup authority validation:** a local warm session maintains a tiny
  shared-memory head table `DocumentHead { session_epoch, document_id,
  authoritative_revision }` (seqlock generations). The client validates its
  `PublishedViewport` against it without RPC; only a head-validated frame
  counts as `time_to_correct_frame`. Remote sessions: paint immediately, but
  the frame is speculative until the control connection confirms the frontier.
  Fallback source: bounded `pread` of the viewport, rendered through the same
  escaping cell pipeline.
- Input: Kitty keyboard protocol / CSI-u where available; declared
  escape-timeout fallback; ambiguous legacy Escape excluded from the gate.
  Which-key, palette, docs, and conflict diagnostics generate from the keymap
  trie + command registry.

## Concurrency, freshness, and derived state

- **Freshness keys are domain-specific; the global sequence is for ordering
  only.** Editing file B must not invalidate work for file A:

```
session_sequence            causal order, reconnect/replay — never freshness
document_revision           document-derived work
workspace_generation(kind)  git / index / config / manifest / project-index
provider_generation         provider implementation/config (incl. LanguageBundle)
```

  A syntax result keys on `(document_id, document_revision,
  syntax_provider_generation)`.
- Jobs/results tagged with their freshness key; bounded per-provider queues;
  latest-wins coalescing; `CancellationToken`; tokio for I/O, bounded rayon for
  CPU; one serial tree-sitter actor per buffer; input/presenter threads
  reserved. Demand is viewport-scoped:

```rust
struct ProviderDemand { revision: DocumentRevision, visible: Vec<ByteRange>,
                        near_viewport: Vec<ByteRange>, priority: Priority }
```

- **`DerivedStateDb`** — coarse semantic derived state is expressed as
  dependency-tracked queries (Salsa is the candidate; rust-analyzer precedent),
  not hand-written callback invalidation, before invalidation spaghetti
  accumulates. In scope: resolved language config, command enabled-state,
  outline-for-revision, workspace symbol projection, effective keymap/provider
  config, git-baseline-derived state. Out of scope: rope edits, cursor motion,
  cell layout, per-keystroke rendering. If Salsa benchmarks poorly the query
  *shape* is kept and the engine replaced.
- **Staleness is UX, not just bookkeeping** — providers attach it to results:

```
Freshness = Fresh | LocallyMapped { from_revision } | Stale { revisions_behind }
          | Disconnected { age } | Unknown
```

  `Fresh` renders silently; stale states get a subtle indicator; picker/search
  headers surface it where it changes interpretation.
- Decorations map forward through each transaction immediately; new-revision
  results replace mapped spans; untransformable results hide, never misplace.

## Vim engine

- Keys → typed IR: `ApplyOperator { operator, motion, count, register,
  range_kind }`; motions yield `SelectionSet` + wise-ness; operators consume.
  Multiple selections native; exact Vim is the default skin.
- **Ex v1 scope (published, compiled to the same command/effect system):**
  ranges/addresses, `:s`, `:g`/`:v`, `:normal`, `:w`/`:wa`, `:e`, buffer
  cycling, `:split`/`:vsplit`/`:close`, `:tab*`, `:marks`/`:registers`,
  native-search grep equivalent, `:cdo`-style multibuffer application.
  Explicitly deferred: shell filters, Vimscript-dependent commands. Ex commands
  that exceed the realtime budget run as `TaskCommand`s.
- **Expression register `=`:** implemented via the closed side-effect-free
  expression language (numbers, strings, lists, arithmetic, string ops,
  cursor/selection queries; no I/O, definitions, mutation, or extension calls)
  — same evaluator as config `when` clauses. Fidelity divergences from
  Vimscript expressions go in the allowlist.
- Dot-repeat = repeatable command factory. Macros: raw keys (oracle fidelity) +
  lowered IR (introspection); render behavior per Command execution.
- Undo: immutable group history (DocumentState) + per-client branch head
  (DurableClientState); undo/redo emit inverse transactions as
  `UndoOf/RedoOf`-kinded mutations against current anchors.
- Conformance: `proptest` grammar-aware generation conditioned on mode/pending
  operator; `cargo-fuzz` on decode/Ex parse; differential vs pinned
  `nvim --embed` comparing mode+operator, buffer, selections, registers+types,
  marks, jump/change lists, search state, messages, undo topology,
  semantics-affecting options. `loom` + fault injection + `tc netem` for the
  distributed side.

## View model

`BufferId ViewId WindowId TabId SplitTree FloatingSurface DecorationNamespace
Extmark PromptSurface CommandLineState MessageLog TerminalSurface` — view
objects owned by the client session.

## Providers

```
PickerProvider [C]        nucleo matching; candidate streams, preview, actions
CompletionSource [C/W]    session (filter/select/accept) client-local +
                          synchronous; local candidates immediate; LSP merges;
                          accept = one atomic revision-validated transaction
DecorationProvider [C/W]  revisioned ranges + signs
TaskProvider [W]          process groups, cancellation, bounded output
FormatterLinterProvider [W] input revision → edits/diagnostics
DebugAdapterProvider [W]  native DAP client
StructuralProvider [C]    tree-sitter text objects, expand/shrink, sibling swap;
                          precomputed revision-tagged ranges; native fallback
StructuralSearchProvider [W] ast-grep-core (patterns, metavariables, rewrite);
                          raw TS queries stay for highlight/objects/outline
ProjectIndexProvider [W]  remote outline/tags → content-hash-keyed deltas →
                          local symbol search merged with client DirtySymbolOverlay
```

- **`LanguageBundle`** — syntax must agree across the latency boundary:
  content-addressed `{language_id, grammar_hash, grammar_abi, grammar_semver,
  highlight/object/outline/injection query hashes, config schema version}`.
  The bundle identity participates in `provider_generation`, project-index
  generation, dirty-overlay generation, and `ConfigBundle`. Wasm parsers for
  dynamically installed grammars (tree-sitter ABI versioning makes native
  loading of arbitrary grammars fragile *and* unsafe).
- **Git:** workspace side establishes baseline via CLI (HEAD, index blob,
  attributes/filters); baseline ships to client; signs computed client-side
  with `imara-diff` against the live buffer; git re-runs only on
  HEAD/index/config change or stage/revert.
- **Terminal buffers:** `portable-pty` + `vt100` emulator → `TerminalSurface`.
- **Multibuffers:** `ExcerptBuffer` for search/references/diagnostics/hunks/
  DAP/rename/AI; commits via `WorkspaceTransaction` + `PersistBatch`.
- **LSP:** revision bridge (requests tagged with remote frontier; ranges
  transformed or rejected); `LspWorkspaceEditAdapter` lowers workspace edits —
  including create/rename/delete — into `WorkspaceTransaction`s; no LSP path
  mutates a rope directly. Wren-owned wire types.
- **AI:** speculative transaction branches; multibuffer diff review; partial
  accept; rebase; explicit commit.
- **Tasks declare document visibility:**

```toml
[task.build]
document_view = "persisted"
save = "prompt"            # never | prompt | all
```

  UI at action time: `Build uses persisted workspace · 3 buffers unsaved`.

## Configuration and trust

Declarative TOML, generated from Nix if desired; conditionals use the closed
typed expression language (context keys: `language, remote, os,
selection.nonempty, lsp.available, document.class, workspace.trusted`);
command invocations use typed argument schemas.

**Trust gates all executable project contributions — including environment
activation.** Sessions attach with a *sanitized inherited environment*;
`.envrc`/flake evaluation, LSP/formatter/task command discovery, and provider
(re)configuration happen only after trust is granted. Evaluating direnv before
the trust gate would execute project-controlled shell logic and make the
boundary cosmetic.

```
safe untrusted:   themes, keymaps, language declarations, queries, passive settings
requires trust:   environment activation (direnv/nix), LSP/formatter/linter
                  commands, tasks, DAP adapters, extensions, external tools
```

Executable config + environment inputs are hashed; material change requires
re-trust. `ConfigBundle` (config + queries + themes + LanguageBundle refs,
content-addressed) uploads to remote sessions; may resolve to a Nix store
closure when both ends share the flake.

## Extensions (WASI)

```
ExtensionManifest (static)      commands (+arg schemas), default_keybindings,
                                settings_schema, languages, queries, snippets,
                                themes, declared placement per capability
Runtime capabilities (WIT)      CompletionSource, DecorationProvider,
                                PickerProvider, TaskProvider, StructuralProvider,
                                CommandHandler, StatusItemProvider,
                                VirtualDocumentProvider, UI contributions
```

- **Two hosts, placement-faithful:** `wren-client-extension-host` runs
  `ClientProvider` capabilities beside the replica; the workspace host runs the
  rest. The same component may be instantiated on either side with
  placement-appropriate capabilities; no live migration.
- Commands return effects (`CommandOutcome`), never mutate the engine. The
  synchronous grammar is closed: extensible text objects work via
  `StructuralProvider` precomputation with native fallback on cache miss —
  never a WASI round-trip on a keypress.
- **Extension UI is a closed declarative vocabulary** — models in, semantic
  actions out (`item-selected`, `button-pressed`, `text-submitted`); never
  `Cell`/`WindowId`/coordinates:

```
TextDocument VirtualList Tree Table Form DiffView Panel Picker Notification
```

- **Capabilities are wren-mediated by default** (raw fs would see persisted
  disk and bypass overlays/revisions/preconditions; raw spawn would bypass the
  task supervisor):

```
wren:document/read-snapshot   wren:document/propose-edit
wren:workspace/read           wren:workspace/search
wren:task/spawn               wren:client/notify · open-url
```

  Raw `wasi:filesystem`/sockets only as explicit high-trust grants; no generic
  process-spawn primitive.
- **Resources:** one `wasmtime::Store` per extension: memory/table/instance
  ceilings, epoch-interruption or fuel for CPU (`ResourceLimiter` alone does
  not bound CPU), max concurrent requests, max result bytes, per-provider
  deadlines. Host crash restarts all extensions; per-extension processes later
  if needed.
- **Phase-0 spike, not just WIT files:** one disposable end-to-end
  `CompletionSource` component (request → bounded-chunk candidate stream with
  backpressure via WASI 0.3 `stream<T>` → cancel mid-stream → quota → trap →
  host restart) and one `DecorationProvider`. Native traits are implemented
  around the WIT-shaped DTOs so the in-process API cannot drift toward
  un-ABI-able signatures. v1 freeze in Phase 5.

## Remote

1. **Transport:** OpenSSH subprocess (pinned via Nix), two connections —
   `control` (Mutation/Result/SessionEvent/Save/Resume, RPC, leases,
   heartbeats) and `bulk` (blobs, rg, build output, indexes) — avoiding
   cross-stream TCP head-of-line blocking. QUIC later as optional data plane;
   resumption is application-level regardless of transport. `prost` schema,
   capability versioning, no non-idempotent ops over 0-RTT.
2. **Namespace:** remote Merkle manifest {path, type, mode, symlink target,
   size, identity, content/tree hash, generation}; BLAKE3; FastCDC where delta
   transfer pays; dirty overlay; LRU cache GC; user-only perms. Git objects
   reused for clean tracked files.
3. **Coherence, tiered — hashing is a correctness-boundary tool, not a polling
   loop:**

```
watch event (notify; PollWatcher on network fs)
  → stat identity/size/mtime
    → content hash only if suspicious
```

   Mandatory hash verification at: overwrite-after-suspicion, explicit reload,
   save with changed identity, reconnect reconciliation. Background hashing
   only under an explicit bytes/sec budget. Merkle/stat scans (incremental,
   low-priority; full on reconnect/overflow/gap/refresh) are the correctness
   mechanism; watchers are hints.
4. **Overlay (tool visibility matrix):**

   |            | local dirty | remote acked | persisted disk |
   |---|---|---|---|
   | LSP        | —           | ✓ (sync'd)   | —              |
   | git signs  | ✓ (buffer, client-side) | — | baseline     |
   | search     | ✓ (replaces rg hits for dirty paths) | — | ✓ (remote rg) |
   | symbols    | ✓ (DirtySymbolOverlay) | — | ✓ (index)     |
   | format     | ✓ (revisioned in-memory) | — | —            |
   | build/task | —           | —            | ✓ (per task.document_view) |

5. **`ClientServices`:** clipboard (OSC 52 default), notify, open_url, terminal
   capabilities — workspace-side providers and extensions call through this;
   `"+y` always lands on the local clipboard.

## Benchmarks / CI

`hdrhistogram`, pinned Nix closure, fixed workloads, bare-metal runners,
controlled CPU frequency, `tc netem`. Latency keys on **physical input
events**; macro-replayed keys are excluded. Recorded:

1. physical input → transaction commit
2. physical input → desired-frame-ready — **hard gate, RealtimeCommands only**
3. TaskCommand yield-to-UI latency (p99 < 62,258ns; max < 63,969ns) + cancellation latency
4. input → terminal-write-completed (hard p99 < 103,626ns; no drops)
5. provider freshness + queue depth (gated per DocumentClass)
6. snapshot retention (`oldest_live_revision`, bytes)
7. memory peak/steady
8. startup scenarios A–D (A/B1 gated)
9. hardware key-to-photon (hard; measured p99 < 81% of rig baseline)
10. remote convergence + persisted-save latency (hard loopback p99 <
    3,958,086ns/16,004,872ns; authoritative netem p99 < 81% of its
    profile-bound baseline)
11. crash-recovery matrix (protocol section) as CI-run fault-injection tests

## Phasing

- **0 — contracts.** State ownership matrix; mutation protocol (atomic
  mutations, per-document fencing, `Received`/`Durable` split, rejection +
  rebase responses, resume/epoch/compaction semantics, persisted mutation-id
  dedup, semantic undo metadata); durability frontiers + WAL formats (client
  and session); view resume + `DocumentHead` validation; command execution
  classes; uncached-remote-open contract; `WorkspaceTransaction` + resource
  ops + path-independent `DocumentId`; DocumentClass policy; file semantics
  (encoding, links, save guarantee); tool visibility matrix; Ex v1 scope;
  expression-language definition; WIT prototypes **plus the end-to-end
  component spike**; conformance harness; input/render benchmark. Exit:
  protocol structs + state-machine diagrams + crash-test matrix exist and are
  reviewed; reproducible latency distributions.
- **1 — native editor, engine as library.** Client embeds `wren-engine`
  against an in-process session behind the same Mutation/Result trait.
  Open/edit/save (full file-semantics policy), transactional undo with group
  history, search, splits, core grammar + registers/marks/macros/dot-repeat +
  Ex v1 core, command classes with TaskCommand runner, layout + renderer +
  presenter, crash-recovery WAL. Exit: small reliable local editor.
- **2 — replicated session.** Daemon behind socket; durable journal +
  `Received`/`Durable`; SessionEvent stream + resume protocol; lease fencing;
  WAL replay; `wren-client-providers` process; tree-sitter via LanguageBundle;
  picker, LSP, completion, git; decoration pipeline; lazy coordinate indexing;
  trust model incl. environment gating; `DerivedStateDb`; freshness SLOs.
  Exit: daily-drivable.
- **3 — workflow providers.** Tasks, PTY+vt100 terminal buffers, DAP,
  format/lint, structural objects + ast-grep search, multibuffers +
  `WorkspaceTransaction`/`PersistBatch`, AI review. Exit: current nvim
  workflows covered natively.
- **4 — remote.** SSH bootstrap → mutation/save frontiers → manifest+cache →
  cached-instant/progressive open with speculative-until-confirmed frames →
  remote providers + symbol index + dirty overlays → tiered coherence +
  reconciliation → QUIC last.
- **5 — extensions.** Freeze WIT v1; dual extension hosts; resource/capability
  policy; manifest contributions + declarative UI; publish API.

Nix flake for dev/CI from day one; home-manager module after interfaces
stabilize.
