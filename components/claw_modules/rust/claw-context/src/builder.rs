//! The context builder: the sole authority on wire order.
//!
//! Callers add blocks in any order; [`ContextBuilder::build`] sorts them by
//! `(band, scope, in-band order)`, drops absent blocks, and renders a single
//! prompt string. It is structurally impossible to emit an out-of-spec layout.

use crate::block::{Block, BlockKind};

/// Separator inserted between rendered blocks. A blank line keeps sections
/// visually distinct without the builder editorializing block content.
const BLOCK_SEPARATOR: &str = "\n\n";

/// Accumulates blocks and renders the final context. Add blocks in any order.
///
/// Blocks may **borrow** their content (see [`Block`]), so the builder carries a
/// lifetime tying it to its content sources; render before they go out of scope.
#[derive(Debug, Default)]
pub struct ContextBuilder<'a> {
    blocks: Vec<Block<'a>>,
}

impl<'a> ContextBuilder<'a> {
    /// A new, empty builder.
    pub fn new() -> Self {
        Self { blocks: Vec::new() }
    }

    /// Add a block (builder style). Order of insertion does not matter.
    pub fn with(mut self, block: Block<'a>) -> Self {
        self.blocks.push(block);
        self
    }

    /// Add a block (mutable-reference style).
    pub fn push(&mut self, block: Block<'a>) -> &mut Self {
        self.blocks.push(block);
        self
    }

    /// Add a block produced by a [`ContextProvider`](crate::ContextProvider).
    pub fn with_provider(self, provider: &'a impl crate::ContextProvider) -> Self {
        self.with(provider.block())
    }

    /// Sort, drop absent blocks, and render the prompt string into a fresh
    /// [`Context`]. Convenience over [`build_into`](Self::build_into) for callers
    /// that do not keep a reusable buffer.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError::DuplicateBlock`] if a canonical block kind is added
    /// more than once.
    pub fn build(self) -> Result<Context, BuildError> {
        let mut rendered = String::new();
        self.build_into(&mut rendered)?;
        Ok(Context { rendered })
    }

    /// Sort, drop absent blocks, and render the prompt prefix into `out`,
    /// **reusing** its allocation (`out` is cleared first; its capacity is
    /// retained). This is the hot-path entry: an agent owns one buffer and
    /// rebuilds the prefix into it only when a block changes, avoiding a fresh
    /// `String` every iteration.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError::DuplicateBlock`] if a canonical block kind is added
    /// more than once (`out` is left untouched in that case).
    pub fn build_into(mut self, out: &mut String) -> Result<(), BuildError> {
        if let Some(kind) = first_canonical_duplicate(&self.blocks) {
            return Err(BuildError::DuplicateBlock(kind));
        }

        self.blocks.sort_by_key(|block| block.kind.sort_key());

        out.clear();
        for block in self.blocks.iter().filter(|block| !block.is_absent()) {
            if !out.is_empty() {
                out.push_str(BLOCK_SEPARATOR);
            }
            out.push_str(block.content.trim());
        }

        Ok(())
    }
}

/// Finds the first canonical block kind that appears more than once, if any.
fn first_canonical_duplicate(blocks: &[Block]) -> Option<BlockKind> {
    let mut seen: Vec<&BlockKind> = Vec::new();
    for block in blocks {
        if !block.kind.is_canonical() {
            continue;
        }
        if seen.iter().any(|kind| **kind == block.kind) {
            return Some(block.kind.clone());
        }
        seen.push(&block.kind);
    }
    None
}

/// A fully rendered context, ready to feed to the LLM.
#[derive(Debug, Clone)]
pub struct Context {
    rendered: String,
}

impl Context {
    /// The rendered prompt string.
    pub fn context(&self) -> &str {
        &self.rendered
    }

    /// Consume into the owned prompt string.
    pub fn into_string(self) -> String {
        self.rendered
    }
}

impl AsRef<str> for Context {
    fn as_ref(&self) -> &str {
        &self.rendered
    }
}

