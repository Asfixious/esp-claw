/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "cap_agent.h"

#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "cap_agent_reply.h"
#include "cap_agent_session_command.h"
#include "cJSON.h"
#include "claw_agent.h"
#include "claw_cap.h"
#include "claw_im_session.h"
#include "esp_err.h"
#include "esp_log.h"

static const char *TAG = "cap_agent";

#define CAP_AGENT_CAP_ID "agent"
#define CAP_AGENT_LIST_RETRIES 3

typedef enum {
    CAP_AGENT_REQUEST_SESSION_CREATE,
    CAP_AGENT_REQUEST_SESSION_OPEN,
    CAP_AGENT_REQUEST_SESSION_CLOSE,
    CAP_AGENT_REQUEST_SESSION_DELETE,
    CAP_AGENT_REQUEST_SESSION_LIST,
    CAP_AGENT_REQUEST_INPUT,
    CAP_AGENT_REQUEST_INTERRUPT,
    CAP_AGENT_REQUEST_CANCEL,
    CAP_AGENT_REQUEST_MESSAGE,
} cap_agent_request_kind_t;

typedef struct {
    cap_agent_request_kind_t kind;
    uint32_t session_id;
    claw_agent_session_persistence_t persistence;
    const char *text;
    bool has_request_id;
    uint32_t request_id;
} cap_agent_request_t;

static const char s_agent_input_schema[] =
    "{\"type\":\"object\",\"properties\":{"
    "\"message\":{\"type\":\"string\"},"
    "\"session\":{\"type\":\"object\",\"properties\":{"
    "\"action\":{\"type\":\"string\",\"enum\":[\"create\",\"open\",\"close\",\"delete\",\"list\"]},"
    "\"session_id\":{\"type\":\"integer\",\"minimum\":1},"
    "\"persistence\":{\"type\":\"string\",\"enum\":[\"persistent\",\"ephemeral\"]}},"
    "\"additionalProperties\":false},"
    "\"input\":{\"type\":\"object\",\"properties\":{"
    "\"text\":{\"type\":\"string\"},"
    "\"request_id\":{\"type\":\"integer\",\"minimum\":1}},"
    "\"required\":[\"text\"],\"additionalProperties\":false},"
    "\"control\":{\"type\":\"object\",\"properties\":{"
    "\"action\":{\"type\":\"string\",\"enum\":[\"interrupt\",\"cancel\"]}},"
    "\"required\":[\"action\"],\"additionalProperties\":false}},"
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

static esp_err_t cap_agent_parse_u32(const cJSON *item, uint32_t *out)
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

