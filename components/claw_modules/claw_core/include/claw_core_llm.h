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

#include "claw_core.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Source of a media asset handed to the agent core for inference. */
typedef enum {
    CLAW_MEDIA_ASSET_KIND_LOCAL_PATH = 0,
    CLAW_MEDIA_ASSET_KIND_REMOTE_URL = 1,
    CLAW_MEDIA_ASSET_KIND_INLINE_BYTES = 2,
} claw_media_asset_kind_t;

typedef struct {
    claw_media_asset_kind_t kind;
    const char *path;
    const char *url;
    const uint8_t *bytes;
    size_t byte_count;
    const char *mime_type;
} claw_media_asset_t;

typedef struct {
    const char *system_prompt;
    const char *user_prompt;
    const claw_media_asset_t *media;
    size_t media_count;
} claw_llm_media_request_t;

/*
 * Run multimodal (media) inference on the agent core's configured LLM.
 *
 * On success returns ESP_OK and sets *out_text to a newly allocated, NUL
 * terminated analysis string (the caller frees it with free()). On failure
 * returns the error code and may set *out_error_message to a newly allocated
 * detail string (also freed with free()).
 */
esp_err_t claw_core_llm_infer_media(claw_core_handle_t core,
                                    const claw_llm_media_request_t *request,
                                    char **out_text,
                                    char **out_error_message);

#ifdef __cplusplus
}
#endif
