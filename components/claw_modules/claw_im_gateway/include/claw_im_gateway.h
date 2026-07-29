/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#pragma once

#include <stddef.h>
#include <stdint.h>

#include "esp_err.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    const char *channel;
    const char *chat_id;
    const char *message;
    const char *message_id;
    const char *event_type;
    const char *link_url;
    const char *link_label;
} claw_im_gateway_message_t;

typedef struct {
    const char *channel;
    const char *chat_id;
    const char *path;
    const char *caption;
} claw_im_gateway_media_t;

typedef esp_err_t (*claw_im_gateway_send_message_fn)(
    const claw_im_gateway_message_t *message,
    void *user_ctx);
typedef esp_err_t (*claw_im_gateway_send_media_fn)(
    const claw_im_gateway_media_t *media,
    void *user_ctx);

typedef struct {
    claw_im_gateway_send_message_fn send_message;
    claw_im_gateway_send_media_fn send_image;
    claw_im_gateway_send_media_fn send_file;
} claw_im_gateway_platform_ops_t;

typedef struct {
    const char *channel;
    const char *cap_group_id;
    const claw_im_gateway_platform_ops_t *ops;
    void *user_ctx;
} claw_im_gateway_platform_config_t;

typedef struct {
    const char *source_cap;
    const char *channel;
    const char *chat_id;
    const char *sender_id;
    const char *message_id;
    const char *event_type;
    const char *content_type;
    const char *text;
    const char *payload_json;
    int64_t timestamp_ms;
} claw_im_gateway_inbound_event_t;

typedef esp_err_t (*claw_im_gateway_inbound_handler_fn)(
    const claw_im_gateway_inbound_event_t *event,
    void *user_ctx);

/**
 * Register the one public IM capability group.
 *
 * The group exports send_message, send_image and send_file. Platform
 * implementations are selected at call time using channel from the capability
 * call context (or from the payload when explicitly provided).
 */
esp_err_t claw_im_gateway_register_group(void);

/**
 * Register or replace one private platform backend identified by channel.
 *
 * Platform components call this during their own registration. They must not
 * register platform-prefixed public send capabilities.
 */
esp_err_t claw_im_gateway_register_platform(
    const claw_im_gateway_platform_config_t *config);
esp_err_t claw_im_gateway_unregister_platform(const char *channel);

esp_err_t claw_im_gateway_send_message(
    const claw_im_gateway_message_t *message);
esp_err_t claw_im_gateway_send_image(const claw_im_gateway_media_t *media);
esp_err_t claw_im_gateway_send_file(const claw_im_gateway_media_t *media);

/**
 * Configure the application-owned ingress consumer.
 *
 * The Gateway normalizes platform input but deliberately does not select
 * sessions or run the Agent. The registered consumer owns that policy.
 */
esp_err_t claw_im_gateway_set_inbound_handler(
    claw_im_gateway_inbound_handler_fn handler,
    void *user_ctx);
esp_err_t claw_im_gateway_publish_inbound(
    const claw_im_gateway_inbound_event_t *event);

#ifdef __cplusplus
}
#endif
