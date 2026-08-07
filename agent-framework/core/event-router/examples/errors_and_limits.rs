//! Demonstrates expected registration, signature, capacity, and provider errors.

use std::rc::Rc;

use claw_event_router::rpc::{
    RpcError, RpcFailure, RpcFrame, RpcLaneStorage, RpcMethod, RpcRegistry, RpcResult, Streaming,
    Unary,
};
use zerocopy::{Immutable, IntoBytes, KnownLayout, TryFromBytes};

#[repr(C)]
#[derive(Clone, Copy, Debug, Immutable, IntoBytes, KnownLayout, PartialEq, Eq, TryFromBytes)]
struct Number {
    value: u32,
}

struct Echo;

impl RpcMethod for Echo {
    const ADDRESS: &'static str = "errors.echo";

    type Request = Number;
    type Response = Number;
    type Input = Unary;
    type Output = Unary;
}

struct IncompatibleEcho;

impl RpcMethod for IncompatibleEcho {
    const ADDRESS: &'static str = Echo::ADDRESS;

    type Request = Number;
    type Response = Number;
    type Input = Unary;
    type Output = Streaming;
}

struct DomainFailure;

impl RpcMethod for DomainFailure {
    const ADDRESS: &'static str = "errors.domain_failure";

    type Request = Number;
    type Response = Number;
    type Input = Unary;
    type Output = Unary;
}

async fn run() -> RpcResult<()> {
    let lanes = Box::leak(Box::new(RpcLaneStorage::<2, 4_096, 8>::new()));
    let registry = Rc::new(RpcRegistry::new(lanes)?);
    registry.register_typed::<Echo, _>(|_context, request: RpcFrame<Number>| async move {
        Ok(*request.view()?)
    })?;

    let duplicate =
        registry.register_typed::<Echo, _>(|_context, request: RpcFrame<Number>| async move {
            Ok(*request.view()?)
        });
    assert!(matches!(duplicate, Err(RpcError::AlreadyRegistered(_))));

    // The method descriptor is checked before request encoding starts. The
    // address matches, but response cardinality does not.
    let mismatch = registry
        .client()
        .call::<IncompatibleEcho>(Number { value: 1 });
    assert!(matches!(mismatch, Err(RpcError::SignatureMismatch { .. })));

    let small_lanes = Box::leak(Box::new(RpcLaneStorage::<1, 2, 1>::new()));
    let small_registry = RpcRegistry::new(small_lanes)?;
    let oversized = small_registry.register_typed::<Echo, _>(
        |_context, request: RpcFrame<Number>| async move { Ok(*request.view()?) },
    );
    assert!(matches!(
        oversized,
        Err(RpcError::MethodFrameExceedsLane {
            frame_size: 4,
            lane_capacity: 2,
            ..
        })
    ));

    registry.register_typed::<DomainFailure, _>(
        |_context, _request: RpcFrame<Number>| async move {
            Err(RpcError::Provider(RpcFailure::new(
                "quota_exhausted",
                "the provider has no remaining quota",
            )))
        },
    )?;
    let failure = registry
        .client()
        .call::<DomainFailure>(Number { value: 1 })?
        .await;
    match failure {
        Err(RpcError::Provider(failure)) => {
            assert_eq!(failure.code(), "quota_exhausted");
            assert_eq!(failure.message(), "the provider has no remaining quota");
        }
        other => {
            println!("unexpected domain failure result: {other:?}");
            return Err(RpcError::Provider(RpcFailure::new(
                "unexpected_result",
                "domain failure example returned an unexpected result",
            )));
        }
    }

    println!("registration, signature, capacity, and provider errors completed");
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> RpcResult<()> {
    run().await
}
