//! Outbound and stage event construction, port of `claw_core_events.c`.
//!
//! Events are built as [`ClawEvent`]s and handed to an injected
//! [`EventPublisher`] (the C `claw_event_router_publish`).

use claw_interfaces::error::{EspErr, ESP_ERR_INVALID_ARG, ESP_OK};
use claw_interfaces::event::{ClawEvent, EventPublisher, SessionPolicy};

use crate::consts::{
    ResponseStatus, REQUEST_FLAG_PUBLISH_OUT_MESSAGE, REQUEST_FLAG_PUBLISH_STAGE_MESSAGE,
};
use crate::request::RequestItem;
use crate::response::ResponseItem;
use crate::util::now_ms;

fn non_empty<'a>(value: &'a Option<String>) -> Option<&'a str> {
    value.as_deref().filter(|s| !s.is_empty())
}

/// `build_out_message_event_common`.
#[allow(clippy::too_many_arguments)]
fn build_common(
    event_id_prefix: &str,
    event_type: &str,
    request_id: u32,
    now: i64,
    channel: &str,
    chat_id: &str,
    text: &str,
) -> Option<ClawEvent> {
    if event_id_prefix.is_empty()
        || event_type.is_empty()
        || channel.is_empty()
        || chat_id.is_empty()
        || text.is_empty()
    {
        return None;
    }
    Some(ClawEvent {
        event_id: format!("{event_id_prefix}-{request_id}-{now}"),
        source_cap: "claw_core".to_string(),
        event_type: event_type.to_string(),
        source_channel: channel.to_string(),
        chat_id: chat_id.to_string(),
        content_type: "text".to_string(),
        text: Some(text.to_string()),
        timestamp_ms: now,
        session_policy: SessionPolicy::Chat,
        ..Default::default()
    })
}

/// `build_response_payload_json`.
fn build_response_payload_json(request: &RequestItem, response: &ResponseItem) -> String {
    let status = match response.status {
        ResponseStatus::Ok => "ok",
        ResponseStatus::Error => "error",
    };
    let mut obj = serde_json::Map::new();
    obj.insert("request_id".into(), serde_json::json!(request.request_id));
    obj.insert("status".into(), serde_json::json!(status));
    if let Some(err) = non_empty(&response.error_message) {
        obj.insert("error_message".into(), serde_json::json!(err));
    }
    serde_json::Value::Object(obj).to_string()
}

/// `claw_core_publish_out_message_if_requested`.
pub fn publish_out_message_if_requested(
    publisher: &dyn EventPublisher,
    request: &RequestItem,
    response: &ResponseItem,
) {
    if request.flags & REQUEST_FLAG_PUBLISH_OUT_MESSAGE == 0 {
        return;
    }

    let text = match response.status {
        ResponseStatus::Ok => non_empty(&response.text),
        ResponseStatus::Error => non_empty(&response.error_message),
    };
    let text = match text {
        Some(t) => t.to_string(),
        None => return,
    };

    let channel = non_empty(&response.target_channel)
        .or_else(|| request.source_channel.as_deref())
        .unwrap_or("");
    let chat_id = non_empty(&response.target_chat_id)
        .or_else(|| request.source_chat_id.as_deref())
        .unwrap_or("");

    let now = now_ms();
    let mut event = match build_common(
        "agent",
        "out_message",
        request.request_id,
        now,
        channel,
        chat_id,
        &text,
    ) {
        Some(e) => e,
        None => return,
    };

    event.message_id = format!("agent-{}", request.request_id);
    event.correlation_id = match non_empty(&request.source_message_id) {
        Some(id) => id.to_string(),
        None => format!("{}", request.request_id),
    };
    event.payload_json = Some(build_response_payload_json(request, response));

    let err = publisher.publish(&event);
    if err != ESP_OK {
        log::error!(
            "Failed to publish out_message for request_id={}: {}",
            request.request_id,
            crate::errname::esp_err_to_name(err)
        );
    }
}

/// `claw_core_publish_stage_text`.
pub fn publish_stage_text(
    publisher: &dyn EventPublisher,
    request: &RequestItem,
    text: &str,
) -> EspErr {
    if text.is_empty() {
        return ESP_ERR_INVALID_ARG;
    }
    if request.flags & REQUEST_FLAG_PUBLISH_STAGE_MESSAGE == 0 {
        return ESP_OK;
    }

    let channel = non_empty(&request.target_channel)
        .or_else(|| request.source_channel.as_deref())
        .unwrap_or("");
    let chat_id = non_empty(&request.target_chat_id)
        .or_else(|| request.source_chat_id.as_deref())
        .unwrap_or("");

    let now = now_ms();
    let event = match build_common(
        "stage",
        "agent_stage",
        request.request_id,
        now,
        channel,
        chat_id,
        text,
    ) {
        Some(e) => e,
        None => return ESP_ERR_INVALID_ARG,
    };

    let err = publisher.publish(&event);
    if err != ESP_OK {
        log::warn!(
            "request={} failed to publish stage event: {}",
            request.request_id,
            crate::errname::esp_err_to_name(err)
        );
    }
    err
}
