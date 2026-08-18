//! Async operations of the ring buffer.
//!
//! Everything here is built on `core::future` / `core::task` only, so the
//! futures are async-runtime agnostic. Parking works by registering a single
//! [`Park`] (a waker slot) on the ring; the opposite side signals it when the
//! relevant state changes.

use core::{
    borrow::Borrow,
    future::{Future, IntoFuture},
    marker::PhantomPinned,
    ops::DerefMut,
    pin::Pin,
    task::{Context, Poll},
};

use anylr::SomeOf;

use abs_cancel::{NonCancellableToken, TrCancellationToken, TrMayCancel};

use abs_buff::x_deps::{abs_cancel, anylr};

use super::{
    error_::{RxError, TxError},
    reclaim_::{ReclPeekRef, ReclSliceMut, ReclSliceRef},
    rx_::RingRx,
    state_::{Park, ParkSide, RingBuffer},
    tx_::RingTx,
};

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// The write end's async borrow future (see [`RingTx::write_at_most_async`]).
///
/// Carries the `[min_len, max_len]` interval derived from the
/// [`Demand`](abs_buff::Demand): the future stays pending until at least
/// `min_len` free units are available (honouring `Demand::at_least`), and
/// then borrows at most `max_len` units.
pub struct WriteAsync<'a, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    tx: &'a mut RingTx<H, B, T>,
    min_len: usize,
    max_len: usize,
}

impl<'a, H, B, T> WriteAsync<'a, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    pub(super) fn new(tx: &'a mut RingTx<H, B, T>, min_len: usize, max_len: usize) -> Self {
        WriteAsync {
            tx,
            min_len,
            max_len,
        }
    }
}

impl<'a, H, B, T> IntoFuture for WriteAsync<'a, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    type IntoFuture = WriteFuture<'a, 'a, NonCancellableToken, H, B, T>;
    type Output = SomeOf<ReclSliceMut<'a, T>, TxError<usize>>;

    fn into_future(self) -> Self::IntoFuture {
        WriteFuture::new(
            self.tx,
            self.min_len,
            self.max_len,
            NonCancellableToken::shared_mut(),
        )
    }
}

impl<'a, H, B, T> TrMayCancel<'a> for WriteAsync<'a, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    type MayCancelOutput = SomeOf<ReclSliceMut<'a, T>, TxError<usize>>;

    fn may_cancel_with<'f, C>(
        self,
        cancel: &'f mut C,
    ) -> impl IntoFuture<Output = Self::MayCancelOutput>
    where
        Self: 'f,
        'f: 'a,
        C: TrCancellationToken + Clone,
    {
        WriteFuture::new(self.tx, self.min_len, self.max_len, cancel)
    }
}

/// The poll-based write future.
pub struct WriteFuture<'ctx, 'tok, C, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    _pin: PhantomPinned,
    tx: &'ctx mut RingTx<H, B, T>,
    /// Demand 的下限（无下限时为 0）：可写空间不足时不返回段。
    min_len: usize,
    /// Demand 的上限（无上限时为 `usize::MAX`）：最多借出这么多。
    max_len: usize,
    cancel: &'tok mut C,
    park: Park<B, T>,
}

impl<'ctx, 'tok, C, H, B, T> WriteFuture<'ctx, 'tok, C, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    fn new(
        tx: &'ctx mut RingTx<H, B, T>,
        min_len: usize,
        max_len: usize,
        cancel: &'tok mut C,
    ) -> Self {
        WriteFuture {
            _pin: PhantomPinned,
            tx,
            min_len,
            max_len,
            cancel,
            park: Park::new(ParkSide::TxUser, super::state_::check_tx_writable_at_least),
        }
    }
}

