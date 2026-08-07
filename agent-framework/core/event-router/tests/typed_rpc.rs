#![allow(clippy::expect_used)]
#![allow(missing_docs)]

use std::rc::Rc;

use claw_event_router::rpc::{
    RpcError, RpcFailure, RpcMethod, RpcRegistry, RpcResult, RpcStream, Streaming, Unary,
};
use futures_lite::future::block_on;
use futures_util::stream;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct Number {
    value: u32,
    label: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct Total {
    value: u32,
}

struct UnaryUnaryMethod;

impl RpcMethod for UnaryUnaryMethod {
    const ADDRESS: &'static str = "typed.unary_unary";
    const PIPE_CAPACITY: usize = 2;

    type Request = Number;
    type Response = Total;
    type Input = Unary;
    type Output = Unary;
}

struct UnaryStreamMethod;

impl RpcMethod for UnaryStreamMethod {
    const ADDRESS: &'static str = "typed.unary_stream";
    const PIPE_CAPACITY: usize = 2;

    type Request = Number;
    type Response = Total;
    type Input = Unary;
    type Output = Streaming;
}

struct StreamUnaryMethod;

impl RpcMethod for StreamUnaryMethod {
    const ADDRESS: &'static str = "typed.stream_unary";
    const PIPE_CAPACITY: usize = 2;

    type Request = Number;
    type Response = Total;
    type Input = Streaming;
    type Output = Unary;
}

struct StreamStreamMethod;

impl RpcMethod for StreamStreamMethod {
    const ADDRESS: &'static str = "typed.stream_stream";
    const PIPE_CAPACITY: usize = 2;

    type Request = Number;
    type Response = Total;
    type Input = Streaming;
    type Output = Streaming;
}

fn number(value: u32) -> Number {
    Number {
        value,
        label: "a frame larger than the two-byte pipe".to_owned(),
    }
}

fn input_stream<I>(values: I) -> RpcStream<Number>
where
    I: IntoIterator<Item = u32>,
    I::IntoIter: 'static,
{
    RpcStream::new(stream::iter(
        values.into_iter().map(|value| Ok(number(value))),
    ))
}

async fn collect(mut values: RpcStream<Total>) -> RpcResult<Vec<Total>> {
    let mut output = Vec::new();
    while let Some(value) = values.next().await {
        output.push(value?);
    }
    Ok(output)
}

#[test]
fn typed_unary_input_and_unary_output_are_self_driving() {
    let registry = Rc::new(RpcRegistry::new());
    registry
        .register_typed::<UnaryUnaryMethod, _>(|_context, request: Number| async move {
            Ok(Total {
                value: request.value + 1,
            })
        })
        .expect("register typed endpoint");

    let call = registry
        .client()
        .call::<UnaryUnaryMethod>(number(41))
        .expect("start typed call");

    assert_eq!(block_on(call), Ok(Total { value: 42 }));
}

#[test]
fn typed_unary_input_and_stream_output_are_self_driving() {
    let registry = Rc::new(RpcRegistry::new());
    registry
        .register_typed::<UnaryStreamMethod, _>(|_context, request: Number| async move {
            Ok(RpcStream::new(stream::iter(vec![
                Ok(Total {
                    value: request.value,
                }),
                Ok(Total {
                    value: request.value + 1,
                }),
            ])))
        })
        .expect("register typed endpoint");

    let responses = registry
        .client()
        .call::<UnaryStreamMethod>(number(6))
        .expect("start typed call");

    assert_eq!(
        block_on(collect(responses)),
        Ok(vec![Total { value: 6 }, Total { value: 7 }])
    );
}

#[test]
fn typed_stream_input_and_unary_output_are_self_driving() {
    let registry = Rc::new(RpcRegistry::new());
    registry
        .register_typed::<StreamUnaryMethod, _>(
            |_context, mut requests: RpcStream<Number>| async move {
                let mut total = 0_u32;
                while let Some(request) = requests.next().await {
                    total = total.checked_add(request?.value).ok_or_else(|| {
                        RpcError::Provider(RpcFailure::new("overflow", "sum overflowed"))
                    })?;
                }
                Ok(Total { value: total })
            },
        )
        .expect("register typed endpoint");

    let call = registry
        .client()
        .call::<StreamUnaryMethod>(input_stream([4, 5, 6]))
        .expect("start typed call");

    assert_eq!(block_on(call), Ok(Total { value: 15 }));
}

#[test]
fn typed_stream_input_and_stream_output_are_self_driving() {
    let registry = Rc::new(RpcRegistry::new());
    registry
        .register_typed::<StreamStreamMethod, _>(
            |_context, requests: RpcStream<Number>| async move {
                let responses = stream::unfold(requests, |mut requests| async move {
                    match requests.next().await {
                        Some(Ok(request)) => Some((
                            Ok(Total {
                                value: request.value + 10,
                            }),
                            requests,
                        )),
                        Some(Err(error)) => Some((Err(error), requests)),
                        None => None,
                    }
                });
                Ok(RpcStream::new(responses))
            },
        )
        .expect("register typed endpoint");

    let responses = registry
        .client()
        .call::<StreamStreamMethod>(input_stream([1, 2, 3]))
        .expect("start typed call");

    assert_eq!(
        block_on(collect(responses)),
        Ok(vec![
            Total { value: 11 },
            Total { value: 12 },
            Total { value: 13 },
        ])
    );
}

struct IncompatibleMethod;

impl RpcMethod for IncompatibleMethod {
    const ADDRESS: &'static str = UnaryUnaryMethod::ADDRESS;

    type Request = Number;
    type Response = Total;
    type Input = Unary;
    type Output = Streaming;
}

#[test]
fn typed_signature_mismatch_is_rejected_before_starting_io() {
    let registry = Rc::new(RpcRegistry::new());
    registry
        .register_typed::<UnaryUnaryMethod, _>(|_context, request: Number| async move {
            Ok(Total {
                value: request.value,
            })
        })
        .expect("register typed endpoint");

    let result = registry.client().call::<IncompatibleMethod>(number(1));

    assert!(matches!(
        result,
        Err(RpcError::SignatureMismatch { address, .. })
            if address.as_str() == UnaryUnaryMethod::ADDRESS
    ));
}

struct TinyFrameMethod;

impl RpcMethod for TinyFrameMethod {
    const ADDRESS: &'static str = "typed.tiny_frame";
    const MAX_REQUEST_FRAME: usize = 1;
    const PIPE_CAPACITY: usize = 1;

    type Request = Number;
    type Response = Total;
    type Input = Unary;
    type Output = Unary;
}

#[test]
fn typed_request_frame_limit_is_enforced() {
    let registry = Rc::new(RpcRegistry::new());
    registry
        .register_typed::<TinyFrameMethod, _>(|_context, request: Number| async move {
            Ok(Total {
                value: request.value,
            })
        })
        .expect("register typed endpoint");
    let call = registry
        .client()
        .call::<TinyFrameMethod>(number(1))
        .expect("start typed call");

    assert!(matches!(
        block_on(call),
        Err(RpcError::FrameTooLarge { limit: 1, .. })
    ));
}
