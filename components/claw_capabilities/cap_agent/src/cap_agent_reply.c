/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "cap_agent_reply.h"

#include <inttypes.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "cJSON.h"
#include "claw_agent.h"
#include "claw_cap.h"
#include "claw_im_session.h"
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/portmacro.h"
#include "freertos/semphr.h"
#include "freertos/task.h"

static const char *TAG = "cap_agent_reply";

#define CAP_AGENT_REPLY_FIELD_LEN       96
#define CAP_AGENT_REPLY_MAX_PUMPS       32
#define CAP_AGENT_REPLY_CAP_OUTPUT_SIZE 256
#define CAP_AGENT_REPLY_MESSAGE_INITIAL_CAPACITY 256
#define CAP_AGENT_REPLY_TASK_STACK_SIZE 8192
/* A pump is session-long; this slice only lets it observe shutdown/errors. */
#define CAP_AGENT_REPLY_RECV_SLICE_MS   5000

typedef struct {
    uint32_t session_id;
    char channel[CAP_AGENT_REPLY_FIELD_LEN];
    char chat_id[CAP_AGENT_REPLY_FIELD_LEN];
    char correlation_id[CAP_AGENT_REPLY_FIELD_LEN];
} cap_agent_reply_route_snapshot_t;

typedef struct {
    uint32_t session_id;
    bool pending_user_turn;
    char last_channel[CAP_AGENT_REPLY_FIELD_LEN];
    char last_chat_id[CAP_AGENT_REPLY_FIELD_LEN];
    char pending_channel[CAP_AGENT_REPLY_FIELD_LEN];
    char pending_chat_id[CAP_AGENT_REPLY_FIELD_LEN];
    char pending_correlation_id[CAP_AGENT_REPLY_FIELD_LEN];
    char active_channel[CAP_AGENT_REPLY_FIELD_LEN];
    char active_chat_id[CAP_AGENT_REPLY_FIELD_LEN];
    char active_correlation_id[CAP_AGENT_REPLY_FIELD_LEN];
    char *output_buffer;
    size_t output_length;
    size_t output_capacity;
    bool output_discarded;
    bool suppress_turn_output;
} cap_agent_reply_pump_t;

static cap_agent_reply_pump_t *s_reply_pumps[CAP_AGENT_REPLY_MAX_PUMPS];
static SemaphoreHandle_t s_reply_pumps_mutex;
static portMUX_TYPE s_reply_pumps_mutex_init_lock = portMUX_INITIALIZER_UNLOCKED;

static bool cap_agent_str_empty(const char *value)
{
    return !value || !value[0];
}

static void cap_agent_reply_reset_output(cap_agent_reply_pump_t *pump)
{
    pump->output_length = 0;
    pump->output_discarded = false;
    if (pump->output_buffer) {
        pump->output_buffer[0] = '\0';
    }
}

static void cap_agent_reply_discard_output(cap_agent_reply_pump_t *pump)
{
    cap_agent_reply_reset_output(pump);
    pump->output_discarded = true;
}

static esp_err_t cap_agent_reply_append_output(cap_agent_reply_pump_t *pump,
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
    if (text_length > SIZE_MAX - pump->output_length - 1) {
        cap_agent_reply_discard_output(pump);
        return ESP_ERR_INVALID_SIZE;
    }
    required = pump->output_length + text_length + 1;
    if (required > pump->output_capacity) {
        capacity = pump->output_capacity;
        if (capacity == 0) {
            capacity = CAP_AGENT_REPLY_MESSAGE_INITIAL_CAPACITY;
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
            cap_agent_reply_discard_output(pump);
            return ESP_ERR_NO_MEM;
        }
        pump->output_buffer = buffer;
        pump->output_capacity = capacity;
    }

    memcpy(pump->output_buffer + pump->output_length, text, text_length + 1);
    pump->output_length += text_length;
    return ESP_OK;
}

