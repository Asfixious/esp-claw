use core::any::type_name;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};

use super::lane::{BorrowedFrame, LaneFrameKind, LaneReader, LaneWriter};
use super::{RpcError, RpcMessage, RpcResult};
use futures_core::Stream;

type FrameOutcome<T, E> = Result<RpcFrame<T>, RpcFrame<E>>;
type FrameOutcomePoll<T, E> = Poll<Option<RpcResult<FrameOutcome<T, E>>>>;

/// A typed, zero-copy view over one RPC frame.
///
/// This value retains the lane buffer. The frame is not consumed and the lane
/// is not reused until this value is dropped. Use [`view`](Self::view) to
/// borrow the validated message in place.
pub struct RpcFrame<T> {
    frame: BorrowedFrame,
    message: PhantomData<fn() -> T>,
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
    pub(crate) fn from_frame(frame: BorrowedFrame) -> RpcResult<Self> {
        T::try_ref_from_bytes(frame.as_bytes()?).map_err(|_| RpcError::InvalidMessageFrame {
            message_type: type_name::<T>(),
        })?;
        Ok(Self {
            frame,
            message: PhantomData,
        })
    }

    /// Borrows the message directly from its backing lane frame.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError::InvalidMessageFrame`] if the bytes no longer satisfy
    /// the message's size, alignment, or bit-validity requirements.
    pub fn view(&self) -> RpcResult<&T> {
        T::try_ref_from_bytes(self.frame.as_bytes()?).map_err(|_| RpcError::InvalidMessageFrame {
            message_type: type_name::<T>(),
        })
    }
}

pub(crate) async fn write_frame<T>(writer: &mut LaneWriter, message: &T) -> RpcResult<()>
where
    T: RpcMessage,
{
    write_tagged_frame(writer, message, LaneFrameKind::Message).await
}

pub(crate) async fn write_method_error_frame<T>(writer: &mut LaneWriter, error: &T) -> RpcResult<()>
where
    T: RpcMessage,
{
    write_tagged_frame(writer, error, LaneFrameKind::MethodError).await
}

async fn write_tagged_frame<T>(
    writer: &mut LaneWriter,
    message: &T,
    kind: LaneFrameKind,
) -> RpcResult<()>
where
    T: RpcMessage,
{
    let payload = message.as_bytes();
    let mut encode = |buffer: &mut [u8]| {
        let destination = buffer
            .get_mut(..payload.len())
            .ok_or(RpcError::InvalidLaneState)?;
        destination.copy_from_slice(payload);
        Ok(payload.len())
    };
    core::future::poll_fn(|context| writer.poll_encode_frame(context, kind, &mut encode)).await
}

pub(crate) struct FramedReader<T> {
    reader: LaneReader,
    finished: bool,
    message: PhantomData<fn() -> T>,
}

impl<T> FramedReader<T> {
    pub(crate) fn new(reader: LaneReader) -> Self {
        Self {
            reader,
            finished: false,
            message: PhantomData,
        }
    }

    fn fail(&mut self, error: RpcError) -> Poll<Option<RpcResult<RpcFrame<T>>>> {
        self.finished = true;
        Poll::Ready(Some(Err(error)))
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

        match this.reader.poll_borrow_frame(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => this.fail(error),
            Poll::Ready(Ok(Some(frame))) => {
                if frame.kind() == LaneFrameKind::Message {
                    Poll::Ready(Some(RpcFrame::from_frame(frame)))
                } else {
                    this.fail(RpcError::InvalidFrameState)
                }
            }
            Poll::Ready(Ok(None)) => {
                this.finished = true;
                Poll::Ready(None)
            }
        }
    }
}

pub(crate) struct OutcomeReader<T, E> {
    reader: LaneReader,
    finished: bool,
    message: PhantomData<fn() -> (T, E)>,
}

impl<T, E> OutcomeReader<T, E> {
    pub(crate) fn new(reader: LaneReader) -> Self {
        Self {
            reader,
            finished: false,
            message: PhantomData,
        }
    }

    fn fail(&mut self, error: RpcError) -> FrameOutcomePoll<T, E> {
        self.finished = true;
        Poll::Ready(Some(Err(error)))
    }
}

impl<T, E> Unpin for OutcomeReader<T, E> {}

impl<T, E> Stream for OutcomeReader<T, E>
where
    T: RpcMessage,
    E: RpcMessage,
{
    type Item = RpcResult<FrameOutcome<T, E>>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }

        match this.reader.poll_borrow_frame(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => this.fail(error),
            Poll::Ready(Ok(Some(frame))) => match frame.kind() {
                LaneFrameKind::Message => Poll::Ready(Some(RpcFrame::from_frame(frame).map(Ok))),
                LaneFrameKind::MethodError => {
                    this.finished = true;
                    Poll::Ready(Some(RpcFrame::from_frame(frame).map(Err)))
                }
            },
            Poll::Ready(Ok(None)) => {
                this.finished = true;
                Poll::Ready(None)
            }
        }
    }
}
