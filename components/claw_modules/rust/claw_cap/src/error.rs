//! Capability-layer errors (no `esp_err_t` in this crate).

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CapabilityError {
    #[error("invalid argument")]
    InvalidArg,
    #[error("out of memory")]
    NoMem,
    #[error("capability not found")]
    NotFound,
    #[error("capability not available")]
    NotAvailable,
    #[error("capability not exposed to the LLM")]
    NotVisible,
    #[error("operation failed")]
    Failed,
}
