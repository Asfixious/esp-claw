//! `claw_sys` — the thin shims to C/IDF facilities that Rust `std` cannot
//! express on its own: the `ESP_LOGx` log backend and the `esp_http_client`
//! networking driver.

pub mod http;
pub mod log_backend;
pub mod thread;

pub use log_backend::init_logger;

#[cfg(target_os = "espidf")]
pub use http::EspIdfHttp;

#[cfg(test)]
mod tests {
    use super::http::{build_auth_header, parse_error_message_body};

    #[test]
    fn auth_header_bearer_default() {
        let (name, value) = build_auth_header(None, Some("sk-123")).unwrap();
        assert_eq!(name, "Authorization");
        assert_eq!(value, "Bearer sk-123");
    }

    #[test]
    fn auth_header_api_key() {
        let (name, value) = build_auth_header(Some("api-key"), Some("sk-123")).unwrap();
        assert_eq!(name, "X-API-Key");
        assert_eq!(value, "sk-123");
    }

    #[test]
    fn auth_header_none_or_empty() {
        assert!(build_auth_header(Some("none"), Some("sk-123")).is_none());
        assert!(build_auth_header(Some("bearer"), Some("")).is_none());
        assert!(build_auth_header(Some("bearer"), None).is_none());
    }

    #[test]
    fn error_body_prefers_error_message() {
        let body = r#"{"error":{"message":"bad key"}}"#;
        assert_eq!(parse_error_message_body(body, 401), "HTTP 401: bad key");
    }

    #[test]
    fn error_body_top_level_message() {
        let body = r#"{"message":"rate limited"}"#;
        assert_eq!(parse_error_message_body(body, 429), "HTTP 429: rate limited");
    }

    #[test]
    fn error_body_non_json_truncates() {
        assert_eq!(parse_error_message_body("oops", 500), "HTTP 500: oops");
        assert_eq!(parse_error_message_body("", 500), "HTTP 500");
    }
}
