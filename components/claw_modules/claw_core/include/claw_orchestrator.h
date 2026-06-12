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
    const char *message_id;
    const char *channel;
    const char *chat_id;
    const char *sender_id;
    const char *session_id;
    const char *text;
} claw_orchestrator_user_message_t;

esp_err_t claw_orchestrator_init(void *cap_user_ctx);
esp_err_t claw_orchestrator_push_user_message(const claw_orchestrator_user_message_t *msg);
esp_err_t claw_orchestrator_session_create(char *out_session_id, size_t out_len);
esp_err_t claw_orchestrator_session_delete(const char *session_id);
size_t claw_orchestrator_session_count(void);
esp_err_t claw_orchestrator_tick(void);

#ifdef __cplusplus
}
#endif