static esp_err_t cap_agent_reply_ensure_mutex(void)
{
    SemaphoreHandle_t candidate;

    if (s_reply_pumps_mutex) {
        return ESP_OK;
    }

    candidate = xSemaphoreCreateMutex();
    if (!candidate) {
        return ESP_ERR_NO_MEM;
    }
    portENTER_CRITICAL(&s_reply_pumps_mutex_init_lock);
    if (!s_reply_pumps_mutex) {
        s_reply_pumps_mutex = candidate;
        candidate = NULL;
    }
    portEXIT_CRITICAL(&s_reply_pumps_mutex_init_lock);
    if (candidate) {
        vSemaphoreDelete(candidate);
    }
    return s_reply_pumps_mutex ? ESP_OK : ESP_ERR_NO_MEM;
}

static esp_err_t cap_agent_reply_lock(void)
{
    esp_err_t err = cap_agent_reply_ensure_mutex();

    if (err != ESP_OK) {
        return err;
    }
    xSemaphoreTake(s_reply_pumps_mutex, portMAX_DELAY);
    return ESP_OK;
}

static void cap_agent_reply_unlock(void)
{
    xSemaphoreGive(s_reply_pumps_mutex);
}

/* Must be called with s_reply_pumps_mutex held. */
static cap_agent_reply_pump_t *cap_agent_reply_find_locked(uint32_t session_id)
{
    for (size_t i = 0; i < CAP_AGENT_REPLY_MAX_PUMPS; i++) {
        cap_agent_reply_pump_t *pump = s_reply_pumps[i];

        if (pump && pump->session_id == session_id) {
            return pump;
        }
    }
    return NULL;
}

static const char *cap_agent_send_capability(const char *channel)
{
    if (cap_agent_str_empty(channel)) {
        return NULL;
    }
    if (strcmp(channel, "feishu") == 0) {
        return "feishu_send_message";
    }
    if (strcmp(channel, "qq") == 0) {
        return "qq_send_message";
    }
    if (strcmp(channel, "tg") == 0 || strcmp(channel, "telegram") == 0) {
        return "tg_send_message";
    }
    if (strcmp(channel, "wechat") == 0) {
        return "wechat_send_message";
    }
    if (strcmp(channel, "local") == 0 || strcmp(channel, "web") == 0) {
        return "local_send_message";
    }
    return NULL;
}

bool cap_agent_reply_route_supported(const char *channel, const char *chat_id)
{
    return !cap_agent_str_empty(chat_id) && cap_agent_send_capability(channel) != NULL;
}

static esp_err_t cap_agent_reply_snapshot_active(
    cap_agent_reply_pump_t *pump,
    cap_agent_reply_route_snapshot_t *out)
{
    esp_err_t err;

    if (!pump || !out) {
        return ESP_ERR_INVALID_ARG;
    }
    err = cap_agent_reply_lock();
    if (err != ESP_OK) {
        return err;
    }
    memset(out, 0, sizeof(*out));
    out->session_id = pump->session_id;
    strlcpy(out->channel, pump->active_channel, sizeof(out->channel));
    strlcpy(out->chat_id, pump->active_chat_id, sizeof(out->chat_id));
    strlcpy(out->correlation_id,
            pump->active_correlation_id,
            sizeof(out->correlation_id));
    cap_agent_reply_unlock();
    return ESP_OK;
}

static char *cap_agent_build_message_payload(
    const cap_agent_reply_route_snapshot_t *route,
    const char *message,
    uint32_t request_id)
{
    cJSON *root = cJSON_CreateObject();
    cJSON *message_item;
    char *payload;

    if (!root) {
        return NULL;
    }
    if (!cJSON_AddStringToObject(root, "channel", route->channel) ||
            !cJSON_AddStringToObject(root, "chat_id", route->chat_id)) {
        cJSON_Delete(root);
        return NULL;
    }
    message_item = cJSON_CreateStringReference(message);
    if (!message_item || !cJSON_AddItemToObject(root, "message", message_item)) {
        cJSON_Delete(message_item);
        cJSON_Delete(root);
        return NULL;
    }
    if (request_id != 0 &&
            !cJSON_AddNumberToObject(root, "request_id", (double)request_id)) {
        cJSON_Delete(root);
        return NULL;
    }
    payload = cJSON_PrintUnformatted(root);
    cJSON_Delete(root);
    return payload;
}

