//! Media preparation, port of `claw_media_pipeline.c`.
//!
//! Local image files are read with `std::fs` and base64-encoded with the
//! `base64` crate (replacing `fopen`/`mbedtls_base64_encode`).

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

use claw_interfaces::error::{
    ESP_ERR_INVALID_ARG, ESP_ERR_INVALID_SIZE, ESP_ERR_NOT_FOUND, ESP_ERR_NOT_SUPPORTED, ESP_FAIL,
};

use super::types::{AssetKind, LlmError, MediaAsset, ModelProfile, Prepared, PreparedKind};

/// Mirror of `image_mime_from_path`: extension-based MIME, case-insensitive.
fn image_mime_from_path(path: &str) -> Option<&'static str> {
    let dot = path.rfind('.')?;
    let ext = path[dot..].to_ascii_lowercase();
    match ext.as_str() {
        ".jpg" | ".jpeg" => Some("image/jpeg"),
        ".png" => Some("image/png"),
        ".gif" => Some("image/gif"),
        ".webp" => Some("image/webp"),
        _ => None,
    }
}

fn prepare_local_path_asset(
    asset: &MediaAsset,
    image_max_bytes: usize,
) -> Result<Prepared, LlmError> {
    let path = match asset.path.as_deref() {
        Some(p) if !p.is_empty() => p,
        _ => return Err(LlmError::new(ESP_ERR_INVALID_ARG, "media path is empty")),
    };
    if !path.starts_with('/') {
        return Err(LlmError::new(ESP_ERR_INVALID_ARG, "media path must be an absolute path"));
    }

    let mime = asset
        .mime_type
        .as_deref()
        .or_else(|| image_mime_from_path(path))
        .ok_or_else(|| {
            LlmError::new(
                ESP_ERR_NOT_SUPPORTED,
                "Only local jpg/jpeg/png/gif/webp files are supported",
            )
        })?;

    let meta = std::fs::metadata(path)
        .map_err(|_| LlmError::new(ESP_ERR_NOT_FOUND, format!("Media file not found: {path}")))?;
    let size = meta.len() as usize;
    if size == 0 {
        return Err(LlmError::new(ESP_ERR_INVALID_SIZE, format!("Media file is empty: {path}")));
    }
    if size > image_max_bytes {
        return Err(LlmError::new(
            ESP_ERR_INVALID_SIZE,
            format!("Media file is too large ({size} bytes > {image_max_bytes} bytes)"),
        ));
    }

    let raw = std::fs::read(path)
        .map_err(|_| LlmError::new(ESP_FAIL, format!("Failed to read full media file: {path}")))?;
    if raw.len() != size {
        return Err(LlmError::new(ESP_FAIL, format!("Failed to read full media file: {path}")));
    }

    let encoded = STANDARD.encode(&raw);
    let payload = format!("data:{mime};base64,{encoded}");

    Ok(Prepared {
        kind: PreparedKind::DataUrl,
        payload,
        mime_type: mime.to_string(),
        original_size: size,
    })
}

/// `claw_media_prepare_asset`
pub fn prepare_asset(
    asset: &MediaAsset,
    profile: &ModelProfile,
    image_max_bytes: usize,
) -> Result<Prepared, LlmError> {
    match asset.kind {
        AssetKind::RemoteUrl => {
            let url = match asset.url.as_deref() {
                Some(u) if !u.is_empty() => u,
                _ => return Err(LlmError::new(ESP_ERR_INVALID_ARG, "media url is empty")),
            };
            Ok(Prepared {
                kind: PreparedKind::RemoteUrl,
                payload: url.to_string(),
                mime_type: String::new(),
                original_size: 0,
            })
        }
        AssetKind::InlineBytes => {
            Err(LlmError::new(ESP_ERR_NOT_SUPPORTED, "Unsupported media asset kind"))
        }
        AssetKind::LocalPath => {
            if profile.image_remote_url_only {
                return Err(LlmError::new(
                    ESP_ERR_NOT_SUPPORTED,
                    "Selected profile only supports remote image URLs",
                ));
            }
            prepare_local_path_asset(asset, image_max_bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> ModelProfile {
        ModelProfile { supports_vision: true, ..Default::default() }
    }

    #[test]
    fn remote_url_passthrough() {
        let asset = MediaAsset {
            kind: AssetKind::RemoteUrl,
            path: None,
            url: Some("https://example.com/a.png".into()),
            bytes: None,
            mime_type: None,
        };
        let p = prepare_asset(&asset, &profile(), 1024).unwrap();
        assert_eq!(p.kind, PreparedKind::RemoteUrl);
        assert_eq!(p.payload, "https://example.com/a.png");
    }

    #[test]
    fn local_path_data_url() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("claw_media_test_{}.png", std::process::id()));
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nABCDE").unwrap();
        let asset = MediaAsset {
            kind: AssetKind::LocalPath,
            path: Some(path.to_string_lossy().into_owned()),
            url: None,
            bytes: None,
            mime_type: None,
        };
        let p = prepare_asset(&asset, &profile(), 1024).unwrap();
        assert_eq!(p.kind, PreparedKind::DataUrl);
        assert!(p.payload.starts_with("data:image/png;base64,"));
        assert_eq!(p.mime_type, "image/png");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_relative_path() {
        let asset = MediaAsset {
            kind: AssetKind::LocalPath,
            path: Some("rel/a.png".into()),
            url: None,
            bytes: None,
            mime_type: None,
        };
        let e = prepare_asset(&asset, &profile(), 1024).unwrap_err();
        assert_eq!(e.err, ESP_ERR_INVALID_ARG);
    }

    #[test]
    fn rejects_unknown_extension() {
        let asset = MediaAsset {
            kind: AssetKind::LocalPath,
            path: Some("/tmp/a.bmp".into()),
            url: None,
            bytes: None,
            mime_type: None,
        };
        let e = prepare_asset(&asset, &profile(), 1024).unwrap_err();
        assert_eq!(e.err, ESP_ERR_NOT_SUPPORTED);
    }
}
