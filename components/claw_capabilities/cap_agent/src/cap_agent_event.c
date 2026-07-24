/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "cap_agent_event.h"

#include <inttypes.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>

#include "cJSON.h"
#include "claw_agent.h"
#include "claw_event_publisher.h"
#include "claw_im_session.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/portmacro.h"
#include "freertos/semphr.h"
#include "freertos/task.h"

static const char *TAG = "cap_agent_event";

#define CAP_AGENT_EVENT_SOURCE_CAP               "agent"
#define CAP_AGENT_EVENT_FIELD_LEN                96
#define CAP_AGENT_EVENT_MAX_PUMPS                32
#define CAP_AGENT_EVENT_MAX_PENDING_ROUTES       32
#define CAP_AGENT_EVENT_MESSAGE_INITIAL_CAPACITY 256
#define CAP_AGENT_EVENT_MAX_OUTPUT_BYTES         (32 * 1024)
#define CAP_AGENT_EVENT_TASK_STACK_SIZE          8192
/* A pump is session-long; this slice only lets it observe shutdown/errors. */
#define CAP_AGENT_EVENT_RECV_SLICE_MS            5000

typedef struct cap_agent_pending_route_t {
    char channel[CAP_AGENT_EVENT_FIELD_LEN];
    char chat_id[CAP_AGENT_EVENT_FIELD_LEN];
    char correlation_id[CAP_AGENT_EVENT_FIELD_LEN];
    struct cap_agent_pending_route_t *next;
} cap_agent_pending_route_t;

typedef struct {
    uint32_t session_id;
    uint32_t current_turn_id;
    uint32_t event_sequence;
    size_t pending_route_count;
    cap_agent_pending_route_t *pending_route_head;
    cap_agent_pending_route_t *pending_route_tail;
    char last_channel[CAP_AGENT_EVENT_FIELD_LEN];
    char last_chat_id[CAP_AGENT_EVENT_FIELD_LEN];
    char active_channel[CAP_AGENT_EVENT_FIELD_LEN];
    char active_chat_id[CAP_AGENT_EVENT_FIELD_LEN];
    char active_correlation_id[CAP_AGENT_EVENT_FIELD_LEN];
    char *output_buffer;
    size_t output_length;
    size_t output_capacity;
    bool output_discarded;
    bool suppress_turn_output;
} cap_agent_event_pump_t;

static cap_agent_event_pump_t *s_event_pumps[CAP_AGENT_EVENT_MAX_PUMPS];
static SemaphoreHandle_t s_event_pumps_mutex;
static portMUX_TYPE s_event_pumps_mutex_init_lock = portMUX_INITIALIZER_UNLOCKED;

static bool cap_agent_str_empty(const char *value)
{
    return !value || !value[0];
}

static bool cap_agent_event_route_valid(const cap_agent_event_route_t *route)
{
    return route && !cap_agent_str_empty(route->channel) &&
           !cap_agent_str_empty(route->chat_id);
}

static void cap_agent_event_reset_output(cap_agent_event_pump_t *pump)
{
    pump->output_length = 0;
    pump->output_discarded = false;
    if (pump->output_buffer) {
        pump->output_buffer[0] = '\0';
    }
}

static void cap_agent_event_discard_output(cap_agent_event_pump_t *pump)
{
    cap_agent_event_reset_output(pump);
    pump->output_discarded = true;
}

static esp_err_t cap_agent_event_publish_error(cap_agent_event_pump_t *pump,
                                               const char *message);

