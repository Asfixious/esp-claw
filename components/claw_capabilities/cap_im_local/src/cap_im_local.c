/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "cap_im_local.h"

#include <inttypes.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "claw_cap.h"
#include "claw_im_gateway.h"
#include "esp_check.h"
#include "esp_err.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"

static const char *TAG = "cap_im_local";

#define CAP_IM_LOCAL_DEFAULT_CHANNEL "local"
#define CAP_IM_LOCAL_DEFAULT_SENDER_ID "local_user"
#define CAP_IM_LOCAL_MESSAGE_ID_SIZE 96
#define CAP_IM_LOCAL_LINK_URL_STACK    256
#define CAP_IM_LOCAL_LINK_LABEL_STACK  128
#define CAP_IM_LOCAL_EMIT_COMBINED_MAX 8192

typedef struct {
    SemaphoreHandle_t lock;
    bool started;
    bool log_outbound_messages;
    char default_channel[16];
    char default_sender_id[96];
    cap_im_local_outbound_callback_t outbound_callback;
    void *outbound_callback_user_ctx;
} cap_im_local_state_t;

static cap_im_local_state_t s_local = {
    .log_outbound_messages = true,
    .default_channel = CAP_IM_LOCAL_DEFAULT_CHANNEL,
    .default_sender_id = CAP_IM_LOCAL_DEFAULT_SENDER_ID,
};

static esp_err_t cap_im_local_lock(void)
{
    if (!s_local.lock) {
        s_local.lock = xSemaphoreCreateMutex();
        if (!s_local.lock) {
            ESP_LOGE(TAG, "Failed to create local IM lock");
            return ESP_ERR_NO_MEM;
        }
    }

    return xSemaphoreTake(s_local.lock, pdMS_TO_TICKS(1000)) == pdTRUE ? ESP_OK : ESP_ERR_TIMEOUT;
}

static void cap_im_local_unlock(void)
{
    if (s_local.lock) {
        xSemaphoreGive(s_local.lock);
    }
}

static int64_t cap_im_local_now_ms(void)
{
    return esp_timer_get_time() / 1000LL;
}

static void cap_im_local_build_message_id(char *buf, size_t buf_size, const char *prefix)
{
    if (!buf || buf_size == 0) {
        return;
    }

    snprintf(buf,
             buf_size,
             "%s-%" PRId64,
             prefix ? prefix : "local",
             cap_im_local_now_ms());
}

static esp_err_t cap_im_local_gateway_init(void)
{
    esp_err_t err = cap_im_local_lock();

    if (err != ESP_OK) {
        return err;
    }

    cap_im_local_unlock();
    return ESP_OK;
}

static esp_err_t cap_im_local_gateway_start(void)
{
    esp_err_t err = cap_im_local_lock();

    if (err != ESP_OK) {
        return err;
    }

    s_local.started = true;
    cap_im_local_unlock();
    return ESP_OK;
}

static esp_err_t cap_im_local_gateway_stop(void)
{
    esp_err_t err = cap_im_local_lock();

    if (err != ESP_OK) {
        return err;
    }

    s_local.started = false;
    cap_im_local_unlock();
    return ESP_OK;
}

static esp_err_t cap_im_local_send_text_impl(const char *channel,
                                             const char *chat_id,
                                             const char *message_id,
                                             const char *text,
                                             const char *link_url,
                                             const char *link_label)
{
    cap_im_local_message_t message = {0};
    cap_im_local_outbound_callback_t callback = NULL;
    void *callback_user_ctx = NULL;
    char message_id_buf[CAP_IM_LOCAL_MESSAGE_ID_SIZE];
    char link_url_stack[CAP_IM_LOCAL_LINK_URL_STACK];
    char link_label_stack[CAP_IM_LOCAL_LINK_LABEL_STACK];
    esp_err_t err;

    if (!chat_id || !chat_id[0] || !text || !text[0]) {
        return ESP_ERR_INVALID_ARG;
    }

    err = cap_im_local_lock();
    if (err != ESP_OK) {
        return err;
    }

    if (!s_local.started) {
        cap_im_local_unlock();
        return ESP_ERR_INVALID_STATE;
    }

    callback = s_local.outbound_callback;
    callback_user_ctx = s_local.outbound_callback_user_ctx;

    message.channel = (channel && channel[0]) ? channel : s_local.default_channel;
    message.chat_id = chat_id;
    message.text = text;
    message.timestamp_ms = cap_im_local_now_ms();
    if (message_id && message_id[0]) {
        message.message_id = message_id;
    } else {
        cap_im_local_build_message_id(message_id_buf, sizeof(message_id_buf), "local-out");
        message.message_id = message_id_buf;
    }

    if (link_url && link_url[0]) {
        strlcpy(link_url_stack, link_url, sizeof(link_url_stack));
        message.link_url = link_url_stack;
    }
    if (link_label && link_label[0]) {
        strlcpy(link_label_stack, link_label, sizeof(link_label_stack));
        message.link_label = link_label_stack;
    }

    if (s_local.log_outbound_messages) {
        ESP_LOGI(TAG, "Outbound local IM channel=%s chat=%s msg=%s text=%.80s%s",
                 message.channel,
                 message.chat_id,
                 message.message_id,
                 text,
                 strlen(text) > 80 ? "..." : "");
    }

    cap_im_local_unlock();

    if (!callback) {
        ESP_LOGW(TAG, "No outbound callback registered for local IM");
        return ESP_ERR_INVALID_STATE;
    }

    return callback(&message, callback_user_ctx);
}

