use super::registry::RpcClient;

/// Identity of one RPC invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RpcCallId(u64);

impl RpcCallId {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric identifier.
    #[must_use]
    pub fn value(self) -> u64 {
        self.0
    }
}

/// Identity of one registered endpoint instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RpcEndpointId(u64);

impl RpcEndpointId {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric identifier.
    #[must_use]
    pub fn value(self) -> u64 {
        self.0
    }
}

/// Metadata and nested-call client supplied to a raw or typed RPC provider.
#[derive(Clone)]
pub struct RpcContext {
    call_id: RpcCallId,
    root_call_id: RpcCallId,
    parent_call_id: Option<RpcCallId>,
    caller_endpoint_id: Option<RpcEndpointId>,
    endpoint_id: RpcEndpointId,
    client: RpcClient,
}

impl RpcContext {
    pub(crate) fn new(
        call_id: RpcCallId,
        root_call_id: RpcCallId,
        parent_call_id: Option<RpcCallId>,
        caller_endpoint_id: Option<RpcEndpointId>,
        endpoint_id: RpcEndpointId,
        client: RpcClient,
    ) -> Self {
        Self {
            call_id,
            root_call_id,
            parent_call_id,
            caller_endpoint_id,
            endpoint_id,
            client,
        }
    }

    /// Returns this invocation's identity.
    #[must_use]
    pub fn call_id(&self) -> RpcCallId {
        self.call_id
    }

    /// Returns the root invocation identity shared by the nested call chain.
    #[must_use]
    pub fn root_call_id(&self) -> RpcCallId {
        self.root_call_id
    }

    /// Returns the immediate parent invocation, if this is a nested call.
    #[must_use]
    pub fn parent_call_id(&self) -> Option<RpcCallId> {
        self.parent_call_id
    }

    /// Returns the endpoint that initiated this call, if any.
    #[must_use]
    pub fn caller_endpoint_id(&self) -> Option<RpcEndpointId> {
        self.caller_endpoint_id
    }

    /// Returns the endpoint handling this invocation.
    #[must_use]
    pub fn endpoint_id(&self) -> RpcEndpointId {
        self.endpoint_id
    }

    /// Returns a client bound to the current endpoint and call lineage.
    ///
    /// Calls made through this client become children of the current call. A
    /// direct call back into the same endpoint instance is rejected, while an
    /// indirect `A -> B -> A` chain remains valid.
    #[must_use]
    pub fn client(&self) -> &RpcClient {
        &self.client
    }
}
