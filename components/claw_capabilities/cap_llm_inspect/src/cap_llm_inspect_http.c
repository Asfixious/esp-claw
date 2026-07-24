/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "cap_llm_inspect_internal.h"

#include <limits.h>
#include <stdlib.h>
#include <string.h>

#include "cJSON.h"
#include "esp_check.h"
#include "esp_crt_bundle.h"
#include "esp_http_client.h"
#include "esp_log.h"

#define CAP_LLM_INSPECT_HTTP_INITIAL_RESPONSE_BYTES 4096U
#define CAP_LLM_INSPECT_HTTP_MAX_RESPONSE_BYTES     (128U * 1024U)

static const char *TAG = "llm_inspect_http";

typedef struct {
    char *data;
    size_t length;
    size_t capacity;
} response_buffer_t;

static esp_err_t response_buffer_create(response_buffer_t *buffer)
{
    buffer->data = calloc(1, CAP_LLM_INSPECT_HTTP_INITIAL_RESPONSE_BYTES);
    if (!buffer->data) {
        return ESP_ERR_NO_MEM;
    }
    buffer->capacity = CAP_LLM_INSPECT_HTTP_INITIAL_RESPONSE_BYTES;
    return ESP_OK;
}

static esp_err_t response_buffer_append(response_buffer_t *buffer,
                                        const char *data,
                                        size_t data_size)
{
    size_t required;
    size_t next_capacity;
    char *resized = NULL;

    if (!buffer || !buffer->data || (!data && data_size > 0)) {
        return ESP_ERR_INVALID_ARG;
    }
    if (data_size > SIZE_MAX - buffer->length - 1) {
        return ESP_ERR_INVALID_SIZE;
    }

    required = buffer->length + data_size + 1;
    if (required > CAP_LLM_INSPECT_HTTP_MAX_RESPONSE_BYTES) {
        return ESP_ERR_INVALID_SIZE;
    }

    next_capacity = buffer->capacity;
    while (next_capacity < required) {
        if (next_capacity > CAP_LLM_INSPECT_HTTP_MAX_RESPONSE_BYTES / 2) {
            next_capacity = CAP_LLM_INSPECT_HTTP_MAX_RESPONSE_BYTES;
        } else {
            next_capacity *= 2;
        }
    }
    if (next_capacity != buffer->capacity) {
        resized = realloc(buffer->data, next_capacity);
        if (!resized) {
            return ESP_ERR_NO_MEM;
        }
        buffer->data = resized;
        buffer->capacity = next_capacity;
    }

    if (data_size > 0) {
        memcpy(buffer->data + buffer->length, data, data_size);
        buffer->length += data_size;
    }
    buffer->data[buffer->length] = '\0';
    return ESP_OK;
}

static void response_buffer_delete(response_buffer_t *buffer)
{
    if (!buffer) {
        return;
    }
    free(buffer->data);
    memset(buffer, 0, sizeof(*buffer));
}

static esp_err_t http_event_handler(esp_http_client_event_t *event)
{
    response_buffer_t *buffer = event ? event->user_data : NULL;

    if (!event || !buffer) {
        return ESP_ERR_INVALID_ARG;
    }
    if (event->event_id != HTTP_EVENT_ON_DATA) {
        return ESP_OK;
    }
    return response_buffer_append(buffer, event->data, event->data_len);
}

static char *build_auth_header(const char *auth_type, const char *api_key)
{
    const char *kind = auth_type && auth_type[0] ? auth_type : "bearer";

    if (!api_key || !api_key[0] || strcmp(kind, "none") == 0) {
        return NULL;
    }
    if (strcmp(kind, "api-key") == 0) {
        return strdup(api_key);
    }
    return cap_llm_inspect_format("Bearer %s", api_key);
}

static const char *auth_header_name(const char *auth_type)
{
    return auth_type && strcmp(auth_type, "api-key") == 0 ?
           "X-API-Key" : "Authorization";
}

static char *parse_http_error(const char *body, int status_code)
{
    cJSON *root = NULL;
    cJSON *error = NULL;
    cJSON *message = NULL;
    char *result = NULL;

    if (!body || !body[0]) {
        return cap_llm_inspect_format("HTTP %d", status_code);
    }

    root = cJSON_Parse(body);
    if (!root) {
        return cap_llm_inspect_format("HTTP %d: %.160s", status_code, body);
    }

    error = cJSON_GetObjectItemCaseSensitive(root, "error");
    if (cJSON_IsObject(error)) {
        message = cJSON_GetObjectItemCaseSensitive(error, "message");
    }
    if (!cJSON_IsString(message)) {
        message = cJSON_GetObjectItemCaseSensitive(root, "message");
    }
    if (cJSON_IsString(message) && message->valuestring && message->valuestring[0]) {
        result = cap_llm_inspect_format("HTTP %d: %s", status_code, message->valuestring);
    } else {
        result = cap_llm_inspect_format("HTTP %d: %.160s", status_code, body);
    }
    cJSON_Delete(root);
    return result;
}