static esp_err_t cap_agent_event_append_output(cap_agent_event_pump_t *pump,
                                               const char *text)
{
    size_t text_length;
    size_t required;
    size_t capacity;
    char *buffer;

    if (cap_agent_str_empty(text) || pump->output_discarded ||
            pump->suppress_turn_output) {
        return ESP_OK;
    }

    text_length = strlen(text);
    if (pump->output_length > CAP_AGENT_EVENT_MAX_OUTPUT_BYTES ||
            text_length >
            CAP_AGENT_EVENT_MAX_OUTPUT_BYTES - pump->output_length) {
        cap_agent_event_discard_output(pump);
        return ESP_ERR_INVALID_SIZE;
    }
    required = pump->output_length + text_length + 1;
    if (required > pump->output_capacity) {
        capacity = pump->output_capacity;
        if (capacity == 0) {
            capacity = CAP_AGENT_EVENT_MESSAGE_INITIAL_CAPACITY;
        }
        while (capacity < required) {
            if (capacity > SIZE_MAX / 2) {
                capacity = required;
                break;
            }
            capacity *= 2;
        }
        buffer = realloc(pump->output_buffer, capacity);
        if (!buffer) {
            cap_agent_event_discard_output(pump);
            return ESP_ERR_NO_MEM;
        }
        pump->output_buffer = buffer;
        pump->output_capacity = capacity;
    }

    memcpy(pump->output_buffer + pump->output_length, text, text_length + 1);
    pump->output_length += text_length;
    return ESP_OK;
}

static esp_err_t cap_agent_event_ensure_mutex(void)
{
    SemaphoreHandle_t candidate;

    if (s_event_pumps_mutex) {
        return ESP_OK;
    }

    candidate = xSemaphoreCreateMutex();
    if (!candidate) {
        return ESP_ERR_NO_MEM;
    }
    portENTER_CRITICAL(&s_event_pumps_mutex_init_lock);
    if (!s_event_pumps_mutex) {
        s_event_pumps_mutex = candidate;
        candidate = NULL;
    }
    portEXIT_CRITICAL(&s_event_pumps_mutex_init_lock);
    if (candidate) {
        vSemaphoreDelete(candidate);
    }
    return s_event_pumps_mutex ? ESP_OK : ESP_ERR_NO_MEM;
}

static esp_err_t cap_agent_event_lock(void)
{
    esp_err_t err = cap_agent_event_ensure_mutex();

    if (err != ESP_OK) {
        return err;
    }
    xSemaphoreTake(s_event_pumps_mutex, portMAX_DELAY);
    return ESP_OK;
}

static void cap_agent_event_unlock(void)
{
    xSemaphoreGive(s_event_pumps_mutex);
}

/* Must be called with s_event_pumps_mutex held. */
static cap_agent_event_pump_t *cap_agent_event_find_locked(uint32_t session_id)
{
    for (size_t i = 0; i < CAP_AGENT_EVENT_MAX_PUMPS; i++) {
        cap_agent_event_pump_t *pump = s_event_pumps[i];

        if (pump && pump->session_id == session_id) {
            return pump;
        }
    }
    return NULL;
}

static cJSON *cap_agent_event_payload_root(const char *kind)
{
    cJSON *root = cJSON_CreateObject();

    if (!root || !cJSON_AddStringToObject(root, "kind", kind)) {
        cJSON_Delete(root);
        return NULL;
    }
    return root;
}

static char *cap_agent_event_print_payload(cJSON *root)
{
    char *payload;

    if (!root) {
        return NULL;
    }
    payload = cJSON_PrintUnformatted(root);
    cJSON_Delete(root);
    return payload;
}

