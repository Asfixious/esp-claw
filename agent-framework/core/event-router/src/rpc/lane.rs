use core::cell::{Ref, RefCell};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use super::{RpcDirection, RpcError, RpcResult};

pub(crate) const LANE_FRAME_ALIGNMENT: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaneOwner {
    Free,
    Reserved(u64),
    Active,
}

#[repr(C, align(16))]
struct AlignedFrame<const M: usize> {
    bytes: [u8; M],
}

impl<const M: usize> AlignedFrame<M> {
    const fn new() -> Self {
        Self { bytes: [0; M] }
    }
}

struct StaticPipeState {
    len: usize,
    occupied: bool,
    frame_borrowed: bool,
    reader_open: bool,
    writer_open: bool,
    reader_waker: Option<Waker>,
    writer_waker: Option<Waker>,
}

impl StaticPipeState {
    const fn new() -> Self {
        Self {
            len: 0,
            occupied: false,
            frame_borrowed: false,
            reader_open: true,
            writer_open: true,
            reader_waker: None,
            writer_waker: None,
        }
    }
}

struct StaticPipe<const M: usize> {
    bytes: RefCell<AlignedFrame<M>>,
    state: RefCell<StaticPipeState>,
}

impl<const M: usize> StaticPipe<M> {
    const fn new() -> Self {
        Self {
            bytes: RefCell::new(AlignedFrame::new()),
            state: RefCell::new(StaticPipeState::new()),
        }
    }

    fn reset(&self) {
        let mut state = self.state.borrow_mut();
        state.len = 0;
        state.occupied = false;
        state.frame_borrowed = false;
        state.reader_open = true;
        state.writer_open = true;
        state.reader_waker = None;
        state.writer_waker = None;
    }

    fn poll_encode_frame(
        &self,
        context: &mut Context<'_>,
        encode: &mut dyn FnMut(&mut [u8]) -> RpcResult<usize>,
    ) -> (Poll<RpcResult<()>>, Option<Waker>) {
        let mut state = self.state.borrow_mut();
        if !state.writer_open {
            return (Poll::Ready(Err(RpcError::FrameWriterClosed)), None);
        }
        if !state.reader_open {
            return (Poll::Ready(Err(RpcError::FrameReaderClosed)), None);
        }
        if state.occupied {
            update_waker(&mut state.writer_waker, context.waker());
            return (Poll::Pending, None);
        }

        let mut bytes = self.bytes.borrow_mut();
        let length = match encode(&mut bytes.bytes) {
            Ok(length) => length,
            Err(error) => return (Poll::Ready(Err(error)), None),
        };
        if length > M {
            return (Poll::Ready(Err(RpcError::InvalidFrameState)), None);
        }
        state.len = length;
        state.occupied = true;
        (Poll::Ready(Ok(())), state.reader_waker.take())
    }

    fn poll_borrow_frame(
        &'static self,
        context: &mut Context<'_>,
    ) -> Poll<RpcResult<Option<Ref<'static, [u8]>>>> {
        let mut state = self.state.borrow_mut();
        if state.frame_borrowed {
            update_waker(&mut state.reader_waker, context.waker());
            return Poll::Pending;
        }
        if !state.occupied {
            if !state.writer_open {
                return Poll::Ready(Ok(None));
            }
            update_waker(&mut state.reader_waker, context.waker());
            return Poll::Pending;
        }
        let length = state.len;
        state.frame_borrowed = true;
        drop(state);

        match Ref::filter_map(self.bytes.borrow(), |frame| frame.bytes.get(..length)) {
            Ok(bytes) => Poll::Ready(Ok(Some(bytes))),
            Err(_) => {
                self.state.borrow_mut().frame_borrowed = false;
                Poll::Ready(Err(RpcError::InvalidFrameState))
            }
        }
    }

    fn release_borrowed_frame(&self) -> (Option<Waker>, Option<Waker>) {
        let mut state = self.state.borrow_mut();
        if !state.frame_borrowed {
            return (None, None);
        };
        state.frame_borrowed = false;
        state.len = 0;
        state.occupied = false;
        (state.writer_waker.take(), state.reader_waker.take())
    }

    fn close_writer(&self) -> Option<Waker> {
        let mut state = self.state.borrow_mut();
        state.writer_open = false;
        state.reader_waker.take()
    }

    fn close_reader(&self) -> Option<Waker> {
        let mut state = self.state.borrow_mut();
        state.reader_open = false;
        state.writer_waker.take()
    }
}

