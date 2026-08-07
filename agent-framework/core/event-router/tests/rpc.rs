#![allow(clippy::expect_used)]
#![allow(missing_docs)]

use core::future::poll_fn;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::rc::Rc;

use claw_event_router::rpc::{
    binary_pipe, close, read, read_to_end, write_all, BinaryIoError, BinaryWriter, BoxBinaryReader,
    BoxBinaryWriter, BytesReader, RawRpcProvider, RpcAddress, RpcContext, RpcError, RpcFuture,
    RpcLaneStorage, RpcRegistry, RpcResult,
};
use futures_lite::future::{block_on, poll_once};
use futures_util::join;

const BODY_LIMIT: usize = 1024;

fn registry() -> Rc<RpcRegistry> {
    let lanes = Box::leak(Box::new(RpcLaneStorage::<4, 128, 8>::new()));
    Rc::new(RpcRegistry::new(lanes).expect("valid lane storage"))
}

struct UnaryUnary;

impl RawRpcProvider for UnaryUnary {
    fn call<'a>(
        &'a self,
        _context: RpcContext,
        mut input: BoxBinaryReader,
        mut output: BoxBinaryWriter,
    ) -> RpcFuture<'a> {
        Box::pin(async move {
            let mut body = read_to_end(input.as_mut(), BODY_LIMIT).await?;
            body.make_ascii_uppercase();
            write_all(output.as_mut(), &body).await?;
            close(output.as_mut()).await?;
            Ok(())
        })
    }
}

struct UnaryStream;

impl RawRpcProvider for UnaryStream {
    fn call<'a>(
        &'a self,
        _context: RpcContext,
        mut input: BoxBinaryReader,
        mut output: BoxBinaryWriter,
    ) -> RpcFuture<'a> {
        Box::pin(async move {
            let request = read_to_end(input.as_mut(), BODY_LIMIT).await?;
            for prefix in [b"first:".as_slice(), b"second:", b"third:"] {
                write_all(output.as_mut(), prefix).await?;
                write_all(output.as_mut(), &request).await?;
                write_all(output.as_mut(), b";").await?;
            }
            close(output.as_mut()).await?;
            Ok(())
        })
    }
}

struct StreamUnary;

impl RawRpcProvider for StreamUnary {
    fn call<'a>(
        &'a self,
        _context: RpcContext,
        mut input: BoxBinaryReader,
        mut output: BoxBinaryWriter,
    ) -> RpcFuture<'a> {
        Box::pin(async move {
            let body = read_to_end(input.as_mut(), BODY_LIMIT).await?;
            let response = body.len().to_string();
            write_all(output.as_mut(), response.as_bytes()).await?;
            close(output.as_mut()).await?;
            Ok(())
        })
    }
}

struct StreamStream;

impl RawRpcProvider for StreamStream {
    fn call<'a>(
        &'a self,
        _context: RpcContext,
        mut input: BoxBinaryReader,
        mut output: BoxBinaryWriter,
    ) -> RpcFuture<'a> {
        Box::pin(async move {
            let mut buffer = [0_u8; 2];
            loop {
                let count = read(input.as_mut(), &mut buffer).await?;
                if count == 0 {
                    close(output.as_mut()).await?;
                    return Ok(());
                }
                let bytes = buffer.get(..count).ok_or(BinaryIoError::InvalidReadCount {
                    reported: count,
                    available: buffer.len(),
                })?;
                write_all(output.as_mut(), bytes).await?;
            }
        })
    }
}

#[test]
fn one_call_supports_unary_input_and_unary_output() {
    let registry = registry();
    let address = RpcAddress::parse("test.unary_unary").expect("valid address");
    registry
        .register_raw(address.clone(), UnaryUnary)
        .expect("register endpoint");
    let (mut response_reader, response_writer) = binary_pipe(2).expect("valid capacity");
    let call = registry
        .client()
        .call_raw(
            &address,
            BytesReader::new(b"hello".to_vec()).boxed(),
            Box::pin(response_writer),
        )
        .expect("start call");

    let (call_result, response) = block_on(async {
        join!(
            call,
            read_to_end(Pin::new(&mut response_reader), BODY_LIMIT)
        )
    });

    assert_eq!(call_result, Ok(()));
    assert_eq!(response, Ok(b"HELLO".to_vec()));
}

