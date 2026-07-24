/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "cap_agent.h"

#include <errno.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "cap_agent_event.h"
#include "cap_agent_session_command.h"
#include "cJSON.h"
#include "claw_agent.h"
#include "claw_cap.h"
#include "claw_im_session.h"
#include "esp_err.h"
#include "esp_log.h"

static const char *TAG = "cap_agent";

#define CAP_AGENT_CAP_ID       "agent"
#define CAP_AGENT_LIST_RETRIES 3

typedef enum {
    CAP_AGENT_RPC_SESSION_CREATE,
    CAP_AGENT_RPC_SESSION_OPEN,
    CAP_AGENT_RPC_SESSION_LIST,
    CAP_AGENT_RPC_SESSION_SUBMIT,
    CAP_AGENT_RPC_SESSION_RESPOND,
    CAP_AGENT_RPC_SESSION_INPUT,
    CAP_AGENT_RPC_SESSION_INTERRUPT,
    CAP_AGENT_RPC_SESSION_CANCEL,
    CAP_AGENT_RPC_SESSION_CLOSE,
    CAP_AGENT_RPC_SESSION_DELETE,
    CAP_AGENT_RPC_SESSION_COMMAND,
} cap_agent_rpc_method_t;

typedef struct {
    cap_agent_rpc_method_t method;
    const char *method_name;
    uint32_t session_id;
    uint32_t request_id;
    claw_agent_session_persistence_t persistence;
    const char *text;
} cap_agent_rpc_request_t;

static const char s_agent_input_schema[] =
    "{\"type\":\"object\","
    "\"required\":[\"method\",\"args\"],"
    "\"properties\":{"
    "\"method\":{\"type\":\"string\",\"enum\":["
    "\"session.create\",\"session.open\",\"session.list\","
    "\"session.submit\",\"session.respond\",\"session.input\","
    "\"session.interrupt\",\"session.cancel\",\"session.close\","
    "\"session.delete\",\"session.command\"]},"
    "\"args\":{\"type\":\"object\"}},"
    "\"additionalProperties\":false}";

static bool cap_agent_str_empty(const char *value)
{
    return !value || !value[0];
}

static bool cap_agent_object_has_only(const cJSON *object,
                                      const char *const *allowed,
                                      size_t allowed_count)
{
    const cJSON *item;

    if (!cJSON_IsObject(object)) {
        return false;
    }
    cJSON_ArrayForEach(item, object) {
        bool found = false;

        if (!item->string) {
            return false;
        }
        for (size_t i = 0; i < allowed_count; i++) {
            if (strcmp(item->string, allowed[i]) == 0) {
                found = true;
                break;
            }
        }
        if (!found) {
            return false;
        }
    }
    return true;
}

static esp_err_t cap_agent_parse_u32_json(const cJSON *item, uint32_t *out)
{
    double value;
    uint32_t parsed;

    if (!cJSON_IsNumber(item) || !out) {
        return ESP_ERR_INVALID_ARG;
    }
    value = item->valuedouble;
    if (value < 1.0 || value > (double)UINT32_MAX) {
        return ESP_ERR_INVALID_ARG;
    }
    parsed = (uint32_t)value;
    if ((double)parsed != value) {
        return ESP_ERR_INVALID_ARG;
    }
    *out = parsed;
    return ESP_OK;
}

static esp_err_t cap_agent_parse_u32_string(const char *value, uint32_t *out)
{
    char *end = NULL;
    unsigned long parsed;

    if (cap_agent_str_empty(value) || !out) {
        return ESP_ERR_INVALID_ARG;
    }
    errno = 0;
    parsed = strtoul(value, &end, 10);
    if (errno == ERANGE || !end || *end != '\0' ||
            parsed == 0 || parsed > UINT32_MAX) {
        return ESP_ERR_INVALID_ARG;
    }
    *out = (uint32_t)parsed;
    return ESP_OK;
}

