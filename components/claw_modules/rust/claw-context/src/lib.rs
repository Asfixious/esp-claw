//! `claw-context` — the agent context builder.
//!
//! This crate owns **placement and rendering**, never **content**. It assembles
//! the prompt string handed to the LLM each iteration by ordering injected
//! blocks into the mutability-graded wire layout (see `docs/context-model.md`),
//! dropping absent blocks, and rendering a single string. What each block
//! *contains* — instructions, persona, memory, retrieved knowledge — is supplied
//! by callers, not by this crate.
//!
//! # Example
//!
//! ```
//! use claw_context::{Block, BlockKind, ContextBuilder};
//!
//! // Blocks may be added in any order; the builder is the sole authority on
//! // wire order (band, then scope, then in-band order).
//! let context = ContextBuilder::new()
//!     .with(Block::new(BlockKind::CurrentInput, "What's the weather?"))
//!     .with(Block::new(BlockKind::CommonInstruction, "You are a helpful agent."))
//!     .build()
//!     .expect("no duplicate canonical blocks");
//!
//! // Feed directly to the LLM.
//! let prompt: &str = context.context();
//! assert_eq!(prompt, "You are a helpful agent.\n\nWhat's the weather?");
//! ```

mod block;
mod builder;

pub use block::{Band, Block, BlockKind, Scope};
pub use builder::{BuildError, Context, ContextBuilder};

/// The seam for filling blocks. Content sources (instruction loaders, memory
/// providers, summarizers, …) implement this outside the crate; the builder
/// consumes them via [`ContextBuilder::with_provider`].
pub trait ContextProvider {
    /// Produce this provider's block (placement + content).
    fn block(&self) -> Block;
}