static esp_err_t cap_agent_send_reply(cap_agent_reply_pump_t *pump,
                                      const char *message,
                                      uint32_t request_id)
{
    cap_agent_reply_route_snapshot_t route;
    const char *cap_name;
    char session_id[16];
    char *payload;
    char *output;
    claw_cap_call_context_t call_ctx = {0};
    esp_err_t err;

    if (cap_agent_str_empty(message)) {
        return ESP_OK;
    }
    err = cap_agent_reply_snapshot_active(pump, &route);
    if (err != ESP_OK) {
        return err;
    }
    cap_name = cap_agent_send_capability(route.channel);
    if (!cap_name || cap_agent_str_empty(route.chat_id)) {
        return ESP_OK;
    }

    payload = cap_agent_build_message_payload(&route, message, request_id);
    if (!payload) {
        return ESP_ERR_NO_MEM;
    }
    output = calloc(1, CAP_AGENT_REPLY_CAP_OUTPUT_SIZE);
    if (!output) {
        free(payload);
        return ESP_ERR_NO_MEM;
    }

    snprintf(session_id, sizeof(session_id), "%" PRIu32, route.session_id);
    call_ctx.request_id = request_id;
    call_ctx.session_id = session_id;
    call_ctx.channel = route.channel;
    call_ctx.chat_id = route.chat_id;
    call_ctx.target_channel = route.channel;
    call_ctx.target_chat_id = route.chat_id;
    call_ctx.source_cap = "cap_agent";
    call_ctx.correlation_id = route.correlation_id[0] ? route.correlation_id : NULL;
    call_ctx.caller = CLAW_CAP_CALLER_SYSTEM;

    err = claw_cap_call(cap_name,
                        payload,
                        &call_ctx,
                        output,
                        CAP_AGENT_REPLY_CAP_OUTPUT_SIZE);
    if (err == ESP_OK && request_id != 0) {
        err = claw_im_session_note_input_request(route.channel,
                                                 route.chat_id,
                                                 route.session_id,
                                                 request_id);
    }
    ESP_LOGI(TAG,
             "send reply cap=%s session=%" PRIu32 " request=%" PRIu32
             " err=%s output=%s",
             cap_name,
             route.session_id,
             request_id,
             esp_err_to_name(err),
             output[0] ? output : "-");

    free(output);
    free(payload);
    return err;
}

static esp_err_t cap_agent_reply_flush_output(cap_agent_reply_pump_t *pump)
{
    esp_err_t err = ESP_OK;

    if (!pump->output_discarded && !pump->suppress_turn_output &&
            pump->output_length > 0) {
        err = cap_agent_send_reply(pump, pump->output_buffer, 0);
    }
    cap_agent_reply_reset_output(pump);
    return err;
}

static char *cap_agent_format_permission_request(const char *summary)
{
    static const char prefix[] = "Permission approval needed:\n";
    static const char suffix[] = "\n\nReply with approval or rejection.";
    size_t summary_len;
    size_t message_len;
    char *message;

    if (cap_agent_str_empty(summary)) {
        return NULL;
    }
    summary_len = strlen(summary);
    if (summary_len > SIZE_MAX - sizeof(prefix) - sizeof(suffix)) {
        return NULL;
    }
    message_len = (sizeof(prefix) - 1) + summary_len + (sizeof(suffix) - 1);
    message = malloc(message_len + 1);
    if (!message) {
        return NULL;
    }
    snprintf(message, message_len + 1, "%s%s%s", prefix, summary, suffix);
    return message;
}

