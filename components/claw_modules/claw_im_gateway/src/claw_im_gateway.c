/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "claw_im_gateway.h"

#include <stdbool.h>
#include <stdio.h>
#include <string.h>

#include "cJSON.h"
#include "claw_cap.h"
#include "esp_check.h"
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"

static const char *TAG = "claw_im_gateway";

#define CLAW_IM_GATEWAY_MAX_PLATFORMS 8
#define CLAW_IM_GATEWAY_CHANNEL_SIZE  24
#define CLAW_IM_GATEWAY_GROUP_ID_SIZE 64

typedef struct {
    bool used;
    char channel[CLAW_IM_GATEWAY_CHANNEL_SIZE];
    char cap_group_id[CLAW_IM_GATEWAY_GROUP_ID_SIZE];
    claw_im_gateway_platform_ops_t ops;
    void *user_ctx;
} claw_im_gateway_platform_t;

typedef struct {
    SemaphoreHandle_t mutex;
    claw_im_gateway_platform_t platforms[CLAW_IM_GATEWAY_MAX_PLATFORMS];
    claw_im_gateway_inbound_handler_fn inbound_handler;
    void *inbound_user_ctx;
} claw_im_gateway_runtime_t;

static claw_im_gateway_runtime_t s_gateway;

static esp_err_t claw_im_gateway_ensure_mutex(void)
{
    if (s_gateway.mutex) {
        return ESP_OK;
    }

    s_gateway.mutex = xSemaphoreCreateMutex();
    return s_gateway.mutex ? ESP_OK : ESP_ERR_NO_MEM;
}

static esp_err_t claw_im_gateway_lock(void)
{
    esp_err_t err = claw_im_gateway_ensure_mutex();

    if (err != ESP_OK) {
        return err;
    }
    return xSemaphoreTake(s_gateway.mutex, portMAX_DELAY) == pdTRUE ?
           ESP_OK : ESP_ERR_TIMEOUT;
}

static void claw_im_gateway_unlock(void)
{
    xSemaphoreGive(s_gateway.mutex);
}

static const char *claw_im_gateway_json_string(cJSON *root, const char *key)
{
    cJSON *item = cJSON_GetObjectItem(root, key);

    return cJSON_IsString(item) && item->valuestring && item->valuestring[0] ?
           item->valuestring : NULL;
}

static const char *claw_im_gateway_resolve_channel(
    cJSON *root,
    const claw_cap_call_context_t *ctx)
{
    const char *value;

    if (ctx && ctx->target_channel && ctx->target_channel[0]) {
        return ctx->target_channel;
    }
    value = claw_im_gateway_json_string(root, "channel");
    if (value) {
        return value;
    }
    return ctx && ctx->channel && ctx->channel[0] ? ctx->channel : NULL;
}

static const char *claw_im_gateway_resolve_chat_id(
    cJSON *root,
    const claw_cap_call_context_t *ctx)
{
    const char *value;

    if (ctx && ctx->target_chat_id && ctx->target_chat_id[0]) {
        return ctx->target_chat_id;
    }
    value = claw_im_gateway_json_string(root, "chat_id");
    if (value) {
        return value;
    }
    return ctx && ctx->chat_id && ctx->chat_id[0] ? ctx->chat_id : NULL;
}

static esp_err_t claw_im_gateway_find_platform(
    const char *channel,
    claw_im_gateway_platform_t *out_platform)
{
    size_t i;
    esp_err_t err;

    if (!channel || !channel[0] || !out_platform) {
        return ESP_ERR_INVALID_ARG;
    }

    err = claw_im_gateway_lock();
    if (err != ESP_OK) {
        return err;
    }
    for (i = 0; i < CLAW_IM_GATEWAY_MAX_PLATFORMS; i++) {
        if (s_gateway.platforms[i].used &&
                strcmp(s_gateway.platforms[i].channel, channel) == 0) {
            *out_platform = s_gateway.platforms[i];
            claw_im_gateway_unlock();
            return ESP_OK;
        }
    }
    claw_im_gateway_unlock();
    return ESP_ERR_NOT_FOUND;
}

