//! `ClawHttp`/`StreamingHttp` drivers over `esp_http_client`, porting
//! `claw_llm_http_transport.c`.
//!
//! The pure-Rust helpers (auth header construction, error-body parsing) are
//! host-testable; only the `esp_http_client` plumbing is gated to the espidf
//! target.

use claw_interface::http::HttpStatusCode;

/// Build the error message for a non-200 response, mirroring
/// `parse_error_message_body`: prefer `error.message`, then top-level
/// `message`, else a truncated body echo.
#[cfg_attr(not(target_os = "espidf"), allow(dead_code))]
fn parse_error_message_body(body: &str, status: HttpStatusCode) -> String {
    if body.is_empty() {
        return format!("HTTP {status}");
    }
    match serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|root| extract_message(&root))
    {
        Some(msg) => format!("HTTP {status}: {msg}"),
        None => format!("HTTP {status}: {}", truncate(body, 160)),
    }
}

/// First non-empty string among `error.message` then top-level `message`.
#[cfg_attr(not(target_os = "espidf"), allow(dead_code))]
fn extract_message(root: &serde_json::Value) -> Option<String> {
    let nested = root.get("error").and_then(|e| e.get("message"));
    [nested, root.get("message")]
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .find(|s| !s.is_empty())
        .map(str::to_owned)
}

