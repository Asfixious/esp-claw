use core::any::type_name;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};

use futures_core::Stream;
use zerocopy::{Immutable, IntoBytes};

use super::io::{write_all, BoxBinaryReader, BoxBinaryWriter};
use super::lane::BorrowedFrame;
use super::{RpcError, RpcMessage, RpcResult};

const FRAME_HEADER_SIZE: usize = size_of::<u32>();

/// A complete frame buffer returned by a binary transport.
///
/// This type is public only because it appears in the hidden transport hook on
/// [`BinaryReader`](super::BinaryReader). Application code normally sees
/// [`RpcFrame`] instead.
#[doc(hidden)]
pub struct RpcFrameBuffer {
    frame: BorrowedFrame,
}

impl RpcFrameBuffer {
    pub(crate) fn borrowed(frame: BorrowedFrame) -> Self {
        Self { frame }
    }

    fn as_bytes(&self) -> RpcResult<&[u8]> {
        self.frame.as_bytes()
    }
}

enum RpcFrameStorage<T> {
    Borrowed(RpcFrameBuffer),
    Owned(T),
}

/// A typed, zero-copy view over one RPC frame.
///
/// For fixed RPC lanes this value retains the lane buffer. The frame is not
/// consumed and the lane is not reused until this value is dropped. Use
/// [`view`](Self::view) to borrow the validated message in place.
pub struct RpcFrame<T> {
    storage: RpcFrameStorage<T>,
}

impl<T> core::fmt::Debug for RpcFrame<T>
where
    T: RpcMessage + core::fmt::Debug,
{
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.view() {
            Ok(message) => formatter.debug_tuple("RpcFrame").field(message).finish(),
            Err(error) => formatter
                .debug_struct("RpcFrame")
                .field("error", &error)
                .finish(),
        }
    }
}

impl<T> RpcFrame<T>
where
    T: RpcMessage,
{
    fn from_buffer(buffer: RpcFrameBuffer) -> RpcResult<Self> {
        T::try_ref_from_bytes(buffer.as_bytes()?).map_err(|_| RpcError::InvalidMessageFrame {
            message_type: type_name::<T>(),
        })?;
        Ok(Self {
            storage: RpcFrameStorage::Borrowed(buffer),
        })
    }

    fn from_owned_bytes(bytes: &[u8]) -> RpcResult<Self> {
        let message = T::try_read_from_bytes(bytes).map_err(|_| RpcError::InvalidMessageFrame {
            message_type: type_name::<T>(),
        })?;
        Ok(Self {
            storage: RpcFrameStorage::Owned(message),
        })
    }

    /// Borrows the message directly from its backing frame.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError::InvalidMessageFrame`] if the bytes no longer satisfy
    /// the message's size, alignment, or bit-validity requirements.
    pub fn view(&self) -> RpcResult<&T> {
        match &self.storage {
            RpcFrameStorage::Borrowed(buffer) => {
                T::try_ref_from_bytes(buffer.as_bytes()?).map_err(|_| {
                    RpcError::InvalidMessageFrame {
                        message_type: type_name::<T>(),
                    }
                })
            }
            RpcFrameStorage::Owned(message) => Ok(message),
        }
    }

    /// Returns the exact wire bytes retained by this frame.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError::InvalidFrameState`] if internal frame ownership was
    /// violated.
    pub fn as_bytes(&self) -> RpcResult<&[u8]> {
        match &self.storage {
            RpcFrameStorage::Borrowed(buffer) => buffer.as_bytes(),
            RpcFrameStorage::Owned(message) => Ok(message.as_bytes()),
        }
    }

    pub(crate) fn is_borrowed(&self) -> bool {
        matches!(self.storage, RpcFrameStorage::Borrowed(_))
    }
}

pub(crate) async fn write_frame<T>(
    writer: &mut BoxBinaryWriter,
    message: &T,
    limit: usize,
) -> RpcResult<()>
where
    T: IntoBytes + Immutable + ?Sized,
{
    let payload = message.as_bytes();
    if payload.len() > limit {
        return Err(RpcError::FrameTooLarge {
            size: payload.len(),
            limit,
        });
    }

    let mut encode = |buffer: &mut [u8]| {
        let Some(destination) = buffer.get_mut(..payload.len()) else {
            return Err(RpcError::FrameBufferFull {
                limit: buffer.len(),
            });
        };
        destination.copy_from_slice(payload);
        Ok(payload.len())
    };
    let direct = core::future::poll_fn(|context| {
        writer
            .as_mut()
            .poll_encode_frame(context, limit, &mut encode)
    })
    .await?;
    if direct {
        return Ok(());
    }

    let length = u32::try_from(payload.len()).map_err(|_| RpcError::FrameTooLarge {
        size: payload.len(),
        limit,
    })?;
    write_all(writer.as_mut(), &length.to_le_bytes()).await?;
    write_all(writer.as_mut(), payload).await?;
    Ok(())
}

pub(crate) struct FramedReader<T> {
    reader: BoxBinaryReader,
    limit: usize,
    header: [u8; FRAME_HEADER_SIZE],
    header_read: usize,
    payload: Option<Vec<u8>>,
    payload_read: usize,
    direct: bool,
    finished: bool,
    message: PhantomData<fn() -> T>,
}

impl<T> FramedReader<T> {
    pub(crate) fn new(reader: BoxBinaryReader, limit: usize) -> Self {
        let direct = reader.as_ref().get_ref().supports_borrowed_frames();
        Self {
            reader,
            limit,
            header: [0; FRAME_HEADER_SIZE],
            header_read: 0,
            payload: None,
            payload_read: 0,
            direct,
            finished: false,
            message: PhantomData,
        }
    }

    fn fail(&mut self, error: RpcError) -> Poll<Option<RpcResult<RpcFrame<T>>>> {
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

    fn finish_frame(&mut self) -> RpcResult<RpcFrame<T>>
    where
        T: RpcMessage,
    {
        let payload = self.payload.take().ok_or(RpcError::InvalidFrameState)?;
        let message = RpcFrame::from_owned_bytes(&payload)?;
        self.header_read = 0;
        self.payload_read = 0;
        Ok(message)
    }
}

impl<T> Unpin for FramedReader<T> {}

impl<T> Stream for FramedReader<T>
where
    T: RpcMessage,
{
    type Item = RpcResult<RpcFrame<T>>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }

        if this.direct {
            return match this.reader.as_mut().poll_borrow_frame(context) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(error)) => this.fail(error),
                Poll::Ready(Ok(Some(buffer))) => Poll::Ready(Some(RpcFrame::from_buffer(buffer))),
                Poll::Ready(Ok(None)) => {
                    this.finished = true;
                    Poll::Ready(None)
                }
            };
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

pub(crate) async fn next_frame<T>(reader: &mut FramedReader<T>) -> Option<RpcResult<RpcFrame<T>>>
where
    T: RpcMessage,
{
    core::future::poll_fn(|context| Pin::new(&mut *reader).poll_next(context)).await
}

pub(crate) async fn decode_unary<T>(reader: BoxBinaryReader, limit: usize) -> RpcResult<RpcFrame<T>>
where
    T: RpcMessage,
{
    let mut frames = FramedReader::new(reader, limit);
    let message = next_frame(&mut frames)
        .await
        .ok_or(RpcError::MissingUnaryFrame)??;
    if message.is_borrowed() {
        return Ok(message);
    }
    match next_frame(&mut frames).await {
        None => Ok(message),
        Some(Ok(_)) => Err(RpcError::ExtraUnaryFrame),
        Some(Err(error)) => Err(error),
    }
}
