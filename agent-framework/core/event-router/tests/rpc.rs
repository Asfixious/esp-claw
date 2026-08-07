#![allow(clippy::expect_used)]
#![allow(missing_docs)]

use core::pin::Pin;
use core::task::{Context, Poll};
use std::rc::Rc;

use claw_event_router::rpc::{
    binary_pipe, close, read, read_to_end, write_all, BinaryIoError, BinaryWriter, BoxBinaryReader,
    BoxBinaryWriter, BytesReader, RawRpcProvider, RpcAddress, RpcContext, RpcError, RpcFuture,
    RpcRegistry, RpcResult,
};
use futures_lite::future::block_on;
use futures_util::join;

const BODY_LIMIT: usize = 1024;

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
    let registry = Rc::new(RpcRegistry::new());
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
    let registry = Rc::new(RpcRegistry::new());
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
    let registry = Rc::new(RpcRegistry::new());
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
    let registry = Rc::new(RpcRegistry::new());
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
    let registry = Rc::new(RpcRegistry::new());
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
    let registry = Rc::new(RpcRegistry::new());
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
    let registry = Rc::new(RpcRegistry::new());
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

#[test]
fn dropping_call_future_cancels_both_stream_directions() {
    let registry = Rc::new(RpcRegistry::new());
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
