/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "app_claw_cli.h"
#include "app_claw.h"

#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#if CONFIG_APP_CLAW_CAP_IM_QQ
#include "cap_im_qq.h"
#include "cmd_cap_im_qq.h"
#endif
#if CONFIG_APP_CLAW_CAP_IM_FEISHU
#include "cmd_cap_im_feishu.h"
#endif
#if CONFIG_APP_CLAW_CAP_IM_TG
#include "cmd_cap_im_tg.h"
#endif
#if CONFIG_APP_CLAW_CAP_IM_WECHAT
#include "cmd_cap_im_wechat.h"
#endif
#if CONFIG_APP_CLAW_CAP_LUA
#include "cmd_cap_lua.h"
#endif
#if CONFIG_APP_CLAW_CAP_LLM_INSPECT
#include "cmd_cap_llm_inspect.h"
#endif
#if CONFIG_APP_CLAW_CAP_ROUTER_MGR
#include "cmd_cap_router_mgr.h"
#endif
#if CONFIG_APP_CLAW_CAP_SCHEDULER
#include "cmd_cap_scheduler.h"
#endif
#if CONFIG_APP_CLAW_CAP_WEB_SEARCH
#include "cmd_cap_web_search.h"
#endif
#include "claw_agent.h"
#include "claw_cap.h"
#include "claw_event_publisher.h"
#include "claw_event_router.h"
#include "cJSON.h"
#include "esp_console.h"
#include "esp_log.h"

static const char *TAG = "app_claw_cli";
static const size_t CAP_OUTPUT_BUF_SIZE = 1024;

static uint32_t s_current_session_id;
static uint32_t s_active_turn_id;
static uint32_t s_pending_input_request_id;
static uint32_t *s_cli_session_ids;
static size_t s_cli_session_count;
static size_t s_cli_session_capacity;

static bool cli_session_is_owned(uint32_t session_id)
{
    for (size_t i = 0; i < s_cli_session_count; i++) {
        if (s_cli_session_ids[i] == session_id) {
            return true;
        }
    }
    return false;
}

static esp_err_t remember_cli_session(uint32_t session_id)
{
    uint32_t *resized;
    size_t next_capacity;

    if (session_id == 0) {
        return ESP_ERR_INVALID_ARG;
    }
    if (cli_session_is_owned(session_id)) {
        return ESP_OK;
    }
    if (s_cli_session_count == s_cli_session_capacity) {
        next_capacity = s_cli_session_capacity == 0 ? 4 : s_cli_session_capacity * 2;
        if (next_capacity < s_cli_session_capacity ||
                next_capacity > SIZE_MAX / sizeof(*s_cli_session_ids)) {
            return ESP_ERR_NO_MEM;
        }
        resized = realloc(s_cli_session_ids, next_capacity * sizeof(*s_cli_session_ids));
        if (!resized) {
            return ESP_ERR_NO_MEM;
        }
        s_cli_session_ids = resized;
        s_cli_session_capacity = next_capacity;
    }
    s_cli_session_ids[s_cli_session_count++] = session_id;
    return ESP_OK;
}

static void forget_cli_session(uint32_t session_id)
{
    for (size_t i = 0; i < s_cli_session_count; i++) {
        if (s_cli_session_ids[i] == session_id) {
            s_cli_session_ids[i] = s_cli_session_ids[s_cli_session_count - 1];
            s_cli_session_count--;
            return;
        }
    }
}

static char *join_prompt_args(int argc, char **argv)
{
    char *prompt = NULL;
    size_t prompt_len = 0;
    int i;

    if (argc < 2) {
        return NULL;
    }

    for (i = 1; i < argc; i++) {
        prompt_len += strlen(argv[i]) + 1;
    }

    prompt = calloc(1, prompt_len + 1);
    if (!prompt) {
        return NULL;
    }

    for (i = 1; i < argc; i++) {
        if (i > 1) {
            strcat(prompt, " ");
        }
        strcat(prompt, argv[i]);
    }

    return prompt;
}

