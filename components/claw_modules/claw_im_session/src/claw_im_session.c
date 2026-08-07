/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "claw_im_session.h"

#include <inttypes.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>

#include "cJSON.h"
#include "claw_cap.h"
#include "claw_event_publisher.h"
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/portmacro.h"
#include "freertos/semphr.h"
#include "nvs.h"

static const char *TAG = "claw_im_session";

#define CLAW_IM_SESSION_CURSOR_CAPACITY 32
#define CLAW_IM_SESSION_CHANNEL_SIZE    32
#define CLAW_IM_SESSION_CHAT_ID_SIZE    96
#define CLAW_IM_SESSION_RPC_OUTPUT_SIZE 256
#define CLAW_IM_SESSION_BINDING_MAGIC   UINT32_C(0x43494D42)
#define CLAW_IM_SESSION_BINDING_VERSION 1

static const char *CLAW_IM_SESSION_NVS_NAMESPACE = "claw_im";
static const char *CLAW_IM_SESSION_NVS_KEY = "bindings";

typedef struct {
    bool occupied;
    bool open;
    char channel[CLAW_IM_SESSION_CHANNEL_SIZE];
    char chat_id[CLAW_IM_SESSION_CHAT_ID_SIZE];
    uint32_t session_id;
    uint32_t request_session_id;
    uint32_t request_id;
} claw_im_session_cursor_t;

typedef struct {
    uint32_t magic;
    uint16_t version;
    uint16_t count;
} claw_im_session_binding_header_t;

typedef struct {
    uint32_t session_id;
    char channel[CLAW_IM_SESSION_CHANNEL_SIZE];
    char chat_id[CLAW_IM_SESSION_CHAT_ID_SIZE];
} claw_im_session_binding_record_t;

_Static_assert(sizeof(claw_im_session_binding_header_t) == 8,
               "IM session binding header layout changed");
_Static_assert(sizeof(claw_im_session_binding_record_t) == 132,
               "IM session binding record layout changed");

static claw_im_session_cursor_t s_cursors[CLAW_IM_SESSION_CURSOR_CAPACITY];
static SemaphoreHandle_t s_cursor_mutex;
static portMUX_TYPE s_cursor_mutex_init_lock = portMUX_INITIALIZER_UNLOCKED;
static bool s_bindings_loaded;
static bool s_bindings_dirty;

static bool claw_im_session_key_valid(const char *channel, const char *chat_id)
{
    return channel && channel[0] &&
           strlen(channel) < CLAW_IM_SESSION_CHANNEL_SIZE &&
           chat_id && chat_id[0] &&
           strlen(chat_id) < CLAW_IM_SESSION_CHAT_ID_SIZE;
}