#[test]
fn one_call_supports_unary_input_and_stream_output() {
    let registry = registry();
    let address = RpcAddress::parse("test.unary_stream").expect("valid address");
    registry
        .register_raw(address.clone(), UnaryStream)
        .expect("register endpoint");
    let (mut response_reader, response_writer) = binary_pipe(3).expect("valid capacity");
    let call = registry
        .client()
        .call_raw(
            &address,
            BytesReader::new(b"x".to_vec()).boxed(),
            Box::pin(response_writer),
        )
        .expect("start call");

    let (call_result, response) = block_on(async {
        join!(
            call,
            read_to_end(Pin::new(&mut response_reader), BODY_LIMIT)
        )
    });

    assert_eq!(call_result, Ok(()));
    assert_eq!(response, Ok(b"first:x;second:x;third:x;".to_vec()));
}

#[test]
fn one_call_supports_stream_input_and_unary_output() {
    let registry = registry();
    let address = RpcAddress::parse("test.stream_unary").expect("valid address");
    registry
        .register_raw(address.clone(), StreamUnary)
        .expect("register endpoint");
    let (request_reader, mut request_writer) = binary_pipe(2).expect("valid capacity");
    let (mut response_reader, response_writer) = binary_pipe(2).expect("valid capacity");
    let call = registry
        .client()
        .call_raw(
            &address,
            Box::pin(request_reader),
            Box::pin(response_writer),
        )
        .expect("start call");

    let producer = async {
        for chunk in [b"abc".as_slice(), b"def", b"ghi"] {
            write_all(Pin::new(&mut request_writer), chunk).await?;
        }
        close(Pin::new(&mut request_writer)).await
    };
    let (producer_result, call_result, response) = block_on(async {
        join!(
            producer,
            call,
            read_to_end(Pin::new(&mut response_reader), BODY_LIMIT)
        )
    });

    assert_eq!(producer_result, Ok(()));
    assert_eq!(call_result, Ok(()));
    assert_eq!(response, Ok(b"9".to_vec()));
}

#[test]
fn one_call_supports_stream_input_and_stream_output() {
    let registry = registry();
    let address = RpcAddress::parse("test.stream_stream").expect("valid address");
    registry
        .register_raw(address.clone(), StreamStream)
        .expect("register endpoint");
    let (request_reader, mut request_writer) = binary_pipe(2).expect("valid capacity");
    let (mut response_reader, response_writer) = binary_pipe(2).expect("valid capacity");
    let call = registry
        .client()
        .call_raw(
            &address,
            Box::pin(request_reader),
            Box::pin(response_writer),
        )
        .expect("start call");

    let producer = async {
        write_all(Pin::new(&mut request_writer), b"duplex-stream").await?;
        close(Pin::new(&mut request_writer)).await
    };
    let (producer_result, call_result, response) = block_on(async {
        join!(
            producer,
            call,
            read_to_end(Pin::new(&mut response_reader), BODY_LIMIT)
        )
    });

    assert_eq!(producer_result, Ok(()));
    assert_eq!(call_result, Ok(()));
    assert_eq!(response, Ok(b"duplex-stream".to_vec()));
}

struct NullWriter;

impl BinaryWriter for NullWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, BinaryIoError>> {
        Poll::Ready(Ok(buffer.len()))
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Result<(), BinaryIoError>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Result<(), BinaryIoError>> {
        Poll::Ready(Ok(()))
    }
}

struct DirectSelfCaller {
    address: RpcAddress,
}

impl RawRpcProvider for DirectSelfCaller {
    fn call<'a>(
        &'a self,
        context: RpcContext,
        _input: BoxBinaryReader,
        _output: BoxBinaryWriter,
    ) -> RpcFuture<'a> {
        Box::pin(async move {
            let nested = context.client().call_raw(
                &self.address,
                BytesReader::new(Vec::new()).boxed(),
                Box::pin(NullWriter),
            )?;
            nested.await
        })
    }
}