static esp_err_t cap_agent_parse_persistence(const cJSON *session,
                                             claw_agent_session_persistence_t *out)
{
    const char *value = cJSON_GetStringValue(
        cJSON_GetObjectItemCaseSensitive(session, "persistence"));

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

static esp_err_t cap_agent_parse_session_action(const cJSON *session,
                                                const char *action,
                                                cap_agent_request_t *out)
{
    static const char *const create_fields[] = {"action", "persistence"};
    static const char *const id_fields[] = {"action", "session_id"};
    static const char *const list_fields[] = {"action"};
    const cJSON *session_id = cJSON_GetObjectItemCaseSensitive(session, "session_id");

    if (strcmp(action, "create") == 0) {
        if (!cap_agent_object_has_only(session,
                                       create_fields,
                                       sizeof(create_fields) / sizeof(create_fields[0]))) {
            return ESP_ERR_INVALID_ARG;
        }
        out->kind = CAP_AGENT_REQUEST_SESSION_CREATE;
        return cap_agent_parse_persistence(session, &out->persistence);
    }
    if (strcmp(action, "list") == 0) {
        if (!cap_agent_object_has_only(session,
                                       list_fields,
                                       sizeof(list_fields) / sizeof(list_fields[0]))) {
            return ESP_ERR_INVALID_ARG;
        }
        out->kind = CAP_AGENT_REQUEST_SESSION_LIST;
        return ESP_OK;
    }
    if (!cap_agent_object_has_only(session,
                                   id_fields,
                                   sizeof(id_fields) / sizeof(id_fields[0]))) {
        return ESP_ERR_INVALID_ARG;
    }
    if (cap_agent_parse_u32(session_id, &out->session_id) != ESP_OK) {
        return ESP_ERR_INVALID_ARG;
    }
    if (strcmp(action, "open") == 0) {
        out->kind = CAP_AGENT_REQUEST_SESSION_OPEN;
        return ESP_OK;
    }
    if (strcmp(action, "close") == 0) {
        out->kind = CAP_AGENT_REQUEST_SESSION_CLOSE;
        return ESP_OK;
    }
    if (strcmp(action, "delete") == 0) {
        out->kind = CAP_AGENT_REQUEST_SESSION_DELETE;
        return ESP_OK;
    }
    return ESP_ERR_INVALID_ARG;
}

static esp_err_t cap_agent_parse_input(const cJSON *session,
                                       const cJSON *input,
                                       cap_agent_request_t *out)
{
    static const char *const session_fields[] = {"session_id"};
    static const char *const input_fields[] = {"text", "request_id"};
    const cJSON *request_id;

    if (!cap_agent_object_has_only(session,
                                   session_fields,
                                   sizeof(session_fields) / sizeof(session_fields[0])) ||
            !cap_agent_object_has_only(input,
                                       input_fields,
                                       sizeof(input_fields) / sizeof(input_fields[0])) ||
            cap_agent_parse_u32(cJSON_GetObjectItemCaseSensitive(session, "session_id"),
                                &out->session_id) != ESP_OK) {
        return ESP_ERR_INVALID_ARG;
    }
    out->text = cJSON_GetStringValue(cJSON_GetObjectItemCaseSensitive(input, "text"));
    if (!out->text) {
        return ESP_ERR_INVALID_ARG;
    }

    request_id = cJSON_GetObjectItemCaseSensitive(input, "request_id");
    if (request_id) {
        if (cap_agent_parse_u32(request_id, &out->request_id) != ESP_OK) {
            return ESP_ERR_INVALID_ARG;
        }
        out->has_request_id = true;
    }
    out->kind = CAP_AGENT_REQUEST_INPUT;
    return ESP_OK;
}

static esp_err_t cap_agent_parse_control(const cJSON *session,
                                         const cJSON *control,
                                         cap_agent_request_t *out)
{
    static const char *const session_fields[] = {"session_id"};
    static const char *const control_fields[] = {"action"};
    const char *action;

    if (!cap_agent_object_has_only(session,
                                   session_fields,
                                   sizeof(session_fields) / sizeof(session_fields[0])) ||
            !cap_agent_object_has_only(control,
                                       control_fields,
                                       sizeof(control_fields) / sizeof(control_fields[0])) ||
            cap_agent_parse_u32(cJSON_GetObjectItemCaseSensitive(session, "session_id"),
                                &out->session_id) != ESP_OK) {
        return ESP_ERR_INVALID_ARG;
    }
    action = cJSON_GetStringValue(cJSON_GetObjectItemCaseSensitive(control, "action"));
    if (!action) {
        return ESP_ERR_INVALID_ARG;
    }
    if (strcmp(action, "interrupt") == 0) {
        out->kind = CAP_AGENT_REQUEST_INTERRUPT;
        return ESP_OK;
    }
    if (strcmp(action, "cancel") == 0) {
        out->kind = CAP_AGENT_REQUEST_CANCEL;
        return ESP_OK;
    }
    return ESP_ERR_INVALID_ARG;
}

static esp_err_t cap_agent_parse_request(const cJSON *root, cap_agent_request_t *out)
{
    static const char *const root_fields[] = {"message", "session", "input", "control"};
    const cJSON *session;
    const cJSON *input;
    const cJSON *control;
    const cJSON *message;
    const cJSON *action_item;
    const char *action;

    if (!out ||
            !cap_agent_object_has_only(root,
                                       root_fields,
                                       sizeof(root_fields) / sizeof(root_fields[0]))) {
        return ESP_ERR_INVALID_ARG;
    }
    memset(out, 0, sizeof(*out));
    session = cJSON_GetObjectItemCaseSensitive(root, "session");
    input = cJSON_GetObjectItemCaseSensitive(root, "input");
    control = cJSON_GetObjectItemCaseSensitive(root, "control");
    message = cJSON_GetObjectItemCaseSensitive(root, "message");
    if (message) {
        if (!cJSON_IsString(message) || !message->valuestring ||
                session || input || control) {
            return ESP_ERR_INVALID_ARG;
        }
        out->kind = CAP_AGENT_REQUEST_MESSAGE;
        out->text = message->valuestring;
        return ESP_OK;
    }
    if (!cJSON_IsObject(session) || (input && control)) {
        return ESP_ERR_INVALID_ARG;
    }

    action_item = cJSON_GetObjectItemCaseSensitive(session, "action");
    if (action_item) {
        action = cJSON_GetStringValue(action_item);
        if (!action) {
            return ESP_ERR_INVALID_ARG;
        }
        if (input || control) {
            return ESP_ERR_INVALID_ARG;
        }
        return cap_agent_parse_session_action(session, action, out);
    }
    if (input) {
        return cap_agent_parse_input(session, input, out);
    }
    if (control) {
        return cap_agent_parse_control(session, control, out);
    }
    return ESP_ERR_INVALID_ARG;
}

static esp_err_t cap_agent_write_json(cJSON *root, char *output, size_t output_size)
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

static cJSON *cap_agent_result(const char *operation, uint32_t session_id)
{
    cJSON *root = cJSON_CreateObject();
    cJSON *session = NULL;

    if (!root || !cJSON_AddBoolToObject(root, "ok", true) ||
            !cJSON_AddStringToObject(root, "operation", operation)) {
        cJSON_Delete(root);
        return NULL;
    }
    if (session_id != 0) {
        session = cJSON_CreateObject();
        if (!session ||
                !cJSON_AddNumberToObject(session, "session_id", (double)session_id)) {
            cJSON_Delete(session);
            cJSON_Delete(root);
            return NULL;
        }
        cJSON_AddItemToObject(root, "session", session);
    }
    return root;
}

static void cap_agent_write_error(char *output,
                                  size_t output_size,
                                  esp_err_t err,
                                  const char *message)
{
    cJSON *root;
    bool built;

    if (!output || output_size == 0) {
        return;
    }
    root = cJSON_CreateObject();
    built = root && cJSON_AddBoolToObject(root, "ok", false) &&
            cJSON_AddStringToObject(root, "error", message) &&
            cJSON_AddNumberToObject(root, "code", (double)err);
    if (!built) {
        cJSON_Delete(root);
        snprintf(output, output_size, "{\"ok\":false,\"code\":%d}", (int)err);
        return;
    }
    if (cap_agent_write_json(root, output, output_size) != ESP_OK) {
        snprintf(output, output_size, "{\"ok\":false,\"code\":%d}", (int)err);
    }
}

static void cap_agent_route_from_context(const claw_cap_call_context_t *ctx,
                                         cap_agent_reply_route_t *out)
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

static esp_err_t cap_agent_execute_input(const cap_agent_request_t *request,
                                         const claw_cap_call_context_t *ctx,
                                         char *output,
                                         size_t output_size)
{
    cap_agent_reply_route_t route;
    cJSON *result;
    cJSON *input_result;
    esp_err_t err;

    if (!cap_agent_reply_is_attached(request->session_id)) {
        /* IM owns automatic session selection. Explicit C/API users must open
         * through cap_agent so we never attach a second receiver to a stream
         * owned elsewhere. */
        if (!claw_im_session_is_managed(request->session_id)) {
            return ESP_ERR_INVALID_STATE;
        }
        err = cap_agent_reply_ensure(request->session_id);
        if (err != ESP_OK) {
            return err;
        }
    }

    if (request->has_request_id) {
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
        cap_agent_route_from_context(ctx, &route);
        err = cap_agent_reply_submit(request->session_id, request->text, &route);
    }
    if (err != ESP_OK) {
        return err;
    }

    result = cap_agent_result("input", request->session_id);
    input_result = cJSON_CreateObject();
    if (!result || !input_result ||
            !cJSON_AddBoolToObject(input_result, "accepted", true)) {
        cJSON_Delete(input_result);
        cJSON_Delete(result);
        return ESP_ERR_NO_MEM;
    }
    if (request->has_request_id &&
            !cJSON_AddNumberToObject(input_result,
                                    "request_id",
                                    (double)request->request_id)) {
        cJSON_Delete(input_result);
        cJSON_Delete(result);
        return ESP_ERR_NO_MEM;
    }
    cJSON_AddItemToObject(result, "input", input_result);
    return cap_agent_write_json(result, output, output_size);
}

static esp_err_t cap_agent_execute_list(char *output, size_t output_size)
{
    uint32_t *session_ids = NULL;
    size_t count = 0;
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

    cJSON *result = cap_agent_result("list", 0);
    cJSON *sessions = cJSON_CreateArray();
    if (!result || !sessions) {
        cJSON_Delete(result);
        cJSON_Delete(sessions);
        free(session_ids);
        return ESP_ERR_NO_MEM;
    }
    for (size_t i = 0; i < count; i++) {
        cJSON *id = cJSON_CreateNumber((double)session_ids[i]);
        if (!id) {
            cJSON_Delete(result);
            cJSON_Delete(sessions);
            free(session_ids);
            return ESP_ERR_NO_MEM;
        }
        cJSON_AddItemToArray(sessions, id);
    }
    free(session_ids);
    cJSON_AddItemToObject(result, "sessions", sessions);
    return cap_agent_write_json(result, output, output_size);
}

static esp_err_t cap_agent_execute_session(const cap_agent_request_t *request,
                                           char *output,
                                           size_t output_size)
{
    cJSON *result;
    uint32_t session_id = request->session_id;
    const char *operation;
    esp_err_t err;

    switch (request->kind) {
    case CAP_AGENT_REQUEST_SESSION_CREATE:
        operation = "create";
        err = claw_agent_session_create(request->persistence, &session_id);
        break;
    case CAP_AGENT_REQUEST_SESSION_OPEN:
        operation = "open";
        err = claw_agent_session_open(session_id);
        if (err == ESP_OK) {
            err = cap_agent_reply_ensure(session_id);
            if (err != ESP_OK) {
                (void)claw_agent_session_close(session_id);
            }
        } else if (err == ESP_ERR_INVALID_STATE &&
                   !cap_agent_reply_is_attached(session_id) &&
                   claw_im_session_is_managed(session_id)) {
            /* The IM ingress may have opened this integration-owned stream
             * before Router reached cap_agent. */
            err = cap_agent_reply_ensure(session_id);
        }
        if (err == ESP_OK) {
            (void)claw_im_session_mark_open(session_id);
        }
        break;
    case CAP_AGENT_REQUEST_SESSION_CLOSE:
        operation = "close";
        err = claw_agent_session_close(session_id);
        break;
    case CAP_AGENT_REQUEST_SESSION_DELETE:
        operation = "delete";
        err = claw_agent_session_delete(session_id);
        if (err == ESP_OK) {
            (void)claw_im_session_forget(session_id);
        }
        break;
    case CAP_AGENT_REQUEST_SESSION_LIST:
        return cap_agent_execute_list(output, output_size);
    default:
        return ESP_ERR_INVALID_ARG;
    }
    if (err != ESP_OK) {
        return err;
    }
    result = cap_agent_result(operation, session_id);
    return result ? cap_agent_write_json(result, output, output_size) : ESP_ERR_NO_MEM;
}

static esp_err_t cap_agent_execute_control(const cap_agent_request_t *request,
                                           char *output,
                                           size_t output_size)
{
    const char *operation;
    cJSON *result;
    esp_err_t err;

    if (request->kind == CAP_AGENT_REQUEST_INTERRUPT) {
        operation = "interrupt";
        err = claw_agent_session_interrupt(request->session_id);
    } else if (request->kind == CAP_AGENT_REQUEST_CANCEL) {
        operation = "cancel";
        err = claw_agent_session_cancel(request->session_id);
    } else {
        return ESP_ERR_INVALID_ARG;
    }
    if (err != ESP_OK) {
        return err;
    }
    result = cap_agent_result(operation, request->session_id);
    return result ? cap_agent_write_json(result, output, output_size) : ESP_ERR_NO_MEM;
}

static esp_err_t cap_agent_execute(const char *input_json,
                                   const claw_cap_call_context_t *ctx,
                                   char *output,
                                   size_t output_size)
{
    cap_agent_request_t request;
    cJSON *root;
    esp_err_t err;

    if (!input_json || !output || output_size == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    output[0] = '\0';
    root = cJSON_Parse(input_json);
    if (!root) {
        cap_agent_write_error(output, output_size, ESP_ERR_INVALID_ARG, "invalid input JSON");
        return ESP_ERR_INVALID_ARG;
    }
    err = cap_agent_parse_request(root, &request);
    if (err != ESP_OK) {
        cJSON_Delete(root);
        cap_agent_write_error(output,
                              output_size,
                              err,
                              "expected /session command or explicit session, input, or control operation");
        return err;
    }

    if (request.kind == CAP_AGENT_REQUEST_MESSAGE) {
        if (cap_agent_session_command_matches(request.text)) {
            err = cap_agent_session_command_execute_message(request.text,
                                                            ctx,
                                                            output,
                                                            output_size);
        } else {
            err = ESP_ERR_INVALID_ARG;
            cap_agent_write_error(
                output,
                output_size,
                err,
                "ordinary agent messages require an explicit session and input");
        }
    } else if (request.kind <= CAP_AGENT_REQUEST_SESSION_LIST) {
        err = cap_agent_execute_session(&request, output, output_size);
    } else if (request.kind == CAP_AGENT_REQUEST_INPUT) {
        err = cap_agent_execute_input(&request, ctx, output, output_size);
    } else {
        err = cap_agent_execute_control(&request, output, output_size);
    }
    cJSON_Delete(root);
    if (err != ESP_OK && output[0] == '\0') {
        cap_agent_write_error(output, output_size, err, esp_err_to_name(err));
    }
    return err;
}

/* System-only entry point: it is callable by Router/application code but is
 * intentionally hidden from the model's tool list. */
static const claw_cap_descriptor_t s_agent_descriptors[] = {
    {
        .id = CAP_AGENT_CAP_ID,
        .name = CAP_AGENT_CAP_ID,
        .family = "agent",
        .description = "Submit to explicit AgentSystem sessions and handle /session commands.",
        .kind = CLAW_CAP_KIND_CALLABLE,
        .cap_flags = 0,
        .input_schema_json = s_agent_input_schema,
        .execute = cap_agent_execute,
    },
};

static const claw_cap_group_t s_agent_group = {
    .group_id = "cap_agent",
    .descriptors = s_agent_descriptors,
    .descriptor_count = sizeof(s_agent_descriptors) / sizeof(s_agent_descriptors[0]),
};

esp_err_t cap_agent_register_group(void)
{
    if (claw_cap_group_exists(s_agent_group.group_id)) {
        return ESP_OK;
    }
    return claw_cap_register_group(&s_agent_group);
}
