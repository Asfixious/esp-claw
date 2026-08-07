use core::any::type_name;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};

use super::lane::{BorrowedFrame, LaneReader, LaneWriter};
use super::{RpcError, RpcMessage, RpcResult};
use futures_core::Stream;

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
    fn from_frame(frame: BorrowedFrame) -> RpcResult<Self> {
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
    let payload = message.as_bytes();
    let mut encode = |buffer: &mut [u8]| {
        let destination = buffer
            .get_mut(..payload.len())
            .ok_or(RpcError::InvalidLaneState)?;
        destination.copy_from_slice(payload);
        Ok(payload.len())
    };
    core::future::poll_fn(|context| writer.poll_encode_frame(context, &mut encode)).await
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
            Poll::Ready(Ok(Some(frame))) => Poll::Ready(Some(RpcFrame::from_frame(frame))),
            Poll::Ready(Ok(None)) => {
                this.finished = true;
                Poll::Ready(None)
            }
        }
    }
}
