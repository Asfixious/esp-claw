//! `claw-api` error types.
//!
//! Each public entry point ([`crate::ClawApi::init`], [`crate::ClawApi::chat`],
//! [`crate::ClawApi::infer_media`]) returns its own error enum because their
//! failure modes are not 1-to-1: config validation, chat-only tool errors, and
//! the media pipeline are disjoint. The shared API/transport/parse failures live
//! in [`ClawApiError`], which the per-function enums wrap via `#[from]`.
//!
//! All variants carry only `&'static str` (or, for the genuinely dynamic HTTP
//! transport message, an owned `String`); `Display` text comes from `thiserror`.

use thiserror::Error;

/// Failures shared by chat and media calls (transport, response parsing,
/// allocation). `ApiError` is the static-message catch-all.
#[derive(Debug, Clone, Error)]
pub enum ClawApiError {
    /// HTTP transport failure. Carries the backend/transport detail (e.g.
    /// `"HTTP 401: invalid api key"`), which is inherently dynamic.
    #[error("HTTP transport error: {0}")]
    Transport(String),
    /// The response body was not valid JSON.
    #[error("failed to parse LLM JSON response")]
    Parse,
    /// The model returned no usable content.
    #[error("LLM returned an empty response")]
    EmptyResponse,
    /// The response JSON had an unexpected shape (missing/!assistant message,
    /// missing content, malformed tool call).
    #[error("malformed LLM response: {0}")]
    MalformedResponse(&'static str),
    /// Any other API-side failure (allocation, serialization, ...).
    #[error("{0}")]
    ApiError(&'static str),
}

/// Failures from constructing a [`crate::ClawApi`] (config validation + backend
/// selection).
#[derive(Debug, Clone, Error)]
pub enum InitError {
    #[error("LLM API key is empty")]
    MissingApiKey,
    #[error("LLM model is empty")]
    MissingModel,
    #[error("LLM base URL is empty")]
    MissingBaseUrl,
    #[error("LLM backend type is empty")]
    MissingBackendType,
    #[error("unknown LLM backend type")]
    UnknownBackend,
}

/// Failures from a chat completion request.
#[derive(Debug, Clone, Error)]
pub enum ChatError {
    /// The selected backend/profile does not support tool calls.
    #[error("selected backend does not support tool calls")]
    ToolsUnsupported,
    /// The caller-supplied tools JSON was invalid.
    #[error("invalid tools JSON")]
    InvalidToolsJson,
    /// A shared API/transport/parse failure.
    #[error(transparent)]
    Api(#[from] ClawApiError),
}

/// Failures from a one-shot media inference request (includes the media-prep
/// pipeline used only by this call).
#[derive(Debug, Clone, Error)]
pub enum InferMediaError {
    /// The selected profile does not support vision/media.
    #[error("selected profile does not support media inference")]
    VisionUnsupported,
    /// The request was missing a prompt or media asset.
    #[error("media request is incomplete")]
    IncompleteRequest,
    /// Media path was empty.
    #[error("media path is empty")]
    MediaPathEmpty,
    /// Media path was not absolute.
    #[error("media path must be an absolute path")]
    MediaPathNotAbsolute,
    /// Media URL was empty.
    #[error("media URL is empty")]
    MediaUrlEmpty,
    /// The media file extension/MIME is not a supported image type.
    #[error("only local jpg/jpeg/png/gif/webp files are supported")]
    UnsupportedMediaType,
    /// The media file does not exist.
    #[error("media file not found")]
    MediaNotFound,
    /// The media file was empty.
    #[error("media file is empty")]
    MediaFileEmpty,
    /// The media file exceeded the configured size limit.
    #[error("media file is too large")]
    MediaTooLarge,
    /// Reading the media file failed.
    #[error("failed to read media file")]
    MediaReadFailed,
    /// The asset kind is not supported (e.g. inline bytes).
    #[error("unsupported media asset kind")]
    UnsupportedMediaKind,
    /// The profile only accepts remote image URLs.
    #[error("selected profile only supports remote image URLs")]
    RemoteOnlyProfile,
    /// The backend requires local image data (e.g. Anthropic base64).
    #[error("backend requires local image data")]
    RequiresLocalImage,
    /// Building the provider-specific image payload failed.
    #[error("failed to prepare image payload")]
    PayloadPrepFailed,
    /// A shared API/transport/parse failure.
    #[error(transparent)]
    Api(#[from] ClawApiError),
}