/// The two wire fields of one LLM request, assembled by this crate and fed
/// straight into the API client: a `system` prefix string and the `messages`
/// tail. It is the single `context() -> (system, messages)` hand-off — no other
/// code path assembles a request.
///
/// Every field is a **borrow**, so assembling per iteration allocates nothing:
/// `system` points into the agent's reused prefix buffer (see
/// [`ContextBuilder::build_into`]), `history` into the memory snapshot, and
/// `reminders` into the agent's reused reminder buffer. The tail is a
/// **two-segment view** — persisted `history` plus ephemeral `reminders` — kept
/// separate so appending a reminder never clones the transcript; the backend
/// iterates `history` then `reminders`.
#[derive(Clone, Copy, Debug)]
pub struct RequestContext<'a> {
    system: &'a str,
    history: &'a serde_json::Value,
    reminders: &'a [serde_json::Value],
}

impl<'a> RequestContext<'a> {
    /// Pair an assembled `system` prefix with the message tail's two segments:
    /// the persisted `history` (a JSON array) and the ephemeral `reminders`.
    pub fn new(
        system: &'a str,
        history: &'a serde_json::Value,
        reminders: &'a [serde_json::Value],
    ) -> Self {
        Self {
            system,
            history,
            reminders,
        }
    }

    /// The rendered system-prompt prefix.
    pub fn system(&self) -> &'a str {
        self.system
    }

    /// The persisted conversation history (a JSON array of messages).
    pub fn history(&self) -> &'a serde_json::Value {
        self.history
    }

    /// The ephemeral trailing reminders (never persisted), in order.
    pub fn reminders(&self) -> &'a [serde_json::Value] {
        self.reminders
    }
}

/// Errors returned by [`ContextBuilder::build`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    /// A canonical block kind was added more than once.
    #[error("duplicate canonical block: {0:?}")]
    DuplicateBlock(BlockKind),
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::block::{Band, Scope};
    use std::borrow::Cow;

    #[test]
    fn orders_by_band_then_scope_regardless_of_insertion() {
        let context = ContextBuilder::new()
            .with(Block::new(BlockKind::CurrentInput, "INPUT"))
            .with(Block::new(BlockKind::CommonInstruction, "COMMON"))
            .with(Block::new(BlockKind::OutputContract, "OUTPUT"))
            .with(Block::new(BlockKind::AgentInstruction, "AGENT"))
            .with(Block::new(BlockKind::ConversationSummary, "SUMMARY"))
            .build()
            .unwrap();

        assert_eq!(
            context.context(),
            "COMMON\n\nAGENT\n\nSUMMARY\n\nINPUT\n\nOUTPUT"
        );
    }

    #[test]
    fn immutable_agent_instruction_precedes_mutable_global_memory() {
        let context = ContextBuilder::new()
            .with(Block::new(BlockKind::GlobalMemory, "GLOBAL_MEM"))
            .with(Block::new(BlockKind::AgentInstruction, "AGENT_INSTR"))
            .build()
            .unwrap();

        assert_eq!(context.context(), "AGENT_INSTR\n\nGLOBAL_MEM");
    }

    #[test]
    fn absent_blocks_render_to_zero_bytes() {
        let context = ContextBuilder::new()
            .with(Block::new(BlockKind::CommonInstruction, "COMMON"))
            .with(Block::new(BlockKind::ToolPolicy, ""))
            .with(Block::new(BlockKind::CurrentInput, "   \n  "))
            .build()
            .unwrap();

        assert_eq!(context.context(), "COMMON");
    }

    #[test]
    fn duplicate_canonical_block_is_rejected() {
        let error = ContextBuilder::new()
            .with(Block::new(BlockKind::CommonInstruction, "A"))
            .with(Block::new(BlockKind::CommonInstruction, "B"))
            .build()
            .unwrap_err();

        assert_eq!(
            error,
            BuildError::DuplicateBlock(BlockKind::CommonInstruction)
        );
    }

    #[test]
    fn custom_blocks_slot_within_their_band_and_allow_duplicates() {
        let custom = |order: u16, text: &'static str| {
            Block::new(
                BlockKind::Custom {
                    band: Band::Durable,
                    scope: Scope::Agent,
                    order,
                    label: Cow::Borrowed("x"),
                },
                text,
            )
        };

        let context = ContextBuilder::new()
            .with(Block::new(BlockKind::CommonInstruction, "COMMON"))
            .with(custom(1, "CUSTOM_B"))
            .with(custom(0, "CUSTOM_A"))
            .with(Block::new(BlockKind::CurrentInput, "INPUT"))
            .build()
            .unwrap();

        assert_eq!(context.context(), "COMMON\n\nCUSTOM_A\n\nCUSTOM_B\n\nINPUT");
    }
}
