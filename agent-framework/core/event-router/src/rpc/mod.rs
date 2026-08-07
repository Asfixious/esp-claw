//! Task-local registry over fixed-capacity full-duplex RPC lanes.
//!
//! [`RpcClient::call`] transfers fixed-layout Zerocopy request, response, and
//! method-error messages through bounded frames. An outer [`RpcResult`] reports
//! transport/runtime failures, while each [`RpcMethod`] exposes its own typed
//! `Result<Response, Error>` outcome.
//!
//! Both entry points share one wire-call state machine. [`RpcClient::call_payload`]
//! returns its asynchronous [`RpcPayloadWriter`] / [`RpcPayloadReader`] handles
//! directly; [`RpcClient::call`] adds fixed-layout encoding and zero-copy typed
//! decoding over those same handles.

mod address;
mod context;
mod frame;
mod lane;
mod payload;
mod registry;
mod typed;

pub use address::{RpcAddress, RpcAddressError, RpcGroup, RpcGroupError};
pub use context::{RpcCallId, RpcContext, RpcEndpointId};
pub use frame::RpcFrame;
pub use lane::RpcLaneStorage;
pub use payload::{RpcPayloadFrame, RpcPayloadReader, RpcPayloadWriteFrame, RpcPayloadWriter};
pub(crate) use registry::RpcDirection;
pub use registry::{RpcClient, RpcError, RpcRegistration, RpcRegistry, RpcResult};
pub use typed::{
    RpcHandler, RpcHandlerFuture, RpcHandlerInput, RpcHandlerOutput, RpcInputMode, RpcMessage,
    RpcMethod, RpcOutputMode, RpcStream, RpcUnaryCall, Streaming, Unary,
};