static esp_err_t cap_im_local_gateway_send_message(
    const claw_im_gateway_message_t *message,
    void *user_ctx)
{
    (void)user_ctx;
    return cap_im_local_send_text_with_link(message->channel,
                                            message->chat_id,
                                            message->message_id,
                                            message->message,
                                            message->link_url,
                                            message->link_label);
}

static const claw_im_gateway_platform_ops_t s_local_gateway_ops = {
    .send_message = cap_im_local_gateway_send_message,
};

static const claw_cap_descriptor_t s_local_descriptors[] = {
    {
        .id = "local_gateway",
        .name = "local_gateway",
        .family = "im",
        .description = "Local in-process IM gateway event source.",
        .kind = CLAW_CAP_KIND_EVENT_SOURCE,
        .cap_flags = CLAW_CAP_FLAG_EMITS_EVENTS |
        CLAW_CAP_FLAG_SUPPORTS_LIFECYCLE,
        .input_schema_json = "{\"type\":\"object\",\"properties\":{}}",
        .init = cap_im_local_gateway_init,
        .start = cap_im_local_gateway_start,
        .stop = cap_im_local_gateway_stop,
    },
};

static const claw_cap_group_t s_local_group = {
    .group_id = "cap_im_local",
    .descriptors = s_local_descriptors,
    .descriptor_count = sizeof(s_local_descriptors) / sizeof(s_local_descriptors[0]),
};

esp_err_t cap_im_local_register_group(void)
{
    esp_err_t err;

    ESP_RETURN_ON_ERROR(claw_im_gateway_register_group(),
                        TAG,
                        "register IM Gateway group failed");
    if (!claw_cap_group_exists(s_local_group.group_id)) {
        err = claw_cap_register_group(&s_local_group);
        if (err != ESP_OK) {
            return err;
        }
    }
    ESP_RETURN_ON_ERROR(claw_im_gateway_register_platform(
                            &(claw_im_gateway_platform_config_t) {
        .channel = "web",
        .cap_group_id = s_local_group.group_id,
        .ops = &s_local_gateway_ops,
    }), TAG, "register Web IM backend failed");
    return claw_im_gateway_register_platform(
               &(claw_im_gateway_platform_config_t) {
        .channel = "local",
        .cap_group_id = s_local_group.group_id,
        .ops = &s_local_gateway_ops,
    });
}

esp_err_t cap_im_local_set_config(const cap_im_local_config_t *config)
{
    esp_err_t err;

    if (!config) {
        return ESP_ERR_INVALID_ARG;
    }

    err = cap_im_local_lock();
    if (err != ESP_OK) {
        return err;
    }

    if (config->default_channel && config->default_channel[0]) {
        strlcpy(s_local.default_channel, config->default_channel, sizeof(s_local.default_channel));
    }
    if (config->default_sender_id && config->default_sender_id[0]) {
        strlcpy(s_local.default_sender_id, config->default_sender_id, sizeof(s_local.default_sender_id));
    }
    s_local.log_outbound_messages = config->log_outbound_messages;

    cap_im_local_unlock();
    return ESP_OK;
}

esp_err_t cap_im_local_set_outbound_callback(cap_im_local_outbound_callback_t callback,
                                             void *user_ctx)
{
    esp_err_t err = cap_im_local_lock();

    if (err != ESP_OK) {
        return err;
    }

    s_local.outbound_callback = callback;
    s_local.outbound_callback_user_ctx = user_ctx;
    cap_im_local_unlock();
    return ESP_OK;
}

esp_err_t cap_im_local_start(void)
{
    return cap_im_local_gateway_start();
}

esp_err_t cap_im_local_stop(void)
{
    return cap_im_local_gateway_stop();
}

esp_err_t cap_im_local_send_text(const char *channel,
                                 const char *chat_id,
                                 const char *message_id,
                                 const char *text)
{
    return cap_im_local_send_text_impl(channel, chat_id, message_id, text, NULL, NULL);
}

