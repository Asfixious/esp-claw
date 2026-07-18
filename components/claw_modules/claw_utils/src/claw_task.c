/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "claw_task.h"

#include <stdbool.h>

#include "esp_heap_caps.h"
#include "esp_log.h"
#include "freertos/idf_additions.h"

static const char *TAG = "claw_task";

static bool claw_task_external_memory_available(void)
{
#if defined(CONFIG_FREERTOS_TASK_CREATE_ALLOW_EXT_MEM) && CONFIG_FREERTOS_TASK_CREATE_ALLOW_EXT_MEM
    return heap_caps_get_total_size(MALLOC_CAP_SPIRAM) > 0;
#else
    return false;
#endif
}

static UBaseType_t claw_task_memory_caps(claw_task_stack_policy_t policy)
{
    if (policy != CLAW_TASK_STACK_INTERNAL_ONLY && claw_task_external_memory_available()) {
        return MALLOC_CAP_SPIRAM;
    }
    return MALLOC_CAP_INTERNAL;
}

BaseType_t claw_task_create(const claw_task_config_t *config,
                            TaskFunction_t task_func,
                            void *arg,
                            TaskHandle_t *task_handle)
{
    UBaseType_t memory_caps;

    if (!config || !config->name || !config->name[0] || !task_func || config->stack_size == 0) {
        return errCOULD_NOT_ALLOCATE_REQUIRED_MEMORY;
    }

    memory_caps = claw_task_memory_caps(config->stack_policy);
    if (config->stack_policy == CLAW_TASK_STACK_PSRAM_ONLY &&
            memory_caps != MALLOC_CAP_SPIRAM) {
        ESP_LOGE(TAG, "task '%s' requires PSRAM stack but PSRAM is unavailable", config->name);
        return errCOULD_NOT_ALLOCATE_REQUIRED_MEMORY;
    }

    return xTaskCreatePinnedToCoreWithCaps(task_func,
                                           config->name,
                                           config->stack_size,
                                           arg,
                                           config->priority,
                                           task_handle,
                                           config->core_id,
                                           memory_caps);
}

void claw_task_delete(TaskHandle_t task_handle)
{
    vTaskDeleteWithCaps(task_handle);
}
