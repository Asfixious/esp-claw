//! Asynchronous wire-level RPC payload IO over fixed lane frames.

use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::cell::RefCell;
use std::rc::Rc;

use futures_core::Stream;

use super::lane::{BorrowedFrame, LaneFrameKind, LaneReader, LaneWriter, ReservedFrame};
use super::registry::{PreparedCall, RpcFuture};
use super::{RpcError, RpcResult};

/// One zero-copy response or method-error frame borrowed from an RPC lane.
///
/// The lane cannot publish its next response frame or be reused by another
/// call until this value is dropped.
pub struct RpcPayloadFrame {
    frame: BorrowedFrame,
}

impl RpcPayloadFrame {
    pub(crate) fn from_frame(frame: BorrowedFrame) -> Self {
        Self { frame }
    }
}

impl AsRef<[u8]> for RpcPayloadFrame {
    fn as_ref(&self) -> &[u8] {
        self.frame.as_bytes()
    }
}

impl core::fmt::Debug for RpcPayloadFrame {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("RpcPayloadFrame")
            .field("len", &self.as_ref().len())
            .finish_non_exhaustive()
    }
}

/// A reserved request frame that may be filled directly in lane storage.
///
/// Dropping this value without calling [`commit`](Self::commit) cancels the
/// reservation and publishes nothing.
#[must_use = "a reserved payload frame must be committed to publish it"]
pub struct RpcPayloadWriteFrame<'a> {
    frame: ReservedFrame,
    writer: PhantomData<&'a mut RpcPayloadWriter>,
}

impl RpcPayloadWriteFrame<'_> {
    /// Publishes the initialized prefix of this frame.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError::FrameTooLarge`] when `written` exceeds the writable
    /// slice exposed by [`AsMut<[u8]>`](AsMut).
    pub fn commit(self, written: usize) -> RpcResult<()> {
        self.frame.commit(written, LaneFrameKind::Message)
    }
}

impl AsMut<[u8]> for RpcPayloadWriteFrame<'_> {
    fn as_mut(&mut self) -> &mut [u8] {
        self.frame.as_mut()
    }
}

impl core::fmt::Debug for RpcPayloadWriteFrame<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("RpcPayloadWriteFrame")
            .field("capacity", &self.frame.as_ref().len())
            .finish_non_exhaustive()
    }
}

struct ActivePayloadCall {
    request: LaneWriter,
    response: LaneReader,
    handler: RpcFuture<'static>,
}

type PayloadSetupFuture = Pin<Box<dyn Future<Output = RpcResult<ActivePayloadCall>> + 'static>>;

#[derive(Clone, Copy)]
enum PayloadSide {
    Writer,
    Reader,
}

struct PayloadCallState {
    setup: Option<PayloadSetupFuture>,
    request: Option<LaneWriter>,
    response: Option<LaneReader>,
    handler: Option<RpcFuture<'static>>,
    error: Option<RpcError>,
    writer_alive: bool,
    reader_alive: bool,
    writer_waker: Option<Waker>,
    reader_waker: Option<Waker>,
}

impl PayloadCallState {
    fn update_waker(&mut self, side: PayloadSide, waker: &Waker) {
        let slot = match side {
            PayloadSide::Writer => &mut self.writer_waker,
            PayloadSide::Reader => &mut self.reader_waker,
        };
        if slot
            .as_ref()
            .is_none_or(|registered| !registered.will_wake(waker))
        {
            *slot = Some(waker.clone());
        }
    }

    fn take_peer_waker(&mut self, side: PayloadSide) -> Option<Waker> {
        match side {
            PayloadSide::Writer => self.reader_waker.take(),
            PayloadSide::Reader => self.writer_waker.take(),
        }
    }
}

struct PayloadCallShared {
    state: RefCell<PayloadCallState>,
}