static esp_err_t claw_im_gateway_check_platform_state(
    const claw_im_gateway_platform_t *platform)
{
    claw_cap_state_t state;
    esp_err_t err;

    if (!platform || !platform->used) {
        return ESP_ERR_INVALID_ARG;
    }
    if (!platform->cap_group_id[0]) {
        return ESP_OK;
    }

    err = claw_cap_get_group_state(platform->cap_group_id, &state);
    if (err != ESP_OK) {
        return err;
    }
    return state == CLAW_CAP_STATE_STARTED ? ESP_OK : ESP_ERR_INVALID_STATE;
}

esp_err_t claw_im_gateway_register_platform(
    const claw_im_gateway_platform_config_t *config)
{
    claw_im_gateway_platform_t *slot = NULL;
    size_t i;
    esp_err_t err;

    if (!config || !config->channel || !config->channel[0] ||
            strlen(config->channel) >= CLAW_IM_GATEWAY_CHANNEL_SIZE ||
            !config->ops || !config->ops->send_message) {
        return ESP_ERR_INVALID_ARG;
    }
    if (config->cap_group_id &&
            strlen(config->cap_group_id) >= CLAW_IM_GATEWAY_GROUP_ID_SIZE) {
        return ESP_ERR_INVALID_ARG;
    }

    err = claw_im_gateway_lock();
    if (err != ESP_OK) {
        return err;
    }
    for (i = 0; i < CLAW_IM_GATEWAY_MAX_PLATFORMS; i++) {
        if (s_gateway.platforms[i].used &&
                strcmp(s_gateway.platforms[i].channel, config->channel) == 0) {
            slot = &s_gateway.platforms[i];
            break;
        }
        if (!slot && !s_gateway.platforms[i].used) {
            slot = &s_gateway.platforms[i];
        }
    }
    if (!slot) {
        claw_im_gateway_unlock();
        return ESP_ERR_NO_MEM;
    }

    memset(slot, 0, sizeof(*slot));
    slot->used = true;
    strlcpy(slot->channel, config->channel, sizeof(slot->channel));
    if (config->cap_group_id) {
        strlcpy(slot->cap_group_id,
                config->cap_group_id,
                sizeof(slot->cap_group_id));
    }
    slot->ops = *config->ops;
    slot->user_ctx = config->user_ctx;
    claw_im_gateway_unlock();

    ESP_LOGI(TAG, "Registered IM platform channel=%s group=%s",
             config->channel,
             config->cap_group_id ? config->cap_group_id : "-");
    return ESP_OK;
}

esp_err_t claw_im_gateway_unregister_platform(const char *channel)
{
    size_t i;
    esp_err_t err;

    if (!channel || !channel[0]) {
        return ESP_ERR_INVALID_ARG;
    }

    err = claw_im_gateway_lock();
    if (err != ESP_OK) {
        return err;
    }
    for (i = 0; i < CLAW_IM_GATEWAY_MAX_PLATFORMS; i++) {
        if (s_gateway.platforms[i].used &&
                strcmp(s_gateway.platforms[i].channel, channel) == 0) {
            memset(&s_gateway.platforms[i], 0, sizeof(s_gateway.platforms[i]));
            claw_im_gateway_unlock();
            return ESP_OK;
        }
    }
    claw_im_gateway_unlock();
    return ESP_ERR_NOT_FOUND;
}

esp_err_t claw_im_gateway_send_message(
    const claw_im_gateway_message_t *message)
{
    claw_im_gateway_platform_t platform = {0};
    esp_err_t err;

    if (!message || !message->channel || !message->channel[0] ||
            !message->chat_id || !message->chat_id[0] ||
            !message->message || !message->message[0]) {
        return ESP_ERR_INVALID_ARG;
    }

    err = claw_im_gateway_find_platform(message->channel, &platform);
    if (err != ESP_OK) {
        return err;
    }
    err = claw_im_gateway_check_platform_state(&platform);
    if (err != ESP_OK) {
        return err;
    }
    return platform.ops.send_message(message, platform.user_ctx);
}

