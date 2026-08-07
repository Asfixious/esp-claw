//! Demonstrates fixed RPC lane storage, root-call backpressure, and reuse.

use std::rc::Rc;

use claw_event_router::rpc::{
    RpcError, RpcFrame, RpcLaneStorage, RpcMethod, RpcRegistry, RpcResult, Unary,
};
use futures_util::join;
use static_cell::ConstStaticCell;
use zerocopy::{Immutable, IntoBytes, KnownLayout, TryFromBytes};

static RPC_LANES: ConstStaticCell<RpcLaneStorage<1, 64, 2>> =
    ConstStaticCell::new(RpcLaneStorage::new());

#[repr(C)]
#[derive(Clone, Copy, Debug, Immutable, IntoBytes, KnownLayout, PartialEq, Eq, TryFromBytes)]
struct Number {
    value: u32,
}

struct YieldingEcho;

impl RpcMethod for YieldingEcho {
    const ADDRESS: &'static str = "lanes.yielding_echo";

    type Request = Number;
    type Response = Number;
    type Input = Unary;
    type Output = Unary;
}

async fn run() -> RpcResult<()> {
    // One lane permits one active call. Each direction owns 64 fixed payload
    // bytes, and at most two root calls can wait without allocating a queue.
    let lanes = RPC_LANES.take();
    assert_eq!(lanes.lane_count(), 1);
    assert_eq!(lanes.frame_capacity(), 64);
    assert_eq!(lanes.waiter_capacity(), 2);

    let registry = Rc::new(RpcRegistry::new(lanes)?);
    registry.register_typed::<YieldingEcho, _>(
        |_context, request: RpcFrame<Number>| async move {
            // Let the second root call observe the occupied lane and enter the
            // fixed waiter table before this call returns its lane.
            tokio::task::yield_now().await;
            Ok(*request.view()?)
        },
    )?;

    let client = registry.client();
    let first = client.call::<YieldingEcho>(Number { value: 1 })?;
    let second = client.call::<YieldingEcho>(Number { value: 2 })?;
    let first = async move {
        let frame = first.await?;
        Ok::<Number, RpcError>(*frame.view()?)
    };
    let second = async move {
        let frame = second.await?;
        Ok::<Number, RpcError>(*frame.view()?)
    };
    let (first, second) = join!(first, second);

    assert_eq!(first?, Number { value: 1 });
    assert_eq!(second?, Number { value: 2 });
    println!("one static lane serialized two concurrent root calls");
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> RpcResult<()> {
    run().await
}
