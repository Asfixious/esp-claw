/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
/*
 * C shim bridging the Rust `log` facade to ESP-IDF's logging. ESP_LOGx are
 * macros, so the Rust backend (claw_sys::log_backend) calls this function which
 * forwards to esp_log_write at the requested level.
 */
#include "esp_log.h"

void claw_rs_log(int level, const char *tag, const char *msg)
{
    esp_log_write((esp_log_level_t)level, tag, "%s\n", msg);
}
