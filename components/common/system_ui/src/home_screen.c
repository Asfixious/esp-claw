/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "system_ui_private.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "display_service.h"
#include "esp_check.h"

#define SYSTEM_UI_LOCK_FADE_MIN_DISTANCE 80
#define SYSTEM_UI_LOCK_FADE_MAX_DISTANCE 260
#define SYSTEM_UI_LOCK_FADE_DISTANCE_PCT 45
#define SYSTEM_UI_LOCK_UNLOCK_THRESHOLD_PCT 45
#define SYSTEM_UI_LOCK_ANIM_MS 140

typedef struct {
    int32_t pad;
    int32_t header_y;
    int32_t status_h;
    int32_t clock_y;
    int32_t clock_w;
    int32_t clock_h;
    int32_t swipe_y;
    int32_t date_y;
} system_ui_home_layout_t;

static void system_ui_lock_screen_apply_text_opa(int32_t opa);

static system_ui_home_layout_t system_ui_home_layout(void)
{
    int32_t width = (int32_t)s_ui.width;
    int32_t height = (int32_t)s_ui.height;
    int32_t short_side = system_ui_short_side_from(s_ui.width, s_ui.height);
    int32_t clock_h = system_ui_clamp_i32(height * 30 / 100, 68, 118);
    int32_t clock_w = system_ui_clamp_i32(width - system_ui_clamp_i32(short_side / 8, 28, 64), 176, 380);
    int32_t clock_center_y = system_ui_clamp_i32(height * 58 / 100, clock_h / 2 + 54, height - 58);

    return (system_ui_home_layout_t) {
        .pad = system_ui_clamp_i32(short_side / 16, 14, 30),
        .header_y = system_ui_clamp_i32(short_side / 22, 10, 20),
        .status_h = system_ui_clamp_i32(short_side / 12, 18, 32),
        .clock_y = clock_center_y - clock_h / 2,
        .clock_w = clock_w,
        .clock_h = clock_h,
        .swipe_y = system_ui_clamp_i32(short_side * 22 / 100, 42, 92),
        .date_y = system_ui_min_i32(height - 34, clock_center_y + clock_h / 2 + system_ui_clamp_i32(short_side / 34, 8, 14)),
    };
}

static void system_ui_home_update_clock_locked(void)
{
    time_t now = time(NULL);
    struct tm tm_now = {0};
    char time_text[16];
    char date_text[32];

    if (now > 0 && localtime_r(&now, &tm_now)) {
        strftime(time_text, sizeof(time_text), "%H:%M", &tm_now);
        strftime(date_text, sizeof(date_text), "%a, %d %b", &tm_now);
    } else {
        snprintf(time_text, sizeof(time_text), "--:--");
        snprintf(date_text, sizeof(date_text), "---  --.--");
    }

    if (s_ui.time_label) {
        lv_label_set_text(s_ui.time_label, time_text);
    }
    if (s_ui.date_label) {
        lv_label_set_text(s_ui.date_label, date_text);
    }
}

static void system_ui_home_clock_timer_cb(lv_timer_t *timer)
{
    (void)timer;
    system_ui_home_update_clock_locked();
}

static void system_ui_home_free_emote_data_locked(void)
{
    free(s_ui.emote_data);
    s_ui.emote_data = NULL;
    s_ui.emote_data_size = 0;
    s_ui.emote_loaded = false;
    s_ui.emote_path[0] = '\0';
}

void system_ui_home_set_emote_paused_locked(bool paused)
{
    s_ui.emote_paused_for_display_claim = paused;
}

void system_ui_home_update_locked(void)
{
    char status[96];
    if (s_ui.sta_connected && s_ui.ap_ssid[0]) {
        snprintf(status, sizeof(status), "WIFI ON  |  AP %s", s_ui.ap_ssid);
    } else if (s_ui.sta_connected) {
        snprintf(status, sizeof(status), "WIFI ON");
    } else if (s_ui.ap_ssid[0]) {
        snprintf(status, sizeof(status), "WIFI OFF  |  AP %s", s_ui.ap_ssid);
    } else {
        snprintf(status, sizeof(status), "WIFI OFF");
    }

    if (s_ui.status_label) {
        lv_label_set_text(s_ui.status_label, status);
    }
    system_ui_home_update_clock_locked();
}

