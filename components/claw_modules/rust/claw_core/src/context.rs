//! Context building and persistence, port of `claw_core_context.c` and
//! `claw_core_context_persist.c`.
//!
//! cJSON message/tool arrays are expressed as [`serde_json::Value`] arrays.

use serde_json::Value;

use claw_interfaces::error::{
    EspErr, ESP_ERR_INVALID_ARG, ESP_ERR_INVALID_STATE, ESP_ERR_NO_MEM, ESP_FAIL, ESP_OK,
};

use crate::callbacks::{CapCaller, ContextProvider, PersistContext, PersistRecord, ProviderOutcome};
use crate::consts::{
    ContextKind, ContextRecordType, CONTEXT_PROVIDER_FLAG_REQUEST_START_ONLY, INSERT_QUEUE_LEN,
};
use crate::errname::esp_err_to_name;
use claw_api::LlmResponse;
use crate::request::RequestItem;
use crate::util::append_tool_summary_line;
use crate::util::obs_csv_append;

/// A request-start-only context captured once at request start
/// (`claw_core_cached_context_t`).
#[derive(Clone, Debug)]
pub struct CachedContext {
    pub kind: ContextKind,
    pub content: String,
}

/// Result of [`build_iteration_context`].
pub struct BuiltContext {
    pub system_prompt: String,
    pub messages: Value,
    pub tools_json: Option<String>,
}

/// `claw_core_append_user_message`: push `{"role":"user","content":text}`.
pub fn append_user_message(messages: &mut Value, text: &str) -> EspErr {
    let arr = match messages.as_array_mut() {
        Some(a) => a,
        None => return ESP_ERR_INVALID_ARG,
    };
    arr.push(serde_json::json!({ "role": "user", "content": text }));
    ESP_OK
}

/// Parse a JSON array of objects and append each element to `dest`.
fn append_array_json(dest: &mut Value, json_text: &str) -> EspErr {
    if json_text.is_empty() {
        return ESP_ERR_INVALID_ARG;
    }
    let parsed: Value = match serde_json::from_str(json_text) {
        Ok(v) => v,
        Err(_) => return ESP_FAIL,
    };
    let items = match parsed.as_array() {
        Some(a) => a,
        None => return ESP_FAIL,
    };
    let arr = match dest.as_array_mut() {
        Some(a) => a,
        None => return ESP_ERR_INVALID_ARG,
    };
    for item in items {
        arr.push(item.clone());
    }
    ESP_OK
}

/// `build_current_turn_prompt`.
fn build_current_turn_prompt(request: &RequestItem) -> String {
    let channel = request.source_channel.as_deref().unwrap_or("(unknown)");
    let chat_id = request.source_chat_id.as_deref().unwrap_or("(unknown)");
    format!(
        "## Current Turn Context\n- source_channel: {channel}\n- source_chat_id: {chat_id}\n"
    )
}

/// `append_prompt_section`: append `"\n\n## {name}\n{content}"`.
fn append_prompt_section(prompt: &mut String, section_name: &str, content: &str) -> EspErr {
    if section_name.is_empty() || content.is_empty() {
        return ESP_ERR_INVALID_ARG;
    }
    prompt.push_str("\n\n## ");
    prompt.push_str(section_name);
    prompt.push('\n');
    prompt.push_str(content);
    ESP_OK
}

/// `apply_context_content`.
fn apply_context_content(
    system_prompt: &mut String,
    messages: &mut Value,
    tools: &mut Value,
    kind: ContextKind,
    section_name: &str,
    content: &str,
) -> EspErr {
    if section_name.is_empty() || content.is_empty() {
        return ESP_ERR_INVALID_ARG;
    }
    match kind {
        ContextKind::SystemPrompt => append_prompt_section(system_prompt, section_name, content),
        ContextKind::Messages => append_array_json(messages, content),
        ContextKind::Tools => append_array_json(tools, content),
    }
}

/// `claw_core_collect_request_start_only_contexts`: returns a vec aligned with
/// `providers`; only request-start-only providers populate a `Some`.
pub fn collect_request_start_only_contexts(
    providers: &[Box<dyn ContextProvider>],
    request: &RequestItem,
) -> Result<Vec<Option<CachedContext>>, EspErr> {
    let mut out: Vec<Option<CachedContext>> = vec![None; providers.len()];
    if providers.is_empty() {
        return Ok(out);
    }

    for (i, provider) in providers.iter().enumerate() {
        if provider.flags() & CONTEXT_PROVIDER_FLAG_REQUEST_START_ONLY == 0 {
            continue;
        }
        match provider.collect(request) {
            ProviderOutcome::Skip => continue,
            ProviderOutcome::Error(err) => {
                log::warn!(
                    "context provider collect failed request={} provider={} err={}",
                    request.request_id,
                    provider.name(),
                    esp_err_to_name(err)
                );
                return Err(err);
            }
            ProviderOutcome::Provided { kind, content } => {
                if content.is_empty() {
                    log::warn!(
                        "context provider returned empty content request={} provider={}",
                        request.request_id,
                        provider.name()
                    );
                    return Err(ESP_FAIL);
                }
                log::info!(
                    "context_cached request={} provider={} context_kind={} context_len={}",
                    request.request_id,
                    provider.name(),
                    kind.as_str(),
                    content.len()
                );
                out[i] = Some(CachedContext { kind, content });
            }
        }
    }
    Ok(out)
}