#[test]
fn direct_self_call_is_rejected_by_endpoint_identity() {
    let registry = registry();
    let address = RpcAddress::parse("cycle.direct").expect("valid address");
    registry
        .register_raw(
            address.clone(),
            DirectSelfCaller {
                address: address.clone(),
            },
        )
        .expect("register endpoint");
    let call = registry
        .client()
        .call_raw(
            &address,
            BytesReader::new(Vec::new()).boxed(),
            Box::pin(NullWriter),
        )
        .expect("start call");

    assert_eq!(
        block_on(call),
        Err(RpcError::DirectSelfCall(address.clone()))
    );
}

struct IndirectA {
    own: RpcAddress,
    next: RpcAddress,
}

impl RawRpcProvider for IndirectA {
    fn call<'a>(
        &'a self,
        context: RpcContext,
        _input: BoxBinaryReader,
        _output: BoxBinaryWriter,
    ) -> RpcFuture<'a> {
        Box::pin(async move {
            if context.caller_endpoint_id().is_some() {
                return Ok(());
            }
            let nested = context.client().call_raw(
                &self.next,
                BytesReader::new(self.own.as_str().as_bytes().to_vec()).boxed(),
                Box::pin(NullWriter),
            )?;
            nested.await
        })
    }
}

struct IndirectB {
    next: RpcAddress,
}

impl RawRpcProvider for IndirectB {
    fn call<'a>(
        &'a self,
        context: RpcContext,
        _input: BoxBinaryReader,
        _output: BoxBinaryWriter,
    ) -> RpcFuture<'a> {
        Box::pin(async move {
            let nested = context.client().call_raw(
                &self.next,
                BytesReader::new(Vec::new()).boxed(),
                Box::pin(NullWriter),
            )?;
            nested.await
        })
    }
}

#[test]
fn indirect_a_to_b_to_a_call_is_allowed() {
    let registry = registry();
    let address_a = RpcAddress::parse("cycle.a").expect("valid address");
    let address_b = RpcAddress::parse("cycle.b").expect("valid address");
    registry
        .register_raw(
            address_a.clone(),
            IndirectA {
                own: address_a.clone(),
                next: address_b.clone(),
            },
        )
        .expect("register A");
    registry
        .register_raw(
            address_b,
            IndirectB {
                next: address_a.clone(),
            },
        )
        .expect("register B");
    let call = registry
        .client()
        .call_raw(
            &address_a,
            BytesReader::new(Vec::new()).boxed(),
            Box::pin(NullWriter),
        )
        .expect("start call");

    assert_eq!(block_on(call), Ok(()));
}

struct Noop;

impl RawRpcProvider for Noop {
    fn call<'a>(
        &'a self,
        _context: RpcContext,
        _input: BoxBinaryReader,
        mut output: BoxBinaryWriter,
    ) -> RpcFuture<'a> {
        Box::pin(async move {
            close(output.as_mut()).await?;
            Ok(())
        })
    }
}

#[test]
fn unregister_does_not_cancel_an_in_flight_call() {
    let registry = registry();
    let address = RpcAddress::parse("lifecycle.noop").expect("valid address");
    let registration = registry
        .register_raw(address.clone(), Noop)
        .expect("register endpoint");
    let call = registry
        .client()
        .call_raw(
            &address,
            BytesReader::new(Vec::new()).boxed(),
            Box::pin(NullWriter),
        )
        .expect("start call");

    registry
        .unregister(&registration)
        .expect("unregister endpoint");

    assert_eq!(block_on(call), Ok(()));
    assert!(matches!(
        registry.client().call_raw(
            &address,
            BytesReader::new(Vec::new()).boxed(),
            Box::pin(NullWriter)
        ),
        Err(RpcError::NotFound(missing)) if missing == address
    ));
}

struct NeverCompletes;

impl RawRpcProvider for NeverCompletes {
    fn call<'a>(
        &'a self,
        _context: RpcContext,
        input: BoxBinaryReader,
        output: BoxBinaryWriter,
    ) -> RpcFuture<'a> {
        Box::pin(async move {
            let _owned_streams = (input, output);
            core::future::pending::<RpcResult<()>>().await
        })
    }
}

struct YieldsOnce;