#[cfg_attr(not(target_os = "espidf"), allow(dead_code))]
fn truncate(s: &str, max: usize) -> &str {
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
pub use espidf_driver::{EspHttpByteStream, EspIdfHttp};

#[cfg(target_os = "espidf")]
mod espidf_driver {
    use super::parse_error_message_body;
    use claw_interface::http::{
        blocking, Cancel, ClawHttp, HttpError, HttpGetRequest, HttpJsonRequest, HttpRequestFailure,
        HttpResponse, HttpResponseFuture, HttpStatusCode, StreamingHttp,
    };
    use core::ffi::{c_char, c_int, c_void};
    use core::future::Future;
    use core::pin::Pin;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use core::task::{Context, Poll};
    use futures_lite::Stream;
    use std::ffi::CString;
    use std::time::{Duration, Instant};

    const DEFAULT_INITIAL_URL: &str = "http://127.0.0.1/";

    // `esp_err_t` sentinels from components/esp_common/include/esp_err.h.
    const ESP_OK: c_int = 0;
    const ESP_FAIL: c_int = -1;
    /// ESP-IDF's async HTTP sentinel when an operation is still in progress
    /// (`ESP_ERR_HTTP_BASE + 7`, see `esp_http_client.h`).
    const ESP_ERR_HTTP_EAGAIN: c_int = 0x7007;
    const ERRNO_NONE: c_int = 0;
    const ERRNO_EAGAIN: c_int = 11;
    const ERRNO_EINPROGRESS: c_int = 115;
    const TLS_WANT_READ: c_int = -0x6900;
    const TLS_WANT_WRITE: c_int = -0x6880;
    const ESP_LOG_ERROR: c_int = 1;

    /// Give ESP-IDF's first async TCP-connect poll enough time to complete.
    /// Its TLS transport reuses an `fd_set` after `select`; a timeout clears
    /// that set, so an unrealistically tiny first poll can never make progress.
    /// One second remains safely below the task watchdog window.
    const HTTP_CONNECT_IO_TIMEOUT_MS: u32 = 1_000;
    /// Once connected, each manual write/fetch/read step gets only a small
    /// slice. `esp_http_client_read` preserves a no-data timeout as `-EAGAIN`,
    /// so the stream can return `Pending` and let the Session rotate agents.
    const HTTP_BODY_IO_SLICE_TIMEOUT_MS: u32 = 10;
    /// Give FreeRTOS's idle and system tasks a real scheduling window after a
    /// native async step reports that it would block.
    const HTTP_RETRY_DELAY_MS: u64 = 1;
    const MAX_REDIRECTS: u8 = 10;
    /// ESP-IDF's TLS/lwIP path becomes unstable when multiple long-lived LLM
    /// SSE responses are driven concurrently on this target: sockets are reset
    /// underneath otherwise independent `esp_http_client` handles. Keep the
    /// Agents themselves concurrent, but admit one Rust LLM stream at a time.
    /// Waiting is cooperative (`yield_once`), so the Session can continue to
    /// poll every Agent and capability while the active stream makes progress.
    const STREAM_LIMIT: usize = 1;
    static ACTIVE_STREAMS: AtomicUsize = AtomicUsize::new(0);
    static HTTP_POLL_LOGS_FILTERED: AtomicBool = AtomicBool::new(false);

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

    // ESP-IDF v6.1's esp_http_client_event_id_t inserts
    // ON_HEADERS_COMPLETE and ON_STATUS_CODE between ON_HEADER and ON_DATA.
    // `claw_rs_idf_compat.c` asserts every value consumed here so an SDK enum
    // change fails the firmware build instead of silently dropping body data.
    const HTTP_EVENT_ON_CONNECTED: c_int = 1;
    const HTTP_EVENT_ON_HEADER: c_int = 3;
    const HTTP_EVENT_ON_STATUS_CODE: c_int = 5;
    const HTTP_EVENT_ON_DATA: c_int = 6;
    const HTTP_EVENT_ON_FINISH: c_int = 7;
    const HTTP_METHOD_GET: c_int = 0;
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
        // Remaining optional fields and addr_type are left out. The C-side ABI
        // guard verifies that they still fit here; zero initialization keeps
        // every omitted option disabled and addr_type at its default value.
        _reserved: [u8; 64],
    }

    // `claw_rs_idf_compat.c` asserts the corresponding ESP-IDF C layout. Keep
    // these assertions in Rust as the other half of that ABI contract.
    const _: [(); 208] = [(); core::mem::size_of::<esp_http_client_config_t>()];
    const _: [(); 56] = [(); core::mem::offset_of!(esp_http_client_config_t, client_key_password)];
    const _: [(); 92] = [(); core::mem::offset_of!(esp_http_client_config_t, event_handler)];
    const _: [(); 96] = [(); core::mem::offset_of!(esp_http_client_config_t, transport_type)];
    const _: [(); 100] = [(); core::mem::offset_of!(esp_http_client_config_t, buffer_size)];
    const _: [(); 104] = [(); core::mem::offset_of!(esp_http_client_config_t, buffer_size_tx)];
    const _: [(); 108] = [(); core::mem::offset_of!(esp_http_client_config_t, user_data)];
    const _: [(); 112] = [(); core::mem::offset_of!(esp_http_client_config_t, is_async)];
    const _: [(); 120] = [(); core::mem::offset_of!(esp_http_client_config_t, crt_bundle_attach)];
    const _: [(); 124] = [(); core::mem::offset_of!(esp_http_client_config_t, keep_alive_enable)];

    extern "C" {
        fn esp_http_client_init(config: *const esp_http_client_config_t) -> *mut c_void;
        fn esp_http_client_set_url(client: *mut c_void, url: *const c_char) -> c_int;
        fn esp_http_client_set_method(client: *mut c_void, method: c_int) -> c_int;
        fn esp_http_client_set_header(
            client: *mut c_void,
            key: *const c_char,
            value: *const c_char,
        ) -> c_int;
        fn esp_http_client_set_post_field(
            client: *mut c_void,
            data: *const c_char,
            len: c_int,
        ) -> c_int;
        fn esp_http_client_set_timeout_ms(client: *mut c_void, timeout_ms: c_int) -> c_int;
        fn esp_http_client_set_redirection(client: *mut c_void) -> c_int;
        fn esp_http_client_delete_header(client: *mut c_void, key: *const c_char) -> c_int;
        fn esp_http_client_reset_redirect_counter(client: *mut c_void) -> c_int;
        fn esp_http_client_perform(client: *mut c_void) -> c_int;
        fn esp_http_client_open(client: *mut c_void, write_len: c_int) -> c_int;
        fn esp_http_client_write(client: *mut c_void, data: *const c_char, len: c_int) -> c_int;
        fn esp_http_client_fetch_headers(client: *mut c_void) -> i64;
        fn esp_http_client_read(client: *mut c_void, data: *mut c_char, len: c_int) -> c_int;
        fn esp_http_client_is_complete_data_received(client: *mut c_void) -> bool;
        fn esp_http_client_get_errno(client: *mut c_void) -> c_int;
        fn esp_http_client_get_status_code(client: *mut c_void) -> c_int;
        fn esp_http_client_close(client: *mut c_void) -> c_int;
        fn esp_http_client_cleanup(client: *mut c_void) -> c_int;
        fn esp_crt_bundle_attach(conf: *mut c_void) -> c_int;
        fn esp_err_to_name(err: c_int) -> *const c_char;
        fn esp_log_level_set(tag: *const c_char, level: c_int);
    }

    fn filter_expected_poll_timeout_logs() {
        if HTTP_POLL_LOGS_FILTERED.swap(true, Ordering::AcqRel) {
            return;
        }
        unsafe {
            // The manual state machine intentionally converts these native
            // timeout paths to EAGAIN/Pending. ESP-IDF logs every such poll at
            // WARN; suppress only those two tags while retaining real errors.
            esp_log_level_set(c"HTTP_CLIENT".as_ptr(), ESP_LOG_ERROR);
            esp_log_level_set(c"transport_base".as_ptr(), ESP_LOG_ERROR);
        }
    }

    struct RequestCtx {
        body: Vec<u8>,
        abort: *const AtomicBool,
        body_io_timeout_ms: c_int,
        // Status observed by a callback belonging to the current response.
        // `esp_http_client_get_status_code` itself may retain the previous
        // request's value while a reused connection is still sending headers.
        response_status: c_int,
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
            if evt.event_id == HTTP_EVENT_ON_CONNECTED {
                // The connect phase needs a wider first poll because of
                // ESP-IDF's async-select behavior. Tighten subsequent request
                // send/read operations as soon as that phase is complete.
                let _ = esp_http_client_set_timeout_ms(evt.client, ctx.body_io_timeout_ms);
            }
            if matches!(
                evt.event_id,
                HTTP_EVENT_ON_HEADER | HTTP_EVENT_ON_STATUS_CODE | HTTP_EVENT_ON_DATA
            ) {
                let status = esp_http_client_get_status_code(evt.client);
                ctx.response_status = status;
                if evt.event_id == HTTP_EVENT_ON_DATA
                    && evt.data_len > 0
                    && !evt.data.is_null()
                    && !is_intermediate_status(status)
                {
                    let slice =
                        core::slice::from_raw_parts(evt.data as *const u8, evt.data_len as usize);
                    ctx.body.extend_from_slice(slice);
                }
            }
            // `perform` follows redirects internally. Do not let the
            // intermediate response body leak into the final response/stream.
            if evt.event_id == HTTP_EVENT_ON_FINISH {
                let status = esp_http_client_get_status_code(evt.client);
                if is_intermediate_status(status) {
                    ctx.body.clear();
                    ctx.response_status = 0;
                } else {
                    ctx.response_status = status;
                }
            }
        }
        ESP_OK
    }

    fn is_redirect_status(status: c_int) -> bool {
        matches!(status, 301 | 302 | 303 | 307 | 308)
    }

    fn is_intermediate_status(status: c_int) -> bool {
        matches!(status, 100..=199) || is_redirect_status(status)
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

    fn check_client_call(err: c_int, operation: &'static str) -> Result<(), HttpError> {
        if err == ESP_OK {
            Ok(())
        } else {
            Err(HttpError::RequestFailed(HttpRequestFailure::driver(
                operation,
                err_name(err),
            )))
        }
    }

    fn header_cstring(value: &str, label: &'static str) -> Result<CString, HttpError> {
        CString::new(value).map_err(|_| {
            HttpError::RequestFailed(HttpRequestFailure::HeaderContainsNul { field: label })
        })
    }

    fn body_len_to_c_int(len: usize) -> Result<c_int, HttpError> {
        c_int::try_from(len)
            .map_err(|_| HttpError::RequestFailed(HttpRequestFailure::BodyTooLarge { len }))
    }

    fn timeout_to_c_int(timeout_ms: u32) -> Result<c_int, HttpError> {
        c_int::try_from(timeout_ms).map_err(|_| {
            HttpError::RequestFailed(HttpRequestFailure::TimeoutTooLarge { timeout_ms })
        })
    }

    fn connect_io_timeout_ms(request_timeout_ms: u32) -> u32 {
        if request_timeout_ms == 0 {
            HTTP_CONNECT_IO_TIMEOUT_MS
        } else {
            request_timeout_ms.min(HTTP_CONNECT_IO_TIMEOUT_MS)
        }
    }

    fn body_io_timeout_ms(request_timeout_ms: u32) -> u32 {
        if request_timeout_ms == 0 {
            HTTP_BODY_IO_SLICE_TIMEOUT_MS
        } else {
            request_timeout_ms.min(HTTP_BODY_IO_SLICE_TIMEOUT_MS)
        }
    }

    fn status_code_from_c_int(status: c_int) -> Result<HttpStatusCode, HttpError> {
        let status = u16::try_from(status).map_err(|_| {
            HttpError::RequestFailed(HttpRequestFailure::InvalidStatusCode { status })
        })?;
        Ok(HttpStatusCode::new(status))
    }

    /// Overall wall-clock request deadline. The ESP-IDF client receives a much
    /// shorter per-operation timeout so one TLS/socket step cannot monopolize a
    /// FreeRTOS core for this entire duration.
    struct Deadline {
        limit: Option<Instant>,
    }

    impl Deadline {
        fn new(timeout_ms: u32) -> Self {
            let limit = (timeout_ms > 0)
                .then(|| Instant::now().checked_add(Duration::from_millis(u64::from(timeout_ms))))
                .flatten();
            Self { limit }
        }

        fn expired(&self) -> bool {
            self.limit.is_some_and(|limit| Instant::now() >= limit)
        }
    }

    fn timeout_error(timeout_ms: u32) -> HttpError {
        HttpError::RequestFailed(HttpRequestFailure::driver(
            "esp_http_client",
            format!("request timed out after {timeout_ms} ms"),
        ))
    }

    unsafe fn set_header(client: *mut c_void, name: &str, value: &str) -> Result<(), HttpError> {
        let key = header_cstring(name, "header name")?;
        let val = header_cstring(value, "header value")?;
        check_client_call(
            esp_http_client_set_header(client, key.as_ptr(), val.as_ptr()),
            "esp_http_client_set_header",
        )
    }

    fn cancel_raw_request(client: *mut c_void) {
        // ESP-IDF's `esp_http_client_cancel_request()` closes the socket and
        // immediately starts a replacement connection. That is the opposite of
        // what cancellation/drop needs and races TLS cleanup under multi-agent
        // load. Closing is sufficient; the reusable handle reconnects lazily on
        // the next request.
        close_raw_connection(client);
    }

    fn close_raw_connection(client: *mut c_void) {
        unsafe {
            let _ = esp_http_client_close(client);
        }
    }

    enum ActiveRequestState {
        Prepared,
        InFlight,
        Finished,
    }

    struct ActiveRequestGuard {
        client: *mut c_void,
        state: ActiveRequestState,
    }

    impl ActiveRequestGuard {
        fn new(client: *mut c_void) -> Self {
            Self {
                client,
                state: ActiveRequestState::Prepared,
            }
        }

        fn mark_started(&mut self) {
            if matches!(self.state, ActiveRequestState::Prepared) {
                self.state = ActiveRequestState::InFlight;
            }
        }

        fn finish(&mut self) {
            self.state = ActiveRequestState::Finished;
        }

        fn cancel(&mut self) {
            if matches!(self.state, ActiveRequestState::InFlight) {
                cancel_raw_request(self.client);
            }
            self.finish();
        }
    }

    impl Drop for ActiveRequestGuard {
        fn drop(&mut self) {
            if matches!(self.state, ActiveRequestState::InFlight) {
                cancel_raw_request(self.client);
            }
        }
    }

    /// A persistent async-mode `esp_http_client` handle owned by [`EspIdfHttp`].
    ///
    /// Created when [`EspIdfHttp`] is constructed and reused by subsequent requests
    /// (`keep_alive_enable` + `is_async` are set at init). Each request updates
    /// URL, method, headers, timeout, and body before driving either the
    /// blocking compatibility path or the manual async state machine; the raw
    /// handle itself is torn down only on `Drop`.
    struct EspClient {
        raw: *mut c_void,
        // `config.user_data` points at this box for the client's whole life; the
        // event handler writes the response body here through that raw pointer,
        // so the box must outlive the client and must not move. `Box` keeps the
        // heap payload pinned even when the `EspClient` value itself is moved.
        ctx: Box<RequestCtx>,
        // Names of the headers this code set on the reused client for the last
        // request. Before the next request we delete exactly these instead of
        // wiping every header, so the User-Agent/Host that
        // `esp_http_client_init` installed survive across requests (matching the
        // fresh-client C transport). Sending no User-Agent trips bot management
        // on some LLM API edges (e.g. DeepSeek behind TencentEdgeOne -> 418).
        applied_headers: Vec<CString>,
    }

    impl Drop for EspClient {
        fn drop(&mut self) {
            unsafe { esp_http_client_cleanup(self.raw) };
        }
    }

    impl EspClient {
        /// Initialize a reusable async-mode keep-alive client. Per-request
        /// options are applied later by [`EspClient::prepare_request`].
        fn new(initial_url: &str) -> Result<EspClient, HttpError> {
            filter_expected_poll_timeout_logs();
            // `url` is parsed/copied by `esp_http_client_init`; it only needs to
            // stay alive until that call returns.
            let url = CString::new(initial_url).map_err(|_| HttpError::InvalidUrl)?;
            let mut ctx = Box::new(RequestCtx {
                body: Vec::with_capacity(4096),
                abort: core::ptr::null(),
                body_io_timeout_ms: HTTP_BODY_IO_SLICE_TIMEOUT_MS as c_int,
                response_status: 0,
            });

            let mut config: esp_http_client_config_t = unsafe { core::mem::zeroed() };
            config.url = url.as_ptr();
            config.event_handler = Some(http_event_handler);
            config.user_data = (&mut *ctx as *mut RequestCtx) as *mut c_void;
            config.buffer_size = 4096;
            config.buffer_size_tx = 4096;
            config.crt_bundle_attach = Some(esp_crt_bundle_attach);
            // Reuse the underlying TCP/TLS connection across requests when the
            // server allows it. Async mode exposes EAGAIN between bounded native
            // operations so the cooperative Session can rotate agents.
            config.keep_alive_enable = true;
            config.is_async = true;

            let raw = unsafe { esp_http_client_init(&config) };
            if raw.is_null() {
                return Err(HttpError::ClientInitFailed);
            }
            Ok(EspClient {
                raw,
                ctx,
                applied_headers: Vec::new(),
            })
        }

        /// Set a request header on the reused client and remember its name so
        /// the next request can remove it. Leaves headers we never touch (the
        /// User-Agent/Host from `esp_http_client_init`) in place.
        fn apply_header(&mut self, name: &str, value: &str) -> Result<(), HttpError> {
            unsafe { set_header(self.raw, name, value)? };
            if let Ok(cname) = CString::new(name) {
                if !self
                    .applied_headers
                    .iter()
                    .any(|existing| existing.as_bytes() == cname.as_bytes())
                {
                    self.applied_headers.push(cname);
                }
            }
            Ok(())
        }

        /// Remove the headers set by the previous request. Best-effort: a header
        /// may already be absent. Unlike `esp_http_client_delete_all_headers`,
        /// this preserves the init-time User-Agent/Host.
        fn clear_applied_headers(&mut self) {
            for name in self.applied_headers.drain(..) {
                unsafe { esp_http_client_delete_header(self.raw, name.as_ptr()) };
            }
        }

        /// Apply this request's URL/method/headers/body to the persistent client.
        ///
        /// The returned body buffer must stay alive until the request finishes
        /// because `esp_http_client_set_post_field` stores, rather than copies,
        /// its pointer.
        fn prepare_request(
            &mut self,
            request: &HttpJsonRequest,
            abort: *const AtomicBool,
        ) -> Result<CString, HttpError> {
            self.prepare_base_request(
                request.url,
                request.timeout_ms,
                request.auth,
                request.headers,
                abort,
            )?;

            // `set_post_field` stores the pointer (no copy), so `body` must
            // outlive the blocking perform.
            let body = CString::new(request.body).map_err(|_| HttpError::InvalidBody)?;
            let body_len = body_len_to_c_int(request.body.len())?;

            unsafe {
                check_client_call(
                    esp_http_client_set_method(self.raw, HTTP_METHOD_POST),
                    "esp_http_client_set_method",
                )?;
            }
            // Auth and extra headers were already applied by
            // `prepare_base_request`; only the POST content type is added here.
            self.apply_header("Content-Type", "application/json")?;
            unsafe {
                check_client_call(
                    esp_http_client_set_post_field(self.raw, body.as_ptr(), body_len),
                    "esp_http_client_set_post_field",
                )?;
            }
            Ok(body)
        }

        /// Apply common request fields and clear the response accumulator.
        fn prepare_base_request(
            &mut self,
            url: &str,
            timeout_ms: u32,
            auth: claw_interface::HttpAuth<'_>,
            headers: &[claw_interface::HttpHeader<'_>],
            abort: *const AtomicBool,
        ) -> Result<(), HttpError> {
            self.ctx.body.clear();
            self.ctx.abort = abort;
            self.ctx.response_status = 0;

            // `set_url` copies the string internally; it only needs to live for
            // the duration of the call.
            let url = CString::new(url).map_err(|_| HttpError::InvalidUrl)?;
            // Preserve the API's range validation, but never hand the entire
            // request lifetime to one native socket/TLS operation. In ESP-IDF
            // async mode this timeout still governs individual transport
            // reads and TLS-handshake polls.
            let _ = timeout_to_c_int(timeout_ms)?;
            let connect_timeout_ms = timeout_to_c_int(connect_io_timeout_ms(timeout_ms))?;
            self.ctx.body_io_timeout_ms = timeout_to_c_int(body_io_timeout_ms(timeout_ms))?;

            unsafe {
                check_client_call(
                    esp_http_client_set_url(self.raw, url.as_ptr()),
                    "esp_http_client_set_url",
                )?;
                check_client_call(
                    esp_http_client_set_timeout_ms(self.raw, connect_timeout_ms),
                    "esp_http_client_set_timeout_ms",
                )?;
                check_client_call(
                    esp_http_client_reset_redirect_counter(self.raw),
                    "esp_http_client_reset_redirect_counter",
                )?;
            }
            // Delete only the headers we added last time, preserving the
            // init-time User-Agent/Host, then apply this request's headers.
            self.clear_applied_headers();
            self.set_auth_and_extra_headers(auth, headers)
        }

        fn set_auth_and_extra_headers(
            &mut self,
            auth: claw_interface::HttpAuth<'_>,
            headers: &[claw_interface::HttpHeader<'_>],
        ) -> Result<(), HttpError> {
            if let Some((name, value)) = auth.header() {
                self.apply_header(name, &value)?;
            }
            for header in headers {
                if header.name.is_empty() {
                    continue;
                }
                self.apply_header(header.name, header.value)?;
            }
            Ok(())
        }

        fn prepare_get_request(&mut self, request: &HttpGetRequest<'_>) -> Result<(), HttpError> {
            self.prepare_base_request(
                request.url,
                request.timeout_ms,
                request.auth,
                request.headers,
                core::ptr::null(),
            )?;
            unsafe {
                check_client_call(
                    esp_http_client_set_method(self.raw, HTTP_METHOD_GET),
                    "esp_http_client_set_method",
                )
            }
        }

        /// Run one raw non-blocking transfer step. `Ok(false)` means the
        /// transfer is still in progress; `Ok(true)` means `perform` completed.
        /// An EAGAIN with a non-pending errno is a transport failure.
        fn perform_raw_step(&mut self) -> Result<bool, HttpError> {
            // `esp_http_client_close` leaves the transport's saved socket errno
            // intact. Drain it before starting a new step, then immediately
            // take the errno produced by this step so failures cannot leak
            // across steps or reconnected sockets.
            let _ = unsafe { esp_http_client_get_errno(self.raw) };
            let err = unsafe { esp_http_client_perform(self.raw) };
            let transport_errno = unsafe { esp_http_client_get_errno(self.raw) };
            if err == ESP_ERR_HTTP_EAGAIN {
                if matches!(
                    transport_errno,
                    ERRNO_NONE | ERRNO_EAGAIN | ERRNO_EINPROGRESS
                ) {
                    return Ok(false);
                }
                return Err(HttpError::RequestFailed(HttpRequestFailure::driver(
                    "esp_http_client_perform",
                    format!("async transport errno={transport_errno}"),
                )));
            }
            if err != ESP_OK {
                return Err(HttpError::RequestFailed(HttpRequestFailure::driver(
                    "esp_http_client_perform",
                    err_name(err),
                )));
            }
            Ok(true)
        }

        /// Run one non-blocking transfer step for the buffered-response API.
        fn perform_step(&mut self) -> Result<Option<HttpResponse>, HttpError> {
            if !self.perform_raw_step()? {
                return Ok(None);
            }
            let status =
                status_code_from_c_int(unsafe { esp_http_client_get_status_code(self.raw) })?;
            let body = String::from_utf8_lossy(&self.ctx.body).into_owned();
            if !status.is_success() {
                return Err(HttpError::UnexpectedStatus {
                    status,
                    message: parse_error_message_body(&body, status),
                });
            }
            Ok(Some(HttpResponse {
                status_code: status,
                body,
            }))
        }

        /// Cancel the active transfer without destroying the reusable client
        /// handle. Best-effort: cancellation itself reports [`HttpError::Aborted`]
        /// to the caller even if the ESP-IDF helper says there was no active
        /// socket yet.
        fn cancel_active_request(&mut self) {
            cancel_raw_request(self.raw);
        }

        /// Close the active socket after a transport-level failure while keeping
        /// the reusable client handle alive for the next request.
        fn close_failed_connection(&mut self, error: &HttpError) {
            if matches!(error, HttpError::RequestFailed(_)) {
                close_raw_connection(self.raw);
            }
        }

        /// Blocking compatibility path over the async-mode client. This keeps
        /// the single persistent handle model while the sync trait is still
        /// present during the migration.
        fn execute_blocking(
            &mut self,
            request: &HttpJsonRequest,
            abort: &AtomicBool,
        ) -> Result<HttpResponse, HttpError> {
            let result = self.execute_blocking_inner(request, abort);
            // The event callback borrows this flag only for the active request;
            // never leave a caller-owned pointer in a persistent client.
            self.ctx.abort = core::ptr::null();
            result
        }

        fn execute_blocking_inner(
            &mut self,
            request: &HttpJsonRequest,
            abort: &AtomicBool,
        ) -> Result<HttpResponse, HttpError> {
            let _body = self.prepare_request(request, abort as *const _)?;
            let deadline = Deadline::new(request.timeout_ms);
            let mut started = false;
            loop {
                if abort.load(Ordering::Relaxed) {
                    if started {
                        self.cancel_active_request();
                    }
                    return Err(HttpError::Aborted);
                }
                if deadline.expired() {
                    self.cancel_active_request();
                    return Err(timeout_error(request.timeout_ms));
                }
                match self.perform_step() {
                    Ok(Some(response)) => return Ok(response),
                    Ok(None) => {
                        started = true;
                        std::thread::sleep(Duration::from_millis(HTTP_RETRY_DELAY_MS));
                    }
                    Err(error) => {
                        if abort.load(Ordering::Relaxed) {
                            return Err(HttpError::Aborted);
                        }
                        self.close_failed_connection(&error);
                        return Err(error);
                    }
                }
            }
        }

        async fn execute_async(
            &mut self,
            request: &HttpJsonRequest<'_>,
            cancel: Cancel<'_>,
        ) -> Result<HttpResponse, HttpError> {
            if cancel.is_cancelled() {
                return Err(HttpError::Aborted);
            }
            let deadline = Deadline::new(request.timeout_ms);
            let mut retried_after_close = false;
            'connection: loop {
                if cancel.is_cancelled() {
                    return Err(HttpError::Aborted);
                }
                if deadline.expired() {
                    return Err(timeout_error(request.timeout_ms));
                }
                let body = self.prepare_request(request, core::ptr::null())?;
                let mut active = ActiveRequestGuard::new(self.raw);
                active.mark_started();
                let result = self
                    .execute_prepared_async(body.as_bytes(), request.timeout_ms, &deadline, cancel)
                    .await;
                match result {
                    Ok(response) => {
                        active.finish();
                        return Ok(response);
                    }
                    Err(error) => {
                        if matches!(error, HttpError::Aborted) {
                            active.cancel();
                            return Err(error);
                        }
                        if !retried_after_close
                            && matches!(error, HttpError::RequestFailed(_))
                            && !deadline.expired()
                        {
                            // Recover a stale keep-alive connection using the
                            // same handle. Preparation restores the original
                            // URL if a failed exchange had followed redirects.
                            self.close_failed_connection(&error);
                            active.finish();
                            drop(body);
                            drop(active);
                            retried_after_close = true;
                            continue 'connection;
                        }
                        self.close_failed_connection(&error);
                        active.finish();
                        return Err(error);
                    }
                }
            }
        }

        async fn execute_get_async(
            &mut self,
            request: &HttpGetRequest<'_>,
            cancel: Cancel<'_>,
        ) -> Result<HttpResponse, HttpError> {
            if cancel.is_cancelled() {
                return Err(HttpError::Aborted);
            }
            let deadline = Deadline::new(request.timeout_ms);
            let mut retried_after_close = false;
            'connection: loop {
                if cancel.is_cancelled() {
                    return Err(HttpError::Aborted);
                }
                if deadline.expired() {
                    return Err(timeout_error(request.timeout_ms));
                }
                self.prepare_get_request(request)?;
                let mut active = ActiveRequestGuard::new(self.raw);
                active.mark_started();
                let result = self
                    .execute_prepared_async(&[], request.timeout_ms, &deadline, cancel)
                    .await;
                match result {
                    Ok(response) => {
                        active.finish();
                        return Ok(response);
                    }
                    Err(error) => {
                        if matches!(error, HttpError::Aborted) {
                            active.cancel();
                            return Err(error);
                        }
                        if !retried_after_close
                            && matches!(error, HttpError::RequestFailed(_))
                            && !deadline.expired()
                        {
                            self.close_failed_connection(&error);
                            active.finish();
                            drop(active);
                            retried_after_close = true;
                            continue 'connection;
                        }
                        self.close_failed_connection(&error);
                        active.finish();
                        return Err(error);
                    }
                }
            }
        }
    }

    fn stream_driver_error(operation: &'static str, message: impl Into<String>) -> HttpError {
        HttpError::RequestFailed(HttpRequestFailure::driver(operation, message))
    }

    /// RAII slot for one complete streaming request, from connection setup
    /// through the final SSE body byte. The Session retries acquisition
    /// cooperatively, so waiting for the target's TLS budget never blocks the
    /// executor thread.
    struct StreamPermit;

    impl StreamPermit {
        fn try_acquire() -> Option<Self> {
            let active = ACTIVE_STREAMS.load(Ordering::Acquire);
            if active < STREAM_LIMIT
                && ACTIVE_STREAMS
                    .compare_exchange_weak(active, active + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                Some(Self)
            } else {
                None
            }
        }
    }

    impl Drop for StreamPermit {
        fn drop(&mut self) {
            let previous = ACTIVE_STREAMS.fetch_sub(1, Ordering::AcqRel);
            debug_assert!(previous > 0);
        }
    }

    impl EspClient {
        fn response_status(&self) -> Result<Option<HttpStatusCode>, HttpError> {
            let status = self.ctx.response_status;
            if status <= 0 || is_intermediate_status(status) {
                return Ok(None);
            }
            status_code_from_c_int(status).map(Some)
        }

        fn buffered_response_status(&self) -> Result<Option<HttpStatusCode>, HttpError> {
            let status = self.ctx.response_status;
            if status <= 0 || matches!(status, 100..=199) {
                return Ok(None);
            }
            status_code_from_c_int(status).map(Some)
        }

        fn take_body_chunk(&mut self) -> Option<Vec<u8>> {
            if self.ctx.body.is_empty() {
                return None;
            }
            Some(std::mem::replace(
                &mut self.ctx.body,
                Vec::with_capacity(4096),
            ))
        }

        /// Send one already-prepared request through ESP-IDF's bounded native
        /// operations and stop after the final response headers. Unlike
        /// `esp_http_client_perform`, temporary body-read timeouts are not
        /// interpreted here, so the caller can handle them as cooperative
        /// `EAGAIN` polls.
        async fn open_write_fetch(
            &mut self,
            body_bytes: &[u8],
            timeout_ms: u32,
            deadline: &Deadline,
            cancel: Cancel<'_>,
            accept_redirect_status: bool,
        ) -> Result<HttpStatusCode, HttpError> {
            let body_len = body_len_to_c_int(body_bytes.len())?;
            loop {
                if cancel.is_cancelled() {
                    return Err(HttpError::Aborted);
                }
                if deadline.expired() {
                    return Err(timeout_error(timeout_ms));
                }
                let result = unsafe { esp_http_client_open(self.raw, body_len) };
                if result == ESP_OK {
                    check_client_call(
                        unsafe {
                            esp_http_client_set_timeout_ms(self.raw, self.ctx.body_io_timeout_ms)
                        },
                        "esp_http_client_set_timeout_ms",
                    )?;
                    break;
                }
                if result != ESP_ERR_HTTP_EAGAIN {
                    return Err(stream_driver_error(
                        "esp_http_client_open",
                        err_name(result),
                    ));
                }
                yield_once().await;
            }

            let mut body_offset = 0usize;
            while body_offset < body_bytes.len() {
                if cancel.is_cancelled() {
                    return Err(HttpError::Aborted);
                }
                if deadline.expired() {
                    return Err(timeout_error(timeout_ms));
                }
                let remaining = body_bytes.len() - body_offset;
                let remaining = c_int::try_from(remaining).map_err(|_| {
                    HttpError::RequestFailed(HttpRequestFailure::BodyTooLarge {
                        len: body_bytes.len(),
                    })
                })?;
                let written = unsafe {
                    esp_http_client_write(
                        self.raw,
                        body_bytes.as_ptr().add(body_offset).cast(),
                        remaining,
                    )
                };
                if written > 0 {
                    body_offset += written as usize;
                    continue;
                }
                if written == 0
                    || written == -ESP_ERR_HTTP_EAGAIN
                    || written == TLS_WANT_READ
                    || written == TLS_WANT_WRITE
                {
                    yield_once().await;
                    continue;
                }
                return Err(stream_driver_error(
                    "esp_http_client_write",
                    format!("write returned {written}"),
                ));
            }
            loop {
                if cancel.is_cancelled() {
                    return Err(HttpError::Aborted);
                }
                if deadline.expired() {
                    return Err(timeout_error(timeout_ms));
                }
                let result = unsafe { esp_http_client_fetch_headers(self.raw) };
                if result >= 0 {
                    break;
                }
                if result != -i64::from(ESP_ERR_HTTP_EAGAIN) {
                    return Err(stream_driver_error(
                        "esp_http_client_fetch_headers",
                        format!("fetch returned {result}"),
                    ));
                }
                yield_once().await;
            }

            let status = if accept_redirect_status {
                self.buffered_response_status()
            } else {
                self.response_status()
            }?
            .ok_or_else(|| {
                stream_driver_error(
                    "esp_http_client_fetch_headers",
                    "response headers completed without a final HTTP status",
                )
            })?;
            Ok(status)
        }

        /// Read a buffered response with short native reads. A timeout from one
        /// read becomes `EAGAIN` and yields to the cooperative executor; EOF is
        /// accepted only when ESP-IDF confirms that Content-Length or the final
        /// chunk was received.
        async fn execute_prepared_async(
            &mut self,
            body_bytes: &[u8],
            timeout_ms: u32,
            deadline: &Deadline,
            cancel: Cancel<'_>,
        ) -> Result<HttpResponse, HttpError> {
            let mut redirect_count = 0_u8;
            let mut read_buffer = [0_u8; 4096];

            loop {
                let status = self
                    .open_write_fetch(body_bytes, timeout_ms, deadline, cancel, true)
                    .await?;

                loop {
                    if cancel.is_cancelled() {
                        return Err(HttpError::Aborted);
                    }
                    if deadline.expired() {
                        return Err(timeout_error(timeout_ms));
                    }
                    if unsafe { esp_http_client_is_complete_data_received(self.raw) } {
                        break;
                    }

                    let read = unsafe {
                        esp_http_client_read(
                            self.raw,
                            read_buffer.as_mut_ptr().cast(),
                            read_buffer.len() as c_int,
                        )
                    };
                    if read > 0 {
                        continue;
                    }
                    if read == -ESP_ERR_HTTP_EAGAIN {
                        yield_once().await;
                        continue;
                    }
                    if read == 0 && unsafe { esp_http_client_is_complete_data_received(self.raw) } {
                        break;
                    }
                    let transport_errno = unsafe { esp_http_client_get_errno(self.raw) };
                    return Err(stream_driver_error(
                        "esp_http_client_read",
                        format!(
                            "read returned {read} before completion (transport errno={transport_errno})"
                        ),
                    ));
                }

                if is_redirect_status(status.as_i32()) {
                    if redirect_count >= MAX_REDIRECTS {
                        return Err(stream_driver_error(
                            "esp_http_client_set_redirection",
                            format!("exceeded {MAX_REDIRECTS} redirects"),
                        ));
                    }
                    check_client_call(
                        unsafe { esp_http_client_set_redirection(self.raw) },
                        "esp_http_client_set_redirection",
                    )?;
                    redirect_count = redirect_count.saturating_add(1);
                    // The public redirect helper updates the URL but leaves the
                    // completed response state in place. Closing resets that
                    // state; the reusable handle reconnects lazily, and the
                    // request body remains alive for replay.
                    close_raw_connection(self.raw);
                    self.ctx.body.clear();
                    self.ctx.response_status = 0;
                    continue;
                }

                let body = String::from_utf8_lossy(&self.ctx.body).into_owned();
                if !status.is_success() {
                    // Avoid the perform finalizer's automatic authentication
                    // branch changing a completed error response into another
                    // opaque transport exchange.
                    close_raw_connection(self.raw);
                    return Err(HttpError::UnexpectedStatus {
                        status,
                        message: parse_error_message_body(&body, status),
                    });
                }

                // No socket read remains here. This final `perform` invocation
                // only dispatches ON_FINISH and returns the reusable handle to
                // CONNECTED (or closes it when the peer disabled keep-alive).
                match self.perform_raw_step()? {
                    true => {
                        return Ok(HttpResponse {
                            status_code: status,
                            body,
                        });
                    }
                    false => {
                        return Err(stream_driver_error(
                            "esp_http_client_perform",
                            "complete buffered response finalization returned EAGAIN",
                        ));
                    }
                }
            }
        }

        /// Drive open/write/fetch as bounded native operations. EAGAIN yields to
        /// the Session; the returned stream continues with one bounded read per
        /// `poll_next` and keeps this client exclusively borrowed.
        async fn begin_streaming<'a>(
            &'a mut self,
            request: &HttpJsonRequest<'_>,
            cancel: Cancel<'a>,
        ) -> Result<(HttpStatusCode, EspHttpByteStream<'a>), HttpError> {
            if cancel.is_cancelled() {
                return Err(HttpError::Aborted);
            }

            let mut stream_permit = None;
            while stream_permit.is_none() {
                if cancel.is_cancelled() {
                    return Err(HttpError::Aborted);
                }
                stream_permit = StreamPermit::try_acquire();
                if stream_permit.is_none() {
                    yield_once().await;
                }
            }
            // Admission wait is bounded by the owning Agent/subagent cancel
            // token. Start the per-request network deadline only after this
            // request owns the single hardware stream slot; otherwise a queued
            // Agent could consume its whole HTTP timeout without touching the
            // network.
            let deadline = Deadline::new(request.timeout_ms);
            let mut retried_after_close = false;
            'connection: loop {
                let request_body = self.prepare_request(request, core::ptr::null())?;
                let mut active = ActiveRequestGuard::new(self.raw);
                active.mark_started();

                let begin_result = self
                    .open_write_fetch(
                        request_body.as_bytes(),
                        request.timeout_ms,
                        &deadline,
                        cancel,
                        false,
                    )
                    .await;

                match begin_result {
                    Ok(status) => {
                        active.finish();
                        return Ok((
                            status,
                            EspHttpByteStream {
                                conn: self,
                                _request_body: request_body,
                                _stream_permit: stream_permit
                                    .take()
                                    .expect("stream permit held until body completion"),
                                deadline,
                                timeout_ms: request.timeout_ms,
                                cancel,
                                transfer_complete: false,
                                terminated: false,
                                read_buffer: [0; 4096],
                            },
                        ));
                    }
                    Err(error) => {
                        self.close_failed_connection(&error);
                        active.finish();
                        drop(active);
                        drop(request_body);
                        if !retried_after_close
                            && matches!(error, HttpError::RequestFailed(_))
                            && !deadline.expired()
                        {
                            retried_after_close = true;
                            continue 'connection;
                        }
                        return Err(error);
                    }
                }
            }
        }
    }

    /// ESP-IDF response-body stream borrowing the Agent's persistent client.
    pub struct EspHttpByteStream<'a> {
        conn: &'a mut EspClient,
        // `esp_http_client_set_post_field` retains this allocation's pointer
        // until the request has finished or its connection is closed.
        _request_body: CString,
        // Retain hardware admission until success, error, cancellation, or
        // early stream drop releases the complete SSE request.
        _stream_permit: StreamPermit,
        deadline: Deadline,
        timeout_ms: u32,
        cancel: Cancel<'a>,
        transfer_complete: bool,
        terminated: bool,
        read_buffer: [u8; 4096],
    }

    impl EspHttpByteStream<'_> {
        fn fail(&mut self, error: HttpError) -> Poll<Option<Result<Vec<u8>, HttpError>>> {
            close_raw_connection(self.conn.raw);
            self.transfer_complete = true;
            self.terminated = true;
            Poll::Ready(Some(Err(error)))
        }

        fn finish_transfer(&mut self) -> Result<(), HttpError> {
            match self.conn.perform_raw_step()? {
                true => {
                    self.transfer_complete = true;
                    Ok(())
                }
                false => Err(stream_driver_error(
                    "esp_http_client_perform",
                    "response was complete but finalization returned EAGAIN",
                )),
            }
        }
    }

    impl Stream for EspHttpByteStream<'_> {
        type Item = Result<Vec<u8>, HttpError>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let this = self.get_mut();
            if this.terminated {
                return Poll::Ready(None);
            }
            if this.cancel.is_cancelled() {
                return this.fail(HttpError::Aborted);
            }
            if let Some(chunk) = this.conn.take_body_chunk() {
                return Poll::Ready(Some(Ok(chunk)));
            }
            if this.transfer_complete {
                this.terminated = true;
                return Poll::Ready(None);
            }
            if this.deadline.expired() {
                return this.fail(timeout_error(this.timeout_ms));
            }

            if unsafe { esp_http_client_is_complete_data_received(this.conn.raw) } {
                if let Err(error) = this.finish_transfer() {
                    return this.fail(error);
                }
                this.terminated = true;
                return Poll::Ready(None);
            }

            // One call drains up to 4 KiB but waits at most the 10 ms body I/O
            // slice. A no-data timeout becomes -EAGAIN, so this poll returns
            // Pending and the Session can poll another Agent.
            let read = unsafe {
                esp_http_client_read(
                    this.conn.raw,
                    this.read_buffer.as_mut_ptr().cast(),
                    this.read_buffer.len() as c_int,
                )
            };
            if read > 0 {
                if let Some(chunk) = this.conn.take_body_chunk() {
                    return Poll::Ready(Some(Ok(chunk)));
                }
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            if read == -ESP_ERR_HTTP_EAGAIN {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            if read == 0 && unsafe { esp_http_client_is_complete_data_received(this.conn.raw) } {
                if let Err(error) = this.finish_transfer() {
                    return this.fail(error);
                }
                this.terminated = true;
                return Poll::Ready(None);
            }
            let transport_errno = unsafe { esp_http_client_get_errno(this.conn.raw) };
            this.fail(stream_driver_error(
                "esp_http_client_read",
                format!(
                    "read returned {read} before completion (transport errno={transport_errno})"
                ),
            ))
        }
    }

    impl Drop for EspHttpByteStream<'_> {
        fn drop(&mut self) {
            if !self.transfer_complete {
                close_raw_connection(self.conn.raw);
            }
        }
    }

    /// `esp_http_client`-backed transport implementing [`blocking::ClawHttp`]
    /// (blocking), [`ClawHttp`] (async buffered responses), and [`StreamingHttp`]
    /// (async response chunks).
    ///
    /// The transport owns one persistent keep-alive [`EspClient`] created at
    /// construction and reused until `EspIdfHttp` is dropped. Async cancellation
    /// cancels the active request/socket, not the client handle.
    pub struct EspIdfHttp {
        conn: EspClient,
    }

    impl EspIdfHttp {
        /// Create a transport with a configured reusable ESP-IDF client handle.
        ///
        /// ESP-IDF requires an initial URL (or host/path) at
        /// `esp_http_client_init` time. The URL is still overwritten from every
        /// request before transfer, so this does not bind the transport to one
        /// endpoint.
        pub fn new(initial_url: &str) -> Result<Self, HttpError> {
            Ok(Self {
                conn: EspClient::new(initial_url)?,
            })
        }
    }

    impl Default for EspIdfHttp {
        fn default() -> Self {
            match Self::new(DEFAULT_INITIAL_URL) {
                Ok(http) => http,
                Err(_) => std::process::abort(),
            }
        }
    }

    impl blocking::ClawHttp for EspIdfHttp {
        fn post_json(
            &mut self,
            request: &HttpJsonRequest,
            abort: &AtomicBool,
        ) -> Result<HttpResponse, HttpError> {
            self.conn.execute_blocking(request, abort)
        }
    }

    /// Yields once to the executor, then resumes. Lets cooperatively-scheduled
    /// tasks run between `ESP_ERR_HTTP_EAGAIN` retries instead of spinning the
    /// CPU inside a single poll.
    async fn yield_once() {
        struct YieldOnce(bool);
        impl Future for YieldOnce {
            type Output = ();
            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                if self.0 {
                    Poll::Ready(())
                } else {
                    self.0 = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        }
        YieldOnce(false).await
    }

    impl ClawHttp for EspIdfHttp {
        fn post_json<'a>(
            &'a mut self,
            request: &'a HttpJsonRequest<'a>,
            cancel: Cancel<'a>,
        ) -> HttpResponseFuture<'a> {
            Box::pin(async move { self.conn.execute_async(request, cancel).await })
        }

        fn get_json<'a>(
            &'a mut self,
            request: &'a HttpGetRequest<'a>,
            cancel: Cancel<'a>,
        ) -> HttpResponseFuture<'a> {
            Box::pin(async move { self.conn.execute_get_async(request, cancel).await })
        }
    }

    impl StreamingHttp for EspIdfHttp {
        type ByteStream<'a>
            = EspHttpByteStream<'a>
        where
            Self: 'a;

        async fn post_json_streaming<'a, 'r>(
            &'a mut self,
            request: &'r HttpJsonRequest<'r>,
            cancel: Cancel<'a>,
        ) -> Result<(HttpStatusCode, Self::ByteStream<'a>), HttpError> {
            self.conn.begin_streaming(request, cancel).await
        }
    }
}