static esp_err_t system_ui_home_create_status_bar_locked(const system_ui_home_layout_t *layout)
{
    int32_t status_w = system_ui_max_i32(80, (int32_t)s_ui.width - layout->pad * 2);
    lv_obj_t *bar = lv_obj_create(s_ui.home_tile);
    ESP_RETURN_ON_FALSE(bar != NULL, ESP_ERR_NO_MEM, SYSTEM_UI_TAG, "create status bar failed");
    lv_obj_set_size(bar, (int32_t)s_ui.width - layout->pad * 2, layout->status_h);
    lv_obj_align(bar, LV_ALIGN_TOP_MID, 0, layout->header_y - 2);
    lv_obj_set_style_bg_opa(bar, LV_OPA_TRANSP, 0);
    lv_obj_set_style_border_width(bar, 0, 0);
    lv_obj_set_style_pad_all(bar, 0, 0);
    lv_obj_set_flex_flow(bar, LV_FLEX_FLOW_ROW);
    lv_obj_set_flex_align(bar, LV_FLEX_ALIGN_CENTER, LV_FLEX_ALIGN_CENTER, LV_FLEX_ALIGN_CENTER);
    lv_obj_clear_flag(bar, LV_OBJ_FLAG_SCROLLABLE);

    /* Keep network state centered at the top as one compact status group. */
    s_ui.status_label = lv_label_create(bar);
    ESP_RETURN_ON_FALSE(s_ui.status_label != NULL, ESP_ERR_NO_MEM, SYSTEM_UI_TAG, "create status failed");
    lv_obj_set_width(s_ui.status_label, status_w);
    lv_label_set_long_mode(s_ui.status_label, LV_LABEL_LONG_SCROLL_CIRCULAR);
    lv_obj_set_style_text_color(s_ui.status_label, system_ui_color(SYSTEM_UI_COLOR_MUTED), 0);
    lv_obj_set_style_text_align(s_ui.status_label, LV_TEXT_ALIGN_CENTER, 0);
    system_ui_apply_font(s_ui.status_label);

    return ESP_OK;
}

static int32_t system_ui_lock_fade_distance(void)
{
    return system_ui_clamp_i32((int32_t)s_ui.height * SYSTEM_UI_LOCK_FADE_DISTANCE_PCT / 100,
                               SYSTEM_UI_LOCK_FADE_MIN_DISTANCE, SYSTEM_UI_LOCK_FADE_MAX_DISTANCE);
}

static void system_ui_lock_screen_set_opa(lv_obj_t *obj, int32_t opa)
{
    opa = system_ui_clamp_i32(opa, LV_OPA_TRANSP, LV_OPA_COVER);
    (void)obj;
    system_ui_lock_screen_apply_text_opa(opa);
}

static void system_ui_lock_screen_opa_anim_cb(void *obj, int32_t opa)
{
    system_ui_lock_screen_set_opa((lv_obj_t *)obj, opa);
}

static void system_ui_lock_screen_opa_anim_done_cb(lv_anim_t *anim)
{
    lv_obj_t *lock_screen = (lv_obj_t *)anim->var;
    lv_opa_t opa = s_ui.status_label ? lv_obj_get_style_text_opa(s_ui.status_label, 0) : LV_OPA_COVER;

    if (opa == LV_OPA_TRANSP) {
        lv_obj_add_flag(lock_screen, LV_OBJ_FLAG_HIDDEN);
        s_ui.lock_unlocked = true;
    } else {
        s_ui.lock_unlocked = false;
    }
    s_ui.lock_drag_active = false;
    s_ui.lock_drag_progress = 0;
}