struct LaneState {
    owner: LaneOwner,
    handles: u8,
}

impl LaneState {
    const fn new() -> Self {
        Self {
            owner: LaneOwner::Free,
            handles: 0,
        }
    }

    fn activate(&mut self) {
        self.owner = LaneOwner::Active;
        self.handles = 4;
    }
}

struct WaiterState {
    ticket: Option<u64>,
    waker: Option<Waker>,
}

impl WaiterState {
    const fn new() -> Self {
        Self {
            ticket: None,
            waker: None,
        }
    }
}

struct PoolState<const N: usize, const Q: usize> {
    lanes: [LaneState; N],
    waiters: [WaiterState; Q],
    next_ticket: u64,
}

impl<const N: usize, const Q: usize> PoolState<N, Q> {
    const fn new() -> Self {
        Self {
            lanes: [const { LaneState::new() }; N],
            waiters: [const { WaiterState::new() }; Q],
            next_ticket: 1,
        }
    }

    fn free_lane(&self) -> Option<usize> {
        self.lanes
            .iter()
            .position(|lane| lane.owner == LaneOwner::Free)
    }

    fn reserved_lane(&self, ticket: u64) -> Option<usize> {
        self.lanes
            .iter()
            .position(|lane| lane.owner == LaneOwner::Reserved(ticket))
    }

    fn waiter_index(&self, ticket: u64) -> Option<usize> {
        self.waiters
            .iter()
            .position(|waiter| waiter.ticket == Some(ticket))
    }

    fn next_waiter_index(&self) -> Option<usize> {
        self.waiters
            .iter()
            .enumerate()
            .filter_map(|(index, waiter)| waiter.ticket.map(|ticket| (index, ticket)))
            .min_by_key(|(_, ticket)| *ticket)
            .map(|(index, _)| index)
    }

    fn activate_lane(&mut self, index: usize) -> RpcResult<()> {
        let lane = self
            .lanes
            .get_mut(index)
            .ok_or(RpcError::InvalidLaneState)?;
        lane.activate();
        Ok(())
    }

    fn clear_waiter(&mut self, ticket: u64) -> RpcResult<Option<Waker>> {
        let Some(index) = self.waiter_index(ticket) else {
            return Ok(None);
        };
        let waiter = self
            .waiters
            .get_mut(index)
            .ok_or(RpcError::InvalidLaneState)?;
        waiter.ticket = None;
        Ok(waiter.waker.take())
    }

    fn reserve_lane_for_next_waiter(&mut self, lane_index: usize) -> RpcResult<Option<Waker>> {
        let Some(waiter_index) = self.next_waiter_index() else {
            let lane = self
                .lanes
                .get_mut(lane_index)
                .ok_or(RpcError::InvalidLaneState)?;
            lane.owner = LaneOwner::Free;
            return Ok(None);
        };
        let waiter = self
            .waiters
            .get_mut(waiter_index)
            .ok_or(RpcError::InvalidLaneState)?;
        let ticket = waiter.ticket.ok_or(RpcError::InvalidLaneState)?;
        let waker = waiter.waker.take();
        let lane = self
            .lanes
            .get_mut(lane_index)
            .ok_or(RpcError::InvalidLaneState)?;
        lane.owner = LaneOwner::Reserved(ticket);
        Ok(waker)
    }
}

/// Fixed-capacity storage for active RPC calls and lane waiters.
///
/// Each lane owns one 16-byte-aligned `M`-byte request pipe and one aligned
/// `M`-byte response pipe. `N` therefore reserves approximately `N * 2 * M`
/// payload bytes. A returned [`RpcFrame`](super::RpcFrame) retains its pipe and
/// lane until drop. `Q` bounds the number of root calls that may wait for a
/// lane. The storage must have application lifetime and is intended for the
/// registry's one cooperative executor thread.
pub struct RpcLaneStorage<const N: usize, const M: usize, const Q: usize> {
    state: RefCell<PoolState<N, Q>>,
    requests: [StaticPipe<M>; N],
    responses: [StaticPipe<M>; N],
}