static esp_err_t cap_agent_parse_session_id(
    const cJSON *args,
    const claw_cap_call_context_t *ctx,
    uint32_t *out)
{
    const cJSON *item = cJSON_GetObjectItemCaseSensitive(args, "session_id");

    if (item) {
        return cap_agent_parse_u32_json(item, out);
    }
    return ctx ? cap_agent_parse_u32_string(ctx->session_id, out) :
           ESP_ERR_INVALID_ARG;
}

static esp_err_t cap_agent_parse_request_id(
    const cJSON *args,
    const claw_cap_call_context_t *ctx,
    bool required,
    uint32_t *out)
{
    const cJSON *item = cJSON_GetObjectItemCaseSensitive(args, "request_id");

    *out = 0;
    if (item) {
        return cap_agent_parse_u32_json(item, out);
    }
    if (ctx && ctx->request_id != 0) {
        *out = ctx->request_id;
        return ESP_OK;
    }
    return required ? ESP_ERR_INVALID_ARG : ESP_OK;
}

static esp_err_t cap_agent_parse_persistence(
    const cJSON *args,
    claw_agent_session_persistence_t *out)
{
    const char *value = cJSON_GetStringValue(
        cJSON_GetObjectItemCaseSensitive(args, "persistence"));

    if (!value || !out) {
        return ESP_ERR_INVALID_ARG;
    }
    if (strcmp(value, "persistent") == 0) {
        *out = CLAW_AGENT_SESSION_PERSISTENCE_PERSISTENT;
        return ESP_OK;
    }
    if (strcmp(value, "ephemeral") == 0) {
        *out = CLAW_AGENT_SESSION_PERSISTENCE_EPHEMERAL;
        return ESP_OK;
    }
    return ESP_ERR_INVALID_ARG;
}

static esp_err_t cap_agent_parse_text(const cJSON *args, const char **out)
{
    const char *text;

    if (!out) {
        return ESP_ERR_INVALID_ARG;
    }
    text = cJSON_GetStringValue(cJSON_GetObjectItemCaseSensitive(args, "text"));
    if (!text) {
        return ESP_ERR_INVALID_ARG;
    }
    *out = text;
    return ESP_OK;
}

static esp_err_t cap_agent_parse_method(const char *method,
                                        cap_agent_rpc_method_t *out)
{
    if (!method || !out) {
        return ESP_ERR_INVALID_ARG;
    }
    if (strcmp(method, "session.create") == 0) {
        *out = CAP_AGENT_RPC_SESSION_CREATE;
    } else if (strcmp(method, "session.open") == 0) {
        *out = CAP_AGENT_RPC_SESSION_OPEN;
    } else if (strcmp(method, "session.list") == 0) {
        *out = CAP_AGENT_RPC_SESSION_LIST;
    } else if (strcmp(method, "session.submit") == 0) {
        *out = CAP_AGENT_RPC_SESSION_SUBMIT;
    } else if (strcmp(method, "session.respond") == 0) {
        *out = CAP_AGENT_RPC_SESSION_RESPOND;
    } else if (strcmp(method, "session.input") == 0) {
        *out = CAP_AGENT_RPC_SESSION_INPUT;
    } else if (strcmp(method, "session.interrupt") == 0) {
        *out = CAP_AGENT_RPC_SESSION_INTERRUPT;
    } else if (strcmp(method, "session.cancel") == 0) {
        *out = CAP_AGENT_RPC_SESSION_CANCEL;
    } else if (strcmp(method, "session.close") == 0) {
        *out = CAP_AGENT_RPC_SESSION_CLOSE;
    } else if (strcmp(method, "session.delete") == 0) {
        *out = CAP_AGENT_RPC_SESSION_DELETE;
    } else if (strcmp(method, "session.command") == 0) {
        *out = CAP_AGENT_RPC_SESSION_COMMAND;
    } else {
        return ESP_ERR_NOT_SUPPORTED;
    }
    return ESP_OK;
}