esp_err_t cap_im_local_send_text_with_link(const char *channel,
                                           const char *chat_id,
                                           const char *message_id,
                                           const char *text,
                                           const char *link_url,
                                           const char *link_label)
{
    return cap_im_local_send_text_impl(channel, chat_id, message_id, text, link_url, link_label);
}

static const char *cap_im_local_basename(const char *path)
{
    const char *slash = strrchr(path, '/');

    return slash && slash[1] ? slash + 1 : path;
}

esp_err_t cap_im_local_emit_user_message(const char *channel,
                                         const char *chat_id,
                                         const char *sender_id,
                                         const char *message_id,
                                         const char *text,
                                         const char *const *link_urls,
                                         size_t link_count)
{
    char message_id_buf[CAP_IM_LOCAL_MESSAGE_ID_SIZE];
    const char *resolved_channel;
    const char *resolved_sender_id;
    const char *resolved_message_id;
    char *combined = NULL;
    esp_err_t err;
    bool has_text = text && text[0];

    if (!chat_id || !chat_id[0]) {
        return ESP_ERR_INVALID_ARG;
    }
    if (!has_text && link_count == 0) {
        return ESP_ERR_INVALID_ARG;
    }

    err = cap_im_local_lock();
    if (err != ESP_OK) {
        return err;
    }

    if (!s_local.started) {
        cap_im_local_unlock();
        return ESP_ERR_INVALID_STATE;
    }

    resolved_channel = (channel && channel[0]) ? channel : s_local.default_channel;
    resolved_sender_id = (sender_id && sender_id[0]) ? sender_id : s_local.default_sender_id;
    if (message_id && message_id[0]) {
        resolved_message_id = message_id;
    } else {
        cap_im_local_build_message_id(message_id_buf, sizeof(message_id_buf), "local-in");
        resolved_message_id = message_id_buf;
    }

    cap_im_local_unlock();

    if (link_count == 0) {
        if (!has_text) {
            return ESP_ERR_INVALID_ARG;
        }
        return claw_im_gateway_publish_inbound(&(claw_im_gateway_inbound_event_t) {
            .source_cap = "local_gateway",
            .channel = resolved_channel,
            .chat_id = chat_id,
            .sender_id = resolved_sender_id,
            .message_id = resolved_message_id,
            .event_type = "message",
            .content_type = "text",
            .text = text,
            .timestamp_ms = cap_im_local_now_ms(),
        });
    }

    combined = calloc(1, CAP_IM_LOCAL_EMIT_COMBINED_MAX);
    if (!combined) {
        return ESP_ERR_NO_MEM;
    }

    if (has_text) {
        strlcpy(combined, text, CAP_IM_LOCAL_EMIT_COMBINED_MAX);
    } else {
        strlcpy(combined, "(attachments)", CAP_IM_LOCAL_EMIT_COMBINED_MAX);
    }

    if (link_count > 0) {
        strlcat(combined, "\n\n[Links]\n", CAP_IM_LOCAL_EMIT_COMBINED_MAX);
        for (size_t i = 0; i < link_count; i++) {
            const char *u = link_urls[i];

            if (!u || !u[0]) {
                continue;
            }
            strlcat(combined, "- [", CAP_IM_LOCAL_EMIT_COMBINED_MAX);
            strlcat(combined, cap_im_local_basename(u), CAP_IM_LOCAL_EMIT_COMBINED_MAX);
            strlcat(combined, "](", CAP_IM_LOCAL_EMIT_COMBINED_MAX);
            strlcat(combined, u, CAP_IM_LOCAL_EMIT_COMBINED_MAX);
            strlcat(combined, ")\n", CAP_IM_LOCAL_EMIT_COMBINED_MAX);
        }
    }

    if (!combined[0]) {
        free(combined);
        return ESP_ERR_INVALID_ARG;
    }

    ESP_LOGI(TAG, "Inbound local IM channel=%s chat=%s sender=%s msg=%s text=%.80s%s",
             resolved_channel,
             chat_id,
             resolved_sender_id,
             resolved_message_id,
             combined,
             strlen(combined) > 80 ? "..." : "");

    err = claw_im_gateway_publish_inbound(&(claw_im_gateway_inbound_event_t) {
        .source_cap = "local_gateway",
        .channel = resolved_channel,
        .chat_id = chat_id,
        .sender_id = resolved_sender_id,
        .message_id = resolved_message_id,
        .event_type = "message",
        .content_type = "text",
        .text = combined,
        .timestamp_ms = cap_im_local_now_ms(),
    });
    free(combined);
    return err;
}

esp_err_t cap_im_local_emit_text(const char *channel,
                                 const char *chat_id,
                                 const char *sender_id,
                                 const char *message_id,
                                 const char *text)
{
    return cap_im_local_emit_user_message(channel, chat_id, sender_id, message_id, text, NULL, 0);
}