impl<'ctx, C, H, B, T> Future for WriteFuture<'ctx, '_, C, H, B, T>
where
    C: TrCancellationToken,
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    type Output = SomeOf<ReclSliceMut<'ctx, T>, TxError<usize>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let ring: &'ctx RingBuffer<B, T> =
            unsafe { &*(this.tx.ring() as *const RingBuffer<B, T>) };
        loop {
            match ring.try_write_at(this.max_len) {
                Ok((start, take)) => {
                    // 尊重 demand 下限：可写空间不足 min_len 时不返回不满足要求的段，
                    // 继续等待（等读者腾出空间后由 waker 重新唤醒检查）；
                    if take < this.min_len {
                        if this.cancel.is_cancelled() {
                            this.park.deregister(ring);
                            return Poll::Ready(SomeOf::new_right(TxError::Stuffed(0)));
                        }
                        if this.park.poll(cx, ring, this.min_len).is_pending() {
                            return Poll::Pending;
                        }
                        continue;
                    }
                    this.park.deregister(ring);
                    return Poll::Ready(SomeOf::new_left(ring.write_segm(start, take)));
                }
                Err(TxError::Stuffed(_)) => {
                    if this.cancel.is_cancelled() {
                        this.park.deregister(ring);
                        return Poll::Ready(SomeOf::new_right(TxError::Stuffed(0)));
                    }
                    if this.park.poll(cx, ring, this.min_len).is_pending() {
                        return Poll::Pending;
                    }
                }
                Err(TxError::Closing) => {
                    this.park.deregister(ring);
                    return Poll::Ready(SomeOf::new_right(TxError::Closing));
                }
                Err(TxError::Argument) => unreachable!("[WriteFuture] TxError::Argument"),
            }
        }
    }
}

impl<'ctx, 'tok, C, H, B, T> Drop for WriteFuture<'ctx, 'tok, C, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    fn drop(&mut self) {
        let ring = self.tx.ring();
        self.park.deregister(ring);
    }
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// The read end's async borrow future (see [`RingRx::read_at_most_async`]).
///
/// Carries the `[min_len, max_len]` interval derived from the
/// [`Demand`](abs_buff::Demand): the future stays pending until at least
/// `min_len` readable units are available (honouring `Demand::at_least`),
/// and then borrows at most `max_len` units. If the rx end is closed (EOF),
/// the future returns whatever data is available even if it is below
/// `min_len` — no more data will ever arrive.
pub struct ReadAsync<'a, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    rx: &'a mut RingRx<H, B, T>,
    min_len: usize,
    max_len: usize,
}

impl<'a, H, B, T> ReadAsync<'a, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    pub(super) fn new(rx: &'a mut RingRx<H, B, T>, min_len: usize, max_len: usize) -> Self {
        ReadAsync {
            rx,
            min_len,
            max_len,
        }
    }
}

impl<'a, H, B, T> IntoFuture for ReadAsync<'a, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    type IntoFuture = ReadFuture<'a, 'a, NonCancellableToken, H, B, T>;
    type Output = SomeOf<ReclSliceRef<'a, T>, RxError<usize>>;

    fn into_future(self) -> Self::IntoFuture {
        ReadFuture::new(
            self.rx,
            self.min_len,
            self.max_len,
            NonCancellableToken::shared_mut(),
        )
    }
}

impl<'a, H, B, T> TrMayCancel<'a> for ReadAsync<'a, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    type MayCancelOutput = SomeOf<ReclSliceRef<'a, T>, RxError<usize>>;

    fn may_cancel_with<'f, C>(
        self,
        cancel: &'f mut C,
    ) -> impl IntoFuture<Output = Self::MayCancelOutput>
    where
        Self: 'f,
        'f: 'a,
        C: TrCancellationToken + Clone,
    {
        ReadFuture::new(self.rx, self.min_len, self.max_len, cancel)
    }
}

/// The poll-based read future.
pub struct ReadFuture<'ctx, 'tok, C, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    _pin: PhantomPinned,
    rx: &'ctx mut RingRx<H, B, T>,
    /// Demand 的下限（无下限时为 0）：可读数据不足时不返回段。
    min_len: usize,
    /// Demand 的上限（无上限时为 `usize::MAX`）：最多借出这么多。
    max_len: usize,
    cancel: &'tok mut C,
    park: Park<B, T>,
}

impl<'ctx, 'tok, C, H, B, T> ReadFuture<'ctx, 'tok, C, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    fn new(
        rx: &'ctx mut RingRx<H, B, T>,
        min_len: usize,
        max_len: usize,
        cancel: &'tok mut C,
    ) -> Self {
        ReadFuture {
            _pin: PhantomPinned,
            rx,
            min_len,
            max_len,
            cancel,
            park: Park::new(ParkSide::RxUser, super::state_::check_rx_readable_at_least),
        }
    }
}

