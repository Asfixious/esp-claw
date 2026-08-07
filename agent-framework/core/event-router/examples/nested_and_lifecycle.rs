//! Demonstrates nested call context, self-call protection, and registration lifecycle.

use std::rc::Rc;

use claw_event_router::rpc::{
    RpcContext, RpcError, RpcFrame, RpcLaneStorage, RpcMethod, RpcRegistry, RpcResult, Unary,
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

struct Increment;

impl RpcMethod for Increment {
    const ADDRESS: &'static str = "nested.increment";

    type Request = Number;
    type Response = Number;
    type Input = Unary;
    type Output = Unary;
}

struct IncrementTwice;

impl RpcMethod for IncrementTwice {
    const ADDRESS: &'static str = "nested.increment_twice";

    type Request = Number;
    type Response = Number;
    type Input = Unary;
    type Output = Unary;
}

struct CallsItself;

impl RpcMethod for CallsItself {
    const ADDRESS: &'static str = "nested.calls_itself";

    type Request = Number;
    type Response = Number;
    type Input = Unary;
    type Output = Unary;
}

async fn run() -> RpcResult<()> {
    let lanes = RPC_LANES.take();
    let registry = Rc::new(RpcRegistry::new(lanes)?);

    let increment_registration = registry.register_typed::<Increment, _>(
        |context: RpcContext, request: RpcFrame<Number>| async move {
            println!(
                "increment call={} root={} parent={:?} caller={:?} endpoint={}",
                context.call_id().value(),
                context.root_call_id().value(),
                context.parent_call_id().map(|id| id.value()),
                context.caller_endpoint_id().map(|id| id.value()),
                context.endpoint_id().value(),
            );
            Ok(Number {
                value: request.view()?.value.saturating_add(1),
            })
        },
    )?;

    let twice_registration = registry.register_typed::<IncrementTwice, _>(
        |context: RpcContext, request: RpcFrame<Number>| async move {
            // RpcContext carries a client with the current endpoint and call
            // chain already attached. Nested calls therefore propagate root,
            // parent, and caller endpoint identities automatically.
            let request = *request.view()?;
            let once_frame = context.client().call::<Increment>(request)?.await?;
            let once = *once_frame.view()?;
            drop(once_frame);
            let twice_frame = context.client().call::<Increment>(once)?.await?;
            Ok(*twice_frame.view()?)
        },
    )?;

    registry.register_typed::<CallsItself, _>(
        |context: RpcContext, request: RpcFrame<Number>| async move {
            // This resolves to DirectSelfCall before another provider future starts.
            let frame = context
                .client()
                .call::<CallsItself>(*request.view()?)?
                .await?;
            Ok(*frame.view()?)
        },
    )?;

    let client = registry.client();
    let nested_result = client.call::<IncrementTwice>(Number { value: 10 })?.await?;
    assert_eq!(nested_result.view()?, &Number { value: 12 });
    drop(nested_result);

    let self_call = client.call::<CallsItself>(Number { value: 1 })?.await;
    assert!(matches!(self_call, Err(RpcError::DirectSelfCall(_))));

    // Starting a call retains its exact provider instance. Unregistering the
    // endpoint rejects new calls but does not cancel the in-flight call.
    let in_flight = client.call::<Increment>(Number { value: 20 })?;
    registry.unregister(&increment_registration)?;
    assert_eq!(in_flight.await?.view()?, &Number { value: 21 });
    assert!(matches!(
        client.call::<Increment>(Number { value: 30 }),
        Err(RpcError::NotFound(_))
    ));

    registry.unregister(&twice_registration)?;
    assert!(matches!(
        registry.unregister(&twice_registration),
        Err(RpcError::StaleRegistration(_))
    ));

    println!("nested calls, self-call guard, and lifecycle behavior completed");
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> RpcResult<()> {
    run().await
}
