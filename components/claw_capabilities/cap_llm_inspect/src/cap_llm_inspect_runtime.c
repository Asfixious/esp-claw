/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "cap_llm_inspect_internal.h"

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define CAP_LLM_INSPECT_DEFAULT_TIMEOUT_MS       (120U * 1000U)
#define CAP_LLM_INSPECT_DEFAULT_MAX_TOKENS       8192U
#define CAP_LLM_INSPECT_DEFAULT_IMAGE_MAX_BYTES  (512U * 1024U)

static bool string_is_empty(const char *value)
{
    return !value || value[0] == '\0';
}

char *cap_llm_inspect_format(const char *format, ...)
{
    va_list args;
    va_list copy;
    char *buffer = NULL;
    int required;

    if (!format) {
        return NULL;
    }

    va_start(args, format);
    va_copy(copy, args);
    required = vsnprintf(NULL, 0, format, copy);
    va_end(copy);
    if (required < 0) {
        va_end(args);
        return NULL;
    }

    buffer = calloc(1, (size_t)required + 1);
    if (buffer) {
        vsnprintf(buffer, (size_t)required + 1, format, args);
    }
    va_end(args);
    return buffer;
}

char *cap_llm_inspect_join_url(const char *base_url, const char *path)
{
    bool base_has_slash;
    bool path_has_slash;

    if (string_is_empty(base_url) || string_is_empty(path)) {
        return NULL;
    }

    base_has_slash = base_url[strlen(base_url) - 1] == '/';
    path_has_slash = path[0] == '/';
    if (base_has_slash && path_has_slash) {
        return cap_llm_inspect_format("%s%s", base_url, path + 1);
    }
    if (!base_has_slash && !path_has_slash) {
        return cap_llm_inspect_format("%s/%s", base_url, path);
    }
    return cap_llm_inspect_format("%s%s", base_url, path);
}

static esp_err_t parse_backend(const char *backend_type,
                               cap_llm_inspect_backend_t *out_backend,
                               char **out_error_message)
{
    if (strcmp(backend_type, "openai_compatible") == 0) {
        *out_backend = CAP_LLM_INSPECT_BACKEND_OPENAI_COMPATIBLE;
        return ESP_OK;
    }
    if (strcmp(backend_type, "anthropic_compatible") == 0) {
        *out_backend = CAP_LLM_INSPECT_BACKEND_ANTHROPIC_COMPATIBLE;
        return ESP_OK;
    }

    *out_error_message = cap_llm_inspect_format("Unsupported LLM backend: %s", backend_type);
    return ESP_ERR_NOT_SUPPORTED;
}

esp_err_t cap_llm_inspect_runtime_create(const cap_llm_inspect_config_t *config,
                                         cap_llm_inspect_runtime_handle_t *ret_handle,
                                         char **out_error_message)
{
    cap_llm_inspect_runtime_handle_t runtime = NULL;
    esp_err_t err;

    if (ret_handle) {
        *ret_handle = NULL;
    }
    if (out_error_message) {
        *out_error_message = NULL;
    }
    if (!config || !ret_handle || !out_error_message) {
        return ESP_ERR_INVALID_ARG;
    }
    if (string_is_empty(config->api_key) ||
            string_is_empty(config->backend_type) ||
            string_is_empty(config->model) ||
            string_is_empty(config->base_url)) {
        *out_error_message = strdup("LLM API key, backend, model, and base URL are required");
        return ESP_ERR_INVALID_ARG;
    }

    runtime = calloc(1, sizeof(*runtime));
    if (!runtime) {
        *out_error_message = strdup("Out of memory allocating image inference runtime");
        return ESP_ERR_NO_MEM;
    }

    err = parse_backend(config->backend_type, &runtime->backend, out_error_message);
    if (err != ESP_OK) {
        free(runtime);
        return err;
    }

    runtime->api_key = strdup(config->api_key);
    runtime->model = strdup(config->model);
    runtime->base_url = strdup(config->base_url);
    runtime->auth_type = strdup(string_is_empty(config->auth_type) ? "bearer" : config->auth_type);
    runtime->max_tokens_field = strdup(string_is_empty(config->max_tokens_field) ?
                                       "max_tokens" : config->max_tokens_field);
    runtime->timeout_ms = config->timeout_ms ?
                          config->timeout_ms : CAP_LLM_INSPECT_DEFAULT_TIMEOUT_MS;
    runtime->max_tokens = config->max_tokens ?
                          config->max_tokens : CAP_LLM_INSPECT_DEFAULT_MAX_TOKENS;
    runtime->image_max_bytes = config->image_max_bytes ?
                               config->image_max_bytes : CAP_LLM_INSPECT_DEFAULT_IMAGE_MAX_BYTES;
    runtime->supports_vision = config->supports_vision;
    runtime->image_remote_url_only = config->image_remote_url_only;
    if (!runtime->api_key || !runtime->model || !runtime->base_url ||
            !runtime->auth_type || !runtime->max_tokens_field) {
        cap_llm_inspect_runtime_delete(runtime);
        *out_error_message = strdup("Out of memory copying image inference configuration");
        return ESP_ERR_NO_MEM;
    }

    *ret_handle = runtime;
    return ESP_OK;
}

void cap_llm_inspect_runtime_delete(cap_llm_inspect_runtime_handle_t handle)
{
    if (!handle) {
        return;
    }

    free(handle->api_key);
    free(handle->model);
    free(handle->base_url);
    free(handle->auth_type);
    free(handle->max_tokens_field);
    free(handle);
}

esp_err_t cap_llm_inspect_runtime_infer_image(cap_llm_inspect_runtime_handle_t handle,
                                              const char *path,
                                              const char *system_prompt,
                                              const char *user_prompt,
                                              char **out_text,
                                              char **out_error_message)
{
    cap_llm_inspect_media_t media = {0};
    esp_err_t err;

    if (out_text) {
        *out_text = NULL;
    }
    if (out_error_message) {
        *out_error_message = NULL;
    }
    if (!handle || string_is_empty(path) || string_is_empty(user_prompt) ||
            !out_text || !out_error_message) {
        return ESP_ERR_INVALID_ARG;
    }
    if (!handle->supports_vision) {
        *out_error_message = strdup("Selected LLM profile does not support image inference");
        return ESP_ERR_NOT_SUPPORTED;
    }
    if (handle->image_remote_url_only) {
        *out_error_message = strdup("Selected LLM profile does not accept local images");
        return ESP_ERR_NOT_SUPPORTED;
    }

    err = cap_llm_inspect_media_load(path,
                                     handle->image_max_bytes,
                                     &media,
                                     out_error_message);
    if (err != ESP_OK) {
        return err;
    }

    switch (handle->backend) {
    case CAP_LLM_INSPECT_BACKEND_OPENAI_COMPATIBLE:
        err = cap_llm_inspect_openai_infer(handle,
                                           &media,
                                           system_prompt,
                                           user_prompt,
                                           out_text,
                                           out_error_message);
        break;
    case CAP_LLM_INSPECT_BACKEND_ANTHROPIC_COMPATIBLE:
        err = cap_llm_inspect_anthropic_infer(handle,
                                              &media,
                                              system_prompt,
                                              user_prompt,
                                              out_text,
                                              out_error_message);
        break;
    default:
        *out_error_message = strdup("Unsupported image inference backend");
        err = ESP_ERR_NOT_SUPPORTED;
        break;
    }

    cap_llm_inspect_media_free(&media);
    return err;
}