/* Must be called with s_cursor_mutex held. */
static esp_err_t claw_im_session_load_bindings_locked(void)
{
    claw_im_session_binding_header_t *header;
    claw_im_session_binding_record_t *records;
    nvs_handle_t handle;
    void *blob = NULL;
    size_t blob_size = 0;
    size_t expected_size;
    esp_err_t err;

    if (s_bindings_loaded) {
        return ESP_OK;
    }

    err = nvs_open(CLAW_IM_SESSION_NVS_NAMESPACE, NVS_READONLY, &handle);
    if (err == ESP_ERR_NVS_NOT_FOUND) {
        s_bindings_loaded = true;
        return ESP_OK;
    }
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "binding store open failed: %s", esp_err_to_name(err));
        return err;
    }

    err = nvs_get_blob(handle, CLAW_IM_SESSION_NVS_KEY, NULL, &blob_size);
    if (err == ESP_ERR_NVS_NOT_FOUND) {
        nvs_close(handle);
        s_bindings_loaded = true;
        return ESP_OK;
    }
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "binding store size read failed: %s", esp_err_to_name(err));
        nvs_close(handle);
        return err;
    }
    if (blob_size < sizeof(*header) ||
            blob_size > sizeof(*header) +
            CLAW_IM_SESSION_CURSOR_CAPACITY * sizeof(*records)) {
        ESP_LOGE(TAG, "binding store has invalid size=%u", (unsigned)blob_size);
        nvs_close(handle);
        return ESP_ERR_INVALID_SIZE;
    }

    blob = malloc(blob_size);
    if (!blob) {
        nvs_close(handle);
        return ESP_ERR_NO_MEM;
    }
    err = nvs_get_blob(handle, CLAW_IM_SESSION_NVS_KEY, blob, &blob_size);
    nvs_close(handle);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "binding store read failed: %s", esp_err_to_name(err));
        free(blob);
        return err;
    }

    header = blob;
    if (header->magic != CLAW_IM_SESSION_BINDING_MAGIC ||
            header->version != CLAW_IM_SESSION_BINDING_VERSION ||
            header->count > CLAW_IM_SESSION_CURSOR_CAPACITY) {
        ESP_LOGE(TAG,
                 "binding store header invalid: magic=%" PRIx32 " version=%u count=%u",
                 header->magic,
                 (unsigned)header->version,
                 (unsigned)header->count);
        free(blob);
        return ESP_ERR_INVALID_STATE;
    }
    expected_size = sizeof(*header) + header->count * sizeof(*records);
    if (blob_size != expected_size) {
        ESP_LOGE(TAG,
                 "binding store length mismatch: actual=%u expected=%u",
                 (unsigned)blob_size,
                 (unsigned)expected_size);
        free(blob);
        return ESP_ERR_INVALID_SIZE;
    }

    records = (claw_im_session_binding_record_t *)(header + 1);
    memset(s_cursors, 0, sizeof(s_cursors));
    for (size_t i = 0; i < header->count; i++) {
        const claw_im_session_binding_record_t *record = &records[i];

        if (record->session_id == 0 ||
                !memchr(record->channel, '\0', sizeof(record->channel)) ||
                !memchr(record->chat_id, '\0', sizeof(record->chat_id)) ||
                !claw_im_session_key_valid(record->channel, record->chat_id)) {
            ESP_LOGE(TAG, "binding store record %u is invalid", (unsigned)i);
            memset(s_cursors, 0, sizeof(s_cursors));
            free(blob);
            return ESP_ERR_INVALID_STATE;
        }
        for (size_t j = 0; j < i; j++) {
            if (strcmp(s_cursors[j].channel, record->channel) == 0 &&
                    strcmp(s_cursors[j].chat_id, record->chat_id) == 0) {
                ESP_LOGE(TAG, "binding store record %u is duplicated", (unsigned)i);
                memset(s_cursors, 0, sizeof(s_cursors));
                free(blob);
                return ESP_ERR_INVALID_STATE;
            }
        }
        s_cursors[i].occupied = true;
        s_cursors[i].session_id = record->session_id;
        strlcpy(s_cursors[i].channel,
                record->channel,
                sizeof(s_cursors[i].channel));
        strlcpy(s_cursors[i].chat_id,
                record->chat_id,
                sizeof(s_cursors[i].chat_id));
    }

    ESP_LOGI(TAG, "restored %u persistent IM session bindings", (unsigned)header->count);
    free(blob);
    s_bindings_loaded = true;
    return ESP_OK;
}

