/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "esp_err.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    const char *api_key;
    const char *backend_type;
    const char *model;
    const char *base_url;
    const char *auth_type;
    const char *max_tokens_field;
    uint32_t timeout_ms;
    uint32_t max_tokens;
    size_t image_max_bytes;
    bool supports_vision;
    bool image_remote_url_only;
} cap_llm_inspect_config_t;

/**
 * @brief Configure the standalone image inference client used by this group.
 *
 * Passing an all-empty API configuration leaves the capability registered but
 * unconfigured. The capability owns a copy of every configured string.
 */
esp_err_t cap_llm_inspect_configure(const cap_llm_inspect_config_t *config);
esp_err_t cap_llm_inspect_register_group(void);

#ifdef __cplusplus
}
#endif
