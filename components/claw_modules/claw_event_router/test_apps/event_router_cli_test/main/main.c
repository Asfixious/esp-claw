/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include <errno.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>

#include "cap_router_mgr.h"
#include "cJSON.h"
#include "cmd_cap_router_mgr.h"
#include "claw_cap.h"
#include "claw_event_router.h"
#include "esp_check.h"
#include "esp_console.h"
#include "esp_err.h"
#include "esp_log.h"
#include "esp_vfs_fat.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "nvs_flash.h"
#include "wear_levelling.h"

static const char *TAG = "event_router_test";

#define TEST_FATFS_BASE_PATH       "/tmp"
#define TEST_FATFS_PARTITION_LABEL "storage"
#define TEST_AUTOMATION_DIR        TEST_FATFS_BASE_PATH "/auto"
#define TEST_RULES_PATH            TEST_AUTOMATION_DIR "/rules"

static wl_handle_t s_wl_handle = WL_INVALID_HANDLE;
static char s_agent_input[512];
static char s_agent_session_id[16];
static uint32_t s_agent_request_id;
static unsigned s_agent_call_count;
static char s_outbound_input[512];
static char s_outbound_channel[32];
static char s_outbound_chat_id[64];
static char s_outbound_session_id[16];
static uint32_t s_outbound_request_id;
static unsigned s_outbound_call_count;

static const char *s_seed_rules_json =
    "[]\n";

static esp_err_t test_agent_execute(const char *input_json,
                                    const claw_cap_call_context_t *ctx,
                                    char *output,
                                    size_t output_size)
{
    strlcpy(s_agent_input, input_json ? input_json : "", sizeof(s_agent_input));
    strlcpy(s_agent_session_id,
            ctx && ctx->session_id ? ctx->session_id : "",
            sizeof(s_agent_session_id));
    s_agent_request_id = ctx ? ctx->request_id : 0;
    s_agent_call_count++;
    if (input_json && strstr(input_json, "/session") != NULL) {
        strlcpy(output, "Sessions:\n* 12 (current)", output_size);
    } else {
        strlcpy(output, "{\"ok\":true}", output_size);
    }
    return ESP_OK;
}

static esp_err_t test_outbound_execute(const char *input_json,
                                       const claw_cap_call_context_t *ctx,
                                       char *output,
                                       size_t output_size)
{
    strlcpy(s_outbound_input,
            input_json ? input_json : "",
            sizeof(s_outbound_input));
    strlcpy(s_outbound_channel,
            ctx && ctx->channel ? ctx->channel : "",
            sizeof(s_outbound_channel));
    strlcpy(s_outbound_chat_id,
            ctx && ctx->chat_id ? ctx->chat_id : "",
            sizeof(s_outbound_chat_id));
    strlcpy(s_outbound_session_id,
            ctx && ctx->session_id ? ctx->session_id : "",
            sizeof(s_outbound_session_id));
    s_outbound_request_id = ctx ? ctx->request_id : 0;
    s_outbound_call_count++;
    strlcpy(output, "{\"ok\":true}", output_size);
    return ESP_OK;
}

static const claw_cap_descriptor_t s_test_agent_descriptors[] = {
    {
        .id = "agent",
        .name = "agent",
        .family = "test",
        .description = "Capture Router agent forwarding.",
        .kind = CLAW_CAP_KIND_CALLABLE,
        .input_schema_json = "{\"type\":\"object\"}",
        .execute = test_agent_execute,
    },
    {
        .id = "test_send_message",
        .name = "test_send_message",
        .family = "test",
        .description = "Capture Router outbound messages.",
        .kind = CLAW_CAP_KIND_CALLABLE,
        .input_schema_json = "{\"type\":\"object\"}",
        .execute = test_outbound_execute,
    },
};

static const claw_cap_group_t s_test_agent_group = {
    .group_id = "test_agent",
    .descriptors = s_test_agent_descriptors,
    .descriptor_count = sizeof(s_test_agent_descriptors) /
                        sizeof(s_test_agent_descriptors[0]),
};

static esp_err_t init_nvs(void)
{
    esp_err_t err = nvs_flash_init();

    if (err == ESP_ERR_NVS_NO_FREE_PAGES || err == ESP_ERR_NVS_NEW_VERSION_FOUND) {
        ESP_ERROR_CHECK(nvs_flash_erase());
        err = nvs_flash_init();
    }

    return err;
}

