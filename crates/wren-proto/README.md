# wren-proto

Frozen protocol-major-1 Prost schema and bounded length-delimited transport for
session mutations/results/events, resume, save, document open, and remote RPC.
Every wire DTO converts fallibly at the semantic boundary; malformed versions,
hashes, enums, transactions, trailing bytes, and oversized frames are rejected.