impl<const N: usize, const M: usize, const Q: usize> RpcLaneStorage<N, M, Q> {
    /// Creates zero-initialized fixed-capacity lane storage.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: RefCell::new(PoolState::new()),
            requests: [const { StaticPipe::new() }; N],
            responses: [const { StaticPipe::new() }; N],
        }
    }

    /// Returns the maximum number of active calls.
    #[must_use]
    pub const fn lane_count(&self) -> usize {
        N
    }

    /// Returns the byte capacity of each request and response direction.
    #[must_use]
    pub const fn frame_capacity(&self) -> usize {
        M
    }

    /// Returns the maximum number of root calls waiting for a lane.
    #[must_use]
    pub const fn waiter_capacity(&self) -> usize {
        Q
    }

    fn pipe(&self, lane: usize, direction: RpcDirection) -> Option<&StaticPipe<M>> {
        match direction {
            RpcDirection::Request => self.requests.get(lane),
            RpcDirection::Response => self.responses.get(lane),
        }
    }

    fn reset_lane(&self, lane: usize) -> RpcResult<()> {
        let request = self.requests.get(lane).ok_or(RpcError::InvalidLaneState)?;
        let response = self.responses.get(lane).ok_or(RpcError::InvalidLaneState)?;
        request.reset();
        response.reset();
        Ok(())
    }
}

impl<const N: usize, const M: usize, const Q: usize> Default for RpcLaneStorage<N, M, Q> {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) struct BorrowedFrame {
    bytes: Option<Ref<'static, [u8]>>,
    pool: &'static dyn LanePool,
    lane: usize,
    direction: RpcDirection,
}

impl BorrowedFrame {
    fn new(
        bytes: Ref<'static, [u8]>,
        pool: &'static dyn LanePool,
        lane: usize,
        direction: RpcDirection,
    ) -> Self {
        Self {
            bytes: Some(bytes),
            pool,
            lane,
            direction,
        }
    }

    pub(crate) fn as_bytes(&self) -> RpcResult<&[u8]> {
        self.bytes.as_deref().ok_or(RpcError::InvalidFrameState)
    }
}

impl Drop for BorrowedFrame {
    fn drop(&mut self) {
        drop(self.bytes.take());
        self.pool.release_borrowed_frame(self.lane, self.direction);
        self.pool.release_handle(self.lane);
    }
}

pub(crate) trait LanePool {
    fn frame_capacity(&self) -> usize;
    fn poll_acquire(
        &self,
        token: &mut Option<u64>,
        nested: bool,
        context: &mut Context<'_>,
    ) -> Poll<RpcResult<usize>>;
    fn cancel_waiter(&self, token: u64);
    fn poll_borrow_frame(
        &'static self,
        lane: usize,
        direction: RpcDirection,
        context: &mut Context<'_>,
    ) -> Poll<RpcResult<Option<BorrowedFrame>>>;
    fn poll_encode_frame(
        &self,
        lane: usize,
        direction: RpcDirection,
        context: &mut Context<'_>,
        encode: &mut dyn FnMut(&mut [u8]) -> RpcResult<usize>,
    ) -> Poll<RpcResult<()>>;
    fn close_reader(&self, lane: usize, direction: RpcDirection);
    fn close_writer(&self, lane: usize, direction: RpcDirection);
    fn release_borrowed_frame(&self, lane: usize, direction: RpcDirection);
    fn retain_handle(&self, lane: usize) -> RpcResult<()>;
    fn release_handle(&self, lane: usize);
}

impl<const N: usize, const M: usize, const Q: usize> LanePool for RpcLaneStorage<N, M, Q> {
    fn frame_capacity(&self) -> usize {
        M
    }

