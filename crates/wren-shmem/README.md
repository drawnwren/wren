# wren-shmem

Safe fixed-layout `DocumentHead` publication for the session writer and its
clients. A process-lifetime owner lock enforces the single-writer invariant;
shared/exclusive standard-library file locks make each generation visible as
one complete snapshot across processes.

Only populated entries are written. The authoritative session journal owns
durability, so this reconstructible local-host table deliberately avoids an
`fsync` on each edit. It is not a network-filesystem transport; remote clients
receive heads and resume results through `wren-proto`/`wren-remote` instead.

The on-disk word layout is compact and versioned, but no raw mapping, pointer
conversion, manual `Send`/`Sync` implementation, or first-party unsafe code is
required.
