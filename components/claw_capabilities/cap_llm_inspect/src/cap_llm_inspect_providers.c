/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "cap_llm_inspect_internal.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "cJSON.h"

#define CAP_LLM_INSPECT_OPENAI_PATH       "/chat/completions"
#define CAP_LLM_INSPECT_ANTHROPIC_PATH    "/messages"
#define CAP_LLM_INSPECT_ANTHROPIC_VERSION "2023-06-01"

static esp_err_t json_oom(char **out_error_message)
{
    if (out_error_message && !*out_error_message) {
        *out_error_message = strdup("Out of memory building image request");
    }
    return ESP_ERR_NO_MEM;
}

static bool add_string(cJSON *object, const char *name, const char *value)
{
    return cJSON_AddStringToObject(object, name, value ? value : "") != NULL;
}

static bool add_number(cJSON *object, const char *name, double value)
{
    return cJSON_AddNumberToObject(object, name, value) != NULL;
}

static esp_err_t build_openai_body(const struct cap_llm_inspect_runtime *runtime,
                                   const cap_llm_inspect_media_t *media,
                                   const char *system_prompt,
                                   const char *user_prompt,
                                   char **out_body,
                                   char **out_error_message)
{
    cJSON *body = NULL;
    cJSON *messages = NULL;
    cJSON *system_message = NULL;
    cJSON *user_message = NULL;
    cJSON *content = NULL;
    cJSON *text_block = NULL;
    cJSON *image_block = NULL;
    cJSON *image_value = NULL;
    char *data_url = NULL;
    size_t data_url_size;
    esp_err_t err = ESP_ERR_NO_MEM;

    if (strlen(media->base64_data) >
            SIZE_MAX - strlen("data:;base64,") - strlen(media->mime_type) - 1) {
        *out_error_message = strdup("Image payload is too large");
        return ESP_ERR_INVALID_SIZE;
    }
    data_url_size = strlen("data:;base64,") +
                    strlen(media->mime_type) +
                    strlen(media->base64_data) + 1;
    data_url = malloc(data_url_size);
    if (!data_url) {
        return json_oom(out_error_message);
    }
    snprintf(data_url, data_url_size, "data:%s;base64,%s",
             media->mime_type, media->base64_data);

    body = cJSON_CreateObject();
    messages = cJSON_CreateArray();
    system_message = cJSON_CreateObject();
    user_message = cJSON_CreateObject();
    content = cJSON_CreateArray();
    text_block = cJSON_CreateObject();
    image_block = cJSON_CreateObject();
    image_value = cJSON_CreateObject();
    if (!body || !messages || !system_message || !user_message || !content ||
            !text_block || !image_block || !image_value) {
        goto cleanup;
    }

    if (!add_string(body, "model", runtime->model) ||
            !add_number(body, runtime->max_tokens_field, runtime->max_tokens) ||
            !add_string(system_message, "role", "system") ||
            !add_string(system_message, "content", system_prompt) ||
            !cJSON_AddItemToArray(messages, system_message)) {
        goto cleanup;
    }
    system_message = NULL;

    if (!add_string(user_message, "role", "user") ||
            !add_string(text_block, "type", "text") ||
            !add_string(text_block, "text", user_prompt) ||
            !cJSON_AddItemToArray(content, text_block)) {
        goto cleanup;
    }
    text_block = NULL;

    if (!add_string(image_block, "type", "image_url") ||
            !add_string(image_value, "url", data_url) ||
            !cJSON_AddItemToObject(image_block, "image_url", image_value)) {
        goto cleanup;
    }
    image_value = NULL;
    if (!cJSON_AddItemToArray(content, image_block)) {
        goto cleanup;
    }
    image_block = NULL;
    if (!cJSON_AddItemToObject(user_message, "content", content)) {
        goto cleanup;
    }
    content = NULL;
    if (!cJSON_AddItemToArray(messages, user_message)) {
        goto cleanup;
    }
    user_message = NULL;
    if (!cJSON_AddItemToObject(body, "messages", messages)) {
        goto cleanup;
    }
    messages = NULL;

    *out_body = cJSON_PrintUnformatted(body);
    if (!*out_body) {
        goto cleanup;
    }
    err = ESP_OK;

cleanup:
    free(data_url);
    cJSON_Delete(body);
    cJSON_Delete(messages);
    cJSON_Delete(system_message);
    cJSON_Delete(user_message);
    cJSON_Delete(content);
    cJSON_Delete(text_block);
    cJSON_Delete(image_block);
    cJSON_Delete(image_value);
    if (err != ESP_OK) {
        json_oom(out_error_message);
    }
    return err;
}

