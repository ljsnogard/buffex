use core::{
    borrow::{Borrow, BorrowMut},
    future::{Future, IntoFuture},
    mem::MaybeUninit,
    pin::Pin,
    ptr::NonNull,
    task::{Context, Poll},
};

use pin_project::pin_project;
use pin_utils::pin_mut;

use abs_buff::{
    x_deps::anylr,
    TrBuffIterPeek, TrBuffIterTryPeek,
};
use abs_sync::cancellation::{NonCancellableToken, TrCancellationToken, TrMayCancel};
use anylr::SomeOf;

use atomex::TrCmpxchOrderings;
use segm_buff::x_deps::{abs_buff, abs_sync};

use super::{
    buffer_::{RingBuffer, RxError},
    reclaim_::ReclSliceRef,
    sync_::{CtrlHint, Demand, IoCtx},
    Dual,
};

/// To copy data from, or to peek data stored in, the ring buffer.
pub struct BuffPeek<'a, B, P, T, O>(&'a mut IoCtx<B, P, T, O>)
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings;

impl<'a, B, P, T, O> BuffPeek<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    pub(super) fn new(ctx: &'a mut IoCtx<B, P, T, O>) -> Self {
        ctx.state().incr_use_count();
        BuffPeek(ctx)
    }

    pub fn try_peek(
        &mut self,
    ) -> Result<Dual<ReclSliceRef<'_, P, T, O>>, RxError<usize>> {
        self.0.buffer().try_peek_() 
    }

    pub fn peek_async(&mut self) -> PeekAsync<'_, B, P, T, O> {
        // Safe because IoCtx is !Unpin
        let mut io_ctx = unsafe {
            let mut pointer = NonNull::new_unchecked(self.0.borrow_mut());
            Pin::new_unchecked(pointer.as_mut())
        };
        let _ = io_ctx.as_mut().try_reset_demand();
        PeekAsync::new(io_ctx)
    }
}

impl<B, P, T, O> Drop for BuffPeek<'_, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    fn drop(&mut self) {
        let ctx = self.0.borrow_mut();
        let ctrl = ctx.state().decr_use_count();
        if matches!(ctrl, CtrlHint::MarkClose(_)) {
            ctx.buffer().state().mark_rx_closed()
        }
        #[cfg(test)]
        log::trace!("[BuffPeek::Drop] ctrl({ctrl})");
    }
}

impl<B, P, T, O> TrBuffIterPeek<T> for BuffPeek<'_, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    type SegmRef<'a> = ReclSliceRef<'a, P, T, O> where Self: 'a;
    type Segments<'a> = Dual<Self::SegmRef<'a>> where Self: 'a;
    type PeekAsync<'a> = PeekAsync<'a, B, P, T, O> where Self: 'a;
    type Err = RxError<usize>;

    #[inline]
    fn peek_async(&mut self) -> Self::PeekAsync<'_> {
        BuffPeek::peek_async(self)
    }
}

impl<B, P, T, O> TrBuffIterTryPeek<T> for BuffPeek<'_, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{ 
    #[inline]
    fn try_peek(&mut self) -> SomeOf<
        <Self as TrBuffIterPeek<T>>::Segments<'_>,
        <Self as TrBuffIterPeek<T>>::Err,
    > {
        match BuffPeek::try_peek(self) {
            Result::Ok(segms) => SomeOf::Left(segms),
            Result::Err(err) => SomeOf::Right(err),
        }
    }
}

impl<B, P, T, O> AsRef<RingBuffer<P, T, O>> for BuffPeek<'_, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    #[inline]
    fn as_ref(&self) -> &RingBuffer<P, T, O> {
        self.0.buffer()
    }
}

pub struct PeekAsync<'a, B, P, T, O>(Pin<&'a mut IoCtx<B, P, T, O>>)
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings;

impl<'a, B, P, T, O> PeekAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    #[inline]
    pub(super) fn new(io_ctx: Pin<&'a mut IoCtx<B, P, T, O>>) -> Self {
        PeekAsync(io_ctx)
    }

    #[inline]
    pub fn may_cancel_with<'f, C: TrCancellationToken>(
        self,
        cancel: Pin<&'f mut C>,
    ) -> PeekFuture<'a, 'f, C, B, P, T, O>
    where
        Self: 'f,
    {
        PeekFuture::new(self.0, cancel)
    }
}

