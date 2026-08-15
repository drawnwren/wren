# Contract state machines

## Command execution

```text
physical input
  -> closed modal parse
  -> RealtimeCommand -----------------------> transaction -> DesiredGrid
  -> BoundedCommand -- budget/checkpoint ----> effects     -> DesiredGrid
  -> TaskCommand ---- document barrier ------> worker checkpoints
                        | cancel/panic/quota      |
                        +------ failure <---------+
                        +------ Effects -> revision validation -> transaction
```

Only physical input starts realtime latency measurement. Macro keys retain exact
semantic order but publish at UI operations, checkpoints, and completion.

## Remote open

```text
valid cached revision -- head confirms --> correct frame + editing
        | awaiting head
        +-------------------------------> speculative frame (read-only)

uncached manifest entry -> viewport chunks -> progressive paint (read-only)
                         -> full hash/frontier materialized -> editing + whole-doc ops
```

No sparse text store exists in v1; a progressively painted document is not an
editable partial replica.

## Extension request

```text
manifest placement -> client host | workspace host
request -> concurrency permit -> fuel-limited component call
        -> bounded result chunk -> backpressure -> consumer
        -> cancel / result quota / deadline -> error
        -> trap -> discard Store -> restart extension instance
```

Commands return typed effects. Neither host exposes cells, windows, terminal
coordinates, unrestricted process spawn, or a synchronous grammar hook.
