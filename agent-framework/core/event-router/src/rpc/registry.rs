use core::cell::Cell;
use core::future::Future;
use core::mem::size_of;
use core::pin::Pin;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use super::address::{RpcAddress, RpcAddressError};
use super::context::{RpcCallId, RpcContext, RpcEndpointId};
use super::lane::{LaneAcquire, LaneIo, LaneReader, LaneWriter, RpcLaneStorage};
use super::typed::{
    RpcInputMode, RpcMethod, RpcMethodDescriptor, RpcOutputMode, TypedProvider, TypedRpcHandler,
};

/// Request or response side of one full-duplex RPC lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RpcDirection {
    /// Data sent from caller to provider.
    Request,
    /// Data sent from provider to caller.
    Response,
}

impl core::fmt::Display for RpcDirection {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Request => formatter.write_str("request"),
            Self::Response => formatter.write_str("response"),
        }
    }
}

/// Result returned by RPC operations.
pub type RpcResult<T> = Result<T, RpcError>;

pub(crate) type RpcFuture<'a> = Pin<Box<dyn Future<Output = RpcResult<()>> + 'a>>;

pub(crate) trait RpcProvider {
    fn call<'a>(
        &'a self,
        context: RpcContext,
        input: LaneReader,
        output: LaneWriter,
    ) -> RpcFuture<'a>;
}

/// Task-local registry that resolves RPC addresses to providers.
///
/// The registry uses [`Rc`] and deliberately does not require providers or
/// futures to be `Send`. It is intended to run inside Event Router's
/// cooperative executor thread. Root calls wait when every lane is active;
/// nested calls fail instead of waiting when doing so could deadlock.
pub struct RpcRegistry<const N: usize, const M: usize, const Q: usize> {
    endpoints: RefCell<HashMap<RpcAddress, EndpointEntry>>,
    next_endpoint_id: Cell<u64>,
    next_call_id: Cell<u64>,
    lanes: &'static RpcLaneStorage<N, M, Q>,
}

impl<const N: usize, const M: usize, const Q: usize> RpcRegistry<N, M, Q> {
    /// Creates an empty registry backed by fixed-capacity lane storage.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError::InvalidLaneConfiguration`] when the storage has no
    /// active lanes or no bytes in each request/response direction.
    pub fn new(lanes: &'static RpcLaneStorage<N, M, Q>) -> RpcResult<Self> {
        if N == 0 {
            return Err(RpcError::InvalidLaneConfiguration { field: "N" });
        }
        if M == 0 {
            return Err(RpcError::InvalidLaneConfiguration { field: "M" });
        }
        Ok(Self {
            endpoints: RefCell::new(HashMap::new()),
            next_endpoint_id: Cell::new(1),
            next_call_id: Cell::new(1),
            lanes,
        })
    }

    /// Creates an external client for this registry.
    ///
    /// Calls from the returned client start new root call chains.
    #[must_use]
    pub fn client(self: &Rc<Self>) -> RpcClient {
        let registry: Rc<dyn RegistryAccess> = self.clone();
        RpcClient {
            registry: Rc::downgrade(&registry),
            caller_endpoint_id: None,
            parent_call_id: None,
            root_call_id: None,
        }
    }