impl<'a, B, P, T, O> IntoFuture for PeekAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    type IntoFuture = PeekFuture<'a, 'a, NonCancellableToken, B, P, T, O>;
    type Output = <Self::IntoFuture as Future>::Output;

    fn into_future(self) -> Self::IntoFuture {
        let cancel = NonCancellableToken::pinned();
        PeekFuture::new(self.0, cancel)
    }
}

impl<'a, B, P, T, O> TrMayCancel<'a> for PeekAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    type MayCancelOutput =
        <<Self as IntoFuture>::IntoFuture as Future>::Output;

    #[inline(always)]
    fn may_cancel_with<'f, C: TrCancellationToken>(
        self,
        cancel: Pin<&'f mut C>,
    ) -> impl IntoFuture<Output = Self::MayCancelOutput>
    where
        Self: 'f,
    {
        PeekAsync::may_cancel_with(self, cancel)
    }
}

#[pin_project]
pub struct PeekFuture<'ctx, 'tok, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    io_ctx_: Pin<&'ctx mut IoCtx<B, P, T, O>>,
    cancel_: Pin<&'tok mut C>,
}

impl<'ctx, 'tok, C, B, P, T, O> PeekFuture<'ctx, 'tok, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    pub(super) const fn new(
        io_ctx: Pin<&'ctx mut IoCtx<B, P, T, O>>,
        cancel: Pin<&'tok mut C>,
    ) -> Self {
        PeekFuture {
            io_ctx_: io_ctx,
            cancel_: cancel,
        }
    }
}

impl<'ctx, C, B, P, T, O> Future for PeekFuture<'ctx, '_, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[MaybeUninit<T>]>,
    O: TrCmpxchOrderings,
{
    type Output = SomeOf<Dual<ReclSliceRef<'ctx, P, T, O>>, RxError<usize>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let ring_buf: &'ctx RingBuffer<P, T, O> = unsafe {
            let ptr = this.io_ctx_.as_mut().get_unchecked_mut();
            NonNull::new_unchecked(ptr).as_ref().buffer()
        };
        loop {
            if let Option::Some(demand) = this.io_ctx_.as_mut().demand_mut() {
                let try_peek = ring_buf.try_peek_();
                let SomeOf::Right(rx_err) = try_peek else {
                    // try_peek is ok
                    #[cfg(test)]
                    log::trace!("[PeekFuture::poll] enqueued({demand:p}) try_peek_ ok");
                    let _ = ring_buf.state().dequeue_rx(demand);
                    return Poll::Ready(try_peek.into());
                };
                let RxError::Drained(p) = rx_err else {
                    // try_peek is not TxError::Stuffed
                    #[cfg(test)]
                    log::trace!("[PeekFuture::poll] enqueued({demand:p}) try_peek_ err: {rx_err:?}");
                    let _ = ring_buf.state().dequeue_rx(demand);
                    return Poll::Ready(SomeOf::Right(rx_err));
                };
                let fut_cancel = this
                    .cancel_
                    .as_mut()
                    .cancellation()
                    .into_future();
                pin_mut!(fut_cancel);
                if fut_cancel.poll(cx).is_ready() {
                    #[cfg(test)]
                    log::trace!("[PeekFuture::poll] enqueued({demand:p}) cancelled");
                    let _ = ring_buf.state().dequeue_rx(demand);
                    return Poll::Ready(SomeOf::Right(RxError::Drained(p)));
                }
                break Poll::Pending;
            } else {
                let try_peek = ring_buf.try_peek_();
                let SomeOf::Right(rx_err) = try_peek else {
                    // try_peek is ok
                    return Poll::Ready(try_peek);
                };
                let RxError::Drained(_) = rx_err else {
                    // try_peek is not RxError::Drained
                    #[cfg(test)]
                    log::trace!("[PeekFuture::poll] not queued try_peek_ err: {rx_err:?}");
                    return Poll::Ready(SomeOf::Right(rx_err));
                };
                let demand = Demand::new(
                    Demand::<O>::DEFAULT_PEEK_COUNT,
                    Demand::consumer_check,
                );
                let try_init = this
                    .io_ctx_
                    .as_mut()
                    .try_init_demand(demand);
                let Result::Ok(demand) = try_init else {
                    unreachable!("[PeekFuture::poll]")
                };
                let x = demand.try_init_waker(|| cx.waker().clone());
                assert!(x.is_ok());
                let x = ring_buf.state().enqueue_rx(demand);
                assert!(x);
                #[cfg(test)]
                log::trace!("[PeekFuture::poll] enqueued demand({demand:p})");
            }
        }
    }
}