impl PayloadCallShared {
    fn new(prepared: PreparedCall) -> Self {
        let setup = Box::pin(async move {
            let mut acquired = prepared.acquire().await?;
            let lane = acquired.take_lane()?;
            let handler = acquired.start(lane.request_reader, lane.response_writer);
            Ok(ActivePayloadCall {
                request: lane.request_writer,
                response: lane.response_reader,
                handler,
            })
        });
        Self {
            state: RefCell::new(PayloadCallState {
                setup: Some(setup),
                request: None,
                response: None,
                handler: None,
                error: None,
                writer_alive: true,
                reader_alive: true,
                writer_waker: None,
                reader_waker: None,
            }),
        }
    }

    fn poll_ready(&self, side: PayloadSide, context: &mut Context<'_>) -> Poll<RpcResult<()>> {
        let mut completed_handler = None;
        let (result, peer_waker) = {
            let mut state = self.state.borrow_mut();
            state.update_waker(side, context.waker());
            if let Some(error) = state.error.clone() {
                return Poll::Ready(Err(error));
            }

            let setup_poll = state
                .setup
                .as_mut()
                .map(|setup| setup.as_mut().poll(context));
            match setup_poll {
                Some(Poll::Pending) => return Poll::Pending,
                Some(Poll::Ready(Err(error))) => {
                    state.setup = None;
                    state.error = Some(error.clone());
                    let peer = state.take_peer_waker(side);
                    (Poll::Ready(Err(error)), peer)
                }
                Some(Poll::Ready(Ok(active))) => {
                    state.setup = None;
                    state.request = state.writer_alive.then_some(active.request);
                    state.response = state.reader_alive.then_some(active.response);
                    state.handler = Some(active.handler);
                    let peer = state.take_peer_waker(side);
                    let (result, handler, _completed) = Self::poll_handler(&mut state, context);
                    completed_handler = handler;
                    (result, peer)
                }
                None => {
                    let (result, handler, completed) = Self::poll_handler(&mut state, context);
                    completed_handler = handler;
                    let peer = completed.then(|| state.take_peer_waker(side)).flatten();
                    (result, peer)
                }
            }
        };
        drop(completed_handler);
        if let Some(waker) = peer_waker {
            waker.wake();
        }
        result
    }

    fn poll_handler(
        state: &mut PayloadCallState,
        context: &mut Context<'_>,
    ) -> (Poll<RpcResult<()>>, Option<RpcFuture<'static>>, bool) {
        let handler_poll = state
            .handler
            .as_mut()
            .map(|handler| handler.as_mut().poll(context));
        match handler_poll {
            Some(Poll::Ready(Ok(()))) => {
                let handler = state.handler.take();
                (Poll::Ready(Ok(())), handler, true)
            }
            Some(Poll::Ready(Err(error))) => {
                let handler = state.handler.take();
                state.error = Some(error.clone());
                (Poll::Ready(Err(error)), handler, true)
            }
            Some(Poll::Pending) | None => (Poll::Ready(Ok(())), None, false),
        }
    }

    fn take_request(&self) -> RpcResult<LaneWriter> {
        self.state
            .borrow_mut()
            .request
            .take()
            .ok_or(RpcError::InvalidLaneState)
    }

    fn take_response(&self) -> RpcResult<LaneReader> {
        self.state
            .borrow_mut()
            .response
            .take()
            .ok_or(RpcError::InvalidLaneState)
    }

    fn drop_side(&self, side: PayloadSide) {
        let mut request = None;
        let mut response = None;
        let peer_waker = {
            let mut state = self.state.borrow_mut();
            match side {
                PayloadSide::Writer => {
                    state.writer_alive = false;
                    request = state.request.take();
                    state.reader_waker.take()
                }
                PayloadSide::Reader => {
                    state.reader_alive = false;
                    response = state.response.take();
                    state.writer_waker.take()
                }
            }
        };
        drop(request);
        drop(response);
        if let Some(waker) = peer_waker {
            waker.wake();
        }
    }
}