static esp_err_t claw_im_gateway_send_media(
    const claw_im_gateway_media_t *media,
    bool image)
{
    claw_im_gateway_platform_t platform = {0};
    claw_im_gateway_send_media_fn send;
    esp_err_t err;

    if (!media || !media->channel || !media->channel[0] ||
            !media->chat_id || !media->chat_id[0] ||
            !media->path || !media->path[0]) {
        return ESP_ERR_INVALID_ARG;
    }

    err = claw_im_gateway_find_platform(media->channel, &platform);
    if (err != ESP_OK) {
        return err;
    }
    err = claw_im_gateway_check_platform_state(&platform);
    if (err != ESP_OK) {
        return err;
    }
    send = image ? platform.ops.send_image : platform.ops.send_file;
    if (!send) {
        return ESP_ERR_NOT_SUPPORTED;
    }
    return send(media, platform.user_ctx);
}

esp_err_t claw_im_gateway_send_image(const claw_im_gateway_media_t *media)
{
    return claw_im_gateway_send_media(media, true);
}

esp_err_t claw_im_gateway_send_file(const claw_im_gateway_media_t *media)
{
    return claw_im_gateway_send_media(media, false);
}

esp_err_t claw_im_gateway_set_inbound_handler(
    claw_im_gateway_inbound_handler_fn handler,
    void *user_ctx)
{
    esp_err_t err = claw_im_gateway_lock();

    if (err != ESP_OK) {
        return err;
    }
    s_gateway.inbound_handler = handler;
    s_gateway.inbound_user_ctx = user_ctx;
    claw_im_gateway_unlock();
    return ESP_OK;
}

esp_err_t claw_im_gateway_publish_inbound(
    const claw_im_gateway_inbound_event_t *event)
{
    claw_im_gateway_inbound_handler_fn handler;
    void *user_ctx;
    esp_err_t err;

    if (!event || !event->source_cap || !event->source_cap[0] ||
            !event->channel || !event->channel[0] ||
            !event->chat_id || !event->chat_id[0] ||
            !event->message_id || !event->message_id[0]) {
        return ESP_ERR_INVALID_ARG;
    }

    err = claw_im_gateway_lock();
    if (err != ESP_OK) {
        return err;
    }
    handler = s_gateway.inbound_handler;
    user_ctx = s_gateway.inbound_user_ctx;
    claw_im_gateway_unlock();

    if (!handler) {
        ESP_LOGW(TAG, "No inbound handler for channel=%s", event->channel);
        return ESP_ERR_INVALID_STATE;
    }
    return handler(event, user_ctx);
}

static esp_err_t claw_im_gateway_send_message_execute(
    const char *input_json,
    const claw_cap_call_context_t *ctx,
    char *output,
    size_t output_size)
{
    cJSON *root = cJSON_Parse(input_json ? input_json : "{}");
    claw_im_gateway_message_t message = {0};
    esp_err_t err;

    if (!cJSON_IsObject(root)) {
        cJSON_Delete(root);
        snprintf(output, output_size, "{\"ok\":false,\"error\":\"invalid json\"}");
        return ESP_ERR_INVALID_ARG;
    }

    message.channel = claw_im_gateway_resolve_channel(root, ctx);
    message.chat_id = claw_im_gateway_resolve_chat_id(root, ctx);
    message.message = claw_im_gateway_json_string(root, "message");
    message.message_id = claw_im_gateway_json_string(root, "message_id");
    message.event_type = claw_im_gateway_json_string(root, "event_type");
    message.link_url = claw_im_gateway_json_string(root, "link_url");
    message.link_label = claw_im_gateway_json_string(root, "link_label");

    err = claw_im_gateway_send_message(&message);
    if (err == ESP_OK) {
        snprintf(output, output_size, "{\"ok\":true}");
    } else {
        snprintf(output,
                 output_size,
                 "{\"ok\":false,\"error\":\"%s\"}",
                 esp_err_to_name(err));
    }
    cJSON_Delete(root);
    return err;
}

