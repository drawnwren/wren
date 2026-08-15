# wren-latency

**Layer:** OS-enabled performance harness. **Phase:** 0 measurement validation.

Measures input decode through desired-frame readiness for the deterministic
echo engine, with optional CPU pinning and an isolated presenter thread. It may
use OS facilities; production crates must never depend on it.