static void system_ui_lock_screen_animate_to(lv_obj_t *lock_screen, lv_opa_t target_opa)
{
    lv_anim_t anim;
    lv_anim_delete(lock_screen, system_ui_lock_screen_opa_anim_cb);
    lv_anim_init(&anim);
    lv_anim_set_var(&anim, lock_screen);
    lv_anim_set_exec_cb(&anim, system_ui_lock_screen_opa_anim_cb);
    lv_anim_set_values(&anim, s_ui.status_label ? lv_obj_get_style_text_opa(s_ui.status_label, 0) : LV_OPA_COVER, target_opa);
    lv_anim_set_duration(&anim, SYSTEM_UI_LOCK_ANIM_MS);
    lv_anim_set_path_cb(&anim, lv_anim_path_ease_out);
    lv_anim_set_completed_cb(&anim, system_ui_lock_screen_opa_anim_done_cb);
    lv_anim_start(&anim);
}

static void system_ui_lock_screen_unlock_locked(void)
{
    if (!s_ui.home_tile || s_ui.lock_unlocked) {
        return;
    }

    system_ui_lock_screen_animate_to(s_ui.home_tile, LV_OPA_TRANSP);
}

static void system_ui_lock_screen_restore_locked(void)
{
    if (!s_ui.home_tile) {
        return;
    }

    system_ui_lock_screen_animate_to(s_ui.home_tile, LV_OPA_COVER);
}

static void system_ui_lock_screen_apply_text_opa(int32_t opa)
{
    opa = system_ui_clamp_i32(opa, LV_OPA_TRANSP, LV_OPA_COVER);
    if (s_ui.status_label) {
        lv_obj_set_style_text_opa(s_ui.status_label, (lv_opa_t)opa, 0);
    }
    if (s_ui.time_label) {
        lv_obj_set_style_text_opa(s_ui.time_label, (lv_opa_t)opa, 0);
    }
    if (s_ui.date_label) {
        lv_obj_set_style_text_opa(s_ui.date_label, (lv_opa_t)opa, 0);
    }
    if (s_ui.lock_hint_label) {
        lv_obj_set_style_text_opa(s_ui.lock_hint_label, (lv_opa_t)opa, 0);
    }
}

static void system_ui_lock_screen_touch_cb(lv_event_t *event)
{
    lv_event_code_t code = lv_event_get_code(event);
    lv_indev_t *indev = lv_indev_active();
    lv_point_t point = {0};

    if (!s_ui.home_tile || s_ui.lock_unlocked || !indev) {
        return;
    }

    lv_indev_get_point(indev, &point);
    if (code == LV_EVENT_PRESSED) {
        lv_anim_delete(s_ui.home_tile, system_ui_lock_screen_opa_anim_cb);
        s_ui.lock_drag_active = true;
        s_ui.lock_drag_start_y = point.y;
        s_ui.lock_drag_progress = 0;
        system_ui_lock_screen_set_opa(s_ui.home_tile, LV_OPA_COVER);
    } else if (code == LV_EVENT_PRESSING && s_ui.lock_drag_active) {
        int32_t fade_distance = system_ui_lock_fade_distance();
        int32_t progress = system_ui_clamp_i32(s_ui.lock_drag_start_y - point.y, 0, fade_distance);
        int32_t opa = LV_OPA_COVER - progress * LV_OPA_COVER / fade_distance;
        s_ui.lock_drag_progress = progress;
        system_ui_lock_screen_set_opa(s_ui.home_tile, opa);
    } else if ((code == LV_EVENT_RELEASED || code == LV_EVENT_PRESS_LOST) && s_ui.lock_drag_active) {
        int32_t threshold = system_ui_lock_fade_distance() * SYSTEM_UI_LOCK_UNLOCK_THRESHOLD_PCT / 100;
        if (s_ui.lock_drag_progress >= threshold) {
            system_ui_lock_screen_unlock_locked();
        } else {
            system_ui_lock_screen_restore_locked();
        }
    }

    lv_event_stop_bubbling(event);
}

