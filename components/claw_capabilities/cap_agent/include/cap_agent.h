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

/* Register AgentSystem as a system-only claw_cap entry. Normal input must carry
 * the numeric session selected by the IM layer; raw messages are reserved for
 * /session commands. Explicit callers may choose one session lifecycle, input,
 * or control operation. AgentSystem must be initialized and started
 * separately. */
esp_err_t cap_agent_register_group(void);

#ifdef __cplusplus
}
#endif
