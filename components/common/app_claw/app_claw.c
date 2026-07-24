/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "app_claw.h"
#include "app_claw_cli.h"
#include "app_capabilities.h"
#if CONFIG_APP_CLAW_ENABLE_EMOTE
#include "emote.h"
#endif

#include <stdbool.h>
#include <stdio.h>
#include <string.h>

#if CONFIG_APP_CLAW_CAP_SCHEDULER
#include "cap_scheduler.h"
#endif
#if CONFIG_APP_CLAW_CAP_SYSTEM
#include "cap_system.h"
#endif
#include "claw_agent.h"
#include "claw_paths.h"
#if CONFIG_APP_CLAW_CAP_EVENT_ROUTER
#include "claw_event_publisher.h"
#include "claw_event_router.h"
#endif
#include "esp_check.h"
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#if CONFIG_APP_CLAW_CAP_LUA
#include "cap_lua.h"
#endif

static const char *TAG = "app_claw";
#if CONFIG_APP_CLAW_CAP_EVENT_ROUTER
static const char *APP_STARTUP_EVENT_SOURCE_CAP = "app_claw";
static const char *APP_STARTUP_EVENT_TYPE = "startup";
static const char *APP_STARTUP_EVENT_KEY = "boot_completed";
#endif

static SemaphoreHandle_t s_config_lock;
static app_claw_config_t s_current_config;
static bool s_current_config_valid;
static app_claw_save_config_fn s_save_config;
static void *s_save_config_user_ctx;

static esp_err_t app_claw_ensure_config_lock(void)
{
    if (!s_config_lock) {
        s_config_lock = xSemaphoreCreateMutex();
        if (!s_config_lock) {
            return ESP_ERR_NO_MEM;
        }
    }
    return ESP_OK;
}

static esp_err_t app_claw_store_current_config(const app_claw_config_t *config)
{
    ESP_RETURN_ON_FALSE(config, ESP_ERR_INVALID_ARG, TAG, "config is NULL");
    ESP_RETURN_ON_ERROR(app_claw_ensure_config_lock(), TAG, "config lock unavailable");

    xSemaphoreTake(s_config_lock, portMAX_DELAY);
    s_current_config = *config;
    s_current_config_valid = true;
    xSemaphoreGive(s_config_lock);
    return ESP_OK;
}

esp_err_t app_claw_set_save_config_callback(app_claw_save_config_fn save_config,
                                            void *user_ctx)
{
    ESP_RETURN_ON_ERROR(app_claw_ensure_config_lock(), TAG, "config lock unavailable");

    xSemaphoreTake(s_config_lock, portMAX_DELAY);
    s_save_config = save_config;
    s_save_config_user_ctx = user_ctx;
    xSemaphoreGive(s_config_lock);
    return ESP_OK;
}

esp_err_t app_claw_get_config(app_claw_config_t *out_config)
{
    ESP_RETURN_ON_FALSE(out_config, ESP_ERR_INVALID_ARG, TAG, "out_config is NULL");
    ESP_RETURN_ON_ERROR(app_claw_ensure_config_lock(), TAG, "config lock unavailable");

    xSemaphoreTake(s_config_lock, portMAX_DELAY);
    if (!s_current_config_valid) {
        xSemaphoreGive(s_config_lock);
        return ESP_ERR_INVALID_STATE;
    }
    *out_config = s_current_config;
    xSemaphoreGive(s_config_lock);
    return ESP_OK;
}

esp_err_t app_claw_ui_start(void)
{
#if defined(CONFIG_APP_CLAW_ENABLE_EMOTE)
    return emote_start();
#else
    return ESP_OK;
#endif
}

esp_err_t app_claw_set_network_status(bool sta_connected, const char *ap_ssid)
{
#if defined(CONFIG_APP_CLAW_ENABLE_EMOTE)
    return emote_set_network_status(sta_connected, ap_ssid);
#else
    (void)sta_connected;
    (void)ap_ssid;
    return ESP_OK;
#endif
}

#if CONFIG_APP_CLAW_CAP_EVENT_ROUTER
static esp_err_t app_claw_publish_startup_event(void)
{
    static const char *payload_json =
        "{\"phase\":\"boot_completed\"}";

    ESP_LOGI(TAG, "Publishing startup trigger event: %s/%s",
             APP_STARTUP_EVENT_TYPE, APP_STARTUP_EVENT_KEY);
    return claw_event_router_publish_trigger(APP_STARTUP_EVENT_SOURCE_CAP,
                                             APP_STARTUP_EVENT_TYPE,
                                             APP_STARTUP_EVENT_KEY,
                                             payload_json);
}
#endif

#if CONFIG_APP_CLAW_CAP_SCHEDULER && CONFIG_APP_CLAW_CAP_SYSTEM
static void app_time_sync_success(bool had_valid_time, void *ctx)
{
    (void)ctx;

    if (!had_valid_time) {
        esp_err_t err = cap_scheduler_handle_time_sync();

        if (err != ESP_OK) {
            ESP_LOGW(TAG, "Scheduler rebase after first time sync failed: %s",
                     esp_err_to_name(err));
        } else {
            ESP_LOGI(TAG, "Scheduler rebased after first successful time sync");
        }
    }
}
#endif