static char *join_args_from(int argc, char **argv, int start_index)
{
    char *prompt = NULL;
    size_t prompt_len = 0;
    int i;

    if (argc <= start_index) {
        return NULL;
    }

    for (i = start_index; i < argc; i++) {
        prompt_len += strlen(argv[i]) + 1;
    }

    prompt = calloc(1, prompt_len + 1);
    if (!prompt) {
        return NULL;
    }

    for (i = start_index; i < argc; i++) {
        if (i > start_index) {
            strcat(prompt, " ");
        }
        strcat(prompt, argv[i]);
    }

    return prompt;
}

typedef enum {
    CLI_TURN_DONE = 0,
    CLI_TURN_FAILED = 1,
    CLI_TURN_INPUT_PENDING = 2,
} cli_turn_result_t;

static esp_err_t create_ephemeral_session(uint32_t *out_session_id)
{
    esp_err_t err;

    if (!out_session_id) {
        return ESP_ERR_INVALID_ARG;
    }
    err = claw_agent_session_create(CLAW_AGENT_SESSION_PERSISTENCE_EPHEMERAL,
                                    out_session_id);
    if (err != ESP_OK) {
        return err;
    }
    err = claw_agent_session_open(*out_session_id);
    if (err != ESP_OK) {
        claw_agent_session_delete(*out_session_id);
        *out_session_id = 0;
    }
    return err;
}

static esp_err_t close_session_stream(uint32_t session_id)
{
    esp_err_t err;

    if (session_id == 0) {
        return ESP_OK;
    }
    err = claw_agent_session_close(session_id);
    if (err != ESP_OK) {
        return err;
    }

    for (int i = 0; i < 20; i++) {
        claw_agent_event_t event = {0};

        err = claw_agent_session_receive(session_id, &event, 250);
        if (err == ESP_ERR_TIMEOUT) {
            continue;
        }
        if (err != ESP_OK) {
            return err;
        }
        bool closed = event.kind == CLAW_AGENT_EVENT_KIND_CLOSED;
        claw_agent_event_free(&event);
        if (closed) {
            return ESP_OK;
        }
    }
    return ESP_ERR_TIMEOUT;
}

static esp_err_t create_cli_session(uint32_t *out_session_id)
{
    esp_err_t err = create_ephemeral_session(out_session_id);

    if (err != ESP_OK) {
        return err;
    }
    err = remember_cli_session(*out_session_id);
    if (err != ESP_OK) {
        close_session_stream(*out_session_id);
        claw_agent_session_delete(*out_session_id);
        *out_session_id = 0;
    }
    return err;
}

static esp_err_t ensure_current_session(void)
{
    esp_err_t err;

    if (s_current_session_id != 0) {
        return ESP_OK;
    }
    err = create_cli_session(&s_current_session_id);
    if (err == ESP_OK) {
        printf("Created ephemeral session %" PRIu32 "\n", s_current_session_id);
    }
    return err;
}