impl<'ctx, C, H, B, T> Future for ReadFuture<'ctx, '_, C, H, B, T>
where
    C: TrCancellationToken,
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    type Output = SomeOf<ReclSliceRef<'ctx, T>, RxError<usize>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let ring: &'ctx RingBuffer<B, T> =
            unsafe { &*(this.rx.ring() as *const RingBuffer<B, T>) };
        loop {
            match ring.try_read_at(this.max_len) {
                Ok((start, take)) => {
                    if take < this.min_len {
                        // 数据不足 demand 下限：
                        // - 若 rx 已关闭（EOF），不会再有多余数据，返回现有部分；
                        // - 否则继续等待（等写者补充数据后由 waker 重新唤醒检查）。
                        if ring.is_rx_closed() {
                            this.park.deregister(ring);
                            return Poll::Ready(SomeOf::new_left(ring.read_segm(start, take)));
                        }
                        if this.cancel.is_cancelled() {
                            this.park.deregister(ring);
                            return Poll::Ready(SomeOf::new_right(RxError::Drained(0)));
                        }
                        if this.park.poll(cx, ring, this.min_len).is_pending() {
                            return Poll::Pending;
                        }
                        continue;
                    }
                    this.park.deregister(ring);
                    return Poll::Ready(SomeOf::new_left(ring.read_segm(start, take)));
                }
                Err(RxError::Drained(_)) => {
                    if this.cancel.is_cancelled() {
                        this.park.deregister(ring);
                        return Poll::Ready(SomeOf::new_right(RxError::Drained(0)));
                    }
                    if this.park.poll(cx, ring, this.min_len).is_pending() {
                        return Poll::Pending;
                    }
                }
                Err(RxError::Closing) => {
                    this.park.deregister(ring);
                    return Poll::Ready(SomeOf::new_right(RxError::Closing));
                }
                Err(RxError::Argument) => unreachable!("[ReadFuture] RxError::Argument"),
            }
        }
    }
}

impl<'ctx, 'tok, C, H, B, T> Drop for ReadFuture<'ctx, 'tok, C, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    fn drop(&mut self) {
        let ring = self.rx.ring();
        self.park.deregister(ring);
    }
}

// ---------------------------------------------------------------------------
// Peek
// ---------------------------------------------------------------------------

/// The read end's async peek future (see [`RingRx::peek_async`]).
pub struct PeekAsync<'a, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    rx: &'a mut RingRx<H, B, T>,
}

impl<'a, H, B, T> PeekAsync<'a, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    pub(super) fn new(rx: &'a mut RingRx<H, B, T>) -> Self {
        PeekAsync { rx }
    }
}

impl<'a, H, B, T> IntoFuture for PeekAsync<'a, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    type IntoFuture = PeekFuture<'a, 'a, NonCancellableToken, H, B, T>;
    type Output = SomeOf<ReclPeekRef<'a, T>, RxError<usize>>;

    fn into_future(self) -> Self::IntoFuture {
        PeekFuture::new(self.rx, NonCancellableToken::shared_mut())
    }
}

impl<'a, H, B, T> TrMayCancel<'a> for PeekAsync<'a, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    type MayCancelOutput = SomeOf<ReclPeekRef<'a, T>, RxError<usize>>;

    fn may_cancel_with<'f, C>(
        self,
        cancel: &'f mut C,
    ) -> impl IntoFuture<Output = Self::MayCancelOutput>
    where
        Self: 'f,
        'f: 'a,
        C: TrCancellationToken + Clone,
    {
        PeekFuture::new(self.rx, cancel)
    }
}

/// The poll-based peek future.
pub struct PeekFuture<'ctx, 'tok, C, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    _pin: PhantomPinned,
    rx: &'ctx mut RingRx<H, B, T>,
    cancel: &'tok mut C,
    park: Park<B, T>,
}

impl<'ctx, 'tok, C, H, B, T> PeekFuture<'ctx, 'tok, C, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    fn new(rx: &'ctx mut RingRx<H, B, T>, cancel: &'tok mut C) -> Self {
        PeekFuture {
            _pin: PhantomPinned,
            rx,
            cancel,
            park: Park::new(ParkSide::RxUser, super::state_::check_rx_peekable),
        }
    }
}

impl<'ctx, C, H, B, T> Future for PeekFuture<'ctx, '_, C, H, B, T>
where
    C: TrCancellationToken,
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    type Output = SomeOf<ReclPeekRef<'ctx, T>, RxError<usize>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let ring: &'ctx RingBuffer<B, T> =
            unsafe { &*(this.rx.ring() as *const RingBuffer<B, T>) };
        loop {
            match ring.try_peek_at() {
                Ok((start, take)) => {
                    this.park.deregister(ring);
                    return Poll::Ready(SomeOf::new_left(ring.peek_segm(start, take)));
                }
                Err(RxError::Drained(_)) => {
                    if this.cancel.is_cancelled() {
                        this.park.deregister(ring);
                        return Poll::Ready(SomeOf::new_right(RxError::Drained(0)));
                    }
                    if this.park.poll(cx, ring, 0).is_pending() {
                        return Poll::Pending;
                    }
                }
                Err(RxError::Closing) => {
                    this.park.deregister(ring);
                    return Poll::Ready(SomeOf::new_right(RxError::Closing));
                }
                Err(RxError::Argument) => unreachable!("[PeekFuture] RxError::Argument"),
            }
        }
    }
}