static esp_err_t cap_agent_parse_request(
    const cJSON *root,
    const claw_cap_call_context_t *ctx,
    cap_agent_rpc_request_t *out)
{
    static const char *const root_fields[] = {"method", "args"};
    static const char *const no_fields[] = {NULL};
    static const char *const persistence_fields[] = {"persistence"};
    static const char *const session_fields[] = {"session_id"};
    static const char *const input_fields[] = {
        "session_id", "request_id", "text"
    };
    static const char *const command_fields[] = {"text"};
    const cJSON *args;
    const char *method;
    esp_err_t err;

    if (!out ||
            !cap_agent_object_has_only(root,
                                       root_fields,
                                       sizeof(root_fields) /
                                       sizeof(root_fields[0]))) {
        return ESP_ERR_INVALID_ARG;
    }
    method = cJSON_GetStringValue(
        cJSON_GetObjectItemCaseSensitive(root, "method"));
    args = cJSON_GetObjectItemCaseSensitive(root, "args");
    if (!method || !cJSON_IsObject(args)) {
        return ESP_ERR_INVALID_ARG;
    }

    memset(out, 0, sizeof(*out));
    out->method_name = method;
    err = cap_agent_parse_method(method, &out->method);
    if (err != ESP_OK) {
        return err;
    }

    switch (out->method) {
    case CAP_AGENT_RPC_SESSION_CREATE:
        if (!cap_agent_object_has_only(args,
                                       persistence_fields,
                                       sizeof(persistence_fields) /
                                       sizeof(persistence_fields[0]))) {
            return ESP_ERR_INVALID_ARG;
        }
        return cap_agent_parse_persistence(args, &out->persistence);
    case CAP_AGENT_RPC_SESSION_LIST:
        return cap_agent_object_has_only(args, no_fields, 0) ?
               ESP_OK : ESP_ERR_INVALID_ARG;
    case CAP_AGENT_RPC_SESSION_OPEN:
    case CAP_AGENT_RPC_SESSION_INTERRUPT:
    case CAP_AGENT_RPC_SESSION_CANCEL:
    case CAP_AGENT_RPC_SESSION_CLOSE:
    case CAP_AGENT_RPC_SESSION_DELETE:
        if (!cap_agent_object_has_only(args,
                                       session_fields,
                                       sizeof(session_fields) /
                                       sizeof(session_fields[0]))) {
            return ESP_ERR_INVALID_ARG;
        }
        return cap_agent_parse_session_id(args, ctx, &out->session_id);
    case CAP_AGENT_RPC_SESSION_SUBMIT:
        if (!cap_agent_object_has_only(args,
                                       input_fields,
                                       sizeof(input_fields) /
                                       sizeof(input_fields[0])) ||
                cap_agent_parse_session_id(args, ctx, &out->session_id) != ESP_OK ||
                cap_agent_parse_text(args, &out->text) != ESP_OK) {
            return ESP_ERR_INVALID_ARG;
        }
        return cJSON_GetObjectItemCaseSensitive(args, "request_id") ?
               ESP_ERR_INVALID_ARG : ESP_OK;
    case CAP_AGENT_RPC_SESSION_RESPOND:
        if (!cap_agent_object_has_only(args,
                                       input_fields,
                                       sizeof(input_fields) /
                                       sizeof(input_fields[0])) ||
                cap_agent_parse_session_id(args, ctx, &out->session_id) != ESP_OK ||
                cap_agent_parse_request_id(args,
                                           ctx,
                                           true,
                                           &out->request_id) != ESP_OK ||
                cap_agent_parse_text(args, &out->text) != ESP_OK) {
            return ESP_ERR_INVALID_ARG;
        }
        return ESP_OK;
    case CAP_AGENT_RPC_SESSION_INPUT:
        if (!cap_agent_object_has_only(args,
                                       input_fields,
                                       sizeof(input_fields) /
                                       sizeof(input_fields[0])) ||
                cap_agent_parse_session_id(args, ctx, &out->session_id) != ESP_OK ||
                cap_agent_parse_request_id(args,
                                           ctx,
                                           false,
                                           &out->request_id) != ESP_OK ||
                cap_agent_parse_text(args, &out->text) != ESP_OK) {
            return ESP_ERR_INVALID_ARG;
        }
        return ESP_OK;
    case CAP_AGENT_RPC_SESSION_COMMAND:
        if (!cap_agent_object_has_only(args,
                                       command_fields,
                                       sizeof(command_fields) /
                                       sizeof(command_fields[0]))) {
            return ESP_ERR_INVALID_ARG;
        }
        return cap_agent_parse_text(args, &out->text);
    default:
        return ESP_ERR_NOT_SUPPORTED;
    }
}

