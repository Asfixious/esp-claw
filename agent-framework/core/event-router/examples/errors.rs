//! Demonstrates expected registration, signature, and typed method errors.

use claw_event_router::rpc::{
    RpcError, RpcFrame, RpcLaneStorage, RpcMethod, RpcRegistry, RpcResult, Streaming, Unary,
};
use static_cell::ConstStaticCell;
use zerocopy::{Immutable, IntoBytes, KnownLayout, TryFromBytes};

static RPC_LANES: ConstStaticCell<RpcLaneStorage<2, 4_096, 8>> =
    ConstStaticCell::new(RpcLaneStorage::new());

#[repr(C)]
#[derive(Clone, Copy, Debug, Immutable, IntoBytes, KnownLayout, PartialEq, Eq, TryFromBytes)]
struct Number {
    value: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Immutable, IntoBytes, KnownLayout, PartialEq, Eq, TryFromBytes)]
struct QuotaError {
    remaining: u32,
}

struct Echo;

impl RpcMethod for Echo {
    const ADDRESS: &'static str = "errors.echo";

    type Request = Number;
    type Response = Number;
    type Error = ();
    type Input = Unary;
    type Output = Unary;
}

struct IncompatibleEcho;

impl RpcMethod for IncompatibleEcho {
    const ADDRESS: &'static str = Echo::ADDRESS;

    type Request = Number;
    type Response = Number;
    type Error = ();
    type Input = Unary;
    type Output = Streaming;
}

struct DomainFailure;

impl RpcMethod for DomainFailure {
    const ADDRESS: &'static str = "errors.domain_failure";

    type Request = Number;
    type Response = Number;
    type Error = QuotaError;
    type Input = Unary;
    type Output = Unary;
}

async fn run() -> RpcResult<()> {
    let lanes = RPC_LANES.take();
    let registry = RpcRegistry::new(lanes);
    registry.register::<Echo, _>(|_context, request: RpcFrame<Number>| async move {
        Ok(Ok(*request.view()?))
    })?;

    let duplicate =
        registry.register::<Echo, _>(|_context, request: RpcFrame<Number>| async move {
            Ok(Ok(*request.view()?))
        });
    assert!(matches!(duplicate, Err(RpcError::AlreadyRegistered(_))));

    // The method descriptor is checked before request encoding starts. The
    // address matches, but response cardinality does not.
    let mismatch = registry
        .client()
        .call::<IncompatibleEcho>(Number { value: 1 });
    assert!(matches!(mismatch, Err(RpcError::SignatureMismatch { .. })));

    registry.register::<DomainFailure, _>(|_context, _request: RpcFrame<Number>| async move {
        Ok(Err(QuotaError { remaining: 0 }))
    })?;
    let failure = registry
        .client()
        .call::<DomainFailure>(Number { value: 1 })?
        .await?;
    match failure {
        Err(failure) => {
            assert_eq!(failure.view()?, &QuotaError { remaining: 0 });
        }
        Ok(response) => {
            println!("unexpected domain success: {response:?}");
            return Err(RpcError::InvalidFrameState);
        }
    }

    println!("registration, signature, and typed method errors completed");
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> RpcResult<()> {
    run().await
}