static esp_err_t cap_agent_event_publish(cap_agent_event_pump_t *pump,
                                         const char *event_type,
                                         const char *content_type,
                                         const char *text,
                                         uint32_t request_id,
                                         const char *payload_json,
                                         bool track_input_request)
{
    claw_event_t *event;
    int64_t now_us;
    struct timeval wall_time = {0};
    bool input_tracked = false;
    esp_err_t err;

    if (!pump || cap_agent_str_empty(event_type) ||
            cap_agent_str_empty(content_type)) {
        return ESP_ERR_INVALID_ARG;
    }
    event = calloc(1, sizeof(*event));
    if (!event) {
        return ESP_ERR_NO_MEM;
    }

    err = cap_agent_event_lock();
    if (err != ESP_OK) {
        free(event);
        return err;
    }
    pump->event_sequence++;
    now_us = esp_timer_get_time();
    gettimeofday(&wall_time, NULL);
    snprintf(event->event_id,
             sizeof(event->event_id),
             "agent-%" PRIu32 "-%" PRId64 "-%" PRIu32,
             pump->session_id,
             now_us,
             pump->event_sequence);
    strlcpy(event->source_cap, CAP_AGENT_EVENT_SOURCE_CAP, sizeof(event->source_cap));
    strlcpy(event->event_type, event_type, sizeof(event->event_type));
    strlcpy(event->source_channel,
            pump->active_channel,
            sizeof(event->source_channel));
    strlcpy(event->target_channel,
            pump->active_channel,
            sizeof(event->target_channel));
    strlcpy(event->chat_id, pump->active_chat_id, sizeof(event->chat_id));
    strlcpy(event->target_endpoint,
            pump->active_chat_id,
            sizeof(event->target_endpoint));
    strlcpy(event->correlation_id,
            pump->active_correlation_id,
            sizeof(event->correlation_id));
    strlcpy(event->content_type, content_type, sizeof(event->content_type));
    event->timestamp_ms = ((int64_t)wall_time.tv_sec * 1000) +
                          wall_time.tv_usec / 1000;
    event->session_id = pump->session_id;
    event->request_id = request_id;
    event->session_policy = CLAW_SESSION_POLICY_CHAT;
    cap_agent_event_unlock();

    event->text = (char *)text;
    event->payload_json = (char *)(payload_json ? payload_json : "{}");

    if (track_input_request && request_id != 0 &&
            event->target_channel[0] && event->target_endpoint[0]) {
        err = claw_im_session_note_input_request(event->target_channel,
                                                 event->target_endpoint,
                                                 event->session_id,
                                                 request_id);
        if (err != ESP_OK) {
            free(event);
            return err;
        }
        input_tracked = true;
    }

    err = claw_event_router_publish(event);
    if (err != ESP_OK && input_tracked) {
        (void)claw_im_session_clear_input_request(event->session_id, request_id);
    }
    ESP_LOGD(TAG,
             "published type=%s content=%s session=%" PRIu32
             " request=%" PRIu32 " err=%s",
             event_type,
             content_type,
             event->session_id,
             request_id,
             esp_err_to_name(err));
    free(event);
    return err;
}

static esp_err_t cap_agent_event_publish_simple(cap_agent_event_pump_t *pump,
                                                const char *kind,
                                                cJSON *payload)
{
    char *payload_json = cap_agent_event_print_payload(payload);
    esp_err_t err;

    if (!payload_json) {
        return ESP_ERR_NO_MEM;
    }
    err = cap_agent_event_publish(pump,
                                  "agent_event",
                                  kind,
                                  NULL,
                                  0,
                                  payload_json,
                                  false);
    free(payload_json);
    return err;
}

static esp_err_t cap_agent_event_flush_output(cap_agent_event_pump_t *pump)
{
    cJSON *payload;
    char *payload_json;
    esp_err_t err = ESP_OK;

    if (pump->output_discarded) {
        cap_agent_event_reset_output(pump);
        return cap_agent_event_publish_error(
            pump,
            "Agent output exceeded the device response buffer.");
    }
    if (pump->suppress_turn_output || pump->output_length == 0) {
        cap_agent_event_reset_output(pump);
        return ESP_OK;
    }

    payload = cap_agent_event_payload_root("output");
    if (!payload ||
            !cJSON_AddNumberToObject(payload,
                                    "turn_id",
                                    (double)pump->current_turn_id)) {
        cJSON_Delete(payload);
        cap_agent_event_reset_output(pump);
        return ESP_ERR_NO_MEM;
    }
    payload_json = cap_agent_event_print_payload(payload);
    if (!payload_json) {
        cap_agent_event_reset_output(pump);
        return ESP_ERR_NO_MEM;
    }
    err = cap_agent_event_publish(pump,
                                  "out_message",
                                  "text",
                                  pump->output_buffer,
                                  0,
                                  payload_json,
                                  false);
    free(payload_json);
    cap_agent_event_reset_output(pump);
    return err;
}

