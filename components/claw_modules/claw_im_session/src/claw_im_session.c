/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "claw_im_session.h"

#include <inttypes.h>
#include <stdbool.h>
#include <string.h>

#include "cJSON.h"
#include "claw_cap.h"
#include "claw_event_publisher.h"
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/portmacro.h"
#include "freertos/semphr.h"

static const char *TAG = "claw_im_session";

#define CLAW_IM_SESSION_CURSOR_CAPACITY 32
#define CLAW_IM_SESSION_CHANNEL_SIZE    32
#define CLAW_IM_SESSION_CHAT_ID_SIZE    96
#define CLAW_IM_SESSION_RPC_OUTPUT_SIZE 256

typedef struct {
    bool occupied;
    bool open;
    char channel[CLAW_IM_SESSION_CHANNEL_SIZE];
    char chat_id[CLAW_IM_SESSION_CHAT_ID_SIZE];
    uint32_t session_id;
    uint32_t request_session_id;
    uint32_t request_id;
} claw_im_session_cursor_t;

static claw_im_session_cursor_t s_cursors[CLAW_IM_SESSION_CURSOR_CAPACITY];
static SemaphoreHandle_t s_cursor_mutex;
static portMUX_TYPE s_cursor_mutex_init_lock = portMUX_INITIALIZER_UNLOCKED;

static bool claw_im_session_key_valid(const char *channel, const char *chat_id)
{
    return channel && channel[0] &&
           strlen(channel) < CLAW_IM_SESSION_CHANNEL_SIZE &&
           chat_id && chat_id[0] &&
           strlen(chat_id) < CLAW_IM_SESSION_CHAT_ID_SIZE;
}

static esp_err_t claw_im_session_ensure_mutex(void)
{
    SemaphoreHandle_t candidate;

    if (s_cursor_mutex) {
        return ESP_OK;
    }
    candidate = xSemaphoreCreateMutex();
    if (!candidate) {
        return ESP_ERR_NO_MEM;
    }
    portENTER_CRITICAL(&s_cursor_mutex_init_lock);
    if (!s_cursor_mutex) {
        s_cursor_mutex = candidate;
        candidate = NULL;
    }
    portEXIT_CRITICAL(&s_cursor_mutex_init_lock);
    if (candidate) {
        vSemaphoreDelete(candidate);
    }
    return s_cursor_mutex ? ESP_OK : ESP_ERR_NO_MEM;
}

static esp_err_t claw_im_session_lock(void)
{
    esp_err_t err = claw_im_session_ensure_mutex();

    if (err != ESP_OK) {
        return err;
    }
    xSemaphoreTake(s_cursor_mutex, portMAX_DELAY);
    return ESP_OK;
}

static void claw_im_session_unlock(void)
{
    xSemaphoreGive(s_cursor_mutex);
}

/* Must be called with s_cursor_mutex held. */
static claw_im_session_cursor_t *claw_im_session_find_locked(
    const char *channel,
    const char *chat_id)
{
    for (size_t i = 0; i < CLAW_IM_SESSION_CURSOR_CAPACITY; i++) {
        claw_im_session_cursor_t *cursor = &s_cursors[i];

        if (cursor->occupied && strcmp(cursor->channel, channel) == 0 &&
                strcmp(cursor->chat_id, chat_id) == 0) {
            return cursor;
        }
    }
    return NULL;
}

/* Must be called with s_cursor_mutex held. */
static claw_im_session_cursor_t *claw_im_session_allocate_locked(void)
{
    for (size_t i = 0; i < CLAW_IM_SESSION_CURSOR_CAPACITY; i++) {
        if (!s_cursors[i].occupied) {
            return &s_cursors[i];
        }
    }
    return NULL;
}

static bool claw_im_session_is_command(const char *text)
{
    static const char prefix[] = "/session";

    if (!text) {
        return false;
    }
    while (*text == ' ' || *text == '\t' || *text == '\n' || *text == '\r' ||
            *text == '\f' || *text == '\v') {
        text++;
    }
    if (strncmp(text, prefix, sizeof(prefix) - 1) != 0) {
        return false;
    }
    text += sizeof(prefix) - 1;
    return *text == '\0' || *text == ' ' || *text == '\t' || *text == '\n' ||
           *text == '\r' || *text == '\f' || *text == '\v';
}

