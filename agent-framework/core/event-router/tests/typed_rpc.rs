#![allow(clippy::expect_used)]
#![allow(missing_docs)]

use std::rc::Rc;

use claw_event_router::rpc::{
    RpcDirection, RpcError, RpcFailure, RpcFrame, RpcLaneStorage, RpcMethod, RpcRegistry,
    RpcResult, RpcStream, Streaming, Unary,
};
use futures_lite::future::{block_on, poll_once};
use futures_util::stream;
use zerocopy::{Immutable, IntoBytes, KnownLayout, TryFromBytes};

#[repr(C)]
#[derive(Clone, Copy, Debug, Immutable, IntoBytes, KnownLayout, PartialEq, Eq, TryFromBytes)]
struct Number {
    value: u32,
    label: [u8; 40],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Immutable, IntoBytes, KnownLayout, PartialEq, Eq, TryFromBytes)]
struct Total {
    value: u32,
}

#[repr(C, align(32))]
#[derive(Clone, Copy, Debug, Immutable, IntoBytes, KnownLayout, PartialEq, Eq, TryFromBytes)]
struct OverAligned([u8; 32]);

fn registry() -> Rc<RpcRegistry> {
    let lanes = Box::leak(Box::new(RpcLaneStorage::<4, 4_096, 8>::new()));
    Rc::new(RpcRegistry::new(lanes).expect("valid lane storage"))
}

struct UnaryUnaryMethod;

impl RpcMethod for UnaryUnaryMethod {
    const ADDRESS: &'static str = "typed.unary_unary";
    type Request = Number;
    type Response = Total;
    type Input = Unary;
    type Output = Unary;
}

struct UnaryStreamMethod;

impl RpcMethod for UnaryStreamMethod {
    const ADDRESS: &'static str = "typed.unary_stream";
    type Request = Number;
    type Response = Total;
    type Input = Unary;
    type Output = Streaming;
}

struct StreamUnaryMethod;

impl RpcMethod for StreamUnaryMethod {
    const ADDRESS: &'static str = "typed.stream_unary";
    type Request = Number;
    type Response = Total;
    type Input = Streaming;
    type Output = Unary;
}

struct StreamStreamMethod;

impl RpcMethod for StreamStreamMethod {
    const ADDRESS: &'static str = "typed.stream_stream";
    type Request = Number;
    type Response = Total;
    type Input = Streaming;
    type Output = Streaming;
}