/* Must be called with s_cursor_mutex held. */
static esp_err_t claw_im_session_save_bindings_locked(void)
{
    claw_im_session_binding_header_t *header;
    claw_im_session_binding_record_t *records;
    nvs_handle_t handle;
    void *blob;
    size_t count = 0;
    size_t blob_size;
    bool nvs_opened = false;
    esp_err_t err;

    for (size_t i = 0; i < CLAW_IM_SESSION_CURSOR_CAPACITY; i++) {
        if (s_cursors[i].occupied && s_cursors[i].session_id != 0) {
            count++;
        }
    }
    blob_size = sizeof(*header) + count * sizeof(*records);
    blob = calloc(1, blob_size);
    if (!blob) {
        return ESP_ERR_NO_MEM;
    }

    header = blob;
    header->magic = CLAW_IM_SESSION_BINDING_MAGIC;
    header->version = CLAW_IM_SESSION_BINDING_VERSION;
    header->count = (uint16_t)count;
    records = (claw_im_session_binding_record_t *)(header + 1);
    count = 0;
    for (size_t i = 0; i < CLAW_IM_SESSION_CURSOR_CAPACITY; i++) {
        const claw_im_session_cursor_t *cursor = &s_cursors[i];

        if (!cursor->occupied || cursor->session_id == 0) {
            continue;
        }
        records[count].session_id = cursor->session_id;
        strlcpy(records[count].channel,
                cursor->channel,
                sizeof(records[count].channel));
        strlcpy(records[count].chat_id,
                cursor->chat_id,
                sizeof(records[count].chat_id));
        count++;
    }

    err = nvs_open(CLAW_IM_SESSION_NVS_NAMESPACE, NVS_READWRITE, &handle);
    if (err == ESP_OK) {
        nvs_opened = true;
        err = nvs_set_blob(handle, CLAW_IM_SESSION_NVS_KEY, blob, blob_size);
    }
    if (err == ESP_OK) {
        err = nvs_commit(handle);
    }
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "binding store write failed: %s", esp_err_to_name(err));
    }
    if (nvs_opened) {
        nvs_close(handle);
    }
    free(blob);
    if (err == ESP_OK) {
        s_bindings_dirty = false;
    }
    return err;
}

