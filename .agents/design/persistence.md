# Persistence

The JSON examples below are caller payloads. The persistence backend owns its
physical paths, extensions, framing, and schema-version storage; callers only
address typed logical entries.

Callers open entries through `Persistence::singleton::<T>(name)` or
`Persistence::collection::<T>(name)`. `load` returns only the decoded DTO. A
runtime owner constructs its `DurableState<T>` and registers it with the typed
entry; ordinary registrations are non-owning and `maybe_persist` observes them
at the global persistence boundary.

## ID Allocators

`singleton("id_allocators")`

```json
{
  "next_session_id": 4,
  "next_agent_id": 7
}
```

The constructor decodes this DTO into the two runtime allocators. Their shared
`DurableState` is the only live allocator state and is registered through the
ordinary singleton API.

The session collection is the persistent session registry. The engine publishes
a new ID only after its session state has been constructed; the global loop
persists the dirty counter and state at its next boundary.
Persistent and ephemeral sessions share this allocator, so an ID is never
reused; ephemeral sessions simply have no collection entry.

## ToolRegistry

`singleton("tool_registry")`

```json
{
  "overrides": {
    "tool_name": false
  }
}
```

Only explicit ToolRegistry enable/disable overrides are persisted. Missing
entries use their baked defaults. Overrides are applied before the registry is
started; the tool catalog and registry lifecycle state are rebuilt on boot.

## Session

`collection("sessions")`, keyed by session ID.

```json
{
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

`resume` produces the first resume reminder and is retained until the root agent
actually runs with that reminder. Merely opening and closing a session does not
consume it. Its ToolSet groups and tool calls are not restored or replayed.

A tool call enters `inflight_toolcalls` before execution and leaves only after
its outcome is present in a completed transcript turn. A background
`subagent_spawn` remains after its handler returns and leaves only after the
child's terminal result is present in a completed transcript turn.

Subagents, subagent transcripts, inboxes, and old agent IDs are not persisted.
The root agent is rebuilt on resume.

## Transcript

Every persisted transcript record is one completed turn:

```json
{"turn_id":1,"messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"hi"}]}
```

Open turns are not written. A torn final line is ignored on load.

State snapshots use atomic replacement. Transcript records are appended. State
and transcript have no shared commit marker or cross-record transaction.

## SkillSet

SkillSet is not persisted. Skill roots are rescanned on resume; activated skill
content is already present in the transcript as a tool result.

Profile and long-term memory continue to use their own stores.