static char *cap_agent_format_permission_request(
    const claw_agent_input_requested_event_t *request)
{
    static const char prefix[] = "Permission approval needed:\n";
    static const char suffix[] = "\n\nReply with approval or rejection.";
    const char *tool_name;
    const char *arguments_json;
    const char *reason;
    size_t message_len;
    char *message;

    if (!request) {
        return NULL;
    }
    tool_name = request->tool_call.name ? request->tool_call.name : "unknown";
    arguments_json = request->tool_call.arguments_json ?
                     request->tool_call.arguments_json : "{}";
    reason = request->reason ? request->reason : "";
    if (strlen(tool_name) > SIZE_MAX - strlen(arguments_json) ||
            strlen(tool_name) + strlen(arguments_json) > SIZE_MAX - strlen(reason) ||
            strlen(tool_name) + strlen(arguments_json) + strlen(reason) >
            SIZE_MAX - sizeof(prefix) - sizeof(suffix) - 32) {
        return NULL;
    }
    message_len = (sizeof(prefix) - 1) + strlen("Tool: \nArguments: \nReason: ") +
                  strlen(tool_name) + strlen(arguments_json) + strlen(reason) +
                  (sizeof(suffix) - 1);
    message = malloc(message_len + 1);
    if (!message) {
        return NULL;
    }
    snprintf(message,
             message_len + 1,
             "%sTool: %s\nArguments: %s\nReason: %s%s",
             prefix,
             tool_name,
             arguments_json,
             reason,
             suffix);
    return message;
}

static esp_err_t cap_agent_event_publish_input_request(
    cap_agent_event_pump_t *pump,
    const claw_agent_event_t *event)
{
    const claw_agent_input_requested_event_t *request = &event->data.input_requested;
    cJSON *payload;
    char *payload_json;
    char *message;
    esp_err_t err;

    if (request->request_id == 0 ||
            request->kind != CLAW_AGENT_INPUT_REQUEST_KIND_PERMISSION_APPROVAL) {
        return ESP_ERR_INVALID_ARG;
    }
    message = cap_agent_format_permission_request(request);
    if (!message) {
        return ESP_ERR_NO_MEM;
    }
    payload = cap_agent_event_payload_root("input_requested");
    if (!payload ||
            !cJSON_AddNumberToObject(payload,
                                    "request_id",
                                    (double)request->request_id) ||
            !cJSON_AddStringToObject(payload,
                                    "input_kind",
                                    "permission_approval") ||
            !cJSON_AddStringToObject(payload,
                                    "tool_call_id",
                                    request->tool_call.id ?
                                    request->tool_call.id : "") ||
            !cJSON_AddStringToObject(payload,
                                    "tool_name",
                                    request->tool_call.name ?
                                    request->tool_call.name : "") ||
            !cJSON_AddStringToObject(payload,
                                    "arguments_json",
                                    request->tool_call.arguments_json ?
                                    request->tool_call.arguments_json : "{}") ||
            !cJSON_AddStringToObject(payload,
                                    "reason",
                                    request->reason ? request->reason : "")) {
        cJSON_Delete(payload);
        free(message);
        return ESP_ERR_NO_MEM;
    }
    payload_json = cap_agent_event_print_payload(payload);
    if (!payload_json) {
        free(message);
        return ESP_ERR_NO_MEM;
    }
    err = cap_agent_event_publish(pump,
                                  "out_message",
                                  "input_request",
                                  message,
                                  request->request_id,
                                  payload_json,
                                  true);
    free(payload_json);
    free(message);
    return err;
}

