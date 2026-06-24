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
#[derive(Debug, Default)]
pub struct ContextBuilder {
    blocks: Vec<Block>,
}

impl ContextBuilder {
    /// A new, empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a block (builder style). Order of insertion does not matter.
    pub fn with(mut self, block: Block) -> Self {
        self.blocks.push(block);
        self
    }

    /// Add a block (mutable-reference style).
    pub fn push(&mut self, block: Block) -> &mut Self {
        self.blocks.push(block);
        self
    }

    /// Add a block produced by a [`ContextProvider`](crate::ContextProvider).
    pub fn with_provider(self, provider: &impl crate::ContextProvider) -> Self {
        self.with(provider.block())
    }

    /// Sort, drop absent blocks, and render the prompt string.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError::DuplicateBlock`] if a canonical block kind is added
    /// more than once.
    pub fn build(mut self) -> Result<Context, BuildError> {
        if let Some(kind) = first_canonical_duplicate(&self.blocks) {
            return Err(BuildError::DuplicateBlock(kind));
        }

        self.blocks.sort_by_key(|block| block.kind.sort_key());

        let mut rendered = String::new();
        for block in self.blocks.iter().filter(|block| !block.is_absent()) {
            if !rendered.is_empty() {
                rendered.push_str(BLOCK_SEPARATOR);
            }
            rendered.push_str(block.content.trim());
        }

        Ok(Context { rendered })
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
