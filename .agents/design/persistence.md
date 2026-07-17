# Persistence

```text
<persistence-root>/
  state.json
  sessions/
    <session-id>/
      state.json
      transcript.jsonl
```

## Runtime

`state.json`

```json
{
  "schema_version": 1,
  "next_session_id": 4,
  "next_agent_id": 7,
  "tool_registry": {
    "tool_overrides": {
      "tool_name": false
    }
  }
}
```

Session directories are the session registry. An ID is exposed only after its
incremented counter has been persisted.

Only explicit ToolRegistry enable/disable overrides are persisted. Missing
entries use their baked defaults. Overrides are applied before the registry is
started; the tool catalog and registry lifecycle state are rebuilt on boot.

## Session

`sessions/<session-id>/state.json`

```json
{
  "schema_version": 1,
  "reasoning_effort": "medium",
  "permission_level": "ask",
  "mode": "normal",
  "resume": {
    "tool_set": {
      "loaded_groups": [
        "tool_group_id"
      ]
    },
    "inflight_toolcalls": [
      {
        "tool": "subagent_spawn",
        "arguments": {
          "kind": "worker",
          "name": "researcher",
          "goal": "investigate the failure",
          "foreground": false,
          "timeout_ms": 60000
        }
      }
    ]
  }
}
```

`resume` produces the first resume reminder and is then cleared. Its ToolSet
groups and tool calls are not restored or replayed.

A tool call enters `inflight_toolcalls` before execution and leaves only after
its outcome is present in a completed transcript turn. A background
`subagent_spawn` remains after its handler returns and leaves only after the
child's terminal result is present in a completed transcript turn.

Subagents, subagent transcripts, inboxes, and old agent IDs are not persisted.
The root agent is rebuilt on resume.

## Transcript

Every line in `sessions/<session-id>/transcript.jsonl` is one completed turn:

```json
{"schema_version":1,"turn_id":1,"messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"hi"}]}
```

Open turns are not written. A torn final line is ignored on load.

Both `state.json` files use atomic replacement. `transcript.jsonl` is appended.
State and transcript have no shared commit marker or cross-file transaction.

## SkillSet

SkillSet is not persisted. Skill roots are rescanned on resume; activated skill
content is already present in the transcript as a tool result.

Profile and long-term memory continue to use their own stores.
