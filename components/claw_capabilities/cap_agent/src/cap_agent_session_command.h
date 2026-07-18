/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#pragma once

#include <stdbool.h>
#include <stddef.h>

#include "claw_cap.h"
#include "esp_err.h"

#ifdef __cplusplus
extern "C" {
#endif

bool cap_agent_session_command_matches(const char *message);

esp_err_t cap_agent_session_command_execute_message(
    const char *message,
    const claw_cap_call_context_t *ctx,
    char *output,
    size_t output_size);

#ifdef __cplusplus
}
#endif