static cli_turn_result_t receive_and_print(uint32_t session_id, bool track_input)
{
    uint32_t target_turn_id = track_input ? s_active_turn_id : 0;
    bool output_open = false;
    bool saw_error = false;

    while (true) {
        claw_agent_event_t event = {0};
        esp_err_t err = claw_agent_session_receive(session_id, &event, 130000);

        if (err != ESP_OK) {
            printf("receive failed: %s\n", esp_err_to_name(err));
            return CLI_TURN_FAILED;
        }

        switch (event.kind) {
        case CLAW_AGENT_EVENT_KIND_TURN_STARTED:
            if (event.data.turn_started.origin == CLAW_AGENT_TURN_ORIGIN_USER &&
                    target_turn_id == 0) {
                target_turn_id = event.data.turn_started.turn_id;
                if (track_input) {
                    s_active_turn_id = target_turn_id;
                }
            }
            break;
        case CLAW_AGENT_EVENT_KIND_OUTPUT_DELTA:
            if (!output_open) {
                printf("\nassistant> ");
                output_open = true;
            }
            printf("%s", event.data.text_delta.text ? event.data.text_delta.text : "");
            fflush(stdout);
            break;
        case CLAW_AGENT_EVENT_KIND_OUTPUT_END:
            if (output_open) {
                printf("\n");
                output_open = false;
            }
            break;
        case CLAW_AGENT_EVENT_KIND_TOOL_CALL:
            if (output_open) {
                printf("\n");
                output_open = false;
            }
            printf("[tool] %s\n",
                   event.data.tool_call.name ? event.data.tool_call.name : "unknown");
            break;
        case CLAW_AGENT_EVENT_KIND_INPUT_REQUESTED:
            if (output_open) {
                printf("\n");
                output_open = false;
            }
            printf("input requested id=%" PRIu32 ": %s\n",
                   event.data.input_requested.request_id,
                   event.data.input_requested.reason ? event.data.input_requested.reason : "");
            if (track_input) {
                s_pending_input_request_id = event.data.input_requested.request_id;
                printf("Use: respond <answer>\n");
            }
            claw_agent_event_free(&event);
            return CLI_TURN_INPUT_PENDING;
        case CLAW_AGENT_EVENT_KIND_ERROR:
            if (output_open) {
                printf("\n");
                output_open = false;
            }
            printf("error> %s\n", event.data.error.message ? event.data.error.message : "unknown");
            saw_error = true;
            break;
        case CLAW_AGENT_EVENT_KIND_TURN_ENDED:
            if (target_turn_id != 0 && event.data.turn_ended.turn_id == target_turn_id) {
                if (output_open) {
                    printf("\n");
                }
                if (track_input) {
                    s_active_turn_id = 0;
                    s_pending_input_request_id = 0;
                }
                claw_agent_event_free(&event);
                return saw_error ? CLI_TURN_FAILED : CLI_TURN_DONE;
            }
            break;
        case CLAW_AGENT_EVENT_KIND_CLOSED:
            if (track_input && s_current_session_id == session_id) {
                s_current_session_id = 0;
                s_active_turn_id = 0;
                s_pending_input_request_id = 0;
            }
            claw_agent_event_free(&event);
            printf("session closed\n");
            return CLI_TURN_FAILED;
        default:
            break;
        }
        claw_agent_event_free(&event);
    }
}

static cli_turn_result_t submit_and_print(const char *prompt,
                                          uint32_t session_id,
                                          bool track_input)
{
    esp_err_t err;

    printf("Submitting [session=%" PRIu32 "]...\n", session_id);
    err = claw_agent_session_submit(session_id, prompt);
    if (err != ESP_OK) {
        printf("submit failed: %s\n", esp_err_to_name(err));
        return CLI_TURN_FAILED;
    }
    return receive_and_print(session_id, track_input);
}

static int cmd_ask(int argc, char **argv)
{
    char *prompt = NULL;
    esp_err_t err;

    if (argc < 2) {
        printf("Usage: ask <prompt>\n");
        return 1;
    }

    prompt = join_prompt_args(argc, argv);
    if (!prompt) {
        printf("Out of memory\n");
        return 1;
    }

    err = ensure_current_session();
    if (err != ESP_OK) {
        printf("session create failed: %s\n", esp_err_to_name(err));
        free(prompt);
        return 1;
    }

    argc = submit_and_print(prompt, s_current_session_id, true) == CLI_TURN_FAILED;
    free(prompt);
    return argc;
}

static int cmd_ask_once(int argc, char **argv)
{
    char *prompt = NULL;
    uint32_t session_id = 0;
    cli_turn_result_t turn_result;
    esp_err_t err;
    int rc;

    if (argc < 2) {
        printf("Usage: ask_once <prompt>\n");
        return 1;
    }

    prompt = join_prompt_args(argc, argv);
    if (!prompt) {
        printf("Out of memory\n");
        return 1;
    }

    err = create_ephemeral_session(&session_id);
    if (err != ESP_OK) {
        printf("session create failed: %s\n", esp_err_to_name(err));
        free(prompt);
        return 1;
    }

    turn_result = submit_and_print(prompt, session_id, false);
    if (turn_result == CLI_TURN_INPUT_PENDING) {
        printf("ask_once cannot leave an input request pending; cancelling turn\n");
        claw_agent_session_cancel(session_id);
    }
    rc = turn_result == CLI_TURN_DONE ? 0 : 1;
    err = close_session_stream(session_id);
    if (err != ESP_OK) {
        printf("session close failed: %s\n", esp_err_to_name(err));
        rc = 1;
    }
    err = claw_agent_session_delete(session_id);
    if (err != ESP_OK) {
        printf("session delete failed: %s\n", esp_err_to_name(err));
        rc = 1;
    }
    free(prompt);
    return rc;
}

