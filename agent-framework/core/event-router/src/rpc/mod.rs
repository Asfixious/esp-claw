//! Task-local RPC registry, typed messages, and raw binary transport.
//!
//! The transport kernel has one binary `reader -> writer` shape. Rust callers
//! normally use [`RpcClient::call`] with an [`RpcMethod`], which serializes
//! owned structs as bounded Postcard frames and selects unary or streaming
//! cardinality at the type level. [`RpcClient::call_raw`] remains available for
//! wire adapters and non-Rust integrations.

mod address;
mod context;
mod frame;
mod io;
mod registry;
mod typed;

pub use address::{RpcAddress, RpcAddressError};
pub use context::{RpcCallId, RpcContext, RpcEndpointId};
pub use io::{
    binary_pipe, close, flush, read, read_to_end, write, write_all, BinaryIoError,
    BinaryPipeReader, BinaryPipeWriter, BinaryReader, BinaryWriter, BoxBinaryReader,
    BoxBinaryWriter, BytesReader,
};
pub use registry::{
    RawRpcProvider, RpcClient, RpcError, RpcFailure, RpcFuture, RpcRegistration, RpcRegistry,
    RpcResult,
};
pub use typed::{
    RpcCardinality, RpcHandlerFuture, RpcHandlerInput, RpcHandlerOutput, RpcInputMode, RpcMessage,
    RpcMethod, RpcMethodDescriptor, RpcOutputMode, RpcStream, RpcUnaryCall, Streaming,
    TypedRpcHandler, Unary,
};
