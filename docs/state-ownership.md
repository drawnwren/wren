# State ownership

This matrix is normative. New mutable state requires an owner, lifetime, and
mutation path here before implementation.

| State | Owner | Lifetime | Mutation path |
|---|---|---|---|
| mode, selections, cursor, scroll, prompt | `wren-engine` / client view | live view | synchronous typed key IR |
| pending operator, count, active register | `wren-grammar` parser state | live view | synchronous closed grammar |
| viewport layout and `DesiredGrid` | `wren-view` | frame/view | local invalidation and owned cells |
| presenter's last fully written grid | `wren-presenter` | terminal attachment | capacity-one full-grid publication |
| resume cursor/selections/layout | `wren-client-state` | client durable | atomic checksummed checkpoint |
| published viewport cache | `wren-client-state` | disposable | full-keyed checkpoint; shared-head validation |
| whole-mutation outbox | `wren-session::MutationOutbox` | client durable | append before send; compact on `Durable` only |
| registers, histories, macro/dot state, undo branch head | durable client state | client durable | mutation state deltas + async checkpoint |
| canonical text, revision, immutable undo groups | session authority | document | fenced `DocumentMutation` only |
| session event order, leases, dedup | session authority journal | workspace session | synchronized journal records |
| shared authoritative heads | `wren-sessiond` / `wren-shmem` | daemon | single-writer atomic seqlock batch |
| workspace paths/resources and pending persist batches | workspace authority | workspace | `WorkspaceTransaction` then `PersistBatch` |
| file identity/hash, encoding, EOL/link policy | `wren-session::LocalDocument` | open document | race-resistant open/save checks |
| provider snapshots | `wren-text::SnapshotManager` | bounded request | opaque quota-accounted handles |
| provider results and decorations | provider process/host | derived revision | freshness-keyed latest-wins result |
| WIT component stores and quotas | client/workspace extension host | extension instance | mediated capabilities, fuel, limits |
| terminal capabilities and escape emission | `wren-term` | terminal attachment | backend only |

Undo history is document state; its branch head is durable client state. A
client can therefore choose a branch without changing another client's cursor.
Clipboard registers route through client services, never workspace-side OS APIs.