static bool parse_session_id(const char *value, uint32_t *out_session_id)
{
    char *end = NULL;
    unsigned long parsed;

    if (!value || !value[0] || !out_session_id) {
        return false;
    }
    parsed = strtoul(value, &end, 10);
    if (!end || *end != '\0' || parsed == 0 || parsed > UINT32_MAX) {
        return false;
    }
    *out_session_id = (uint32_t)parsed;
    return true;
}

static int cmd_session(int argc, char **argv)
{
    uint32_t next_session_id = 0;
    uint32_t previous_session_id = s_current_session_id;
    bool created_new = false;
    esp_err_t err;

    if (argc == 1) {
        if (s_current_session_id == 0) {
            printf("Current session: none (next ask creates an ephemeral session)\n");
        } else {
            printf("Current session: %" PRIu32 " (ephemeral)\n", s_current_session_id);
        }
        return 0;
    }

    if (argc != 2) {
        printf("Usage: session [new|id]\n");
        return 1;
    }

    if (strcmp(argv[1], "new") == 0) {
        err = create_cli_session(&next_session_id);
        created_new = err == ESP_OK;
    } else if (parse_session_id(argv[1], &next_session_id)) {
        if (next_session_id == previous_session_id) {
            printf("Current session: %" PRIu32 "\n", s_current_session_id);
            return 0;
        }
        if (!cli_session_is_owned(next_session_id)) {
            printf("session %" PRIu32 " is not a CLI-created ephemeral session\n",
                   next_session_id);
            return 1;
        }
        err = claw_agent_session_open(next_session_id);
    } else {
        printf("session id must be a non-zero integer or 'new'\n");
        return 1;
    }
    if (err != ESP_OK) {
        printf("session open failed: %s\n", esp_err_to_name(err));
        return 1;
    }

    if (previous_session_id != 0) {
        err = close_session_stream(previous_session_id);
        if (err != ESP_OK) {
            printf("current session close failed: %s\n", esp_err_to_name(err));
            close_session_stream(next_session_id);
            if (created_new) {
                claw_agent_session_delete(next_session_id);
                forget_cli_session(next_session_id);
            }
            return 1;
        }
    }

    s_current_session_id = next_session_id;
    s_active_turn_id = 0;
    s_pending_input_request_id = 0;
    printf("Switched to session: %" PRIu32 "\n", s_current_session_id);
    return 0;
}

static int cmd_respond(int argc, char **argv)
{
    char *response = NULL;
    esp_err_t err;

    if (argc < 2) {
        printf("Usage: respond <answer>\n");
        return 1;
    }
    if (s_current_session_id == 0 || s_pending_input_request_id == 0) {
        printf("No pending input request\n");
        return 1;
    }

    response = join_prompt_args(argc, argv);
    if (!response) {
        printf("Out of memory\n");
        return 1;
    }
    err = claw_agent_session_respond(s_current_session_id,
                                     s_pending_input_request_id,
                                     response);
    free(response);
    if (err != ESP_OK) {
        printf("respond failed: %s\n", esp_err_to_name(err));
        return 1;
    }
    s_pending_input_request_id = 0;
    return receive_and_print(s_current_session_id, true) == CLI_TURN_FAILED;
}

static int cmd_cap_list(int argc, char **argv)
{
    claw_cap_list_t list;
    size_t i;

    (void)argc;
    (void)argv;

    list = claw_cap_list();
    if (list.count == 0) {
        printf("No capabilities registered\n");
        return 0;
    }

    for (i = 0; i < list.count; i++) {
        const claw_cap_descriptor_t *item = &list.items[i];

        printf("%s [%s] %s\n",
               item->name,
               item->family ? item->family : "cap",
               item->description ? item->description : "");
    }

    return 0;
}

