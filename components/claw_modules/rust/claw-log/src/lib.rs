//! Upper-layer logging for the claw firmware: the [`log`] facade backend and the
//! flat-tree `tracing` subscriber.
//!
//! - **On device (`espidf`):** both write through `claw_sys`'s `ESP_LOGx` bridge
//!   ([`claw_sys::log_sink`]).
//! - **On host:** the `log` facade is backed by [`env_logger`] with a custom
//!   format, and the `tracing` sink re-enters the `log` facade — so both flow
//!   through `env_logger`.
//!
//! All three sources share one line format: ESP-IDF's `<L> (<ms>) <tag>: <msg>`
//! with ESP-IDF's per-level colors. On device that comes from `ESP_LOGx`; on host
//! the `env_logger` format closure (see `install_logger`) reproduces it.
//!
//! The two streams are independent (no `tracing/log-always`, no `LogTracer`), so
//! a `tracing` event never re-emits as a `log` record.
//!
//! Compile-time level ceilings are selected via the `log_max_*` / `trace_max_*`
//! Cargo features (see `Cargo.toml`); the runtime ceiling is [`init_logger`]'s
//! `max_level` argument (authoritative — `env_logger` does NOT read `RUST_LOG`),
//! and on device ESP-IDF's `esp_log_level_set` / `CONFIG_LOG_DEFAULT_LEVEL`.

pub mod trace;

use log::Level;
use tracing::Level as TraceLevel;

pub use log::LevelFilter;
pub use trace::{FlatTreeSubscriber, TraceSink};

/// Device-only `log::Log` backend: bridges the `log` facade to `claw_sys`'s
/// `ESP_LOGx` sink. On host this role is filled by `env_logger` instead.
#[cfg(target_os = "espidf")]
struct ClawLogger;

#[cfg(target_os = "espidf")]
impl log::Log for ClawLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        claw_sys::log_sink::write(record.level(), record.target(), &record.args().to_string());
    }

    fn flush(&self) {}
}

#[cfg(target_os = "espidf")]
static LOGGER: ClawLogger = ClawLogger;

/// Install the global `log` facade backend, capped at `max_level` — the device
/// `ESP_LOGx` bridge on `espidf`, [`env_logger`] on host.
///
/// `max_level` is the **runtime** gate. On device the `log` macros default to
/// [`LevelFilter::Off`] until it is set, so without it nothing — not even
/// `error!` — is emitted; on host it is `env_logger`'s authoritative filter
/// (`RUST_LOG` is intentionally NOT consulted). It layers under the compile-time
/// `log_max_*` ceiling (which strips higher-level macros from release builds, so
/// `max_level` can only narrow further) and, on device, ESP-IDF's runtime
/// `esp_log_level_set` / `CONFIG_LOG_DEFAULT_LEVEL`.
///
/// Pass [`LevelFilter::Trace`] to defer all filtering to those other gates.
///
/// # Errors
///
/// Returns [`log::SetLoggerError`] if a global logger is already installed (the
/// `log` facade allows exactly one).
pub fn init_logger(max_level: LevelFilter) -> Result<(), log::SetLoggerError> {
    install_logger(max_level)
}

#[cfg(target_os = "espidf")]
fn install_logger(max_level: LevelFilter) -> Result<(), log::SetLoggerError> {
    log::set_logger(&LOGGER)?;
    log::set_max_level(max_level);
    Ok(())
}

/// ESP-IDF's per-level presentation: single letter (`E`/`W`/`I`/`D`/`V`) and the
/// matching ANSI SGR color (`None` = uncolored, as ESP-IDF leaves debug/verbose).
/// Kept in sync with `esp_log.h` so host output matches the device.
#[cfg(not(target_os = "espidf"))]
fn esp_idf_style(level: Level) -> (char, Option<&'static str>) {
    match level {
        Level::Error => ('E', Some("31")), // red
        Level::Warn => ('W', Some("33")),  // yellow
        Level::Info => ('I', Some("32")),  // green
        Level::Debug => ('D', None),
        Level::Trace => ('V', None),
    }
}

#[cfg(not(target_os = "espidf"))]
fn install_logger(max_level: LevelFilter) -> Result<(), log::SetLoggerError> {
    use std::io::Write;
    use std::sync::OnceLock;
    use std::time::Instant;

    // Anchored at logger init so the `(ms)` column mirrors ESP-IDF's boot uptime.
    static START: OnceLock<Instant> = OnceLock::new();

    env_logger::Builder::new()
        // No `parse_env`/`RUST_LOG`: `max_level` is the single authoritative filter.
        .filter_level(max_level)
        // Mirror ESP-IDF's `<L> (<ms>) <tag>: <msg>`; anstream strips the ANSI
        // when stderr is not a TTY.
        .format(|formatter, record| {
            let uptime_ms = START.get_or_init(Instant::now).elapsed().as_millis();
            let (letter, color) = esp_idf_style(record.level());
            let (tag, message) = (record.target(), record.args());
            match color {
                Some(code) => writeln!(
                    formatter,
                    "\x1b[0;{code}m{letter} ({uptime_ms}) {tag}: {message}\x1b[0m"
                ),
                None => writeln!(formatter, "{letter} ({uptime_ms}) {tag}: {message}"),
            }
        })
        .try_init()
}

/// Map a `tracing` level to the `log::Level` the sink expects.
fn to_log_level(level: TraceLevel) -> Level {
    match level {
        TraceLevel::ERROR => Level::Error,
        TraceLevel::WARN => Level::Warn,
        TraceLevel::INFO => Level::Info,
        TraceLevel::DEBUG => Level::Debug,
        TraceLevel::TRACE => Level::Trace,
    }
}

/// The `tracing` sink: forwards each already-formatted line to the same backend
/// as the `log` facade — `claw_sys`'s `ESP_LOGx` bridge on device, the `log`
/// facade (hence `env_logger`) on host — so trace and `log` records share one
/// output.
struct ClawTraceSink;

impl TraceSink for ClawTraceSink {
    fn write_line(&self, level: TraceLevel, tag: &str, line: &str) {
        #[cfg(target_os = "espidf")]
        claw_sys::log_sink::write(to_log_level(level), tag, line);
        #[cfg(not(target_os = "espidf"))]
        log::log!(target: tag, to_log_level(level), "{line}");
    }
}

/// Install the flat-tree `tracing` subscriber. Its sink forwards to the same
/// backend as the `log` facade (`ESP_LOGx` on device, `env_logger` on host).
///
/// Pair it with [`init_logger`] so plain `log::` records are emitted too; the
/// two streams are independent (no `tracing/log-always`, no `LogTracer`), so
/// there is no risk of a log<->trace loop.
///
/// # Errors
///
/// Returns [`tracing::subscriber::SetGlobalDefaultError`] if a global subscriber
/// is already installed (`tracing` allows exactly one).
pub fn init_tracing() -> Result<(), tracing::subscriber::SetGlobalDefaultError> {
    tracing::subscriber::set_global_default(FlatTreeSubscriber::with_sink(ClawTraceSink))
}
