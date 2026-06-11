//! `ClawHttp` driver over `esp_http_client`, porting `claw_llm_http_transport.c`.
//!
//! The pure-Rust helpers (auth header construction, error-body parsing) are
//! host-testable; only the `esp_http_client` plumbing is gated to the espidf
//! target.

/// Decide the auth header `(name, value)` for the given auth type + key.
///
/// Mirrors `build_auth_header_value` + `auth_header_name`: returns `None` when
/// the key is empty or the auth type is `"none"`.
#[cfg_attr(not(any(test, target_os = "espidf")), allow(dead_code))]
pub(crate) fn build_auth_header(auth_type: Option<&str>, api_key: Option<&str>) -> Option<(&'static str, String)> {
    let kind = auth_type.unwrap_or("bearer");
    let key = api_key.unwrap_or("");
    if key.is_empty() || kind == "none" {
        return None;
    }
    Some(match kind {
        "api-key" => ("X-API-Key", key.to_string()),
        _ => ("Authorization", format!("Bearer {key}")),
    })
}

/// Build the error message for a non-200 response, mirroring
/// `parse_error_message_body`: prefer `error.message`, then top-level
/// `message`, else a truncated body echo.
#[cfg_attr(not(any(test, target_os = "espidf")), allow(dead_code))]
pub(crate) fn parse_error_message_body(body: &str, status: i32) -> String {
    if body.is_empty() {
        return format!("HTTP {status}");
    }
    match serde_json::from_str::<serde_json::Value>(body).ok().and_then(|root| extract_message(&root)) {
        Some(msg) => format!("HTTP {status}: {msg}"),
        None => format!("HTTP {status}: {}", truncate(body, 160)),
    }
}

/// First non-empty string among `error.message` then top-level `message`.
#[cfg_attr(not(any(test, target_os = "espidf")), allow(dead_code))]
fn extract_message(root: &serde_json::Value) -> Option<String> {
    let nested = root.get("error").and_then(|e| e.get("message"));
    [nested, root.get("message")]
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .find(|s| !s.is_empty())
        .map(str::to_owned)
}

#[cfg_attr(not(any(test, target_os = "espidf")), allow(dead_code))]
pub(crate) fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(target_os = "espidf")]
pub use espidf_driver::EspIdfHttp;

#[cfg(target_os = "espidf")]
mod espidf_driver {
    use super::{build_auth_header, parse_error_message_body};
    use claw_interfaces::error::{ESP_ERR_INVALID_STATE, ESP_FAIL, ESP_OK};
    use claw_interfaces::http::{ClawHttp, HttpError, HttpJsonRequest, HttpResponse};
    use core::ffi::{c_char, c_int, c_void};
    use core::sync::atomic::{AtomicBool, Ordering};
    use std::ffi::CString;

    // --- esp_http_client FFI ------------------------------------------------
    #[repr(C)]
    struct esp_http_client_event_t {
        event_id: c_int,
        client: *mut c_void,
        data: *mut c_void,
        data_len: c_int,
        user_data: *mut c_void,
        header_key: *mut c_char,
        header_value: *mut c_char,
    }

    // esp_http_client_event_id_t: ERROR=0, ON_CONNECTED=1, HEADERS_SENT=2,
    // ON_HEADER=3, ON_DATA=4, ON_FINISH=5, ...
    const HTTP_EVENT_ON_DATA: c_int = 4;
    const HTTP_METHOD_POST: c_int = 1;

    type HttpEventHandleCb = unsafe extern "C" fn(*mut esp_http_client_event_t) -> c_int;
    type CrtBundleAttachFn = unsafe extern "C" fn(*mut c_void) -> c_int;

    // Only the prefix fields we set are declared; the rest of the struct is
    // zeroed by the caller and ignored by us. esp_http_client reads the full
    // struct, so we must match its real layout. We therefore use the documented
    // field order from esp_http_client.h.
    #[repr(C)]
    struct esp_http_client_config_t {
        url: *const c_char,
        host: *const c_char,
        port: c_int,
        username: *const c_char,
        password: *const c_char,
        auth_type: c_int,
        path: *const c_char,
        query: *const c_char,
        cert_pem: *const c_char,
        cert_len: usize,
        client_cert_pem: *const c_char,
        client_cert_len: usize,
        client_key_pem: *const c_char,
        client_key_len: usize,
        client_key_password: *const c_char,
        client_key_password_len: usize,
        tls_version: c_int,
        user_agent: *const c_char,
        method: c_int,
        timeout_ms: c_int,
        disable_auto_redirect: bool,
        max_redirection_count: c_int,
        max_authorization_retries: c_int,
        event_handler: Option<HttpEventHandleCb>,
        transport_type: c_int,
        buffer_size: c_int,
        buffer_size_tx: c_int,
        user_data: *mut c_void,
        is_async: bool,
        use_global_ca_store: bool,
        skip_cert_common_name_check: bool,
        common_name: *const c_char,
        crt_bundle_attach: Option<CrtBundleAttachFn>,
        keep_alive_enable: bool,
        keep_alive_idle: c_int,
        keep_alive_interval: c_int,
        keep_alive_count: c_int,
        if_name: *mut c_void,
        // Remaining bitfields/ports/cfg are left out; esp_http_client only reads
        // them when the corresponding feature flags are set, none of which we
        // enable. The struct is zero-initialized, so trailing fields read as 0.
        _reserved: [u8; 64],
    }

