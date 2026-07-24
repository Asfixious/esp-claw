/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "cap_llm_inspect.h"
#include "cap_llm_inspect_internal.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "cJSON.h"
#include "claw_cap.h"
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"

static const char *CAP_LLM_INSPECT_SYSTEM_PROMPT =
    "You analyze local image files for the ESP32 claw. "
    "Describe visible content plainly and briefly. "
    "If the image is unclear, say what is uncertain instead of guessing.";
static const char *TAG = "cap_llm_inspect";

static SemaphoreHandle_t s_runtime_lock;
static portMUX_TYPE s_runtime_lock_init_mux = portMUX_INITIALIZER_UNLOCKED;
static cap_llm_inspect_runtime_handle_t s_runtime;

static bool string_is_empty(const char *value)
{
    return !value || value[0] == '\0';
}

static bool config_is_unbound(const cap_llm_inspect_config_t *config)
{
    return string_is_empty(config->api_key) &&
           string_is_empty(config->backend_type) &&
           string_is_empty(config->model) &&
           string_is_empty(config->base_url);
}

static esp_err_t ensure_runtime_lock(void)
{
    SemaphoreHandle_t new_lock = NULL;

    if (s_runtime_lock) {
        return ESP_OK;
    }

    new_lock = xSemaphoreCreateMutex();
    if (!new_lock) {
        return ESP_ERR_NO_MEM;
    }

    portENTER_CRITICAL(&s_runtime_lock_init_mux);
    if (!s_runtime_lock) {
        s_runtime_lock = new_lock;
        new_lock = NULL;
    }
    portEXIT_CRITICAL(&s_runtime_lock_init_mux);
    if (new_lock) {
        vSemaphoreDelete(new_lock);
    }
    return ESP_OK;
}

esp_err_t cap_llm_inspect_configure(const cap_llm_inspect_config_t *config)
{
    cap_llm_inspect_runtime_handle_t replacement = NULL;
    cap_llm_inspect_runtime_handle_t previous = NULL;
    char *error_message = NULL;
    esp_err_t err;

    if (!config) {
        return ESP_ERR_INVALID_ARG;
    }
    if (!config_is_unbound(config)) {
        err = cap_llm_inspect_runtime_create(config, &replacement, &error_message);
        if (err != ESP_OK) {
            ESP_LOGE(TAG, "Failed to configure image inference: %s",
                     error_message ? error_message : esp_err_to_name(err));
            free(error_message);
            return err;
        }
    }

    err = ensure_runtime_lock();
    if (err != ESP_OK) {
        cap_llm_inspect_runtime_delete(replacement);
        return err;
    }

    xSemaphoreTake(s_runtime_lock, portMAX_DELAY);
    previous = s_runtime;
    s_runtime = replacement;
    xSemaphoreGive(s_runtime_lock);
    cap_llm_inspect_runtime_delete(previous);

    ESP_LOGI(TAG, "Standalone image inference %s",
             replacement ? "configured" : "left unconfigured");
    return ESP_OK;
}

static esp_err_t cap_llm_inspect_execute(const char *input_json,
                                         const claw_cap_call_context_t *ctx,
                                         char *output,
                                         size_t output_size)
{
    cJSON *root = NULL;
    cJSON *path_json = NULL;
    cJSON *prompt_json = NULL;
    char *analysis = NULL;
    char *error_message = NULL;
    esp_err_t err;

    if (!input_json || !output || output_size == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    (void)ctx;

    root = cJSON_Parse(input_json);
    if (!root) {
        snprintf(output, output_size, "Error: input must be a JSON object");
        return ESP_ERR_INVALID_ARG;
    }

    path_json = cJSON_GetObjectItem(root, "path");
    prompt_json = cJSON_GetObjectItem(root, "prompt");
    if (!cJSON_IsString(path_json) || !path_json->valuestring[0] ||
            !cJSON_IsString(prompt_json) || !prompt_json->valuestring[0]) {
        cJSON_Delete(root);
        snprintf(output, output_size, "Error: path and prompt are required");
        return ESP_ERR_INVALID_ARG;
    }

    err = ensure_runtime_lock();
    if (err != ESP_OK) {
        cJSON_Delete(root);
        snprintf(output, output_size, "Error: image inference lock unavailable");
        return err;
    }

    xSemaphoreTake(s_runtime_lock, portMAX_DELAY);
    if (!s_runtime) {
        err = ESP_ERR_INVALID_STATE;
        error_message = strdup("LLM image inference is not configured");
    } else {
        err = cap_llm_inspect_runtime_infer_image(s_runtime,
                                                  path_json->valuestring,
                                                  CAP_LLM_INSPECT_SYSTEM_PROMPT,
                                                  prompt_json->valuestring,
                                                  &analysis,
                                                  &error_message);
    }
    xSemaphoreGive(s_runtime_lock);
    cJSON_Delete(root);
    if (err != ESP_OK) {
        snprintf(output,
                 output_size,
                 "Error: image analysis failed (%s)%s%s",
                 esp_err_to_name(err),
                 error_message ? ": " : "",
                 error_message ? error_message : "");
        free(error_message);
        return err;
    }

    snprintf(output, output_size, "%s", analysis ? analysis : "");
    free(analysis);
    free(error_message);
    return ESP_OK;
}

static const claw_cap_descriptor_t s_llm_inspect_descriptors[] = {
    {
        .id = "inspect_image",
        .name = "inspect_image",
        .family = "system",
        .description =
        "Analyze a local image from an absolute path. Confirm the path first, then provide a prompt describing what to inspect.",
        .kind = CLAW_CAP_KIND_CALLABLE,
        .cap_flags = CLAW_CAP_FLAG_CALLABLE_BY_LLM,
        .input_schema_json =
        "{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"},\"prompt\":{\"type\":\"string\"}},\"required\":[\"path\",\"prompt\"]}",
        .execute = cap_llm_inspect_execute,
    },
};

static const claw_cap_group_t s_llm_inspect_group = {
    .group_id = "cap_llm_inspect",
    .descriptors = s_llm_inspect_descriptors,
    .descriptor_count = sizeof(s_llm_inspect_descriptors) / sizeof(s_llm_inspect_descriptors[0]),
};

esp_err_t cap_llm_inspect_register_group(void)
{
    if (claw_cap_group_exists(s_llm_inspect_group.group_id)) {
        return ESP_OK;
    }

    return claw_cap_register_group(&s_llm_inspect_group);
}
