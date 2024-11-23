use core::{
    borrow::{Borrow, BorrowMut},
    future::{Future, IntoFuture},
    pin::Pin,
    ptr::NonNull,
    task::{Context, Poll},
};

use pin_project::pin_project;
use pin_utils::pin_mut;

use abs_buff::{
    TrBuffIterPeek, TrBuffIterRead, TrBuffIterTryPeek, TrBuffIterTryRead,
};
use abs_sync::{cancellation::*, x_deps::pin_utils};
use atomex::TrCmpxchOrderings;
use spmv_oneshot::x_deps::{abs_sync, atomex};

use super::{
    buffer_::{RingBuffer, RxError},
    peek_::{BuffPeek, PeekAsync},
    reclaim_::ReclSliceRef,
    sync_::{CtrlHint, Demand, IoCtx},
    Dual,
};

/// To move data from, or to pull data out of, the ring buffer.
pub struct BuffRx<B, P, T, O>(IoCtx<B, P, T, O>)
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings;

impl<B, P, T, O> BuffRx<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    pub(super) fn new(ctx: IoCtx<B, P, T, O>) -> Self {
        ctx.state().incr_use_count();
        BuffRx(ctx)
    }

    pub fn try_read(
        &mut self,
        length: usize,
    ) -> Result<Dual<ReclSliceRef<'_, P, T, O>>, RxError<usize>> {
        self.0.borrow().buffer().try_read_(length)
    }

    pub fn read_async(
        &mut self,
        length: usize,
    ) -> ReadAsync<'_, B, P, T, O> {
        let context = unsafe {
            let mut pointer = NonNull::new_unchecked(&mut self.0);
            Pin::new_unchecked(pointer.as_mut())
        };
        ReadAsync::new(context, length)
    }

    pub fn try_peek(
        &mut self,
    ) -> Result<Dual<ReclSliceRef<'_, P, T, O>>, RxError<usize>> {
        self.0.buffer().try_peek_()
    }

    pub fn peek_async(&mut self) -> PeekAsync<'_, B, P, T, O> {
        let context = unsafe {
            let mut pointer = NonNull::new_unchecked(&mut self.0);
            Pin::new_unchecked(pointer.as_mut())
        };
        PeekAsync::new(context)
    }

    pub fn as_peek(&mut self) -> BuffPeek<'_, B, P, T, O> {
        BuffPeek::new(&mut self.0)
    }
}

impl<B, P, T, O> Drop for BuffRx<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn drop(&mut self) {
        let ctx = &self.0;
        let hint = ctx.state().decr_use_count();
        if matches!(hint, CtrlHint::MarkClose(_)) {
            ctx.buffer().state().mark_consumer_closed()
        }
        #[cfg(test)]
        log::trace!("[BuffRead::Drop] hint({hint})");
    }
}

impl<B, P, T, O> AsRef<RingBuffer<P, T, O>> for BuffRx<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn as_ref(&self) -> &RingBuffer<P, T, O> {
        self.0.borrow().buffer()
    }
}

impl<B, P, T, O> TrBuffIterRead<T> for BuffRx<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type SliceRef<'a> = ReclSliceRef<'a, P, T, O> where Self: 'a;
    type BuffIter<'a> = Dual<Self::SliceRef<'a>> where Self: 'a;
    type Err = RxError<usize>;
    type ReadAsync<'a> = ReadAsync<'a, B, P, T, O> where Self: 'a;

    #[inline]
    fn read_async(&mut self, length: usize) -> Self::ReadAsync<'_> {
        BuffRx::read_async(self, length)
    }
}

impl<B, P, T, O> TrBuffIterTryRead<T> for BuffRx<B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    #[inline]
    fn try_read(&mut self, length: usize) -> Result<
        <Self as TrBuffIterRead<T>>::BuffIter<'_>,
        <Self as TrBuffIterRead<T>>::Err,
    > {
        BuffRx::try_read(self, length)
    }
}

impl<B, P, T, O> TrBuffIterPeek<T> for BuffRx<B, P, T, O>
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
        BuffRx::peek_async(self)
    }
}

impl<B, P, T, O> TrBuffIterTryPeek<T> for BuffRx<B, P, T, O>
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
        BuffRx::try_peek(self)
    }
}

pub struct ReadAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    io_ctx_: Pin<&'a mut IoCtx<B, P, T, O>>,
    length_: usize,
}

