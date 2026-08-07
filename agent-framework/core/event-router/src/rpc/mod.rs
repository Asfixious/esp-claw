//! Task-local typed RPC registry and fixed-layout lane transport.
//!
//! [`RpcClient::call`] transfers fixed-layout Zerocopy request, response, and
//! method-error messages through bounded frames. An outer [`RpcResult`] reports
//! transport/runtime failures, while each [`RpcMethod`] exposes its own typed
//! `Result<Response, Error>` outcome.

mod address;
mod context;
mod frame;
mod lane;
mod registry;
mod typed;

pub use address::{RpcAddress, RpcAddressError, RpcGroup, RpcGroupError};
pub use context::{RpcCallId, RpcContext, RpcEndpointId};
pub use frame::RpcFrame;
pub use lane::RpcLaneStorage;
pub(crate) use registry::RpcDirection;
pub use registry::{RpcClient, RpcError, RpcRegistration, RpcRegistry, RpcResult};
pub use typed::{
    RpcCardinality, RpcHandler, RpcHandlerFuture, RpcHandlerInput, RpcHandlerOutput, RpcInputMode,
    RpcMessage, RpcMethod, RpcOutputMode, RpcStream, RpcUnaryCall, Streaming, Unary,
};