static esp_err_t init_fatfs(void)
{
    esp_vfs_fat_mount_config_t mount_config = {
        .format_if_mount_failed = true,
        .max_files = 8,
        .allocation_unit_size = 4096,
        .disk_status_check_enable = false,
        .use_one_fat = false,
    };
    esp_err_t err;

    err = esp_vfs_fat_spiflash_mount_rw_wl(TEST_FATFS_BASE_PATH,
                                           TEST_FATFS_PARTITION_LABEL,
                                           &mount_config,
                                           &s_wl_handle);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "Failed to mount FATFS: %s", esp_err_to_name(err));
        return err;
    }

    return ESP_OK;
}

static esp_err_t ensure_dir(const char *path)
{
    if (!path || !path[0]) {
        return ESP_ERR_INVALID_ARG;
    }

    if (mkdir(path, 0775) == 0 || errno == EEXIST) {
        return ESP_OK;
    }

    ESP_LOGE(TAG, "Failed to create directory %s: errno=%d", path, errno);
    return ESP_FAIL;
}

static esp_err_t write_text_file(const char *path, const char *content)
{
    FILE *file = NULL;

    if (!path || !content) {
        return ESP_ERR_INVALID_ARG;
    }

    file = fopen(path, "wb");
    if (!file) {
        ESP_LOGE(TAG, "Failed to open %s for writing: errno=%d", path, errno);
        return ESP_FAIL;
    }

    if (fwrite(content, 1, strlen(content), file) != strlen(content)) {
        fclose(file);
        ESP_LOGE(TAG, "Failed to write %s", path);
        return ESP_FAIL;
    }

    fclose(file);
    return ESP_OK;
}

static esp_err_t prepare_rules_file(void)
{
    ESP_RETURN_ON_ERROR(ensure_dir(TEST_AUTOMATION_DIR), TAG, "Failed to prepare automation dir");
    return write_text_file(TEST_RULES_PATH, s_seed_rules_json);
}

static esp_err_t init_console(void)
{
    esp_console_config_t console_config = {
        .max_cmdline_length = 512,
        .max_cmdline_args = 32,
    };

    ESP_RETURN_ON_ERROR(esp_console_init(&console_config), TAG, "Failed to init console");
    esp_console_register_help_command();
    ESP_RETURN_ON_ERROR(claw_cap_init(), TAG, "Failed to init claw_cap");
    ESP_RETURN_ON_ERROR(cap_router_mgr_register_group(), TAG, "Failed to register router manager cap");
    ESP_RETURN_ON_ERROR(claw_cap_register_group(&s_test_agent_group),
                        TAG,
                        "Failed to register test agent cap");
    ESP_RETURN_ON_ERROR(claw_cap_start_all(), TAG, "Failed to start capabilities");
    register_cap_router_mgr();
    return ESP_OK;
}

static esp_err_t init_event_router(void)
{
    claw_event_router_config_t config = {
        .rules_path = TEST_RULES_PATH,
        .event_queue_len = 4,
        .task_stack_size = 4096,
        .task_priority = 4,
        .task_core = tskNO_AFFINITY,
        .default_route_messages_to_agent = true,
        .default_route_agent_output_to_channel = true,
    };

    ESP_RETURN_ON_ERROR(claw_event_router_init(&config), TAG, "Failed to init event router");
    ESP_RETURN_ON_ERROR(claw_event_router_register_outbound_binding(
                            "cli",
                            "test_send_message"),
                        TAG,
                        "Failed to register test outbound binding");
    return claw_event_router_start();
}

static esp_err_t run_cli_capture(const char *command_line, char **out_stdout_text, int *out_cmd_ret)
{
    FILE *capture = NULL;
    FILE *saved_stdout = NULL;
    char *buffer = NULL;
    size_t buffer_len = 0;
    esp_err_t run_err;

    if (!command_line || !out_stdout_text || !out_cmd_ret) {
        return ESP_ERR_INVALID_ARG;
    }

    *out_stdout_text = NULL;
    *out_cmd_ret = -1;

    capture = open_memstream(&buffer, &buffer_len);
    if (!capture) {
        return ESP_FAIL;
    }

    fflush(stdout);
    saved_stdout = stdout;
    stdout = capture;
    run_err = esp_console_run(command_line, out_cmd_ret);
    fflush(stdout);
    stdout = saved_stdout;

    if (fclose(capture) != 0) {
        free(buffer);
        return ESP_FAIL;
    }

    if (!buffer) {
        buffer = calloc(1, 1);
        if (!buffer) {
            return ESP_ERR_NO_MEM;
        }
    }

    *out_stdout_text = buffer;
    return run_err;
}

