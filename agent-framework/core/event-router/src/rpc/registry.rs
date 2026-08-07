use core::cell::Cell;
use core::future::Future;
use core::marker::PhantomData;
use core::mem::size_of;
use core::pin::Pin;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use getset::CopyGetters;

use super::address::{RpcAddress, RpcAddressError, RpcGroup};
use super::context::{RpcCallId, RpcContext, RpcEndpointId};
use super::lane::{LaneAcquire, LaneIo, LanePool, LaneReader, LaneWriter, RpcLaneStorage};
use super::payload::{RpcPayloadReader, RpcPayloadWriter};
use super::typed::{
    HandlerAdapter, RpcHandler, RpcInputMode, RpcMethod, RpcMethodDescriptor, RpcOutputMode,
};

/// Request or response side of one full-duplex RPC lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RpcDirection {
    /// Data sent from caller to handler.
    Request,
    /// Data sent from handler to caller.
    Response,
}

/// Result returned by RPC operations.
pub type RpcResult<T> = Result<T, RpcError>;

pub(crate) type RpcFuture<'a> = Pin<Box<dyn Future<Output = RpcResult<()>> + 'a>>;

pub(crate) trait ErasedRpcHandler {
    fn call<'a>(
        &'a self,
        context: RpcContext,
        input: LaneReader,
        output: LaneWriter,
    ) -> RpcFuture<'a>;
}

/// Task-local registry that resolves typed RPC addresses.
///
/// The registry internally shares its task-local core with clients and does
/// not require handlers or futures to be `Send`. It is intended to run inside
/// Event Router's cooperative executor thread. Root calls wait when every lane
/// is active; nested calls fail instead of waiting when doing so could deadlock.
pub struct RpcRegistry<const N: usize, const M: usize, const Q: usize> {
    core: Rc<RegistryCore>,
    storage: PhantomData<&'static RpcLaneStorage<N, M, Q>>,
}

struct RegistryCore {
    endpoints: RefCell<HashMap<RpcAddress, EndpointEntry>>,
    next_endpoint_id: Cell<u64>,
    next_call_id: Cell<u64>,
    lanes: &'static dyn LanePool,
}

impl<const N: usize, const M: usize, const Q: usize> RpcRegistry<N, M, Q> {
    /// Creates an empty registry backed by fixed-capacity lane storage.
    ///
    /// `N` and `M` must both be nonzero; invalid const-generic configurations
    /// fail at compile time.
    #[must_use]
    pub fn new(lanes: &'static RpcLaneStorage<N, M, Q>) -> Self {
        const {
            assert!(N > 0, "RPC lane count must be nonzero");
            assert!(M > 0, "RPC lane frame capacity must be nonzero");
        }
        Self {
            core: Rc::new(RegistryCore {
                endpoints: RefCell::new(HashMap::new()),
                next_endpoint_id: Cell::new(1),
                next_call_id: Cell::new(1),
                lanes,
            }),
            storage: PhantomData,
        }
    }

    /// Creates an external client for this registry.
    ///
    /// Calls from the returned client start new root call chains.
    #[must_use]
    pub fn client(&self) -> RpcClient {
        RpcClient {
            registry: Rc::downgrade(&self.core),
            caller_endpoint_id: None,
            parent_call_id: None,
            root_call_id: None,
        }
    }

    /// Returns a sorted snapshot of groups that contain RPCs.
    ///
    /// Each group appears once. Registering or unregistering an RPC does not
    /// mutate a previously returned snapshot; call this method again to observe
    /// the new registry state.
    #[must_use]
    pub fn groups(&self) -> Vec<RpcGroup> {
        let endpoints = self.core.endpoints.borrow();
        let mut groups = Vec::new();
        for address in endpoints.keys() {
            push_group(&mut groups, address);
        }
        groups.sort_unstable();
        groups
    }