static esp_err_t cap_agent_event_publish_tool_call(
    cap_agent_event_pump_t *pump,
    const claw_agent_tool_call_event_t *tool_call)
{
    cJSON *payload = cap_agent_event_payload_root("tool_call");
    char *payload_json;
    char message[96];
    esp_err_t err;

    if (!payload ||
            !cJSON_AddStringToObject(payload,
                                    "tool_call_id",
                                    tool_call && tool_call->id ?
                                    tool_call->id : "") ||
            !cJSON_AddStringToObject(payload,
                                    "tool_name",
                                    tool_call && tool_call->name ?
                                    tool_call->name : "") ||
            !cJSON_AddStringToObject(payload,
                                    "arguments_json",
                                    tool_call && tool_call->arguments_json ?
                                    tool_call->arguments_json : "{}")) {
        cJSON_Delete(payload);
        return ESP_ERR_NO_MEM;
    }
    payload_json = cap_agent_event_print_payload(payload);
    if (!payload_json) {
        return ESP_ERR_NO_MEM;
    }
    snprintf(message,
             sizeof(message),
             "Tool completed: %s",
             tool_call && tool_call->name ? tool_call->name : "unknown");
    err = cap_agent_event_publish(pump,
                                  "agent_stage",
                                  "tool_call",
                                  message,
                                  0,
                                  payload_json,
                                  false);
    free(payload_json);
    return err;
}

static esp_err_t cap_agent_event_publish_error(cap_agent_event_pump_t *pump,
                                               const char *message)
{
    cJSON *payload = cap_agent_event_payload_root("error");
    char *payload_json;
    esp_err_t err;

    if (!payload ||
            !cJSON_AddStringToObject(payload,
                                    "message",
                                    message ? message : "unknown agent error")) {
        cJSON_Delete(payload);
        return ESP_ERR_NO_MEM;
    }
    payload_json = cap_agent_event_print_payload(payload);
    if (!payload_json) {
        return ESP_ERR_NO_MEM;
    }
    err = cap_agent_event_publish(pump,
                                  "out_message",
                                  "error",
                                  message ? message : "Agent request failed.",
                                  0,
                                  payload_json,
                                  false);
    free(payload_json);
    return err;
}

static esp_err_t cap_agent_event_publish_usage(
    cap_agent_event_pump_t *pump,
    const claw_agent_usage_event_t *usage)
{
    cJSON *payload = cap_agent_event_payload_root("usage");

    if (!payload) {
        return ESP_ERR_NO_MEM;
    }
#define CAP_AGENT_ADD_USAGE_FIELD(json, field_name, value)                    \
    do {                                                                      \
        if ((value) == UINT64_MAX) {                                          \
            if (!cJSON_AddNullToObject((json), (field_name))) {               \
                cJSON_Delete(json);                                            \
                return ESP_ERR_NO_MEM;                                         \
            }                                                                 \
        } else if (!cJSON_AddNumberToObject((json),                           \
                                             (field_name),                     \
                                             (double)(value))) {               \
            cJSON_Delete(json);                                                \
            return ESP_ERR_NO_MEM;                                             \
        }                                                                     \
    } while (0)

    CAP_AGENT_ADD_USAGE_FIELD(payload, "input_tokens", usage->input_tokens);
    CAP_AGENT_ADD_USAGE_FIELD(payload, "output_tokens", usage->output_tokens);
    CAP_AGENT_ADD_USAGE_FIELD(payload, "cache_read_tokens", usage->cache_read_tokens);
    CAP_AGENT_ADD_USAGE_FIELD(payload, "cache_write_tokens", usage->cache_write_tokens);
#undef CAP_AGENT_ADD_USAGE_FIELD

    return cap_agent_event_publish_simple(pump, "usage", payload);
}