static int cmd_cap_call(int argc, char **argv)
{
    char *output = NULL;
    char session_id[16] = {0};
    esp_err_t err;
    claw_cap_call_context_t ctx = {
        .caller = CLAW_CAP_CALLER_CONSOLE,
    };

    if (argc < 3) {
        printf("Usage: cap_call <name> <json>\n");
        return 1;
    }

    if (s_current_session_id != 0) {
        snprintf(session_id, sizeof(session_id), "%" PRIu32, s_current_session_id);
        ctx.session_id = session_id;
    }

    {
        cJSON *json = cJSON_Parse(argv[2]);

        if (!json) {
            printf("invalid json\n");
            return 1;
        }
        cJSON_Delete(json);
    }

    output = calloc(1, CAP_OUTPUT_BUF_SIZE);
    if (!output) {
        printf("Out of memory\n");
        return 1;
    }

    err = claw_cap_call(argv[1], argv[2], &ctx, output, CAP_OUTPUT_BUF_SIZE);
    if (err == ESP_OK) {
        printf("%s\n", output);
    } else {
        printf("%s\n", output[0] ? output : esp_err_to_name(err));
    }

    free(output);
    return err == ESP_OK ? 0 : 1;
}

static int cmd_cap_groups(int argc, char **argv)
{
    claw_cap_group_list_t list;
    size_t i;

    (void)argc;
    (void)argv;

    list = claw_cap_list_groups();
    if (list.count == 0) {
        printf("No cap groups loaded\n");
        return 0;
    }

    for (i = 0; i < list.count; i++) {
        const claw_cap_group_info_t *item = &list.items[i];

        printf("%s state=%s descriptors=%u plugin=%s version=%s\n",
               item->group_id ? item->group_id : "(null)",
               claw_cap_state_to_string(item->state),
               (unsigned)item->descriptor_count,
               item->plugin_name ? item->plugin_name : "-",
               item->version ? item->version : "-");
    }

    return 0;
}

static int cmd_cap_enable(int argc, char **argv)
{
    esp_err_t err;

    if (argc != 2) {
        printf("Usage: cap_enable <group_id>\n");
        return 1;
    }

    err = claw_cap_enable_group(argv[1]);
    if (err != ESP_OK) {
        printf("cap_enable failed: %s\n", esp_err_to_name(err));
        return 1;
    }

    printf("enabled %s\n", argv[1]);
    return 0;
}

static int cmd_cap_disable(int argc, char **argv)
{
    esp_err_t err;

    if (argc != 2) {
        printf("Usage: cap_disable <group_id>\n");
        return 1;
    }

    err = claw_cap_disable_group(argv[1]);
    if (err != ESP_OK) {
        printf("cap_disable failed: %s\n", esp_err_to_name(err));
        return 1;
    }

    printf("disabled %s\n", argv[1]);
    return 0;
}

static int cmd_cap_unload(int argc, char **argv)
{
    esp_err_t err;

    if (argc != 2) {
        printf("Usage: cap_unload <group_id>\n");
        return 1;
    }

    err = claw_cap_unregister_group(argv[1], 10000);
    if (err != ESP_OK) {
        printf("cap_unload failed: %s\n", esp_err_to_name(err));
        return 1;
    }

    printf("unloaded %s\n", argv[1]);
    return 0;
}

static int cmd_cap_load(int argc, char **argv)
{
    esp_err_t err;

    if (argc != 2) {
        printf("Usage: cap_load <plugin>\n");
        return 1;
    }

#if CONFIG_APP_CLAW_CAP_IM_QQ
    if (strcmp(argv[1], "qq") == 0 || strcmp(argv[1], "cap_im_qq") == 0) {
        err = cap_im_qq_register_group();
    } else
#endif
    {
        printf("unknown plugin: %s\n", argv[1]);
        return 1;
    }

    if (err != ESP_OK) {
        printf("cap_load failed: %s\n", esp_err_to_name(err));
        return 1;
    }

    printf("loaded %s\n", argv[1]);
    return 0;
}

