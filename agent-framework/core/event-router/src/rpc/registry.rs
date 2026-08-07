use core::cell::Cell;
use core::future::Future;
use core::mem::size_of;
use core::pin::Pin;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use super::address::{RpcAddress, RpcAddressError};
use super::context::{RpcCallId, RpcContext, RpcEndpointId};
use super::lane::{LaneAcquire, LaneIo, LanePool, LaneReader, LaneWriter, RpcLaneStorage};
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
pub struct RpcRegistry {
    endpoints: RefCell<HashMap<RpcAddress, EndpointEntry>>,
    next_endpoint_id: Cell<u64>,
    next_call_id: Cell<u64>,
    lanes: &'static dyn LanePool,
}

impl RpcRegistry {
    /// Creates an empty registry backed by fixed-capacity lane storage.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError::InvalidLaneConfiguration`] when the storage has no
    /// active lanes or no bytes in each request/response direction.
    pub fn new<const N: usize, const M: usize, const Q: usize>(
        lanes: &'static RpcLaneStorage<N, M, Q>,
    ) -> RpcResult<Self> {
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
        RpcClient {
            registry: Rc::downgrade(self),
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
    /// # Errors
    ///
    /// Returns an error if the method configuration or address is invalid, the
    /// address is occupied, either message type exceeds the lane capacity, or
    /// no endpoint identity remains.
    pub fn register_typed<M, H>(&self, handler: H) -> RpcResult<RpcRegistration>
    where
        M: RpcMethod,
        H: TypedRpcHandler<M> + 'static,
    {
        let descriptor = RpcMethodDescriptor::for_method::<M>()?;
        self.validate_method_capacity::<M>(&descriptor)?;
        let address = descriptor.address().clone();
        self.register_provider(
            address,
            Rc::new(TypedProvider::<M, H>::new(handler)),
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
        registry: Weak<Self>,
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

    fn validate_method_capacity<M>(&self, descriptor: &RpcMethodDescriptor) -> RpcResult<()>
    where
        M: RpcMethod,
    {
        let lane_capacity = self.lanes.frame_capacity();
        let request_size = size_of::<M::Request>();
        if request_size > lane_capacity {
            return Err(RpcError::MethodFrameExceedsLane {
                address: descriptor.address().clone(),
                direction: RpcDirection::Request,
                frame_size: request_size,
                lane_capacity,
            });
        }
        let response_size = size_of::<M::Response>();
        if response_size > lane_capacity {
            return Err(RpcError::MethodFrameExceedsLane {
                address: descriptor.address().clone(),
                direction: RpcDirection::Response,
                frame_size: response_size,
                lane_capacity,
            });
        }
        Ok(())
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
    registry: Weak<RpcRegistry>,
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
    /// exist, its typed signature differs, either message type exceeds the lane
    /// capacity, identifiers are exhausted, or this is a direct synchronous
    /// self-call. The returned future or stream may later report lane
    /// acquisition or framing errors.
    pub fn call<M>(
        &self,
        input: <M::Input as RpcInputMode<M::Request>>::ClientInput,
    ) -> RpcResult<<M::Output as RpcOutputMode<M::Response>>::ClientCall>
    where
        M: RpcMethod,
    {
        let descriptor = RpcMethodDescriptor::for_method::<M>()?;
        let registry = self.registry.upgrade().ok_or(RpcError::RegistryDropped)?;
        registry.validate_method_capacity::<M>(&descriptor)?;
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
    /// A method message is larger than the configured lane direction.
    #[error(
        "RPC {direction} message size {frame_size} for {address} exceeds lane capacity {lane_capacity}"
    )]
    MethodFrameExceedsLane {
        /// Method address.
        address: RpcAddress,
        /// Request or response direction.
        direction: RpcDirection,
        /// Fixed-layout message size.
        frame_size: usize,
        /// Lane direction capacity.
        lane_capacity: usize,
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
    /// A message requires stricter alignment than fixed lanes provide.
    #[error(
        "RPC message {message_type} requires {required}-byte alignment, lane provides {available}"
    )]
    MessageAlignmentExceedsLane {
        /// Rust message type whose alignment cannot be provided.
        message_type: &'static str,
        /// Alignment required by the message.
        required: usize,
        /// Alignment guaranteed by every lane frame.
        available: usize,
    },
    /// A provider returned a domain-specific failure.
    #[error("RPC provider failed ({code}): {message}", code = .0.code(), message = .0.message())]
    Provider(RpcFailure),
}
