//! [`LlmCompactor`] — the default [`Compactor`], backed by [`ClawApi`].
//!
//! It flattens the aged window into a plain `role: content` transcript and asks
//! the model for a concise recap, returned as a single `system` summary message.
//!
//! This module lives behind the `llm` cargo feature (on by default). With the
//! feature off, `claw-memory` has no LLM dependency and you inject your own
//! [`Compactor`].

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::{json, Value};

use claw_api::{ChatRequest, ClawApi};

use crate::compaction::{CompactError, Compactor};

/// System prompt steering the summarization.
const SUMMARY_SYSTEM_PROMPT: &str = "You compress conversation history. Produce a \
concise, faithful summary of the conversation so far, preserving decisions, \
facts, user intent, open questions, and any tool results needed to keep going. \
Do not invent details. Output plain text only.";

/// Instruction prefacing the transcript handed to the model.
const SUMMARY_USER_PREFIX: &str = "Summarize the following conversation transcript:";

/// A [`Compactor`] that summarizes the aged window via the LLM client.
///
/// Holds a shared [`ClawApi`]; typically the same client the agent uses, passed
/// in through `ConversationDeps.compactor`.
pub struct LlmCompactor {
    api: Arc<ClawApi>,
}

impl LlmCompactor {
    /// Build a compactor over the given LLM client.
    ///
    /// Typically the `api` is the same [`ClawApi`] the agent already uses, wired
    /// into `ConversationDeps.compactor` so compaction summarizes through the
    /// configured backend.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use claw_api::{ClawApi, ClawApiConfig};
    /// use claw_memory::{Compactor, LlmCompactor};
    /// # use std::sync::atomic::AtomicBool;
    /// # use claw_interface::http::{ClawHttp, HttpError, HttpJsonRequest, HttpResponse};
    /// # struct StubHttp;
    /// # impl ClawHttp for StubHttp {
    /// #     fn post_json(&self, _: &HttpJsonRequest, _: &AtomicBool) -> Result<HttpResponse, HttpError> {
    /// #         Ok(HttpResponse { status_code: 200, body: "{}".into() })
    /// #     }
    /// # }
    /// let api = ClawApi::init(
    ///     ClawApiConfig {
    ///         backend_type: "openai_compatible".into(),
    ///         api_key: Some("sk-test".into()),
    ///         model: Some("gpt-4o-mini".into()),
    ///         base_url: Some("https://api.openai.com/v1".into()),
    ///         ..Default::default()
    ///     },
    ///     Arc::new(StubHttp),
    /// )
    /// .expect("init");
    ///
    /// // A ready-to-inject `Compactor`.
    /// let compactor: Arc<dyn Compactor> = Arc::new(LlmCompactor::new(Arc::new(api)));
    /// # let _ = compactor;
    /// ```
    pub fn new(api: Arc<ClawApi>) -> Self {
        Self { api }
    }
}

impl Compactor for LlmCompactor {
    fn compact(&self, window: &[Value]) -> Result<Vec<Value>, CompactError> {
        let transcript = render_transcript(window);
        let messages = json!([
            { "role": "user", "content": format!("{SUMMARY_USER_PREFIX}\n\n{transcript}") }
        ]);

        // todo: thread a real abort flag once the `Compactor` trait carries one;
        // for now a compaction summarization request is not cancellable.
        let abort = AtomicBool::new(false);
        let response = self
            .api
            .chat(&ChatRequest::new(SUMMARY_SYSTEM_PROMPT, &messages), &abort)
            .map_err(|err| CompactError::Backend(err.to_string()))?;

        let summary = response.text.unwrap_or_default();
        if summary.trim().is_empty() {
            return Err(CompactError::Backend(
                "model returned an empty summary".to_string(),
            ));
        }

        Ok(vec![json!({
            "role": "system",
            "content": format!("Summary of earlier conversation:\n{summary}"),
        })])
    }
}

/// Flatten chat messages into a readable `role: content` transcript.
///
/// Summarizing the *text* (rather than replaying the raw messages as a chat)
/// keeps the summarization request itself immune to tool-call pairing rules —
/// the backend never sees an orphaned `tool` message it would reject.
fn render_transcript(window: &[Value]) -> String {
    let mut out = String::new();
    for message in window {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("?");
        let content = match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        out.push_str(role);
        out.push_str(": ");
        out.push_str(&content);
        out.push('\n');
    }
    out
}