static esp_err_t claw_im_gateway_send_media_execute(
    const char *input_json,
    const claw_cap_call_context_t *ctx,
    char *output,
    size_t output_size,
    bool image)
{
    cJSON *root = cJSON_Parse(input_json ? input_json : "{}");
    claw_im_gateway_media_t media = {0};
    esp_err_t err;

    if (!cJSON_IsObject(root)) {
        cJSON_Delete(root);
        snprintf(output, output_size, "{\"ok\":false,\"error\":\"invalid json\"}");
        return ESP_ERR_INVALID_ARG;
    }

    media.channel = claw_im_gateway_resolve_channel(root, ctx);
    media.chat_id = claw_im_gateway_resolve_chat_id(root, ctx);
    media.path = claw_im_gateway_json_string(root, "path");
    media.caption = claw_im_gateway_json_string(root, "caption");

    err = image ? claw_im_gateway_send_image(&media) :
          claw_im_gateway_send_file(&media);
    if (err == ESP_OK) {
        snprintf(output, output_size, "{\"ok\":true}");
    } else {
        snprintf(output,
                 output_size,
                 "{\"ok\":false,\"error\":\"%s\"}",
                 esp_err_to_name(err));
    }
    cJSON_Delete(root);
    return err;
}

static esp_err_t claw_im_gateway_send_image_execute(
    const char *input_json,
    const claw_cap_call_context_t *ctx,
    char *output,
    size_t output_size)
{
    return claw_im_gateway_send_media_execute(input_json,
                                              ctx,
                                              output,
                                              output_size,
                                              true);
}

static esp_err_t claw_im_gateway_send_file_execute(
    const char *input_json,
    const claw_cap_call_context_t *ctx,
    char *output,
    size_t output_size)
{
    return claw_im_gateway_send_media_execute(input_json,
                                              ctx,
                                              output,
                                              output_size,
                                              false);
}

static const claw_cap_descriptor_t s_im_gateway_descriptors[] = {
    {
        .id = "send_message",
        .name = "send_message",
        .family = "im",
        .description = "Send a text message through the IM Gateway selected by channel and chat_id.",
        .kind = CLAW_CAP_KIND_CALLABLE,
        .cap_flags = CLAW_CAP_FLAG_CALLABLE_BY_LLM,
        .input_schema_json =
        "{\"type\":\"object\",\"properties\":{\"channel\":{\"type\":\"string\"},\"chat_id\":{\"type\":\"string\"},\"message\":{\"type\":\"string\"},\"message_id\":{\"type\":\"string\"},\"event_type\":{\"type\":\"string\"},\"link_url\":{\"type\":\"string\"},\"link_label\":{\"type\":\"string\"}},\"required\":[\"message\"]}",
        .execute = claw_im_gateway_send_message_execute,
    },
    {
        .id = "send_image",
        .name = "send_image",
        .family = "im",
        .description = "Send a local image through the IM Gateway selected by channel and chat_id.",
        .kind = CLAW_CAP_KIND_CALLABLE,
        .cap_flags = CLAW_CAP_FLAG_CALLABLE_BY_LLM,
        .input_schema_json =
        "{\"type\":\"object\",\"properties\":{\"channel\":{\"type\":\"string\"},\"chat_id\":{\"type\":\"string\"},\"path\":{\"type\":\"string\"},\"caption\":{\"type\":\"string\"}},\"required\":[\"path\"]}",
        .execute = claw_im_gateway_send_image_execute,
    },
    {
        .id = "send_file",
        .name = "send_file",
        .family = "im",
        .description = "Send a local file through the IM Gateway selected by channel and chat_id.",
        .kind = CLAW_CAP_KIND_CALLABLE,
        .cap_flags = CLAW_CAP_FLAG_CALLABLE_BY_LLM,
        .input_schema_json =
        "{\"type\":\"object\",\"properties\":{\"channel\":{\"type\":\"string\"},\"chat_id\":{\"type\":\"string\"},\"path\":{\"type\":\"string\"},\"caption\":{\"type\":\"string\"}},\"required\":[\"path\"]}",
        .execute = claw_im_gateway_send_file_execute,
    },
};

static const claw_cap_group_t s_im_gateway_group = {
    .group_id = "cap_im_gateway",
    .descriptors = s_im_gateway_descriptors,
    .descriptor_count = sizeof(s_im_gateway_descriptors) /
                        sizeof(s_im_gateway_descriptors[0]),
};

esp_err_t claw_im_gateway_register_group(void)
{
    ESP_RETURN_ON_ERROR(claw_im_gateway_ensure_mutex(),
                        TAG,
                        "Failed to initialize IM Gateway");
    if (claw_cap_group_exists(s_im_gateway_group.group_id)) {
        return ESP_OK;
    }
    return claw_cap_register_group(&s_im_gateway_group);
}