impl<'a, B, P, T, O> ReadAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    pub(super) const fn new(
        io_ctx: Pin<&'a mut IoCtx<B, P, T, O>>,
        length: usize,
    ) -> Self {
        ReadAsync {
            io_ctx_: io_ctx,
            length_: length,
        }
    }

    #[inline(always)]
    pub fn may_cancel_with<C>(
        self,
        cancel: Pin<&'a mut C>,
    ) -> ReadFuture<'a, C, B, P, T, O>
    where
        C: TrCancellationToken,
    {
        ReadFuture::new(self.io_ctx_, self.length_, cancel)
    }
}

impl<'a, B, P, T, O> IntoFuture for ReadAsync<'a, B, P, T, O>
where
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type IntoFuture = ReadFuture<'a, NonCancellableToken, B, P, T, O>;
    type Output = <Self::IntoFuture as Future>::Output;

    fn into_future(self) -> Self::IntoFuture {
        let cancel = NonCancellableToken::pinned();
        ReadFuture::new(self.io_ctx_, self.length_, cancel)
    }
}

impl<'a, B, P, T, O> TrIntoFutureMayCancel<'a> for ReadAsync<'a, B, P, T, O>
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
        ReadAsync::may_cancel_with(self, cancel)
    }
}

#[pin_project]
pub struct ReadFuture<'a, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    io_ctx_: Pin<&'a mut IoCtx<B, P, T, O>>,
    length_: usize,
    cancel_: Pin<&'a mut C>,
}

impl<'a, C, B, P, T, O> Future for ReadFuture<'a, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    type Output = Result<Dual<ReclSliceRef<'a, P, T, O>>, RxError<usize>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let f = self.read_async_();
        pin_mut!(f);
        f.poll(cx)
    }
}

impl<'a, C, B, P, T, O> ReadFuture<'a, C, B, P, T, O>
where
    C: TrCancellationToken,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    pub(super) const fn new(
        io_ctx: Pin<&'a mut IoCtx<B, P, T, O>>,
        length: usize,
        cancel: Pin<&'a mut C>,
    ) -> Self {
        ReadFuture {
            io_ctx_: io_ctx,
            length_: length,
            cancel_: cancel,
        }
    }

    async fn read_async_(
        self: Pin<&mut Self>,
    ) -> Result<Dual<ReclSliceRef<'a, P, T, O>>, RxError<usize>> {
        let this = self.project();
        let mut p_ctx = unsafe {
            let ptr = this.io_ctx_.as_mut().get_unchecked_mut();
            NonNull::new_unchecked(ptr)
        };
        let p_ring_buf = unsafe { 
            let ring_buf = p_ctx.as_ref().buffer();
            let ptr = ring_buf as *const _ as *mut RingBuffer<P, T, O>;
            NonNull::new_unchecked(ptr)
        };
        let try_read = unsafe {
            p_ring_buf.as_ref().try_read_(*this.length_)
        };
        let Result::Err(read_err) = try_read else {
            #[cfg(test)]
            log::trace!("[ReadFuture::read_async_] try_read ok");
            return try_read;
        };
        let RxError::Drained(_) = read_err else {
            #[cfg(test)]
            log::trace!("[ReadFuture::read_async_] {read_err}");
            return Result::Err(read_err);
        };
        loop {
            let opt_demand = unsafe { p_ctx.as_ref().demand() };
            if let Option::Some(demand_ref) = opt_demand {
                let sign_recv = demand_ref.signal.peeker();
                pin_mut!(sign_recv);

                #[cfg(test)]
                log::trace!("[ReadFuture::read_async_] before await sig({demand_ref:p})");

                let x = sign_recv
                    .peek_async()
                    .may_cancel_with(this.cancel_.as_mut())
                    .await;

                #[cfg(test)]
                log::trace!("[ReadFuture::read_async_] sig recv({demand_ref:p}) {x:?}");

                let ring_buf = unsafe { p_ring_buf.as_ref() };
                let _ = ring_buf.state().dequeue_consumer(demand_ref);
                return if x.is_ok() {
                    unsafe { p_ring_buf.as_ref().try_read_(*this.length_) }
                } else {
                    Result::Err(RxError::Drained(0usize))
                }
            } else {
                let demand_ref = Demand::new(Demand::consumer_check);
                let try_init = unsafe {
                    let ctx_pin = Pin::new_unchecked(p_ctx.as_mut());
                    ctx_pin.try_init_demand(demand_ref)
                };
                let Result::Ok(demand_ref) = try_init else {
                    continue;
                };
                let ring_buf = unsafe { p_ring_buf.as_ref() };
                let x = ring_buf.state().enqueue_consumer(demand_ref);
                #[cfg(test)]
                log::trace!("[ReadFuture::read_async_] enqueued demand({demand_ref:p})");
                assert!(x)
            }
        }
    }
}