impl RawRpcProvider for YieldsOnce {
    fn call<'a>(
        &'a self,
        _context: RpcContext,
        _input: BoxBinaryReader,
        mut output: BoxBinaryWriter,
    ) -> RpcFuture<'a> {
        Box::pin(async move {
            futures_lite::future::yield_now().await;
            close(output.as_mut()).await?;
            Ok(())
        })
    }
}

#[test]
fn dropping_call_future_cancels_both_stream_directions() {
    let registry = registry();
    let address = RpcAddress::parse("lifecycle.pending").expect("valid address");
    registry
        .register_raw(address.clone(), NeverCompletes)
        .expect("register endpoint");
    let (request_reader, mut request_writer) = binary_pipe(1).expect("valid capacity");
    let (mut response_reader, response_writer) = binary_pipe(1).expect("valid capacity");
    let call = registry
        .client()
        .call_raw(
            &address,
            Box::pin(request_reader),
            Box::pin(response_writer),
        )
        .expect("start call");

    drop(call);

    block_on(async {
        assert_eq!(
            write_all(Pin::new(&mut request_writer), b"x").await,
            Err(BinaryIoError::BrokenPipe)
        );
        let mut byte = [0_u8; 1];
        assert_eq!(read(Pin::new(&mut response_reader), &mut byte).await, Ok(0));
    });
}

#[test]
fn root_calls_wait_at_the_lane_limit_and_waiter_overflow_is_bounded() {
    let lanes = Box::leak(Box::new(RpcLaneStorage::<1, 128, 1>::new()));
    let registry = Rc::new(RpcRegistry::new(lanes).expect("valid lane storage"));
    let pending_address = RpcAddress::parse("lanes.pending").expect("valid address");
    let ready_address = RpcAddress::parse("lanes.ready").expect("valid address");
    registry
        .register_raw(pending_address.clone(), NeverCompletes)
        .expect("register pending endpoint");
    registry
        .register_raw(ready_address.clone(), Noop)
        .expect("register ready endpoint");

    block_on(async {
        let mut active = registry
            .client()
            .call_raw(
                &pending_address,
                BytesReader::new(Vec::new()).boxed(),
                Box::pin(NullWriter),
            )
            .expect("start active call");
        assert!(poll_once(active.as_mut()).await.is_none());

        let mut waiter = registry
            .client()
            .call_raw(
                &ready_address,
                BytesReader::new(Vec::new()).boxed(),
                Box::pin(NullWriter),
            )
            .expect("start waiting call");
        assert!(poll_once(waiter.as_mut()).await.is_none());

        let mut overflow = registry
            .client()
            .call_raw(
                &ready_address,
                BytesReader::new(Vec::new()).boxed(),
                Box::pin(NullWriter),
            )
            .expect("construct overflow call");
        assert_eq!(
            poll_once(overflow.as_mut()).await,
            Some(Err(RpcError::LaneWaiterCapacityExceeded { limit: 1 }))
        );

        drop(active);
        assert_eq!(waiter.await, Ok(()));

        let recovered = registry
            .client()
            .call_raw(
                &ready_address,
                BytesReader::new(Vec::new()).boxed(),
                Box::pin(NullWriter),
            )
            .expect("start recovered call");
        assert_eq!(recovered.await, Ok(()));
    });
}

