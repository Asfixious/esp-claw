//! `esp_err_t` mapping shared across the claw_modules Rust crates.
//!
//! These mirror the ESP-IDF `esp_err.h` values exactly so the C ABI exported by
//! each crate returns identical codes to the original C implementation.

/// Rust mirror of `esp_err_t` (a 32-bit signed integer in ESP-IDF).
pub type EspErr = i32;

pub const ESP_OK: EspErr = 0;
pub const ESP_FAIL: EspErr = -1;

pub const ESP_ERR_NO_MEM: EspErr = 0x101;
pub const ESP_ERR_INVALID_ARG: EspErr = 0x102;
pub const ESP_ERR_INVALID_STATE: EspErr = 0x103;
pub const ESP_ERR_INVALID_SIZE: EspErr = 0x104;
pub const ESP_ERR_NOT_FOUND: EspErr = 0x105;
pub const ESP_ERR_NOT_SUPPORTED: EspErr = 0x106;
pub const ESP_ERR_TIMEOUT: EspErr = 0x107;
pub const ESP_ERR_INVALID_RESPONSE: EspErr = 0x108;
pub const ESP_ERR_NO_MEM_DATA: EspErr = 0x109;
