//! `log::Log` backend that forwards the `log` facade to ESP-IDF's `ESP_LOGx`.
//!
//! `ESP_LOGx` are C macros, so a tiny C shim `claw_rs_log(level, tag, msg)` (see
//! `csrc/claw_rs_log.c`) bridges to `esp_log_write`. On the host the backend
//! prints to stderr so `cargo test -- --nocapture` shows module logs.

#[cfg(target_os = "espidf")]
use core::ffi::c_char;
use core::ffi::c_int;
use std::ffi::CString;

use log::{Level, LevelFilter, Metadata, Record};

/// ESP log levels (`esp_log_level_t`).
const ESP_LOG_ERROR: c_int = 1;
const ESP_LOG_WARN: c_int = 2;
const ESP_LOG_INFO: c_int = 3;
const ESP_LOG_DEBUG: c_int = 4;
const ESP_LOG_VERBOSE: c_int = 5;

#[cfg(target_os = "espidf")]
extern "C" {
    /// Defined in `csrc/claw_rs_log.c`; calls `esp_log_write`.
    fn claw_rs_log(level: c_int, tag: *const c_char, msg: *const c_char);
}

fn map_level(level: Level) -> c_int {
    match level {
        Level::Error => ESP_LOG_ERROR,
        Level::Warn => ESP_LOG_WARN,
        Level::Info => ESP_LOG_INFO,
        Level::Debug => ESP_LOG_DEBUG,
        Level::Trace => ESP_LOG_VERBOSE,
    }
}

fn emit(level: c_int, tag: &str, msg: &str) {
    #[cfg(target_os = "espidf")]
    {
        let tag_c = CString::new(tag).unwrap_or_else(|_| CString::new("rust").unwrap());
        let msg_c = CString::new(msg).unwrap_or_else(|_| CString::new("(invalid log)").unwrap());
        unsafe { claw_rs_log(level, tag_c.as_ptr(), msg_c.as_ptr()) };
    }
    #[cfg(not(target_os = "espidf"))]
    {
        let _ = (level, CString::new(tag));
        eprintln!("[{tag}] {msg}");
    }
}

struct ClawLogger;

impl log::Log for ClawLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        emit(
            map_level(record.level()),
            record.target(),
            &record.args().to_string(),
        );
    }

    fn flush(&self) {}
}

static LOGGER: ClawLogger = ClawLogger;

/// Install the `ESP_LOGx`-backed global logger. Safe to call more than once;
/// later calls are ignored.
pub fn init_logger() {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(LevelFilter::Debug);
    }
}
