use core::{
    borrow::{Borrow, BorrowMut},
    future::{Future, IntoFuture},
    marker::PhantomData,
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
use asyncex_channel::x_deps::{abs_sync, atomex};
use atomex::TrCmpxchOrderings;

use super::{
    buffer_::{IoCtrl, IoCtx, RingBuffer, RxError},
    peek_::{BuffPeek, PeekAsync},
    reclaim_::ReclSliceRef,
    sync_::{Demand, RwState},
    Dual,
};

/// To move data from, or to put data out of, the ring buffer.
pub struct BuffRead<X, B, P, T, O>(X, PhantomData<IoCtx<B, P, T, O>>)
where
    X: BorrowMut<IoCtx<B, P, T, O>>,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings;

impl<X, B, P, T, O> BuffRead<X, B, P, T, O>
where
    X: BorrowMut<IoCtx<B, P, T, O>>,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    pub(super) fn new(ctx: X) -> Self {
        ctx.borrow().state().incr_use_count();
        BuffRead(ctx, PhantomData)
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
            let mut pointer = NonNull::new_unchecked(self.0.borrow_mut());
            Pin::new_unchecked(pointer.as_mut())
        };
        ReadAsync::new(context, length)
    }

    pub fn try_peek(
        &mut self,
    ) -> Result<Dual<ReclSliceRef<'_, P, T, O>>, RxError<usize>> {
        self.0.borrow().buffer().try_peek_()
    }

    pub fn peek_async(&mut self) -> PeekAsync<'_, B, P, T, O> {
        let context = unsafe {
            let mut pointer = NonNull::new_unchecked(self.0.borrow_mut());
            Pin::new_unchecked(pointer.as_mut())
        };
        PeekAsync::new(context)
    }

    pub fn as_peek(
        &mut self,
    ) -> BuffPeek<&mut IoCtx<B, P, T, O>, B, P, T, O> {
        let ctx = self.0.borrow_mut();
        ctx.state().incr_use_count();
        BuffPeek::new(ctx)
    }
}

impl<X, B, P, T, O> Drop for BuffRead<X, B, P, T, O>
where
    X: BorrowMut<IoCtx<B, P, T, O>>,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn drop(&mut self) {
        let ctx = self.0.borrow_mut();
        let ctrl = ctx.state().decr_use_count();
        if matches!(ctrl, IoCtrl::MarkClose(_)) {
            ctx.buffer().state().mark_consumer_closed()
        }
        #[cfg(test)]
        log::trace!("[BuffRead::Drop] ctrl({ctrl})");
    }
}

impl<X, B, P, T, O> AsRef<RingBuffer<P, T, O>> for BuffRead<X, B, P, T, O>
where
    X: BorrowMut<IoCtx<B, P, T, O>>,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn as_ref(&self) -> &RingBuffer<P, T, O> {
        self.0.borrow().buffer()
    }
}

impl<X, B, P, T, O> TrBuffIterRead<T> for BuffRead<X, B, P, T, O>
where
    X: BorrowMut<IoCtx<B, P, T, O>>,
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
        BuffRead::read_async(self, length)
    }
}

impl<X, B, P, T, O> TrBuffIterTryRead<T> for BuffRead<X, B, P, T, O>
where
    X: BorrowMut<IoCtx<B, P, T, O>>,
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
        BuffRead::try_read(self, length)
    }
}

impl<X, B, P, T, O> Borrow<RingBuffer<P, T, O>> for BuffRead<X, B, P, T, O>
where
    X: BorrowMut<IoCtx<B, P, T, O>>,
    B: Borrow<RingBuffer<P, T, O>>,
    P: BorrowMut<[T]>,
    T: Clone,
    O: TrCmpxchOrderings,
{
    fn borrow(&self) -> &RingBuffer<P, T, O> {
        self.0.borrow().buffer()
    }
}

impl<X, B, P, T, O> TrBuffIterPeek<T> for BuffRead<X, B, P, T, O>
where
    X: BorrowMut<IoCtx<B, P, T, O>>,
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
        BuffRead::peek_async(self)
    }
}

impl<X, B, P, T, O> TrBuffIterTryPeek<T> for BuffRead<X, B, P, T, O>
where
    X: BorrowMut<IoCtx<B, P, T, O>>,
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
        BuffRead::try_peek(self)
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
        let mut check = move |s: &RwState<O>| {
            Demand::<O>::consumer_check(s)
        };
        loop {
            let ring_buf = unsafe { p_ring_buf.as_ref() };
            if let Option::Some(demand_ref) = this.io_ctx_.demand() {
                let sign_recv = demand_ref.signal.peeker();
                pin_mut!(sign_recv);
                let x = sign_recv
                    .peek_async()
                    .may_cancel_with(this.cancel_.as_mut())
                    .await;
                let _ = ring_buf.state().dequeue_consumer(demand_ref);
                return if x.is_ok() {
                    #[cfg(test)]
                    log::trace!("[ReadFuture::read_async_] sig recv ok");
                    unsafe { p_ring_buf.as_ref().try_read_(*this.length_) }
                } else {
                    #[cfg(test)]
                    log::trace!("[ReadFuture::read_async_] sig recv err");
                    Result::Err(RxError::Drained(0usize))
                }
            } else {
                let demand = Demand::new(&mut check);
                let try_init = unsafe {
                    let ctx_pin = Pin::new_unchecked(p_ctx.as_mut());
                    ctx_pin.try_init_demand(demand)
                };
                let Result::Ok(demand_ref) = try_init else { continue; };
                let x = ring_buf.state().enqueue_consumer(demand_ref);
                #[cfg(test)]
                log::trace!("[ReadFuture::read_async_] enqueued");
                assert!(x)
            }
        }
    }
}
