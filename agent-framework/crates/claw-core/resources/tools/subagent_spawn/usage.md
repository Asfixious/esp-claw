The child does not see the parent conversation; make `goal` complete and
standalone. This tool runs in the background and delivers the final result
automatically; use `subagent_run` to wait in the current tool call.

For multi-step work involving LLM calls, tools, network access, or device I/O,
set `timeout_ms` to at least 300000. After spawning, avoid repeated
`subagent_watch` calls: they add parent LLM traffic and do not make the child
finish sooner. Wait for the automatically delivered detached result unless a
single status snapshot is actually needed.