    /// Registers a typed provider for method `M`.
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
    /// struct TooLarge;
    ///
    /// impl RpcMethod for TooLarge {
    ///     const ADDRESS: &'static str = "static.too_large";
    ///     type Request = [u8; 65];
    ///     type Response = [u8; 1];
    ///     type Input = Unary;
    ///     type Output = Unary;
    /// }
    ///
    /// static LANES: ConstStaticCell<RpcLaneStorage<1, 64, 1>> =
    ///     ConstStaticCell::new(RpcLaneStorage::new());
    /// let registry = RpcRegistry::new(LANES.take())?;
    /// let _ = registry.register_typed::<TooLarge, _>(
    ///     |_context, _request: RpcFrame<[u8; 65]>| async move { Ok([0]) },
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
    ///     type Request = TooAligned;
    ///     type Response = [u8; 1];
    ///     type Input = Unary;
    ///     type Output = Unary;
    /// }
    ///
    /// static LANES: ConstStaticCell<RpcLaneStorage<1, 64, 1>> =
    ///     ConstStaticCell::new(RpcLaneStorage::new());
    /// let registry = RpcRegistry::new(LANES.take())?;
    /// let _ = registry.register_typed::<InvalidAlignment, _>(
    ///     |_context, _request: RpcFrame<TooAligned>| async move { Ok([0]) },
    /// )?;
    /// # Ok::<(), claw_event_router::rpc::RpcError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the method address is invalid, the address is
    /// occupied, or no endpoint identity remains.
    ///
    /// Compilation fails if the fixed request or response type is larger than
    /// the registry's `M`-byte lane frames or requires stricter alignment than
    /// the lane frame provides.
    pub fn register_typed<Method, H>(&self, handler: H) -> RpcResult<RpcRegistration>
    where
        Method: RpcMethod,
        H: TypedRpcHandler<Method> + 'static,
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
        }
        let descriptor = RpcMethodDescriptor::for_method::<Method>()?;
        let address = descriptor.address().clone();
        self.register_provider(
            address,
            Rc::new(TypedProvider::<Method, H>::new(handler)),
            descriptor,
        )
    }

    fn register_provider(
        &self,
        address: RpcAddress,
        provider: Rc<dyn RpcProvider>,
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
                provider,
                descriptor,
            },
        );
        Ok(registration)
    }

    /// Unregisters the exact endpoint instance represented by `registration`.
    ///
    /// Calls already in flight retain their provider and may finish normally.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError::StaleRegistration`] when the address is absent or
    /// now belongs to a different endpoint instance.
    pub fn unregister(&self, registration: &RpcRegistration) -> RpcResult<()> {
        let mut endpoints = self.endpoints.borrow_mut();
        let is_current = endpoints
            .get(registration.address())
            .is_some_and(|entry| entry.endpoint_id == registration.endpoint_id());
        if !is_current {
            return Err(RpcError::StaleRegistration(registration.address().clone()));
        }
        endpoints.remove(registration.address());
        Ok(())
    }

    /// Returns the number of currently registered endpoints.
    #[must_use]
    pub fn len(&self) -> usize {
        self.endpoints.borrow().len()
    }

    /// Returns whether no endpoints are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.endpoints.borrow().is_empty()
    }

    fn prepare_call(
        &self,
        registry: Weak<dyn RegistryAccess>,
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
        if caller.caller_endpoint_id == Some(endpoint.endpoint_id) {
            return Err(RpcError::DirectSelfCall(address.clone()));
        }

        let call_id = RpcCallId::new(self.take_call_id()?);
        let root_call_id = caller.root_call_id.unwrap_or(call_id);
        let nested_client = RpcClient {
            registry,
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

trait RegistryAccess {
    fn prepare_call(
        &self,
        registry: Weak<dyn RegistryAccess>,
        caller: &RpcClient,
        address: &RpcAddress,
        expected: &RpcMethodDescriptor,
    ) -> RpcResult<PreparedCall>;
}

impl<const N: usize, const M: usize, const Q: usize> RegistryAccess for RpcRegistry<N, M, Q> {
    fn prepare_call(
        &self,
        registry: Weak<dyn RegistryAccess>,
        caller: &RpcClient,
        address: &RpcAddress,
        expected: &RpcMethodDescriptor,
    ) -> RpcResult<PreparedCall> {
        self.prepare_call(registry, caller, address, expected)
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
    provider: Rc<dyn RpcProvider>,
    descriptor: RpcMethodDescriptor,
}

pub(crate) struct PreparedCall {
    endpoint: EndpointEntry,
    context: RpcContext,
    lane: LaneAcquire,
}

impl PreparedCall {
    pub(crate) async fn acquire(self) -> RpcResult<AcquiredCall> {
        let lane = self.lane.await?;
        Ok(AcquiredCall {
            endpoint: self.endpoint,
            context: self.context,
            lane: Some(lane),
        })
    }
}

pub(crate) struct AcquiredCall {
    endpoint: EndpointEntry,
    context: RpcContext,
    lane: Option<LaneIo>,
}

impl AcquiredCall {
    pub(crate) fn take_lane(&mut self) -> RpcResult<LaneIo> {
        self.lane.take().ok_or(RpcError::InvalidLaneState)
    }

    pub(crate) fn start(self, input: LaneReader, output: LaneWriter) -> RpcFuture<'static> {
        let endpoint = self.endpoint;
        let context = self.context;
        Box::pin(async move { endpoint.provider.call(context, input, output).await })
    }
}

/// Handle used to invoke endpoints in one registry.
#[derive(Clone)]
pub struct RpcClient {
    registry: Weak<dyn RegistryAccess>,
    caller_endpoint_id: Option<RpcEndpointId>,
    parent_call_id: Option<RpcCallId>,
    root_call_id: Option<RpcCallId>,
}

impl RpcClient {
    /// Starts a typed call for method `M`.
    ///
    /// The one method selects its input and output shape through `M`; callers do
    /// not choose between separate unary and streaming entry points. The
    /// returned future or stream drives request encoding, provider execution,
    /// and response decoding together.
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
    ) -> RpcResult<<M::Output as RpcOutputMode<M::Response>>::ClientCall>
    where
        M: RpcMethod,
    {
        let descriptor = RpcMethodDescriptor::for_method::<M>()?;
        let registry = self.registry.upgrade().ok_or(RpcError::RegistryDropped)?;
        let prepared = registry.prepare_call(
            self.registry.clone(),
            self,
            descriptor.address(),
            &descriptor,
        )?;
        let setup = super::typed::setup_call::<M>(prepared, input);
        Ok(super::typed::make_client_call::<M>(setup))
    }
}

/// Identity token for safely unregistering one endpoint instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcRegistration {
    address: RpcAddress,
    endpoint_id: RpcEndpointId,
}

