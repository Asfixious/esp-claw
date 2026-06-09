//! `esp_err_to_name` equivalent for the error codes the core produces.

use claw_interfaces::error::*;

/// Mirror of `esp_err_to_name` for the codes used by claw_core. Used for the
/// tool-error fallback text and default error messages.
pub fn esp_err_to_name(err: EspErr) -> &'static str {
    match err {
        ESP_OK => "ESP_OK",
        ESP_FAIL => "ESP_FAIL",
        ESP_ERR_NO_MEM => "ESP_ERR_NO_MEM",
        ESP_ERR_INVALID_ARG => "ESP_ERR_INVALID_ARG",
        ESP_ERR_INVALID_STATE => "ESP_ERR_INVALID_STATE",
        ESP_ERR_INVALID_SIZE => "ESP_ERR_INVALID_SIZE",
        ESP_ERR_NOT_FOUND => "ESP_ERR_NOT_FOUND",
        ESP_ERR_NOT_SUPPORTED => "ESP_ERR_NOT_SUPPORTED",
        ESP_ERR_TIMEOUT => "ESP_ERR_TIMEOUT",
        ESP_ERR_INVALID_RESPONSE => "ESP_ERR_INVALID_RESPONSE",
        _ => "UNKNOWN ERROR",
    }
}