static esp_err_t cap_agent_write_json(cJSON *root,
                                      char *output,
                                      size_t output_size)
{
    char *json;
    size_t json_size;

    if (!root || !output || output_size == 0) {
        cJSON_Delete(root);
        return ESP_ERR_INVALID_ARG;
    }
    json = cJSON_PrintUnformatted(root);
    cJSON_Delete(root);
    if (!json) {
        return ESP_ERR_NO_MEM;
    }
    json_size = strlen(json) + 1;
    if (json_size > output_size) {
        output[0] = '\0';
        free(json);
        return ESP_ERR_INVALID_SIZE;
    }
    memcpy(output, json, json_size);
    free(json);
    return ESP_OK;
}

static cJSON *cap_agent_result_root(const char *method)
{
    cJSON *root = cJSON_CreateObject();
    cJSON *result = cJSON_CreateObject();

    if (!root || !result ||
            !cJSON_AddBoolToObject(root, "ok", true) ||
            !cJSON_AddStringToObject(root, "method", method)) {
        cJSON_Delete(root);
        cJSON_Delete(result);
        return NULL;
    }
    cJSON_AddItemToObject(root, "result", result);
    return root;
}

static cJSON *cap_agent_result_object(cJSON *root)
{
    return root ? cJSON_GetObjectItemCaseSensitive(root, "result") : NULL;
}

static void cap_agent_write_error(char *output,
                                  size_t output_size,
                                  const char *method,
                                  esp_err_t err,
                                  const char *message)
{
    cJSON *root;
    cJSON *error;

    if (!output || output_size == 0) {
        return;
    }
    root = cJSON_CreateObject();
    error = cJSON_CreateObject();
    if (!root || !error ||
            !cJSON_AddBoolToObject(root, "ok", false) ||
            !cJSON_AddStringToObject(root, "method", method ? method : "") ||
            !cJSON_AddNumberToObject(error, "code", (double)err) ||
            !cJSON_AddStringToObject(error, "name", esp_err_to_name(err)) ||
            !cJSON_AddStringToObject(error,
                                    "message",
                                    message ? message : esp_err_to_name(err))) {
        cJSON_Delete(root);
        cJSON_Delete(error);
        snprintf(output, output_size, "{\"ok\":false,\"code\":%d}", (int)err);
        return;
    }
    cJSON_AddItemToObject(root, "error", error);
    if (cap_agent_write_json(root, output, output_size) != ESP_OK) {
        snprintf(output, output_size, "{\"ok\":false,\"code\":%d}", (int)err);
    }
}

static void cap_agent_route_from_context(const claw_cap_call_context_t *ctx,
                                         cap_agent_event_route_t *out)
{
    memset(out, 0, sizeof(*out));
    if (!ctx) {
        return;
    }
    out->channel = !cap_agent_str_empty(ctx->target_channel) ?
                   ctx->target_channel : ctx->channel;
    out->chat_id = !cap_agent_str_empty(ctx->target_chat_id) ?
                   ctx->target_chat_id : ctx->chat_id;
    out->correlation_id = ctx->correlation_id;
}