static int cmd_cap(int argc, char **argv)
{
    if (argc < 2) {
        printf("Usage: cap <list|call|groups|enable|disable|unload|load> ...\n");
        return 1;
    }

    if (strcmp(argv[1], "list") == 0) {
        return cmd_cap_list(argc - 1, &argv[1]);
    }
    if (strcmp(argv[1], "call") == 0) {
        return cmd_cap_call(argc - 1, &argv[1]);
    }
    if (strcmp(argv[1], "groups") == 0) {
        return cmd_cap_groups(argc - 1, &argv[1]);
    }
    if (strcmp(argv[1], "enable") == 0) {
        return cmd_cap_enable(argc - 1, &argv[1]);
    }
    if (strcmp(argv[1], "disable") == 0) {
        return cmd_cap_disable(argc - 1, &argv[1]);
    }
    if (strcmp(argv[1], "unload") == 0) {
        return cmd_cap_unload(argc - 1, &argv[1]);
    }
    if (strcmp(argv[1], "load") == 0) {
        return cmd_cap_load(argc - 1, &argv[1]);
    }

    printf("Unknown cap subcommand: %s\n", argv[1]);
    printf("Usage: cap <list|call|groups|enable|disable|unload|load> ...\n");
    return 1;
}

static int cmd_auto_reload(int argc, char **argv)
{
    esp_err_t err;

    (void)argc;
    (void)argv;

    err = claw_event_router_reload();
    if (err != ESP_OK) {
        printf("auto_reload failed: %s\n", esp_err_to_name(err));
        return 1;
    }

    printf("automation rules reloaded\n");
    return 0;
}

static int cmd_auto_rules(int argc, char **argv)
{
    char *output = NULL;
    esp_err_t err;

    (void)argc;
    (void)argv;

    output = calloc(1, 4096);
    if (!output) {
        printf("Out of memory\n");
        return 1;
    }

    err = claw_event_router_list_rules_json(output, 4096);
    if (err != ESP_OK) {
        printf("auto_rules failed: %s\n", esp_err_to_name(err));
        free(output);
        return 1;
    }

    printf("%s\n", output);
    free(output);
    return 0;
}

static int cmd_auto_rule(int argc, char **argv)
{
    char *output = NULL;
    esp_err_t err;

    if (argc != 2) {
        printf("Usage: auto_rule <id>\n");
        return 1;
    }

    output = calloc(1, 2048);
    if (!output) {
        printf("Out of memory\n");
        return 1;
    }

    err = claw_event_router_get_rule_json(argv[1], output, 2048);
    if (err != ESP_OK) {
        printf("auto_rule failed: %s\n", esp_err_to_name(err));
        free(output);
        return 1;
    }

    printf("%s\n", output);
    free(output);
    return 0;
}

static int cmd_auto_last(int argc, char **argv)
{
    claw_event_router_result_t result = {0};
    esp_err_t err;

    (void)argc;
    (void)argv;

    err = claw_event_router_get_last_result(&result);
    if (err != ESP_OK) {
        printf("auto_last failed: %s\n", esp_err_to_name(err));
        return 1;
    }

    printf("matched=%s matched_rules=%d action_count=%d failed_actions=%d route=%d handled_at_ms=%" PRId64 "\n",
           result.matched ? "true" : "false",
           result.matched_rules,
           result.action_count,
           result.failed_actions,
           (int)result.route,
           result.handled_at_ms);
    printf("first_rule_id=%s\n", result.first_rule_id[0] ? result.first_rule_id : "-");
    printf("ack=%s\n", result.ack[0] ? result.ack : "-");
    printf("last_error=%s\n", esp_err_to_name(result.last_error));
    return 0;
}

static int cmd_auto_add_rule(int argc, char **argv)
{
    esp_err_t err;

    if (argc != 2) {
        printf("Usage: auto_add_rule <rule_json>\n");
        return 1;
    }

    err = claw_event_router_add_rule_json(argv[1]);
    if (err != ESP_OK) {
        printf("auto_add_rule failed: %s\n", esp_err_to_name(err));
        return 1;
    }

    printf("automation rule added\n");
    return 0;
}

static int cmd_auto_update_rule(int argc, char **argv)
{
    esp_err_t err;

    if (argc != 2) {
        printf("Usage: auto_update_rule <rule_json>\n");
        return 1;
    }

    err = claw_event_router_update_rule_json(argv[1]);
    if (err != ESP_OK) {
        printf("auto_update_rule failed: %s\n", esp_err_to_name(err));
        return 1;
    }

    printf("automation rule updated\n");
    return 0;
}