static esp_err_t parse_openai_text(const char *response_body,
                                   char **out_text,
                                   char **out_error_message)
{
    cJSON *root = NULL;
    cJSON *choices = NULL;
    cJSON *choice = NULL;
    cJSON *message = NULL;
    cJSON *content = NULL;
    esp_err_t err = ESP_FAIL;

    root = cJSON_Parse(response_body);
    if (!root) {
        *out_error_message = strdup("Failed to parse OpenAI-compatible response");
        return ESP_FAIL;
    }

    choices = cJSON_GetObjectItemCaseSensitive(root, "choices");
    choice = cJSON_IsArray(choices) ? cJSON_GetArrayItem(choices, 0) : NULL;
    message = cJSON_IsObject(choice) ?
              cJSON_GetObjectItemCaseSensitive(choice, "message") : NULL;
    content = cJSON_IsObject(message) ?
              cJSON_GetObjectItemCaseSensitive(message, "content") : NULL;
    if (!cJSON_IsString(content) || !content->valuestring || !content->valuestring[0]) {
        *out_error_message = strdup("OpenAI-compatible response contains no text");
        goto cleanup;
    }

    *out_text = strdup(content->valuestring);
    if (!*out_text) {
        err = json_oom(out_error_message);
        goto cleanup;
    }
    err = ESP_OK;

cleanup:
    cJSON_Delete(root);
    return err;
}

esp_err_t cap_llm_inspect_openai_infer(const struct cap_llm_inspect_runtime *runtime,
                                       const cap_llm_inspect_media_t *media,
                                       const char *system_prompt,
                                       const char *user_prompt,
                                       char **out_text,
                                       char **out_error_message)
{
    cap_llm_inspect_http_request_t request = {0};
    cap_llm_inspect_http_response_t response = {0};
    char *url = NULL;
    char *body = NULL;
    esp_err_t err;

    err = build_openai_body(runtime,
                            media,
                            system_prompt,
                            user_prompt,
                            &body,
                            out_error_message);
    if (err != ESP_OK) {
        return err;
    }

    url = cap_llm_inspect_join_url(runtime->base_url, CAP_LLM_INSPECT_OPENAI_PATH);
    if (!url) {
        free(body);
        return json_oom(out_error_message);
    }

    request.url = url;
    request.body = body;
    request.api_key = runtime->api_key;
    request.auth_type = runtime->auth_type;
    request.timeout_ms = runtime->timeout_ms;
    err = cap_llm_inspect_http_post_json(&request, &response, out_error_message);
    if (err == ESP_OK) {
        err = parse_openai_text(response.body, out_text, out_error_message);
    }

    cap_llm_inspect_http_response_free(&response);
    free(url);
    free(body);
    return err;
}

static esp_err_t build_anthropic_body(const struct cap_llm_inspect_runtime *runtime,
                                      const cap_llm_inspect_media_t *media,
                                      const char *system_prompt,
                                      const char *user_prompt,
                                      char **out_body,
                                      char **out_error_message)
{
    cJSON *body = NULL;
    cJSON *messages = NULL;
    cJSON *user_message = NULL;
    cJSON *content = NULL;
    cJSON *text_block = NULL;
    cJSON *image_block = NULL;
    cJSON *source = NULL;
    esp_err_t err = ESP_ERR_NO_MEM;

    body = cJSON_CreateObject();
    messages = cJSON_CreateArray();
    user_message = cJSON_CreateObject();
    content = cJSON_CreateArray();
    text_block = cJSON_CreateObject();
    image_block = cJSON_CreateObject();
    source = cJSON_CreateObject();
    if (!body || !messages || !user_message || !content ||
            !text_block || !image_block || !source) {
        goto cleanup;
    }

    if (!add_string(body, "model", runtime->model) ||
            !add_number(body, "max_tokens", runtime->max_tokens) ||
            !add_string(body, "system", system_prompt) ||
            !add_string(user_message, "role", "user") ||
            !add_string(text_block, "type", "text") ||
            !add_string(text_block, "text", user_prompt) ||
            !cJSON_AddItemToArray(content, text_block)) {
        goto cleanup;
    }
    text_block = NULL;

    if (!add_string(image_block, "type", "image") ||
            !add_string(source, "type", "base64") ||
            !add_string(source, "media_type", media->mime_type) ||
            !add_string(source, "data", media->base64_data) ||
            !cJSON_AddItemToObject(image_block, "source", source)) {
        goto cleanup;
    }
    source = NULL;
    if (!cJSON_AddItemToArray(content, image_block)) {
        goto cleanup;
    }
    image_block = NULL;
    if (!cJSON_AddItemToObject(user_message, "content", content)) {
        goto cleanup;
    }
    content = NULL;
    if (!cJSON_AddItemToArray(messages, user_message)) {
        goto cleanup;
    }
    user_message = NULL;
    if (!cJSON_AddItemToObject(body, "messages", messages)) {
        goto cleanup;
    }
    messages = NULL;

    *out_body = cJSON_PrintUnformatted(body);
    if (!*out_body) {
        goto cleanup;
    }
    err = ESP_OK;

cleanup:
    cJSON_Delete(body);
    cJSON_Delete(messages);
    cJSON_Delete(user_message);
    cJSON_Delete(content);
    cJSON_Delete(text_block);
    cJSON_Delete(image_block);
    cJSON_Delete(source);
    if (err != ESP_OK) {
        json_oom(out_error_message);
    }
    return err;
}

