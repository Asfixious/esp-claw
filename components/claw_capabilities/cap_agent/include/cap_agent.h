/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#pragma once

#include "esp_err.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Register the system-only `agent` RPC capability. Requests use the strict
 * {"method":"...","args":{...}} envelope; Session/request IDs may come from
 * args or claw_cap_call_context_t. Opened Session streams are translated to
 * Event Router events by cap_agent. AgentSystem lifecycle remains
 * application-owned and must be initialized and started separately. */
esp_err_t cap_agent_register_group(void);

#ifdef __cplusplus
}
#endif