esp_err_t cap_llm_inspect_http_post_json(const cap_llm_inspect_http_request_t *request,
                                         cap_llm_inspect_http_response_t *out_response,
                                         char **out_error_message)
{
    response_buffer_t response_buffer = {0};
    esp_http_client_config_t client_config = {0};
    esp_http_client_handle_t client = NULL;
    char *auth_header = NULL;
    size_t body_size;
    int status_code;
    esp_err_t ret;

    if (out_response) {
        memset(out_response, 0, sizeof(*out_response));
    }
    if (out_error_message) {
        *out_error_message = NULL;
    }
    if (!request || !request->url || !request->body ||
            !out_response || !out_error_message) {
        return ESP_ERR_INVALID_ARG;
    }

    body_size = strlen(request->body);
    if (body_size > INT_MAX) {
        *out_error_message = strdup("Image request body is too large");
        return ESP_ERR_INVALID_SIZE;
    }

    ret = response_buffer_create(&response_buffer);
    if (ret != ESP_OK) {
        *out_error_message = strdup("Out of memory allocating HTTP response buffer");
        return ret;
    }

    client_config.url = request->url;
    client_config.event_handler = http_event_handler;
    client_config.user_data = &response_buffer;
    client_config.timeout_ms = request->timeout_ms;
    client_config.buffer_size = 4096;
    client_config.buffer_size_tx = 4096;
    client_config.keep_alive_enable = true;
    client_config.crt_bundle_attach = esp_crt_bundle_attach;
    client = esp_http_client_init(&client_config);
    if (!client) {
        *out_error_message = strdup("Failed to create HTTP client");
        ret = ESP_FAIL;
        goto cleanup;
    }

    ESP_GOTO_ON_ERROR(esp_http_client_set_method(client, HTTP_METHOD_POST),
                      cleanup, TAG, "Failed to set HTTP method");
    ESP_GOTO_ON_ERROR(esp_http_client_set_header(client, "Content-Type", "application/json"),
                      cleanup, TAG, "Failed to set content type");

    auth_header = build_auth_header(request->auth_type, request->api_key);
    if (auth_header) {
        ESP_GOTO_ON_ERROR(esp_http_client_set_header(client,
                                                     auth_header_name(request->auth_type),
                                                     auth_header),
                          cleanup, TAG, "Failed to set authorization header");
    }
    for (size_t i = 0; i < request->header_count; i++) {
        const cap_llm_inspect_http_header_t *header = &request->headers[i];

        if (!header->name || !header->name[0] || !header->value) {
            continue;
        }
        ESP_GOTO_ON_ERROR(esp_http_client_set_header(client, header->name, header->value),
                          cleanup, TAG, "Failed to set provider header");
    }
    ESP_GOTO_ON_ERROR(esp_http_client_set_post_field(client,
                                                     request->body,
                                                     (int)body_size),
                      cleanup, TAG, "Failed to set HTTP request body");

    ret = esp_http_client_perform(client);
    if (ret != ESP_OK) {
        *out_error_message = cap_llm_inspect_format("HTTP request failed: %s",
                                                    esp_err_to_name(ret));
        goto cleanup;
    }

    status_code = esp_http_client_get_status_code(client);
    if (status_code < 200 || status_code >= 300) {
        *out_error_message = parse_http_error(response_buffer.data, status_code);
        ret = ESP_FAIL;
        goto cleanup;
    }

    out_response->body = response_buffer.data;
    out_response->status_code = status_code;
    response_buffer.data = NULL;
    ret = ESP_OK;

cleanup:
    if (ret != ESP_OK && !*out_error_message) {
        *out_error_message = cap_llm_inspect_format("HTTP setup failed: %s",
                                                    esp_err_to_name(ret));
    }
    free(auth_header);
    if (client) {
        esp_http_client_cleanup(client);
    }
    response_buffer_delete(&response_buffer);
    return ret;
}

void cap_llm_inspect_http_response_free(cap_llm_inspect_http_response_t *response)
{
    if (!response) {
        return;
    }
    free(response->body);
    memset(response, 0, sizeof(*response));
}
