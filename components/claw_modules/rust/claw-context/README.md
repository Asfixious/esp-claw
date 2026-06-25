# claw-context

The agent context builder: assembles the prompt string handed to the LLM each
iteration.

This crate owns **placement and rendering**, never **content**. It orders
injected blocks into the mutability-graded wire layout (see
`docs/context-model.md`), drops absent blocks, and renders a single string.
What each block *contains* — instructions, persona, memory, retrieved
knowledge — is supplied by callers, not by this crate.

## How it works

You add `Block`s in any order; the builder is the sole authority on wire order,
sorting by **band**, then **scope**, then in-band order. Each canonical block
kind may appear at most once (a duplicate is a `BuildError`), and missing blocks
simply don't render.

| Item | Role |
|---|---|
| `Block` | One piece of content plus its placement. `Block::new(kind, text)`. |
| `BlockKind` | The canonical block kinds (e.g. `CommonInstruction`, `AgentInstruction`, `AgentMemory`, `ActiveSkills`, `ModeFraming`, `RecentContext`, `CurrentInput`, `OutputContract`) plus `Custom { band, scope, order, label }` for caller-defined blocks. |
| `Band` / `Scope` | The two axes the layout sorts on (durability band, ownership scope). |
| `ContextBuilder` | Collects blocks (`with`, `with_provider`) and renders via `build()`. |
| `Context` | The built result; `context()` returns the final prompt `&str`. |
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

Pure-Rust, depends only on `thiserror`. It bundles into the firmware's `claw_rt`
staticlib and is fully host-testable. Content providers (instruction loaders,
memory providers, skill sets, summarizers) live in other crates and plug in
through `ContextProvider`.
