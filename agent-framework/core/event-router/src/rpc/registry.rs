use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use super::address::{RpcAddress, RpcAddressError};
use super::context::{RpcCallId, RpcContext, RpcEndpointId};
use super::io::{binary_pipe, BinaryIoError, BoxBinaryReader, BoxBinaryWriter};
use super::typed::{
    RpcInputMode, RpcMethod, RpcMethodDescriptor, RpcOutputMode, TypedProvider, TypedRpcHandler,
};

/// Result returned by RPC operations.
pub type RpcResult<T> = Result<T, RpcError>;

/// Dynamically dispatched, task-local RPC execution.
pub type RpcFuture<'a> = Pin<Box<dyn Future<Output = RpcResult<()>> + 'a>>;

/// One implementation registered at an [`RpcAddress`].
///
/// Input and output always use the same binary interfaces. A provider chooses
/// unary behavior by consuming or producing a finite body and chooses streaming
/// behavior by keeping the corresponding side open while data is produced.
pub trait RawRpcProvider {
    /// Handles one invocation.
    ///
    /// Implementations must close `output` after producing the final byte. The
    /// returned future must be driven concurrently with streaming producers and
    /// consumers so bounded pipes can apply backpressure without deadlocking.
    fn call<'a>(
        &'a self,
        context: RpcContext,
        input: BoxBinaryReader,
        output: BoxBinaryWriter,
    ) -> RpcFuture<'a>;
}

/// Task-local registry that resolves RPC addresses to providers.
///
/// The registry uses [`Rc`] and deliberately does not require providers or
/// futures to be `Send`. It is intended to run inside Event Router's
/// cooperative executor thread.
pub struct RpcRegistry {
    endpoints: RefCell<HashMap<RpcAddress, EndpointEntry>>,
    next_endpoint_id: Cell<u64>,
    next_call_id: Cell<u64>,
}

impl RpcRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            endpoints: RefCell::new(HashMap::new()),
            next_endpoint_id: Cell::new(1),
            next_call_id: Cell::new(1),
        }
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

    /// Registers an untyped binary provider at an address.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError::AlreadyRegistered`] if the address is occupied or
    /// [`RpcError::IdentifierExhausted`] if no endpoint identity remains.
    pub fn register_raw(
        &self,
        address: RpcAddress,
        provider: impl RawRpcProvider + 'static,
    ) -> RpcResult<RpcRegistration> {
        self.register_provider(address, Rc::new(provider), None)
    }

    /// Registers a typed provider for method `M`.
    ///
    /// The method descriptor is retained with the endpoint so typed clients can
    /// reject request, response, or cardinality mismatches before payload IO.
    ///
    /// # Errors
    ///
    /// Returns an error if the method configuration or address is invalid, the
    /// address is occupied, or no endpoint identity remains.
    pub fn register_typed<M, H>(&self, handler: H) -> RpcResult<RpcRegistration>
    where
        M: RpcMethod,
        H: TypedRpcHandler<M> + 'static,
    {
        let descriptor = RpcMethodDescriptor::for_method::<M>()?;
        let address = descriptor.address().clone();
        self.register_provider(
            address,
            Rc::new(TypedProvider::<M, H>::new(handler)),
            Some(descriptor),
        )
    }

    fn register_provider(
        &self,
        address: RpcAddress,
        provider: Rc<dyn RawRpcProvider>,
        descriptor: Option<RpcMethodDescriptor>,
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

    fn begin_call(
        &self,
        registry: Weak<Self>,
        caller: &RpcClient,
        address: &RpcAddress,
        input: BoxBinaryReader,
        output: BoxBinaryWriter,
        expected: Option<&RpcMethodDescriptor>,
    ) -> RpcResult<RpcFuture<'static>> {
        let endpoint = self
            .endpoints
            .borrow()
            .get(address)
            .cloned()
            .ok_or_else(|| RpcError::NotFound(address.clone()))?;
        if let Some(expected) = expected {
            if endpoint.descriptor.as_ref() != Some(expected) {
                return Err(RpcError::SignatureMismatch {
                    address: address.clone(),
                    expected: expected.method_type_name(),
                    registered: endpoint
                        .descriptor
                        .as_ref()
                        .map(RpcMethodDescriptor::method_type_name),
                });
            }
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
        Ok(Box::pin(async move {
            endpoint.provider.call(context, input, output).await
        }))
    }

    fn take_endpoint_id(&self) -> RpcResult<u64> {
        take_identifier(&self.next_endpoint_id)
    }

    fn take_call_id(&self) -> RpcResult<u64> {
        take_identifier(&self.next_call_id)
    }
}

