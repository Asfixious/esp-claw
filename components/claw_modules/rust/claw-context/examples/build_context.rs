//! Demonstrates assembling a context with `claw-context`.
//!
//! Run with:
//!
//! ```bash
//! cargo run -p claw-context --example build_context --target x86_64-unknown-linux-gnu
//! ```
//!
//! The crate owns *placement and rendering*; this example plays the role of the
//! *content sources* (the "filling"), both inline via [`Block::new`] and through
//! the [`ContextProvider`] seam. It shows that blocks added in any order always
//! render in the spec wire order, and that a conversation-mode context omits the
//! working-mode scaffolding.

use std::borrow::Cow;

use claw_context::{Block, BlockKind, BuildError, Context, ContextBuilder, ContextProvider};

/// A trivial content source: loads a fixed block kind with fixed text. In a real
/// system this would read `soul.md`, query memory, summarize history, etc.
struct StaticSource {
    kind: BlockKind,
    text: &'static str,
}

impl ContextProvider for StaticSource {
    fn block(&self) -> Block<'_> {
        Block::new(self.kind.clone(), self.text)
    }
}

/// Build a conversation-mode context: instructions + dialogue, no task framing.
fn conversation_context() -> Result<Context, BuildError> {
    let common = StaticSource {
        kind: BlockKind::CommonInstruction,
        text: "You are Claw, a helpful on-device agent. Be concise.",
    };
    let agent = StaticSource {
        kind: BlockKind::AgentInstruction,
        text: "Role: frontend. Answer the user directly or route to a worker.",
    };

    // Note the deliberately scrambled insertion order — the builder re-sorts it.
    ContextBuilder::new()
        .with(Block::new(
            BlockKind::OutputContract,
            "Reply in natural language, one short paragraph.",
        ))
        .with(Block::new(BlockKind::CurrentInput, "What can you do?"))
        .with_provider(&agent)
        .with_provider(&common)
        .with(Block::new(
            BlockKind::ConversationSummary,
            "Earlier: the user greeted the agent.",
        ))
        .build()
}

/// Build a working-mode context: adds durable memory, an active skill, mode
/// framing, and a custom retrieved-docs block at the bottom of the durable band.
fn working_context() -> Result<Context, BuildError> {
    let retrieved_docs = Block::new(
        BlockKind::Custom {
            band: claw_context::Band::Durable,
            scope: claw_context::Scope::Agent,
            // Sort after ModeFraming (order 2) within the agent durable group.
            order: 3,
            label: Cow::Borrowed("RetrievedDocs"),
        },
        "Doc: the GPIO API exposes claw_gpio_set_level(pin, level).",
    );

    ContextBuilder::new()
        .with(Block::new(
            BlockKind::CommonInstruction,
            "You are Claw, a helpful on-device agent.",
        ))
        .with(Block::new(
            BlockKind::AgentInstruction,
            "Role: worker. Execute the task and report structured results.",
        ))
        .with(Block::new(
            BlockKind::AgentMemory,
            "Prefers metric units. Has an ESP32-S3 DevKitC.",
        ))
        .with(Block::new(
            BlockKind::ActiveSkills,
            "Skill blink_led: toggle the on-board LED N times.",
        ))
        .with(Block::new(
            BlockKind::ModeFraming,
            "Task: blink the LED 3 times. Workspace: board=esp32s3.",
        ))
        .with(retrieved_docs)
        .with(Block::new(
            BlockKind::RecentContext,
            "tool_result(claw_gpio_set_level): ok",
        ))
        .with(Block::new(BlockKind::CurrentInput, "Make the LED blink."))
        .with(Block::new(
            BlockKind::OutputContract,
            "Respond as JSON: {actions, blockers, needs_approval, next_step}.",
        ))
        .build()
}

fn main() -> Result<(), BuildError> {
    let conversation = conversation_context()?;
    println!(
        "===== conversation-mode context =====\n{}\n",
        conversation.context()
    );

    let working = working_context()?;
    println!("===== working-mode context =====\n{}", working.context());

    Ok(())
}