impl RpcRegistration {
    /// Returns the registered address.
    #[must_use]
    pub fn address(&self) -> &RpcAddress {
        &self.address
    }

    /// Returns the registered endpoint instance identity.
    #[must_use]
    pub fn endpoint_id(&self) -> RpcEndpointId {
        self.endpoint_id
    }
}

/// Structured provider failure suitable for an RPC protocol boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcFailure {
    code: Box<str>,
    message: Box<str>,
}

impl RpcFailure {
    /// Creates a provider-defined failure with a stable machine code.
    #[must_use]
    pub fn new(code: impl Into<Box<str>>, message: impl Into<Box<str>>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// Returns the stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Returns the human-readable detail.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Error returned while registering or invoking RPC endpoints.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RpcError {
    /// Address parsing failed at an API boundary.
    #[error(transparent)]
    Address(#[from] RpcAddressError),
    /// A second provider attempted to occupy an existing address.
    #[error("RPC endpoint is already registered: {0}")]
    AlreadyRegistered(RpcAddress),
    /// No provider currently owns the requested address.
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
        /// Client method marker name.
        expected: &'static str,
        /// Registered method marker name.
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
    /// Fixed lane storage has zero active lanes or zero frame bytes.
    #[error("invalid RPC lane configuration: {field} must be nonzero")]
    InvalidLaneConfiguration {
        /// Invalid const generic field.
        field: &'static str,
    },
    /// A typed frame writer was already closed.
    #[error("typed RPC frame writer is closed")]
    FrameWriterClosed,
    /// The typed frame receiver was dropped before the writer completed.
    #[error("typed RPC frame reader is closed")]
    FrameReaderClosed,
    /// Frame bytes do not satisfy a message's size, alignment, or validity.
    #[error("invalid fixed-layout RPC frame for {message_type}")]
    InvalidMessageFrame {
        /// Rust message type that rejected the bytes.
        message_type: &'static str,
    },
    /// A provider returned a domain-specific failure.
    #[error("RPC provider failed ({code}): {message}", code = .0.code(), message = .0.message())]
    Provider(RpcFailure),
}