    /// Returns a sorted snapshot of RPC addresses in `group`.
    ///
    /// An unknown group produces an empty snapshot. Registering or
    /// unregistering an RPC does not mutate a previously returned snapshot.
    #[must_use]
    pub fn rpcs(&self, group: &RpcGroup) -> Vec<RpcAddress> {
        let mut addresses: Vec<_> = self
            .core
            .endpoints
            .borrow()
            .keys()
            .filter(|address| address.group() == group.as_ref())
            .cloned()
            .collect();
        addresses.sort_unstable();
        addresses
    }

    /// Registers a handler for method `M`.
    ///
    /// The method descriptor is retained with the endpoint so typed clients can
    /// reject request, response, or cardinality mismatches before payload IO.
    ///
    /// # Compile-time layout checks
    ///
    /// A Method whose fixed-layout message exceeds `M` does not compile:
    ///
    /// ```compile_fail
    /// use claw_event_router::rpc::{
    ///     RpcFrame, RpcLaneStorage, RpcMethod, RpcRegistry, Unary,
    /// };
    /// use static_cell::ConstStaticCell;
    ///
    /// struct TooLargeError;
    ///
    /// impl RpcMethod for TooLargeError {
    ///     const ADDRESS: &'static str = "static.too_large";
    ///     type Request = [u8; 1];
    ///     type Response = [u8; 1];
    ///     type Error = [u8; 65];
    ///     type Input = Unary;
    ///     type Output = Unary;
    /// }
    ///
    /// static LANES: ConstStaticCell<RpcLaneStorage<1, 64, 1>> =
    ///     ConstStaticCell::new(RpcLaneStorage::new());
    /// let registry = RpcRegistry::new(LANES.take());
    /// let _ = registry.register::<TooLargeError, _>(
    ///     |_context, _request: RpcFrame<[u8; 1]>| async move { Ok(Ok([0])) },
    /// )?;
    /// # Ok::<(), claw_event_router::rpc::RpcError>(())
    /// ```
    ///
    /// A Method whose message requires stricter alignment than the lane frame
    /// also does not compile:
    ///
    /// ```compile_fail
    /// use claw_event_router::rpc::{
    ///     RpcFrame, RpcLaneStorage, RpcMethod, RpcRegistry, Unary,
    /// };
    /// use static_cell::ConstStaticCell;
    /// use zerocopy::{Immutable, IntoBytes, KnownLayout, TryFromBytes};
    ///
    /// #[repr(C, align(32))]
    /// #[derive(Immutable, IntoBytes, KnownLayout, TryFromBytes)]
    /// struct TooAligned([u8; 32]);
    ///
    /// struct InvalidAlignment;
    ///
    /// impl RpcMethod for InvalidAlignment {
    ///     const ADDRESS: &'static str = "static.invalid_alignment";
    ///     type Request = [u8; 1];
    ///     type Response = [u8; 1];
    ///     type Error = TooAligned;
    ///     type Input = Unary;
    ///     type Output = Unary;
    /// }
    ///
    /// static LANES: ConstStaticCell<RpcLaneStorage<1, 64, 1>> =
    ///     ConstStaticCell::new(RpcLaneStorage::new());
    /// let registry = RpcRegistry::new(LANES.take());
    /// let _ = registry.register::<InvalidAlignment, _>(
    ///     |_context, _request: RpcFrame<[u8; 1]>| async move { Ok(Ok([0])) },
    /// )?;
    /// # Ok::<(), claw_event_router::rpc::RpcError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the method address is invalid, the address is
    /// occupied, or no endpoint identity remains.
    ///
    /// Compilation fails if the fixed request, response, or method-error type is
    /// larger than the registry's `M`-byte lane frames or requires stricter
    /// alignment than the lane frame provides.
    pub fn register<Method, H>(&self, handler: H) -> RpcResult<RpcRegistration>
    where
        Method: RpcMethod,
        H: RpcHandler<Method> + 'static,
    {
        const {
            assert!(
                size_of::<Method::Request>() <= M,
                "RPC request message exceeds lane frame capacity"
            );
            assert!(
                size_of::<Method::Response>() <= M,
                "RPC response message exceeds lane frame capacity"
            );
            assert!(
                size_of::<Method::Error>() <= M,
                "RPC method error exceeds lane frame capacity"
            );
        }
        let descriptor = RpcMethodDescriptor::for_method::<Method>()?;
        let address = descriptor.address().clone();
        self.core.insert_handler(
            address,
            Rc::new(HandlerAdapter::<Method, H>::new(handler)),
            descriptor,
        )
    }