static bool output_contains(const char *text, const char *needle)
{
    if (!needle || !needle[0]) {
        return true;
    }

    return text && strstr(text, needle) != NULL;
}

static bool run_cli_and_check(const char *label,
                              const char *command_line,
                              const char *expect_1,
                              const char *expect_2,
                              TickType_t wait_after_ticks)
{
    char *stdout_text = NULL;
    int cmd_ret = -1;
    esp_err_t run_err;
    bool passed = true;

    ESP_LOGI(TAG, "[RUN] %s", label);
    ESP_LOGI(TAG, "cmd: %s", command_line);

    run_err = run_cli_capture(command_line, &stdout_text, &cmd_ret);
    if (run_err != ESP_OK) {
        ESP_LOGE(TAG, "Command dispatch failed: %s", esp_err_to_name(run_err));
        free(stdout_text);
        return false;
    }

    ESP_LOGI(TAG, "ret=%d", cmd_ret);
    ESP_LOGI(TAG, "stdout:\n%s", stdout_text ? stdout_text : "");

    if (cmd_ret != 0) {
        ESP_LOGE(TAG, "Command returned non-zero status");
        passed = false;
    }
    if (!output_contains(stdout_text, expect_1)) {
        ESP_LOGE(TAG, "Missing expected output: %s", expect_1);
        passed = false;
    }
    if (!output_contains(stdout_text, expect_2)) {
        ESP_LOGE(TAG, "Missing expected output: %s", expect_2);
        passed = false;
    }

    free(stdout_text);

    if (passed && wait_after_ticks > 0) {
        vTaskDelay(wait_after_ticks);
    }

    ESP_LOGI(TAG, "[%s] %s", passed ? "PASS" : "FAIL", label);
    return passed;
}

static bool agent_rpc_forwarding_check(void)
{
    static const char *agent_rule_json =
        "{\"id\":\"agent_rpc\",\"enabled\":true,\"consume_on_match\":true,"
        "\"match\":{\"event_type\":\"message\",\"content_type\":\"text\",\"source_cap\":\"test_source\","
        "\"channel\":\"cli\",\"chat_id\":\"room_agent\",\"text\":\"call agent\"},"
        "\"actions\":[{\"type\":\"call_cap\",\"cap\":\"agent\",\"input\":{"
        "\"method\":\"session.input\",\"args\":{\"text\":\"{{event.text}}\"}}}]}";
    claw_event_t event = {
        .source_cap = "test_source",
        .event_type = "message",
        .source_channel = "cli",
        .chat_id = "room_agent",
        .content_type = "text",
        .session_policy = CLAW_SESSION_POLICY_CHAT,
    };
    claw_event_router_result_t result = {0};
    cJSON *root = NULL;
    cJSON *args = NULL;
    const char *method = NULL;
    const char *forwarded_text = NULL;
    esp_err_t err;
    bool passed = true;

    ESP_LOGI(TAG, "[RUN] agent_rpc_forwarding");
    err = claw_event_router_add_rule_json(agent_rule_json);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "Failed to add agent RPC rule: %s", esp_err_to_name(err));
        return false;
    }

    event.text = calloc(1, 32);
    if (!event.text) {
        (void)claw_event_router_delete_rule("agent_rpc");
        return false;
    }

    memset(s_agent_input, 0, sizeof(s_agent_input));
    memset(s_agent_session_id, 0, sizeof(s_agent_session_id));
    s_agent_request_id = 0;
    s_agent_call_count = 0;
    event.session_id = 42;
    event.request_id = 7;
    strlcpy(event.text, "call agent", 32);
    err = claw_event_router_handle_event(&event, &result);
    root = cJSON_Parse(s_agent_input);
    method = cJSON_GetStringValue(cJSON_GetObjectItemCaseSensitive(root, "method"));
    args = cJSON_GetObjectItemCaseSensitive(root, "args");
    forwarded_text = cJSON_GetStringValue(
        cJSON_GetObjectItemCaseSensitive(args, "text"));
    if (err != ESP_OK ||
            s_agent_call_count != 1 ||
            strcmp(s_agent_session_id, "42") != 0 ||
            s_agent_request_id != 7 ||
            !method || strcmp(method, "session.input") != 0 ||
            !forwarded_text || strcmp(forwarded_text, "call agent") != 0) {
        ESP_LOGE(TAG,
                 "agent RPC forwarding returned err=%s input=%s session=%s request=%" PRIu32,
                 esp_err_to_name(err),
                 s_agent_input,
                 s_agent_session_id,
                 s_agent_request_id);
        passed = false;
    }
    cJSON_Delete(root);

    claw_event_free(&event);
    (void)claw_event_router_delete_rule("agent_rpc");
    ESP_LOGI(TAG, "[%s] agent_rpc_forwarding", passed ? "PASS" : "FAIL");
    return passed;
}

