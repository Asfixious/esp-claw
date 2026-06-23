# Trace Format Specification

`claw-log`'s `FlatTreeSubscriber` flattens the tracing span tree into single lines that an offline parser can reconstruct back into a tree. This document is the **authoritative definition**; the `trace.rs` implementation and its unit tests must follow it strictly.

## Line Structure

```
TRACE <ts> <type> <tracing-context> <incremental-context> <custom-context>
```

- Anything before `TRACE` is the transport-layer prefix (ESP_LOG's `I (…) tag:` / the host logger's prefix) and is **not part of this format**; parsing starts at the `TRACE` marker.
- `<ts>`: framework-filled **monotonic timestamp (ms, since boot; on host normalized to an equivalent monotonic clock)**, the first token after `TRACE`. The duration of a span is the `<ts>` difference between its `exit` and `enter` and **does not depend on the transport prefix**.
- The three layers, and the tokens inside each `<...>`, are separated by a **single space** (no alignment padding); a line never contains a newline (`\n` is converted to a space before emit).
- Each token inside `<...>` is `key=value`, and **neither key nor value may contain a space** (space is the separator). The writer asserts this with `debug_assert!`; anything containing spaces must go in the custom context.
- **All parsable structural information lives inside the `<...>` blocks**; everything after them is the custom context — free text, **not parsed** (spaces / commas / pipes / angle brackets are all allowed).
- A line has **at most two `<...>` blocks**: tracing context (always present) + incremental context (optional, `enter` only).

## ① `<type>`

`enter` (enter a span) / `exit` (leave a span) / `event` (an instantaneous record inside a span).

## ② tracing-context (framework-filled, one `<...>` block)

Coordinates the framework derives from metadata / stack / thread. Contents are fixed per type:

| type | `<...>` contents |
|------|------|
| `enter` | `span=<id> parent=<id\|none> task=<label> span-name=<name> target=<module>` |
| `exit`  | `span=<id> task=<label>` |
| `event` | `span=<id\|none> task=<label> event-name=<name> target=<module>` |

- `span`/`parent`: a framework-assigned, **monotonically increasing, never-recycled** unique id (not the raw tracing id), globally unique across the whole trace stream, so pairing/tree-building is unambiguous.
- `span`: the id of the span this record belongs to; for an `event` with no enclosing span it is `none` (how the consumer renders an orphan is decided by the visualizer and is out of scope for this spec).
- `parent`: the id of the parent span; the outermost span has no parent and is recorded as `none`.
- `task`: the thread name, taken from the thread API (host `std::thread::name`, ESP FreeRTOS task name), used to disambiguate across threads.
- `enter`/`exit` come **one pair per span**, corresponding to span creation/destruction (not per poll); they are paired by the same `span` (duration = the `<ts>` difference), and the span name is looked up by `span` from the `enter` line.

## ③ incremental-context (claw_core, one `<...>` block, `enter` only)

Closed set `{session, turn, agent, iteration}`, fixed order `session` → `turn` → `agent` → `iteration` (`trace.rs`'s `INHERITED_CONTEXT_KEYS`). Which span opens each key is listed under the span hierarchy below.

- A key appears **once, on the `enter` line of the span that opens it**; descendant lines / events do **not** repeat it, and the consumer reconstructs it via the stack: `enter` pushes, `exit` pops, and the full context of any line = the merged stack (child overrides parent).
- **Prefix-closed**: because the span hierarchy is a fixed nesting, the reconstructed key set is always a prefix of `session → turn → agent → iteration` — if a later key is present, all its predecessors are too (`agent` present ⟹ `session`+`turn` present; `iteration` present ⟹ all three present). There is never a "has `agent` but missing `turn`" gap.
- A subagent re-opening `agent` is a shadow that overrides its subtree, reverted on `exit`.
- If a span opens no key, the **whole block is omitted** (no empty `<>`).

## ④ custom-context (call site, free-form)

Each span/event's own content — **developer-defined, no format requirement, no `|`** — appended verbatim after the `<...>` blocks; the framework does not parse it.

- Only `enter` (span creation arguments) and `event` (record content) may carry a custom context.
- An `exit` line has only the tracing context (`span=<id> task=<label>`); it carries **neither incremental nor custom context**.

## Span Hierarchy

`session` (opens `session`) > `turn` (opens `turn`) > `agent` (opens `agent`) > `iteration_loop` (opens `iteration`).

- span = a unit of work with a start and end (`enter`/`exit` paired); event = an instantaneous fact.

## Example (with subagent shadow)

```
TRACE 2100 enter <span=1 parent=none task=main span-name=session target=claw_core::orchestrator> <session=session-1>
TRACE 2105 enter <span=2 parent=1 task=main span-name=turn target=claw_core::orchestrator> <turn=7> message_id=m1 cause=message
TRACE 2110 enter <span=3 parent=2 task=main span-name=agent target=claw_core::agent::registry> <agent=agent-1> kind=conversation depth=0
TRACE 2112 enter <span=4 parent=3 task=main span-name=iteration_loop target=claw_core::iteration_loop> <iteration=iteration-0>
TRACE 2120 enter <span=5 parent=4 task=main span-name=agent target=claw_core::agent::registry> <agent=agent-2> kind=tool depth=1
TRACE 2121 event <span=5 task=main event-name=spawned target=claw_core::agent::registry> parent_agent=agent-1 child_agent=agent-2
TRACE 2130 exit <span=5 task=main>
TRACE 2150 event <span=4 task=main event-name=completion target=claw_core::iteration_loop> status=done 👋 Hello!
TRACE 2152 exit <span=4 task=main>
TRACE 2154 exit <span=3 task=main>
TRACE 2156 exit <span=2 task=main>
TRACE 2158 exit <span=1 task=main>
```

- The `event-name=spawned` on `span=5` carries no incremental context; the merged stack reconstructs it as `session-1 + turn-7 + agent-2 + iteration-0`. The `event-name=completion` on `span=4` reconstructs as `session-1 + turn-7 + agent-1 + iteration-0`.