static esp_err_t system_ui_create_home_tile_locked(void)
{
    system_ui_home_layout_t layout = system_ui_home_layout();

    /* The lock screen is an overlay above the launcher, so dragging only changes opacity and never scrolls back. */
    s_ui.home_tile = lv_obj_create(s_ui.home_screen);
    ESP_RETURN_ON_FALSE(s_ui.home_tile != NULL, ESP_ERR_NO_MEM, SYSTEM_UI_TAG, "create lock screen failed");
    lv_obj_set_size(s_ui.home_tile, LV_PCT(100), LV_PCT(100));
    lv_obj_set_style_bg_color(s_ui.home_tile, system_ui_color(SYSTEM_UI_COLOR_BG), 0);
    lv_obj_set_style_bg_opa(s_ui.home_tile, LV_OPA_COVER, 0);
    system_ui_lock_screen_set_opa(s_ui.home_tile, LV_OPA_COVER);
    lv_obj_set_style_border_width(s_ui.home_tile, 0, 0);
    lv_obj_set_style_pad_all(s_ui.home_tile, 0, 0);
    lv_obj_clear_flag(s_ui.home_tile, LV_OBJ_FLAG_SCROLLABLE);
    system_ui_apply_font(s_ui.home_tile);

    ESP_RETURN_ON_ERROR(system_ui_home_create_status_bar_locked(&layout), SYSTEM_UI_TAG, "create status bar failed");

    s_ui.time_label = lv_label_create(s_ui.home_tile);
    ESP_RETURN_ON_FALSE(s_ui.time_label != NULL, ESP_ERR_NO_MEM, SYSTEM_UI_TAG, "create time failed");
    lv_obj_set_size(s_ui.time_label, layout.clock_w, layout.clock_h);
    lv_label_set_long_mode(s_ui.time_label, LV_LABEL_LONG_CLIP);
    lv_obj_set_style_text_color(s_ui.time_label, system_ui_color(SYSTEM_UI_COLOR_TEXT), 0);
    lv_obj_set_style_text_align(s_ui.time_label, LV_TEXT_ALIGN_CENTER, 0);
    system_ui_apply_clock_font(s_ui.time_label);
    lv_obj_align(s_ui.time_label, LV_ALIGN_TOP_MID, 0, layout.clock_y);

    s_ui.date_label = lv_label_create(s_ui.home_tile);
    ESP_RETURN_ON_FALSE(s_ui.date_label != NULL, ESP_ERR_NO_MEM, SYSTEM_UI_TAG, "create date failed");
    lv_obj_set_width(s_ui.date_label, layout.clock_w);
    lv_obj_set_style_text_color(s_ui.date_label, system_ui_color(SYSTEM_UI_COLOR_MUTED), 0);
    lv_obj_set_style_text_align(s_ui.date_label, LV_TEXT_ALIGN_CENTER, 0);
    system_ui_apply_font(s_ui.date_label);
    lv_obj_align(s_ui.date_label, LV_ALIGN_TOP_MID, 0, layout.date_y);

#if CONFIG_ESP_BOARD_DEV_LCD_TOUCH_SUPPORT
    /* Only touch-capable boards should advertise the swipe unlock gesture. */
    s_ui.lock_hint_label = lv_label_create(s_ui.home_tile);
    ESP_RETURN_ON_FALSE(s_ui.lock_hint_label != NULL, ESP_ERR_NO_MEM, SYSTEM_UI_TAG, "create swipe hint failed");
    lv_label_set_text(s_ui.lock_hint_label, "Swipe up to open");
    lv_obj_set_width(s_ui.lock_hint_label, layout.clock_w);
    lv_obj_set_style_text_color(s_ui.lock_hint_label, system_ui_color(SYSTEM_UI_COLOR_MUTED), 0);
    lv_obj_set_style_text_align(s_ui.lock_hint_label, LV_TEXT_ALIGN_CENTER, 0);
    system_ui_apply_font(s_ui.lock_hint_label);
    lv_obj_align(s_ui.lock_hint_label, LV_ALIGN_TOP_MID, 0, layout.swipe_y);
#endif

    lv_obj_t *touch_layer = lv_obj_create(s_ui.home_tile);
    ESP_RETURN_ON_FALSE(touch_layer != NULL, ESP_ERR_NO_MEM, SYSTEM_UI_TAG, "create lock touch layer failed");
    lv_obj_set_size(touch_layer, LV_PCT(100), LV_PCT(100));
    lv_obj_set_style_bg_opa(touch_layer, LV_OPA_TRANSP, 0);
    lv_obj_set_style_border_width(touch_layer, 0, 0);
    lv_obj_set_style_pad_all(touch_layer, 0, 0);
    lv_obj_clear_flag(touch_layer, LV_OBJ_FLAG_SCROLLABLE);
    lv_obj_add_flag(touch_layer, LV_OBJ_FLAG_CLICKABLE);
    lv_obj_add_event_cb(touch_layer, system_ui_lock_screen_touch_cb, LV_EVENT_PRESSED, NULL);
    lv_obj_add_event_cb(touch_layer, system_ui_lock_screen_touch_cb, LV_EVENT_PRESSING, NULL);
    lv_obj_add_event_cb(touch_layer, system_ui_lock_screen_touch_cb, LV_EVENT_RELEASED, NULL);
    lv_obj_add_event_cb(touch_layer, system_ui_lock_screen_touch_cb, LV_EVENT_PRESS_LOST, NULL);

    s_ui.home_clock_timer = lv_timer_create(system_ui_home_clock_timer_cb, 1000, NULL);
    ESP_RETURN_ON_FALSE(s_ui.home_clock_timer != NULL, ESP_ERR_NO_MEM, SYSTEM_UI_TAG, "create home clock timer failed");
    system_ui_home_update_locked();
    return ESP_OK;
}