static void cap_agent_event_begin_turn(cap_agent_event_pump_t *pump,
                                       const claw_agent_turn_started_event_t *turn)
{
    cap_agent_pending_route_t *pending = NULL;
    cJSON *payload;
    esp_err_t err;

    cap_agent_event_reset_output(pump);
    pump->suppress_turn_output = false;
    pump->current_turn_id = turn->turn_id;

    err = cap_agent_event_lock();
    if (err == ESP_OK) {
        if (turn->origin == CLAW_AGENT_TURN_ORIGIN_USER &&
                pump->pending_route_head) {
            pending = pump->pending_route_head;
            pump->pending_route_head = pending->next;
            if (!pump->pending_route_head) {
                pump->pending_route_tail = NULL;
            }
            pump->pending_route_count--;
            strlcpy(pump->active_channel,
                    pending->channel,
                    sizeof(pump->active_channel));
            strlcpy(pump->active_chat_id,
                    pending->chat_id,
                    sizeof(pump->active_chat_id));
            strlcpy(pump->active_correlation_id,
                    pending->correlation_id,
                    sizeof(pump->active_correlation_id));
        } else {
            strlcpy(pump->active_channel,
                    pump->last_channel,
                    sizeof(pump->active_channel));
            strlcpy(pump->active_chat_id,
                    pump->last_chat_id,
                    sizeof(pump->active_chat_id));
            pump->active_correlation_id[0] = '\0';
            if (turn->origin == CLAW_AGENT_TURN_ORIGIN_USER) {
                ESP_LOGW(TAG,
                         "user turn has no accepted route session=%" PRIu32
                         " turn=%" PRIu32,
                         pump->session_id,
                         turn->turn_id);
            }
        }
        cap_agent_event_unlock();
    }
    free(pending);

    payload = cap_agent_event_payload_root("turn_started");
    if (payload) {
        cJSON_AddNumberToObject(payload, "turn_id", (double)turn->turn_id);
        cJSON_AddStringToObject(
            payload,
            "origin",
            turn->origin == CLAW_AGENT_TURN_ORIGIN_USER ? "user" : "tool_call");
        cJSON_AddNumberToObject(payload, "agent_id", (double)turn->agent_id);
        (void)cap_agent_event_publish_simple(pump, "turn_started", payload);
    }
}

static void cap_agent_event_clear_active(cap_agent_event_pump_t *pump)
{
    if (cap_agent_event_lock() != ESP_OK) {
        return;
    }
    pump->active_channel[0] = '\0';
    pump->active_chat_id[0] = '\0';
    pump->active_correlation_id[0] = '\0';
    cap_agent_event_unlock();
}

static void cap_agent_event_free_pending_routes(cap_agent_event_pump_t *pump)
{
    cap_agent_pending_route_t *route = pump->pending_route_head;

    while (route) {
        cap_agent_pending_route_t *next = route->next;

        free(route);
        route = next;
    }
    pump->pending_route_head = NULL;
    pump->pending_route_tail = NULL;
    pump->pending_route_count = 0;
}

static void cap_agent_event_remove_pump(cap_agent_event_pump_t *pump)
{
    if (cap_agent_event_lock() != ESP_OK) {
        ESP_LOGE(TAG,
                 "failed to remove event pump session=%" PRIu32,
                 pump->session_id);
        return;
    }
    for (size_t i = 0; i < CAP_AGENT_EVENT_MAX_PUMPS; i++) {
        if (s_event_pumps[i] == pump) {
            s_event_pumps[i] = NULL;
            break;
        }
    }
    cap_agent_event_free_pending_routes(pump);
    cap_agent_event_unlock();
    (void)claw_im_session_mark_closed(pump->session_id);
    free(pump->output_buffer);
    free(pump);
}