    fn poll_acquire(
        &self,
        token: &mut Option<u64>,
        nested: bool,
        context: &mut Context<'_>,
    ) -> Poll<RpcResult<usize>> {
        let mut state = self.state.borrow_mut();
        if let Some(ticket) = *token {
            if let Some(index) = state.reserved_lane(ticket) {
                if let Err(error) = state.clear_waiter(ticket) {
                    return Poll::Ready(Err(error));
                }
                if let Err(error) = state.activate_lane(index) {
                    return Poll::Ready(Err(error));
                }
                *token = None;
                drop(state);
                return Poll::Ready(self.reset_lane(index).map(|()| index));
            }
            let Some(waiter_index) = state.waiter_index(ticket) else {
                return Poll::Ready(Err(RpcError::InvalidLaneState));
            };
            let Some(waiter) = state.waiters.get_mut(waiter_index) else {
                return Poll::Ready(Err(RpcError::InvalidLaneState));
            };
            update_waker(&mut waiter.waker, context.waker());
            return Poll::Pending;
        }

        if let Some(index) = state.free_lane() {
            if let Err(error) = state.activate_lane(index) {
                return Poll::Ready(Err(error));
            }
            drop(state);
            return Poll::Ready(self.reset_lane(index).map(|()| index));
        }
        if nested {
            return Poll::Ready(Err(RpcError::NestedLaneExhausted { limit: N }));
        }
        let Some(waiter_index) = state
            .waiters
            .iter()
            .position(|waiter| waiter.ticket.is_none())
        else {
            return Poll::Ready(Err(RpcError::LaneWaiterCapacityExceeded { limit: Q }));
        };
        let ticket = state.next_ticket;
        let Some(next_ticket) = ticket.checked_add(1) else {
            return Poll::Ready(Err(RpcError::IdentifierExhausted));
        };
        state.next_ticket = next_ticket;
        let Some(waiter) = state.waiters.get_mut(waiter_index) else {
            return Poll::Ready(Err(RpcError::InvalidLaneState));
        };
        waiter.ticket = Some(ticket);
        update_waker(&mut waiter.waker, context.waker());
        *token = Some(ticket);
        Poll::Pending
    }