static esp_err_t parse_anthropic_text(const char *response_body,
                                      char **out_text,
                                      char **out_error_message)
{
    cJSON *root = NULL;
    cJSON *content = NULL;
    cJSON *block = NULL;
    size_t text_size = 0;
    size_t offset = 0;

    root = cJSON_Parse(response_body);
    if (!root) {
        *out_error_message = strdup("Failed to parse Anthropic-compatible response");
        return ESP_FAIL;
    }

    content = cJSON_GetObjectItemCaseSensitive(root, "content");
    if (!cJSON_IsArray(content)) {
        cJSON_Delete(root);
        *out_error_message = strdup("Anthropic-compatible response contains no content");
        return ESP_FAIL;
    }

    cJSON_ArrayForEach(block, content) {
        cJSON *type = cJSON_GetObjectItemCaseSensitive(block, "type");
        cJSON *text = cJSON_GetObjectItemCaseSensitive(block, "text");
        size_t block_size;

        if (!cJSON_IsString(type) || !type->valuestring ||
                strcmp(type->valuestring, "text") != 0 ||
                !cJSON_IsString(text) || !text->valuestring) {
            continue;
        }
        block_size = strlen(text->valuestring);
        if (block_size > SIZE_MAX - text_size - 1) {
            cJSON_Delete(root);
            *out_error_message = strdup("Anthropic-compatible response is too large");
            return ESP_ERR_INVALID_SIZE;
        }
        text_size += block_size;
    }
    if (text_size == 0) {
        cJSON_Delete(root);
        *out_error_message = strdup("Anthropic-compatible response contains no text");
        return ESP_FAIL;
    }

    *out_text = calloc(1, text_size + 1);
    if (!*out_text) {
        cJSON_Delete(root);
        return json_oom(out_error_message);
    }

    cJSON_ArrayForEach(block, content) {
        cJSON *type = cJSON_GetObjectItemCaseSensitive(block, "type");
        cJSON *text = cJSON_GetObjectItemCaseSensitive(block, "text");
        size_t block_size;

        if (!cJSON_IsString(type) || !type->valuestring ||
                strcmp(type->valuestring, "text") != 0 ||
                !cJSON_IsString(text) || !text->valuestring) {
            continue;
        }
        block_size = strlen(text->valuestring);
        memcpy(*out_text + offset, text->valuestring, block_size);
        offset += block_size;
    }

    cJSON_Delete(root);
    return ESP_OK;
}

esp_err_t cap_llm_inspect_anthropic_infer(const struct cap_llm_inspect_runtime *runtime,
                                          const cap_llm_inspect_media_t *media,
                                          const char *system_prompt,
                                          const char *user_prompt,
                                          char **out_text,
                                          char **out_error_message)
{
    cap_llm_inspect_http_request_t request = {0};
    cap_llm_inspect_http_response_t response = {0};
    char *url = NULL;
    char *body = NULL;
    esp_err_t err;
    const cap_llm_inspect_http_header_t headers[] = {
        { .name = "x-api-key", .value = runtime->api_key },
        { .name = "anthropic-version", .value = CAP_LLM_INSPECT_ANTHROPIC_VERSION },
    };

    err = build_anthropic_body(runtime,
                               media,
                               system_prompt,
                               user_prompt,
                               &body,
                               out_error_message);
    if (err != ESP_OK) {
        return err;
    }

    url = cap_llm_inspect_join_url(runtime->base_url, CAP_LLM_INSPECT_ANTHROPIC_PATH);
    if (!url) {
        free(body);
        return json_oom(out_error_message);
    }

    request.url = url;
    request.body = body;
    request.auth_type = "none";
    request.timeout_ms = runtime->timeout_ms;
    request.headers = headers;
    request.header_count = sizeof(headers) / sizeof(headers[0]);
    err = cap_llm_inspect_http_post_json(&request, &response, out_error_message);
    if (err == ESP_OK) {
        err = parse_anthropic_text(response.body, out_text, out_error_message);
    }

    cap_llm_inspect_http_response_free(&response);
    free(url);
    free(body);
    return err;
}
