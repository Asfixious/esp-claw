use core::cmp;
use core::future::poll_fn;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use super::frame::RpcFrameBuffer;
use super::RpcResult;

/// A dynamically dispatched asynchronous binary reader.
pub type BoxBinaryReader = Pin<Box<dyn BinaryReader>>;

/// A dynamically dispatched asynchronous binary writer.
pub type BoxBinaryWriter = Pin<Box<dyn BinaryWriter>>;

/// Pull-based asynchronous byte input used by every RPC invocation.
pub trait BinaryReader {
    /// Attempts to read bytes into `buffer`.
    ///
    /// Returning `Ok(0)` for a non-empty buffer means EOF. Implementations
    /// return [`Poll::Pending`] when no bytes are currently available but the
    /// stream can still produce more data.
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<Result<usize, BinaryIoError>>;

    /// Returns whether this reader can lend complete frames without copying.
    #[doc(hidden)]
    fn supports_borrowed_frames(&self) -> bool {
        false
    }

    /// Borrows one complete transport-owned frame, or returns `None` at EOF.
    #[doc(hidden)]
    fn poll_borrow_frame(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<RpcResult<Option<RpcFrameBuffer>>> {
        Poll::Ready(Ok(None))
    }
}

/// Push-based asynchronous byte output used by every RPC invocation.
pub trait BinaryWriter {
    /// Attempts to write bytes from `buffer`.
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, BinaryIoError>>;

    /// Flushes bytes accepted by this writer.
    fn poll_flush(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), BinaryIoError>>;

    /// Closes this writer and communicates EOF to its reader.
    fn poll_close(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), BinaryIoError>>;

    /// Lets a fixed-buffer transport encode one complete typed frame in place.
    ///
    /// `true` means the frame was written. `false` means this writer only
    /// supports byte IO and the caller must use ordinary framing.
    #[doc(hidden)]
    fn poll_encode_frame(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        _limit: usize,
        _encode: &mut dyn FnMut(&mut [u8]) -> RpcResult<usize>,
    ) -> Poll<RpcResult<bool>> {
        Poll::Ready(Ok(false))
    }
}

/// Reads one available byte fragment.
///
/// # Errors
///
/// Returns the error produced by `reader`.
pub async fn read<R>(mut reader: Pin<&mut R>, buffer: &mut [u8]) -> Result<usize, BinaryIoError>
where
    R: BinaryReader + ?Sized,
{
    poll_fn(|context| reader.as_mut().poll_read(context, buffer)).await
}

/// Writes one available byte fragment.
///
/// # Errors
///
/// Returns the error produced by `writer`.
pub async fn write<W>(mut writer: Pin<&mut W>, buffer: &[u8]) -> Result<usize, BinaryIoError>
where
    W: BinaryWriter + ?Sized,
{
    poll_fn(|context| writer.as_mut().poll_write(context, buffer)).await
}

/// Writes the complete buffer, waiting for backpressure to clear as needed.
///
/// # Errors
///
/// Returns [`BinaryIoError::WriteZero`] if the writer makes no progress, or
/// propagates an error returned by the writer.
pub async fn write_all<W>(mut writer: Pin<&mut W>, mut buffer: &[u8]) -> Result<(), BinaryIoError>
where
    W: BinaryWriter + ?Sized,
{
    while !buffer.is_empty() {
        let written = write(writer.as_mut(), buffer).await?;
        if written == 0 {
            return Err(BinaryIoError::WriteZero);
        }
        buffer = buffer
            .get(written..)
            .ok_or(BinaryIoError::InvalidWriteCount {
                reported: written,
                available: buffer.len(),
            })?;
    }
    Ok(())
}

/// Flushes a binary writer.
///
/// # Errors
///
/// Returns the error produced by `writer`.
pub async fn flush<W>(mut writer: Pin<&mut W>) -> Result<(), BinaryIoError>
where
    W: BinaryWriter + ?Sized,
{
    poll_fn(|context| writer.as_mut().poll_flush(context)).await
}

/// Closes a binary writer.
///
/// # Errors
///
/// Returns the error produced by `writer`.
pub async fn close<W>(mut writer: Pin<&mut W>) -> Result<(), BinaryIoError>
where
    W: BinaryWriter + ?Sized,
{
    poll_fn(|context| writer.as_mut().poll_close(context)).await
}

/// Reads a finite body until EOF, enforcing a maximum result size.
///
/// # Errors
///
/// Returns [`BinaryIoError::LimitExceeded`] if more than `limit` bytes arrive,
/// [`BinaryIoError::InvalidReadCount`] for a broken reader implementation, or
/// propagates an error returned by the reader.
pub async fn read_to_end<R>(mut reader: Pin<&mut R>, limit: usize) -> Result<Vec<u8>, BinaryIoError>
where
    R: BinaryReader + ?Sized,
{
    const READ_CHUNK_SIZE: usize = 1024;

    let mut output = Vec::new();
    let mut buffer = [0_u8; READ_CHUNK_SIZE];
    loop {
        let read_count = read(reader.as_mut(), &mut buffer).await?;
        if read_count == 0 {
            return Ok(output);
        }
        let bytes = buffer
            .get(..read_count)
            .ok_or(BinaryIoError::InvalidReadCount {
                reported: read_count,
                available: buffer.len(),
            })?;
        let new_length = output
            .len()
            .checked_add(bytes.len())
            .ok_or(BinaryIoError::LimitExceeded { limit })?;
        if new_length > limit {
            return Err(BinaryIoError::LimitExceeded { limit });
        }
        output.extend_from_slice(bytes);
    }
}

/// A finite in-memory reader suitable for a unary request or response body.
#[derive(Debug)]
pub struct BytesReader {
    bytes: Box<[u8]>,
    offset: usize,
}

impl BytesReader {
    /// Creates a reader that reaches EOF after all supplied bytes are read.
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: bytes.into().into_boxed_slice(),
            offset: 0,
        }
    }

    /// Boxes this reader for use in an RPC invocation.
    #[must_use]
    pub fn boxed(self) -> BoxBinaryReader {
        Box::pin(self)
    }
}