/// `claw_core_cached_contexts_have_messages`.
pub fn cached_contexts_have_messages(contexts: &[Option<CachedContext>]) -> bool {
    contexts
        .iter()
        .flatten()
        .any(|c| c.kind == ContextKind::Messages)
}

/// `claw_core_build_iteration_context`.
#[allow(clippy::too_many_arguments)]
pub fn build_iteration_context(
    system_prompt_base: &str,
    providers: &[Box<dyn ContextProvider>],
    request: &RequestItem,
    runtime_messages: &Value,
    request_start_contexts: &[Option<CachedContext>],
    inject_active_user: bool,
    obs_providers_csv: &mut String,
) -> Result<BuiltContext, EspErr> {
    let mut system_prompt = system_prompt_base.to_string();
    let mut messages = Value::Array(Vec::new());
    let mut tools = Value::Array(Vec::new());

    for (i, provider) in providers.iter().enumerate() {
        if provider.flags() & CONTEXT_PROVIDER_FLAG_REQUEST_START_ONLY != 0 {
            if let Some(Some(cached)) = request_start_contexts.get(i) {
                let err = apply_context_content(
                    &mut system_prompt,
                    &mut messages,
                    &mut tools,
                    cached.kind,
                    provider.name(),
                    &cached.content,
                );
                if err != ESP_OK {
                    return Err(err);
                }
                obs_csv_append(obs_providers_csv, provider.name(), true);
            }
            continue;
        }

        match provider.collect(request) {
            ProviderOutcome::Skip => continue,
            ProviderOutcome::Error(err) => {
                log::warn!(
                    "context provider collect failed request={} provider={} err={}",
                    request.request_id,
                    provider.name(),
                    esp_err_to_name(err)
                );
                return Err(err);
            }
            ProviderOutcome::Provided { kind, content } => {
                if content.is_empty() {
                    log::warn!(
                        "context provider returned empty content request={} provider={}",
                        request.request_id,
                        provider.name()
                    );
                    return Err(ESP_FAIL);
                }
                log::info!(
                    "context_loaded request={} provider={} context_kind={} context_len={}",
                    request.request_id,
                    provider.name(),
                    kind.as_str(),
                    content.len()
                );
                obs_csv_append(obs_providers_csv, provider.name(), true);
                let err = apply_context_content(
                    &mut system_prompt,
                    &mut messages,
                    &mut tools,
                    kind,
                    provider.name(),
                    &content,
                );
                if err != ESP_OK {
                    return Err(err);
                }
            }
        }
    }

    let turn_prompt = build_current_turn_prompt(request);
    let err = append_prompt_section(&mut system_prompt, "Core Request", &turn_prompt);
    if err != ESP_OK {
        return Err(err);
    }

    if inject_active_user {
        let err = append_user_message(&mut messages, &request.user_text);
        if err != ESP_OK {
            return Err(err);
        }
    }

    if let Some(runtime_arr) = runtime_messages.as_array() {
        if !runtime_arr.is_empty() {
            let dest = messages.as_array_mut().unwrap();
            for item in runtime_arr {
                dest.push(item.clone());
            }
        }
    }

    let tools_json = match tools.as_array() {
        Some(a) if !a.is_empty() => Some(tools.to_string()),
        _ => None,
    };

    Ok(BuiltContext {
        system_prompt,
        messages,
        tools_json,
    })
}

/// `claw_core_append_assistant_tool_calls`: parse the raw assistant message and
/// push it onto `messages`.
pub fn append_assistant_tool_calls(messages: &mut Value, response: &LlmResponse) -> EspErr {
    let raw = match response.raw_message_json.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => return ESP_ERR_INVALID_STATE,
    };
    let assistant: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return ESP_ERR_INVALID_STATE,
    };
    match messages.as_array_mut() {
        Some(a) => {
            a.push(assistant);
            ESP_OK
        }
        None => ESP_ERR_INVALID_ARG,
    }
}