    /// Unregisters the exact endpoint instance represented by `registration`.
    ///
    /// Calls already in flight retain their handler and may finish normally.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError::StaleRegistration`] when the address is absent or
    /// now belongs to a different endpoint instance.
    pub fn unregister(&self, registration: &RpcRegistration) -> RpcResult<()> {
        let mut endpoints = self.core.endpoints.borrow_mut();
        let is_current = endpoints
            .get(&registration.address)
            .is_some_and(|entry| entry.endpoint_id == registration.endpoint_id);
        if !is_current {
            return Err(RpcError::StaleRegistration(registration.address.clone()));
        }
        endpoints.remove(&registration.address);
        Ok(())
    }
}

impl RegistryCore {
    fn insert_handler(
        &self,
        address: RpcAddress,
        handler: Rc<dyn ErasedRpcHandler>,
        descriptor: RpcMethodDescriptor,
    ) -> RpcResult<RpcRegistration> {
        if self.endpoints.borrow().contains_key(&address) {
            return Err(RpcError::AlreadyRegistered(address));
        }
        let endpoint_id = RpcEndpointId::new(self.take_endpoint_id()?);
        let registration = RpcRegistration {
            address: address.clone(),
            endpoint_id,
        };
        self.endpoints.borrow_mut().insert(
            address,
            EndpointEntry {
                endpoint_id,
                handler,
                descriptor,
            },
        );
        Ok(registration)
    }

    fn prepare_typed_call(
        self: &Rc<Self>,
        caller: &RpcClient,
        address: &RpcAddress,
        expected: &RpcMethodDescriptor,
    ) -> RpcResult<PreparedCall> {
        let endpoint = self
            .endpoints
            .borrow()
            .get(address)
            .cloned()
            .ok_or_else(|| RpcError::NotFound(address.clone()))?;
        if &endpoint.descriptor != expected {
            return Err(RpcError::SignatureMismatch {
                address: address.clone(),
                expected: expected.method_type_name(),
                registered: endpoint.descriptor.method_type_name(),
            });
        }
        self.prepare_resolved_call(caller, address, endpoint)
    }

    fn prepare_payload_call(
        self: &Rc<Self>,
        caller: &RpcClient,
        address: &RpcAddress,
    ) -> RpcResult<PreparedCall> {
        let endpoint = self
            .endpoints
            .borrow()
            .get(address)
            .cloned()
            .ok_or_else(|| RpcError::NotFound(address.clone()))?;
        self.prepare_resolved_call(caller, address, endpoint)
    }

    fn prepare_resolved_call(
        self: &Rc<Self>,
        caller: &RpcClient,
        address: &RpcAddress,
        endpoint: EndpointEntry,
    ) -> RpcResult<PreparedCall> {
        if caller.caller_endpoint_id == Some(endpoint.endpoint_id) {
            return Err(RpcError::DirectSelfCall(address.clone()));
        }

        let call_id = RpcCallId::new(self.take_call_id()?);
        let root_call_id = caller.root_call_id.unwrap_or(call_id);
        let nested_client = RpcClient {
            registry: Rc::downgrade(self),
            caller_endpoint_id: Some(endpoint.endpoint_id),
            parent_call_id: Some(call_id),
            root_call_id: Some(root_call_id),
        };
        let context = RpcContext::new(
            call_id,
            root_call_id,
            caller.parent_call_id,
            caller.caller_endpoint_id,
            endpoint.endpoint_id,
            nested_client,
        );
        Ok(PreparedCall {
            request_frame_size: endpoint.descriptor.request_frame_size(),
            endpoint,
            context,
            lane: LaneAcquire::new(self.lanes, caller.caller_endpoint_id.is_some()),
        })
    }