static esp_err_t claw_im_session_call_agent(const char *method,
                                            cJSON *args,
                                            cJSON **out_response)
{
    cJSON *request = NULL;
    cJSON *response = NULL;
    char *input_json = NULL;
    char *output = NULL;
    claw_cap_call_context_t ctx = {
        .source_cap = "claw_im_session",
        .caller = CLAW_CAP_CALLER_SYSTEM,
    };
    esp_err_t err;

    if (!method || !args) {
        cJSON_Delete(args);
        return ESP_ERR_INVALID_ARG;
    }
    request = cJSON_CreateObject();
    if (!request ||
            !cJSON_AddStringToObject(request, "method", method) ||
            !cJSON_AddItemToObject(request, "args", args)) {
        cJSON_Delete(request);
        cJSON_Delete(args);
        return ESP_ERR_NO_MEM;
    }
    args = NULL;
    input_json = cJSON_PrintUnformatted(request);
    cJSON_Delete(request);
    if (!input_json) {
        return ESP_ERR_NO_MEM;
    }
    output = calloc(1, CLAW_IM_SESSION_RPC_OUTPUT_SIZE);
    if (!output) {
        free(input_json);
        return ESP_ERR_NO_MEM;
    }
    err = claw_cap_call("agent",
                        input_json,
                        &ctx,
                        output,
                        CLAW_IM_SESSION_RPC_OUTPUT_SIZE);
    free(input_json);
    if (err == ESP_OK && out_response) {
        response = cJSON_Parse(output);
        if (!cJSON_IsObject(response)) {
            cJSON_Delete(response);
            err = ESP_ERR_INVALID_RESPONSE;
        } else {
            *out_response = response;
        }
    }
    free(output);
    return err;
}

static esp_err_t claw_im_session_agent_create(
    claw_agent_session_persistence_t persistence,
    uint32_t *out_session_id)
{
    cJSON *args = cJSON_CreateObject();
    cJSON *response = NULL;
    const cJSON *result;
    const cJSON *session_id;
    esp_err_t err;

    if (!out_session_id || !args ||
            !cJSON_AddStringToObject(
                args,
                "persistence",
                persistence == CLAW_AGENT_SESSION_PERSISTENCE_EPHEMERAL ?
                "ephemeral" : "persistent")) {
        cJSON_Delete(args);
        return ESP_ERR_NO_MEM;
    }
    err = claw_im_session_call_agent("session.create", args, &response);
    if (err != ESP_OK) {
        return err;
    }
    result = cJSON_GetObjectItemCaseSensitive(response, "result");
    session_id = cJSON_GetObjectItemCaseSensitive(result, "session_id");
    if (!cJSON_IsNumber(session_id) ||
            session_id->valuedouble < 1.0 ||
            session_id->valuedouble > (double)UINT32_MAX ||
            (double)(uint32_t)session_id->valuedouble != session_id->valuedouble) {
        cJSON_Delete(response);
        return ESP_ERR_INVALID_RESPONSE;
    }
    *out_session_id = (uint32_t)session_id->valuedouble;
    cJSON_Delete(response);
    return ESP_OK;
}

static esp_err_t claw_im_session_agent_id_call(const char *method,
                                               uint32_t session_id)
{
    cJSON *args = cJSON_CreateObject();

    if (!args ||
            !cJSON_AddNumberToObject(args,
                                    "session_id",
                                    (double)session_id)) {
        cJSON_Delete(args);
        return ESP_ERR_NO_MEM;
    }
    return claw_im_session_call_agent(method, args, NULL);
}

esp_err_t claw_im_session_publish_message(
    const char *source_cap,
    const char *channel,
    const char *chat_id,
    claw_agent_session_persistence_t persistence,
    const char *text,
    const char *sender_id,
    const char *message_id)
{
    claw_im_session_input_t input = {0};
    esp_err_t err;

    if (!source_cap || !channel || !chat_id || !text) {
        return ESP_ERR_INVALID_ARG;
    }
    if (claw_im_session_is_command(text)) {
        return claw_event_router_publish_message(source_cap,
                                                 channel,
                                                 chat_id,
                                                 text,
                                                 sender_id,
                                                 message_id);
    }
    err = claw_im_session_prepare_input(channel,
                                        chat_id,
                                        persistence,
                                        &input);
    if (err != ESP_OK) {
        return err;
    }
    return claw_event_router_publish_session_message(source_cap,
                                                     channel,
                                                     chat_id,
                                                     input.session_id,
                                                     input.request_id,
                                                     text,
                                                     sender_id,
                                                     message_id);
}

esp_err_t claw_im_session_get_selected(const char *channel,
                                       const char *chat_id,
                                       uint32_t *out_session_id)
{
    claw_im_session_cursor_t *cursor;
    esp_err_t err;

    if (!claw_im_session_key_valid(channel, chat_id) || !out_session_id) {
        return ESP_ERR_INVALID_ARG;
    }
    *out_session_id = 0;
    err = claw_im_session_lock();
    if (err != ESP_OK) {
        return err;
    }
    cursor = claw_im_session_find_locked(channel, chat_id);
    if (!cursor || cursor->session_id == 0) {
        claw_im_session_unlock();
        return ESP_ERR_NOT_FOUND;
    }
    *out_session_id = cursor->session_id;
    claw_im_session_unlock();
    return ESP_OK;
}