impl Default for RpcRegistry {
    fn default() -> Self {
        Self::new()
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
    provider: Rc<dyn RawRpcProvider>,
    descriptor: Option<RpcMethodDescriptor>,
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
    /// Returns an error when the method configuration is invalid, the registry
    /// was dropped, the endpoint does not exist, its typed signature differs,
    /// identifiers are exhausted, or this is a direct synchronous self-call.
    pub fn call<M>(
        &self,
        input: <M::Input as RpcInputMode<M::Request>>::ClientInput,
    ) -> RpcResult<<M::Output as RpcOutputMode<M::Response>>::ClientCall>
    where
        M: RpcMethod,
    {
        let descriptor = RpcMethodDescriptor::for_method::<M>()?;
        let registry = self.registry.upgrade().ok_or(RpcError::RegistryDropped)?;
        let (request_reader, request_writer) = binary_pipe(M::PIPE_CAPACITY)?;
        let (response_reader, response_writer) = binary_pipe(M::PIPE_CAPACITY)?;
        let provider = registry.begin_call(
            self.registry.clone(),
            self,
            descriptor.address(),
            Box::pin(request_reader),
            Box::pin(response_writer),
            Some(&descriptor),
        )?;
        let input = M::Input::encode_input(input, Box::pin(request_writer), M::MAX_REQUEST_FRAME);
        Ok(M::Output::make_client_call(
            input,
            provider,
            Box::pin(response_reader),
            M::MAX_RESPONSE_FRAME,
        ))
    }

    /// Starts an untyped call with caller-provided binary input and output.
    ///
    /// This method does not distinguish unary and streaming calls. Either side
    /// may be finite or incremental, independently of the other side.
    ///
    /// The returned future owns the selected provider, so unregistering the
    /// endpoint does not cancel an invocation that has already started.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry was dropped, the endpoint does not
    /// exist, identifiers are exhausted, or this client attempts a direct
    /// synchronous self-call.
    pub fn call_raw(
        &self,
        address: &RpcAddress,
        input: BoxBinaryReader,
        output: BoxBinaryWriter,
    ) -> RpcResult<RpcFuture<'static>> {
        let registry = self.registry.upgrade().ok_or(RpcError::RegistryDropped)?;
        registry.begin_call(self.registry.clone(), self, address, input, output, None)
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
    /// A typed method declared a zero frame limit or pipe capacity.
    #[error("invalid RPC method configuration for {address}: {field} must be nonzero")]
    InvalidMethodConfiguration {
        /// Method address.
        address: RpcAddress,
        /// Invalid associated constant.
        field: &'static str,
    },
    /// The client method marker does not match the registered typed endpoint.
    #[error("RPC signature mismatch at {address}: expected {expected}, registered {registered:?}")]
    SignatureMismatch {
        /// Invoked address.
        address: RpcAddress,
        /// Client method marker name.
        expected: &'static str,
        /// Registered method marker name, or `None` for a raw endpoint.
        registered: Option<&'static str>,
    },
    /// One encoded or declared frame exceeds its method limit.
    #[error("RPC frame size {size} exceeds limit {limit}")]
    FrameTooLarge {
        /// Encoded or declared frame size.
        size: usize,
        /// Configured maximum frame size.
        limit: usize,
    },
    /// A peer closed its stream in the middle of a frame.
    #[error("RPC stream ended in the middle of a frame")]
    IncompleteFrame,
    /// Internal framing state became inconsistent.
    #[error("invalid RPC frame decoder state")]
    InvalidFrameState,
    /// A unary side reached EOF without carrying a message.
    #[error("unary RPC side did not contain a message")]
    MissingUnaryFrame,
    /// A unary side carried more than one message.
    #[error("unary RPC side contained more than one message")]
    ExtraUnaryFrame,
    /// A completed unary call future was polled again.
    #[error("completed unary RPC call was polled again")]
    CompletedCallPolled,
    /// Postcard could not serialize a typed message.
    #[error("failed to encode typed RPC message: {0}")]
    Encode(#[source] postcard::Error),
    /// Postcard could not deserialize a typed message.
    #[error("failed to decode typed RPC message: {0}")]
    Decode(#[source] postcard::Error),
    /// A binary input or output operation failed.
    #[error(transparent)]
    BinaryIo(#[from] BinaryIoError),
    /// A provider returned a domain-specific failure.
    #[error("RPC provider failed ({code}): {message}", code = .0.code(), message = .0.message())]
    Provider(RpcFailure),
}

impl RpcError {
    pub(crate) fn encode(source: postcard::Error) -> Self {
        Self::Encode(source)
    }

    pub(crate) fn decode(source: postcard::Error) -> Self {
        Self::Decode(source)
    }
}