static bool run_session_command_forwarding_check(void)
{
    static const char *session_rule_json =
        "{\"id\":\"session_forward\",\"enabled\":true,\"consume_on_match\":true,"
        "\"match\":{\"event_type\":\"message\",\"content_type\":\"text\","
        "\"source_cap\":\"test_source\",\"channel\":\"cli\","
        "\"chat_id\":\"room_session\",\"text\":\"/session\","
        "\"text_match_rule\":\"prefix\"},"
        "\"actions\":[{\"type\":\"call_cap\",\"cap\":\"agent\","
        "\"input\":{\"method\":\"session.command\",\"args\":{"
        "\"text\":\"{{event.text}}\"}}},{\"type\":\"send_message\","
        "\"input\":{\"channel\":\"{{event.source_channel}}\","
        "\"chat_id\":\"{{event.chat_id}}\"}}]}";
    claw_event_t event = {
        .source_cap = "test_source",
        .event_type = "message",
        .source_channel = "cli",
        .chat_id = "room_session",
        .content_type = "text",
        .session_policy = CLAW_SESSION_POLICY_CHAT,
    };
    claw_event_router_result_t result = {0};
    cJSON *agent_root = NULL;
    cJSON *agent_args = NULL;
    cJSON *outbound_root = NULL;
    const char *agent_method;
    const char *agent_message;
    const char *outbound_message;
    const char *outbound_chat_id;
    esp_err_t err;
    bool passed;

    ESP_LOGI(TAG, "[RUN] session_command_forwarding");
    err = claw_event_router_add_rule_json(session_rule_json);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "Failed to add session forwarding rule: %s", esp_err_to_name(err));
        return false;
    }
    event.text = strdup("/session switch 12");
    event.session_id = 12;
    if (!event.text) {
        (void)claw_event_router_delete_rule("session_forward");
        return false;
    }

    memset(s_agent_input, 0, sizeof(s_agent_input));
    memset(s_outbound_input, 0, sizeof(s_outbound_input));
    memset(s_outbound_channel, 0, sizeof(s_outbound_channel));
    memset(s_outbound_chat_id, 0, sizeof(s_outbound_chat_id));
    s_agent_call_count = 0;
    s_outbound_call_count = 0;
    err = claw_event_router_handle_event(&event, &result);
    agent_root = cJSON_Parse(s_agent_input);
    outbound_root = cJSON_Parse(s_outbound_input);
    agent_method = cJSON_GetStringValue(
        cJSON_GetObjectItemCaseSensitive(agent_root, "method"));
    agent_args = cJSON_GetObjectItemCaseSensitive(agent_root, "args");
    agent_message = cJSON_GetStringValue(
        cJSON_GetObjectItemCaseSensitive(agent_args, "text"));
    outbound_message = cJSON_GetStringValue(
        cJSON_GetObjectItemCaseSensitive(outbound_root, "message"));
    outbound_chat_id = cJSON_GetStringValue(
        cJSON_GetObjectItemCaseSensitive(outbound_root, "chat_id"));
    passed = err == ESP_OK &&
            result.action_count == 2 &&
            result.failed_actions == 0 &&
            s_agent_call_count == 1 &&
            strcmp(s_agent_session_id, "12") == 0 &&
            agent_method && strcmp(agent_method, "session.command") == 0 &&
            agent_message && strcmp(agent_message, "/session switch 12") == 0 &&
            s_outbound_call_count == 1 &&
            strcmp(s_outbound_channel, "cli") == 0 &&
            strcmp(s_outbound_chat_id, "room_session") == 0 &&
            outbound_chat_id && strcmp(outbound_chat_id, "room_session") == 0 &&
            outbound_message &&
            strcmp(outbound_message, "Sessions:\n* 12 (current)") == 0;
    if (!passed) {
        ESP_LOGE(TAG,
                 "session forwarding failed err=%s actions=%u/%u agent=%s outbound=%s",
                 esp_err_to_name(err),
                 (unsigned)result.action_count,
                 (unsigned)result.failed_actions,
                 s_agent_input,
                 s_outbound_input);
    }

    cJSON_Delete(agent_root);
    cJSON_Delete(outbound_root);
    claw_event_free(&event);
    (void)claw_event_router_delete_rule("session_forward");
    ESP_LOGI(TAG, "[%s] session_command_forwarding", passed ? "PASS" : "FAIL");
    return passed;
}

