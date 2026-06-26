# claw-context

The agent context builder: the **single assembler** that turns injected context
into the two wire fields of an LLM request — the `system` prefix string and the
`messages` tail.

This crate owns **placement and rendering**, never **content**. It orders
injected blocks into the mutability-graded wire layout (see
`docs/context-model.md`), drops absent blocks, and renders the prefix. What each
block *contains* — instructions, persona, memory, retrieved knowledge — is
supplied by callers, not by this crate.

## The two lanes

A request has exactly two wire fields, and this crate produces both:

- **PREFIX (cacheable) -> `system`**: scope-layered prose `Block`s (Global ->
  Session -> Agent) rendered into one string. Render it into a **reused buffer**
  with `ContextBuilder::build_into(&mut String)` and rebuild only when a block
  changes, so a steady prefix costs nothing per iteration.
- **TAIL -> `messages`**: the persisted conversation `history` (owned by memory)
  plus ephemeral `reminders` (per-request nudges, never persisted). These are
  **not** owned here — they pass through by reference.

`RequestContext::new(system, history, reminders)` pairs the prefix with the
tail's two segments as all-borrows (`history` + `reminders` stay separate so
appending a reminder never clones the transcript) — the single
`context() -> (system, messages)` hand-off fed straight into the API client.

## How it works

You add `Block`s in any order; the builder is the sole authority on wire order,
sorting by **band**, then **scope**, then in-band order. Each canonical block
kind may appear at most once (a duplicate is a `BuildError`), and missing blocks
simply don't render.

| Item | Role |
|---|---|
| `Block<'a>` | One piece of content plus its placement. `Block::new(kind, text)`; content is a `Cow`, so it may **borrow** its source (no per-iteration clone). |
| `BlockKind` | The canonical block kinds (e.g. `CommonInstruction`, `AgentInstruction`, `AgentMemory`, `ActiveSkills`, `ModeFraming`, `RecentContext`, `CurrentInput`, `OutputContract`) plus `Custom { band, scope, order, label }` for caller-defined blocks. |
| `Band` / `Scope` | The two axes the layout sorts on (durability band, ownership scope). |
| `ContextBuilder` | Collects blocks (`with`, `with_provider`) and renders the prefix via `build()` (fresh `String`) or `build_into(&mut String)` (reused buffer). |
| `Context` | An owned built prefix; `context()` returns the prompt `&str`. |
| `RequestContext<'a>` | The assembled `(system, messages)` hand-off: a `system` prefix plus the tail's two segments (`history` + `reminders`), all borrows. |
| `ContextProvider` | The seam for content sources: implement `block()` and feed via `with_provider`. |
| `BuildError` | Build-time failure (e.g. a duplicate canonical block). |

## Example

```rust
use claw_context::{Block, BlockKind, ContextBuilder};

let context = ContextBuilder::new()
    .with(Block::new(BlockKind::CurrentInput, "What's the weather?"))
    .with(Block::new(BlockKind::CommonInstruction, "You are a helpful agent."))
    .build()
    .expect("no duplicate canonical blocks");

assert_eq!(
    context.context(),
    "You are a helpful agent.\n\nWhat's the weather?",
);
```

A fuller, runnable walkthrough — conversation-mode vs working-mode contexts, the
`ContextProvider` seam, and a `Custom` block — is in `examples/`:

```bash
cargo run -p claw-context --example build_context --target x86_64-unknown-linux-gnu
```

## Where it fits

Pure-Rust, depends only on `thiserror` and `serde_json` (to carry the structured
`messages` tail). It bundles into the firmware's `claw_rt` staticlib and is fully
host-testable. Content providers (instruction loaders, memory providers, skill
sets, summarizers) live in other crates and plug in through `ContextProvider`;
the conversation `history` and `reminders` are passed in by the agent.
