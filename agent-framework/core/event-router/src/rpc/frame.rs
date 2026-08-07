use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};

use futures_core::Stream;
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::io::{write_all, BoxBinaryReader, BoxBinaryWriter};
use super::{RpcError, RpcResult};

const FRAME_HEADER_SIZE: usize = size_of::<u32>();

pub(crate) async fn write_frame<T>(
    writer: &mut BoxBinaryWriter,
    message: &T,
    limit: usize,
) -> RpcResult<()>
where
    T: Serialize + ?Sized,
{
    let payload = postcard::to_allocvec(message).map_err(RpcError::encode)?;
    if payload.len() > limit {
        return Err(RpcError::FrameTooLarge {
            size: payload.len(),
            limit,
        });
    }
    let length = u32::try_from(payload.len()).map_err(|_| RpcError::FrameTooLarge {
        size: payload.len(),
        limit,
    })?;
    write_all(writer.as_mut(), &length.to_le_bytes()).await?;
    write_all(writer.as_mut(), &payload).await?;
    Ok(())
}

pub(crate) struct FramedReader<T> {
    reader: BoxBinaryReader,
    limit: usize,
    header: [u8; FRAME_HEADER_SIZE],
    header_read: usize,
    payload: Option<Vec<u8>>,
    payload_read: usize,
    finished: bool,
    message: PhantomData<fn() -> T>,
}

impl<T> FramedReader<T> {
    pub(crate) fn new(reader: BoxBinaryReader, limit: usize) -> Self {
        Self {
            reader,
            limit,
            header: [0; FRAME_HEADER_SIZE],
            header_read: 0,
            payload: None,
            payload_read: 0,
            finished: false,
            message: PhantomData,
        }
    }

    fn fail(&mut self, error: RpcError) -> Poll<Option<RpcResult<T>>> {
        self.finished = true;
        Poll::Ready(Some(Err(error)))
    }

    fn prepare_payload(&mut self) -> RpcResult<()> {
        let length = u32::from_le_bytes(self.header) as usize;
        if length > self.limit {
            return Err(RpcError::FrameTooLarge {
                size: length,
                limit: self.limit,
            });
        }
        self.payload = Some(vec![0_u8; length]);
        self.payload_read = 0;
        Ok(())
    }

    fn finish_frame(&mut self) -> RpcResult<T>
    where
        T: DeserializeOwned,
    {
        let payload = self.payload.take().ok_or(RpcError::InvalidFrameState)?;
        let message = postcard::from_bytes(&payload).map_err(RpcError::decode)?;
        self.header_read = 0;
        self.payload_read = 0;
        Ok(message)
    }
}

impl<T> Unpin for FramedReader<T> {}

impl<T> Stream for FramedReader<T>
where
    T: DeserializeOwned,
{
    type Item = RpcResult<T>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }

        if this.payload.is_none() {
            if this.header_read < FRAME_HEADER_SIZE {
                let Some(destination) = this.header.get_mut(this.header_read..) else {
                    return this.fail(RpcError::InvalidFrameState);
                };
                match this.reader.as_mut().poll_read(context, destination) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => return this.fail(error.into()),
                    Poll::Ready(Ok(0)) if this.header_read == 0 => {
                        this.finished = true;
                        return Poll::Ready(None);
                    }
                    Poll::Ready(Ok(0)) => return this.fail(RpcError::IncompleteFrame),
                    Poll::Ready(Ok(count)) => {
                        if count > destination.len() {
                            return this.fail(RpcError::InvalidFrameState);
                        }
                        let Some(header_read) = this.header_read.checked_add(count) else {
                            return this.fail(RpcError::InvalidFrameState);
                        };
                        this.header_read = header_read;
                        context.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                }
            }
            if let Err(error) = this.prepare_payload() {
                return this.fail(error);
            }
        }

        let Some(payload) = this.payload.as_mut() else {
            return this.fail(RpcError::InvalidFrameState);
        };
        if this.payload_read < payload.len() {
            let Some(destination) = payload.get_mut(this.payload_read..) else {
                return this.fail(RpcError::InvalidFrameState);
            };
            match this.reader.as_mut().poll_read(context, destination) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return this.fail(error.into()),
                Poll::Ready(Ok(0)) => return this.fail(RpcError::IncompleteFrame),
                Poll::Ready(Ok(count)) => {
                    if count > destination.len() {
                        return this.fail(RpcError::InvalidFrameState);
                    }
                    let Some(payload_read) = this.payload_read.checked_add(count) else {
                        return this.fail(RpcError::InvalidFrameState);
                    };
                    this.payload_read = payload_read;
                    if this.payload_read < payload.len() {
                        context.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                }
            }
        }

        Poll::Ready(Some(this.finish_frame()))
    }
}

pub(crate) async fn next_frame<T>(reader: &mut FramedReader<T>) -> Option<RpcResult<T>>
where
    T: DeserializeOwned,
{
    core::future::poll_fn(|context| Pin::new(&mut *reader).poll_next(context)).await
}

pub(crate) async fn decode_unary<T>(reader: BoxBinaryReader, limit: usize) -> RpcResult<T>
where
    T: DeserializeOwned,
{
    let mut frames = FramedReader::new(reader, limit);
    let message = next_frame(&mut frames)
        .await
        .ok_or(RpcError::MissingUnaryFrame)??;
    match next_frame(&mut frames).await {
        None => Ok(message),
        Some(Ok(_)) => Err(RpcError::ExtraUnaryFrame),
        Some(Err(error)) => Err(error),
    }
}
