# wren-command

**Layer:** OS-enabled command scheduler.

Runs `TaskCommand` work away from the input thread with a bounded queue,
cooperative cancellation/checkpoints, and barriers for affected documents.
Semantic command schemas, effects, and task descriptors live in `wren-types`.