static void cap_agent_drain_unattached_close(uint32_t session_id)
{
    for (size_t attempt = 0; attempt < 8; attempt++) {
        claw_agent_event_t event = {0};
        esp_err_t err = claw_agent_session_receive(session_id, &event, 250);

        if (err == ESP_ERR_TIMEOUT) {
            continue;
        }
        if (err != ESP_OK) {
            return;
        }
        bool closed = event.kind == CLAW_AGENT_EVENT_KIND_CLOSED;

        claw_agent_event_free(&event);
        if (closed) {
            return;
        }
    }
}

static esp_err_t cap_agent_execute_create(const cap_agent_rpc_request_t *request,
                                          char *output,
                                          size_t output_size)
{
    cJSON *root;
    cJSON *result;
    uint32_t session_id;
    esp_err_t err = claw_agent_session_create(request->persistence, &session_id);

    if (err != ESP_OK) {
        return err;
    }
    root = cap_agent_result_root(request->method_name);
    result = cap_agent_result_object(root);
    if (!root || !result ||
            !cJSON_AddNumberToObject(result,
                                    "session_id",
                                    (double)session_id)) {
        cJSON_Delete(root);
        return ESP_ERR_NO_MEM;
    }
    return cap_agent_write_json(root, output, output_size);
}

static esp_err_t cap_agent_execute_list(const cap_agent_rpc_request_t *request,
                                        char *output,
                                        size_t output_size)
{
    uint32_t *session_ids = NULL;
    size_t count = 0;
    cJSON *root;
    cJSON *result;
    cJSON *sessions;
    esp_err_t err = ESP_ERR_INVALID_SIZE;

    for (size_t attempt = 0; attempt < CAP_AGENT_LIST_RETRIES; attempt++) {
        err = claw_agent_session_list(NULL, 0, &count);
        if (err != ESP_OK && err != ESP_ERR_INVALID_SIZE) {
            return err;
        }
        if (count == 0) {
            err = ESP_OK;
            break;
        }
        if (count > SIZE_MAX / sizeof(*session_ids)) {
            return ESP_ERR_INVALID_SIZE;
        }
        session_ids = calloc(count, sizeof(*session_ids));
        if (!session_ids) {
            return ESP_ERR_NO_MEM;
        }
        err = claw_agent_session_list(session_ids, count, &count);
        if (err != ESP_ERR_INVALID_SIZE) {
            break;
        }
        free(session_ids);
        session_ids = NULL;
    }
    if (err != ESP_OK) {
        free(session_ids);
        return err;
    }

    root = cap_agent_result_root(request->method_name);
    result = cap_agent_result_object(root);
    sessions = cJSON_CreateArray();
    if (!root || !result || !sessions) {
        cJSON_Delete(root);
        cJSON_Delete(sessions);
        free(session_ids);
        return ESP_ERR_NO_MEM;
    }
    for (size_t i = 0; i < count; i++) {
        cJSON *id = cJSON_CreateNumber((double)session_ids[i]);

        if (!id) {
            cJSON_Delete(root);
            free(session_ids);
            return ESP_ERR_NO_MEM;
        }
        cJSON_AddItemToArray(sessions, id);
    }
    free(session_ids);
    cJSON_AddItemToObject(result, "sessions", sessions);
    return cap_agent_write_json(root, output, output_size);
}