impl BinaryReader for BytesReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<Result<usize, BinaryIoError>> {
        if buffer.is_empty() || self.offset == self.bytes.len() {
            return Poll::Ready(Ok(0));
        }
        let Some(remaining) = self.bytes.get(self.offset..) else {
            return Poll::Ready(Err(BinaryIoError::InvalidReadPosition));
        };
        let count = cmp::min(remaining.len(), buffer.len());
        let Some(source) = remaining.get(..count) else {
            return Poll::Ready(Err(BinaryIoError::InvalidReadPosition));
        };
        let Some(destination) = buffer.get_mut(..count) else {
            return Poll::Ready(Err(BinaryIoError::InvalidReadPosition));
        };
        destination.copy_from_slice(source);
        self.offset = self
            .offset
            .checked_add(count)
            .ok_or(BinaryIoError::InvalidReadPosition)?;
        Poll::Ready(Ok(count))
    }
}

/// Creates a bounded, single-reader/single-writer in-memory binary pipe.
///
/// The byte capacity is a hard upper bound. A full pipe returns
/// [`Poll::Pending`] from [`BinaryWriter::poll_write`] until the reader consumes
/// data, providing deterministic backpressure.
///
/// # Errors
///
/// Returns [`BinaryIoError::ZeroCapacity`] when `capacity` is zero.
pub fn binary_pipe(capacity: usize) -> Result<(BinaryPipeReader, BinaryPipeWriter), BinaryIoError> {
    if capacity == 0 {
        return Err(BinaryIoError::ZeroCapacity);
    }
    let shared = Rc::new(RefCell::new(PipeState::new(capacity)));
    Ok((
        BinaryPipeReader {
            shared: Rc::clone(&shared),
        },
        BinaryPipeWriter { shared },
    ))
}

/// Reader half of a bounded [`binary_pipe`].
pub struct BinaryPipeReader {
    shared: Rc<RefCell<PipeState>>,
}

