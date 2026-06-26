//! Memory wiring: the concrete collaborators `claw_core` injects into
//! `claw_memory::ConversationDeps`.
//!
//! `claw-memory` (the core crate) defines only the seams — the `Compactor`,
//! `ClawFs`, and `ClawThread` traits — and never the implementations. This
//! module is the agent wiring layer's home for the LLM-backed ones, so the
//! memory core stays free of any LLM dependency.

mod llm_compactor;

pub use llm_compactor::LlmCompactor;