static esp_err_t cap_agent_send_input_request(cap_agent_reply_pump_t *pump,
                                              const claw_agent_event_t *event)
{
    const claw_agent_input_requested_event_t *request = &event->data.input_requested;
    char *message;
    esp_err_t err;

    if (request->request_id == 0 ||
            request->kind != CLAW_AGENT_INPUT_REQUEST_KIND_PERMISSION_APPROVAL) {
        return ESP_ERR_INVALID_ARG;
    }
    message = cap_agent_format_permission_request(request->summary);
    if (!message) {
        return ESP_ERR_NO_MEM;
    }
    err = cap_agent_send_reply(pump, message, request->request_id);
    free(message);
    return err;
}

static void cap_agent_reply_begin_turn(cap_agent_reply_pump_t *pump,
                                       const claw_agent_turn_started_event_t *turn)
{
    esp_err_t err;

    cap_agent_reply_reset_output(pump);
    pump->suppress_turn_output = false;

    err = cap_agent_reply_lock();
    if (err == ESP_OK) {
        if (turn->origin == CLAW_AGENT_TURN_ORIGIN_USER && pump->pending_user_turn) {
            strlcpy(pump->active_channel,
                    pump->pending_channel,
                    sizeof(pump->active_channel));
            strlcpy(pump->active_chat_id,
                    pump->pending_chat_id,
                    sizeof(pump->active_chat_id));
            strlcpy(pump->active_correlation_id,
                    pump->pending_correlation_id,
                    sizeof(pump->active_correlation_id));
            pump->pending_user_turn = false;
            pump->pending_channel[0] = '\0';
            pump->pending_chat_id[0] = '\0';
            pump->pending_correlation_id[0] = '\0';
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
        cap_agent_reply_unlock();
    }

    ESP_LOGI(TAG,
             "turn started session=%" PRIu32 " turn=%" PRIu32
             " origin=%d agent=%" PRIu32,
             pump->session_id,
             turn->turn_id,
             (int)turn->origin,
             turn->agent_id);
}

static void cap_agent_reply_clear_active(cap_agent_reply_pump_t *pump)
{
    if (cap_agent_reply_lock() != ESP_OK) {
        return;
    }
    pump->active_channel[0] = '\0';
    pump->active_chat_id[0] = '\0';
    pump->active_correlation_id[0] = '\0';
    cap_agent_reply_unlock();
}

static void cap_agent_reply_remove_pump(cap_agent_reply_pump_t *pump)
{
    if (cap_agent_reply_lock() != ESP_OK) {
        ESP_LOGE(TAG,
                 "failed to remove reply pump session=%" PRIu32,
                 pump->session_id);
        return;
    }
    for (size_t i = 0; i < CAP_AGENT_REPLY_MAX_PUMPS; i++) {
        if (s_reply_pumps[i] == pump) {
            s_reply_pumps[i] = NULL;
            break;
        }
    }
    cap_agent_reply_unlock();
    (void)claw_im_session_mark_closed(pump->session_id);
    free(pump->output_buffer);
    free(pump);
}

static void cap_agent_reply_task(void *param)
{
    cap_agent_reply_pump_t *pump = (cap_agent_reply_pump_t *)param;
    bool closed = false;

    while (!closed) {
        claw_agent_event_t event = {0};
        esp_err_t err = claw_agent_session_receive(pump->session_id,
                                                   &event,
                                                   CAP_AGENT_REPLY_RECV_SLICE_MS);
        if (err == ESP_ERR_TIMEOUT) {
            continue;
        }
        if (err != ESP_OK) {
            ESP_LOGW(TAG,
                     "receive failed session=%" PRIu32 " err=%s",
                     pump->session_id,
                     esp_err_to_name(err));
            break;
        }

        switch (event.kind) {
        case CLAW_AGENT_EVENT_KIND_TURN_STARTED:
            cap_agent_reply_begin_turn(pump, &event.data.turn_started);
            break;
        case CLAW_AGENT_EVENT_KIND_INPUT_REQUESTED: {
            esp_err_t send_err = cap_agent_send_input_request(pump, &event);
            if (send_err != ESP_OK) {
                ESP_LOGW(TAG,
                         "input request delivery failed session=%" PRIu32 " err=%s",
                         pump->session_id,
                         esp_err_to_name(send_err));
            }
            break;
        }
        case CLAW_AGENT_EVENT_KIND_OUTPUT_DELTA: {
            esp_err_t append_err = cap_agent_reply_append_output(
                pump,
                event.data.text_delta.text);
            if (append_err != ESP_OK) {
                ESP_LOGW(TAG,
                         "reply aggregation failed session=%" PRIu32 " err=%s",
                         pump->session_id,
                         esp_err_to_name(append_err));
            }
            break;
        }
        case CLAW_AGENT_EVENT_KIND_REASONING_DELTA:
            break;
        case CLAW_AGENT_EVENT_KIND_OUTPUT_END: {
            esp_err_t send_err = cap_agent_reply_flush_output(pump);
            if (send_err != ESP_OK) {
                ESP_LOGW(TAG,
                         "reply send failed session=%" PRIu32 " err=%s",
                         pump->session_id,
                         esp_err_to_name(send_err));
            }
            break;
        }
        case CLAW_AGENT_EVENT_KIND_TOOL_CALL:
            ESP_LOGI(TAG,
                     "tool call session=%" PRIu32 " id=%s name=%s",
                     pump->session_id,
                     event.data.tool_call.id ? event.data.tool_call.id : "-",
                     event.data.tool_call.name ? event.data.tool_call.name : "-");
            break;
        case CLAW_AGENT_EVENT_KIND_ITERATION_STARTED:
            ESP_LOGD(TAG,
                     "iteration started session=%" PRIu32 " iteration=%" PRIu32,
                     pump->session_id,
                     event.data.iteration.iteration_id);
            break;
        case CLAW_AGENT_EVENT_KIND_REASONING_END:
        case CLAW_AGENT_EVENT_KIND_TOOL_CALLS_END:
        case CLAW_AGENT_EVENT_KIND_ITERATION_ENDED:
            break;
        case CLAW_AGENT_EVENT_KIND_TURN_ENDED:
            if (pump->output_length > 0 || pump->output_discarded) {
                ESP_LOGW(TAG,
                         "discarding unterminated output session=%" PRIu32,
                         pump->session_id);
            }
            cap_agent_reply_reset_output(pump);
            pump->suppress_turn_output = false;
            ESP_LOGI(TAG,
                     "turn ended session=%" PRIu32 " turn=%" PRIu32,
                     pump->session_id,
                     event.data.turn_ended.turn_id);
            cap_agent_reply_clear_active(pump);
            (void)claw_im_session_clear_session_input(pump->session_id);
            break;
        case CLAW_AGENT_EVENT_KIND_ERROR:
            cap_agent_reply_reset_output(pump);
            pump->suppress_turn_output = true;
            ESP_LOGW(TAG,
                     "agent error session=%" PRIu32 " error=%s",
                     pump->session_id,
                     event.data.error.message ? event.data.error.message : "-");
            break;
        case CLAW_AGENT_EVENT_KIND_CLOSED:
            cap_agent_reply_reset_output(pump);
            closed = true;
            break;
        default:
            ESP_LOGW(TAG,
                     "unknown event session=%" PRIu32 " kind=%d",
                     pump->session_id,
                     (int)event.kind);
            break;
        }

        claw_agent_event_free(&event);
    }

    cap_agent_reply_remove_pump(pump);
    vTaskDelete(NULL);
}

bool cap_agent_reply_is_attached(uint32_t session_id)
{
    bool attached = false;

    if (session_id == 0 || cap_agent_reply_lock() != ESP_OK) {
        return false;
    }
    attached = cap_agent_reply_find_locked(session_id) != NULL;
    cap_agent_reply_unlock();
    return attached;
}

esp_err_t cap_agent_reply_ensure(uint32_t session_id)
{
    cap_agent_reply_pump_t *pump;
    size_t free_slot = CAP_AGENT_REPLY_MAX_PUMPS;
    esp_err_t err;

    if (session_id == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    err = cap_agent_reply_lock();
    if (err != ESP_OK) {
        return err;
    }
    for (size_t i = 0; i < CAP_AGENT_REPLY_MAX_PUMPS; i++) {
        pump = s_reply_pumps[i];
        if (!pump) {
            if (free_slot == CAP_AGENT_REPLY_MAX_PUMPS) {
                free_slot = i;
            }
            continue;
        }
        if (pump->session_id == session_id) {
            cap_agent_reply_unlock();
            return ESP_OK;
        }
    }
    if (free_slot == CAP_AGENT_REPLY_MAX_PUMPS) {
        cap_agent_reply_unlock();
        return ESP_ERR_NO_MEM;
    }

    pump = calloc(1, sizeof(*pump));
    if (!pump) {
        cap_agent_reply_unlock();
        return ESP_ERR_NO_MEM;
    }
    pump->session_id = session_id;
    s_reply_pumps[free_slot] = pump;
    if (xTaskCreate(cap_agent_reply_task,
                    "cap_agent_reply",
                    CAP_AGENT_REPLY_TASK_STACK_SIZE,
                    pump,
                    tskIDLE_PRIORITY + 1,
                    NULL) != pdPASS) {
        s_reply_pumps[free_slot] = NULL;
        cap_agent_reply_unlock();
        free(pump);
        return ESP_ERR_NO_MEM;
    }
    cap_agent_reply_unlock();
    return ESP_OK;
}

esp_err_t cap_agent_reply_submit(uint32_t session_id,
                                 const char *text,
                                 const cap_agent_reply_route_t *route)
{
    cap_agent_reply_pump_t *pump;
    bool route_supported;
    esp_err_t err;

    if (session_id == 0 || !text) {
        return ESP_ERR_INVALID_ARG;
    }
    err = cap_agent_reply_lock();
    if (err != ESP_OK) {
        return err;
    }
    pump = cap_agent_reply_find_locked(session_id);
    if (!pump) {
        cap_agent_reply_unlock();
        return ESP_ERR_NOT_FOUND;
    }

    /* Keep TURN_STARTED behind this mutex until the Rust actor has accepted or
     * rejected the submit. No C-side busy decision is made here. */
    err = claw_agent_session_submit(session_id, text);
    if (err == ESP_OK) {
        route_supported = route &&
                cap_agent_reply_route_supported(route->channel, route->chat_id);
        pump->pending_user_turn = true;
        if (route_supported) {
            strlcpy(pump->pending_channel,
                    route->channel,
                    sizeof(pump->pending_channel));
            strlcpy(pump->pending_chat_id,
                    route->chat_id,
                    sizeof(pump->pending_chat_id));
            strlcpy(pump->pending_correlation_id,
                    route->correlation_id ? route->correlation_id : "",
                    sizeof(pump->pending_correlation_id));
            strlcpy(pump->last_channel,
                    route->channel,
                    sizeof(pump->last_channel));
            strlcpy(pump->last_chat_id,
                    route->chat_id,
                    sizeof(pump->last_chat_id));
        } else {
            pump->pending_channel[0] = '\0';
            pump->pending_chat_id[0] = '\0';
            pump->pending_correlation_id[0] = '\0';
            pump->last_channel[0] = '\0';
            pump->last_chat_id[0] = '\0';
        }
    }
    cap_agent_reply_unlock();
    return err;
}