static void cap_agent_event_task(void *param)
{
    cap_agent_event_pump_t *pump = (cap_agent_event_pump_t *)param;
    bool closed = false;

    while (!closed) {
        claw_agent_event_t event = {0};
        esp_err_t err = claw_agent_session_receive(pump->session_id,
                                                   &event,
                                                   CAP_AGENT_EVENT_RECV_SLICE_MS);
        if (err == ESP_ERR_TIMEOUT) {
            continue;
        }
        if (err != ESP_OK) {
            char message[96];

            snprintf(message,
                     sizeof(message),
                     "Agent event stream failed: %s",
                     esp_err_to_name(err));
            (void)cap_agent_event_publish_error(pump, message);
            ESP_LOGW(TAG,
                     "receive failed session=%" PRIu32 " err=%s",
                     pump->session_id,
                     esp_err_to_name(err));
            break;
        }

        switch (event.kind) {
        case CLAW_AGENT_EVENT_KIND_TURN_STARTED:
            cap_agent_event_begin_turn(pump, &event.data.turn_started);
            break;
        case CLAW_AGENT_EVENT_KIND_INPUT_REQUESTED:
            err = cap_agent_event_publish_input_request(pump, &event);
            break;
        case CLAW_AGENT_EVENT_KIND_OUTPUT_DELTA:
            err = cap_agent_event_append_output(pump, event.data.text_delta.text);
            break;
        case CLAW_AGENT_EVENT_KIND_OUTPUT_END:
            err = cap_agent_event_flush_output(pump);
            break;
        case CLAW_AGENT_EVENT_KIND_TOOL_CALL:
            err = cap_agent_event_publish_tool_call(pump, &event.data.tool_call);
            break;
        case CLAW_AGENT_EVENT_KIND_ITERATION_STARTED: {
            cJSON *payload = cap_agent_event_payload_root("iteration_started");

            if (payload) {
                cJSON_AddNumberToObject(payload,
                                        "iteration_id",
                                        (double)event.data.iteration.iteration_id);
                err = cap_agent_event_publish_simple(pump,
                                                     "iteration_started",
                                                     payload);
            } else {
                err = ESP_ERR_NO_MEM;
            }
            break;
        }
        case CLAW_AGENT_EVENT_KIND_REASONING_DELTA:
            /* Streaming deltas are intentionally not queued through the
             * rule engine. OUTPUT_DELTA is coalesced at OUTPUT_END. */
            break;
        case CLAW_AGENT_EVENT_KIND_REASONING_END:
        case CLAW_AGENT_EVENT_KIND_TOOL_CALLS_END:
        case CLAW_AGENT_EVENT_KIND_ITERATION_ENDED:
            break;
        case CLAW_AGENT_EVENT_KIND_TURN_ENDED: {
            cJSON *payload;

            if (pump->output_length > 0 || pump->output_discarded) {
                ESP_LOGW(TAG,
                         "discarding unterminated output session=%" PRIu32,
                         pump->session_id);
            }
            cap_agent_event_reset_output(pump);
            pump->suppress_turn_output = false;
            payload = cap_agent_event_payload_root("turn_ended");
            if (payload) {
                cJSON_AddNumberToObject(payload,
                                        "turn_id",
                                        (double)event.data.turn_ended.turn_id);
                err = cap_agent_event_publish_simple(pump, "turn_ended", payload);
            } else {
                err = ESP_ERR_NO_MEM;
            }
            cap_agent_event_clear_active(pump);
            (void)claw_im_session_clear_session_input(pump->session_id);
            break;
        }
        case CLAW_AGENT_EVENT_KIND_ERROR:
            cap_agent_event_reset_output(pump);
            pump->suppress_turn_output = true;
            err = cap_agent_event_publish_error(pump, event.data.error.message);
            break;
        case CLAW_AGENT_EVENT_KIND_USAGE:
            err = cap_agent_event_publish_usage(pump, &event.data.usage);
            break;
        case CLAW_AGENT_EVENT_KIND_CLOSED: {
            cJSON *payload = cap_agent_event_payload_root("closed");

            cap_agent_event_reset_output(pump);
            if (payload) {
                err = cap_agent_event_publish_simple(pump, "closed", payload);
            } else {
                err = ESP_ERR_NO_MEM;
            }
            closed = true;
            break;
        }
        default:
            ESP_LOGW(TAG,
                     "unknown event session=%" PRIu32 " kind=%d",
                     pump->session_id,
                     (int)event.kind);
            break;
        }

        if (err != ESP_OK) {
            ESP_LOGW(TAG,
                     "event translation failed session=%" PRIu32
                     " kind=%d err=%s",
                     pump->session_id,
                     (int)event.kind,
                     esp_err_to_name(err));
        }
        claw_agent_event_free(&event);
    }

    cap_agent_event_remove_pump(pump);
    vTaskDelete(NULL);
}

bool cap_agent_event_is_attached(uint32_t session_id)
{
    bool attached = false;

    if (session_id == 0 || cap_agent_event_lock() != ESP_OK) {
        return false;
    }
    attached = cap_agent_event_find_locked(session_id) != NULL;
    cap_agent_event_unlock();
    return attached;
}

