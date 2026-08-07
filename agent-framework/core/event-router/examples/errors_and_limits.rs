//! Demonstrates expected registration, signature, framing, and provider errors.

use std::rc::Rc;

use claw_event_router::rpc::{
    RpcError, RpcFailure, RpcMethod, RpcRegistry, RpcResult, Streaming, Unary,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
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

struct TinyRequestFrame;

impl RpcMethod for TinyRequestFrame {
    const ADDRESS: &'static str = "errors.tiny_frame";
    const MAX_REQUEST_FRAME: usize = 1;
    const PIPE_CAPACITY: usize = 1;

    type Request = Number;
    type Response = Number;
    type Input = Unary;
    type Output = Unary;
}

struct InvalidConfiguration;

impl RpcMethod for InvalidConfiguration {
    const ADDRESS: &'static str = "errors.invalid_configuration";
    const PIPE_CAPACITY: usize = 0;

    type Request = Number;
    type Response = Number;
    type Input = Unary;
    type Output = Unary;
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
    let registry = Rc::new(RpcRegistry::new());
    registry.register_typed::<Echo, _>(|_context, request: Number| async move { Ok(request) })?;

    let duplicate =
        registry.register_typed::<Echo, _>(|_context, request: Number| async move { Ok(request) });
    assert!(matches!(duplicate, Err(RpcError::AlreadyRegistered(_))));

    // The method descriptor is checked before request encoding starts. The
    // address matches, but response cardinality does not.
    let mismatch = registry
        .client()
        .call::<IncompatibleEcho>(Number { value: 1 });
    assert!(matches!(mismatch, Err(RpcError::SignatureMismatch { .. })));

    registry.register_typed::<TinyRequestFrame, _>(|_context, request: Number| async move {
        Ok(request)
    })?;
    let oversized = registry
        .client()
        .call::<TinyRequestFrame>(Number { value: u32::MAX })?
        .await;
    assert!(matches!(
        oversized,
        Err(RpcError::FrameTooLarge { limit: 1, .. })
    ));

    let invalid = registry.register_typed::<InvalidConfiguration, _>(
        |_context, request: Number| async move { Ok(request) },
    );
    assert!(matches!(
        invalid,
        Err(RpcError::InvalidMethodConfiguration {
            field: "PIPE_CAPACITY",
            ..
        })
    ));

    registry.register_typed::<DomainFailure, _>(|_context, _request: Number| async move {
        Err(RpcError::Provider(RpcFailure::new(
            "quota_exhausted",
            "the provider has no remaining quota",
        )))
    })?;
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

    println!("registration, signature, limit, configuration, and provider errors completed");
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> RpcResult<()> {
    run().await
}
