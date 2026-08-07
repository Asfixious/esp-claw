//! Demonstrates the low-level raw binary RPC escape hatch.

use core::pin::Pin;
use std::rc::Rc;

use claw_event_router::rpc::{
    binary_pipe, close, flush, read, read_to_end, write_all, BinaryIoError, BoxBinaryReader,
    BoxBinaryWriter, BytesReader, RawRpcProvider, RpcAddress, RpcContext, RpcFuture,
    RpcLaneStorage, RpcRegistry, RpcResult,
};
use futures_util::join;

const BODY_LIMIT: usize = 4_096;

struct Uppercase;

impl RawRpcProvider for Uppercase {
    fn call<'a>(
        &'a self,
        _context: RpcContext,
        mut input: BoxBinaryReader,
        mut output: BoxBinaryWriter,
    ) -> RpcFuture<'a> {
        Box::pin(async move {
            let mut buffer = [0_u8; 8];
            loop {
                let count = read(input.as_mut(), &mut buffer).await?;
                if count == 0 {
                    close(output.as_mut()).await?;
                    return Ok(());
                }
                let available = buffer.len();
                let bytes = buffer
                    .get_mut(..count)
                    .ok_or(BinaryIoError::InvalidReadCount {
                        reported: count,
                        available,
                    })?;
                bytes.make_ascii_uppercase();
                write_all(output.as_mut(), bytes).await?;
            }
        })
    }
}

async fn finite_body_call(registry: &Rc<RpcRegistry>, address: &RpcAddress) -> RpcResult<()> {
    let (mut response_reader, response_writer) = binary_pipe(3)?;
    let call = registry.client().call_raw(
        address,
        BytesReader::new(b"finite body".to_vec()).boxed(),
        Box::pin(response_writer),
    )?;

    // Raw calls expose the provider future and output reader independently, so
    // both must be driven together when bounded backpressure is possible.
    let (call_result, response_result) = join!(
        call,
        read_to_end(Pin::new(&mut response_reader), BODY_LIMIT)
    );
    call_result?;
    let response = response_result?;
    assert_eq!(response, b"FINITE BODY");
    Ok(())
}

async fn streaming_body_call(registry: &Rc<RpcRegistry>, address: &RpcAddress) -> RpcResult<()> {
    let (request_reader, mut request_writer) = binary_pipe(2)?;
    let (mut response_reader, response_writer) = binary_pipe(2)?;
    let call =
        registry
            .client()
            .call_raw(address, Box::pin(request_reader), Box::pin(response_writer))?;

    let producer = async {
        for chunk in [b"bounded ".as_slice(), b"streaming ", b"body"] {
            write_all(Pin::new(&mut request_writer), chunk).await?;
        }
        flush(Pin::new(&mut request_writer)).await?;
        close(Pin::new(&mut request_writer)).await
    };
    let consumer = read_to_end(Pin::new(&mut response_reader), BODY_LIMIT);
    let (producer_result, call_result, response_result) = join!(producer, call, consumer);
    producer_result?;
    call_result?;
    let response = response_result?;
    assert_eq!(response, b"BOUNDED STREAMING BODY");
    Ok(())
}

async fn run() -> RpcResult<()> {
    let lanes = Box::leak(Box::new(RpcLaneStorage::<2, 128, 8>::new()));
    let registry = Rc::new(RpcRegistry::new(lanes)?);
    let address = RpcAddress::parse("raw.uppercase")?;
    registry.register_raw(address.clone(), Uppercase)?;

    finite_body_call(&registry, &address).await?;
    streaming_body_call(&registry, &address).await?;

    println!("raw finite and streaming bodies completed");
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> RpcResult<()> {
    run().await
}
