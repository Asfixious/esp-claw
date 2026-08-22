#include "esp_err.h"
#include "esp_log.h"
#include "esp_lcd_touch_gt911.h"
#include "driver/i2c_master.h"
#include "esp_board_periph.h"
#include "dev_custom.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "driver/gpio.h"
static const char *TAG = "WS43C_SETUP";
static esp_err_t ws43c_backlight_on(void)
{
    void *handle = NULL;

    esp_err_t ret = esp_board_periph_ref_handle("i2c_master", &handle);
    if (ret != ESP_OK || handle == NULL) {
        ESP_LOGE(TAG, "Failed to get i2c_master");
        return ESP_FAIL;
    }

    i2c_master_bus_handle_t bus = (i2c_master_bus_handle_t)handle;
    i2c_master_dev_handle_t ch422g = NULL;

    i2c_device_config_t cfg = {
        .dev_addr_length = I2C_ADDR_BIT_LEN_7,
        .device_address = 0x24,
        .scl_speed_hz = 400000,
    };

    ret = i2c_master_bus_add_device(bus, &cfg, &ch422g);

    if (ret == ESP_OK) {
        uint8_t mode[] = {0x02, 0xFF};
        ret = i2c_master_transmit(ch422g, mode, sizeof(mode), 100);
    }

    if (ret == ESP_OK) {
        uint8_t output[] = {0x03, 0xF7};
        ret = i2c_master_transmit(ch422g, output, sizeof(output), 100);
    }

    if (ch422g != NULL) {
        i2c_master_bus_rm_device(ch422g);
    }

    esp_board_periph_unref_handle("i2c_master");
    return ret;
}
    static void ws43c_backlight_task(void *arg)
    
{
    vTaskDelay(pdMS_TO_TICKS(500)); // Wait for .5 second before starting the backlight task
    
    for (int i = 0; i < 50; i++) {
        esp_err_t ret = ws43c_backlight_on();

        if (ret == ESP_OK) {
            ESP_LOGI(TAG, "Backlight ON");
            vTaskDelete(NULL);
            return;
        }

        vTaskDelay(pdMS_TO_TICKS(100));
    }
    ESP_LOGE(TAG, "Backlight failed after retries");
    vTaskDelete(NULL);
}

static void __attribute__((constructor)) ws43c_start_backlight_task(void)
{
    xTaskCreate(ws43c_backlight_task,
                "ws43c_bl",
                3072,
                NULL,
                5,
                NULL);
}
static esp_err_t ws43c_touch_reset(void)
{
    void *handle = NULL;

    esp_err_t ret = esp_board_periph_ref_handle("i2c_master", &handle);
    if (ret != ESP_OK || handle == NULL) {
        ESP_LOGE(TAG, "Failed to get i2c_master for touch reset");
        return ESP_FAIL;
    }

    i2c_master_bus_handle_t bus = (i2c_master_bus_handle_t)handle;
    i2c_master_dev_handle_t io_expander = NULL;

    i2c_device_config_t cfg = {
        .dev_addr_length = I2C_ADDR_BIT_LEN_7,
        .device_address = 0x24,
        .scl_speed_hz = 400000,
    };

    ret = i2c_master_bus_add_device(bus, &cfg, &io_expander);

    if (ret == ESP_OK) {
        uint8_t mode[] = {0x02, 0xFF};
        ret = i2c_master_transmit(io_expander, mode, sizeof(mode), 100);
    }

    /* IO1 low = GT911 reset asserted */
    if (ret == ESP_OK) {
        uint8_t output_low[] = {0x03, 0xF5};
        ret = i2c_master_transmit(io_expander, output_low,
                                  sizeof(output_low), 100);
    }

    if (ret == ESP_OK) {
        gpio_set_direction(GPIO_NUM_4, GPIO_MODE_OUTPUT);
        gpio_set_level(GPIO_NUM_4, 0);

        vTaskDelay(pdMS_TO_TICKS(100));

        /* IO1 high = release GT911 reset */
        uint8_t output_high[] = {0x03, 0xF7};
        ret = i2c_master_transmit(io_expander, output_high,
                                  sizeof(output_high), 100);

        vTaskDelay(pdMS_TO_TICKS(200));

        /* Release GT911 INT line after address selection */
        gpio_set_direction(GPIO_NUM_4, GPIO_MODE_INPUT);
    }

    if (io_expander != NULL) {
        i2c_master_bus_rm_device(io_expander);
    }

    esp_board_periph_unref_handle("i2c_master");

    return ret;
}
esp_err_t lcd_touch_factory_entry_t(
    esp_lcd_panel_io_handle_t io,
    const esp_lcd_touch_config_t *touch_dev_config,
    esp_lcd_touch_handle_t *ret_touch)
{
    esp_err_t ret = ws43c_touch_reset();
    if (ret != ESP_OK) {
        ESP_LOGE(TAG, "GT911 hardware reset failed");
        return ret;
    }

    return esp_lcd_touch_new_i2c_gt911(io, touch_dev_config, ret_touch);
}