/// Asynchronous request-side payload writer for one runtime-addressed RPC.
///
/// [`write`](Self::write) publishes at most one request frame and returns the
/// number of bytes accepted. [`write_all`](Self::write_all) repeats that
/// operation across frames. The target Method's fixed request wire size is the
/// per-frame capacity; callers do not provide a separate size limit.
pub struct RpcPayloadWriter {
    shared: Rc<PayloadCallShared>,
    writer: Option<LaneWriter>,
    frame_capacity: usize,
    closed: bool,
}

impl RpcPayloadWriter {
    fn poll_operational(&mut self, context: &mut Context<'_>) -> Poll<RpcResult<()>> {
        if self.closed {
            return Poll::Ready(Err(RpcError::FrameWriterClosed));
        }
        match self.shared.poll_ready(PayloadSide::Writer, context) {
            Poll::Ready(Ok(())) => {
                if self.writer.is_none() {
                    self.writer = Some(self.shared.take_request()?);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Waits for writable lane storage and reserves one request frame in place.
    ///
    /// The returned slice has exactly the target Method's fixed request wire
    /// size. Dropping the reservation publishes nothing.
    pub async fn reserve(&mut self) -> RpcResult<RpcPayloadWriteFrame<'_>> {
        let frame_capacity = self.frame_capacity;
        let frame = core::future::poll_fn(|context| {
            match self.poll_operational(context) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            }
            self.writer
                .as_mut()
                .ok_or(RpcError::InvalidLaneState)?
                .poll_reserve_frame(context)
        })
        .await?
        .limit(frame_capacity)?;
        Ok(RpcPayloadWriteFrame {
            frame,
            writer: PhantomData,
        })
    }

    /// Writes at most one request frame and returns the number of bytes written.
    ///
    /// A slice larger than the Method's request frame is partially written;
    /// pass the remaining suffix to another call or use [`write_all`](Self::write_all).
    pub async fn write(&mut self, bytes: &[u8]) -> RpcResult<usize> {
        if bytes.is_empty() || self.frame_capacity == 0 {
            return Ok(0);
        }
        let mut frame = self.reserve().await?;
        let written = bytes.len().min(frame.as_mut().len());
        let source = bytes.get(..written).ok_or(RpcError::InvalidFrameState)?;
        let destination = frame
            .as_mut()
            .get_mut(..written)
            .ok_or(RpcError::InvalidFrameState)?;
        destination.copy_from_slice(source);
        frame.commit(written)?;
        Ok(written)
    }

    pub(crate) async fn write_frame(&mut self, bytes: &[u8]) -> RpcResult<()> {
        let mut frame = self.reserve().await?;
        if frame.as_mut().len() != bytes.len() {
            return Err(RpcError::InvalidFrameState);
        }
        frame.as_mut().copy_from_slice(bytes);
        frame.commit(bytes.len())
    }

    /// Writes the complete slice, splitting it across request frames as needed.
    pub async fn write_all(&mut self, mut bytes: &[u8]) -> RpcResult<()> {
        while !bytes.is_empty() {
            let written = self.write(bytes).await?;
            if written == 0 {
                return Err(RpcError::PayloadWriteZero);
            }
            bytes = bytes.get(written..).ok_or(RpcError::InvalidFrameState)?;
        }
        Ok(())
    }

    /// Closes the request direction and publishes EOF to the handler.
    pub async fn close(&mut self) -> RpcResult<()> {
        if self.closed {
            return Ok(());
        }
        core::future::poll_fn(|context| self.poll_operational(context)).await?;
        drop(self.writer.take());
        self.shared.drop_side(PayloadSide::Writer);
        self.closed = true;
        Ok(())
    }
}

impl core::fmt::Debug for RpcPayloadWriter {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("RpcPayloadWriter")
            .field("frame_capacity", &self.frame_capacity)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl Drop for RpcPayloadWriter {
    fn drop(&mut self) {
        drop(self.writer.take());
        self.shared.drop_side(PayloadSide::Writer);
        self.closed = true;
    }
}

/// Asynchronous response-side payload reader for one runtime-addressed RPC.
pub struct RpcPayloadReader {
    shared: Rc<PayloadCallShared>,
    reader: Option<LaneReader>,
    finished: bool,
}

impl RpcPayloadReader {
    fn poll_operational(&mut self, context: &mut Context<'_>) -> Poll<RpcResult<()>> {
        if self.finished {
            return Poll::Ready(Ok(()));
        }
        match self.shared.poll_ready(PayloadSide::Reader, context) {
            Poll::Ready(Ok(())) => {
                if self.reader.is_none() {
                    self.reader = Some(self.shared.take_response()?);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Waits for the next response frame.
    ///
    /// `Ok(None)` is response EOF. The inner `Result` distinguishes a normal
    /// response frame from the Method's terminal error frame.
    pub async fn read(&mut self) -> RpcResult<Option<Result<RpcPayloadFrame, RpcPayloadFrame>>> {
        core::future::poll_fn(|context| self.poll_read(context)).await
    }

    pub(crate) fn poll_read(
        &mut self,
        context: &mut Context<'_>,
    ) -> Poll<RpcResult<Option<Result<RpcPayloadFrame, RpcPayloadFrame>>>> {
        if self.finished {
            return Poll::Ready(Ok(None));
        }
        match self.poll_operational(context) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(error)) => {
                self.finished = true;
                return Poll::Ready(Err(error));
            }
            Poll::Pending => return Poll::Pending,
        }
        let reader = match self.reader.as_mut() {
            Some(reader) => reader,
            None => return Poll::Ready(Err(RpcError::InvalidLaneState)),
        };
        match reader.poll_borrow_frame(context) {
            Poll::Ready(Ok(Some(frame))) => match frame.kind() {
                LaneFrameKind::Message => {
                    Poll::Ready(Ok(Some(Ok(RpcPayloadFrame::from_frame(frame)))))
                }
                LaneFrameKind::MethodError => {
                    self.finished = true;
                    drop(self.reader.take());
                    Poll::Ready(Ok(Some(Err(RpcPayloadFrame::from_frame(frame)))))
                }
            },
            Poll::Ready(Ok(None)) => {
                self.finished = true;
                drop(self.reader.take());
                Poll::Ready(Ok(None))
            }
            Poll::Ready(Err(error)) => {
                self.finished = true;
                drop(self.reader.take());
                Poll::Ready(Err(error))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl core::fmt::Debug for RpcPayloadReader {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("RpcPayloadReader")
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl Drop for RpcPayloadReader {
    fn drop(&mut self) {
        drop(self.reader.take());
        self.shared.drop_side(PayloadSide::Reader);
        self.finished = true;
    }
}

pub(crate) fn make_payload_call(prepared: PreparedCall) -> (RpcPayloadWriter, RpcPayloadReader) {
    let frame_capacity = prepared.request_frame_size();
    let shared = Rc::new(PayloadCallShared::new(prepared));
    (
        RpcPayloadWriter {
            shared: Rc::clone(&shared),
            writer: None,
            frame_capacity,
            closed: false,
        },
        RpcPayloadReader {
            shared,
            reader: None,
            finished: false,
        },
    )
}

pub(crate) struct PayloadReaderStream {
    reader: LaneReader,
    finished: bool,
}

impl PayloadReaderStream {
    pub(crate) fn new(reader: LaneReader) -> Self {
        Self {
            reader,
            finished: false,
        }
    }

    fn fail(&mut self, error: RpcError) -> Poll<Option<RpcResult<RpcPayloadFrame>>> {
        self.finished = true;
        Poll::Ready(Some(Err(error)))
    }
}

impl Unpin for PayloadReaderStream {}

impl Stream for PayloadReaderStream {
    type Item = RpcResult<RpcPayloadFrame>;

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
                    Poll::Ready(Some(Ok(RpcPayloadFrame::from_frame(frame))))
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