static esp_err_t cap_agent_execute_open(const cap_agent_rpc_request_t *request,
                                        char *output,
                                        size_t output_size)
{
    cJSON *root;
    cJSON *result;
    esp_err_t err = claw_agent_session_open(request->session_id);

    if (err != ESP_OK) {
        return err;
    }
    err = cap_agent_event_attach(request->session_id);
    if (err != ESP_OK) {
        if (claw_agent_session_close(request->session_id) == ESP_OK) {
            cap_agent_drain_unattached_close(request->session_id);
        }
        return err;
    }

    root = cap_agent_result_root(request->method_name);
    result = cap_agent_result_object(root);
    if (!root || !result ||
            !cJSON_AddNumberToObject(result,
                                    "session_id",
                                    (double)request->session_id) ||
            !cJSON_AddBoolToObject(result, "attached", true)) {
        cJSON_Delete(root);
        return ESP_ERR_NO_MEM;
    }
    return cap_agent_write_json(root, output, output_size);
}

static esp_err_t cap_agent_execute_input(const cap_agent_rpc_request_t *request,
                                         const claw_cap_call_context_t *ctx,
                                         char *output,
                                         size_t output_size)
{
    cap_agent_event_route_t route;
    cJSON *root;
    cJSON *result;
    const char *operation;
    esp_err_t err;

    if (!cap_agent_event_is_attached(request->session_id)) {
        return ESP_ERR_INVALID_STATE;
    }
    if (request->method == CAP_AGENT_RPC_SESSION_RESPOND ||
            (request->method == CAP_AGENT_RPC_SESSION_INPUT &&
             request->request_id != 0)) {
        operation = "respond";
        err = claw_agent_session_respond(request->session_id,
                                         request->request_id,
                                         request->text);
        if (err == ESP_OK) {
            esp_err_t clear_err = claw_im_session_clear_input_request(
                request->session_id,
                request->request_id);

            if (clear_err != ESP_OK) {
                ESP_LOGW(TAG,
                         "failed to clear IM request session=%" PRIu32
                         " request=%" PRIu32 " err=%s",
                         request->session_id,
                         request->request_id,
                         esp_err_to_name(clear_err));
            }
        }
    } else {
        operation = "submit";
        cap_agent_route_from_context(ctx, &route);
        err = cap_agent_event_submit(request->session_id, request->text, &route);
    }
    if (err != ESP_OK) {
        return err;
    }

    root = cap_agent_result_root(request->method_name);
    result = cap_agent_result_object(root);
    if (!root || !result ||
            !cJSON_AddStringToObject(result, "operation", operation) ||
            !cJSON_AddNumberToObject(result,
                                    "session_id",
                                    (double)request->session_id) ||
            !cJSON_AddBoolToObject(result, "accepted", true) ||
            (request->request_id != 0 &&
             !cJSON_AddNumberToObject(result,
                                     "request_id",
                                     (double)request->request_id))) {
        cJSON_Delete(root);
        return ESP_ERR_NO_MEM;
    }
    return cap_agent_write_json(root, output, output_size);
}

static esp_err_t cap_agent_execute_session_operation(
    const cap_agent_rpc_request_t *request,
    char *output,
    size_t output_size)
{
    cJSON *root;
    cJSON *result;
    esp_err_t err;

    switch (request->method) {
    case CAP_AGENT_RPC_SESSION_INTERRUPT:
        err = claw_agent_session_interrupt(request->session_id);
        break;
    case CAP_AGENT_RPC_SESSION_CANCEL:
        err = claw_agent_session_cancel(request->session_id);
        break;
    case CAP_AGENT_RPC_SESSION_CLOSE:
        err = claw_agent_session_close(request->session_id);
        break;
    case CAP_AGENT_RPC_SESSION_DELETE:
        err = claw_agent_session_delete(request->session_id);
        break;
    default:
        return ESP_ERR_INVALID_ARG;
    }
    if (err != ESP_OK) {
        return err;
    }

    root = cap_agent_result_root(request->method_name);
    result = cap_agent_result_object(root);
    if (!root || !result ||
            !cJSON_AddNumberToObject(result,
                                    "session_id",
                                    (double)request->session_id) ||
            !cJSON_AddBoolToObject(result, "accepted", true)) {
        cJSON_Delete(root);
        return ESP_ERR_NO_MEM;
    }
    return cap_agent_write_json(root, output, output_size);
}

