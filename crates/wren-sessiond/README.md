# wren-sessiond

Journaled session daemon. Local mode serves the bounded Wren protocol over a
private Unix socket and publishes document heads through shared memory. Remote
mode is an OpenSSH stdio agent with separate control and bulk lanes, durable
path bindings, manifests, content-addressed blobs, search, mutations, and
frontier-fenced saves.