/// `claw_core_append_tool_results_messages`: run each tool call, append the
/// `tool` messages to `runtime_messages`, accumulate the tool summary, and
/// return the serialized tool-results array.
pub fn append_tool_results_messages(
    call_cap: &dyn CapCaller,
    runtime_messages: &mut Value,
    response: &LlmResponse,
    request: &RequestItem,
    tool_summary: &mut String,
) -> Result<String, EspErr> {
    let runtime_arr = match runtime_messages.as_array_mut() {
        Some(a) => a,
        None => return Err(ESP_ERR_INVALID_ARG),
    };
    let mut tool_results: Vec<Value> = Vec::new();

    for tc in &response.tool_calls {
        log::info!(
            "tool_call request={} name={} args={}",
            request.request_id,
            if tc.name.is_empty() { "(null)" } else { &tc.name },
            tc.arguments_json
        );

        let (err, output) = call_cap.call_cap(&tc.name, &tc.arguments_json, request);
        let content = match output {
            Some(o) => o,
            None => {
                if err != ESP_OK {
                    esp_err_to_name(err).to_string()
                } else {
                    return Err(ESP_ERR_NO_MEM);
                }
            }
        };

        log::info!(
            "tool_result request={} name={} err={} output={}",
            request.request_id,
            if tc.name.is_empty() { "(null)" } else { &tc.name },
            esp_err_to_name(err),
            content
        );

        if !tc.name.is_empty() {
            let summary_err = append_tool_summary_line(tool_summary, &tc.name, err == ESP_OK);
            if summary_err != ESP_OK {
                log::warn!("tool summary truncated for request={}", request.request_id);
            }
        }

        let tool_message = serde_json::json!({
            "role": "tool",
            "tool_call_id": tc.id,
            "content": content,
            "is_error": err != ESP_OK,
        });

        runtime_arr.push(tool_message.clone());
        tool_results.push(tool_message);
    }

    Ok(Value::Array(tool_results).to_string())
}

// --- persistence (claw_core_context_persist.c) ---------------------------

/// `persist_context_batch_if_configured`.
fn persist_batch_if_configured(
    persist: Option<&dyn PersistContext>,
    request: &RequestItem,
    records: &[PersistRecord],
    turn_completed: bool,
) -> EspErr {
    let persist = match persist {
        Some(p) => p,
        None => return ESP_OK,
    };
    let session = request.session_id_str();
    if session.is_empty() {
        return ESP_OK;
    }
    if records.is_empty() {
        return ESP_ERR_INVALID_ARG;
    }
    persist.persist(session, request, records, turn_completed)
}

/// `claw_core_log_context_persist_failure`.
pub fn log_context_persist_failure(request: &RequestItem, operation: &str, err: EspErr) {
    if err == ESP_OK {
        return;
    }
    log::warn!(
        "{} failed for request={}: {}",
        operation,
        request.request_id,
        esp_err_to_name(err)
    );
}

/// `claw_core_persist_context_user_messages_if_configured`. Returns whether the
/// batch was actually persisted.
pub fn persist_context_user_messages_if_configured(
    persist: Option<&dyn PersistContext>,
    request: &RequestItem,
    texts: &[&str],
) -> Result<bool, EspErr> {
    if texts.is_empty() || texts.len() > INSERT_QUEUE_LEN {
        return Err(ESP_ERR_INVALID_ARG);
    }
    let has_persist = persist.is_some() && !request.session_id_str().is_empty();
    if !has_persist {
        return Ok(false);
    }
    let mut records = Vec::with_capacity(texts.len());
    for t in texts {
        if t.is_empty() {
            return Err(ESP_ERR_INVALID_ARG);
        }
        records.push(PersistRecord {
            record_type: ContextRecordType::User,
            message_json: None,
            text: Some((*t).to_string()),
        });
    }
    let err = persist_batch_if_configured(persist, request, &records, false);
    if err == ESP_OK {
        Ok(true)
    } else {
        Err(err)
    }
}

/// `claw_core_persist_context_tool_round_if_configured`.
pub fn persist_context_tool_round_if_configured(
    persist: Option<&dyn PersistContext>,
    request: &RequestItem,
    assistant_tool_message_json: &str,
    tool_results_json: &str,
) -> EspErr {
    if assistant_tool_message_json.is_empty() || tool_results_json.is_empty() {
        return ESP_ERR_INVALID_ARG;
    }
    let records = [
        PersistRecord {
            record_type: ContextRecordType::AssistantTool,
            message_json: Some(assistant_tool_message_json.to_string()),
            text: None,
        },
        PersistRecord {
            record_type: ContextRecordType::ToolResult,
            message_json: Some(tool_results_json.to_string()),
            text: None,
        },
    ];
    persist_batch_if_configured(persist, request, &records, false)
}

/// `claw_core_persist_context_final_if_configured`.
pub fn persist_context_final_if_configured(
    persist: Option<&dyn PersistContext>,
    request: &RequestItem,
    assistant_final_json: Option<&str>,
    assistant_text: Option<&str>,
) -> EspErr {
    let records = [PersistRecord {
        record_type: ContextRecordType::AssistantFinal,
        message_json: assistant_final_json.map(|s| s.to_string()),
        text: assistant_text.map(|s| s.to_string()),
    }];
    persist_batch_if_configured(persist, request, &records, true)
}

