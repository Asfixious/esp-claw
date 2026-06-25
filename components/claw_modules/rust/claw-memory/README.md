# claw-memory

The agent memory subsystem.

Today this is the **short-term conversation memory** plus the **shared worker
pool** that runs its background jobs. The conversation memory is an append-only
transcript with automatic, invisible compaction; the pool amortizes FreeRTOS
task allocation by giving every memory type one place to submit background work
(compaction now; profile / long-term extraction later).

The core depends only on `claw-interface` (the `ClawFs` persistence seam) and
`claw-sys` (worker spawning). Summarization is **injected** via the `Compactor`
trait, so the crate has no built-in LLM dependency unless you want one.

## Public API

| Item | Role |
|---|---|
| `ConversationMemory` | The per-conversation transcript. `new(id, config, deps)`, `group()`, `messages()`, `flush()`. |
| `ConversationConfig` | Tuning: `dir`, `compact_threshold_tokens`, `keep_recent_tokens`, `segment_token_budget`, `persist_debounce`. Build with `ConversationConfig::new(dir)`. |
| `ConversationDeps` | Injected collaborators: `fs` (`ClawFs`), `pool` (`MemoryTaskPool`), `compactor` (`Compactor`). |
| `GroupGuard` | One turn, returned by `group()`. `append_user`, `append_assistant`, `append_tool_result`, `append_patch`. Commits the whole turn as one record on drop. |
| `Compactor` / `CompactError` | The summarization seam: fold an aged message window into a shorter summary. |
| `MemoryTaskPool` / `PoolConfig` / `MemoryJob` | The shared background worker pool — created once at boot, shared by every memory type. |
| `LlmCompactor` | *(feature `llm`, default on)* A ready-made `Compactor` backed by `claw-api`. |
| `NoopCompactor` | *(feature `compactor-stub`)* A never-compacts stub for host CLIs and tests. |

### How a turn flows

1. Call `memory.group()` to open a turn; append user / assistant / tool-result
   messages to the returned `GroupGuard`.
2. When the guard drops, the whole turn is committed as a single record.
3. `memory.messages()` returns what to send the model — committed turns plus the
   open one, with any compaction already spliced in.
4. `memory.flush()` forces a checkpoint (e.g. on clean shutdown).

**Compaction is automatic and invisible.** When committing a turn grows the
transcript past `ConversationConfig::compact_threshold_tokens`, the memory
schedules a background job on the pool that calls your `Compactor` and splices
in the result. Callers never trigger it.

## Features

| Feature | Default | Effect |
|---|---|---|
| `llm` | yes | Pulls in `claw-api` and provides `LlmCompactor`. Disable to keep the crate LLM-free and inject your own `Compactor`. |
| `compactor-stub` | no | Adds `NoopCompactor` (host-only convenience). |

## Example

```bash
cargo run -p claw-memory --example conversation --target x86_64-unknown-linux-gnu
```

Drives a conversation through two turns with a local (LLM-free) compactor over
an in-memory `MemFs`, then prints the message list the model would receive. See
the crate-level rustdoc for the same flow with inline commentary.

## Where it fits

A pure-Rust core crate (no platform/FFI). It persists through the injected
`ClawFs` and runs its background compute on the injected `MemoryTaskPool`, so it
is fully host-testable; the tests under `tests/` exercise it over both in-memory
and on-disk `ClawFs` doubles.