static int cmd_auto_delete_rule(int argc, char **argv)
{
    esp_err_t err;

    if (argc != 2) {
        printf("Usage: auto_delete_rule <id>\n");
        return 1;
    }

    err = claw_event_router_delete_rule(argv[1]);
    if (err != ESP_OK) {
        printf("auto_delete_rule failed: %s\n", esp_err_to_name(err));
        return 1;
    }

    printf("automation rule deleted\n");
    return 0;
}

static int cmd_auto_emit_message(int argc, char **argv)
{
    char *text = NULL;
    esp_err_t err;

    if (argc < 5) {
        printf("Usage: auto_emit_message <source_cap> <channel> <chat_id> <text>\n");
        return 1;
    }

    text = join_args_from(argc, argv, 4);
    if (!text) {
        printf("Out of memory\n");
        return 1;
    }

    err = claw_event_router_publish_message(argv[1], argv[2], argv[3], text, "console", "cli-msg");
    if (err != ESP_OK) {
        printf("auto_emit_message failed: %s\n", esp_err_to_name(err));
        free(text);
        return 1;
    }

    printf("message event published via %s to %s:%s\n", argv[1], argv[2], argv[3]);
    free(text);
    return 0;
}

static int cmd_auto_emit_trigger(int argc, char **argv)
{
    esp_err_t err;

    if (argc != 5) {
        printf("Usage: auto_emit_trigger <source_cap> <event_type> <event_key> <payload_json>\n");
        return 1;
    }

    {
        cJSON *json = cJSON_Parse(argv[4]);

        if (!json || !cJSON_IsObject(json)) {
            cJSON_Delete(json);
            printf("payload_json must be a JSON object\n");
            return 1;
        }
        cJSON_Delete(json);
    }

    err = claw_event_router_publish_trigger(argv[1], argv[2], argv[3], argv[4]);
    if (err != ESP_OK) {
        printf("auto_emit_trigger failed: %s\n", esp_err_to_name(err));
        return 1;
    }

    printf("trigger event published via %s type=%s key=%s\n", argv[1], argv[2], argv[3]);
    return 0;
}

static int cmd_auto(int argc, char **argv)
{
    if (argc < 2) {
        printf("Usage: auto <reload|rules|rule|add_rule|update_rule|delete_rule|last|emit_message|emit_trigger> ...\n");
        return 1;
    }

    if (strcmp(argv[1], "reload") == 0) {
        return cmd_auto_reload(argc - 1, &argv[1]);
    }
    if (strcmp(argv[1], "rules") == 0) {
        return cmd_auto_rules(argc - 1, &argv[1]);
    }
    if (strcmp(argv[1], "rule") == 0) {
        return cmd_auto_rule(argc - 1, &argv[1]);
    }
    if (strcmp(argv[1], "last") == 0) {
        return cmd_auto_last(argc - 1, &argv[1]);
    }
    if (strcmp(argv[1], "add_rule") == 0) {
        return cmd_auto_add_rule(argc - 1, &argv[1]);
    }
    if (strcmp(argv[1], "update_rule") == 0) {
        return cmd_auto_update_rule(argc - 1, &argv[1]);
    }
    if (strcmp(argv[1], "delete_rule") == 0) {
        return cmd_auto_delete_rule(argc - 1, &argv[1]);
    }
    if (strcmp(argv[1], "emit_message") == 0) {
        return cmd_auto_emit_message(argc - 1, &argv[1]);
    }
    if (strcmp(argv[1], "emit_trigger") == 0) {
        return cmd_auto_emit_trigger(argc - 1, &argv[1]);
    }

    printf("Unknown auto subcommand: %s\n", argv[1]);
    printf("Usage: auto <reload|rules|rule|add_rule|update_rule|delete_rule|last|emit_message|emit_trigger> ...\n");
    return 1;
}