/* Must be called with s_cursor_mutex held. */
static esp_err_t claw_im_session_persist_bindings_locked(void)
{
    s_bindings_dirty = true;
    return claw_im_session_save_bindings_locked();
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
    err = claw_im_session_load_bindings_locked();
    if (err == ESP_OK && s_bindings_dirty) {
        err = claw_im_session_save_bindings_locked();
    }
    if (err != ESP_OK) {
        xSemaphoreGive(s_cursor_mutex);
    }
    return err;
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

static const char *claw_im_session_xml_entity(char value)
{
    switch (value) {
    case '&':
        return "&amp;";
    case '<':
        return "&lt;";
    case '>':
        return "&gt;";
    case '"':
        return "&quot;";
    case '\'':
        return "&apos;";
    default:
        return NULL;
    }
}

static bool claw_im_session_xml_escaped_size(const char *value,
                                             size_t *out_size)
{
    size_t size = 0;

    if (!value || !out_size) {
        return false;
    }
    while (*value) {
        const char *entity = claw_im_session_xml_entity(*value++);
        size_t part_size = entity ? strlen(entity) : 1;

        if (part_size > SIZE_MAX - size) {
            return false;
        }
        size += part_size;
    }
    *out_size = size;
    return true;
}

static char *claw_im_session_xml_escape_into(char *out, const char *value)
{
    while (*value) {
        const char *entity = claw_im_session_xml_entity(*value++);

        if (entity) {
            size_t entity_size = strlen(entity);

            memcpy(out, entity, entity_size);
            out += entity_size;
        } else {
            *out++ = value[-1];
        }
    }
    return out;
}

static bool claw_im_session_add_size(size_t *total, size_t value)
{
    if (value > SIZE_MAX - *total) {
        return false;
    }
    *total += value;
    return true;
}

static esp_err_t claw_im_session_wrap_message(const char *channel,
                                              const char *chat_id,
                                              const char *text,
                                              char **out_message)
{
    static const char open[] = "<message channel=\"";
    static const char chat_id_open[] = "\" chat_id=\"";
    static const char body_open[] = "\">\n";
    static const char close[] = "\n</message>";
    size_t channel_size;
    size_t chat_id_size;
    size_t text_size;
    size_t total_size = 1;
    char *message;
    char *cursor;

    if (!channel || !chat_id || !text || !out_message) {
        return ESP_ERR_INVALID_ARG;
    }
    *out_message = NULL;
    if (!claw_im_session_xml_escaped_size(channel, &channel_size) ||
            !claw_im_session_xml_escaped_size(chat_id, &chat_id_size) ||
            !claw_im_session_xml_escaped_size(text, &text_size) ||
            !claw_im_session_add_size(&total_size, sizeof(open) - 1) ||
            !claw_im_session_add_size(&total_size, channel_size) ||
            !claw_im_session_add_size(&total_size,
                                      sizeof(chat_id_open) - 1) ||
            !claw_im_session_add_size(&total_size, chat_id_size) ||
            !claw_im_session_add_size(&total_size, sizeof(body_open) - 1) ||
            !claw_im_session_add_size(&total_size, text_size) ||
            !claw_im_session_add_size(&total_size, sizeof(close) - 1)) {
        return ESP_ERR_INVALID_SIZE;
    }

    message = malloc(total_size);
    if (!message) {
        return ESP_ERR_NO_MEM;
    }
    cursor = message;
    memcpy(cursor, open, sizeof(open) - 1);
    cursor += sizeof(open) - 1;
    cursor = claw_im_session_xml_escape_into(cursor, channel);
    memcpy(cursor, chat_id_open, sizeof(chat_id_open) - 1);
    cursor += sizeof(chat_id_open) - 1;
    cursor = claw_im_session_xml_escape_into(cursor, chat_id);
    memcpy(cursor, body_open, sizeof(body_open) - 1);
    cursor += sizeof(body_open) - 1;
    cursor = claw_im_session_xml_escape_into(cursor, text);
    memcpy(cursor, close, sizeof(close) - 1);
    cursor += sizeof(close) - 1;
    *cursor = '\0';
    *out_message = message;
    return ESP_OK;
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
    char *agent_message = NULL;
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
    err = claw_im_session_wrap_message(channel,
                                       chat_id,
                                       text,
                                       &agent_message);
    if (err != ESP_OK) {
        return err;
    }
    err = claw_im_session_prepare_input(channel,
                                        chat_id,
                                        persistence,
                                        &input);
    if (err != ESP_OK) {
        free(agent_message);
        return err;
    }
    err = claw_event_router_publish_session_message(source_cap,
                                                    channel,
                                                    chat_id,
                                                    input.session_id,
                                                    input.request_id,
                                                    agent_message,
                                                    sender_id,
                                                    message_id);
    free(agent_message);
    return err;
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

    if (cursor && cursor->session_id != 0 && !cursor->open) {
        err = claw_im_session_agent_id_call("session.open", cursor->session_id);
        if (err == ESP_OK) {
            cursor->open = true;
        } else if (err == ESP_ERR_NOT_FOUND) {
            ESP_LOGW(TAG,
                     "discarding stale binding session=%" PRIu32 " channel=%s chat=%s",
                     cursor->session_id,
                     channel,
                     chat_id);
            cursor->session_id = 0;
            cursor->open = false;
            err = claw_im_session_persist_bindings_locked();
            if (err != ESP_OK) {
                claw_im_session_unlock();
                return err;
            }
        } else {
            claw_im_session_unlock();
            return err;
        }
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
        err = claw_im_session_persist_bindings_locked();
        if (err != ESP_OK) {
            claw_im_session_unlock();
            return err;
        }
        ESP_LOGI(TAG,
                 "created session=%" PRIu32 " channel=%s chat=%s",
                 session_id,
                 channel,
                 chat_id);
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
    err = claw_im_session_persist_bindings_locked();
    claw_im_session_unlock();
    return err;
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
    bool binding_changed = false;
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
            binding_changed = true;
        }
        if (cursor->request_session_id == session_id) {
            cursor->request_session_id = 0;
            cursor->request_id = 0;
        }
        if (cursor->session_id == 0 && cursor->request_session_id == 0) {
            memset(cursor, 0, sizeof(*cursor));
        }
    }
    if (binding_changed) {
        err = claw_im_session_persist_bindings_locked();
    }
    claw_im_session_unlock();
    return err;
}

esp_err_t claw_im_session_note_input_request(const char *channel,
                                             const char *chat_id,
                                             uint32_t session_id,
                                             uint32_t request_id)
{
    bool binding_changed = false;
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
        binding_changed = true;
    } else if (cursor->session_id == session_id) {
        cursor->open = true;
    }
    cursor->request_session_id = session_id;
    cursor->request_id = request_id;
    if (binding_changed) {
        err = claw_im_session_persist_bindings_locked();
    }
    claw_im_session_unlock();
    return err;
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