// Resolve the storage paths threaded through the capability framework from the
// logical homes registered in claw_paths. This is where app_claw owns the data
// layout (the subdirectory convention); main only decides the mount points.
static esp_err_t build_storage_paths(app_claw_storage_paths_t *paths)
{
    memset(paths, 0, sizeof(*paths));

    ESP_RETURN_ON_ERROR(claw_paths_join(CLAW_PATH_DATA, NULL, paths->fatfs_base_path, sizeof(paths->fatfs_base_path)),
                        TAG, "data home unavailable");
    ESP_RETURN_ON_ERROR(claw_paths_join(CLAW_PATH_DATA, "agent", paths->agent_root_dir, sizeof(paths->agent_root_dir)),
                        TAG, "agent root path too long");
    ESP_RETURN_ON_ERROR(claw_paths_join(CLAW_PATH_DATA, "skills", paths->skills_root_dir, sizeof(paths->skills_root_dir)),
                        TAG, "skills root path too long");
    ESP_RETURN_ON_ERROR(claw_paths_join(CLAW_PATH_DATA, "scripts", paths->lua_root_dir, sizeof(paths->lua_root_dir)),
                        TAG, "lua root path too long");
    ESP_RETURN_ON_ERROR(claw_paths_join(CLAW_PATH_DATA, "router_rules/router_rules.json", paths->router_rules_path, sizeof(paths->router_rules_path)),
                        TAG, "router rules path too long");
    ESP_RETURN_ON_ERROR(claw_paths_join(CLAW_PATH_DATA, "scheduler/schedules.json", paths->scheduler_rules_path, sizeof(paths->scheduler_rules_path)),
                        TAG, "scheduler rules path too long");
    ESP_RETURN_ON_ERROR(claw_paths_join(CLAW_PATH_DATA, "inbox", paths->im_attachment_root, sizeof(paths->im_attachment_root)),
                        TAG, "inbox path too long");

    ESP_RETURN_ON_ERROR(claw_paths_join(CLAW_PATH_SYSTEM, "skills", paths->system_skills_root_dir, sizeof(paths->system_skills_root_dir)),
                        TAG, "system skills root path too long");

    return ESP_OK;
}

