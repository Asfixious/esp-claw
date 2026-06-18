//! `claw_core_llm_*` C ABI (media inference) consumed by the C `cap_llm_inspect`
//! component.
//!
//! Exports [`claw_core_llm_infer_media`], matching `claw_core_llm.h`: it runs a
//! one-shot multimodal inference on the agent core's configured LLM and returns
//! the analysis text (or a detail message on failure) as `malloc`-allocated C
//! strings the caller frees with `free`.

use core::ffi::{c_char, c_int};
use core::ptr;
use std::ffi::CStr;

use claw_api::{MediaAsset, MediaRequest};
use claw_platform::{esp_err_t as EspErr, ESP_ERR_INVALID_ARG, ESP_ERR_INVALID_STATE, ESP_OK};

use crate::core_abi::{claw_core_handle_t, core_from_handle};

/// `claw_media_asset_kind_t`.
pub const CLAW_MEDIA_ASSET_KIND_LOCAL_PATH: c_int = 0;
pub const CLAW_MEDIA_ASSET_KIND_REMOTE_URL: c_int = 1;
pub const CLAW_MEDIA_ASSET_KIND_INLINE_BYTES: c_int = 2;

/// `claw_media_asset_t`.
#[repr(C)]
pub struct claw_media_asset_t {
    pub kind: c_int,
    pub path: *const c_char,
    pub url: *const c_char,
    pub bytes: *const u8,
    pub byte_count: usize,
    pub mime_type: *const c_char,
}

/// `claw_llm_media_request_t`.
#[repr(C)]
pub struct claw_llm_media_request_t {
    pub system_prompt: *const c_char,
    pub user_prompt: *const c_char,
    pub media: *const claw_media_asset_t,
    pub media_count: usize,
}

unsafe fn cstr_opt<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        None
    } else {
        CStr::from_ptr(p).to_str().ok()
    }
}

unsafe fn cstr_nonempty<'a>(p: *const c_char) -> Option<&'a str> {
    cstr_opt(p).filter(|s| !s.is_empty())
}

/// `claw_core_llm_infer_media` — run multimodal inference on `core`'s LLM.
///
/// # Safety
/// `core` must be null or a live handle from `claw_core_create`; `request` (and
/// its `media` array / C string fields) must be null or valid for reads;
/// `out_text` / `out_error_message` must be null or valid, writable pointers.
#[no_mangle]
pub unsafe extern "C" fn claw_core_llm_infer_media(
    core: claw_core_handle_t,
    request: *const claw_llm_media_request_t,
    out_text: *mut *mut c_char,
    out_error_message: *mut *mut c_char,
) -> EspErr {
    if !out_text.is_null() {
        *out_text = ptr::null_mut();
    }
    if !out_error_message.is_null() {
        *out_error_message = ptr::null_mut();
    }
    if core.is_null() || request.is_null() {
        return ESP_ERR_INVALID_ARG;
    }
    let Some(core) = core_from_handle(core) else {
        return ESP_ERR_INVALID_STATE;
    };

    let req = &*request;
    let assets = build_assets(req);

    let mut request_rs = MediaRequest::new(&assets);
    if let Some(system) = cstr_nonempty(req.system_prompt) {
        request_rs = request_rs.with_system_prompt(system);
    }
    if let Some(user) = cstr_nonempty(req.user_prompt) {
        request_rs = request_rs.with_user_prompt(user);
    }

    match core.infer_media(&request_rs) {
        Ok(text) => {
            if !out_text.is_null() {
                *out_text = crate::ffi::c_strdup(&text);
            }
            ESP_OK
        }
        Err(err) => {
            if !out_error_message.is_null() {
                *out_error_message = crate::ffi::c_strdup(&err.to_string());
            }
            crate::errmap::infer_media_error_code(&err)
        }
    }
}

/// Build the owned [`MediaAsset`] vector from the C `media` array.
unsafe fn build_assets(req: &claw_llm_media_request_t) -> Vec<MediaAsset> {
    let mut assets: Vec<MediaAsset> = Vec::with_capacity(req.media_count);
    if req.media.is_null() || req.media_count == 0 {
        return assets;
    }
    let media = core::slice::from_raw_parts(req.media, req.media_count);
    for asset in media {
        match asset.kind {
            CLAW_MEDIA_ASSET_KIND_REMOTE_URL => {
                let url = cstr_opt(asset.url).unwrap_or("");
                assets.push(MediaAsset::remote_url(url));
            }
            CLAW_MEDIA_ASSET_KIND_INLINE_BYTES => {
                let bytes = if asset.bytes.is_null() || asset.byte_count == 0 {
                    Vec::new()
                } else {
                    core::slice::from_raw_parts(asset.bytes, asset.byte_count).to_vec()
                };
                let mime = cstr_nonempty(asset.mime_type);
                let media_asset = MediaAsset::inline_bytes(bytes, mime.unwrap_or(""));
                let media_asset = match mime {
                    Some(m) => media_asset.with_mime_type(m),
                    None => media_asset,
                };
                assets.push(media_asset);
            }
            _ => {
                let path = cstr_opt(asset.path).unwrap_or("");
                assets.push(MediaAsset::local_path(path));
            }
        }
    }
    assets
}