    fn take_endpoint_id(&self) -> RpcResult<u64> {
        take_identifier(&self.next_endpoint_id)
    }

    fn take_call_id(&self) -> RpcResult<u64> {
        take_identifier(&self.next_call_id)
    }
}

fn push_group(groups: &mut Vec<RpcGroup>, address: &RpcAddress) {
    let group = address.group();
    if !groups.iter().any(|registered| registered.as_ref() == group) {
        groups.push(RpcGroup::from_validated(group));
    }
}

fn take_identifier(next: &Cell<u64>) -> RpcResult<u64> {
    let value = next.get();
    let following = value.checked_add(1).ok_or(RpcError::IdentifierExhausted)?;
    next.set(following);
    Ok(value)
}

#[derive(Clone)]
struct EndpointEntry {
    endpoint_id: RpcEndpointId,
    handler: Rc<dyn ErasedRpcHandler>,
    descriptor: RpcMethodDescriptor,
}

#[derive(CopyGetters)]
pub(crate) struct PreparedCall {
    #[getset(get_copy = "pub(crate)")]
    request_frame_size: usize,
    endpoint: EndpointEntry,
    context: RpcContext,
    lane: LaneAcquire,
}

impl PreparedCall {
    pub(crate) fn poll_acquire(
        &mut self,
        context: &mut core::task::Context<'_>,
    ) -> core::task::Poll<RpcResult<LaneIo>> {
        Pin::new(&mut self.lane).poll(context)
    }

    pub(crate) fn start(self, input: LaneReader, output: LaneWriter) -> RpcFuture<'static> {
        let endpoint = self.endpoint;
        let context = self.context;
        Box::pin(async move { endpoint.handler.call(context, input, output).await })
    }
}

/// Handle used to invoke endpoints in one registry.
#[derive(Clone)]
pub struct RpcClient {
    registry: Weak<RegistryCore>,
    caller_endpoint_id: Option<RpcEndpointId>,
    parent_call_id: Option<RpcCallId>,
    root_call_id: Option<RpcCallId>,
}

impl RpcClient {
    /// Starts a typed call for method `M`.
    ///
    /// The one method selects its input and output shape through `M`; callers do
    /// not choose between separate unary and streaming entry points. The
    /// returned future or stream drives request encoding, handler execution,
    /// and response decoding together. Transport/runtime failures use the outer
    /// [`RpcResult`]; successful transport yields the Method's typed
    /// `Result<Response, Error>` as zero-copy frames.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry was dropped, the endpoint does not
    /// exist, its typed signature differs, identifiers are exhausted, or this
    /// is a direct synchronous self-call. The returned future or stream may
    /// later report lane acquisition or framing errors.
    pub fn call<M>(
        &self,
        input: <M::Input as RpcInputMode<M::Request>>::ClientInput,
    ) -> RpcResult<<M::Output as RpcOutputMode<M::Response, M::Error>>::ClientCall>
    where
        M: RpcMethod,
    {
        let descriptor = RpcMethodDescriptor::for_method::<M>()?;
        let address = descriptor.address().clone();
        let registry = self.registry.upgrade().ok_or(RpcError::RegistryDropped)?;
        let prepared = registry.prepare_typed_call(self, &address, &descriptor)?;
        Ok(super::typed::make_typed_call::<M>(prepared, input))
    }

    /// Starts an untyped wire-level call to the typed endpoint at `address`.
    ///
    /// The returned [`RpcPayloadWriter`] and [`RpcPayloadReader`] expose
    /// full-duplex asynchronous frame IO. Each written frame must match the
    /// registered [`RpcMethod::Request`] wire layout. Response and method-error
    /// frames remain borrowed from the lane until dropped. This entry point
    /// intentionally cannot perform compile-time signature or cardinality
    /// checks.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry was dropped, the endpoint does not
    /// exist, identifiers are exhausted, or this is a direct synchronous
    /// self-call. The returned stream may later report lane, frame, or typed
    /// request validation failures.
    pub fn call_payload(
        &self,
        address: &RpcAddress,
    ) -> RpcResult<(RpcPayloadWriter, RpcPayloadReader)> {
        let registry = self.registry.upgrade().ok_or(RpcError::RegistryDropped)?;
        let prepared = registry.prepare_payload_call(self, address)?;
        Ok(super::payload::make_payload_call(prepared))
    }
}