esp_err_t app_claw_start(const app_claw_config_t *config)
{
    app_claw_storage_paths_t paths;
    claw_agent_config_t agent_config = {0};
#if CONFIG_APP_CLAW_CAP_EVENT_ROUTER
    claw_event_router_config_t router_config = {
        .rules_path = NULL,
        .task_stack_size = 8 * 1024,
        .task_priority = 5,
        .task_core = tskNO_AFFINITY,
        .default_route_messages_to_agent = false,
        .default_route_agent_output_to_channel = false,
    };
#endif
    if (!config) {
        return ESP_ERR_INVALID_ARG;
    }
    ESP_RETURN_ON_ERROR(app_claw_store_current_config(config), TAG, "Failed to store Claw config");
    ESP_RETURN_ON_ERROR(build_storage_paths(&paths), TAG, "Failed to resolve storage paths");

#if CONFIG_APP_CLAW_CAP_EVENT_ROUTER
    router_config.default_route_messages_to_agent = true;
    router_config.default_route_agent_output_to_channel = true;
    router_config.rules_path = paths.router_rules_path;
#endif

#if CONFIG_APP_CLAW_CAP_EVENT_ROUTER
    ESP_RETURN_ON_ERROR(claw_event_router_init(&router_config), TAG, "Failed to init event router");
#endif

#if CONFIG_APP_CLAW_CAP_SCHEDULER
    ESP_RETURN_ON_ERROR(cap_scheduler_init(&(cap_scheduler_config_t) {
                            .schedules_path = paths.scheduler_rules_path,
                            .tick_ms = 1000,
                            .max_items = 32,
                            .task_stack_size = 6144,
                            .task_priority = 5,
                            .task_core = tskNO_AFFINITY,
                            .publish_event = claw_event_router_publish,
                            .persist_after_fire = true,
                        }),
                        TAG, "Failed to init scheduler");
#endif
    ESP_RETURN_ON_ERROR(app_capabilities_init(config, &paths), TAG, "Failed to init capabilities");

    agent_config.api_key = config->llm_api_key;
    agent_config.backend_type = config->llm_backend_type;
    agent_config.model = config->llm_model;
    agent_config.base_url = config->llm_base_url;
    agent_config.persistence_dir = paths.agent_root_dir;
    agent_config.skills_root_dir = paths.skills_root_dir;
    agent_config.system_skills_root_dir = paths.system_skills_root_dir;

    ESP_LOGI(TAG, "Initializing AgentSystem backend=%s base_url=%s model=%s token=%s",
             config->llm_backend_type[0] ? config->llm_backend_type : "(unbound)",
             config->llm_base_url[0] ? config->llm_base_url : "(empty)",
             config->llm_model[0] ? config->llm_model : "(empty)",
             config->llm_api_key[0] ? "configured" : "missing");
    ESP_RETURN_ON_ERROR(claw_agent_init(&agent_config), TAG, "Failed to initialize AgentSystem");
    ESP_RETURN_ON_ERROR(claw_agent_start(), TAG, "Failed to start AgentSystem");

#if CONFIG_APP_CLAW_CAP_IM_QQ
    ESP_RETURN_ON_ERROR(claw_event_router_register_outbound_binding("qq", "qq_send_message"),
                        TAG, "Failed to bind QQ outbound");
#endif
#if CONFIG_APP_CLAW_CAP_IM_FEISHU
    ESP_RETURN_ON_ERROR(claw_event_router_register_outbound_binding("feishu", "feishu_send_message"),
                        TAG, "Failed to bind Feishu outbound");
#endif
#if CONFIG_APP_CLAW_CAP_IM_TG
    ESP_RETURN_ON_ERROR(claw_event_router_register_outbound_binding("telegram", "tg_send_message"),
                        TAG, "Failed to bind Telegram outbound");
#endif
#if CONFIG_APP_CLAW_CAP_IM_WECHAT
    ESP_RETURN_ON_ERROR(claw_event_router_register_outbound_binding("wechat", "wechat_send_message"),
                        TAG, "Failed to bind WeChat outbound");
#endif
#if CONFIG_APP_CLAW_CAP_IM_LOCAL
    ESP_RETURN_ON_ERROR(claw_event_router_register_outbound_binding("web", "local_send_message"),
                        TAG, "Failed to bind Web / local IM outbound");
#endif

#if CONFIG_APP_CLAW_CAP_EVENT_ROUTER
    ESP_RETURN_ON_ERROR(claw_event_router_start(), TAG, "Failed to start event router");
#endif
#if CONFIG_APP_CLAW_CAP_SCHEDULER
    ESP_RETURN_ON_ERROR(cap_scheduler_start(), TAG, "Failed to start scheduler");
#endif

#if CONFIG_APP_CLAW_CAP_SYSTEM
    ESP_ERROR_CHECK(cap_system_time_sync_service_start(&(cap_system_time_sync_service_config_t) {
                        .network_ready = NULL,
#if CONFIG_APP_CLAW_CAP_SCHEDULER
                        .on_sync_success = app_time_sync_success,
#else
                        .on_sync_success = NULL,
#endif
                    }));
#endif

#if CONFIG_APP_CLAW_ENABLE_CLI
    ESP_RETURN_ON_ERROR(app_claw_cli_start(), TAG, "Failed to start CLI");
#endif
#if CONFIG_APP_CLAW_CAP_EVENT_ROUTER
    ESP_RETURN_ON_ERROR(app_claw_publish_startup_event(), TAG,
                        "Failed to publish startup event");
#endif
    ESP_LOGI(TAG, "App Claw runtime started");

    return ESP_OK;
}

esp_err_t app_claw_update_config(const app_claw_config_t *config)
{
    claw_agent_api_config_t api_config;
    app_claw_config_t previous_config;
    bool had_previous_config = false;
    esp_err_t err;

    if (!config) {
        return ESP_ERR_INVALID_ARG;
    }
    ESP_RETURN_ON_ERROR(app_claw_ensure_config_lock(), TAG, "config lock unavailable");
    api_config = (claw_agent_api_config_t) {
        .api_key = config->llm_api_key,
        .backend_type = config->llm_backend_type,
        .model = config->llm_model,
        .base_url = config->llm_base_url,
    };

    xSemaphoreTake(s_config_lock, portMAX_DELAY);
    if (s_current_config_valid) {
        previous_config = s_current_config;
        had_previous_config = true;
    }
    err = app_capabilities_update_config(config);
    if (err == ESP_OK) {
        err = claw_agent_link_api(&api_config, CLAW_AGENT_API_PURPOSE_ROOT_AGENT, true);
    }
    if (err == ESP_OK) {
        s_current_config = *config;
        s_current_config_valid = true;
    } else if (had_previous_config) {
        esp_err_t rollback_err = app_capabilities_update_config(&previous_config);

        if (rollback_err != ESP_OK) {
            ESP_LOGE(TAG, "Failed to restore capability configuration: %s",
                     esp_err_to_name(rollback_err));
        }
    }
    xSemaphoreGive(s_config_lock);
    return err;
}

esp_err_t app_claw_apply_config(const app_claw_config_t *config)
{
    app_claw_save_config_fn save_config = NULL;
    void *save_user_ctx = NULL;
    esp_err_t err;

    if (!config) {
        return ESP_ERR_INVALID_ARG;
    }
    ESP_RETURN_ON_ERROR(app_claw_ensure_config_lock(), TAG, "config lock unavailable");

    xSemaphoreTake(s_config_lock, portMAX_DELAY);
    save_config = s_save_config;
    save_user_ctx = s_save_config_user_ctx;
    xSemaphoreGive(s_config_lock);

    if (save_config) {
        err = save_config(config, save_user_ctx);
        if (err != ESP_OK) {
            return err;
        }
    }
    return app_claw_update_config(config);
}