impl<'ctx, 'tok, C, H, B, T> Drop for PeekFuture<'ctx, 'tok, C, H, B, T>
where
    H: Borrow<RingBuffer<B, T>>,
    B: DerefMut<Target = [T]>,
{
    fn drop(&mut self) {
        let ring = self.rx.ring();
        self.park.deregister(ring);
    }
}

// ---------------------------------------------------------------------------
// Generic park future (used by the runtime-side waits and the framework
// `AsyncRead` / `AsyncWrite` adapters)
// ---------------------------------------------------------------------------

/// A future that parks (registers a waker) until the given condition holds.
pub struct ParkFuture<'a, B, T>
where
    B: DerefMut<Target = [T]>,
{
    ring: &'a RingBuffer<B, T>,
    park: Park<B, T>,
    arg: usize,
}

impl<'a, B, T> ParkFuture<'a, B, T>
where
    B: DerefMut<Target = [T]>,
{
    pub(super) fn new(
        ring: &'a RingBuffer<B, T>,
        side: ParkSide,
        check: super::state_::ParkCheck<B, T>,
        arg: usize,
    ) -> Self {
        ParkFuture {
            ring,
            park: Park::new(side, check),
            arg,
        }
    }
}

impl<'a, B, T> Future for ParkFuture<'a, B, T>
where
    B: DerefMut<Target = [T]>,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.park.poll(cx, this.ring, this.arg).is_pending() {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

impl<'a, B, T> Drop for ParkFuture<'a, B, T>
where
    B: DerefMut<Target = [T]>,
{
    fn drop(&mut self) {
        self.park.deregister(self.ring);
    }
}

macro_rules! wait_future {
    ($name:ident, $future:ident, $doc:literal, $side:expr, $check:expr, $arg:expr) => {
        #[doc = $doc]
        pub struct $name<'a, B, T>
        where
            B: DerefMut<Target = [T]>,
        {
            ring: &'a RingBuffer<B, T>,
            park: Park<B, T>,
        }

        impl<'a, B, T> $name<'a, B, T>
        where
            B: DerefMut<Target = [T]>,
        {
            #[allow(dead_code)] // some waits are constructed from the ring only
            pub(super) fn new(ring: &'a RingBuffer<B, T>) -> Self {
                $name {
                    ring,
                    park: Park::new($side, $check),
                }
            }
        }

        impl<'a, B, T> IntoFuture for $name<'a, B, T>
        where
            B: DerefMut<Target = [T]>,
        {
            type IntoFuture = $future<'a, B, T>;
            type Output = ();

            fn into_future(self) -> Self::IntoFuture {
                $future {
                    ring: self.ring,
                    park: self.park,
                }
            }
        }

        /// The poll-based future of [`$name`].
        pub struct $future<'a, B, T>
        where
            B: DerefMut<Target = [T]>,
        {
            ring: &'a RingBuffer<B, T>,
            park: Park<B, T>,
        }

        impl<'a, B, T> Future for $future<'a, B, T>
        where
            B: DerefMut<Target = [T]>,
        {
            type Output = ();

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let this = self.get_mut();
                if this.park.poll(cx, this.ring, $arg).is_pending() {
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            }
        }

        impl<'a, B, T> Drop for $future<'a, B, T>
        where
            B: DerefMut<Target = [T]>,
        {
            fn drop(&mut self) {
                self.park.deregister(self.ring);
            }
        }
    };
}

wait_future!(
    WaitFlushed,
    WaitFlushedFuture,
    "Parks until buffered data is available for [`RingBuffer::take_send_iovecs`], or the tx end is closed.",
    ParkSide::TxRuntime,
    super::state_::check_tx_flushed,
    0
);

wait_future!(
    WaitRxIdle,
    WaitRxIdleFuture,
    "Parks until free space is available for [`RingBuffer::take_recv_iovecs`], or the rx end is closed.",
    ParkSide::RxRuntime,
    super::state_::check_rx_idle,
    0
);

wait_future!(
    WaitTxIdle,
    WaitTxIdleFuture,
    "Parks until free space is available for the user writer, or the tx end is closed.",
    ParkSide::TxUser,
    super::state_::check_tx_writable,
    1
);