esp_err_t cap_agent_event_attach(uint32_t session_id)
{
    cap_agent_event_pump_t *pump;
    size_t free_slot = CAP_AGENT_EVENT_MAX_PUMPS;
    esp_err_t err;

    if (session_id == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    err = cap_agent_event_lock();
    if (err != ESP_OK) {
        return err;
    }
    for (size_t i = 0; i < CAP_AGENT_EVENT_MAX_PUMPS; i++) {
        pump = s_event_pumps[i];
        if (!pump) {
            if (free_slot == CAP_AGENT_EVENT_MAX_PUMPS) {
                free_slot = i;
            }
            continue;
        }
        if (pump->session_id == session_id) {
            cap_agent_event_unlock();
            return ESP_OK;
        }
    }
    if (free_slot == CAP_AGENT_EVENT_MAX_PUMPS) {
        cap_agent_event_unlock();
        return ESP_ERR_NO_MEM;
    }

    pump = calloc(1, sizeof(*pump));
    if (!pump) {
        cap_agent_event_unlock();
        return ESP_ERR_NO_MEM;
    }
    pump->session_id = session_id;
    s_event_pumps[free_slot] = pump;
    if (xTaskCreate(cap_agent_event_task,
                    "cap_agent_event",
                    CAP_AGENT_EVENT_TASK_STACK_SIZE,
                    pump,
                    tskIDLE_PRIORITY + 1,
                    NULL) != pdPASS) {
        s_event_pumps[free_slot] = NULL;
        cap_agent_event_unlock();
        free(pump);
        return ESP_ERR_NO_MEM;
    }
    cap_agent_event_unlock();
    return ESP_OK;
}

esp_err_t cap_agent_event_submit(uint32_t session_id,
                                 const char *text,
                                 const cap_agent_event_route_t *route)
{
    cap_agent_event_pump_t *pump;
    cap_agent_pending_route_t *pending;
    esp_err_t err;

    if (session_id == 0 || !text) {
        return ESP_ERR_INVALID_ARG;
    }
    pending = calloc(1, sizeof(*pending));
    if (!pending) {
        return ESP_ERR_NO_MEM;
    }
    if (cap_agent_event_route_valid(route)) {
        strlcpy(pending->channel, route->channel, sizeof(pending->channel));
        strlcpy(pending->chat_id, route->chat_id, sizeof(pending->chat_id));
        strlcpy(pending->correlation_id,
                route->correlation_id ? route->correlation_id : "",
                sizeof(pending->correlation_id));
    }

    err = cap_agent_event_lock();
    if (err != ESP_OK) {
        free(pending);
        return err;
    }
    pump = cap_agent_event_find_locked(session_id);
    if (!pump) {
        cap_agent_event_unlock();
        free(pending);
        return ESP_ERR_NOT_FOUND;
    }
    if (pump->pending_route_count >= CAP_AGENT_EVENT_MAX_PENDING_ROUTES) {
        cap_agent_event_unlock();
        free(pending);
        return ESP_ERR_NO_MEM;
    }

    /* Keep TURN_STARTED behind this mutex until the Rust actor has accepted or
     * rejected the submit, then append route metadata in the same FIFO order
     * as SessionControl::append(). */
    err = claw_agent_session_submit(session_id, text);
    if (err == ESP_OK) {
        if (pump->pending_route_tail) {
            pump->pending_route_tail->next = pending;
        } else {
            pump->pending_route_head = pending;
        }
        pump->pending_route_tail = pending;
        pump->pending_route_count++;
        if (pending->channel[0] && pending->chat_id[0]) {
            strlcpy(pump->last_channel,
                    pending->channel,
                    sizeof(pump->last_channel));
            strlcpy(pump->last_chat_id,
                    pending->chat_id,
                    sizeof(pump->last_chat_id));
        }
        pending = NULL;
    }
    cap_agent_event_unlock();
    free(pending);
    return err;
}