static esp_err_t cap_agent_execute_rpc(const cap_agent_rpc_request_t *request,
                                       const claw_cap_call_context_t *ctx,
                                       char *output,
                                       size_t output_size)
{
    switch (request->method) {
    case CAP_AGENT_RPC_SESSION_CREATE:
        return cap_agent_execute_create(request, output, output_size);
    case CAP_AGENT_RPC_SESSION_OPEN:
        return cap_agent_execute_open(request, output, output_size);
    case CAP_AGENT_RPC_SESSION_LIST:
        return cap_agent_execute_list(request, output, output_size);
    case CAP_AGENT_RPC_SESSION_SUBMIT:
    case CAP_AGENT_RPC_SESSION_RESPOND:
    case CAP_AGENT_RPC_SESSION_INPUT:
        return cap_agent_execute_input(request, ctx, output, output_size);
    case CAP_AGENT_RPC_SESSION_INTERRUPT:
    case CAP_AGENT_RPC_SESSION_CANCEL:
    case CAP_AGENT_RPC_SESSION_CLOSE:
    case CAP_AGENT_RPC_SESSION_DELETE:
        return cap_agent_execute_session_operation(request, output, output_size);
    case CAP_AGENT_RPC_SESSION_COMMAND:
        if (!cap_agent_session_command_matches(request->text)) {
            return ESP_ERR_INVALID_ARG;
        }
        return cap_agent_session_command_execute_message(request->text,
                                                         ctx,
                                                         output,
                                                         output_size);
    default:
        return ESP_ERR_NOT_SUPPORTED;
    }
}

static esp_err_t cap_agent_execute(const char *input_json,
                                   const claw_cap_call_context_t *ctx,
                                   char *output,
                                   size_t output_size)
{
    cap_agent_rpc_request_t request = {0};
    cJSON *root;
    esp_err_t err;

    if (!input_json || !output || output_size == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    output[0] = '\0';
    root = cJSON_Parse(input_json);
    if (!cJSON_IsObject(root)) {
        cJSON_Delete(root);
        cap_agent_write_error(output,
                              output_size,
                              NULL,
                              ESP_ERR_INVALID_ARG,
                              "expected an Agent RPC object");
        return ESP_ERR_INVALID_ARG;
    }
    err = cap_agent_parse_request(root, ctx, &request);
    if (err == ESP_OK) {
        err = cap_agent_execute_rpc(&request, ctx, output, output_size);
    }
    if (err != ESP_OK && output[0] == '\0') {
        cap_agent_write_error(output,
                              output_size,
                              request.method_name,
                              err,
                              esp_err_to_name(err));
    }
    cJSON_Delete(root);
    return err;
}

/* System-only RPC endpoint. AgentSystem lifecycle and API credentials remain
 * application-owned; Session methods are the C-system integration surface. */
static const claw_cap_descriptor_t s_agent_descriptors[] = {
    {
        .id = CAP_AGENT_CAP_ID,
        .name = CAP_AGENT_CAP_ID,
        .family = "agent",
        .description = "RPC adapter for claw_agent.h Session APIs.",
        .kind = CLAW_CAP_KIND_HYBRID,
        .cap_flags = CLAW_CAP_FLAG_EMITS_EVENTS,
        .input_schema_json = s_agent_input_schema,
        .execute = cap_agent_execute,
    },
};

static const claw_cap_group_t s_agent_group = {
    .group_id = "cap_agent",
    .descriptors = s_agent_descriptors,
    .descriptor_count = sizeof(s_agent_descriptors) /
                        sizeof(s_agent_descriptors[0]),
};

esp_err_t cap_agent_register_group(void)
{
    if (claw_cap_group_exists(s_agent_group.group_id)) {
        return ESP_OK;
    }
    return claw_cap_register_group(&s_agent_group);
}
