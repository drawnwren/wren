# wren-client-state

Client-owned durable registers, search/command histories, global marks, undo
branch heads, repeat data, `ResumeViewState`, and disposable
`PublishedViewport` caches. State is checksummed and replaced atomically. A
cached grid is returned as correct only when its complete key, resume state,
and the session's shared-memory authoritative document head agree.