/// Writer half of a bounded [`binary_pipe`].
pub struct BinaryPipeWriter {
    shared: Rc<RefCell<PipeState>>,
}

struct PipeState {
    chunks: VecDeque<PipeChunk>,
    buffered: usize,
    capacity: usize,
    reader_open: bool,
    writer_open: bool,
    reader_waker: Option<Waker>,
    writer_waker: Option<Waker>,
}

impl PipeState {
    fn new(capacity: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            buffered: 0,
            capacity,
            reader_open: true,
            writer_open: true,
            reader_waker: None,
            writer_waker: None,
        }
    }
}

struct PipeChunk {
    bytes: Box<[u8]>,
    offset: usize,
}

impl BinaryReader for BinaryPipeReader {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<Result<usize, BinaryIoError>> {
        if buffer.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let mut state = self.shared.borrow_mut();
        if state.buffered == 0 {
            if !state.writer_open {
                return Poll::Ready(Ok(0));
            }
            update_waker(&mut state.reader_waker, context.waker());
            return Poll::Pending;
        }

        let Some(chunk) = state.chunks.front_mut() else {
            return Poll::Ready(Err(BinaryIoError::InvalidPipeState));
        };
        let Some(remaining) = chunk.bytes.get(chunk.offset..) else {
            return Poll::Ready(Err(BinaryIoError::InvalidPipeState));
        };
        let count = cmp::min(remaining.len(), buffer.len());
        let Some(source) = remaining.get(..count) else {
            return Poll::Ready(Err(BinaryIoError::InvalidPipeState));
        };
        let Some(destination) = buffer.get_mut(..count) else {
            return Poll::Ready(Err(BinaryIoError::InvalidPipeState));
        };
        destination.copy_from_slice(source);
        chunk.offset = chunk
            .offset
            .checked_add(count)
            .ok_or(BinaryIoError::InvalidPipeState)?;
        let chunk_finished = chunk.offset == chunk.bytes.len();
        state.buffered = state
            .buffered
            .checked_sub(count)
            .ok_or(BinaryIoError::InvalidPipeState)?;
        if chunk_finished {
            state.chunks.pop_front();
        }
        let writer_waker = state.writer_waker.take();
        drop(state);
        if let Some(waker) = writer_waker {
            waker.wake();
        }
        Poll::Ready(Ok(count))
    }
}

impl Drop for BinaryPipeReader {
    fn drop(&mut self) {
        let writer_waker = {
            let mut state = self.shared.borrow_mut();
            state.reader_open = false;
            state.writer_waker.take()
        };
        if let Some(waker) = writer_waker {
            waker.wake();
        }
    }
}