static bool agent_output_forwarding_check(void)
{
    claw_event_t event = {
        .source_cap = "agent",
        .event_type = "out_message",
        .source_channel = "cli",
        .chat_id = "room_output",
        .target_channel = "cli",
        .target_endpoint = "room_output",
        .content_type = "text",
        .session_id = 77,
        .request_id = 9,
    };
    claw_event_router_result_t result = {0};
    cJSON *outbound_root = NULL;
    const char *message;
    esp_err_t err;
    bool passed;

    ESP_LOGI(TAG, "[RUN] agent_output_forwarding");
    event.text = strdup("agent answer");
    if (!event.text) {
        return false;
    }

    memset(s_outbound_input, 0, sizeof(s_outbound_input));
    memset(s_outbound_channel, 0, sizeof(s_outbound_channel));
    memset(s_outbound_chat_id, 0, sizeof(s_outbound_chat_id));
    memset(s_outbound_session_id, 0, sizeof(s_outbound_session_id));
    s_outbound_request_id = 0;
    s_outbound_call_count = 0;

    err = claw_event_router_handle_event(&event, &result);
    outbound_root = cJSON_Parse(s_outbound_input);
    message = cJSON_GetStringValue(
        cJSON_GetObjectItemCaseSensitive(outbound_root, "message"));
    passed = err == ESP_OK &&
            result.action_count == 1 &&
            result.failed_actions == 0 &&
            s_outbound_call_count == 1 &&
            strcmp(s_outbound_channel, "cli") == 0 &&
            strcmp(s_outbound_chat_id, "room_output") == 0 &&
            strcmp(s_outbound_session_id, "77") == 0 &&
            s_outbound_request_id == 9 &&
            message && strcmp(message, "agent answer") == 0;
    if (!passed) {
        ESP_LOGE(TAG,
                 "agent output forwarding failed err=%s actions=%u/%u ctx=%s:%s/%s/%" PRIu32
                 " payload=%s",
                 esp_err_to_name(err),
                 (unsigned)result.action_count,
                 (unsigned)result.failed_actions,
                 s_outbound_channel,
                 s_outbound_chat_id,
                 s_outbound_session_id,
                 s_outbound_request_id,
                 s_outbound_input);
    }

    cJSON_Delete(outbound_root);
    claw_event_free(&event);
    ESP_LOGI(TAG, "[%s] agent_output_forwarding", passed ? "PASS" : "FAIL");
    return passed;
}

