//! Task-local typed RPC registry and fixed-layout lane transport.
//!
//! [`RpcClient::call`] transfers fixed-layout Zerocopy messages through bounded
//! frames and selects unary or streaming cardinality at the type level.

mod address;
mod context;
mod frame;
mod lane;
mod registry;
mod typed;

pub use address::{RpcAddress, RpcAddressError};
pub use context::{RpcCallId, RpcContext, RpcEndpointId};
pub use frame::RpcFrame;
pub use lane::RpcLaneStorage;
pub use registry::{
    RpcClient, RpcDirection, RpcError, RpcFailure, RpcRegistration, RpcRegistry, RpcResult,
};
pub use typed::{
    RpcCardinality, RpcHandlerFuture, RpcHandlerInput, RpcHandlerOutput, RpcInputMode, RpcMessage,
    RpcMethod, RpcMethodDescriptor, RpcOutputMode, RpcStream, RpcUnaryCall, Streaming,
    TypedRpcHandler, Unary,
};