/// `claw_core_build_context_failure_trace`.
pub fn build_context_failure_trace(error_message: Option<&str>, tool_summary: &str) -> String {
    let reason = error_message.filter(|s| !s.is_empty()).unwrap_or("unknown error");
    if !tool_summary.is_empty() {
        format!(
            "Session note: the previous request failed before producing a final answer.\nReason: {reason}\n{tool_summary}"
        )
    } else {
        format!(
            "Session note: the previous request failed before producing a final answer.\nReason: {reason}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedProvider {
        name: String,
        flags: u32,
        outcome_kind: ContextKind,
        content: String,
    }
    impl ContextProvider for FixedProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn flags(&self) -> u32 {
            self.flags
        }
        fn collect(&self, _request: &RequestItem) -> ProviderOutcome {
            ProviderOutcome::Provided {
                kind: self.outcome_kind,
                content: self.content.clone(),
            }
        }
    }

    fn req() -> RequestItem {
        RequestItem {
            request_id: 1,
            user_text: "hello".into(),
            source_channel: Some("cli".into()),
            source_chat_id: Some("c1".into()),
            ..Default::default()
        }
    }

    #[test]
    fn builds_prompt_messages_and_tools() {
        let providers: Vec<Box<dyn ContextProvider>> = vec![
            Box::new(FixedProvider {
                name: "skills".into(),
                flags: 0,
                outcome_kind: ContextKind::SystemPrompt,
                content: "be nice".into(),
            }),
            Box::new(FixedProvider {
                name: "history".into(),
                flags: 0,
                outcome_kind: ContextKind::Messages,
                content: r#"[{"role":"user","content":"prev"}]"#.into(),
            }),
            Box::new(FixedProvider {
                name: "tools".into(),
                flags: 0,
                outcome_kind: ContextKind::Tools,
                content: r#"[{"type":"function"}]"#.into(),
            }),
        ];
        let mut csv = String::new();
        let runtime = Value::Array(vec![]);
        let built = build_iteration_context(
            "BASE",
            &providers,
            &req(),
            &runtime,
            &[],
            true,
            &mut csv,
        )
        .unwrap();

        assert!(built.system_prompt.starts_with("BASE"));
        assert!(built.system_prompt.contains("## skills\nbe nice"));
        assert!(built.system_prompt.contains("## Core Request"));
        // history message + injected active user
        let msgs = built.messages.as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["content"], "prev");
        assert_eq!(msgs[1]["content"], "hello");
        assert!(built.tools_json.is_some());
        assert_eq!(csv, "skills,history,tools");
    }

    #[test]
    fn append_assistant_then_tool_results() {
        struct EchoCap;
        impl CapCaller for EchoCap {
            fn call_cap(
                &self,
                cap_name: &str,
                input_json: &str,
                _request: &RequestItem,
            ) -> (EspErr, Option<String>) {
                (ESP_OK, Some(format!("{cap_name}:{input_json}")))
            }
        }
        let resp = LlmResponse {
            text: None,
            reasoning_content: None,
            raw_message_json: Some(
                r#"{"role":"assistant","tool_calls":[{"id":"t1"}]}"#.to_string(),
            ),
            tool_calls: vec![claw_api::ToolCall {
                id: "t1".into(),
                name: "files".into(),
                arguments_json: "{}".into(),
            }],
        };
        let mut runtime = Value::Array(vec![]);
        assert_eq!(append_assistant_tool_calls(&mut runtime, &resp), ESP_OK);
        let mut summary = String::new();
        let results =
            append_tool_results_messages(&EchoCap, &mut runtime, &resp, &req(), &mut summary)
                .unwrap();

        let arr = runtime.as_array().unwrap();
        assert_eq!(arr.len(), 2); // assistant + 1 tool result
        assert_eq!(arr[1]["role"], "tool");
        assert_eq!(arr[1]["tool_call_id"], "t1");
        assert_eq!(arr[1]["content"], "files:{}");
        assert_eq!(arr[1]["is_error"], false);
        assert!(results.contains("\"role\":\"tool\""));
        assert_eq!(summary, "[tool_calls]\n- files: ok\n");
    }

    #[test]
    fn failure_trace_with_and_without_summary() {
        let t = build_context_failure_trace(Some("boom"), "");
        assert!(t.contains("Reason: boom"));
        assert!(!t.contains("[tool_calls]"));
        let t2 = build_context_failure_trace(None, "[tool_calls]\n- x: ok\n");
        assert!(t2.contains("Reason: unknown error"));
        assert!(t2.contains("[tool_calls]"));
    }
}
