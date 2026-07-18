/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "cap_agent_session_command.h"

#include <inttypes.h>
#include <stdbool.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "cap_agent_reply.h"
#include "claw_agent.h"
#include "claw_im_session.h"
#include "esp_log.h"

static const char *TAG = "cap_agent_session";

#define CAP_AGENT_SESSION_LIST_RETRIES 3

static const char s_session_command_usage[] =
    "Usage:\n"
    "/session new             Create and switch to a persistent session\n"
    "/session list            List global numeric sessions\n"
    "/session switch <id>     Switch this chat to a session\n"
    "/session delete <id>     Delete a non-current session\n"
    "\n"
    "<id> is the global numeric AgentSystem session id.";

typedef enum {
    CAP_AGENT_SESSION_COMMAND_NEW,
    CAP_AGENT_SESSION_COMMAND_LIST,
    CAP_AGENT_SESSION_COMMAND_SWITCH,
    CAP_AGENT_SESSION_COMMAND_DELETE,
} cap_agent_session_command_kind_t;

typedef struct {
    cap_agent_session_command_kind_t kind;
    uint32_t session_id;
} cap_agent_session_command_t;

static bool cap_agent_session_is_ascii_space(char ch)
{
    return ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r' ||
           ch == '\f' || ch == '\v';
}

static const char *cap_agent_session_skip_ascii_space(const char *text)
{
    if (!text) {
        return "";
    }
    while (*text && cap_agent_session_is_ascii_space(*text)) {
        text++;
    }
    return text;
}

static void cap_agent_session_write(char *output,
                                    size_t output_size,
                                    const char *message)
{
    if (output && output_size > 0) {
        snprintf(output, output_size, "%s", message ? message : "");
    }
}

static void cap_agent_session_write_format(char *output,
                                           size_t output_size,
                                           const char *format,
                                           ...)
{
    va_list args;

    if (!output || output_size == 0 || !format) {
        return;
    }
    va_start(args, format);
    vsnprintf(output, output_size, format, args);
    va_end(args);
}

static esp_err_t cap_agent_session_append(char *output,
                                          size_t output_size,
                                          size_t *offset,
                                          const char *format,
                                          ...)
{
    va_list args;
    int written;

    if (!output || output_size == 0 || !offset || *offset >= output_size) {
        return ESP_ERR_INVALID_ARG;
    }
    va_start(args, format);
    written = vsnprintf(output + *offset, output_size - *offset, format, args);
    va_end(args);
    if (written < 0) {
        return ESP_FAIL;
    }
    if ((size_t)written >= output_size - *offset) {
        *offset = output_size - 1;
        return ESP_ERR_INVALID_SIZE;
    }
    *offset += (size_t)written;
    return ESP_OK;
}

static esp_err_t cap_agent_session_read_token(const char **cursor,
                                              char *token,
                                              size_t token_size,
                                              bool *out_has_token)
{
    const char *start;
    size_t length = 0;

    if (!cursor || !*cursor || !token || token_size == 0 || !out_has_token) {
        return ESP_ERR_INVALID_ARG;
    }
    token[0] = '\0';
    *out_has_token = false;
    start = cap_agent_session_skip_ascii_space(*cursor);
    while (start[length] && !cap_agent_session_is_ascii_space(start[length])) {
        length++;
    }
    if (length == 0) {
        *cursor = start;
        return ESP_OK;
    }
    if (length >= token_size) {
        return ESP_ERR_INVALID_SIZE;
    }
    memcpy(token, start, length);
    token[length] = '\0';
    *cursor = start + length;
    *out_has_token = true;
    return ESP_OK;
}