impl BinaryWriter for BinaryPipeWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, BinaryIoError>> {
        if buffer.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let mut state = self.shared.borrow_mut();
        if !state.writer_open {
            return Poll::Ready(Err(BinaryIoError::WriterClosed));
        }
        if !state.reader_open {
            return Poll::Ready(Err(BinaryIoError::BrokenPipe));
        }
        let available = state
            .capacity
            .checked_sub(state.buffered)
            .ok_or(BinaryIoError::InvalidPipeState)?;
        if available == 0 {
            update_waker(&mut state.writer_waker, context.waker());
            return Poll::Pending;
        }
        let count = cmp::min(available, buffer.len());
        let Some(bytes) = buffer.get(..count) else {
            return Poll::Ready(Err(BinaryIoError::InvalidPipeState));
        };
        state.chunks.push_back(PipeChunk {
            bytes: bytes.into(),
            offset: 0,
        });
        state.buffered = state
            .buffered
            .checked_add(count)
            .ok_or(BinaryIoError::InvalidPipeState)?;
        let reader_waker = state.reader_waker.take();
        drop(state);
        if let Some(waker) = reader_waker {
            waker.wake();
        }
        Poll::Ready(Ok(count))
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Result<(), BinaryIoError>> {
        let state = self.shared.borrow();
        if !state.writer_open {
            return Poll::Ready(Err(BinaryIoError::WriterClosed));
        }
        if !state.reader_open {
            return Poll::Ready(Err(BinaryIoError::BrokenPipe));
        }
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Result<(), BinaryIoError>> {
        let reader_waker = {
            let mut state = self.shared.borrow_mut();
            if !state.writer_open {
                return Poll::Ready(Ok(()));
            }
            state.writer_open = false;
            state.reader_waker.take()
        };
        if let Some(waker) = reader_waker {
            waker.wake();
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for BinaryPipeWriter {
    fn drop(&mut self) {
        let reader_waker = {
            let mut state = self.shared.borrow_mut();
            state.writer_open = false;
            state.reader_waker.take()
        };
        if let Some(waker) = reader_waker {
            waker.wake();
        }
    }
}

fn update_waker(slot: &mut Option<Waker>, waker: &Waker) {
    if slot
        .as_ref()
        .is_none_or(|registered| !registered.will_wake(waker))
    {
        *slot = Some(waker.clone());
    }
}

/// Error produced by binary readers and writers.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BinaryIoError {
    /// A pipe must reserve at least one byte.
    #[error("binary pipe capacity must be greater than zero")]
    ZeroCapacity,
    /// The reader side of a pipe was dropped.
    #[error("binary pipe reader is closed")]
    BrokenPipe,
    /// A write was attempted after closing the writer.
    #[error("binary writer is closed")]
    WriterClosed,
    /// A writer reported successful progress of zero bytes.
    #[error("binary writer made no progress")]
    WriteZero,
    /// A reader reported more bytes than its destination could contain.
    #[error("binary reader reported {reported} bytes for a {available}-byte buffer")]
    InvalidReadCount {
        /// Count reported by the reader.
        reported: usize,
        /// Available destination size.
        available: usize,
    },
    /// A writer reported more bytes than its source contained.
    #[error("binary writer reported {reported} bytes for a {available}-byte buffer")]
    InvalidWriteCount {
        /// Count reported by the writer.
        reported: usize,
        /// Available source size.
        available: usize,
    },
    /// A finite read crossed its configured allocation limit.
    #[error("binary body exceeds the {limit}-byte limit")]
    LimitExceeded {
        /// Maximum permitted body size.
        limit: usize,
    },
    /// A reader implementation reached an inconsistent cursor position.
    #[error("binary reader has an invalid position")]
    InvalidReadPosition,
    /// A pipe's internal counters and queued chunks disagree.
    #[error("binary pipe is in an invalid state")]
    InvalidPipeState,
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use futures_lite::future::block_on;
    use futures_util::join;

    #[test]
    fn bytes_reader_is_finite_and_reaches_eof() {
        block_on(async {
            let mut reader = BytesReader::new(b"abc".to_vec());
            let body = read_to_end(Pin::new(&mut reader), 3).await;
            assert_eq!(body, Ok(b"abc".to_vec()));
        });
    }

    #[test]
    fn bounded_pipe_applies_backpressure_and_preserves_bytes() {
        block_on(async {
            let (mut reader, mut writer) = binary_pipe(2).expect("valid capacity");
            let producer = async {
                write_all(Pin::new(&mut writer), b"abcdef").await?;
                close(Pin::new(&mut writer)).await
            };
            let consumer = read_to_end(Pin::new(&mut reader), 6);
            let (write_result, read_result) = join!(producer, consumer);
            assert_eq!(write_result, Ok(()));
            assert_eq!(read_result, Ok(b"abcdef".to_vec()));
        });
    }

    #[test]
    fn dropping_writer_communicates_eof() {
        block_on(async {
            let (mut reader, writer) = binary_pipe(1).expect("valid capacity");
            drop(writer);
            assert_eq!(read_to_end(Pin::new(&mut reader), 1).await, Ok(Vec::new()));
        });
    }

    #[test]
    fn dropping_reader_breaks_pending_writer() {
        block_on(async {
            let (reader, mut writer) = binary_pipe(1).expect("valid capacity");
            drop(reader);
            assert_eq!(
                write(Pin::new(&mut writer), b"x").await,
                Err(BinaryIoError::BrokenPipe)
            );
        });
    }
}
