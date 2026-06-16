//! C ABI for the Rust orchestrator (`claw_orchestrator.h`).

use core::ffi::{c_char, c_void};
use std::ffi::CStr;
use std::sync::{Arc, Mutex, OnceLock};

use claw_api::{ClawApi, ClawApiConfig};
use claw_core::SessionError;
use claw_core::{
    ChannelEgressHub, DeliverError, InboundMessage, Orchestrator, SessionId,
};
use claw_interfaces::error::{
    EspErr, ESP_ERR_INVALID_ARG, ESP_ERR_INVALID_STATE, ESP_ERR_NOT_FOUND, ESP_OK,
};
use claw_interfaces::http::{ClawHttp, HttpError, HttpJsonRequest, HttpResponse};

struct OrchestratorState {
    orchestrator: Mutex<Orchestrator>,
}

static ORCHESTRATOR: OnceLock<Mutex<OrchestratorState>> = OnceLock::new();

struct NoopHttp;

impl ClawHttp for NoopHttp {
    fn post_json(
        &self,
        _request: &HttpJsonRequest,
        _abort: &std::sync::atomic::AtomicBool,
    ) -> Result<HttpResponse, HttpError> {
        Err(HttpError::RequestFailed("noop".into()))
    }
}

fn boot_llm() -> Arc<ClawApi> {
    Arc::new(
        ClawApi::init(
            ClawApiConfig {
                api_key: Some("boot".into()),
                model: Some("gpt-4o-mini".into()),
                backend_type: "openai_compatible".into(),
                base_url: Some("https://api.example.com/v1".into()),
                ..Default::default()
            },
            Arc::new(NoopHttp),
        )
        .expect("boot llm"),
    )
}

fn with_state<F>(f: F) -> EspErr
where
    F: FnOnce(&OrchestratorState) -> EspErr,
{
    let Some(state) = ORCHESTRATOR.get() else {
        return ESP_ERR_INVALID_STATE;
    };
    match state.lock() {
        Ok(guard) => f(&guard),
        Err(_) => ESP_ERR_INVALID_STATE,
    }
}

fn with_orchestrator<F>(f: F) -> EspErr
where
    F: FnOnce(&Orchestrator) -> EspErr,
{
    with_state(|state| match state.orchestrator.lock() {
        Ok(orch) => f(&orch),
        Err(_) => ESP_ERR_INVALID_STATE,
    })
}

fn with_orchestrator_mut<F>(f: F) -> EspErr
where
    F: FnOnce(&mut Orchestrator) -> EspErr,
{
    with_state(|state| match state.orchestrator.lock() {
        Ok(mut orch) => f(&mut orch),
        Err(_) => ESP_ERR_INVALID_STATE,
    })
}

fn cstr_input<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr).to_str().ok() }
}

fn copy_session_id(dst: *mut c_char, dst_len: usize, id: SessionId) -> EspErr {
    if dst.is_null() || dst_len == 0 {
        return ESP_ERR_INVALID_ARG;
    }
    let wire = id.to_wire();
    let bytes = wire.as_bytes();
    if bytes.len() >= dst_len {
        return ESP_ERR_INVALID_ARG;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst as *mut u8, bytes.len());
        *dst.add(bytes.len()) = 0;
    }
    ESP_OK
}

fn deliver_err_to_esp(err: DeliverError) -> EspErr {
    match err {
        DeliverError::SessionNotFound(_) | DeliverError::InvalidSessionId(_) => ESP_ERR_NOT_FOUND,
        DeliverError::NoReplyRoute(_) => ESP_ERR_INVALID_STATE,
        DeliverError::Session(_) => ESP_ERR_NOT_FOUND,
        DeliverError::Channel(_) => ESP_ERR_INVALID_STATE,
    }
}

/// Initialize orchestrator + capability host (call once at boot).
#[no_mangle]
pub extern "C" fn claw_orchestrator_init(cap_user_ctx: *mut c_void) -> EspErr {
    if ORCHESTRATOR.get().is_some() {
        return ESP_OK;
    }

    let _ = cap_user_ctx;
    let egress: Arc<dyn claw_core::ChannelEgress> = Arc::new(ChannelEgressHub::new());

    let orchestrator = Orchestrator::builder()
        .config_egress(egress)
        .with_llm(boot_llm())
        .build();

    let state = OrchestratorState {
        orchestrator: Mutex::new(orchestrator),
    };
    let _ = ORCHESTRATOR.set(Mutex::new(state));
    ESP_OK
}

#[repr(C)]
pub struct claw_orchestrator_user_message_t {
    pub message_id: *const c_char,
    pub channel: *const c_char,
    pub chat_id: *const c_char,
    pub sender_id: *const c_char,
    pub session_id: *const c_char,
    pub text: *const c_char,
}

#[no_mangle]
pub extern "C" fn claw_orchestrator_push_user_message(
    msg: *const claw_orchestrator_user_message_t,
) -> EspErr {
    let Some(msg) = (unsafe { msg.as_ref() }) else {
        return ESP_ERR_INVALID_ARG;
    };
    let Some(channel) = cstr_input(msg.channel) else {
        return ESP_ERR_INVALID_ARG;
    };
    let Some(chat_id) = cstr_input(msg.chat_id) else {
        return ESP_ERR_INVALID_ARG;
    };
    let Some(text) = cstr_input(msg.text) else {
        return ESP_ERR_INVALID_ARG;
    };

    with_orchestrator_mut(|orch| {
        let inbound = InboundMessage {
            message_id: cstr_input(msg.message_id)
                .unwrap_or("msg")
                .to_string(),
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            sender_id: cstr_input(msg.sender_id).map(str::to_string),
            session_id: String::new(),
            text: text.to_string(),
        };

        let result = match cstr_input(msg.session_id) {
            Some(wire) => {
                let mut msg = inbound;
                msg.session_id = wire.to_string();
                orch.accept_user_message(msg)
            }
            None => orch.accept_user_message_for_route(channel, chat_id, inbound),
        };

        match result {
            Ok(()) => ESP_OK,
            Err(err) => deliver_err_to_esp(err),
        }
    })
}

#[no_mangle]
pub extern "C" fn claw_orchestrator_session_create(
    out_session_id: *mut c_char,
    out_len: usize,
) -> EspErr {
    with_orchestrator(|orch| {
        let id = orch.session_create();
        copy_session_id(out_session_id, out_len, id)
    })
}

#[no_mangle]
pub extern "C" fn claw_orchestrator_session_delete(session_id: *const c_char) -> EspErr {
    let Some(wire) = cstr_input(session_id) else {
        return ESP_ERR_INVALID_ARG;
    };
    let Ok(id) = SessionId::from_wire(wire) else {
        return ESP_ERR_INVALID_ARG;
    };
    with_orchestrator(|orch| match orch.session_delete(id) {
        Ok(()) => ESP_OK,
        Err(SessionError::NotFound(_)) => ESP_ERR_NOT_FOUND,
        Err(SessionError::AlreadyExists(_)) => ESP_ERR_INVALID_ARG,
    })
}

#[no_mangle]
pub extern "C" fn claw_orchestrator_session_count() -> usize {
    let Some(state) = ORCHESTRATOR.get() else {
        return 0;
    };
    let Ok(guard) = state.lock() else {
        return 0;
    };
    let Ok(orch) = guard.orchestrator.lock() else {
        return 0;
    };
    orch.session_count()
}

/// Compatibility no-op: deliver happens synchronously in [`claw_orchestrator_push_user_message`].
#[no_mangle]
pub extern "C" fn claw_orchestrator_tick() -> EspErr {
    ESP_OK
}
