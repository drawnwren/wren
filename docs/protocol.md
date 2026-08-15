# Mutation, durability, and recovery protocol

The semantic contract lives in `wren-types`; the explicitly versioned Prost
transport lives in `wren-proto`. `ClientMutation` is one semantic atomic unit:
its state deltas and every document transaction are accepted or rejected
together. `wren-session::SessionAuthority` is the executable authority and
`wren-sessiond::SessionServer` exposes the same contract over a control socket.

## Mutation lifecycle

```text
created -> applied -> local-durable -> received -> remote-durable -> persisted
             |              |             |
             |              |             +-- compact whole-mutation outbox
             |              +-- informational only; never compact here
             +-- optimistic replica is immediately visible
```

- The client appends a complete checksummed `ClientMutation` to its outbox. Text
  and accompanying state changes therefore cannot tear apart.
- The daemon emits `Received` before authority submission. A crash injected at
  this boundary leaves no journal commit and proves the outbox must remain.
- `Durable` is emitted only after the checksummed session-journal record has
  been synchronized. The durable result and mutation-ID hash survive restart.
- A duplicate ID with the same content returns the original durable result
  without a second apply; reuse with different content is a hard collision.
- Document revisions and leases are validated for every member before a cloned
  staged workspace is installed. The authority never silently rebases.
- `RebaseRequired` is constructed from document transaction history, not the
  resumable event window, so event compaction cannot fabricate an empty delta.

## Resume and authority-head lifecycle

```text
Resume(epoch, last sequence, frontiers, outstanding IDs)
  |-- epoch and retained interval agree -> Replay(events after last sequence)
  `-- daemon restart / compacted continuity / mismatch
        -> SnapshotRequired(new epoch, workspace generation, document heads)
```

The daemon advances `session_epoch` on restart. A 0600 shared-memory table
publishes `{session_epoch, document_id, authoritative_revision}` in one seqlock
generation after each durable mutation. Client viewport caches count as correct
only after that table validates the complete cache/resume key. A stuck odd
generation is treated as interrupted and falls back to the control protocol.

Session sequence is causal ordering only. Document revision and domain-specific
workspace/provider generations are freshness keys.

## Persistence lifecycle

`WorkspaceExecutor` validates every document edit and resource precondition
before installing any memory change. It then returns a `PersistBatch`. Disk
actions are best-effort transactional: partial failure retains authoritative
memory, records the completed action count, and offers an identity-fenced retry.
`:w` completes only at the persisted document frontier.

## Crash-test matrix

| Fault | Required result | Executable evidence |
|---|---|---|
| lost durable acknowledgement | retry same ID; no second apply | `authority::durable_follows_journal_sync_and_duplicate_ids_are_not_reapplied` |
| duplicate mutation | same durable result | same authority test + outbox reconciliation test |
| mutation-ID collision | reject differing contents | authority dedup implementation |
| client crash / torn outbox tail | recover complete mutations only | `outbox::torn_tail_preserves_the_last_complete_whole_mutation` |
| corrupt client WAL | fail loudly | `wal::detects_checksum_corruption` |
| daemon crash after `Received` | no journal apply; retry succeeds | `wren-sessiond::crash_after_received_leaves_no_commit_and_retry_is_durable` |
| daemon crash after `Durable` | journal replay retains text and dedup | authority durable/restart test |
| both sides crash | retry reconciles by mutation ID | `outbox::both_sides_can_crash_after_durable_and_reconcile_by_mutation_id` |
| torn session-journal tail | retain complete records only | `journal::recovers_complete_records_and_ignores_a_torn_tail` |
| corrupt session journal | fail loudly | `journal::checksum_corruption_is_never_silently_replayed` |
| event continuity break | epoch changes; snapshot required | authority resume/epoch test |
| stale base after event compaction | return retained document delta | authority resume/epoch test |
| stale lease | `LeaseLost`, no apply | authority fencing test |
| multi-document precondition failure | no text or state member applies | authority atomicity test |
| external write before save | refuse overwrite; retain memory text | `local::refuses_to_overwrite_external_changes` |
| partial workspace persistence | mark batch; preserve memory; fence retry | workspace partial-persist test |

The fault points are deterministic, so the matrix runs in ordinary CI rather
than depending on timing races.
