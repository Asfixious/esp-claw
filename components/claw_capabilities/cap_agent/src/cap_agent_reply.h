/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "esp_err.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    const char *channel;
    const char *chat_id;
    const char *correlation_id;
} cap_agent_reply_route_t;

bool cap_agent_reply_route_supported(const char *channel, const char *chat_id);
bool cap_agent_reply_is_attached(uint32_t session_id);

/* Attach exactly one long-lived event pump to a stream opened by cap_agent or
 * the shared IM session layer. This must not adopt another C API consumer's
 * already-open stream. */
esp_err_t cap_agent_reply_ensure(uint32_t session_id);

/* Submit through the Rust session actor, then attach the route only when that
 * actor accepts the new turn. The actor remains the sole busy/concurrency
 * authority; this function only closes the event/route race. */
esp_err_t cap_agent_reply_submit(uint32_t session_id,
                                 const char *text,
                                 const cap_agent_reply_route_t *route);

#ifdef __cplusplus
}
#endif