#[test]
fn many_root_calls_reuse_a_smaller_fixed_lane_pool() {
    const CALL_COUNT: usize = 32;

    let lanes = Box::leak(Box::new(RpcLaneStorage::<4, 128, 28>::new()));
    let registry = Rc::new(RpcRegistry::new(lanes).expect("valid lane storage"));
    let address = RpcAddress::parse("lanes.stress").expect("valid address");
    registry
        .register_raw(address.clone(), YieldsOnce)
        .expect("register yielding endpoint");

    let client = registry.client();
    let mut calls = (0..CALL_COUNT)
        .map(|_| {
            client
                .call_raw(
                    &address,
                    BytesReader::new(Vec::new()).boxed(),
                    Box::pin(NullWriter),
                )
                .expect("start stress call")
        })
        .map(Some)
        .collect::<Vec<_>>();
    let result = block_on(poll_fn(|context| {
        let mut has_pending = false;
        for slot in &mut calls {
            let Some(call) = slot.as_mut() else {
                continue;
            };
            match call.as_mut().poll(context) {
                Poll::Ready(Ok(())) => *slot = None,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => has_pending = true,
            }
        }
        if has_pending {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }));

    assert_eq!(result, Ok(()));
    assert!(calls.into_iter().all(|call| call.is_none()));
}

#[test]
fn cancelling_a_reserved_waiter_hands_the_lane_to_the_next_waiter() {
    let lanes = Box::leak(Box::new(RpcLaneStorage::<1, 128, 2>::new()));
    let registry = Rc::new(RpcRegistry::new(lanes).expect("valid lane storage"));
    let pending_address = RpcAddress::parse("lanes.reserved_pending").expect("valid address");
    let ready_address = RpcAddress::parse("lanes.reserved_ready").expect("valid address");
    registry
        .register_raw(pending_address.clone(), NeverCompletes)
        .expect("register pending endpoint");
    registry
        .register_raw(ready_address.clone(), Noop)
        .expect("register ready endpoint");

    block_on(async {
        let mut active = registry
            .client()
            .call_raw(
                &pending_address,
                BytesReader::new(Vec::new()).boxed(),
                Box::pin(NullWriter),
            )
            .expect("start active call");
        assert!(poll_once(active.as_mut()).await.is_none());

        let mut first_waiter = registry
            .client()
            .call_raw(
                &ready_address,
                BytesReader::new(Vec::new()).boxed(),
                Box::pin(NullWriter),
            )
            .expect("start first waiter");
        let mut second_waiter = registry
            .client()
            .call_raw(
                &ready_address,
                BytesReader::new(Vec::new()).boxed(),
                Box::pin(NullWriter),
            )
            .expect("start second waiter");
        assert!(poll_once(first_waiter.as_mut()).await.is_none());
        assert!(poll_once(second_waiter.as_mut()).await.is_none());

        drop(active);
        drop(first_waiter);
        assert_eq!(second_waiter.await, Ok(()));
    });
}

struct CallsOther {
    next: RpcAddress,
}

impl RawRpcProvider for CallsOther {
    fn call<'a>(
        &'a self,
        context: RpcContext,
        _input: BoxBinaryReader,
        _output: BoxBinaryWriter,
    ) -> RpcFuture<'a> {
        Box::pin(async move {
            context
                .client()
                .call_raw(
                    &self.next,
                    BytesReader::new(Vec::new()).boxed(),
                    Box::pin(NullWriter),
                )?
                .await
        })
    }
}

#[test]
fn nested_call_fails_instead_of_deadlocking_when_all_lanes_are_held() {
    let lanes = Box::leak(Box::new(RpcLaneStorage::<1, 128, 1>::new()));
    let registry = Rc::new(RpcRegistry::new(lanes).expect("valid lane storage"));
    let outer_address = RpcAddress::parse("lanes.outer").expect("valid address");
    let inner_address = RpcAddress::parse("lanes.inner").expect("valid address");
    registry
        .register_raw(inner_address.clone(), Noop)
        .expect("register inner endpoint");
    registry
        .register_raw(
            outer_address.clone(),
            CallsOther {
                next: inner_address,
            },
        )
        .expect("register outer endpoint");

    let call = registry
        .client()
        .call_raw(
            &outer_address,
            BytesReader::new(Vec::new()).boxed(),
            Box::pin(NullWriter),
        )
        .expect("start outer call");
    assert_eq!(
        block_on(call),
        Err(RpcError::NestedLaneExhausted { limit: 1 })
    );
}

#[test]
fn zero_lane_dimensions_are_rejected() {
    let zero_lanes = Box::leak(Box::new(RpcLaneStorage::<0, 1, 1>::new()));
    assert!(matches!(
        RpcRegistry::new(zero_lanes),
        Err(RpcError::InvalidLaneConfiguration { field: "N" })
    ));

    let zero_bytes = Box::leak(Box::new(RpcLaneStorage::<1, 0, 1>::new()));
    assert!(matches!(
        RpcRegistry::new(zero_bytes),
        Err(RpcError::InvalidLaneConfiguration { field: "M" })
    ));
}