/// Identity token for safely unregistering one endpoint instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcRegistration {
    /// Registered address.
    address: RpcAddress,
    /// Registered endpoint instance identity.
    endpoint_id: RpcEndpointId,
}

/// Transport/runtime error returned while registering or invoking RPC endpoints.
///
/// Method-specific business errors are carried as typed method-error frames
/// and are not represented by this enum. Wire-level callers receive the same
/// frame through [`RpcPayloadReader`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RpcError {
    /// Address parsing failed at an API boundary.
    #[error(transparent)]
    Address(#[from] RpcAddressError),
    /// A second handler attempted to occupy an existing address.
    #[error("RPC endpoint is already registered: {0}")]
    AlreadyRegistered(RpcAddress),
    /// No handler currently owns the requested address.
    #[error("RPC endpoint not found: {0}")]
    NotFound(RpcAddress),
    /// An endpoint attempted to synchronously invoke itself.
    #[error("direct RPC self-call is forbidden: {0}")]
    DirectSelfCall(RpcAddress),
    /// The registry behind an [`RpcClient`] no longer exists.
    #[error("RPC registry was dropped")]
    RegistryDropped,
    /// The endpoint or call identifier space was exhausted.
    #[error("RPC identifier space exhausted")]
    IdentifierExhausted,
    /// An unregister token no longer identifies the current endpoint instance.
    #[error("stale RPC registration: {0}")]
    StaleRegistration(RpcAddress),
    /// The client method marker does not match the registered typed endpoint.
    #[error("RPC signature mismatch at {address}: expected {expected}, registered {registered:?}")]
    SignatureMismatch {
        /// Invoked address.
        address: RpcAddress,
        /// Signature requested by the client.
        expected: &'static str,
        /// Signature registered at the address.
        registered: &'static str,
    },
    /// Internal framing state became inconsistent.
    #[error("invalid RPC frame decoder state")]
    InvalidFrameState,
    /// A unary side reached EOF without carrying a message.
    #[error("unary RPC side did not contain a message")]
    MissingUnaryFrame,
    /// A completed unary call future was polled again.
    #[error("completed unary RPC call was polled again")]
    CompletedCallPolled,
    /// Fixed lane storage has no free slot for a nested call.
    #[error("nested RPC call cannot acquire one of {limit} lanes without deadlocking")]
    NestedLaneExhausted {
        /// Configured active lane count.
        limit: usize,
    },
    /// The bounded root-call waiter table is full.
    #[error("RPC lane waiter capacity {limit} is exhausted")]
    LaneWaiterCapacityExceeded {
        /// Configured waiter capacity.
        limit: usize,
    },
    /// Fixed lane state became internally inconsistent.
    #[error("invalid RPC lane state")]
    InvalidLaneState,
    /// An RPC frame writer was already closed.
    #[error("RPC frame writer is closed")]
    FrameWriterClosed,
    /// An RPC frame receiver was dropped before the writer completed.
    #[error("RPC frame reader is closed")]
    FrameReaderClosed,
    /// Frame bytes do not satisfy a message's size, alignment, or validity.
    #[error("invalid fixed-layout RPC frame for {message_type}")]
    InvalidMessageFrame {
        /// Rust message type that rejected the bytes.
        message_type: &'static str,
    },
    /// A runtime-sized payload exceeds the available bytes for its write.
    #[error("RPC frame size {size} exceeds available capacity {capacity}")]
    FrameTooLarge {
        /// Requested payload size.
        size: usize,
        /// Available bytes for this write.
        capacity: usize,
    },
    /// A non-empty `write_all` operation could not make forward progress.
    #[error("RPC payload writer accepted zero bytes")]
    PayloadWriteZero,
}