esp_err_t system_ui_create_home_locked(void)
{
    ESP_RETURN_ON_ERROR(system_ui_launcher_load_locked(), SYSTEM_UI_TAG, "load launcher failed");

    s_ui.home_screen = lv_obj_create(NULL);
    ESP_RETURN_ON_FALSE(s_ui.home_screen != NULL, ESP_ERR_NO_MEM, SYSTEM_UI_TAG, "create home failed");
    lv_obj_set_style_bg_color(s_ui.home_screen, system_ui_color(SYSTEM_UI_COLOR_BG), 0);
    lv_obj_set_style_bg_opa(s_ui.home_screen, LV_OPA_COVER, 0);
    lv_obj_set_style_border_width(s_ui.home_screen, 0, 0);
    lv_obj_set_style_pad_all(s_ui.home_screen, 0, 0);
    lv_obj_clear_flag(s_ui.home_screen, LV_OBJ_FLAG_SCROLLABLE);

    s_ui.tileview = lv_tileview_create(s_ui.home_screen);
    ESP_RETURN_ON_FALSE(s_ui.tileview != NULL, ESP_ERR_NO_MEM, SYSTEM_UI_TAG, "create tileview failed");
    lv_obj_set_size(s_ui.tileview, LV_PCT(100), LV_PCT(100));
    lv_obj_set_style_bg_color(s_ui.tileview, system_ui_color(SYSTEM_UI_COLOR_BG), 0);
    lv_obj_set_style_bg_opa(s_ui.tileview, LV_OPA_COVER, 0);
    lv_obj_set_style_border_width(s_ui.tileview, 0, 0);
    lv_obj_set_style_pad_all(s_ui.tileview, 0, 0);
    lv_obj_set_scrollbar_mode(s_ui.tileview, LV_SCROLLBAR_MODE_OFF);

    ESP_RETURN_ON_ERROR(system_ui_launcher_create_pages_locked(), SYSTEM_UI_TAG, "create launcher pages failed");
    if (s_ui.launcher_first_tile) {
        lv_tileview_set_tile(s_ui.tileview, s_ui.launcher_first_tile, LV_ANIM_OFF);
    }
    ESP_RETURN_ON_ERROR(system_ui_create_home_tile_locked(), SYSTEM_UI_TAG, "create lock screen failed");
    system_ui_load_screen_locked(s_ui.home_screen);
    return ESP_OK;
}