esp_err_t claw_im_session_prepare_input(
    const char *channel,
    const char *chat_id,
    claw_agent_session_persistence_t persistence,
    claw_im_session_input_t *out_input)
{
    claw_im_session_cursor_t *cursor;
    uint32_t session_id;
    esp_err_t err;

    if (!claw_im_session_key_valid(channel, chat_id) || !out_input) {
        return ESP_ERR_INVALID_ARG;
    }
    memset(out_input, 0, sizeof(*out_input));
    err = claw_im_session_lock();
    if (err != ESP_OK) {
        return err;
    }
    cursor = claw_im_session_find_locked(channel, chat_id);
    if (cursor && cursor->request_id != 0) {
        if (cursor->request_session_id == 0) {
            claw_im_session_unlock();
            return ESP_ERR_INVALID_STATE;
        }
        out_input->session_id = cursor->request_session_id;
        out_input->request_id = cursor->request_id;
        claw_im_session_unlock();
        return ESP_OK;
    }

    if (!cursor || cursor->session_id == 0) {
        claw_im_session_cursor_t *allocated = cursor;

        if (!allocated) {
            allocated = claw_im_session_allocate_locked();
        }
        if (!allocated) {
            claw_im_session_unlock();
            return ESP_ERR_NO_MEM;
        }
        err = claw_im_session_agent_create(persistence, &session_id);
        if (err != ESP_OK) {
            claw_im_session_unlock();
            return err;
        }
        err = claw_im_session_agent_id_call("session.open", session_id);
        if (err != ESP_OK) {
            esp_err_t cleanup_err = claw_im_session_agent_id_call(
                "session.delete",
                session_id);

            if (cleanup_err != ESP_OK) {
                ESP_LOGW(TAG,
                         "failed to delete unopened session=%" PRIu32 " err=%s",
                         session_id,
                         esp_err_to_name(cleanup_err));
            }
            claw_im_session_unlock();
            return err;
        }
        cursor = allocated;
        if (!cursor->occupied) {
            memset(cursor, 0, sizeof(*cursor));
            cursor->occupied = true;
            strlcpy(cursor->channel, channel, sizeof(cursor->channel));
            strlcpy(cursor->chat_id, chat_id, sizeof(cursor->chat_id));
        }
        cursor->session_id = session_id;
        cursor->open = true;
        ESP_LOGI(TAG,
                 "created session=%" PRIu32 " channel=%s chat=%s",
                 session_id,
                 channel,
                 chat_id);
    } else if (!cursor->open) {
        err = claw_im_session_agent_id_call("session.open", cursor->session_id);
        if (err != ESP_OK) {
            claw_im_session_unlock();
            return err;
        }
        cursor->open = true;
    }

    out_input->session_id = cursor->session_id;
    claw_im_session_unlock();
    return ESP_OK;
}