fn number(value: u32) -> Number {
    Number {
        value,
        label: [b'x'; 40],
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

async fn collect(mut values: RpcStream<RpcFrame<Total>>) -> RpcResult<Vec<Total>> {
    let mut output = Vec::new();
    while let Some(value) = values.next().await {
        output.push(*value?.view()?);
    }
    Ok(output)
}

#[test]
fn typed_unary_input_and_unary_output_are_self_driving() {
    let registry = registry();
    registry
        .register_typed::<UnaryUnaryMethod, _>(|_context, request: RpcFrame<Number>| async move {
            Ok(Total {
                value: request.view()?.value + 1,
            })
        })
        .expect("register typed endpoint");

    let call = registry
        .client()
        .call::<UnaryUnaryMethod>(number(41))
        .expect("start typed call");

    let response = block_on(call).expect("complete unary call");
    assert_eq!(response.view(), Ok(&Total { value: 42 }));
}

#[test]
fn typed_unary_input_and_stream_output_are_self_driving() {
    let registry = registry();
    registry
        .register_typed::<UnaryStreamMethod, _>(|_context, request: RpcFrame<Number>| async move {
            let value = request.view()?.value;
            Ok(RpcStream::new(stream::iter(vec![
                Ok(Total { value }),
                Ok(Total { value: value + 1 }),
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
    let registry = registry();
    registry
        .register_typed::<StreamUnaryMethod, _>(
            |_context, mut requests: RpcStream<RpcFrame<Number>>| async move {
                let mut total = 0_u32;
                while let Some(request) = requests.next().await {
                    total = total.checked_add(request?.view()?.value).ok_or_else(|| {
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

    let response = block_on(call).expect("complete stream-unary call");
    assert_eq!(response.view(), Ok(&Total { value: 15 }));
}

#[test]
fn typed_stream_input_and_stream_output_are_self_driving() {
    let registry = registry();
    registry
        .register_typed::<StreamStreamMethod, _>(
            |_context, requests: RpcStream<RpcFrame<Number>>| async move {
                let responses = stream::unfold(requests, |mut requests| async move {
                    match requests.next().await {
                        Some(Ok(request)) => match request.view() {
                            Ok(request) => Some((
                                Ok(Total {
                                    value: request.value + 10,
                                }),
                                requests,
                            )),
                            Err(error) => Some((Err(error), requests)),
                        },
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
    let registry = registry();
    registry
        .register_typed::<UnaryUnaryMethod, _>(|_context, request: RpcFrame<Number>| async move {
            Ok(Total {
                value: request.view()?.value,
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
    type Request = Number;
    type Response = Total;
    type Input = Unary;
    type Output = Unary;
}

#[test]
fn typed_request_frame_limit_is_enforced() {
    let registry = registry();
    registry
        .register_typed::<TinyFrameMethod, _>(|_context, request: RpcFrame<Number>| async move {
            Ok(Total {
                value: request.view()?.value,
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

struct EmptyFrameMethod;

impl RpcMethod for EmptyFrameMethod {
    const ADDRESS: &'static str = "typed.empty_frame";

    type Request = ();
    type Response = ();
    type Input = Unary;
    type Output = Unary;
}

#[test]
fn zero_byte_zerocopy_frames_are_preserved_by_static_lanes() {
    let registry = registry();
    registry
        .register_typed::<EmptyFrameMethod, _>(|_context, request: RpcFrame<()>| async move {
            request.view()?;
            Ok(())
        })
        .expect("register empty-frame endpoint");

    let call = registry
        .client()
        .call::<EmptyFrameMethod>(())
        .expect("start empty-frame call");
    let response = block_on(call).expect("complete empty-frame call");
    assert_eq!(response.view(), Ok(&()));
}

#[test]
fn typed_registration_requires_method_frames_to_fit_the_lane_capacity() {
    let lanes = Box::leak(Box::new(RpcLaneStorage::<1, 8, 1>::new()));
    let registry = Rc::new(RpcRegistry::new(lanes).expect("valid lane storage"));
    let result = registry.register_typed::<UnaryUnaryMethod, _>(
        |_context, request: RpcFrame<Number>| async move {
            Ok(Total {
                value: request.view()?.value,
            })
        },
    );

    assert!(matches!(
        result,
        Err(RpcError::MethodFrameExceedsLane {
            direction: RpcDirection::Request,
            frame_limit: 4_096,
            lane_capacity: 8,
            ..
        })
    ));
}

struct LeaseMethod;

impl RpcMethod for LeaseMethod {
    const ADDRESS: &'static str = "typed.frame_lease";
    const MAX_REQUEST_FRAME: usize = 64;
    const MAX_RESPONSE_FRAME: usize = 64;

    type Request = Number;
    type Response = Number;
    type Input = Unary;
    type Output = Unary;
}

#[test]
fn response_frame_retains_an_aligned_lane_until_drop() {
    block_on(async {
        let lanes = Box::leak(Box::new(RpcLaneStorage::<1, 64, 2>::new()));
        let registry = Rc::new(RpcRegistry::new(lanes).expect("valid lane storage"));
        registry
            .register_typed::<LeaseMethod, _>(|_context, request: RpcFrame<Number>| async move {
                Ok(*request.view()?)
            })
            .expect("register lease endpoint");

        let client = registry.client();
        let first = client
            .call::<LeaseMethod>(number(1))
            .expect("start first call")
            .await
            .expect("finish first call");
        let address = first.as_bytes().expect("borrow response bytes").as_ptr() as usize;
        assert_eq!(address % align_of::<Number>(), 0);

        let mut second = Box::pin(
            client
                .call::<LeaseMethod>(number(2))
                .expect("start second call"),
        );
        assert!(poll_once(second.as_mut()).await.is_none());

        drop(first);
        let second = second.await.expect("finish second call after frame drop");
        assert_eq!(second.view(), Ok(&number(2)));
    });
}

struct OverAlignedMethod;

impl RpcMethod for OverAlignedMethod {
    const ADDRESS: &'static str = "typed.over_aligned";

    type Request = OverAligned;
    type Response = OverAligned;
    type Input = Unary;
    type Output = Unary;
}

#[test]
fn message_alignment_larger_than_lane_alignment_is_rejected() {
    let registry = registry();
    let result = registry.register_typed::<OverAlignedMethod, _>(
        |_context, request: RpcFrame<OverAligned>| async move { Ok(*request.view()?) },
    );

    assert!(matches!(
        result,
        Err(RpcError::MessageAlignmentExceedsLane {
            required: 32,
            available: 16,
            ..
        })
    ));
}