esp_err_t system_ui_show_home(void)
{
    ESP_RETURN_ON_FALSE(s_ui.started && s_ui.home_screen, ESP_ERR_INVALID_STATE,
                        SYSTEM_UI_TAG, "runtime not started");
    ESP_RETURN_ON_FALSE(!display_service_has_exclusive_session(), ESP_ERR_INVALID_STATE,
                        SYSTEM_UI_TAG, "exclusive display session active");
    ESP_RETURN_ON_ERROR(system_ui_lock(), SYSTEM_UI_TAG, "lock failed");
    if (s_ui.launcher_first_tile) {
        lv_tileview_set_tile(s_ui.tileview, s_ui.launcher_first_tile, LV_ANIM_OFF);
    }
    if (s_ui.home_tile) {
        lv_anim_delete(s_ui.home_tile, system_ui_lock_screen_opa_anim_cb);
        lv_obj_remove_flag(s_ui.home_tile, LV_OBJ_FLAG_HIDDEN);
        system_ui_lock_screen_set_opa(s_ui.home_tile, LV_OPA_COVER);
        lv_obj_move_foreground(s_ui.home_tile);
        s_ui.lock_unlocked = false;
        s_ui.lock_drag_active = false;
        s_ui.lock_drag_progress = 0;
    }
    system_ui_load_screen_locked(s_ui.home_screen);
    system_ui_unlock();
    return ESP_OK;
}

esp_err_t system_ui_reload_home(void)
{
    ESP_RETURN_ON_FALSE(s_ui.started, ESP_ERR_INVALID_STATE,
                        SYSTEM_UI_TAG, "runtime not started");
    ESP_RETURN_ON_FALSE(!display_service_has_exclusive_session(), ESP_ERR_INVALID_STATE,
                        SYSTEM_UI_TAG, "exclusive display session active");
    ESP_RETURN_ON_ERROR(system_ui_lock(), SYSTEM_UI_TAG, "lock failed");
    system_ui_delete_home_locked();
    esp_err_t err = system_ui_create_home_locked();
    if (err == ESP_OK) {
        display_service_set_default_screen_locked(s_ui.home_screen);
    } else {
        system_ui_delete_home_locked();
        display_service_set_default_screen_locked(NULL);
    }
    system_ui_unlock();
    return err;
}

void system_ui_delete_home_locked(void)
{
    if (s_ui.home_clock_timer) {
        lv_timer_delete(s_ui.home_clock_timer);
        s_ui.home_clock_timer = NULL;
    }
    if (s_ui.home_screen) {
        lv_obj_delete(s_ui.home_screen);
    }
    system_ui_launcher_delete_locked();
    s_ui.home_screen = NULL;
    s_ui.home_tile = NULL;
    s_ui.tileview = NULL;
    s_ui.launcher_first_tile = NULL;
    s_ui.emote_anim = NULL;
    s_ui.status_label = NULL;
    s_ui.time_label = NULL;
    s_ui.date_label = NULL;
    s_ui.lock_hint_label = NULL;
    s_ui.lock_unlocked = false;
    s_ui.lock_drag_active = false;
    s_ui.lock_drag_start_y = 0;
    s_ui.lock_drag_progress = 0;
    s_ui.emote_paused_for_display_claim = false;
    system_ui_home_free_emote_data_locked();
}

esp_err_t system_ui_set_network_status(bool sta_connected, const char *ap_ssid)
{
    system_ui_work_event_t event = {
        .type = SYSTEM_UI_WORK_EVENT_NETWORK_STATUS,
    };

    /* Network callbacks run from the system event task, so keep this path non-blocking and let the UI event task take the LVGL lock. */
    ESP_RETURN_ON_FALSE(s_ui.started, ESP_ERR_INVALID_STATE, SYSTEM_UI_TAG, "runtime not started");
    event.generation = s_ui.generation;
    event.network_status.sta_connected = sta_connected;
    strlcpy(event.network_status.ap_ssid, ap_ssid ? ap_ssid : "", sizeof(event.network_status.ap_ssid));
    return system_ui_post_work_event(&event, pdMS_TO_TICKS(100));
}