static bool run_smoke_suite(void)
{
    bool ok = true;

    ok = run_cli_and_check("list_empty_rules",
                           "event_router --rules",
                           "[]",
                           NULL,
                           0) && ok;

    ok = run_cli_and_check("add_message_rule",
                           "event_router --add-rule-json "
                           "{\"id\":\"msg_drop\",\"description\":\"drop_message\",\"ack\":\"message_ack_v1\","
                           "\"match\":{\"event_type\":\"message\",\"event_key\":\"text\",\"content_type\":\"text\","
                           "\"source_cap\":\"test_source\",\"source_channel\":\"cli\",\"chat_id\":\"room1\","
                           "\"text\":\"hello_router\"},\"actions\":[{\"type\":\"drop\"}]}",
                           "\"ok\":true",
                           NULL,
                           0) && ok;

    ok = run_cli_and_check("read_message_rule",
                           "event_router --rule msg_drop",
                           "\"id\":\"msg_drop\"",
                           "\"ack\":\"message_ack_v1\"",
                           0) && ok;

    ok = run_cli_and_check("update_message_rule",
                           "event_router --update-rule-json "
                           "{\"id\":\"msg_drop\",\"description\":\"drop_message_updated\",\"ack\":\"message_ack_v2\","
                           "\"match\":{\"event_type\":\"message\",\"event_key\":\"text\",\"content_type\":\"text\","
                           "\"source_cap\":\"test_source\",\"source_channel\":\"cli\",\"chat_id\":\"room1\","
                           "\"text\":\"hello_router\"},\"actions\":[{\"type\":\"drop\"}]}",
                           "\"ok\":true",
                           NULL,
                           0) && ok;

    ok = run_cli_and_check("reload_rules",
                           "event_router --reload",
                           "\"action\":\"reload_router_rules\"",
                           NULL,
                           0) && ok;

    ok = run_cli_and_check("emit_message",
                           "event_router --emit-message --source-cap test_source "
                           "--channel cli --chat-id room1 --text hello_router",
                           "message event published via test_source to cli:room1",
                           NULL,
                           pdMS_TO_TICKS(200)) && ok;

    ok = run_cli_and_check("check_last_message_result",
                           "event_router --last",
                           "first_rule_id=msg_drop",
                           "ack=message_ack_v2",
                           0) && ok;

    ok = run_cli_and_check("add_trigger_rule",
                           "event_router --add-rule-json "
                           "{\"id\":\"trigger_drop\",\"description\":\"drop_trigger\",\"ack\":\"trigger_ack\","
                           "\"match\":{\"event_type\":\"doorbell\",\"event_key\":\"ding\",\"source_cap\":\"test_source\"},"
                           "\"actions\":[{\"type\":\"drop\"}]}",
                           "\"ok\":true",
                           NULL,
                           0) && ok;

    ok = run_cli_and_check("emit_trigger",
                           "event_router --emit-trigger --source-cap test_source "
                           "--event-type doorbell --event-key ding --payload-json {\"state\":\"on\"}",
                           "trigger event published via test_source type=doorbell key=ding",
                           NULL,
                           pdMS_TO_TICKS(200)) && ok;

    ok = run_cli_and_check("check_last_trigger_result",
                           "event_router --last",
                           "first_rule_id=trigger_drop",
                           "ack=trigger_ack",
                           0) && ok;

    ok = run_cli_and_check("delete_message_rule",
                           "event_router --delete-rule msg_drop",
                           "\"action\":\"delete_router_rule\"",
                           NULL,
                           0) && ok;

    ok = run_cli_and_check("delete_trigger_rule",
                           "event_router --delete-rule trigger_drop",
                           "\"action\":\"delete_router_rule\"",
                           NULL,
                           0) && ok;

    ok = run_cli_and_check("list_rules_after_cleanup",
                           "event_router --rules",
                           "[]",
                           NULL,
                           0) && ok;

    ok = agent_rpc_forwarding_check() && ok;
    ok = run_session_command_forwarding_check() && ok;
    ok = agent_output_forwarding_check() && ok;

    return ok;
}

void app_main(void)
{
    bool passed;

    ESP_LOGI(TAG, "Starting event_router CLI smoke app");
    ESP_ERROR_CHECK(init_nvs());
    ESP_ERROR_CHECK(init_fatfs());
    ESP_ERROR_CHECK(prepare_rules_file());
    ESP_ERROR_CHECK(init_console());
    ESP_ERROR_CHECK(init_event_router());

    passed = run_smoke_suite();
    if (!passed) {
        ESP_LOGE(TAG, "CLI smoke test failed");
        abort();
    }

    ESP_LOGI(TAG, "CLI smoke test passed");
    while (true) {
        vTaskDelay(pdMS_TO_TICKS(60000));
    }
}