    fn cancel_waiter(&self, token: u64) {
        let waker = {
            let mut state = self.state.borrow_mut();
            let _removed_waker = state.clear_waiter(token).ok().flatten();
            let reserved = state.reserved_lane(token);
            reserved.and_then(|index| state.reserve_lane_for_next_waiter(index).ok().flatten())
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn poll_borrow_frame(
        &'static self,
        lane: usize,
        direction: RpcDirection,
        context: &mut Context<'_>,
    ) -> Poll<RpcResult<Option<BorrowedFrame>>> {
        let Some(pipe) = self.pipe(lane, direction) else {
            return Poll::Ready(Err(RpcError::InvalidLaneState));
        };
        match pipe.poll_borrow_frame(context) {
            Poll::Ready(Ok(Some(bytes))) => {
                if let Err(error) = self.retain_handle(lane) {
                    drop(bytes);
                    self.release_borrowed_frame(lane, direction);
                    return Poll::Ready(Err(error));
                }
                Poll::Ready(Ok(Some(BorrowedFrame::new(bytes, self, lane, direction))))
            }
            Poll::Ready(Ok(None)) => Poll::Ready(Ok(None)),
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_encode_frame(
        &self,
        lane: usize,
        direction: RpcDirection,
        context: &mut Context<'_>,
        encode: &mut dyn FnMut(&mut [u8]) -> RpcResult<usize>,
    ) -> Poll<RpcResult<()>> {
        let Some(pipe) = self.pipe(lane, direction) else {
            return Poll::Ready(Err(RpcError::InvalidLaneState));
        };
        let (poll, waker) = pipe.poll_encode_frame(context, encode);
        if let Some(waker) = waker {
            waker.wake();
        }
        poll
    }

    fn close_reader(&self, lane: usize, direction: RpcDirection) {
        let waker = self
            .pipe(lane, direction)
            .and_then(StaticPipe::close_reader);
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn close_writer(&self, lane: usize, direction: RpcDirection) {
        let waker = self
            .pipe(lane, direction)
            .and_then(StaticPipe::close_writer);
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn release_borrowed_frame(&self, lane: usize, direction: RpcDirection) {
        let Some(pipe) = self.pipe(lane, direction) else {
            return;
        };
        let (writer_waker, reader_waker) = pipe.release_borrowed_frame();
        if let Some(waker) = writer_waker {
            waker.wake();
        }
        if let Some(waker) = reader_waker {
            waker.wake();
        }
    }

    fn retain_handle(&self, lane_index: usize) -> RpcResult<()> {
        let mut state = self.state.borrow_mut();
        let lane = state
            .lanes
            .get_mut(lane_index)
            .ok_or(RpcError::InvalidLaneState)?;
        if lane.owner != LaneOwner::Active || lane.handles == 0 {
            return Err(RpcError::InvalidLaneState);
        }
        lane.handles = lane
            .handles
            .checked_add(1)
            .ok_or(RpcError::InvalidLaneState)?;
        Ok(())
    }

    fn release_handle(&self, lane_index: usize) {
        let waker = {
            let mut state = self.state.borrow_mut();
            let released = match state.lanes.get_mut(lane_index) {
                Some(lane) if lane.handles > 0 => {
                    lane.handles = lane.handles.saturating_sub(1);
                    lane.handles == 0
                }
                _ => false,
            };
            if released {
                state
                    .reserve_lane_for_next_waiter(lane_index)
                    .ok()
                    .flatten()
            } else {
                None
            }
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

pub(crate) struct LaneAcquire {
    pool: &'static dyn LanePool,
    nested: bool,
    token: Option<u64>,
    finished: bool,
}

impl LaneAcquire {
    pub(crate) fn new(pool: &'static dyn LanePool, nested: bool) -> Self {
        Self {
            pool,
            nested,
            token: None,
            finished: false,
        }
    }
}

impl Future for LaneAcquire {
    type Output = RpcResult<LaneIo>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(Err(RpcError::CompletedCallPolled));
        }
        match this
            .pool
            .poll_acquire(&mut this.token, this.nested, context)
        {
            Poll::Ready(Ok(index)) => {
                this.finished = true;
                Poll::Ready(Ok(LaneIo::new(this.pool, index)))
            }
            Poll::Ready(Err(error)) => {
                this.finished = true;
                Poll::Ready(Err(error))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for LaneAcquire {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.pool.cancel_waiter(token);
        }
    }
}

pub(crate) struct LaneIo {
    pub(crate) request_reader: LaneReader,
    pub(crate) request_writer: LaneWriter,
    pub(crate) response_reader: LaneReader,
    pub(crate) response_writer: LaneWriter,
}

impl LaneIo {
    fn new(pool: &'static dyn LanePool, lane: usize) -> Self {
        Self {
            request_reader: LaneReader::new(pool, lane, RpcDirection::Request),
            request_writer: LaneWriter::new(pool, lane, RpcDirection::Request),
            response_reader: LaneReader::new(pool, lane, RpcDirection::Response),
            response_writer: LaneWriter::new(pool, lane, RpcDirection::Response),
        }
    }
}

pub(crate) struct LaneReader {
    pool: &'static dyn LanePool,
    lane: usize,
    direction: RpcDirection,
}

impl LaneReader {
    fn new(pool: &'static dyn LanePool, lane: usize, direction: RpcDirection) -> Self {
        Self {
            pool,
            lane,
            direction,
        }
    }

    pub(crate) fn poll_borrow_frame(
        &mut self,
        context: &mut Context<'_>,
    ) -> Poll<RpcResult<Option<BorrowedFrame>>> {
        self.pool
            .poll_borrow_frame(self.lane, self.direction, context)
    }
}

impl Drop for LaneReader {
    fn drop(&mut self) {
        self.pool.close_reader(self.lane, self.direction);
        self.pool.release_handle(self.lane);
    }
}

pub(crate) struct LaneWriter {
    pool: &'static dyn LanePool,
    lane: usize,
    direction: RpcDirection,
}

impl LaneWriter {
    fn new(pool: &'static dyn LanePool, lane: usize, direction: RpcDirection) -> Self {
        Self {
            pool,
            lane,
            direction,
        }
    }

    pub(crate) fn close(&mut self) {
        self.pool.close_writer(self.lane, self.direction);
    }

    pub(crate) fn poll_encode_frame(
        &mut self,
        context: &mut Context<'_>,
        encode: &mut dyn FnMut(&mut [u8]) -> RpcResult<usize>,
    ) -> Poll<RpcResult<()>> {
        self.pool
            .poll_encode_frame(self.lane, self.direction, context, encode)
    }
}

impl Drop for LaneWriter {
    fn drop(&mut self) {
        self.pool.close_writer(self.lane, self.direction);
        self.pool.release_handle(self.lane);
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