static esp_err_t cap_agent_session_parse_id(const char *token,
                                            uint32_t *out_session_id)
{
    uint32_t value = 0;

    if (!token || !token[0] || !out_session_id) {
        return ESP_ERR_INVALID_ARG;
    }
    for (const char *cursor = token; *cursor; cursor++) {
        uint32_t digit;

        if (*cursor < '0' || *cursor > '9') {
            return ESP_ERR_INVALID_ARG;
        }
        digit = (uint32_t)(*cursor - '0');
        if (value > (UINT32_MAX - digit) / 10U) {
            return ESP_ERR_INVALID_ARG;
        }
        value = value * 10U + digit;
    }
    if (value == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    *out_session_id = value;
    return ESP_OK;
}

static esp_err_t cap_agent_session_parse_command(
    const char *command_text,
    cap_agent_session_command_t *out_command)
{
    const char *cursor = command_text;
    char operation[8];
    char argument[16];
    char extra[2];
    bool has_operation;
    bool has_argument;
    bool has_extra;
    esp_err_t err;

    if (!out_command) {
        return ESP_ERR_INVALID_ARG;
    }
    memset(out_command, 0, sizeof(*out_command));
    err = cap_agent_session_read_token(&cursor,
                                       operation,
                                       sizeof(operation),
                                       &has_operation);
    if (err != ESP_OK || !has_operation) {
        return ESP_ERR_INVALID_ARG;
    }
    err = cap_agent_session_read_token(&cursor,
                                       argument,
                                       sizeof(argument),
                                       &has_argument);
    if (err != ESP_OK) {
        return err;
    }
    err = cap_agent_session_read_token(&cursor, extra, sizeof(extra), &has_extra);
    if (err != ESP_OK || has_extra) {
        return ESP_ERR_INVALID_ARG;
    }

    if (strcmp(operation, "new") == 0) {
        out_command->kind = CAP_AGENT_SESSION_COMMAND_NEW;
        return has_argument ? ESP_ERR_INVALID_ARG : ESP_OK;
    }
    if (strcmp(operation, "list") == 0) {
        out_command->kind = CAP_AGENT_SESSION_COMMAND_LIST;
        return has_argument ? ESP_ERR_INVALID_ARG : ESP_OK;
    }
    if (strcmp(operation, "switch") == 0) {
        out_command->kind = CAP_AGENT_SESSION_COMMAND_SWITCH;
    } else if (strcmp(operation, "delete") == 0) {
        out_command->kind = CAP_AGENT_SESSION_COMMAND_DELETE;
    } else {
        return ESP_ERR_INVALID_ARG;
    }
    return has_argument ? cap_agent_session_parse_id(argument,
                                                      &out_command->session_id) :
                          ESP_ERR_INVALID_ARG;
}

static esp_err_t cap_agent_session_load_ids(uint32_t **out_ids,
                                            size_t *out_count)
{
    esp_err_t err = ESP_ERR_INVALID_SIZE;

    if (!out_ids || !out_count) {
        return ESP_ERR_INVALID_ARG;
    }
    *out_ids = NULL;
    *out_count = 0;
    for (size_t attempt = 0; attempt < CAP_AGENT_SESSION_LIST_RETRIES; attempt++) {
        uint32_t *ids = NULL;
        size_t count = 0;

        err = claw_agent_session_list(NULL, 0, &count);
        if (err != ESP_OK && err != ESP_ERR_INVALID_SIZE) {
            return err;
        }
        if (count == 0) {
            return ESP_OK;
        }
        if (count > SIZE_MAX / sizeof(*ids)) {
            return ESP_ERR_INVALID_SIZE;
        }
        ids = calloc(count, sizeof(*ids));
        if (!ids) {
            return ESP_ERR_NO_MEM;
        }
        err = claw_agent_session_list(ids, count, &count);
        if (err == ESP_OK) {
            *out_ids = ids;
            *out_count = count;
            return ESP_OK;
        }
        free(ids);
        if (err != ESP_ERR_INVALID_SIZE) {
            return err;
        }
    }
    return err;
}

static bool cap_agent_session_contains(const uint32_t *ids,
                                       size_t count,
                                       uint32_t session_id)
{
    for (size_t i = 0; i < count; i++) {
        if (ids[i] == session_id) {
            return true;
        }
    }
    return false;
}

static esp_err_t cap_agent_session_get_current(
    const claw_cap_call_context_t *ctx,
    uint32_t *out_session_id)
{
    esp_err_t err = claw_im_session_get_selected(ctx->channel,
                                                 ctx->chat_id,
                                                 out_session_id);

    if (err == ESP_ERR_NOT_FOUND) {
        *out_session_id = 0;
        return ESP_OK;
    }
    return err;
}

static esp_err_t cap_agent_session_select_open(
    const claw_cap_call_context_t *ctx,
    uint32_t session_id)
{
    bool opened_here = false;
    esp_err_t err;

    if (!cap_agent_reply_is_attached(session_id)) {
        err = claw_agent_session_open(session_id);
        if (err == ESP_OK) {
            opened_here = true;
        } else if (err == ESP_ERR_INVALID_STATE) {
            if (!cap_agent_reply_is_attached(session_id)) {
                /* IM may have opened the stream while publishing the inbound
                 * event. Only adopt that stream when the IM map proves it is
                 * ours; otherwise another C API consumer owns receive(). */
                if (!claw_im_session_is_managed(session_id)) {
                    return err;
                }
                err = cap_agent_reply_ensure(session_id);
                if (err != ESP_OK) {
                    return err;
                }
            }
        } else {
            return err;
        }
    }
    if (opened_here) {
        err = cap_agent_reply_ensure(session_id);
        if (err != ESP_OK) {
            (void)claw_agent_session_close(session_id);
            return err;
        }
    }
    err = claw_im_session_select(ctx->channel, ctx->chat_id, session_id);
    if (err != ESP_OK) {
        return err;
    }
    return claw_im_session_mark_open(session_id);
}

static esp_err_t cap_agent_session_create_and_select(
    const claw_cap_call_context_t *ctx,
    uint32_t *out_session_id)
{
    uint32_t session_id;
    esp_err_t err = claw_agent_session_create(
        CLAW_AGENT_SESSION_PERSISTENCE_PERSISTENT,
        &session_id);

    if (err != ESP_OK) {
        return err;
    }
    *out_session_id = session_id;
    err = cap_agent_session_select_open(ctx, session_id);
    if (err != ESP_OK) {
        ESP_LOGW(TAG,
                 "created session=%" PRIu32 " but failed to select it: %s",
                 session_id,
                 esp_err_to_name(err));
        (void)claw_agent_session_delete(session_id);
        return err;
    }
    return ESP_OK;
}

static esp_err_t cap_agent_session_write_list(
    const claw_cap_call_context_t *ctx,
    char *output,
    size_t output_size)
{
    uint32_t *ids = NULL;
    uint32_t current = 0;
    size_t count = 0;
    size_t offset = 0;
    esp_err_t err;

    err = cap_agent_session_load_ids(&ids, &count);
    if (err != ESP_OK) {
        return err;
    }
    err = cap_agent_session_get_current(ctx, &current);
    if (err != ESP_OK) {
        free(ids);
        return err;
    }
    err = cap_agent_session_append(output, output_size, &offset, "Sessions:");
    if (err == ESP_OK && count == 0) {
        err = cap_agent_session_append(output, output_size, &offset, " none");
    }
    for (size_t i = 0; err == ESP_OK && i < count; i++) {
        err = cap_agent_session_append(output,
                                       output_size,
                                       &offset,
                                       "\n* %" PRIu32 "%s",
                                       ids[i],
                                       ids[i] == current ? " (current)" : "");
    }
    free(ids);
    return err;
}

static bool cap_agent_session_context_valid(const claw_cap_call_context_t *ctx)
{
    return ctx && ctx->channel && ctx->channel[0] &&
           ctx->chat_id && ctx->chat_id[0];
}

static void cap_agent_session_write_failure(const char *operation,
                                            uint32_t session_id,
                                            esp_err_t err,
                                            char *output,
                                            size_t output_size)
{
    ESP_LOGW(TAG,
             "%s session=%" PRIu32 " failed: %s",
             operation,
             session_id,
             esp_err_to_name(err));
    if (err == ESP_ERR_NOT_FOUND && session_id != 0) {
        cap_agent_session_write_format(
            output,
            output_size,
            "Cannot %s session %" PRIu32
            ": no such session. Send /session list to list sessions.",
            operation,
            session_id);
    } else {
        cap_agent_session_write_format(output,
                                       output_size,
                                       "Session command failed: cannot %s session (%s).",
                                       operation,
                                       esp_err_to_name(err));
    }
}

static esp_err_t cap_agent_session_run_command(
    const cap_agent_session_command_t *command,
    const claw_cap_call_context_t *ctx,
    char *output,
    size_t output_size)
{
    uint32_t *ids = NULL;
    uint32_t current = 0;
    uint32_t session_id = command->session_id;
    size_t count = 0;
    esp_err_t err;

    if (!cap_agent_session_context_valid(ctx)) {
        cap_agent_session_write(output,
                                output_size,
                                "Session command failed: missing chat context.");
        return ESP_OK;
    }

    switch (command->kind) {
    case CAP_AGENT_SESSION_COMMAND_NEW:
        err = cap_agent_session_create_and_select(ctx, &session_id);
        if (err == ESP_OK) {
            cap_agent_session_write_format(output,
                                           output_size,
                                           "Started a new session: %" PRIu32,
                                           session_id);
        } else {
            cap_agent_session_write_failure("create", session_id, err, output, output_size);
        }
        return ESP_OK;
    case CAP_AGENT_SESSION_COMMAND_LIST:
        err = cap_agent_session_write_list(ctx, output, output_size);
        if (err != ESP_OK) {
            cap_agent_session_write_failure("list", 0, err, output, output_size);
        }
        return ESP_OK;
    case CAP_AGENT_SESSION_COMMAND_SWITCH:
        err = cap_agent_session_load_ids(&ids, &count);
        if (err == ESP_OK && !cap_agent_session_contains(ids, count, session_id)) {
            err = ESP_ERR_NOT_FOUND;
        }
        free(ids);
        if (err == ESP_OK) {
            err = cap_agent_session_select_open(ctx, session_id);
        }
        if (err == ESP_OK) {
            cap_agent_session_write_format(output,
                                           output_size,
                                           "Switched to session: %" PRIu32,
                                           session_id);
        } else {
            cap_agent_session_write_failure("switch to",
                                            session_id,
                                            err,
                                            output,
                                            output_size);
        }
        return ESP_OK;
    case CAP_AGENT_SESSION_COMMAND_DELETE:
        err = cap_agent_session_get_current(ctx, &current);
        if (err == ESP_OK && current == session_id) {
            cap_agent_session_write_format(
                output,
                output_size,
                "Cannot delete the current session %" PRIu32
                ". Switch to another session first.",
                session_id);
            return ESP_OK;
        }
        if (err == ESP_OK) {
            err = claw_agent_session_delete(session_id);
        }
        if (err == ESP_OK) {
            (void)claw_im_session_forget(session_id);
            cap_agent_session_write_format(output,
                                           output_size,
                                           "Deleted session: %" PRIu32,
                                           session_id);
        } else {
            cap_agent_session_write_failure("delete",
                                            session_id,
                                            err,
                                            output,
                                            output_size);
        }
        return ESP_OK;
    default:
        return ESP_ERR_INVALID_ARG;
    }
}

bool cap_agent_session_command_matches(const char *message)
{
    static const char prefix[] = "/session";
    const char *trimmed = cap_agent_session_skip_ascii_space(message);
    const char *remainder;

    if (strncmp(trimmed, prefix, sizeof(prefix) - 1) != 0) {
        return false;
    }
    remainder = trimmed + sizeof(prefix) - 1;
    return *remainder == '\0' || cap_agent_session_is_ascii_space(*remainder);
}

esp_err_t cap_agent_session_command_execute_message(
    const char *message,
    const claw_cap_call_context_t *ctx,
    char *output,
    size_t output_size)
{
    static const char prefix[] = "/session";
    const char *command_text;
    cap_agent_session_command_t command;

    if (!output || output_size == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    output[0] = '\0';
    if (!cap_agent_session_command_matches(message)) {
        return ESP_ERR_INVALID_ARG;
    }
    command_text = cap_agent_session_skip_ascii_space(message) + sizeof(prefix) - 1;
    command_text = cap_agent_session_skip_ascii_space(command_text);
    if (cap_agent_session_parse_command(command_text, &command) != ESP_OK) {
        cap_agent_session_write(output, output_size, s_session_command_usage);
        return ESP_OK;
    }
    return cap_agent_session_run_command(&command, ctx, output, output_size);
}
