# wren-proto

Versioned, bounded, big-endian-u32-length-delimited transport for session mutations/results,
events, resume, save, document open, and remote RPC. Protocol major 3 serializes
the validated semantic types directly, eliminating the parallel wire object
graph while rejecting malformed versions, trailing bytes, and oversized frames.
