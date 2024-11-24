use core::{
    borrow::{Borrow, BorrowMut},
    future::{Future, IntoFuture},
    pin::Pin,
    ptr::NonNull,
    task::{Context, Poll},
};

use pin_project::pin_project;
use pin_utils::pin_mut;

use abs_buff::{TrBuffIterPeek, TrBuffIterTryPeek};
use abs_sync::{cancellation::*, x_deps::pin_utils};
use atomex::TrCmpxchOrderings;
use spmv_oneshot::x_deps::{abs_sync, atomex};

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
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings;

impl<'a, B, P, T, O> BuffPeek<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
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
        let io_ctx = unsafe {
            let mut pointer = NonNull::new_unchecked(self.0.borrow_mut());
            Pin::new_unchecked(pointer.as_mut())
        };
        PeekAsync::new(io_ctx)
    }
}

impl<B, P, T, O> Drop for BuffPeek<'_, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn drop(&mut self) {
        let ctx = self.0.borrow_mut();
        let ctrl = ctx.state().decr_use_count();
        if matches!(ctrl, CtrlHint::MarkClose(_)) {
            ctx.buffer().state().mark_consumer_closed()
        }
        #[cfg(test)]
        log::trace!("[BuffPeek::Drop] ctrl({ctrl})");
    }
}

impl<B, P, T, O> TrBuffIterPeek<T> for BuffPeek<'_, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type SliceRef<'a> = ReclSliceRef<'a, P, T, O> where Self: 'a;
    type BuffIter<'a> = Dual<Self::SliceRef<'a>> where Self: 'a;
    type Err = RxError<usize>;
    type PeekAsync<'a> = PeekAsync<'a, B, P, T, O> where Self: 'a;

    #[inline]
    fn peek_async(&mut self) -> Self::PeekAsync<'_> {
        BuffPeek::peek_async(self)
    }
}

impl<B, P, T, O> TrBuffIterTryPeek<T> for BuffPeek<'_, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{ 
    #[inline]
    fn try_peek(&mut self) -> Result<
        <Self as TrBuffIterPeek<T>>::BuffIter<'_>,
        <Self as TrBuffIterPeek<T>>::Err,
    > {
        BuffPeek::try_peek(self)
    }
}

impl<B, P, T, O> AsRef<RingBuffer<P, T, O>> for BuffPeek<'_, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
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
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings;

impl<'a, B, P, T, O> PeekAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    #[inline(always)]
    pub(super) fn new(io_ctx: Pin<&'a mut IoCtx<B, P, T, O>>) -> Self {
        PeekAsync(io_ctx)
    }

    #[inline(always)]
    pub fn may_cancel_with<C>(
        self,
        cancel: Pin<&'a mut C>,
    ) -> PeekFuture<'a, C, B, P, T, O>
    where
        C: TrCancellationToken,
    {
        PeekFuture::new(self.0, cancel)
    }
}

impl<'a, B, P, T, O> IntoFuture for PeekAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type IntoFuture = PeekFuture<'a, NonCancellableToken, B, P, T, O>;
    type Output = <Self::IntoFuture as Future>::Output;

    fn into_future(self) -> Self::IntoFuture {
        let cancel = NonCancellableToken::pinned();
        PeekFuture::new(self.0, cancel)
    }
}

impl<'a, B, P, T, O> TrIntoFutureMayCancel<'a> for PeekAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type MayCancelOutput =
        <<Self as IntoFuture>::IntoFuture as Future>::Output;

    #[inline(always)]
    fn may_cancel_with<C>(
        self,
        cancel: Pin<&'a mut C>,
    ) -> impl Future<Output = Self::MayCancelOutput>
    where
        C: TrCancellationToken,
    {
        PeekAsync::may_cancel_with(self, cancel)
    }
}

#[pin_project]
pub struct PeekFuture<'a, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    io_ctx_: Pin<&'a mut IoCtx<B, P, T, O>>,
    cancel_: Pin<&'a mut C>,
}

impl<'a, C, B, P, T, O> PeekFuture<'a, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    pub(super) const fn new(
        io_ctx: Pin<&'a mut IoCtx<B, P, T, O>>,
        cancel: Pin<&'a mut C>,
    ) -> Self {
        PeekFuture {
            io_ctx_: io_ctx,
            cancel_: cancel,
        }
    }

    async fn peek_async_(self: Pin<&mut Self>) -> <Self as Future>::Output {
        let this = self.project();
        let ring_buf: &'a RingBuffer<P, T, O> = unsafe {
            let ptr = this.io_ctx_.as_mut().get_unchecked_mut();
            NonNull::new_unchecked(ptr).as_ref().buffer()
        };
        let try_peek =  ring_buf.try_peek_();
        let Result::Err(peek_err) = try_peek else {
            return try_peek;
        };
        let RxError::Drained(_) = peek_err else {
            return Result::Err(peek_err);
        };
        loop {
            if let Option::Some(demand) = this.io_ctx_.as_mut().demand_mut() {
                let x = demand
                    .recv_signal_async(this.cancel_.as_mut())
                    .await;

                return if x.is_ok() {
                    ring_buf.try_peek_()
                } else {
                    let _ = ring_buf.state().dequeue_consumer(demand);
                    Result::Err(RxError::Drained(0usize))
                }
            } else {
                let try_init = this
                    .io_ctx_
                    .as_mut()
                    .try_init_demand(Demand::new(Demand::consumer_check));
                let Result::Ok(demand_ref) = try_init else {
                    continue;
                };
                let x = ring_buf.state().enqueue_consumer(demand_ref);
                #[cfg(test)]
                log::trace!("[ReadFuture::peek_async_] enqueued demand({demand_ref:p})");
                assert!(x)
            }
        }
    }
}

impl<'a, C, B, P, T, O> Future for PeekFuture<'a, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type Output = Result<Dual<ReclSliceRef<'a, P, T, O>>, RxError<usize>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let f = self.peek_async_();
        pin_mut!(f);
        f.poll(cx)
    }
}