    extern "C" {
        fn esp_http_client_init(config: *const esp_http_client_config_t) -> *mut c_void;
        fn esp_http_client_set_method(client: *mut c_void, method: c_int) -> c_int;
        fn esp_http_client_set_header(client: *mut c_void, key: *const c_char, value: *const c_char) -> c_int;
        fn esp_http_client_set_post_field(client: *mut c_void, data: *const c_char, len: c_int) -> c_int;
        fn esp_http_client_perform(client: *mut c_void) -> c_int;
        fn esp_http_client_get_status_code(client: *mut c_void) -> c_int;
        fn esp_http_client_cleanup(client: *mut c_void) -> c_int;
        fn esp_crt_bundle_attach(conf: *mut c_void) -> c_int;
        fn esp_err_to_name(err: c_int) -> *const c_char;
    }

    struct RequestCtx {
        body: Vec<u8>,
        abort: *const AtomicBool,
    }

    extern "C" fn http_event_handler(evt: *mut esp_http_client_event_t) -> c_int {
        unsafe {
            let evt = &*evt;
            let ctx = evt.user_data as *mut RequestCtx;
            if ctx.is_null() {
                return ESP_OK;
            }
            let ctx = &mut *ctx;
            if !ctx.abort.is_null() && (*ctx.abort).load(Ordering::Relaxed) {
                return ESP_FAIL;
            }
            if evt.event_id == HTTP_EVENT_ON_DATA && evt.data_len > 0 {
                let slice = core::slice::from_raw_parts(evt.data as *const u8, evt.data_len as usize);
                ctx.body.extend_from_slice(slice);
            }
        }
        ESP_OK
    }

    fn err_name(err: c_int) -> String {
        unsafe {
            let p = esp_err_to_name(err);
            if p.is_null() {
                return format!("{err}");
            }
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }

    /// `esp_http_client`-backed [`ClawHttp`].
    pub struct EspIdfHttp;

    impl ClawHttp for EspIdfHttp {
        fn post_json(&self, request: &HttpJsonRequest, abort: &AtomicBool) -> Result<HttpResponse, HttpError> {
            let url = CString::new(request.url)
                .map_err(|_| HttpError { err: ESP_FAIL, message: "invalid url".into() })?;
            let mut ctx = Box::new(RequestCtx { body: Vec::with_capacity(4096), abort: abort as *const _ });

            let mut config: esp_http_client_config_t = unsafe { core::mem::zeroed() };
            config.url = url.as_ptr();
            config.event_handler = Some(http_event_handler);
            config.user_data = (&mut *ctx as *mut RequestCtx) as *mut c_void;
            config.timeout_ms = request.timeout_ms as c_int;
            config.buffer_size = 4096;
            config.buffer_size_tx = 4096;
            config.crt_bundle_attach = Some(esp_crt_bundle_attach);

            let client = unsafe { esp_http_client_init(&config) };
            if client.is_null() {
                return Err(HttpError { err: ESP_FAIL, message: "Failed to create HTTP client".into() });
            }

            let result = (|| unsafe {
                esp_http_client_set_method(client, HTTP_METHOD_POST);
                let ct_key = CString::new("Content-Type").unwrap();
                let ct_val = CString::new("application/json").unwrap();
                esp_http_client_set_header(client, ct_key.as_ptr(), ct_val.as_ptr());

                if let Some((name, value)) = build_auth_header(request.auth_type, request.api_key) {
                    let k = CString::new(name).unwrap();
                    if let Ok(v) = CString::new(value) {
                        esp_http_client_set_header(client, k.as_ptr(), v.as_ptr());
                    }
                }
                for h in request.headers {
                    if h.name.is_empty() {
                        continue;
                    }
                    if let (Ok(k), Ok(v)) = (CString::new(h.name), CString::new(h.value)) {
                        esp_http_client_set_header(client, k.as_ptr(), v.as_ptr());
                    }
                }
                let body = CString::new(request.body).map_err(|_| HttpError { err: ESP_FAIL, message: "body has NUL".into() })?;
                esp_http_client_set_post_field(client, body.as_ptr(), request.body.len() as c_int);

                let err = esp_http_client_perform(client);
                let aborted = abort.load(Ordering::Relaxed);
                if err != ESP_OK {
                    if aborted {
                        return Err(HttpError { err: ESP_ERR_INVALID_STATE, message: "HTTP request aborted by caller".into() });
                    }
                    return Err(HttpError { err, message: format!("HTTP request failed: {}", err_name(err)) });
                }
                if aborted {
                    return Err(HttpError { err: ESP_ERR_INVALID_STATE, message: "HTTP request aborted by caller".into() });
                }
                let status = esp_http_client_get_status_code(client);
                let body_str = String::from_utf8_lossy(&ctx.body).into_owned();
                if status != 200 {
                    return Err(HttpError { err: ESP_FAIL, message: parse_error_message_body(&body_str, status) });
                }
                Ok(HttpResponse { status_code: status, body: body_str })
            })();

            unsafe { esp_http_client_cleanup(client) };
            result
        }
    }
}