static void register_cap_cli_commands(void)
{
#if CONFIG_APP_CLAW_CAP_IM_QQ
    register_cap_im_qq();
#endif
#if CONFIG_APP_CLAW_CAP_IM_FEISHU
    register_cap_im_feishu();
#endif
#if CONFIG_APP_CLAW_CAP_IM_TG
    register_cap_im_tg();
#endif
#if CONFIG_APP_CLAW_CAP_IM_WECHAT
    register_cap_im_wechat();
#endif
#if CONFIG_APP_CLAW_CAP_LUA
    register_cap_lua();
#endif
#if CONFIG_APP_CLAW_CAP_LLM_INSPECT
    register_cap_llm_inspect();
#endif
#if CONFIG_APP_CLAW_CAP_ROUTER_MGR
    register_cap_router_mgr();
#endif
#if CONFIG_APP_CLAW_CAP_SCHEDULER
    register_cap_scheduler();
#endif
#if CONFIG_APP_CLAW_CAP_WEB_SEARCH
    register_cap_web_search();
#endif
}

esp_err_t app_claw_cli_start(void)
{
    esp_console_repl_t *repl = NULL;
    esp_console_repl_config_t repl_config = ESP_CONSOLE_REPL_CONFIG_DEFAULT();

    ESP_LOGI(TAG, "Starting console REPL");

    repl_config.prompt = "app> ";
    repl_config.task_stack_size = 10240;
    repl_config.max_cmdline_length = 512;

#if CONFIG_ESP_CONSOLE_UART_DEFAULT || CONFIG_ESP_CONSOLE_UART_CUSTOM
    esp_console_dev_uart_config_t hw_config = ESP_CONSOLE_DEV_UART_CONFIG_DEFAULT();
    ESP_ERROR_CHECK(esp_console_new_repl_uart(&hw_config, &repl_config, &repl));
#elif CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG
    esp_console_dev_usb_serial_jtag_config_t hw_config =
        ESP_CONSOLE_DEV_USB_SERIAL_JTAG_CONFIG_DEFAULT();
    ESP_ERROR_CHECK(esp_console_new_repl_usb_serial_jtag(&hw_config, &repl_config, &repl));
#elif CONFIG_ESP_CONSOLE_USB_CDC
    esp_console_dev_usb_cdc_config_t hw_config = ESP_CONSOLE_DEV_CDC_CONFIG_DEFAULT();
    ESP_ERROR_CHECK(esp_console_new_repl_usb_cdc(&hw_config, &repl_config, &repl));
#else
    ESP_LOGE(TAG, "No supported console backend is enabled");
    return ESP_ERR_NOT_SUPPORTED;
#endif

    esp_console_register_help_command();
    register_cap_cli_commands();

    {
        esp_console_cmd_t ask_cmd = {
            .command = "ask",
            .help = "Submit a multi-turn prompt using the current session: ask <prompt>",
            .func = cmd_ask,
        };
        ESP_ERROR_CHECK(esp_console_cmd_register(&ask_cmd));
    }

    {
        esp_console_cmd_t ask_once_cmd = {
            .command = "ask_once",
            .help = "Submit a single-turn prompt without session history: ask_once <prompt>",
            .func = cmd_ask_once,
        };
        ESP_ERROR_CHECK(esp_console_cmd_register(&ask_once_cmd));
    }

    {
        esp_console_cmd_t session_cmd = {
            .command = "session",
            .help = "Show, create, or switch numeric sessions: session [new|id]",
            .func = cmd_session,
        };
        ESP_ERROR_CHECK(esp_console_cmd_register(&session_cmd));
    }

    {
        esp_console_cmd_t respond_cmd = {
            .command = "respond",
            .help = "Respond to the current agent input request: respond <answer>",
            .func = cmd_respond,
        };
        ESP_ERROR_CHECK(esp_console_cmd_register(&respond_cmd));
    }

    {
        esp_console_cmd_t cap_cmd = {
            .command = "cap",
            .help = "cap operations: cap <list|call|groups|enable|disable|unload|load> ...",
            .func = cmd_cap,
        };
        ESP_ERROR_CHECK(esp_console_cmd_register(&cap_cmd));
    }

    {
        esp_console_cmd_t auto_cmd = {
            .command = "auto",
            .help = "Automation operations: auto <reload|rules|rule|add_rule|update_rule|delete_rule|last|emit_message|emit_trigger> ...",
            .func = cmd_auto,
        };
        ESP_ERROR_CHECK(esp_console_cmd_register(&auto_cmd));
    }

    printf("Type 'help', 'auto rules', 'auto last', or 'auto emit_message qq_gateway qq 123 hello'\n");
    return esp_console_start_repl(repl);
}
