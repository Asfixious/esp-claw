/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#pragma once

#include <stddef.h>

#include "cap_llm_inspect.h"

typedef enum {
    CAP_LLM_INSPECT_BACKEND_OPENAI_COMPATIBLE = 0,
    CAP_LLM_INSPECT_BACKEND_ANTHROPIC_COMPATIBLE = 1,
} cap_llm_inspect_backend_t;

struct cap_llm_inspect_runtime {
    char *api_key;
    char *model;
    char *base_url;
    char *auth_type;
    char *max_tokens_field;
    cap_llm_inspect_backend_t backend;
    uint32_t timeout_ms;
    uint32_t max_tokens;
    size_t image_max_bytes;
    bool supports_vision;
    bool image_remote_url_only;
};

typedef struct cap_llm_inspect_runtime *cap_llm_inspect_runtime_handle_t;

typedef struct {
    char *base64_data;
    char mime_type[16];
    size_t original_size;
} cap_llm_inspect_media_t;

typedef struct {
    const char *name;
    const char *value;
} cap_llm_inspect_http_header_t;

typedef struct {
    const char *url;
    const char *body;
    const char *api_key;
    const char *auth_type;
    uint32_t timeout_ms;
    const cap_llm_inspect_http_header_t *headers;
    size_t header_count;
} cap_llm_inspect_http_request_t;

typedef struct {
    char *body;
    int status_code;
} cap_llm_inspect_http_response_t;

esp_err_t cap_llm_inspect_runtime_create(const cap_llm_inspect_config_t *config,
                                         cap_llm_inspect_runtime_handle_t *ret_handle,
                                         char **out_error_message);
void cap_llm_inspect_runtime_delete(cap_llm_inspect_runtime_handle_t handle);
esp_err_t cap_llm_inspect_runtime_infer_image(cap_llm_inspect_runtime_handle_t handle,
                                              const char *path,
                                              const char *system_prompt,
                                              const char *user_prompt,
                                              char **out_text,
                                              char **out_error_message);

esp_err_t cap_llm_inspect_media_load(const char *path,
                                     size_t image_max_bytes,
                                     cap_llm_inspect_media_t *out_media,
                                     char **out_error_message);
void cap_llm_inspect_media_free(cap_llm_inspect_media_t *media);

esp_err_t cap_llm_inspect_http_post_json(const cap_llm_inspect_http_request_t *request,
                                         cap_llm_inspect_http_response_t *out_response,
                                         char **out_error_message);
void cap_llm_inspect_http_response_free(cap_llm_inspect_http_response_t *response);

esp_err_t cap_llm_inspect_openai_infer(const struct cap_llm_inspect_runtime *runtime,
                                       const cap_llm_inspect_media_t *media,
                                       const char *system_prompt,
                                       const char *user_prompt,
                                       char **out_text,
                                       char **out_error_message);
esp_err_t cap_llm_inspect_anthropic_infer(const struct cap_llm_inspect_runtime *runtime,
                                          const cap_llm_inspect_media_t *media,
                                          const char *system_prompt,
                                          const char *user_prompt,
                                          char **out_text,
                                          char **out_error_message);

char *cap_llm_inspect_format(const char *format, ...);
char *cap_llm_inspect_join_url(const char *base_url, const char *path);
