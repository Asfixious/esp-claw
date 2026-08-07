//! Demonstrates all four typed RPC request/response cardinality combinations.

use std::rc::Rc;

use claw_event_router::rpc::{
    RpcContext, RpcHandlerFuture, RpcHandlerInput, RpcHandlerOutput, RpcMethod, RpcRegistry,
    RpcResult, RpcStream, Streaming, TypedRpcHandler, Unary,
};
use futures_util::stream;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct Number {
    value: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct Total {
    value: u32,
}

struct UnaryUnary;

impl RpcMethod for UnaryUnary {
    const ADDRESS: &'static str = "example.unary_unary";

    type Request = Number;
    type Response = Total;
    type Input = Unary;
    type Output = Unary;
}

struct AddOffset {
    offset: u32,
}

// Implementing the handler trait directly allows its future to borrow handler
// state. Stateless handlers can use the closure shorthand shown below.
impl TypedRpcHandler<UnaryUnary> for AddOffset {
    fn call<'a>(
        &'a self,
        _context: RpcContext,
        request: RpcHandlerInput<UnaryUnary>,
    ) -> RpcHandlerFuture<'a, RpcHandlerOutput<UnaryUnary>> {
        Box::pin(async move {
            Ok(Total {
                value: request.value.saturating_add(self.offset),
            })
        })
    }
}

struct UnaryStream;

impl RpcMethod for UnaryStream {
    const ADDRESS: &'static str = "example.unary_stream";

    type Request = Number;
    type Response = Total;
    type Input = Unary;
    type Output = Streaming;
}

struct StreamUnary;

impl RpcMethod for StreamUnary {
    const ADDRESS: &'static str = "example.stream_unary";

    type Request = Number;
    type Response = Total;
    type Input = Streaming;
    type Output = Unary;
}

struct StreamStream;

impl RpcMethod for StreamStream {
    const ADDRESS: &'static str = "example.stream_stream";

    type Request = Number;
    type Response = Total;
    type Input = Streaming;
    type Output = Streaming;
}

fn request_stream<I>(values: I) -> RpcStream<Number>
where
    I: IntoIterator<Item = u32>,
    I::IntoIter: 'static,
{
    RpcStream::new(stream::iter(
        values.into_iter().map(|value| Ok(Number { value })),
    ))
}

async fn collect(mut stream: RpcStream<Total>) -> RpcResult<Vec<Total>> {
    let mut values = Vec::new();
    while let Some(value) = stream.next().await {
        values.push(value?);
    }
    Ok(values)
}

async fn run() -> RpcResult<()> {
    let registry = Rc::new(RpcRegistry::new());

    registry.register_typed::<UnaryUnary, _>(AddOffset { offset: 1 })?;

    registry.register_typed::<UnaryStream, _>(|_context, request: Number| async move {
        Ok(RpcStream::new(stream::iter([
            Ok(Total {
                value: request.value,
            }),
            Ok(Total {
                value: request.value.saturating_add(1),
            }),
        ])))
    })?;

    registry.register_typed::<StreamUnary, _>(
        |_context, mut requests: RpcStream<Number>| async move {
            let mut total = 0_u32;
            while let Some(request) = requests.next().await {
                total = total.saturating_add(request?.value);
            }
            Ok(Total { value: total })
        },
    )?;

    registry.register_typed::<StreamStream, _>(
        |_context, requests: RpcStream<Number>| async move {
            let responses = stream::unfold(requests, |mut requests| async move {
                match requests.next().await {
                    Some(Ok(request)) => Some((
                        Ok(Total {
                            value: request.value.saturating_mul(2),
                        }),
                        requests,
                    )),
                    Some(Err(error)) => Some((Err(error), requests)),
                    None => None,
                }
            });
            Ok(RpcStream::new(responses))
        },
    )?;

    let client = registry.client();

    // Unary -> Unary returns a self-driving future.
    let unary = client.call::<UnaryUnary>(Number { value: 41 })?.await?;
    assert_eq!(unary, Total { value: 42 });

    // Unary -> Stream returns a self-driving RpcStream.
    let unary_stream = collect(client.call::<UnaryStream>(Number { value: 7 })?).await?;
    assert_eq!(unary_stream, vec![Total { value: 7 }, Total { value: 8 }]);

    // Stream -> Unary accepts RpcStream<Request>.
    let stream_unary = client
        .call::<StreamUnary>(request_stream([2, 3, 4]))?
        .await?;
    assert_eq!(stream_unary, Total { value: 9 });

    // Stream -> Stream can transform requests incrementally without buffering
    // the complete request or response body.
    let stream_stream = collect(client.call::<StreamStream>(request_stream([3, 5, 8]))?).await?;
    assert_eq!(
        stream_stream,
        vec![Total { value: 6 }, Total { value: 10 }, Total { value: 16 },]
    );

    println!("all four typed RPC shapes completed");
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> RpcResult<()> {
    run().await
}