esp_err_t claw_im_session_select(const char *channel,
                                 const char *chat_id,
                                 uint32_t session_id)
{
    claw_im_session_cursor_t *cursor;
    esp_err_t err;

    if (!claw_im_session_key_valid(channel, chat_id) || session_id == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    err = claw_im_session_lock();
    if (err != ESP_OK) {
        return err;
    }
    cursor = claw_im_session_find_locked(channel, chat_id);
    if (!cursor) {
        cursor = claw_im_session_allocate_locked();
        if (!cursor) {
            claw_im_session_unlock();
            return ESP_ERR_NO_MEM;
        }
        memset(cursor, 0, sizeof(*cursor));
        cursor->occupied = true;
        strlcpy(cursor->channel, channel, sizeof(cursor->channel));
        strlcpy(cursor->chat_id, chat_id, sizeof(cursor->chat_id));
    }
    cursor->open = false;
    cursor->session_id = session_id;
    cursor->request_session_id = 0;
    cursor->request_id = 0;
    claw_im_session_unlock();
    return ESP_OK;
}

bool claw_im_session_is_managed(uint32_t session_id)
{
    bool managed = false;

    if (session_id == 0 || claw_im_session_lock() != ESP_OK) {
        return false;
    }
    for (size_t i = 0; i < CLAW_IM_SESSION_CURSOR_CAPACITY; i++) {
        const claw_im_session_cursor_t *cursor = &s_cursors[i];

        if (cursor->occupied &&
                (cursor->session_id == session_id ||
                 cursor->request_session_id == session_id)) {
            managed = true;
            break;
        }
    }
    claw_im_session_unlock();
    return managed;
}

esp_err_t claw_im_session_mark_open(uint32_t session_id)
{
    esp_err_t err;

    if (session_id == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    err = claw_im_session_lock();
    if (err != ESP_OK) {
        return err;
    }
    for (size_t i = 0; i < CLAW_IM_SESSION_CURSOR_CAPACITY; i++) {
        if (s_cursors[i].occupied && s_cursors[i].session_id == session_id) {
            s_cursors[i].open = true;
        }
    }
    claw_im_session_unlock();
    return ESP_OK;
}

esp_err_t claw_im_session_mark_closed(uint32_t session_id)
{
    esp_err_t err;

    if (session_id == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    err = claw_im_session_lock();
    if (err != ESP_OK) {
        return err;
    }
    for (size_t i = 0; i < CLAW_IM_SESSION_CURSOR_CAPACITY; i++) {
        claw_im_session_cursor_t *cursor = &s_cursors[i];

        if (!cursor->occupied) {
            continue;
        }
        if (cursor->session_id == session_id) {
            cursor->open = false;
        }
        if (cursor->request_session_id == session_id) {
            cursor->request_session_id = 0;
            cursor->request_id = 0;
        }
    }
    claw_im_session_unlock();
    return ESP_OK;
}

esp_err_t claw_im_session_forget(uint32_t session_id)
{
    esp_err_t err;

    if (session_id == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    err = claw_im_session_lock();
    if (err != ESP_OK) {
        return err;
    }
    for (size_t i = 0; i < CLAW_IM_SESSION_CURSOR_CAPACITY; i++) {
        claw_im_session_cursor_t *cursor = &s_cursors[i];

        if (!cursor->occupied) {
            continue;
        }
        if (cursor->session_id == session_id) {
            cursor->session_id = 0;
            cursor->open = false;
        }
        if (cursor->request_session_id == session_id) {
            cursor->request_session_id = 0;
            cursor->request_id = 0;
        }
        if (cursor->session_id == 0 && cursor->request_session_id == 0) {
            memset(cursor, 0, sizeof(*cursor));
        }
    }
    claw_im_session_unlock();
    return ESP_OK;
}

esp_err_t claw_im_session_note_input_request(const char *channel,
                                             const char *chat_id,
                                             uint32_t session_id,
                                             uint32_t request_id)
{
    claw_im_session_cursor_t *cursor;
    esp_err_t err;

    if (!claw_im_session_key_valid(channel, chat_id) ||
            session_id == 0 || request_id == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    err = claw_im_session_lock();
    if (err != ESP_OK) {
        return err;
    }
    cursor = claw_im_session_find_locked(channel, chat_id);
    if (!cursor) {
        cursor = claw_im_session_allocate_locked();
        if (!cursor) {
            claw_im_session_unlock();
            return ESP_ERR_NO_MEM;
        }
        memset(cursor, 0, sizeof(*cursor));
        cursor->occupied = true;
        strlcpy(cursor->channel, channel, sizeof(cursor->channel));
        strlcpy(cursor->chat_id, chat_id, sizeof(cursor->chat_id));
    }
    if (cursor->session_id == 0) {
        cursor->session_id = session_id;
        cursor->open = true;
    } else if (cursor->session_id == session_id) {
        cursor->open = true;
    }
    cursor->request_session_id = session_id;
    cursor->request_id = request_id;
    claw_im_session_unlock();
    return ESP_OK;
}

esp_err_t claw_im_session_clear_input_request(uint32_t session_id,
                                              uint32_t request_id)
{
    esp_err_t err;

    if (session_id == 0 || request_id == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    err = claw_im_session_lock();
    if (err != ESP_OK) {
        return err;
    }
    for (size_t i = 0; i < CLAW_IM_SESSION_CURSOR_CAPACITY; i++) {
        if (s_cursors[i].occupied &&
                s_cursors[i].request_session_id == session_id &&
                s_cursors[i].request_id == request_id) {
            s_cursors[i].request_session_id = 0;
            s_cursors[i].request_id = 0;
        }
    }
    claw_im_session_unlock();
    return ESP_OK;
}

esp_err_t claw_im_session_clear_session_input(uint32_t session_id)
{
    esp_err_t err;

    if (session_id == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    err = claw_im_session_lock();
    if (err != ESP_OK) {
        return err;
    }
    for (size_t i = 0; i < CLAW_IM_SESSION_CURSOR_CAPACITY; i++) {
        if (s_cursors[i].occupied &&
                s_cursors[i].request_session_id == session_id) {
            s_cursors[i].request_session_id = 0;
            s_cursors[i].request_id = 0;
        }
    }
    claw_im_session_unlock();
    return ESP_OK;
}
